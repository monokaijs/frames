use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use frames_core::audio::PcmFormat;
use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
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
                    cpal::SampleFormat::F32 => {
                        capture::<f32>(&device, config.into(), tx, flag.clone())?
                    }
                    cpal::SampleFormat::I16 => {
                        capture::<i16>(&device, config.into(), tx, flag.clone())?
                    }
                    cpal::SampleFormat::I32 => {
                        capture::<i32>(&device, config.into(), tx, flag.clone())?
                    }
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
    stop: Arc<AtomicBool>,
) -> anyhow::Result<cpal::Stream>
where
    T: cpal::SizedSample + cpal::Sample,
    f32: cpal::FromSample<T>,
{
    let maximum = config.sample_rate as usize * config.channels as usize * 400 / 1000;
    let ring = Arc::new((
        std::sync::Mutex::new(VecDeque::<f32>::with_capacity(maximum)),
        std::sync::Condvar::new(),
    ));
    let worker = ring.clone();
    std::thread::spawn(move || {
        while !stop.load(Ordering::Acquire) {
            let mut samples = worker.0.lock().unwrap();
            while samples.is_empty() && !stop.load(Ordering::Acquire) {
                samples = worker
                    .1
                    .wait_timeout(samples, std::time::Duration::from_millis(100))
                    .unwrap()
                    .0;
            }
            let bytes: Vec<u8> = samples.drain(..).flat_map(f32::to_le_bytes).collect();
            drop(samples);
            if !bytes.is_empty() {
                let _ = tx.try_send(bytes);
            }
        }
    });
    Ok(device.build_input_stream(
        config,
        move |samples: &[T], _| {
            if samples.len() > maximum {
                return;
            }
            if let Ok(mut queue) = ring.0.try_lock() {
                let excess = (queue.len() + samples.len()).saturating_sub(maximum);
                queue.drain(..excess);
                queue.extend(samples.iter().map(|&v| cpal::Sample::to_sample::<f32>(v)));
                drop(queue);
                ring.1.notify_one();
            }
        },
        |error| tracing::warn!(%error,"audio capture"),
        None,
    )?)
}

#[derive(Default)]
pub struct PlaybackStats {
    pub underruns: AtomicU64,
    pub dropped_samples: AtomicU64,
}

pub struct Playback {
    pub stats: Arc<PlaybackStats>,
    pub format: PcmFormat,
    queue: Arc<parking_lot::Mutex<PlaybackQueue>>,
    _guard: Guard,
}

#[derive(Default)]
struct PlaybackQueue {
    samples: VecDeque<f32>,
    callback_samples: usize,
    channels: usize,
    phase: f64,
    step: f64,
    integral: f64,
    stats: Arc<PlaybackStats>,
}
impl PlaybackQueue {
    fn push(&mut self, pcm: &[u8], format: PcmFormat) {
        let channels = format.channels as usize;
        self.channels = channels;
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
        self.stats
            .dropped_samples
            .fetch_add(excess as u64, Ordering::Relaxed);
        let target = (self.callback_samples + jitter / 2).max(channels);
        let error = (self.samples.len() as f64 - target as f64) / target as f64;
        self.integral = (self.integral + error * 0.000001).clamp(-0.0008, 0.0008);
        self.step = 1.0 + (error * 0.0004 + self.integral).clamp(-0.001, 0.001);
    }

    fn fill<T: cpal::Sample + cpal::FromSample<f32>>(&mut self, output: &mut [T]) {
        // Device callbacks may be longer than 20 ms. Retain enough audio for
        // their next request as well as the bounded network jitter allowance.
        self.callback_samples = self.callback_samples.max(output.len());
        let channels = self.channels.max(1);
        let mut underrun = false;
        for frame in output.chunks_mut(channels) {
            if self.samples.len() < channels {
                for sample in frame {
                    *sample = cpal::Sample::from_sample(0.0);
                }
                underrun = true;
                continue;
            }
            for (channel, sample) in frame.iter_mut().enumerate() {
                let a = self.samples[channel];
                let b = self.samples.get(channels + channel).copied().unwrap_or(a);
                *sample = cpal::Sample::from_sample(a + (b - a) * self.phase as f32);
            }
            self.phase += if self.step == 0.0 { 1.0 } else { self.step };
            let consume = self.phase.floor() as usize;
            self.phase -= consume as f64;
            for _ in 0..consume.min(2) * channels {
                self.samples.pop_front();
            }
        }
        if underrun {
            self.stats.underruns.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl Playback {
    pub fn new() -> anyhow::Result<Self> {
        let queue = Arc::new(parking_lot::Mutex::new(PlaybackQueue::default()));
        let samples = queue.clone();
        let stats = queue.lock().stats.clone();
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
                {
                    let mut queue = samples.lock();
                    queue.channels = format.channels as usize;
                    queue.samples = VecDeque::with_capacity(
                        format.rate as usize * format.channels as usize / 4,
                    );
                }
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
            stats,
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
    let stats = queue.lock().stats.clone();
    Ok(device.build_output_stream(
        config,
        move |samples: &mut [T], _| {
            if let Some(mut queue) = queue.try_lock() {
                queue.fill(samples);
            } else {
                samples.fill(cpal::Sample::from_sample(0.0));
                stats.underruns.fetch_add(1, Ordering::Relaxed);
            }
        },
        |error| tracing::warn!(%error,"audio output"),
        None,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn playback_clock_tracks_faster_and_slower_devices_without_growing_a_backlog() {
        let format = PcmFormat {
            rate: 48_000,
            channels: 2,
        };
        let pcm = |frames: usize| {
            (0..frames)
                .flat_map(|_| [0.25f32, -0.25])
                .flat_map(f32::to_le_bytes)
                .collect::<Vec<_>>()
        };
        let normal = pcm(480);
        let faster = pcm(481);
        let slower = pcm(479);
        for changed in [&faster, &slower] {
            let mut queue = PlaybackQueue::default();
            let mut output = [0f32; 960];
            queue.fill(&mut output);
            queue.push(&normal, format);
            queue.push(&normal, format);
            let dropped = queue.stats.dropped_samples.load(Ordering::Relaxed);
            let underruns = queue.stats.underruns.load(Ordering::Relaxed);
            for n in 0..20_000 {
                queue.fill(&mut output);
                assert!(
                    output
                        .chunks_exact(2)
                        .all(|p| p[0] == 0.25 && p[1] == -0.25)
                );
                queue.push(if n % 10 == 0 { changed } else { &normal }, format);
            }
            assert_eq!(queue.stats.dropped_samples.load(Ordering::Relaxed), dropped);
            assert_eq!(queue.stats.underruns.load(Ordering::Relaxed), underruns);
            assert!((960..=2880).contains(&queue.samples.len()));
        }
    }

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
