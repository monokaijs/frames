//! Read-only production-session check against the generated Mac text fixture.
//! FRAMES_SCREEN_TEXT_SOURCE=source.png text_quality_probe IP:PORT REPORT.json
#[allow(dead_code)]
#[path = "../src/connection_history.rs"]
mod connection_history;
#[allow(dead_code)]
#[path = "../src/connection_preferences.rs"]
mod connection_preferences;
#[allow(dead_code)]
#[path = "../src/device_preview.rs"]
mod device_preview;
#[allow(dead_code)]
#[path = "../src/device_session.rs"]
mod device_session;
#[allow(dead_code)]
#[path = "../src/device_setup.rs"]
mod device_setup;
#[allow(dead_code)]
#[path = "../src/file_worker.rs"]
mod file_worker;
#[allow(dead_code)]
#[path = "../src/lifetime.rs"]
mod lifetime;
#[allow(dead_code)]
#[path = "../src/media_debug.rs"]
mod media_debug;
#[allow(dead_code)]
#[path = "../src/media_sender.rs"]
mod media_sender;
#[allow(dead_code, unused_imports)]
#[path = "../src/session.rs"]
mod session;
#[allow(dead_code)]
#[path = "../src/streaming.rs"]
mod streaming;
#[allow(dead_code)]
#[path = "../src/update.rs"]
mod update;

use anyhow::{Context, ensure};
use frames_native::{ClientEvent, ClientUpdate};
use std::time::{Duration, Instant};

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("frames=info,frames_core=info,frames_native=info,text_quality_probe=info")
        .with_writer(std::io::stderr)
        .init();
    let args: Vec<_> = std::env::args().collect();
    let address = args
        .get(1)
        .context("text_quality_probe IP:PORT REPORT.json")?
        .parse()?;
    let report = args.get(2).context("missing report path")?;
    let source = image::open(
        std::env::var_os("FRAMES_SCREEN_TEXT_SOURCE")
            .context("FRAMES_SCREEN_TEXT_SOURCE must name the generated text PNG")?,
    )?
    .to_rgba8();
    let runtime = tokio::runtime::Runtime::new()?;
    let (ui, updates) = frames_native::client_channel(32);
    let (events, receiver) = tokio::sync::mpsc::unbounded_channel();
    let job = runtime.spawn(session::client(
        address,
        None,
        ui,
        receiver,
        false,
        session::MediaOptions::default(),
        frames_core::model::ViewMode::Windows,
    ));
    let started = Instant::now();
    let cycle = std::env::var_os("FRAMES_QA_QUALITY_CYCLE").is_some();
    let presets = [
        frames_core::adaptive::QualityPreset::High,
        frames_core::adaptive::QualityPreset::Low,
        frames_core::adaptive::QualityPreset::Medium,
        frames_core::adaptive::QualityPreset::High,
    ];
    let mut stage = 0usize;
    let mut stage_frames = [0u64; 4];
    let mut quality_acks = 0u64;
    let mut target = None;
    let mut frames = 0;
    let mut samples = Vec::new();
    let mut last_picture = None;
    let mut last_sample = started - Duration::from_secs(1);
    let result = (|| -> anyhow::Result<()> {
        while started.elapsed() < Duration::from_secs(if cycle { 40 } else { 30 }) {
            if cycle {
                let wanted = (started.elapsed().as_secs() as usize / 10).min(3);
                if wanted != stage {
                    stage = wanted;
                    events.send(ClientEvent::Quality(presets[stage]))?;
                }
            }
            ensure!(
                !job.is_finished(),
                "production session ended before the quality check"
            );
            let update = match updates.recv_timeout(Duration::from_millis(100)) {
                Ok(update) => update,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(error) => return Err(error.into()),
            };
            match update {
                ClientUpdate::Windows(windows) => {
                    for window in windows {
                        let fixture = window.app_id == "dev.frames.qa.fixture"
                            && window.title.starts_with("Frames QA source");
                        if fixture {
                            target = Some(window.key);
                        }
                        events.send(ClientEvent::Visibility {
                            window: window.key,
                            visible: fixture,
                            focused: fixture,
                        })?;
                    }
                }
                ClientUpdate::Frame(key, frame) if Some(key) == target => {
                    let expected = if cycle && stage == 1 {
                        (source.width() * 75 / 100, source.height() * 75 / 100)
                    } else {
                        source.dimensions()
                    };
                    if (frame.width, frame.height) != expected
                        && cycle
                        && started.elapsed().as_secs() % 10 < 4
                    {
                        continue;
                    }
                    ensure!(
                        (frame.width, frame.height) == expected,
                        "preset {:?} delivered {}x{} instead of {:?}",
                        presets[stage],
                        frame.width,
                        frame.height,
                        expected
                    );
                    stage_frames[stage] += 1;
                    if cycle && stage == 1 {
                        frames += 1;
                        continue;
                    }
                    frames += 1;
                    if last_sample.elapsed() < Duration::from_secs(1) {
                        continue;
                    }
                    last_sample = Instant::now();
                    let frame = frame.cpu_copy()?;
                    let mut total = 0u64;
                    let mut edges = 0u64;
                    for y in (20..source.height() - 20).step_by(2) {
                        for x in (20..source.width() - 20).step_by(2) {
                            let c = source.get_pixel(x, y)[0] as i32;
                            if (c - source.get_pixel(x - 1, y)[0] as i32).abs() > 40
                                || (c - source.get_pixel(x, y - 1)[0] as i32).abs() > 40
                            {
                                total += (c - frame.bgra
                                    [(y as usize * frame.width as usize + x as usize) * 4]
                                    as i32)
                                    .unsigned_abs() as u64;
                                edges += 1;
                            }
                        }
                    }
                    ensure!(edges > 1000, "the fixture contains no readable text edges");
                    let error = total as f64 / edges as f64;
                    last_picture = Some(frame);
                    samples.push(error);
                    ensure!(
                        error < 35.0,
                        "text is blurred after production delivery: edge error {error}"
                    );
                    eprintln!(
                        "text sample {:?} edge_error={error:.3}",
                        source.dimensions()
                    );
                }
                ClientUpdate::Quality(preset) => {
                    quality_acks += 1;
                    eprintln!("quality applied: {preset:?}");
                }
                ClientUpdate::Status(key, message) if Some(key) == target => {
                    ensure!(
                        !message.contains("failed") && !message.contains("stopped"),
                        "production session: {message}"
                    );
                }
                _ => {}
            }
        }
        ensure!(
            frames >= 30 && samples.len() >= 10,
            "not enough sustained text pictures: frames={frames}, samples={}",
            samples.len()
        );
        if cycle {
            ensure!(
                stage_frames.iter().all(|count| *count > 0) && quality_acks >= 4,
                "quality switching did not reach every preset: {stage_frames:?}, acknowledgements={quality_acks}"
            );
        }
        Ok(())
    })();
    let _ = events.send(ClientEvent::Disconnected);
    runtime.block_on(async {
        let _ = tokio::time::timeout(Duration::from_secs(3), job).await;
    });
    if let Some(picture) = last_picture {
        let rgba: Vec<u8> = picture
            .bgra
            .chunks_exact(4)
            .flat_map(|p| [p[2], p[1], p[0], 255])
            .collect();
        image::save_buffer(
            std::path::Path::new(report).with_extension("decoded.png"),
            &rgba,
            picture.width,
            picture.height,
            image::ColorType::Rgba8,
        )?;
    }
    std::fs::write(
        report,
        serde_json::to_vec_pretty(
            &serde_json::json!({"passed":result.is_ok(),"failure":result.as_ref().err().map(ToString::to_string),"width":source.width(),"height":source.height(),"frames":frames,"edge_error_samples":samples,"preset_frames":stage_frames,"quality_acknowledgements":quality_acks}),
        )?,
    )?;
    result
}
