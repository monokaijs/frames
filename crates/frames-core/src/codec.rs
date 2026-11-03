//! In-process codecs with complete packets and dedicated blocking workers.
//! CPU frames and pinned D3D11 surfaces share packet identity and bounded workers.
use crate::{media::RawFrame, worker::Worker};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot, watch};
pub(crate) mod runtime;
#[cfg(windows)]
pub use runtime::GpuConverter;

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
        "\n--- Reed-Solomon SIMD ---\n",
        include_str!("../../../licenses/Reed-Solomon-SIMD.txt"),
        "\n\n",
        include_str!("../../../licenses/libvpx-BSD.txt"),
        "\n\n",
        include_str!("../../../licenses/libaom-BSD.txt"),
        "\n\n",
        include_str!("../../../licenses/dav1d-BSD.txt"),
        "\n\n",
        include_str!("../../../licenses/SDL3-zlib.txt"),
        "\n\n",
        include_str!("../../../licenses/sdl3-rs.txt"),
        "\n\n",
        include_str!("../../../licenses/vigem-client-MIT.txt"),
        "\n\n",
        include_str!("../../../licenses/vigem-bus.txt"),
        "\n\n",
        include_str!("../../../licenses/usbip-win2.txt"),
        "\n--- ProtoInput ---\n",
        include_str!("../../../licenses/ProtoInput-MIT.txt"),
        "\n--- EasyHook ---\n",
        include_str!("../../../licenses/EasyHook-MIT.txt"),
        "\n--- udis86 ---\n",
        include_str!("../../../licenses/udis86-BSD.txt"),
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
                    media_generation: 0,
                    #[cfg(windows)]
                    gpu: None,
                    captured_at: std::time::Instant::now(),
                    frame_number: 0,
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
                                anyhow::ensure!(
                                    packets.len() == 1,
                                    "Encoder buffered a picture instead of returning one complete packet"
                                );
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

#[cfg(windows)]
#[derive(Debug)]
pub struct HardwareLease(*mut std::ffi::c_void);
#[cfg(windows)]
unsafe impl Send for HardwareLease {}
#[cfg(windows)]
unsafe impl Sync for HardwareLease {}
#[cfg(windows)]
impl HardwareLease {
    pub(crate) unsafe fn new(pointer: *mut std::ffi::c_void) -> Self {
        Self(pointer)
    }
}
#[cfg(windows)]
impl Drop for HardwareLease {
    fn drop(&mut self) {
        unsafe {
            runtime::frames_surface_release(self.0);
        }
    }
}
#[derive(Clone, Debug)]
pub struct EncodedPacket {
    pub generation: u32,
    pub surface_encoded: bool,
    pub encode_micros: u32,
    pub number: u64,
    pub captured_at: Instant,
    pub encoded_at: Instant,
    pub keyframe: bool,
    pub bytes: Vec<u8>,
}
impl std::ops::Deref for EncodedPacket {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.bytes
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateUpdate {
    Applied,
    RestartRequired,
}
pub struct Encoder {
    pub worker: Worker,
    pub frames: mpsc::Receiver<EncodedPacket>,
    keyframes: watch::Sender<u64>,
    pub bitrate: Arc<AtomicU32>,
}
impl Encoder {
    pub fn request_keyframe(&self) {
        self.keyframes
            .send_modify(|serial| *serial = serial.wrapping_add(1));
    }
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
    let bitrate = Arc::new(AtomicU32::new(12_000_000));
    let target_bitrate = bitrate.clone();
    let (keyframes, mut requested_keyframe) = watch::channel(0u64);
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
        let mut number = 0;
        let mut current_bitrate = 12_000_000;
        let mut current_rate = fps;
        let mut keyframe_serial = 0;
        let mut next = Instant::now();
        loop {
            let frame=handle.block_on(async{
                if !first || input.borrow().is_none() {
                    tokio::select!{biased;_=stopping.changed()=>return None,r=requested_keyframe.changed()=>if r.is_err(){return None},r=input.changed()=>if r.is_err(){return None}}
                }
                if Instant::now()<next {
                    tokio::select!{biased;_=stopping.changed()=>return None,_=tokio::time::sleep_until(next.into())=>{}}
                }
                input.borrow_and_update().clone()
            });
            let Some(frame) = frame else { break };
            first = false;
            let requested = *requested_keyframe.borrow_and_update();
            if requested != keyframe_serial {
                context.request_keyframe();
                keyframe_serial = requested;
            }
            let rate = rate.load(Ordering::Relaxed).clamp(1, fps);
            next = Instant::now() + Duration::from_secs_f64(1.0 / rate as f64);
            let target = target_bitrate
                .load(Ordering::Relaxed)
                .clamp(128_000, 100_000_000);
            if target != current_bitrate || rate != current_rate {
                match context.set_rate(target, rate) {
                    Ok(RateUpdate::Applied) => {}
                    Ok(RateUpdate::RestartRequired) => {
                        tracing::debug!(target, "encoder requires a rate-control restart");
                        match runtime::Context::encoder_at_rate(&name, width, height, rate, target)
                        {
                            Ok(mut replacement) => {
                                let _ = replacement.set_rate(target, rate);
                                replacement.request_keyframe();
                                context = replacement;
                            }
                            Err(error) => {
                                tracing::warn!(%error, "encoder rate-control restart failed");
                                break;
                            }
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "encoder rate control failed");
                        break;
                    }
                }
                current_bitrate = target;
                current_rate = rate;
            }
            let encoding_started = Instant::now();
            let packets = match context.encode(&frame) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error=%e,"Video encoder failed");
                    break;
                }
            };
            let mut closed = false;
            for bytes in packets {
                let packet = EncodedPacket {
                    surface_encoded: context.gpu_encoder_active(),
                    encode_micros: encoding_started.elapsed().as_micros().min(u32::MAX as u128)
                        as u32,
                    generation: 0,
                    number,
                    captured_at: frame.captured_at,
                    encoded_at: Instant::now(),
                    keyframe: bytes.first() == Some(&1),
                    bytes,
                };
                tracing::trace!(
                    capture = frame.frame_number,
                    frame = number,
                    encode_us = packet.encode_micros,
                    keyframe = packet.keyframe,
                    "video encoded"
                );
                number += 1;
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
    Ok(Encoder {
        worker,
        frames,
        keyframes,
        bitrate,
    })
}

/// Every protocol-6 packet carries the encoder's actual keyframe flag, including AV1/VP9.
pub fn is_keyframe(_codec: Codec, packet: &[u8], _first: bool) -> bool {
    packet.first() == Some(&1)
}

#[derive(Default)]
pub struct DecodeMetrics {
    pub decoded: AtomicU64,
    retained: std::sync::Mutex<std::collections::HashMap<(u32, u64), usize>>,
    pub failed: AtomicBool,
    pub histogram: std::sync::Mutex<crate::media::LatencyHistogram>,
    floor: AtomicU64,
    generation: AtomicU32,
    external_pending: std::sync::Mutex<std::collections::VecDeque<u64>>,
}
pub struct Decoder {
    pub worker: Worker,
    pub input: mpsc::Sender<EncodedPacket>,
    pub output: watch::Receiver<Option<RawFrame>>,
    pending: Arc<AtomicUsize>,
    pub metrics: Arc<DecodeMetrics>,
    external: bool,
}
impl Decoder {
    pub fn set_generation(&self, generation: u32) {
        self.metrics.floor.store(0, Ordering::Release);
        self.metrics.generation.store(generation, Ordering::Release);
    }
    pub fn is_external(&self) -> bool {
        self.external
    }
    pub fn discard_before(&self, number: u64) {
        self.metrics.floor.fetch_max(number, Ordering::Release);
        if self.external {
            self.metrics
                .retained
                .lock()
                .unwrap()
                .retain(|(_, n), _| *n >= number);
            let mut ids = self.metrics.external_pending.lock().unwrap();
            ids.retain(|n| *n >= number);
            self.pending.store(ids.len(), Ordering::Release);
        }
    }
    pub fn acknowledge_external(&self, number: u64, micros: u64) {
        if !self.external {
            return;
        }
        let mut ids = self.metrics.external_pending.lock().unwrap();
        if !ids.contains(&number) {
            return;
        }
        ids.retain(|n| *n > number);
        self.metrics
            .retained
            .lock()
            .unwrap()
            .retain(|(_, n), _| *n > number);
        self.pending.store(ids.len(), Ordering::Release);
        self.metrics.decoded.fetch_add(1, Ordering::Relaxed);
        self.metrics.histogram.lock().unwrap().record(micros);
    }
    pub fn external(
        mut submit: impl FnMut(EncodedPacket) -> anyhow::Result<()> + Send + 'static,
    ) -> Self {
        let (input, mut incoming) = mpsc::channel::<EncodedPacket>(2);
        let (output_sender, output) = watch::channel(None);
        let (stop, mut stopping) = watch::channel(false);
        let metrics = Arc::new(DecodeMetrics::default());
        let worker_metrics = metrics.clone();
        let task = tokio::spawn(async move {
            let _output_sender = output_sender;
            loop {
                let packet = tokio::select! { _ = stopping.changed() => break, packet = incoming.recv() => { let Some(packet) = packet else { break; }; packet } };
                if packet.generation != worker_metrics.generation.load(Ordering::Acquire)
                    || packet.number < worker_metrics.floor.load(Ordering::Acquire)
                {
                    continue;
                }
                if let Err(error) = submit(packet) {
                    tracing::warn!(%error, "native decode submission failed");
                    worker_metrics.failed.store(true, Ordering::Release);
                }
            }
        });
        Self {
            worker: Worker::new(stop, task),
            input,
            output,
            pending: Arc::new(AtomicUsize::new(0)),
            metrics,
            external: true,
        }
    }
    pub fn retained_bytes(&self) -> usize {
        self.metrics.retained.lock().unwrap().values().sum()
    }
    pub fn in_flight(&self) -> usize {
        self.pending.load(Ordering::Acquire)
    }
    pub fn try_submit(
        &self,
        bytes: Vec<u8>,
    ) -> Result<(), mpsc::error::TrySendError<EncodedPacket>> {
        self.try_submit_packet(EncodedPacket {
            surface_encoded: false,
            encode_micros: 0,
            generation: 0,
            number: 0,
            captured_at: Instant::now(),
            encoded_at: Instant::now(),
            keyframe: bytes.first() == Some(&1),
            bytes,
        })
    }
    pub fn try_submit_packet(
        &self,
        packet: EncodedPacket,
    ) -> Result<(), mpsc::error::TrySendError<EncodedPacket>> {
        if self.external {
            let mut ids = self.metrics.external_pending.lock().unwrap();
            if ids.len() >= 2 {
                return Err(mpsc::error::TrySendError::Full(packet));
            }
            ids.push_back(packet.number);
        }
        let number = packet.number;
        let key = (packet.generation, number);
        self.metrics
            .retained
            .lock()
            .unwrap()
            .insert(key, packet.bytes.len());
        self.pending.fetch_add(1, Ordering::AcqRel);
        if let Err(e) = self.input.try_send(packet) {
            self.metrics.retained.lock().unwrap().remove(&key);
            self.pending.fetch_sub(1, Ordering::AcqRel);
            if self.external {
                self.metrics
                    .external_pending
                    .lock()
                    .unwrap()
                    .retain(|n| *n != number);
            }
            return Err(e);
        }
        Ok(())
    }
    pub async fn shutdown(&mut self) {
        self.worker.shutdown().await;
    }
}
pub async fn decode(codec: Codec, width: u32, height: u32) -> anyhow::Result<Decoder> {
    decode_inner(codec, width, height, false, true).await
}
pub async fn decode_for_presentation(
    codec: Codec,
    width: u32,
    height: u32,
) -> anyhow::Result<Decoder> {
    decode_inner(
        codec,
        width,
        height,
        std::env::var("FRAMES_GPU").as_deref() != Ok("0"),
        true,
    )
    .await
}
pub async fn decode_software(codec: Codec, width: u32, height: u32) -> anyhow::Result<Decoder> {
    decode_inner(codec, width, height, false, false).await
}
async fn decode_inner(
    codec: Codec,
    width: u32,
    height: u32,
    surfaces: bool,
    allow_hardware: bool,
) -> anyhow::Result<Decoder> {
    anyhow::ensure!(
        width > 0 && height > 0 && width <= 8192 && height <= 8192,
        "Invalid decoder dimensions"
    );
    let (input, mut incoming) = mpsc::channel::<EncodedPacket>(2);
    let (sender, output) = watch::channel(None);
    let pending = Arc::new(AtomicUsize::new(0));
    let completed = pending.clone();
    let metrics = Arc::new(DecodeMetrics::default());
    let worker_metrics = metrics.clone();
    let (stop, mut stopping) = watch::channel(false);
    let (ready, started) = oneshot::channel();
    let handle = tokio::runtime::Handle::current();
    let task = tokio::task::spawn_blocking(move || {
        // No frame threading: it deliberately queues pictures. Hardware fallback
        // is tested with a complete reference packet before touching live state.
        let hardware = allow_hardware
            && cfg!(any(target_os = "macos", windows))
            && std::env::var("FRAMES_HW_DECODE").as_deref() != Ok("0");
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
        let mut hardware_active = hw_works;
        let mut awaiting_keyframe = false;
        loop {
            let packet = handle.block_on(async {
                tokio::select! {biased;_=stopping.changed()=>None,p=incoming.recv()=>p}
            });
            let Some(packet) = packet else { break };
            if packet.generation != worker_metrics.generation.load(Ordering::Acquire)
                || packet.number < worker_metrics.floor.load(Ordering::Acquire)
                || (awaiting_keyframe && !packet.keyframe)
            {
                worker_metrics
                    .retained
                    .lock()
                    .unwrap()
                    .remove(&(packet.generation, packet.number));
                let _ = completed.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                    Some(n.saturating_sub(1))
                });
                continue;
            }
            if packet.keyframe && awaiting_keyframe {
                context.flush_decoder();
                awaiting_keyframe = false;
            }
            let started = Instant::now();
            let mut decoded = if surfaces {
                context.decode_presentable(&packet)
            } else {
                context.decode(&packet)
            };
            // A reference probe cannot establish support for every live bitstream.
            // Retry an independent picture immediately, including hardware paths
            // that silently buffer it. An idle desktop may never send another.
            if hardware_active
                && (decoded.is_err() || (packet.keyframe && matches!(decoded, Ok(None))))
            {
                hardware_active = false;
                tracing::warn!(
                    ?codec,
                    "hardware decoder did not produce a picture; using software"
                );
                match runtime::Context::decoder(codec.decoder(), width, height, false) {
                    Ok(replacement) => {
                        context = replacement;
                        if packet.keyframe {
                            decoded = context.decode(&packet);
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "software decoder fallback failed");
                        break;
                    }
                }
            }
            worker_metrics
                .histogram
                .lock()
                .unwrap()
                .record(started.elapsed().as_micros() as u64);
            let _ = completed.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                Some(n.saturating_sub(1))
            });
            worker_metrics
                .retained
                .lock()
                .unwrap()
                .remove(&(packet.generation, packet.number));
            match decoded {
                Ok(Some(mut frame)) => {
                    if packet.generation != worker_metrics.generation.load(Ordering::Acquire)
                        || packet.number < worker_metrics.floor.load(Ordering::Acquire)
                    {
                        continue;
                    }
                    worker_metrics.decoded.fetch_add(1, Ordering::Relaxed);
                    frame.media_generation = packet.generation;
                    frame.frame_number = packet.number;
                    frame.captured_at = packet.captured_at;
                    if sender.send(Some(frame)).is_err() {
                        break;
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(error=%e,"Video decoder failed; awaiting an independent picture");
                    worker_metrics.failed.store(true, Ordering::Release);
                    awaiting_keyframe = true;
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
        metrics,
        external: false,
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
