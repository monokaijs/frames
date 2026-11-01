use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use frames_core::audio::PcmFormat;
use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

pub struct AudioCapture {
    pub format: PcmFormat,
    pub packets: tokio::sync::mpsc::Receiver<Vec<u8>>,
    pub guard: Box<dyn crate::Capture>,
}
struct Guard(Arc<AtomicBool>);
impl crate::Capture for Guard {
    fn stop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

pub fn loopback() -> anyhow::Result<AudioCapture> {
    #[cfg(not(windows))]
    {
        anyhow::bail!("macOS system audio uses the ScreenCaptureKit helper")
    }
    #[cfg(windows)]
    {
        let (tx, packets) = tokio::sync::mpsc::channel(2);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let guard = Guard(stop.clone());
        let flag = stop.clone();
        std::thread::spawn(move || {
            let result = (|| -> anyhow::Result<_> {
                let host = cpal::default_host();
                let device = host
                    .default_output_device()
                    .ok_or_else(|| anyhow::anyhow!("no output device for system audio"))?;
                let config = device.default_output_config()?;
                let format = PcmFormat {
                    rate: config.sample_rate(),
                    channels: config.channels(),
                };
                let stream = match config.sample_format() {
                    cpal::SampleFormat::F32 => capture::<f32>(&device, config.into(), tx)?,
                    cpal::SampleFormat::I16 => capture::<i16>(&device, config.into(), tx)?,
                    cpal::SampleFormat::I32 => capture::<i32>(&device, config.into(), tx)?,
                    _ => anyhow::bail!("unsupported system audio format"),
                };
                stream.play()?;
                Ok((stream, format))
            })();
            match result {
                Ok((stream, format)) => {
                    let _ = ready_tx.send(Ok(format));
                    while !flag.load(Ordering::Acquire) {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    drop(stream);
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error.to_string()));
                }
            }
        });
        let format = ready_rx
            .recv_timeout(std::time::Duration::from_secs(5))?
            .map_err(anyhow::Error::msg)?;
        Ok(AudioCapture {
            format,
            packets,
            guard: Box::new(guard),
        })
    }
}
#[cfg(windows)]
fn capture<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    tx: tokio::sync::mpsc::Sender<Vec<u8>>,
) -> anyhow::Result<cpal::Stream>
where
    T: cpal::SizedSample + cpal::Sample,
    f32: cpal::FromSample<T>,
{
    Ok(device.build_input_stream(
        config,
        move |samples: &[T], _| {
            let bytes = samples
                .iter()
                .flat_map(|&value| cpal::Sample::to_sample::<f32>(value).to_le_bytes())
                .collect();
            let _ = tx.try_send(bytes);
        },
        |error| tracing::warn!(%error,"audio capture"),
        None,
    )?)
}

pub struct Playback {
    pub format: PcmFormat,
    queue: Arc<parking_lot::Mutex<PlaybackQueue>>,
    _guard: Guard,
}

#[derive(Default)]
struct PlaybackQueue {
    samples: VecDeque<f32>,
    callback_samples: usize,
}
impl PlaybackQueue {
    fn push(&mut self, pcm: &[u8], format: PcmFormat) {
        let channels = format.channels as usize;
        // Preserve whole interleaved frames, including when trimming a backlog.
        let frames = pcm.len() / (channels * 4);
        self.samples.extend(
            pcm[..frames * channels * 4]
                .chunks_exact(4)
                .map(|unit| f32::from_le_bytes(unit.try_into().unwrap())),
        );
        let jitter = format.rate as usize * channels * 20 / 1000;
        let max = self.callback_samples + jitter;
        let excess = self.samples.len().saturating_sub(max).div_ceil(channels) * channels;
        self.samples.drain(..excess);
    }

    fn fill<T: cpal::Sample + cpal::FromSample<f32>>(&mut self, output: &mut [T]) {
        // Device callbacks may be longer than 20 ms. Retain enough audio for
        // their next request as well as the bounded network jitter allowance.
        self.callback_samples = self.callback_samples.max(output.len());
        for sample in output {
            *sample = cpal::Sample::from_sample(self.samples.pop_front().unwrap_or(0.0));
        }
    }
}

impl Playback {
    pub fn new() -> anyhow::Result<Self> {
        let queue = Arc::new(parking_lot::Mutex::new(PlaybackQueue::default()));
        let samples = queue.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let guard = Guard(stop.clone());
        let flag = stop.clone();
        std::thread::spawn(move || {
            let result = (|| -> anyhow::Result<_> {
                let device = cpal::default_host()
                    .default_output_device()
                    .ok_or_else(|| anyhow::anyhow!("no playback device"))?;
                let config = device.default_output_config()?;
                let format = PcmFormat {
                    rate: config.sample_rate(),
                    channels: config.channels(),
                };
                let stream = match config.sample_format() {
                    cpal::SampleFormat::F32 => output::<f32>(&device, config.into(), samples)?,
                    cpal::SampleFormat::I16 => output::<i16>(&device, config.into(), samples)?,
                    cpal::SampleFormat::I32 => output::<i32>(&device, config.into(), samples)?,
                    _ => anyhow::bail!("unsupported audio output format"),
                };
                stream.play()?;
                Ok((stream, format))
            })();
            match result {
                Ok((stream, format)) => {
                    let _ = ready_tx.send(Ok(format));
                    while !flag.load(Ordering::Acquire) {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    drop(stream);
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error.to_string()));
                }
            }
        });
        let format = ready_rx
            .recv_timeout(std::time::Duration::from_secs(5))?
            .map_err(anyhow::Error::msg)?;
        Ok(Self {
            format,
            queue,
            _guard: guard,
        })
    }
    pub fn push(&self, pcm: &[u8]) {
        self.queue.lock().push(pcm, self.format);
    }
}
fn output<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    queue: Arc<parking_lot::Mutex<PlaybackQueue>>,
) -> anyhow::Result<cpal::Stream>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    Ok(device.build_output_stream(
        config,
        move |samples: &mut [T], _| {
            queue.lock().fill(samples);
        },
        |error| tracing::warn!(%error,"audio output"),
        None,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playback_retains_audio_for_callbacks_longer_than_twenty_milliseconds() {
        let format = PcmFormat {
            rate: 48000,
            channels: 2,
        };
        let mut queue = PlaybackQueue::default();
        let mut output = vec![0f32; 4800 * 2]; // A 100 ms device callback.
        queue.fill(&mut output);
        let packet: Vec<_> = std::iter::repeat_n(0.25f32, 480 * 2)
            .flat_map(f32::to_le_bytes)
            .collect();
        for _ in 0..10 {
            queue.push(&packet, format);
        }
        queue.fill(&mut output);
        assert!(output.iter().all(|&sample| sample == 0.25));
        // Keep the most recent callback plus 20 ms when a backlog builds.
        for _ in 0..20 {
            queue.push(&packet, format);
        }
        assert_eq!(queue.samples.len(), 5760 * 2);
    }

    #[test]
    fn trimming_playback_preserves_channel_alignment() {
        let format = PcmFormat {
            rate: 11025,
            channels: 2,
        };
        let pcm: Vec<_> = (0..400)
            .flat_map(|_| [0.25f32, -0.25])
            .flat_map(f32::to_le_bytes)
            .collect();
        let mut queue = PlaybackQueue::default();
        queue.push(&pcm, format);
        let mut output = [0f32; 2];
        queue.fill(&mut output);
        assert_eq!(output, [0.25, -0.25]);
    }
}
