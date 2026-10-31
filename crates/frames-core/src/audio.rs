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
                if !handle.block_on(async{tokio::select!{biased;_=stopping.changed()=>false,r=sender.send(packet)=>r.is_ok()}}){return;}
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
