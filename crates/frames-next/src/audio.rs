//! Host audio to the viewer: 10 ms Opus packets as datagrams, each carrying
//! the previous packet too, through v1's Opus workers and jitter buffer
//! (`frames-core::audio`).
//!
//! Datagram: `0x02`, sequence (u32 LE), send time (u32 LE, µs), then the
//! packet (`frames_core::audio::packet_with_previous`). `0x02` is neither a
//! media wire byte (`0x2_`) nor the input tag (`0x01`).

use frames_core::audio::{JitterBuffer, PcmFormat, packet_with_previous, split_packet};
use frames_media::api::MediaTransport;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub const TAG: u8 = 0x02;
const FORMAT: PcmFormat = PcmFormat {
    rate: 48_000,
    channels: 2,
};
/// One 10 ms chunk of 48 kHz stereo f32.
const CHUNK_SAMPLES: usize = 480 * 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Off,
    /// This machine's audio (capture on the host, playback on the viewer).
    On,
    /// A generated 440 Hz tone on the host, a silent checking sink on the
    /// viewer: unattended runs never capture or play the user's audio.
    Qa,
}

pub fn parse_mode(value: &str) -> anyhow::Result<Mode> {
    Ok(match value {
        "on" => Mode::On,
        "off" => Mode::Off,
        "qa" => Mode::Qa,
        other => anyhow::bail!("--audio takes on, off or qa, not {other}"),
    })
}

fn pcm_bytes(samples: &[f32]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

/// What a host's audio is.
#[derive(Clone, Debug)]
pub enum Source {
    /// Everything this computer plays.
    System,
    /// One application's (sharing selected windows of one process, as v1).
    #[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
    Process {
        pid: u32,
        /// Where the Windows adapter is extracted (unused on macOS).
        #[cfg_attr(not(windows), allow(dead_code))]
        dir: std::path::PathBuf,
    },
}

/// Host: streams `mode`'s audio from `source` until the session closes.
pub async fn host(session: frames_net::Session, mode: Mode, source: Source) -> anyhow::Result<()> {
    if mode == Mode::Off {
        return Ok(());
    }
    let (pcm_tx, mut pcm_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let mut format = FORMAT;
    let mut _capture = None;
    match mode {
        Mode::Qa => {
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_millis(10));
                let mut phase = 0f32;
                loop {
                    tick.tick().await;
                    let mut samples = Vec::with_capacity(CHUNK_SAMPLES);
                    for _ in 0..CHUNK_SAMPLES / 2 {
                        let value = (phase * std::f32::consts::TAU).sin() * 0.25;
                        samples.extend([value, value]);
                        phase = (phase + 440.0 / 48_000.0).fract();
                    }
                    if pcm_tx.send(pcm_bytes(&samples)).await.is_err() {
                        return;
                    }
                }
            });
        }
        Mode::On => {
            let mut capture = start_capture(&source)?;
            format = PcmFormat {
                rate: capture.format.rate,
                channels: capture.format.channels,
            };
            println!(
                "audio: capturing {} audio at {} Hz, {} channels",
                match source {
                    Source::System => "system",
                    Source::Process { .. } => "application",
                },
                format.rate,
                format.channels
            );
            drop(pcm_tx);
            // The encoder reads the capture's PCM; the capture (its guard)
            // stays alive with the session.
            pcm_rx = std::mem::replace(&mut capture.packets, tokio::sync::mpsc::channel(1).1);
            _capture = Some(capture);
        }
        Mode::Off => unreachable!(),
    }
    let mut encoder = frames_core::audio::encode(format, pcm_rx).await?;
    let media = session.media();
    let clock = frames_media::clock::SystemClock::new();
    let mut seq = 0u32;
    let mut previous: Option<Vec<u8>> = None;
    while let Some(packet) = encoder.packets.recv().await {
        if session.is_closed() {
            break;
        }
        let Ok(body) = packet_with_previous(&packet, previous.as_deref(), 1100) else {
            continue;
        };
        let mut datagram = Vec::with_capacity(9 + body.len());
        datagram.push(TAG);
        datagram.extend(seq.to_le_bytes());
        datagram.extend((frames_media::api::MediaClock::now_us(&clock) as u32).to_le_bytes());
        datagram.extend(body);
        let _ = media.try_send(datagram.into());
        seq = seq.wrapping_add(1);
        previous = Some(packet);
    }
    encoder.shutdown().await;
    Ok(())
}

/// System audio: WASAPI loopback on Windows, ScreenCaptureKit on macOS.
/// One application's: process loopback through the shared-input adapter
/// (Windows), or ScreenCaptureKit filtered to that application (macOS).
fn start_capture(source: &Source) -> anyhow::Result<frames_audio::AudioCapture> {
    match source {
        #[cfg(target_os = "macos")]
        Source::System => mac_capture(None),
        #[cfg(not(target_os = "macos"))]
        Source::System => frames_audio::loopback(),
        #[cfg(windows)]
        Source::Process { pid, dir } => process_capture(*pid, dir),
        #[cfg(target_os = "macos")]
        Source::Process { pid, .. } => mac_capture(Some(*pid as i32)),
        #[cfg(not(any(windows, target_os = "macos")))]
        Source::Process { .. } => anyhow::bail!("application audio needs a Windows or macOS host"),
    }
}

/// ScreenCaptureKit audio (Screen Recording permission), as v1's
/// SystemAudioCapture: the whole Mac but Frames, or one application.
#[cfg(target_os = "macos")]
fn mac_capture(pid: Option<i32>) -> anyhow::Result<frames_audio::AudioCapture> {
    use frames_platform_macos::audio::{CHANNELS, MacAudio, SAMPLE_RATE};
    use std::sync::atomic::{AtomicBool, Ordering};
    // ScreenCaptureKit hands over 10-20 ms at a time.
    let (tx, packets) = tokio::sync::mpsc::channel::<Vec<u8>>(32);
    let capture = MacAudio::start(pid, move |samples| {
        if let Some(samples) = samples {
            let _ = tx.try_send(pcm_bytes(samples));
        }
    })
    .map_err(|reason| anyhow::anyhow!("macOS audio capture: {reason}"))?;
    // The capture stops when the session's guard is dropped.
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    std::thread::Builder::new()
        .name("frames-mac-audio".into())
        .spawn(move || {
            while !flag.load(Ordering::Acquire) {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            drop(capture);
        })?;
    Ok(frames_audio::AudioCapture {
        format: frames_audio::PcmFormat {
            rate: SAMPLE_RATE,
            channels: CHANNELS,
        },
        packets,
        guard: frames_audio::Guard::new(stop),
    })
}

#[cfg(windows)]
fn process_capture(pid: u32, dir: &std::path::Path) -> anyhow::Result<frames_audio::AudioCapture> {
    use std::sync::atomic::{AtomicBool, Ordering};
    let mut capture = frames_platform_windows::shared_input::ProcessAudio::start(pid, dir)?;
    let (tx, packets) = tokio::sync::mpsc::channel(2);
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    std::thread::Builder::new()
        .name("frames-app-audio".into())
        .spawn(move || {
            // 100 ms of 48 kHz stereo f32 at most per read.
            let mut bytes = vec![0u8; 38_400];
            while !flag.load(Ordering::Acquire) {
                match capture.read(&mut bytes) {
                    None => break,
                    Some(0) => std::thread::sleep(std::time::Duration::from_millis(5)),
                    Some(n) => {
                        let _ = tx.try_send(bytes[..n].to_vec());
                    }
                }
            }
        })?;
    Ok(frames_audio::AudioCapture {
        format: frames_audio::PcmFormat {
            rate: 48_000,
            channels: 2,
        },
        packets,
        guard: frames_audio::Guard::new(stop),
    })
}

/// What the viewer's audio did, for reports.
#[derive(Debug, Default)]
pub struct Stats {
    pub packets: AtomicU64,
    pub concealed: AtomicU64,
    pub pcm_chunks: AtomicU64,
    /// Zero crossings per second in the decoded audio, ×10 (QA tone check).
    pub crossing_rate_x10: AtomicU64,
}

/// Viewer: plays (or, in QA, checks) the host's audio datagrams.
pub struct Receiver {
    datagrams: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    pub stats: Arc<Stats>,
}

impl Receiver {
    pub async fn start(mode: Mode) -> anyhow::Result<Option<Self>> {
        if mode == Mode::Off {
            return Ok(None);
        }
        let (datagrams, mut incoming) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let stats = Arc::new(Stats::default());
        let decoder = frames_core::audio::decode(FORMAT).await?;
        let input = decoder.input.clone();
        let counters = stats.clone();
        tokio::spawn(async move {
            let mut jitter = JitterBuffer::default();
            let mut unwrap: Option<(u32, u64)> = None;
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(5));
            loop {
                tokio::select! {
                    datagram = incoming.recv() => {
                        let Some(datagram) = datagram else { return };
                        if datagram.len() < 10 || datagram[0] != TAG {
                            continue;
                        }
                        let seq = u32::from_le_bytes(datagram[1..5].try_into().unwrap());
                        let number = match unwrap {
                            None => u64::from(seq),
                            Some((last, base)) => base + u64::from(seq.wrapping_sub(last)),
                        };
                        unwrap = Some((seq, number));
                        let Some((current, previous)) = split_packet(&datagram[9..]) else {
                            continue;
                        };
                        if jitter.received == 0 {
                            jitter.skip_before(number);
                        }
                        jitter.push(number, current, previous);
                        counters.packets.fetch_add(1, Ordering::Relaxed);
                    }
                    _ = tick.tick() => {}
                }
                let mut ready = Vec::new();
                jitter.drain(std::time::Instant::now(), |packet| ready.push(packet));
                counters
                    .concealed
                    .store(jitter.concealed, Ordering::Relaxed);
                for packet in ready {
                    if input.send(packet).await.is_err() {
                        return;
                    }
                }
            }
        });
        let counters = stats.clone();
        let mut sink = Sink::new(mode)?;
        tokio::spawn(async move {
            // Own the whole decoder: capturing only `decoder.pcm` would drop
            // its worker (and stop decoding) when `start` returns.
            let mut decoder = decoder;
            let started = std::time::Instant::now();
            let mut crossings = 0u64;
            let mut last = 0f32;
            while let Some(pcm) = decoder.pcm.recv().await {
                counters.pcm_chunks.fetch_add(1, Ordering::Relaxed);
                for frame in pcm.chunks_exact(8) {
                    let left = f32::from_le_bytes(frame[..4].try_into().unwrap());
                    if (left >= 0.0) != (last >= 0.0) {
                        crossings += 1;
                    }
                    last = left;
                }
                let seconds = started.elapsed().as_secs_f64().max(0.001);
                counters.crossing_rate_x10.store(
                    (crossings as f64 / seconds * 10.0) as u64,
                    Ordering::Relaxed,
                );
                sink.play(&pcm);
            }
        });
        Ok(Some(Self { datagrams, stats }))
    }

    /// Hands over an audio datagram from the media channel.
    pub fn push(&self, datagram: &[u8]) {
        let _ = self.datagrams.send(datagram.to_vec());
    }
}

enum Sink {
    /// Decoded and checked, never played.
    Silent,
    Speakers(frames_audio::Playback),
}

impl Sink {
    fn new(mode: Mode) -> anyhow::Result<Self> {
        match mode {
            Mode::On => Ok(Self::Speakers(frames_audio::Playback::new()?)),
            _ => Ok(Self::Silent),
        }
    }

    fn play(&mut self, pcm: &[u8]) {
        match self {
            Self::Silent => {}
            Self::Speakers(playback) => playback.push(pcm),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn a_tone_survives_encode_redundancy_jitter_and_decode() {
        let receiver = Receiver::start(Mode::Qa).await.unwrap().unwrap();
        let (pcm_tx, pcm_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        tokio::spawn(async move {
            let mut phase = 0f32;
            for _ in 0..150 {
                let mut samples = Vec::with_capacity(CHUNK_SAMPLES);
                for _ in 0..CHUNK_SAMPLES / 2 {
                    let value = (phase * std::f32::consts::TAU).sin() * 0.25;
                    samples.extend([value, value]);
                    phase = (phase + 440.0 / 48_000.0).fract();
                }
                if pcm_tx.send(pcm_bytes(&samples)).await.is_err() {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        });
        let mut encoder = frames_core::audio::encode(FORMAT, pcm_rx).await.unwrap();
        let (mut seq, mut previous): (u32, Option<Vec<u8>>) = (0, None);
        while let Some(packet) = encoder.packets.recv().await {
            let body = packet_with_previous(&packet, previous.as_deref(), 1100).unwrap();
            let mut datagram = vec![TAG];
            datagram.extend(seq.to_le_bytes());
            datagram.extend(0u32.to_le_bytes());
            datagram.extend(body);
            receiver.push(&datagram);
            seq += 1;
            previous = Some(packet);
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let stats = &receiver.stats;
        let (packets, chunks) = (
            stats.packets.load(Ordering::Relaxed),
            stats.pcm_chunks.load(Ordering::Relaxed),
        );
        let crossings = stats.crossing_rate_x10.load(Ordering::Relaxed) as f64 / 10.0;
        println!("packets {packets} pcm {chunks} crossings/s {crossings}");
        assert!(packets >= 140, "{packets}");
        assert!(chunks >= 130, "{chunks}");
    }
}
