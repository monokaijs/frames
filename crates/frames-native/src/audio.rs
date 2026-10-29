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
        let (tx, packets) = tokio::sync::mpsc::channel(8);
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
    queue: Arc<parking_lot::Mutex<VecDeque<f32>>>,
    _guard: Guard,
}
impl Playback {
    pub fn new() -> anyhow::Result<Self> {
        let queue = Arc::new(parking_lot::Mutex::new(VecDeque::<f32>::new()));
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
        let mut queue = self.queue.lock();
        let max = self.format.rate as usize * self.format.channels as usize / 10;
        for unit in pcm.as_chunks::<4>().0 {
            queue.push_back(f32::from_le_bytes(*unit));
        }
        while queue.len() > max {
            queue.pop_front();
        }
    }
}
fn output<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    queue: Arc<parking_lot::Mutex<VecDeque<f32>>>,
) -> anyhow::Result<cpal::Stream>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    Ok(device.build_output_stream(
        config,
        move |samples: &mut [T], _| {
            let mut queue = queue.lock();
            for sample in samples {
                *sample = cpal::Sample::from_sample(queue.pop_front().unwrap_or(0.0));
            }
        },
        |error| tracing::warn!(%error,"audio output"),
        None,
    )?)
}
