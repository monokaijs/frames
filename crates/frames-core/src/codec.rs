//! FFmpeg process adapter. Frames does not bundle the user's FFmpeg binary.
//! Native capture remains per-window; CPU readback is explicitly reported.
use crate::media::{MAX_FRAME_BYTES, RawFrame};
mod avi;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicUsize, Ordering},
    },
    time::Duration,
};
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
        if let Ok(executable) = std::env::current_exe()
            && let Some(directory) = executable.parent()
        {
            let bundled = directory.join("ffmpeg");
            if bundled.is_file() {
                return bundled.into_os_string();
            }
        }
        for path in [
            "/opt/homebrew/opt/ffmpeg-full/bin/ffmpeg",
            "/opt/homebrew/bin/ffmpeg",
            "/usr/local/opt/ffmpeg-full/bin/ffmpeg",
            "/usr/local/bin/ffmpeg",
        ] {
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

const NATIVE_H264_ENCODER: &str = "frames_videotoolbox";

fn native_h264_command(width: u32, height: u32, fps: u32) -> Option<Command> {
    #[cfg(target_os = "macos")]
    {
        let executable = std::env::current_exe().ok()?;
        let dir = executable.parent()?;
        let helper = std::env::var_os("FRAMES_MAC_HELPER")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                [
                    dir.join("FramesHelper"),
                    dir.join("../Resources/FramesHelper"),
                ]
                .into_iter()
                .find(|p| p.is_file())
            })?;
        let mut command = Command::new(helper);
        command.args([
            "--encode-h264",
            &width.to_string(),
            &height.to_string(),
            &fps.to_string(),
            "12000000",
        ]);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        Some(command)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (width, height, fps);
        None
    }
}

async fn read_native_frame(
    reader: &mut (impl tokio::io::AsyncRead + Unpin),
) -> std::io::Result<Vec<u8>> {
    let size = reader.read_u32_le().await? as usize;
    if size == 0 || size > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid native encoder packet size",
        ));
    }
    let mut bytes = vec![0; size];
    reader.read_exact(&mut bytes).await?;
    Ok(bytes)
}

async fn probe_native_h264() -> Option<CodecCapability> {
    let mut child = native_h264_command(128, 128, 30)?.spawn().ok()?;
    let mut stdin = child.stdin.take()?;
    let write = tokio::spawn(async move {
        let blue = [220u8, 30, 20, 255].repeat(128 * 128 * 2);
        let _ = stdin.write_all(&blue).await;
    });
    let output = tokio::time::timeout(Duration::from_secs(5), child.wait_with_output())
        .await
        .ok()?
        .ok()?;
    let _ = write.await;
    if !output.status.success() || output.stdout.is_empty() {
        return None;
    }
    let mut source = std::io::Cursor::new(output.stdout);
    let mut bitstream = Vec::new();
    while source.position() < source.get_ref().len() as u64 {
        bitstream.extend(read_native_frame(&mut source).await.ok()?);
    }
    probe_bitstream(Codec::H264, bitstream)
        .await
        .then(|| CodecCapability {
            codec: Codec::H264,
            encoder: Some(NATIVE_H264_ENCODER.into()),
            decoder: true,
            hardware: true,
        })
}

async fn probe_bitstream(codec: Codec, output: Vec<u8>) -> bool {
    let Ok(mut decoder) = command()
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
    else {
        return false;
    };
    let mut stdin = decoder.stdin.take().unwrap();
    let write = tokio::spawn(async move {
        let _ = stdin.write_all(&output).await;
    });
    let valid = matches!(tokio::time::timeout(Duration::from_secs(5), decoder.wait_with_output()).await,
        Ok(Ok(ref o)) if o.status.success() && o.stdout.len() == 128 * 128 * 4);
    let _ = write.await;
    valid
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
    let mut options = if matches!(encoder, "libx264" | "libx265") {
        vec!["-preset", "ultrafast", "-tune", "zerolatency", "-bf", "0"]
    } else if encoder.ends_with("_nvenc") {
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
    } else if encoder.ends_with("_amf") {
        vec![
            "-usage",
            "ultralowlatency",
            "-quality",
            "speed",
            "-async_depth",
            "1",
            "-preanalysis",
            "0",
            "-preencode",
            "0",
            "-bf",
            "0",
        ]
    } else if encoder.ends_with("_qsv") {
        let mut options = vec!["-preset", "veryfast", "-async_depth", "1", "-bf", "0"];
        options.extend(if encoder.starts_with("h264_") {
            ["-look_ahead", "0"]
        } else {
            ["-look_ahead_depth", "0"]
        });
        options
    } else if encoder.ends_with("_videotoolbox") {
        // Real-time scheduling alone still permits encoder buffering. This
        // flag enables VideoToolbox's one-frame-in/one-frame-out rate control.
        vec![
            "-realtime",
            "1",
            "-allow_sw",
            "0",
            "-bf",
            "0",
            "-flags",
            "+low_delay",
        ]
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
    };
    // AMF otherwise deliberately withholds one output picture even with no B-frames.
    options.extend(["-flags", "+low_delay"]);
    options
}

/// Clients need decoders only. Decode generated reference frames without probing
/// hardware encoders, which can reset buggy GPU drivers even on a receiving PC.
pub async fn probe_decoders() -> Vec<CodecCapability> {
    let samples: [(Codec, &[u8]); 4] = [
        (Codec::H264, include_bytes!("../fixtures/probe.h264")),
        (Codec::Hevc, include_bytes!("../fixtures/probe.hevc")),
        (Codec::Av1, include_bytes!("../fixtures/probe-av1.ivf")),
        (Codec::Vp9, include_bytes!("../fixtures/probe-vp9.ivf")),
    ];
    let mut capabilities = Vec::new();
    for (codec, sample) in samples {
        // Match live decoding: let FFmpeg choose an available implementation.
        // Essentials builds may support AV1 without shipping libdav1d.
        if probe_bitstream(codec, sample.to_vec()).await {
            capabilities.push(CodecCapability {
                codec,
                encoder: None,
                decoder: true,
                hardware: false,
            });
        }
    }
    capabilities
}

/// Encodes a real short clip and decodes it again before advertising the path.
pub async fn probe() -> Vec<CodecCapability> {
    let mut capabilities = vec![];
    for codec in [Codec::H264, Codec::Hevc, Codec::Av1, Codec::Vp9] {
        if codec == Codec::H264
            && let Some(native) = probe_native_h264().await
        {
            capabilities.push(native);
            continue;
        }
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
                // Match the live encoder: VideoToolbox low-delay rate control
                // requires an explicit bitrate, including during this probe.
                "-b:v",
                "12M",
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
            if probe_bitstream(codec, output).await {
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
    input: watch::Receiver<Option<RawFrame>>,
) -> anyhow::Result<Encoder> {
    encode_paced(
        width,
        height,
        fps,
        capability,
        input,
        Arc::new(AtomicU32::new(fps)),
    )
    .await
}

pub async fn encode_paced(
    width: u32,
    height: u32,
    fps: u32,
    capability: &CodecCapability,
    mut input: watch::Receiver<Option<RawFrame>>,
    rate: Arc<AtomicU32>,
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
    let native = encoder == NATIVE_H264_ENCODER;
    let mut cmd = if native {
        native_h264_command(width, height, fps)
            .ok_or_else(|| anyhow::anyhow!("native VideoToolbox helper missing"))?
    } else {
        command()
    };
    if !native {
        cmd.args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-filter_threads",
            "1",
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
            if matches!(capability.codec, Codec::H264 | Codec::Hevc) {
                "avi"
            } else {
                capability.codec.demuxer()
            },
            "pipe:1",
        ]);
    }
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
        let mut submitted: Option<tokio::time::Instant> = None;
        loop {
            let period = Duration::from_micros(
                1_000_000 / rate.load(Ordering::Relaxed).clamp(1, 240) as u64,
            );
            if let Some(last) = submitted {
                tokio::time::sleep_until(last + period).await;
            }
            let frame = input.borrow_and_update().clone();
            if let Some(frame) = frame {
                if frame.width != width || frame.height != height || !frame.validate() {
                    break;
                }
                submitted = Some(tokio::time::Instant::now());
                if stdin.write_all(&frame.bgra).await.is_err() {
                    break;
                }
            }
            if input.changed().await.is_err() {
                break;
            }
        }
    });
    let (sender, frames) = mpsc::channel(1);
    let codec = capability.codec;
    tokio::spawn(async move {
        if native {
            while let Ok(frame) = read_native_frame(&mut stdout).await {
                if sender.send(frame).await.is_err() {
                    break;
                }
            }
        } else if matches!(codec, Codec::Vp9 | Codec::Av1) {
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
            if avi::header(&mut stdout).await.is_err() {
                return;
            }
            while let Ok(mut frame) = avi::packet(&mut stdout).await {
                // The receiving elementary-stream parser also needs a picture
                // boundary now, rather than waiting for a later capture.
                append_aud(codec, &mut frame);
                if sender.send(frame).await.is_err() {
                    break;
                }
            }
        }
    });
    Ok(Encoder {
        child,
        frames,
        input_task,
    })
}

fn append_aud(codec: Codec, frame: &mut Vec<u8>) {
    match codec {
        Codec::H264 => frame.extend_from_slice(&[0, 0, 0, 1, 9, 0xf0]),
        Codec::Hevc => frame.extend_from_slice(&[0, 0, 0, 1, 70, 1, 0x50]),
        _ => {}
    }
}

#[cfg(test)]
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
    pending: Arc<AtomicUsize>,
}
impl Decoder {
    pub fn in_flight(&self) -> usize {
        self.pending.load(Ordering::Acquire)
    }
    pub fn try_submit(&self, frame: Vec<u8>) -> Result<(), mpsc::error::TrySendError<Vec<u8>>> {
        self.pending.fetch_add(1, Ordering::AcqRel);
        if let Err(error) = self.input.try_send(frame) {
            self.pending.fetch_sub(1, Ordering::AcqRel);
            return Err(error);
        }
        Ok(())
    }
}
pub async fn decode(codec: Codec, width: u32, height: u32) -> anyhow::Result<Decoder> {
    anyhow::ensure!(
        width > 0 && height > 0 && width <= 8192 && height <= 8192,
        "invalid decode size"
    );
    let mut child = command()
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-filter_threads",
            "1",
            "-flags",
            "low_delay",
            "-probesize",
            "32",
            "-analyzeduration",
            "0",
            "-f",
            codec.demuxer(),
            // This is an input option: bound decoder parallelism per window.
            // Putting it only after -i configures the rawvideo output instead.
            "-threads",
            "1",
            "-i",
            "pipe:0",
            "-an",
            "-vf",
            // Remove encoder padding exactly. Chroma-aligned rounding would
            // shorten odd-sized raw frames and desynchronize read_exact below.
            &format!("crop={width}:{height}:0:0:exact=1"),
            "-f",
            "rawvideo",
            "-pix_fmt",
            "bgra",
            "-threads",
            "1",
            "pipe:1",
        ])
        .spawn()?;
    crate::process::track(&child)?;
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    // A cold decoder may not read its pipe immediately. Allow a bounded startup
    // burst; ClientMedia applies the smaller live backlog limit once it renders.
    let (input, mut encoded) = mpsc::channel::<Vec<u8>>(16);
    tokio::spawn(async move {
        while let Some(frame) = encoded.recv().await {
            if stdin.write_all(&frame).await.is_err() {
                break;
            }
        }
    });
    let (sender, output) = watch::channel(None);
    let pending = Arc::new(AtomicUsize::new(0));
    let completed = pending.clone();
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
                    bgra: bgra.into(),
                }))
                .is_err()
            {
                break;
            }
            let _ = completed.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                Some(n.saturating_sub(1))
            });
        }
    });
    Ok(Decoder {
        child,
        input,
        output,
        pending,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn native_packet_framing_handles_short_reads_and_rejects_bad_sizes() {
        let (mut tx, mut rx) = tokio::io::duplex(16);
        tokio::spawn(async move {
            for byte in [3, 0, 0, 0, 10, 20, 30] {
                tx.write_all(&[byte]).await.unwrap();
                tokio::task::yield_now().await;
            }
        });
        assert_eq!(read_native_frame(&mut rx).await.unwrap(), [10, 20, 30]);
        for size in [0u32, MAX_FRAME_BYTES as u32 + 1] {
            let mut input = std::io::Cursor::new(size.to_le_bytes());
            assert_eq!(
                read_native_frame(&mut input).await.unwrap_err().kind(),
                std::io::ErrorKind::InvalidData
            );
        }
        let mut truncated = std::io::Cursor::new([3, 0, 0, 0, 10]);
        assert_eq!(
            read_native_frame(&mut truncated).await.unwrap_err().kind(),
            std::io::ErrorKind::UnexpectedEof
        );
    }
    #[cfg(target_os = "macos")]
    #[tokio::test]
    #[ignore = "requires built FramesHelper and physical VideoToolbox hardware"]
    async fn native_h264_single_frame_decodes_without_another_input_or_eof() {
        let mut child = native_h264_command(128, 128, 60).unwrap().spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        stdin
            .write_all(&[220u8, 30, 20, 255].repeat(128 * 128))
            .await
            .unwrap();
        let packet = tokio::time::timeout(Duration::from_secs(2), read_native_frame(&mut stdout))
            .await
            .expect("encoder buffered the only submitted frame")
            .unwrap();
        assert!(is_keyframe(Codec::H264, &packet, false));
        let mut decoder = decode(Codec::H264, 128, 128).await.unwrap();
        decoder.input.send(packet).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), decoder.output.changed())
            .await
            .expect("decoder waited for another access unit")
            .unwrap();
        let frame = decoder.output.borrow().clone().unwrap();
        assert!(frame.bgra[0] > 140 && frame.bgra[2] < 100);
        decoder.child.kill().await.unwrap();
        child.kill().await.unwrap();
        drop(stdin);
    }
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
