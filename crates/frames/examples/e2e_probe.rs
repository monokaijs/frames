//! End-to-end client probe for scripts/e2e_matrix.py, on the production
//! session, codec and transport paths (no native UI).
//!
//! Only observes and configures the generated window titled
//! "Frames QA source"; never sends input and never persists pixels.
//!
//! e2e_probe IP:PORT REPORT.json [--audio] [--video-seconds N]
//!     [--marker-dir DIR --clipboard-expect TEXT --clipboard-send TEXT]
//!
//! Clipboard check (opt-in, changes this machine's clipboard and restores
//! it): after connecting the probe writes DIR/ready; the driver then sets
//! the host clipboard to the expected text, which must arrive here. The
//! probe then copies the send text locally and writes DIR/sent; the driver
//! checks that text on the host.
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
use frames_core::{adaptive::QualityPreset, model::*};
use frames_native::{ClientEvent, ClientReceiver, ClientUpdate};
use serde_json::{Map, Value, json};
use std::path::PathBuf;
use std::time::{Duration, Instant};

struct Options {
    address: std::net::SocketAddr,
    report: PathBuf,
    audio: bool,
    video_seconds: u64,
    marker_dir: Option<PathBuf>,
    clipboard_expect: Option<String>,
    clipboard_send: Option<String>,
}

fn options() -> anyhow::Result<Options> {
    let args: Vec<String> = std::env::args().collect();
    let value = |flag: &str| {
        args.iter()
            .position(|arg| arg == flag)
            .and_then(|i| args.get(i + 1).cloned())
    };
    Ok(Options {
        address: args
            .get(1)
            .context("usage: e2e_probe IP:PORT REPORT.json [options]")?
            .parse()?,
        report: args.get(2).context("missing report path")?.into(),
        audio: args.iter().any(|arg| arg == "--audio"),
        video_seconds: value("--video-seconds").map_or(Ok(10), |v| v.parse())?,
        marker_dir: value("--marker-dir").map(Into::into),
        clipboard_expect: value("--clipboard-expect"),
        clipboard_send: value("--clipboard-send"),
    })
}

/// Results by check name: "pass", "fail: reason", or "skip: reason".
#[derive(Default)]
struct Report {
    checks: Map<String, Value>,
    measurements: Map<String, Value>,
}

impl Report {
    fn check(&mut self, name: &str, result: anyhow::Result<String>) -> bool {
        let ok = result.is_ok();
        let value = match result {
            Ok(detail) => json!({"status": "pass", "detail": detail}),
            Err(error) => json!({"status": "fail", "detail": format!("{error:#}")}),
        };
        eprintln!("probe check {name}: {value}");
        self.checks.insert(name.into(), value);
        ok
    }

    fn skip(&mut self, name: &str, reason: &str) {
        self.checks
            .insert(name.into(), json!({"status": "skip", "detail": reason}));
    }

    fn measure(&mut self, name: &str, value: impl Into<Value>) {
        self.measurements.insert(name.into(), value.into());
    }
}

/// Frames of one window collected over a period.
#[derive(Default)]
struct VideoStats {
    frames: u64,
    first: Option<Instant>,
    last: Option<Instant>,
    latency_us: Vec<u64>,
    sizes: Vec<(u32, u32)>,
    overlays: u64,
    audio_line: Option<String>,
    quality_acks: Vec<QualityPreset>,
}

impl VideoStats {
    fn fps(&self) -> f64 {
        match (self.first, self.last) {
            (Some(first), Some(last)) if self.frames > 1 => {
                (self.frames - 1) as f64 / last.duration_since(first).as_secs_f64().max(1e-3)
            }
            _ => 0.0,
        }
    }

    fn percentile_ms(&self, percent: usize) -> f64 {
        let mut sorted = self.latency_us.clone();
        sorted.sort_unstable();
        sorted
            .get(sorted.len().saturating_sub(1) * percent / 100)
            .map_or(0.0, |us| *us as f64 / 1000.0)
    }
}

struct Session<'a> {
    updates: &'a ClientReceiver,
    events: &'a tokio::sync::mpsc::UnboundedSender<ClientEvent>,
    window: WindowId,
}

impl Session<'_> {
    fn send(&self, event: ClientEvent) -> anyhow::Result<()> {
        self.events
            .send(event)
            .map_err(|_| anyhow::anyhow!("session event queue closed"))
    }

    /// Collects updates for `duration`, or until `stop` returns true.
    fn collect(
        &self,
        duration: Duration,
        mut stop: impl FnMut(&VideoStats) -> bool,
    ) -> anyhow::Result<VideoStats> {
        let mut stats = VideoStats::default();
        let end = Instant::now() + duration;
        while Instant::now() < end && !stop(&stats) {
            let update = match self.updates.recv_timeout(Duration::from_millis(20)) {
                Ok(update) => update,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => anyhow::bail!("session ended"),
            };
            match update {
                ClientUpdate::Frame(key, frame) if key == self.window => {
                    ensure!(frame.validate(), "invalid decoded frame");
                    let now = Instant::now();
                    stats.frames += 1;
                    stats.first.get_or_insert(now);
                    stats.last = Some(now);
                    stats
                        .latency_us
                        .push(now.saturating_duration_since(frame.captured_at).as_micros() as u64);
                    stats.sizes.push((frame.width, frame.height));
                    // Emulate presentation so the client's media worker sees
                    // the same acknowledgements as with a native window.
                    self.send(ClientEvent::Presented {
                        window: key,
                        generation: frame.media_generation,
                        number: frame.frame_number,
                        at: now,
                    })?;
                }
                ClientUpdate::DebugOverlay(key, overlay) if key == self.window => {
                    stats.overlays += 1;
                    if let Some(line) = overlay.lines.iter().find(|l| l.text.starts_with("Audio")) {
                        stats.audio_line = Some(line.text.clone());
                    }
                }
                ClientUpdate::Quality(preset) => stats.quality_acks.push(preset),
                ClientUpdate::Shutdown => anyhow::bail!("session shut down"),
                _ => {}
            }
        }
        Ok(stats)
    }
}

fn main() -> anyhow::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("gamepad-worker") {
        let _lease = update::runtime_lease()?;
        return frames_native::gamepad::capture_worker();
    }
    tracing_subscriber::fmt()
        .with_env_filter("warn,frames=info,e2e_probe=info,frames_core=info,frames_native=info")
        .with_writer(std::io::stderr)
        .init();
    let options = options()?;
    let clipboard = options.clipboard_expect.is_some() && options.clipboard_send.is_some();
    let local = if clipboard {
        Some(frames_native::desktop(vec![])?)
    } else {
        None
    };
    let original_clipboard = local.as_ref().and_then(|d| d.clipboard().ok().flatten());

    let runtime = tokio::runtime::Runtime::new()?;
    let (ui, updates) = frames_native::client_channel(64);
    let (events, receiver) = tokio::sync::mpsc::unbounded_channel();
    let connect_started = Instant::now();
    let job = runtime.spawn(session::client(
        options.address,
        None,
        ui,
        receiver,
        clipboard,
        session::MediaOptions {
            quality: QualityPreset::High,
            audio: options.audio,
            bandwidth_mbps: 0,
        },
        ViewMode::Windows,
    ));
    let mut report = Report::default();
    let result = run(
        &options,
        &updates,
        &events,
        &mut report,
        local.as_deref(),
        connect_started,
    );
    if let Err(error) = &result {
        report.check("probe", Err(anyhow::anyhow!("{error:#}")));
    }

    // Disconnect: the session must end promptly once the UI goes away.
    let _ = events.send(ClientEvent::Disconnected);
    drop(events);
    let disconnect_started = Instant::now();
    let ended = runtime.block_on(async { tokio::time::timeout(Duration::from_secs(8), job).await });
    let disconnect_ms = disconnect_started.elapsed().as_millis() as u64;
    report.measure("disconnect_ms", disconnect_ms);
    report.check(
        "disconnect",
        match ended {
            Ok(Ok(Ok(()))) => Ok(format!("session ended in {disconnect_ms} ms")),
            Ok(Ok(Err(error))) => Err(anyhow::anyhow!("session ended with error: {error:#}")),
            Ok(Err(error)) => Err(anyhow::anyhow!("session task failed: {error}")),
            Err(_) => Err(anyhow::anyhow!("session still running after 8 s")),
        },
    );
    if let (Some(local), Some(original)) = (local.as_ref(), original_clipboard.as_deref()) {
        let _ = local.set_clipboard(original);
    }
    let failed = report
        .checks
        .values()
        .filter(|c| c["status"] == "fail")
        .count();
    let output = json!({
        "checks": report.checks,
        "measurements": report.measurements,
        "failed": failed,
        "pixels_persisted": false,
    });
    std::fs::write(&options.report, serde_json::to_vec_pretty(&output)?)?;
    println!("{}", serde_json::to_string_pretty(&output)?);
    runtime.shutdown_timeout(Duration::from_secs(2));
    anyhow::ensure!(failed == 0, "{failed} probe checks failed");
    Ok(())
}

fn run(
    options: &Options,
    updates: &ClientReceiver,
    events: &tokio::sync::mpsc::UnboundedSender<ClientEvent>,
    report: &mut Report,
    local: Option<&dyn frames_native::Desktop>,
    connect_started: Instant,
) -> anyhow::Result<()> {
    // 1. Window list with the generated fixture.
    let deadline = Instant::now() + Duration::from_secs(45);
    let target = loop {
        ensure!(
            Instant::now() < deadline,
            "no Frames QA source window within 45 s"
        );
        match updates.recv_timeout(Duration::from_millis(50)) {
            Ok(ClientUpdate::Windows(windows)) => {
                if let Some(window) = windows.into_iter().find(|w| w.title == "Frames QA source") {
                    break window;
                }
            }
            Ok(ClientUpdate::Shutdown) => anyhow::bail!("session shut down before the window list"),
            Ok(_) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => anyhow::bail!("session ended before the window list"),
        }
    };
    let snapshot_ms = connect_started.elapsed().as_millis() as u64;
    report.measure("snapshot_ms", snapshot_ms);
    let expected = (
        (target.bounds.width * target.scale).round() as u32,
        (target.bounds.height * target.scale).round() as u32,
    );
    report.check(
        "window_list",
        if target.bounds.width > 0. && target.bounds.height > 0. && target.visible {
            Ok(format!(
                "'{}' from {} at {}x{} pt, scale {}, after {snapshot_ms} ms",
                target.title,
                target.app_id,
                target.bounds.width,
                target.bounds.height,
                target.scale
            ))
        } else {
            Err(anyhow::anyhow!(
                "fixture window has no visible geometry: {:?}",
                target.bounds
            ))
        },
    );
    let session = Session {
        updates,
        events,
        window: target.key,
    };
    session.send(ClientEvent::Visibility {
        window: target.key,
        visible: true,
        focused: true,
    })?;

    // 2. Sustained video.
    let first = session.collect(Duration::from_secs(15), |stats| stats.frames > 0)?;
    ensure!(
        first.frames > 0,
        "no decoded frame within 15 s of subscribing"
    );
    let (width, height) = first.sizes[0];
    report.measure(
        "first_frame_ms",
        connect_started.elapsed().as_millis() as u64,
    );
    report.measure("frame_width", width);
    report.measure("frame_height", height);
    let video = session.collect(Duration::from_secs(options.video_seconds), |_| false)?;
    let fps = video.fps();
    report.measure("fps", (fps * 10.0).round() / 10.0);
    report.measure("capture_to_decoded_p50_ms", video.percentile_ms(50));
    report.measure("capture_to_decoded_p95_ms", video.percentile_ms(95));
    report.check(
        "video",
        (|| {
            // Mac windows report frame bounds including the standard title
            // bar, which capture excludes: allow a shorter height.
            ensure!(
                width.abs_diff(expected.0) <= 2 && height <= expected.1 + 2 && height > 0,
                "decoded {width}x{height}, window metadata {}x{}",
                expected.0,
                expected.1
            );
            ensure!(
                fps >= 20.0,
                "only {fps:.1} fps over {} s",
                options.video_seconds
            );
            Ok(format!(
                "{width}x{height} at {fps:.1} fps, capture→decoded p50 {:.1} ms p95 {:.1} ms",
                video.percentile_ms(50),
                video.percentile_ms(95)
            ))
        })(),
    );

    // 3. Debug overlay must not disturb streaming.
    session.send(ClientEvent::DebugOverlay { enabled: true })?;
    let overlay = session.collect(Duration::from_secs(4), |_| false)?;
    session.send(ClientEvent::DebugOverlay { enabled: false })?;
    report.check(
        "debug_overlay",
        (|| {
            ensure!(overlay.overlays > 0, "no overlay snapshot within 4 s");
            ensure!(
                overlay.fps() >= fps * 0.7,
                "fps dropped to {:.1} with the overlay (was {fps:.1})",
                overlay.fps()
            );
            Ok(format!(
                "{} snapshots, {:.1} fps while enabled",
                overlay.overlays,
                overlay.fps()
            ))
        })(),
    );
    if options.audio {
        report.check(
            "audio",
            overlay
                .audio_line
                .clone()
                .context("the client never started an audio stream"),
        );
    } else {
        report.skip("audio", "not requested");
    }

    // 4. Live image-quality change: Low scales the stream, High restores it.
    let started = Instant::now();
    session.send(ClientEvent::Quality(QualityPreset::Low))?;
    let low = session.collect(Duration::from_secs(10), |s| {
        s.quality_acks.contains(&QualityPreset::Low)
            && s.sizes.last().is_some_and(|size| size.0 < width)
    })?;
    let low_ms = started.elapsed().as_millis() as u64;
    let started = Instant::now();
    session.send(ClientEvent::Quality(QualityPreset::High))?;
    let high = session.collect(Duration::from_secs(10), |s| {
        s.quality_acks.contains(&QualityPreset::High)
            && s.sizes.last().is_some_and(|size| *size == (width, height))
    })?;
    let high_ms = started.elapsed().as_millis() as u64;
    report.measure("quality_low_ms", low_ms);
    report.measure("quality_high_ms", high_ms);
    report.check(
        "quality",
        (|| {
            ensure!(
                low.quality_acks.contains(&QualityPreset::Low),
                "Low was not acknowledged"
            );
            let low_size = low.sizes.last().copied().context("no frame at Low")?;
            ensure!(low_size.0 < width, "Low kept {}x{}", low_size.0, low_size.1);
            ensure!(
                high.quality_acks.contains(&QualityPreset::High),
                "High was not acknowledged"
            );
            let high_size = high.sizes.last().copied().context("no frame at High")?;
            ensure!(
                high_size == (width, height),
                "High returned {}x{}",
                high_size.0,
                high_size.1
            );
            Ok(format!(
                "Low {}x{} in {low_ms} ms, High {}x{} in {high_ms} ms",
                low_size.0, low_size.1, high_size.0, high_size.1
            ))
        })(),
    );

    // 5. Clipboard both ways (opt-in).
    match (
        local,
        &options.marker_dir,
        &options.clipboard_expect,
        &options.clipboard_send,
    ) {
        (Some(local), Some(dir), Some(expect), Some(send)) => {
            std::fs::write(dir.join("ready"), b"ready")?;
            let started = Instant::now();
            let mut seen = None;
            while started.elapsed() < Duration::from_secs(15) {
                // Keep the media worker drained while waiting.
                let _ = session.collect(Duration::from_millis(200), |_| false)?;
                seen = local.clipboard().ok().flatten();
                if seen.as_deref() == Some(expect.as_str()) {
                    break;
                }
            }
            let host_to_client_ms = started.elapsed().as_millis() as u64;
            let arrived = seen.as_deref() == Some(expect.as_str());
            if arrived {
                report.measure("clipboard_host_to_client_ms", host_to_client_ms);
            }
            report.check(
                "clipboard_host_to_client",
                if arrived {
                    Ok(format!("host text arrived in {host_to_client_ms} ms"))
                } else {
                    Err(anyhow::anyhow!("client clipboard is {seen:?} after 15 s"))
                },
            );
            local.set_clipboard(send)?;
            std::fs::write(dir.join("sent"), b"sent")?;
            // Give the client clipboard worker time to send before disconnect;
            // the driver verifies the host side.
            let _ = session.collect(Duration::from_secs(4), |_| false)?;
        }
        _ => report.skip("clipboard_host_to_client", "not requested"),
    }
    Ok(())
}
