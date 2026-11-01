//! Raw 10 ms Opus packets, without Ogg pages or subprocess buffering.
use crate::{codec::runtime::Resampler, worker::Worker};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, watch};
pub const PACKET_MILLIS: usize = 10;
const OPUS_FORMAT: PcmFormat = PcmFormat {
    rate: 48000,
    channels: 2,
};
const OPUS_SAMPLES: usize = 480 * 2;
pub fn packet_bytes(format: PcmFormat) -> usize {
    format.rate as usize * format.channels as usize * 4 * PACKET_MILLIS / 1000
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PcmFormat {
    pub rate: u32,
    pub channels: u16,
}
fn validate(format: PcmFormat) -> anyhow::Result<()> {
    anyhow::ensure!(
        (8000..=192000).contains(&format.rate) && (1..=8).contains(&format.channels),
        "Invalid PCM format"
    );
    Ok(())
}
pub struct Encoder {
    pub worker: Worker,
    pub packets: mpsc::Receiver<Vec<u8>>,
}
impl Encoder {
    pub async fn shutdown(&mut self) {
        self.worker.shutdown().await;
    }
}
pub async fn encode(
    format: PcmFormat,
    mut input: mpsc::Receiver<Vec<u8>>,
) -> anyhow::Result<Encoder> {
    validate(format)?;
    let (sender, packets) = mpsc::channel(1);
    let (stop, mut stopping) = watch::channel(false);
    let (ready, started) = oneshot::channel();
    let handle = tokio::runtime::Handle::current();
    // Native callbacks can contain many 10 ms frames. Deliver them at their
    // playback rate so a single callback cannot flood the receiver's decoder.
    let mut clock = tokio::time::interval(std::time::Duration::from_millis(PACKET_MILLIS as u64));
    clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let task = tokio::task::spawn_blocking(move || {
        let setup = (|| -> anyhow::Result<_> {
            let mut encoder =
                opus::Encoder::new(48000, opus::Channels::Stereo, opus::Application::LowDelay)?;
            encoder.set_bitrate(opus::Bitrate::Bits(128000))?;
            encoder.set_complexity(5)?;
            let resampler = Resampler::new(format, OPUS_FORMAT)?;
            Ok((encoder, resampler))
        })();
        let (mut encoder, mut resampler) = match setup {
            Ok(v) => {
                let _ = ready.send(Ok(()));
                v
            }
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
        let mut pending = Vec::new();
        loop {
            let bytes = handle.block_on(async {
                tokio::select! {biased;_=stopping.changed()=>None,b=input.recv()=>b}
            });
            let Some(bytes) = bytes else { break };
            if bytes.len() > 192000 * 8 * 4
                || !bytes.len().is_multiple_of(format.channels as usize * 4)
            {
                break;
            }
            let samples: Vec<f32> = bytes
                .chunks_exact(4)
                .map(|v| {
                    let v = f32::from_le_bytes(v.try_into().unwrap());
                    if v.is_finite() { v.clamp(-1., 1.) } else { 0. }
                })
                .collect();
            let samples = match resampler.convert(&samples) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error=%e,"Audio resample failed");
                    break;
                }
            };
            pending.extend(samples);
            let complete = pending.len() / OPUS_SAMPLES * OPUS_SAMPLES;
            for samples in pending[..complete].chunks_exact(OPUS_SAMPLES) {
                let mut packet = vec![0; 4000];
                match encoder.encode_float(samples, &mut packet) {
                    Ok(n) => packet.truncate(n),
                    Err(e) => {
                        tracing::warn!(error=%e,"Opus encode failed");
                        return;
                    }
                }
                if !handle.block_on(async {
                    tokio::select! {
                        biased;
                        _ = stopping.changed() => false,
                        result = async {
                            clock.tick().await;
                            sender.send(packet).await
                        } => result.is_ok(),
                    }
                }) {
                    return;
                }
            }
            pending.drain(..complete);
        }
    });
    let worker = Worker::new(stop, task);
    started.await??;
    Ok(Encoder { worker, packets })
}
pub struct Decoder {
    pub worker: Worker,
    pub input: mpsc::Sender<Vec<u8>>,
    pub pcm: mpsc::Receiver<Vec<u8>>,
}
impl Decoder {
    pub async fn shutdown(&mut self) {
        self.worker.shutdown().await;
    }
}
pub async fn decode(format: PcmFormat) -> anyhow::Result<Decoder> {
    validate(format)?;
    let (input, mut incoming) = mpsc::channel::<Vec<u8>>(2);
    let (sender, pcm) = mpsc::channel(2);
    let (stop, mut stopping) = watch::channel(false);
    let (ready, started) = oneshot::channel();
    let handle = tokio::runtime::Handle::current();
    let task = tokio::task::spawn_blocking(move || {
        let setup = (|| -> anyhow::Result<_> {
            Ok((
                opus::Decoder::new(48000, opus::Channels::Stereo)?,
                Resampler::new(OPUS_FORMAT, format)?,
            ))
        })();
        let (mut decoder, mut resampler) = match setup {
            Ok(v) => {
                let _ = ready.send(Ok(()));
                v
            }
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
        let mut buffer = vec![0.; OPUS_SAMPLES];
        loop {
            let packet = handle.block_on(async {
                tokio::select! {biased;_=stopping.changed()=>None,p=incoming.recv()=>p}
            });
            let Some(packet) = packet else { break };
            if packet.len() > 4000 {
                continue;
            }
            // Empty packet requests one 10 ms Opus loss-concealment frame.
            let count = match decoder.decode_float(&packet, &mut buffer, false) {
                Ok(n) if n == 480 => n,
                _ => continue,
            };
            let samples = match resampler.convert(&buffer[..count * 2]) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error=%e,"Audio resample failed");
                    break;
                }
            };
            let bytes = samples.into_iter().flat_map(f32::to_le_bytes).collect();
            if !handle.block_on(async {
                tokio::select! {biased;_=stopping.changed()=>false,r=sender.send(bytes)=>r.is_ok()}
            }) {
                break;
            }
        }
    });
    let worker = Worker::new(stop, task);
    started.await??;
    Ok(Decoder { worker, input, pcm })
}

/// Codec-independent redundancy also protects CELT music packets, where Opus
/// in-band FEC is not available. Lengths are checked before taking any slice.
pub fn packet_with_previous(
    current: &[u8],
    previous: Option<&[u8]>,
    capacity: usize,
) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        !current.is_empty() && current.len() <= 4000,
        "invalid Opus packet"
    );
    let previous = previous
        .filter(|p| p.len() <= 4000 && 6 + current.len() + p.len() <= capacity)
        .unwrap_or_default();
    let mut packet = Vec::with_capacity(6 + current.len() + previous.len());
    packet.extend_from_slice(b"A6");
    packet.extend_from_slice(&(current.len() as u16).to_be_bytes());
    packet.extend_from_slice(&(previous.len() as u16).to_be_bytes());
    packet.extend_from_slice(current);
    packet.extend_from_slice(previous);
    Ok(packet)
}
pub fn split_packet(packet: &[u8]) -> Option<(&[u8], Option<&[u8]>)> {
    if packet.len() < 7 || &packet[..2] != b"A6" {
        return None;
    }
    let current = u16::from_be_bytes(packet[2..4].try_into().ok()?) as usize;
    let previous = u16::from_be_bytes(packet[4..6].try_into().ok()?) as usize;
    if current == 0 || current > 4000 || previous > 4000 || packet.len() != 6 + current + previous {
        return None;
    }
    Some((
        &packet[6..6 + current],
        if previous == 0 {
            None
        } else {
            Some(&packet[6 + current..])
        },
    ))
}

/// Sequence-aware, bounded reordering. No more than two 10 ms PLC packets are
/// emitted after a gap; long outages jump to fresh audio instead of replaying it.
#[derive(Default)]
pub struct JitterBuffer {
    pending: std::collections::BTreeMap<u64, Vec<u8>>,
    pub next: u64,
    pub received: u64,
    pub concealed: u64,
    pub deadline: Option<std::time::Instant>,
}
impl JitterBuffer {
    pub fn retained_bytes(&self) -> usize {
        self.pending.values().map(Vec::len).sum()
    }
    pub fn skip_before(&mut self, number: u64) {
        self.next = self.next.max(number);
        self.pending.retain(|n, _| *n >= self.next);
        self.deadline = None;
    }
    pub fn push(&mut self, number: u64, current: &[u8], previous: Option<&[u8]>) {
        if number < self.next || self.pending.contains_key(&number) {
            return;
        }
        self.received += 1;
        if let Some(previous) = previous
            && number > 0
            && number > self.next
            && self.pending.len() < 4
        {
            self.pending
                .entry(number - 1)
                .or_insert_with(|| previous.to_vec());
        }
        if self.pending.len() < 4 {
            self.pending.insert(number, current.to_vec());
        }
    }
    pub fn drain(&mut self, now: std::time::Instant, mut submit: impl FnMut(Vec<u8>)) {
        loop {
            while let Some(packet) = self.pending.remove(&self.next) {
                submit(packet);
                self.next += 1;
                self.deadline = None;
            }
            let Some((&first, _)) = self.pending.first_key_value() else {
                return;
            };
            let deadline = *self
                .deadline
                .get_or_insert(now + std::time::Duration::from_millis(10));
            if now < deadline && self.pending.len() < 3 {
                return;
            }
            for _ in 0..first.saturating_sub(self.next).min(2) {
                submit(Vec::new());
                self.concealed += 1;
            }
            self.next = first;
            self.deadline = None;
        }
    }
}
#[cfg(test)]
mod redundancy_tests {
    use super::*;
    #[test]
    fn reordering_duplicates_redundancy_and_outage_never_replay_old_audio() {
        let now = std::time::Instant::now();
        let mut jitter = JitterBuffer::default();
        let mut played = Vec::new();
        jitter.push(1, &[11], None);
        jitter.drain(now, |p| played.push(p));
        assert!(played.is_empty());
        jitter.push(0, &[10], None);
        jitter.drain(now, |p| played.push(p));
        assert_eq!(played, vec![vec![10], vec![11]]);
        jitter.push(1, &[11], None);
        jitter.push(3, &[13], Some(&[12]));
        jitter.drain(now, |p| played.push(p));
        assert_eq!(played, vec![vec![10], vec![11], vec![12], vec![13]]);
        jitter.push(100, &[100], None);
        jitter.drain(now, |p| played.push(p));
        jitter.drain(now + std::time::Duration::from_millis(10), |p| {
            played.push(p)
        });
        assert_eq!(jitter.concealed, 2);
        assert_eq!(jitter.next, 101);
        assert_eq!(&played[4..], &[vec![], vec![], vec![100]]);
        jitter.skip_before(110);
        jitter.push(109, &[109], None);
        assert_eq!(jitter.retained_bytes(), 0);
    }
    #[test]
    fn audio_redundancy_fits_mtu_and_rejects_truncation() {
        let packet = packet_with_previous(&[1, 2, 3], Some(&[4, 5]), 1200).unwrap();
        assert_eq!(
            split_packet(&packet),
            Some((&[1, 2, 3][..], Some(&[4, 5][..])))
        );
        let packet = packet_with_previous(&[1, 2, 3], Some(&[4, 5]), 9).unwrap();
        assert_eq!(split_packet(&packet), Some((&[1, 2, 3][..], None)));
        assert!(split_packet(&packet[..packet.len() - 1]).is_none());
    }
}
