//! In-process codecs with complete packets and dedicated blocking workers.
//! Capture and presentation currently use CPU BGRA buffers.
use crate::{media::RawFrame, worker::Worker};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot, watch};
pub(crate) mod runtime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    H264,
    Hevc,
    Av1,
    Vp9,
}
impl Codec {
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

pub fn runtime_status() -> anyhow::Result<&'static str> {
    runtime::initialize()?;
    Ok("bundled LGPL libavcodec; in-process video and Opus; no FFmpeg executable")
}
pub fn licenses() -> &'static str {
    concat!(
        include_str!("../../../THIRD_PARTY.md"),
        "\n\n",
        include_str!("../../../licenses/FFmpeg-LGPL-2.1.txt"),
        "\n\n",
        include_str!("../../../licenses/FFmpeg-LGPL-3.txt"),
        "\n\n",
        include_str!("../../../licenses/GPL-3.txt"),
        "\n\n",
        include_str!("../../../licenses/Opus-BSD.txt"),
        "\n\n",
        include_str!("../../../licenses/libvpx-BSD.txt"),
        "\n\n",
        include_str!("../../../licenses/libaom-BSD.txt"),
        "\n\n",
        include_str!("../../../licenses/dav1d-BSD.txt"),
    )
}
fn sample(codec: Codec) -> Vec<u8> {
    let bytes: &[u8] = match codec {
        Codec::H264 => include_bytes!("../fixtures/probe.h264"),
        Codec::Hevc => include_bytes!("../fixtures/probe.hevc"),
        Codec::Av1 => include_bytes!("../fixtures/probe-av1.ivf"),
        Codec::Vp9 => include_bytes!("../fixtures/probe-vp9.ivf"),
    };
    let data = if matches!(codec, Codec::Av1 | Codec::Vp9) {
        let size = u32::from_le_bytes(bytes[32..36].try_into().unwrap()) as usize;
        &bytes[44..44 + size]
    } else {
        bytes
    };
    let mut packet = vec![1];
    packet.extend_from_slice(data);
    packet
}
/// Receiving clients exercise decoders only; never initialize encoder drivers.
pub async fn probe_decoders() -> Vec<CodecCapability> {
    tokio::task::spawn_blocking(|| {
        [Codec::H264, Codec::Hevc, Codec::Av1, Codec::Vp9]
            .into_iter()
            .filter_map(|codec| {
                let mut context =
                    runtime::Context::decoder(codec.decoder(), 128, 128, false).ok()?;
                context.decode(&sample(codec)).ok()??;
                Some(CodecCapability {
                    codec,
                    encoder: None,
                    decoder: true,
                    hardware: false,
                })
            })
            .collect()
    })
    .await
    .unwrap_or_default()
}
/// Advertise only paths that produce and decode a single picture without EOF.
pub async fn probe() -> Vec<CodecCapability> {
    static CAPABILITIES: tokio::sync::OnceCell<Vec<CodecCapability>> =
        tokio::sync::OnceCell::const_new();
    CAPABILITIES
        .get_or_init(|| async {
            tokio::task::spawn_blocking(|| {
                let mut result = Vec::new();
                let frame = RawFrame {
                    width: 128,
                    height: 128,
                    bgra: [220, 30, 20, 255].repeat(128 * 128).into(),
                };
                for codec in [Codec::H264, Codec::Hevc, Codec::Av1, Codec::Vp9] {
                    for &(name, hardware) in candidates(codec) {
                        let valid = (|| -> anyhow::Result<()> {
                            let mut encoder = runtime::Context::encoder(name, 128, 128, 60)?;
                            let mut decoder =
                                runtime::Context::decoder(codec.decoder(), 128, 128, false)?;
                            for _ in 0..4 {
                                let packets = encoder.encode(&frame)?;
                                let output = decoder
                                    .decode(&packets[0])?
                                    .ok_or_else(|| anyhow::anyhow!("Decoder buffers pictures"))?;
                                anyhow::ensure!(
                                    output.bgra[0] > 140 && output.bgra[2] < 100,
                                    "Invalid codec pixels"
                                );
                            }
                            Ok(())
                        })();
                        match valid {
                            Ok(()) => {
                                result.push(CodecCapability {
                                    codec,
                                    encoder: Some(name.into()),
                                    decoder: true,
                                    hardware,
                                });
                                break;
                            }
                            Err(e) => tracing::debug!(name,error=%e,"Codec probe unavailable"),
                        }
                    }
                }
                result
            })
            .await
            .unwrap_or_default()
        })
        .await
        .clone()
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
    pub worker: Worker,
    pub frames: mpsc::Receiver<Vec<u8>>,
}
impl Encoder {
    pub async fn shutdown(&mut self) {
        self.worker.shutdown().await;
    }
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
        width > 0 && height > 0 && width <= 8192 && height <= 8192 && (1..=240).contains(&fps),
        "Invalid encoder dimensions or rate"
    );
    let name = capability
        .encoder
        .clone()
        .ok_or_else(|| anyhow::anyhow!("Encoding unavailable"))?;
    let (tx, frames) = mpsc::channel(1);
    let (stop, mut stopping) = watch::channel(false);
    let (ready, started) = oneshot::channel();
    let handle = tokio::runtime::Handle::current();
    let task = tokio::task::spawn_blocking(move || {
        let mut context = match runtime::Context::encoder(&name, width, height, fps) {
            Ok(c) => {
                let _ = ready.send(Ok(()));
                c
            }
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
        let mut first = true;
        let mut next = Instant::now();
        loop {
            let frame=handle.block_on(async{
                if !first || input.borrow().is_none() {
                    tokio::select!{biased;_=stopping.changed()=>return None,r=input.changed()=>if r.is_err(){return None}}
                }
                if Instant::now()<next {
                    tokio::select!{biased;_=stopping.changed()=>return None,_=tokio::time::sleep_until(next.into())=>{}}
                }
                input.borrow_and_update().clone()
            });
            let Some(frame) = frame else { break };
            first = false;
            let rate = rate.load(Ordering::Relaxed).clamp(1, fps);
            next = Instant::now() + Duration::from_secs_f64(1.0 / rate as f64);
            let packets = match context.encode(&frame) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error=%e,"Video encoder failed");
                    break;
                }
            };
            let mut closed = false;
            for packet in packets {
                if !handle.block_on(async {
                    tokio::select! {biased;_=stopping.changed()=>false,r=tx.send(packet)=>r.is_ok()}
                }) {
                    closed = true;
                    break;
                }
            }
            if closed {
                break;
            }
        }
    });
    let worker = Worker::new(stop, task);
    started.await??;
    Ok(Encoder { worker, frames })
}

/// Every protocol-5 packet carries the encoder's actual keyframe flag, including AV1/VP9.
pub fn is_keyframe(_codec: Codec, packet: &[u8], _first: bool) -> bool {
    packet.first() == Some(&1)
}

pub struct Decoder {
    pub worker: Worker,
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
        if let Err(e) = self.input.try_send(frame) {
            self.pending.fetch_sub(1, Ordering::AcqRel);
            return Err(e);
        }
        Ok(())
    }
    pub async fn shutdown(&mut self) {
        self.worker.shutdown().await;
    }
}
pub async fn decode(codec: Codec, width: u32, height: u32) -> anyhow::Result<Decoder> {
    anyhow::ensure!(
        width > 0 && height > 0 && width <= 8192 && height <= 8192,
        "Invalid decoder dimensions"
    );
    let (input, mut incoming) = mpsc::channel::<Vec<u8>>(2);
    let (sender, output) = watch::channel(None);
    let pending = Arc::new(AtomicUsize::new(0));
    let completed = pending.clone();
    let (stop, mut stopping) = watch::channel(false);
    let (ready, started) = oneshot::channel();
    let handle = tokio::runtime::Handle::current();
    let task = tokio::task::spawn_blocking(move || {
        // No frame threading: it deliberately queues pictures. Hardware fallback
        // is tested with a complete reference packet before touching live state.
        let hardware =
            cfg!(target_os = "macos") && std::env::var("FRAMES_HW_DECODE").as_deref() != Ok("0");
        let hw_works = hardware
            && (|| -> anyhow::Result<bool> {
                let mut test = runtime::Context::decoder(codec.decoder(), 128, 128, true)?;
                Ok(test.decode(&sample(codec))?.is_some())
            })()
            .unwrap_or(false);
        let mut context = match runtime::Context::decoder(codec.decoder(), width, height, hw_works)
        {
            Ok(c) => {
                let _ = ready.send(Ok(()));
                c
            }
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
        tracing::debug!(
            ?codec,
            hardware = hw_works,
            "In-process video decoder started"
        );
        loop {
            let packet = handle.block_on(async {
                tokio::select! {biased;_=stopping.changed()=>None,p=incoming.recv()=>p}
            });
            let Some(packet) = packet else { break };
            let decoded = context.decode(&packet);
            let _ = completed.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                Some(n.saturating_sub(1))
            });
            match decoded {
                Ok(Some(frame)) => {
                    if sender.send(Some(frame)).is_err() {
                        break;
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(error=%e,"Video decoder failed");
                    break;
                }
            }
        }
    });
    let worker = Worker::new(stop, task);
    started.await??;
    Ok(Decoder {
        worker,
        input,
        output,
        pending,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn negotiation_requires_mutual_codec() {
        let caps = vec![CodecCapability {
            codec: Codec::Vp9,
            encoder: Some("libvpx-vp9".into()),
            decoder: true,
            hardware: false,
        }];
        assert!(negotiate(&caps, &caps, Some(Codec::H264)).is_err());
        assert_eq!(negotiate(&caps, &caps, None).unwrap().codec, Codec::Vp9);
    }
}
