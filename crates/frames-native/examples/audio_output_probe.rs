//! Measure a known 997 Hz test tone on the default output, without saving audio.
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let seconds = std::env::args()
        .nth(1)
        .map(|v| v.parse())
        .transpose()?
        .unwrap_or(8);
    let mut capture = frames_native::audio::loopback()?;
    println!("default_output_format={:?}", capture.format);
    let end = tokio::time::Instant::now() + Duration::from_secs(seconds);
    let mut report = tokio::time::Instant::now();
    let mut peak = 0f32;
    let mut tone = 0f64;
    let mut blocks = 0;
    while let Ok(Some(bytes)) = tokio::time::timeout_at(end, capture.packets.recv()).await {
        let samples: Vec<_> = bytes
            .chunks_exact(capture.format.channels as usize * 4)
            .map(|frame| f32::from_le_bytes(frame[..4].try_into().unwrap()))
            .collect();
        let mut real = 0.;
        let mut imaginary = 0.;
        for (index, &sample) in samples.iter().enumerate() {
            peak = peak.max(sample.abs());
            let angle = std::f64::consts::TAU * 997. * index as f64 / capture.format.rate as f64;
            real += sample as f64 * angle.cos();
            imaginary += sample as f64 * angle.sin();
        }
        if !samples.is_empty() {
            tone += 2. * real.hypot(imaginary) / samples.len() as f64;
            blocks += 1;
        }
        if report.elapsed() >= Duration::from_millis(500) {
            println!(
                "blocks={blocks} peak={peak:.5} tone997={:.5}",
                tone / blocks.max(1) as f64
            );
            report = tokio::time::Instant::now();
            peak = 0.;
            tone = 0.;
            blocks = 0;
        }
    }
    capture.guard.stop();
    Ok(())
}
