//! FFmpeg process adapter. Frames does not bundle the user's FFmpeg binary.
//! Native capture remains per-window; CPU readback is explicitly reported.
use crate::media::{MAX_FRAME_BYTES, RawFrame};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
    sync::{mpsc, watch},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    H264,
    Hevc,
    Av1,
    Vp9,
}
impl Codec {
    pub fn demuxer(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::Hevc => "hevc",
            Self::Av1 | Self::Vp9 => "ivf",
        }
    }
    pub fn decoder(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::Hevc => "hevc",
            Self::Av1 => "libdav1d",
            Self::Vp9 => "vp9",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodecCapability {
    pub codec: Codec,
    pub encoder: Option<String>,
    pub decoder: bool,
    pub hardware: bool,
}

pub fn ffmpeg_path() -> std::ffi::OsString {
    if let Some(path) = std::env::var_os("FRAMES_FFMPEG") {
        return path;
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(executable) = std::env::current_exe() {
            if let Some(directory) = executable.parent() {
                let bundled = directory.join("ffmpeg");
                if bundled.is_file() {
                    return bundled.into_os_string();
                }
            }
        }
        for path in ["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg"] {
            if std::path::Path::new(path).is_file() {
                return path.into();
            }
        }
    }
    "ffmpeg".into()
}

pub fn command() -> Command {
    let mut command = Command::new(ffmpeg_path());
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    command
}

fn candidates(codec: Codec) -> &'static [(&'static str, bool)] {
    match codec {
        Codec::H264 => {
            if cfg!(target_os = "macos") {
                &[("h264_videotoolbox", true)]
            } else {
                &[
                    ("h264_nvenc", true),
                    ("h264_qsv", true),
                    ("h264_amf", true),
                    ("h264_mf", true),
                ]
            }
        }
        Codec::Hevc => {
            if cfg!(target_os = "macos") {
                &[("hevc_videotoolbox", true)]
            } else {
                &[
                    ("hevc_nvenc", true),
                    ("hevc_qsv", true),
                    ("hevc_amf", true),
                    ("hevc_mf", true),
                ]
            }
        }
        Codec::Av1 => &[
            ("av1_nvenc", true),
            ("av1_qsv", true),
            ("av1_amf", true),
            ("av1_mf", true),
            ("libaom-av1", false),
        ],
        Codec::Vp9 => &[("libvpx-vp9", false)],
    }
}

fn encoder_options(encoder: &str) -> Vec<&'static str> {
    if encoder.ends_with("_nvenc") {
        vec![
            "-preset",
            "p1",
            "-tune",
            "ull",
            "-zerolatency",
            "1",
            "-bf",
            "0",
        ]
    } else if encoder.ends_with("_videotoolbox") {
        vec!["-realtime", "1", "-allow_sw", "0", "-bf", "0"]
    } else if encoder.ends_with("_mf") {
        vec!["-hw_encoding", "1", "-bf", "0"]
    } else if encoder == "libvpx-vp9" {
        vec![
            "-deadline",
            "realtime",
            "-cpu-used",
            "8",
            "-lag-in-frames",
            "0",
            "-row-mt",
            "1",
        ]
    } else if encoder == "libaom-av1" {
        vec![
            "-usage",
            "realtime",
            "-cpu-used",
            "8",
            "-lag-in-frames",
            "0",
            "-row-mt",
            "1",
        ]
    } else {
        vec!["-bf", "0"]
    }
}

/// Encodes a real short clip and decodes it again before advertising the path.
pub async fn probe() -> Vec<CodecCapability> {
    let mut capabilities = vec![];
    for codec in [Codec::H264, Codec::Hevc, Codec::Av1, Codec::Vp9] {
        let mut found = None;
        for &(encoder, hardware) in candidates(codec) {
            let mut cmd = command();
            cmd.args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=blue:s=128x128:r=30",
                "-frames:v",
                "2",
                "-an",
                "-c:v",
                encoder,
            ]);
            cmd.args(encoder_options(encoder));
            cmd.args([
                "-g",
                "30",
                "-pix_fmt",
                "yuv420p",
                "-f",
                codec.demuxer(),
                "pipe:1",
            ]);
            let output = match tokio::time::timeout(Duration::from_secs(5), cmd.output()).await {
                Ok(Ok(output)) if output.status.success() && !output.stdout.is_empty() => {
                    output.stdout
                }
                _ => continue,
            };
            let mut decoder = match command()
                .args([
                    "-v",
                    "error",
                    "-f",
                    codec.demuxer(),
                    "-i",
                    "pipe:0",
                    "-frames:v",
                    "1",
                    "-f",
                    "rawvideo",
                    "-pix_fmt",
                    "bgra",
                    "pipe:1",
                ])
                .spawn()
            {
                Ok(c) => c,
                Err(_) => continue,
            };
            let mut stdin = decoder.stdin.take().unwrap();
            let write = tokio::spawn(async move {
                let _ = stdin.write_all(&output).await;
            });
            let valid = matches!(tokio::time::timeout(Duration::from_secs(5), decoder.wait_with_output()).await, Ok(Ok(ref o)) if o.status.success() && o.stdout.len() == 128*128*4);
            let _ = write.await;
            if valid {
                found = Some(CodecCapability {
                    codec,
                    encoder: Some(encoder.to_owned()),
                    decoder: true,
                    hardware,
                });
                break;
            }
        }
        if let Some(cap) = found {
            capabilities.push(cap);
        }
    }
    capabilities
}

pub fn negotiate(
    host: &[CodecCapability],
    client: &[CodecCapability],
    requested: Option<Codec>,
) -> anyhow::Result<CodecCapability> {
    let mut compatible: BTreeMap<Codec, CodecCapability> = host
        .iter()
        .filter(|h| h.encoder.is_some() && client.iter().any(|c| c.codec == h.codec && c.decoder))
        .map(|h| (h.codec, h.clone()))
        .collect();
    if let Some(codec) = requested {
        return compatible.remove(&codec).ok_or_else(|| {
            anyhow::anyhow!("requested codec {codec:?} has no working encoder/decoder pair")
        });
    }
    for codec in [Codec::H264, Codec::Hevc, Codec::Av1] {
        if let Some(c) = compatible.get(&codec).filter(|c| c.hardware) {
            return Ok(c.clone());
        }
    }
    compatible
        .remove(&Codec::Vp9)
        .or_else(|| compatible.remove(&Codec::Av1))
        .ok_or_else(|| anyhow::anyhow!("no mutually supported codec; run frames doctor"))
}

pub struct Encoder {
    pub child: Child,
    pub frames: mpsc::Receiver<Vec<u8>>,
    pub input_task: tokio::task::JoinHandle<()>,
}

pub async fn encode(
    width: u32,
    height: u32,
    fps: u32,
    capability: &CodecCapability,
    mut input: watch::Receiver<Option<RawFrame>>,
) -> anyhow::Result<Encoder> {
    anyhow::ensure!(
        width > 0 && height > 0 && width <= 8192 && height <= 8192,
        "invalid encoder dimensions"
    );
    let encoder = capability
        .encoder
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("encoding unavailable"))?;
    let size = format!("{width}x{height}");
    let mut cmd = command();
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "rawvideo",
        "-pix_fmt",
        "bgra",
        "-s",
        &size,
        "-r",
        &fps.to_string(),
        "-i",
        "pipe:0",
        "-an",
        "-c:v",
        encoder,
    ]);
    cmd.args(encoder_options(encoder));
    cmd.args([
        "-g",
        "60",
        "-b:v",
        "12M",
        "-pix_fmt",
        "yuv420p",
        "-vf",
        "pad=max(128\\,ceil(iw/2)*2):max(128\\,ceil(ih/2)*2)",
    ]);
    match capability.codec {
        Codec::H264 => {
            cmd.args(["-bsf:v", "h264_metadata=aud=insert"]);
        }
        Codec::Hevc => {
            cmd.args(["-bsf:v", "hevc_metadata=aud=insert"]);
        }
        _ => {}
    }
    cmd.args([
        "-flush_packets",
        "1",
        "-f",
        capability.codec.demuxer(),
        "pipe:1",
    ]);
    cmd.stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    crate::process::track(&child)?;
    if let Some(stderr) = child.stderr.take() {
        let name = encoder.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::warn!(encoder=%name,message=%line,"encoder diagnostic");
            }
        });
    }
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let input_task = tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(Duration::from_micros(1_000_000 / fps.max(1) as u64));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let frame = input.borrow_and_update().clone();
            if let Some(frame) = frame {
                if frame.width != width || frame.height != height || !frame.validate() {
                    break;
                }
                if stdin.write_all(&frame.bgra).await.is_err() {
                    break;
                }
            }
            if input.has_changed().is_err() {
                break;
            }
        }
    });
    let (sender, frames) = mpsc::channel(2);
    let codec = capability.codec;
    tokio::spawn(async move {
        if matches!(codec, Codec::Vp9 | Codec::Av1) {
            let mut header = [0; 32];
            if stdout.read_exact(&mut header).await.is_err() {
                return;
            }
            if &header[..4] != b"DKIF" {
                return;
            }
            let mut first = true;
            loop {
                let mut fh = [0; 12];
                if stdout.read_exact(&mut fh).await.is_err() {
                    break;
                }
                let size = u32::from_le_bytes(fh[..4].try_into().unwrap()) as usize;
                if size == 0 || size > MAX_FRAME_BYTES {
                    break;
                }
                let mut frame = if first { header.to_vec() } else { vec![] };
                first = false;
                frame.extend_from_slice(&fh);
                let start = frame.len();
                frame.resize(start + size, 0);
                if stdout.read_exact(&mut frame[start..]).await.is_err()
                    || sender.send(frame).await.is_err()
                {
                    break;
                }
            }
        } else {
            let mut buffer = Vec::new();
            let mut chunk = [0; 65536];
            loop {
                let n = match stdout.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                buffer.extend_from_slice(&chunk[..n]);
                while let Some(boundary) = second_aud(&buffer, codec) {
                    let next = buffer.split_off(boundary);
                    let frame = std::mem::replace(&mut buffer, next);
                    if sender.send(frame).await.is_err() {
                        return;
                    }
                }
                if buffer.len() > MAX_FRAME_BYTES {
                    break;
                }
            }
            if !buffer.is_empty() {
                let _ = sender.send(buffer).await;
            }
        }
    });
    Ok(Encoder {
        child,
        frames,
        input_task,
    })
}

fn second_aud(bytes: &[u8], codec: Codec) -> Option<usize> {
    let mut found = false;
    let mut i = 0;
    while i + 5 < bytes.len() {
        let len = if bytes[i..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if bytes[i..].starts_with(&[0, 0, 1]) {
            3
        } else {
            i += 1;
            continue;
        };
        let typ = if codec == Codec::H264 {
            bytes[i + len] & 31
        } else {
            (bytes[i + len] >> 1) & 63
        };
        if typ == if codec == Codec::H264 { 9 } else { 35 } {
            if found {
                return Some(i);
            }
            found = true;
        }
        i += len + 1;
    }
    None
}

pub fn is_keyframe(codec: Codec, bytes: &[u8], first: bool) -> bool {
    if first {
        return true;
    }
    if matches!(codec, Codec::Av1 | Codec::Vp9) {
        return false;
    }
    bytes.windows(6).any(|b| {
        let n = if b.starts_with(&[0, 0, 0, 1]) {
            Some(b[4])
        } else if b.starts_with(&[0, 0, 1]) {
            Some(b[3])
        } else {
            None
        };
        n.is_some_and(|n| {
            if codec == Codec::H264 {
                n & 31 == 5
            } else {
                matches!((n >> 1) & 63, 19..=21)
            }
        })
    })
}

pub struct Decoder {
    pub child: Child,
    pub input: mpsc::Sender<Vec<u8>>,
    pub output: watch::Receiver<Option<RawFrame>>,
}
pub async fn decode(codec: Codec, width: u32, height: u32) -> anyhow::Result<Decoder> {
    anyhow::ensure!(
        width > 0 && height > 0 && width <= 8192 && height <= 8192,
        "invalid decode size"
    );
    let padded_width = width.div_ceil(2) * 2;
    let padded_height = height.div_ceil(2) * 2;
    let mut child = command()
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-flags",
            "low_delay",
            "-probesize",
            "32",
            "-analyzeduration",
            "0",
            "-f",
            codec.demuxer(),
            "-i",
            "pipe:0",
            "-an",
            "-vf",
            &format!("crop={width}:{height}:0:0"),
            "-f",
            "rawvideo",
            "-pix_fmt",
            "bgra",
            "-threads",
            "2",
            "pipe:1",
        ])
        .spawn()?;
    crate::process::track(&child)?;
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let (input, mut encoded) = mpsc::channel::<Vec<u8>>(4);
    tokio::spawn(async move {
        while let Some(frame) = encoded.recv().await {
            if stdin.write_all(&frame).await.is_err() {
                break;
            }
        }
    });
    let (sender, output) = watch::channel(None);
    let _ = (padded_width, padded_height);
    tokio::spawn(async move {
        loop {
            let mut bgra = vec![0; width as usize * height as usize * 4];
            if stdout.read_exact(&mut bgra).await.is_err() {
                break;
            }
            if sender
                .send(Some(RawFrame {
                    width,
                    height,
                    bgra,
                }))
                .is_err()
            {
                break;
            }
        }
    });
    Ok(Decoder {
        child,
        input,
        output,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn negotiation_requires_a_mutual_path_and_honors_manual_selection() {
        let host = vec![CodecCapability {
            codec: Codec::Vp9,
            encoder: Some("libvpx-vp9".into()),
            decoder: true,
            hardware: false,
        }];
        assert!(negotiate(&host, &host, Some(Codec::H264)).is_err());
        assert_eq!(negotiate(&host, &host, None).unwrap().codec, Codec::Vp9);
    }
    #[test]
    fn annex_b_preserves_sps_with_first_access_unit() {
        let bytes = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 9, 0xf0, 0, 0, 1, 0x65, 3, 4, 0, 0, 0, 1, 9, 0xf0,
            0,
        ];
        assert_eq!(second_aud(&bytes, Codec::H264), Some(19));
        assert!(is_keyframe(Codec::H264, &bytes, false));
    }
}
