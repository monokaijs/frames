//! LAN acceptance client using the production session, codec and transport paths.
//! Only controls generated windows whose title begins with "Frames QA source".
//! Never persists captured pixels; reports metadata and measured restore time.
#[allow(dead_code)]
#[path = "../src/connection_history.rs"]
mod connection_history;
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
use frames_core::{input::InputEvent, model::*};
use frames_native::{ClientEvent, ClientUpdate};
use std::time::{Duration, Instant};

fn main() -> anyhow::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("gamepad-worker") {
        let _lease = update::runtime_lease()?;
        return frames_native::gamepad::capture_worker();
    }
    tracing_subscriber::fmt()
        .with_env_filter("frames=info,control_probe=info,frames_core=info,frames_native=info")
        .with_writer(std::io::stderr)
        .init();
    let args: Vec<String> = std::env::args().collect();
    let address = args
        .get(1)
        .context("usage: control_probe IP:PORT REPORT.json [--capture-only] [--desktop]")?
        .parse()?;
    let report_path = args.get(2).context("missing report path")?;
    let capture_only = args.iter().any(|v| v == "--capture-only");
    let test_desktop = args.iter().any(|v| v == "--desktop");
    let latency_mode = args.iter().any(|v| v == "--latency-only");
    let focused = args.iter().any(|v| v == "--focused");
    let skip_pause = args.iter().any(|v| v == "--skip-pause");
    let large = args.iter().any(|v| v == "--1080p");
    let runtime = tokio::runtime::Runtime::new()?;
    let (ui, updates) = frames_native::client_channel(32);
    let (events, receiver) = tokio::sync::mpsc::unbounded_channel();
    let job = runtime.spawn(session::client(
        address,
        None,
        ui,
        receiver,
        false,
        Default::default(),
        ViewMode::Windows,
    ));
    let mut passed = Vec::<String>::new();
    let mut measurements = serde_json::Map::new();
    measurements.insert(
        "requested_fps".into(),
        (if focused { 60 } else { 24 }).into(),
    );
    let result = (|| -> anyhow::Result<()> {
        let start = Instant::now();
        let mut target: Option<RemoteWindow> = None;
        let mut stage = "discover";
        let mut reported_stage = "";
        let mut pause_until = start;
        let mut paused_at = start;
        let mut resumed_at = start;
        let mut requested_size = (0., 0.);
        let mut desktop_key = None;
        let mut click_started = start;
        let mut close_started = start;
        let mut latency_samples = Vec::<u64>::new();
        let mut next_click = start;
        let send = |event| {
            events
                .send(event)
                .map_err(|_| anyhow::anyhow!("session event queue closed"))
        };
        while start.elapsed() < Duration::from_secs(110) {
            if reported_stage != stage {
                eprintln!(
                    "probe stage={stage} elapsed_ms={}",
                    start.elapsed().as_millis()
                );
                reported_stage = stage;
            }
            if stage == "latency_wait" && Instant::now() >= next_click {
                let target = target.as_ref().unwrap();
                ensure!(
                    target.app_id == "dev.frames.qa.fixture",
                    "latency test only targets generated fixtures"
                );
                click_started = Instant::now();
                for down in [true, false] {
                    send(ClientEvent::Input {
                        window: target.key,
                        event: InputEvent::Button {
                            button: 0,
                            down,
                            x: 100. / target.bounds.width,
                            y: (target.bounds.height - if large { 216. } else { 96. })
                                / target.bounds.height,
                        },
                    })?;
                }
                stage = "latency_video";
            }
            ensure!(
                stage != "latency_video" || click_started.elapsed() < Duration::from_secs(5),
                "no updated pixels within five seconds after the test click"
            );
            if job.is_finished() {
                anyhow::bail!("session ended before fixture checkpoints; inspect host/client logs");
            }
            if stage == "paused" && Instant::now() >= pause_until {
                passed.push("decoded frames stop while unsubscribed".into());
                resumed_at = Instant::now();
                send(ClientEvent::Visibility {
                    window: target.as_ref().unwrap().key,
                    visible: true,
                    focused,
                })?;
                stage = "resuming";
            }
            let update = match updates.recv_timeout(Duration::from_millis(50)) {
                Ok(update) => update,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(error) => return Err(error.into()),
            };
            match update {
                ClientUpdate::Windows(windows) => {
                    if stage == "discover" {
                        if let Some(window) = windows.iter().find(|w| w.title == "Frames QA source")
                        {
                            ensure!(window.icon_png.is_some(), "fixture app icon missing");
                            target = Some(window.clone());
                            passed.push("window snapshot with title and icon".into());
                            send(ClientEvent::Visibility {
                                window: window.key,
                                visible: true,
                                focused,
                            })?;
                            stage = "first_frame";
                        }
                    } else if stage == "marked"
                        && windows.iter().any(|w| {
                            Some(w.key) == target.as_ref().map(|w| w.key)
                                && w.title == "Frames QA source marked"
                        })
                    {
                        passed.push("remote native menu command changed fixture title".into());
                        let current = windows
                            .iter()
                            .find(|w| Some(w.key) == target.as_ref().map(|w| w.key))
                            .unwrap();
                        ensure!(
                            current.resize == ResizeCapability::Supported,
                            "fixture remote resize was not detected"
                        );
                        requested_size = (current.bounds.width + 80., current.bounds.height + 40.);
                        send(ClientEvent::Action {
                            window: current.key,
                            action: WindowAction::Resize {
                                width: requested_size.0,
                                height: requested_size.1,
                            },
                        })?;
                        stage = "resized";
                    } else if stage == "text"
                        && windows.iter().any(|w| {
                            Some(w.key) == target.as_ref().map(|w| w.key)
                                && w.title == "Frames QA source text: Frames Việt ✓"
                        })
                    {
                        passed.push("Unicode input reached host fixture".into());
                        if test_desktop {
                            send(ClientEvent::ViewMode(ViewMode::Desktop))?;
                            stage = "desktop_snapshot";
                        } else {
                            close_started = Instant::now();
                            send(ClientEvent::Action {
                                window: target.as_ref().unwrap().key,
                                action: WindowAction::Close,
                            })?;
                            stage = "closed";
                        }
                    } else if stage == "desktop_snapshot" {
                        if let Some(desktop) =
                            windows.iter().find(|w| w.role == WindowRole::Desktop)
                        {
                            desktop_key = Some(desktop.key);
                            send(ClientEvent::Visibility {
                                window: desktop.key,
                                visible: true,
                                focused,
                            })?;
                            stage = "desktop_frame";
                        }
                    } else if stage == "back_to_windows"
                        && windows.iter().any(|w| {
                            w.app_id == target.as_ref().unwrap().app_id
                                && w.title == "Frames QA source text: Frames Việt ✓"
                        })
                        && windows.iter().all(|w| w.role != WindowRole::Desktop)
                    {
                        passed.push("desktop -> per-window snapshot reconciled".into());
                        // Switching capture modes reconciles a fresh graph and may
                        // assign a new generation to the same underlying OS window.
                        target = windows
                            .iter()
                            .find(|w| {
                                w.app_id == target.as_ref().unwrap().app_id
                                    && w.title == "Frames QA source text: Frames Việt ✓"
                            })
                            .cloned();
                        send(ClientEvent::Action {
                            window: target.as_ref().unwrap().key,
                            action: WindowAction::Close,
                        })?;
                        stage = "closed";
                        close_started = Instant::now();
                    } else if stage == "closed"
                        && windows
                            .iter()
                            .all(|w| Some(w.key) != target.as_ref().map(|w| w.key))
                    {
                        passed.push("remote close removes fixture identity".into());
                        let millis = close_started.elapsed().as_millis() as u64;
                        measurements.insert("close_reconciled_ms".into(), millis.into());
                        ensure!(
                            millis < 2000,
                            "closing the generated window took {millis} ms"
                        );
                        return Ok(());
                    } else if stage == "click"
                        && windows.iter().any(|w| {
                            Some(w.key) == target.as_ref().map(|w| w.key)
                                && w.title == "Frames QA source clicked"
                        })
                    {
                        let millis = click_started.elapsed().as_millis() as u64;
                        measurements.insert("click_delivered_ms".into(), millis.into());
                        ensure!(millis < 2000, "click delivery took {millis} ms");
                        passed.push("mouse down/up activated the generated host button".into());
                        send(ClientEvent::Input {
                            window: target.as_ref().unwrap().key,
                            event: InputEvent::Text {
                                text: "Frames Việt ✓".into(),
                            },
                        })?;
                        stage = "text";
                    }
                }
                ClientUpdate::Status(key, status)
                    if Some(key) == target.as_ref().map(|w| w.key) =>
                {
                    eprintln!(
                        "probe codec={status} stage={stage} elapsed_ms={}",
                        start.elapsed().as_millis()
                    );
                    measurements.insert("video_codec_status".into(), status.into());
                }
                ClientUpdate::Frame(key, frame) if Some(key) == target.as_ref().map(|w| w.key) => {
                    ensure!(frame.validate(), "invalid decoded frame");
                    let frame = frame.cpu_copy()?;
                    if stage == "latency_video" {
                        let index =
                            ((frame.height / 2 * frame.width + frame.width / 2) * 4) as usize;
                        let green = frame.bgra[index + 1] > 150
                            && frame.bgra[index + 2] < 100
                            && frame.bgra[index] < 100;
                        let red = frame.bgra[index + 2] > 150
                            && frame.bgra[index + 1] < 100
                            && frame.bgra[index] < 100;
                        if (latency_samples.len().is_multiple_of(2) && green)
                            || (!latency_samples.len().is_multiple_of(2) && red)
                        {
                            latency_samples.push(click_started.elapsed().as_millis() as u64);
                            if latency_samples.len() == 20 {
                                let mut sorted = latency_samples.clone();
                                sorted.sort_unstable();
                                measurements.insert(
                                    "click_to_decoded_video_samples_ms".into(),
                                    serde_json::json!(latency_samples),
                                );
                                measurements.insert(
                                    "click_to_decoded_video_p50_ms".into(),
                                    sorted[10].into(),
                                );
                                measurements.insert(
                                    "click_to_decoded_video_p95_ms".into(),
                                    sorted[18].into(),
                                );
                                passed.push(
                                    "twenty remote clicks changed pixels in decoded video".into(),
                                );
                                send(ClientEvent::Action {
                                    window: key,
                                    action: WindowAction::Close,
                                })?;
                                return Ok(());
                            }
                            stage = "latency_wait";
                            next_click = Instant::now() + Duration::from_millis(100);
                        }
                        continue;
                    }
                    if click_started != start
                        && !measurements.contains_key("click_to_decoded_video_ms")
                    {
                        let index =
                            ((frame.height / 2 * frame.width + frame.width / 2) * 4) as usize;
                        if frame.bgra[index + 1] > 150
                            && frame.bgra[index + 2] < 100
                            && frame.bgra[index] < 100
                        {
                            measurements.insert(
                                "click_to_decoded_video_ms".into(),
                                (click_started.elapsed().as_millis() as u64).into(),
                            );
                            passed.push("click changed pixels in the decoded live video".into());
                        }
                    }
                    if stage == "first_frame" {
                        let source = target.as_ref().unwrap();
                        if source.app_id == "dev.frames.qa.fixture" {
                            ensure!(
                                (source.bounds.width - if large { 1920. } else { 440. }).abs() < 1.
                                    && (source.bounds.height - if large { 1080. } else { 320. })
                                        .abs()
                                        < 1.,
                                "Mac fixture still includes title-bar chrome: {:?}",
                                source.bounds
                            );
                            ensure!(
                                frame.width == (source.bounds.width * source.scale).round() as u32
                                    && frame.height
                                        == (source.bounds.height * source.scale).round() as u32,
                                "content capture dimensions do not match metadata"
                            );
                            for (x, y) in [(8, 8), (frame.width - 9, 8)] {
                                let index = ((y * frame.width + x) * 4) as usize;
                                ensure!(
                                    frame.bgra[index + 2] > 150 && frame.bgra[index] < 100,
                                    "host title bar leaked into captured content"
                                );
                            }
                            passed.push("standard Mac title bar excluded; content dimensions match scale metadata".into());
                        }
                        let pixel =
                            ((frame.height / 2 * frame.width + frame.width / 2) * 4) as usize;
                        let bgra = &frame.bgra[pixel..pixel + 4];
                        ensure!(
                            bgra[2] > 150 && bgra[0] < 100,
                            "source center was not red under blue cover: {bgra:?}"
                        );
                        measurements.insert(
                            "initial_dimensions".into(),
                            serde_json::json!([frame.width, frame.height]),
                        );
                        passed.push(
                            "live direct-window capture decodes red source beneath blue cover"
                                .into(),
                        );
                        if latency_mode && skip_pause {
                            send(ClientEvent::Action {
                                window: key,
                                action: WindowAction::Focus,
                            })?;
                            stage = "focus_for_latency";
                            continue;
                        }
                        send(ClientEvent::Visibility {
                            window: key,
                            visible: false,
                            focused: false,
                        })?;
                        paused_at = Instant::now();
                        pause_until = paused_at + Duration::from_millis(900);
                        stage = "paused";
                    } else if stage == "paused" {
                        ensure!(
                            paused_at.elapsed() < Duration::from_millis(500),
                            "decoded frames continued beyond 500 ms after unsubscribe"
                        );
                    } else if stage == "resuming" {
                        let millis = resumed_at.elapsed().as_millis() as u64;
                        measurements.insert("fresh_frame_after_resume_ms".into(), millis.into());
                        measurements
                            .insert("resume_500ms_target_met".into(), (millis <= 500).into());
                        passed.push("fresh decoded frame after resubscribe".into());
                        if capture_only {
                            return Ok(());
                        }
                        if latency_mode {
                            send(ClientEvent::Action {
                                window: key,
                                action: WindowAction::Focus,
                            })?;
                            stage = "focus_for_latency";
                            continue;
                        }
                        send(ClientEvent::Action {
                            window: key,
                            action: WindowAction::Menu,
                        })?;
                        stage = "menu";
                    }
                }
                ClientUpdate::Menu { window, items }
                    if stage == "menu" && Some(window) == target.as_ref().map(|w| w.key) =>
                {
                    fn find<'a>(items: &'a [MenuItem], title: &str) -> Option<&'a MenuItem> {
                        items.iter().find_map(|item| {
                            if item.title.replace('&', "") == title {
                                Some(item)
                            } else {
                                find(&item.children, title)
                            }
                        })
                    }
                    let mark = find(&items, "Mark QA").context("fixture menu command missing")?;
                    ensure!(mark.enabled, "fixture menu command disabled");
                    ensure!(
                        !find(&items, "Unavailable QA")
                            .context("disabled fixture item missing")?
                            .enabled,
                        "disabled fixture menu item became enabled"
                    );
                    passed.push("native menu projection preserves disabled command".into());
                    send(ClientEvent::Action {
                        window,
                        action: WindowAction::MenuItem {
                            id: mark.id.clone(),
                        },
                    })?;
                    stage = "marked";
                }
                ClientUpdate::Ack(key, _, error)
                    if stage == "focus_for_latency"
                        && Some(key) == target.as_ref().map(|w| w.key) =>
                {
                    ensure!(error.is_none(), "latency fixture focus failed: {error:?}");
                    stage = "latency_wait";
                    next_click = Instant::now() + Duration::from_millis(150);
                }
                ClientUpdate::Ack(key, actual, error)
                    if stage == "resized" && Some(key) == target.as_ref().map(|w| w.key) =>
                {
                    ensure!(error.is_none(), "resize rejected: {error:?}");
                    let actual =
                        actual.context("resize acknowledgement missing actual geometry")?;
                    measurements.insert(
                        "resize_requested".into(),
                        serde_json::json!([requested_size.0, requested_size.1]),
                    );
                    measurements.insert(
                        "resize_acknowledged".into(),
                        serde_json::json!([actual.bounds.width, actual.bounds.height]),
                    );
                    ensure!(
                        (actual.bounds.width - requested_size.0).abs() < 3.
                            && (actual.bounds.height - requested_size.1).abs() < 3.,
                        "resize acknowledgement differs from requested fixture size"
                    );
                    passed.push("host resize acknowledged actual dimensions".into());
                    target = Some(actual);
                    send(ClientEvent::Action {
                        window: key,
                        action: WindowAction::Focus,
                    })?;
                    stage = "focus_for_click";
                }
                ClientUpdate::Ack(key, _, error)
                    if stage == "focus_for_click"
                        && Some(key) == target.as_ref().map(|w| w.key) =>
                {
                    ensure!(error.is_none(), "fixture focus failed: {error:?}");
                    let actual = target.as_ref().unwrap();
                    ensure!(
                        actual.app_id == "dev.frames.qa.fixture",
                        "click test only targets the generated Mac fixture"
                    );
                    std::thread::sleep(Duration::from_millis(150));
                    let x = 100. / actual.bounds.width;
                    let y = (actual.bounds.height - 96.) / actual.bounds.height;
                    click_started = Instant::now();
                    for down in [true, false] {
                        send(ClientEvent::Input {
                            window: key,
                            event: InputEvent::Button {
                                button: 0,
                                down,
                                x,
                                y,
                            },
                        })?;
                    }
                    stage = "click";
                }
                ClientUpdate::Ack(key, _, error)
                    if stage == "closed" && Some(key) == target.as_ref().map(|w| w.key) =>
                {
                    ensure!(error.is_none(), "host close failed: {error:?}");
                    measurements.insert(
                        "close_ack_ms".into(),
                        (close_started.elapsed().as_millis() as u64).into(),
                    );
                    eprintln!(
                        "probe close accepted elapsed_ms={}",
                        start.elapsed().as_millis()
                    );
                }
                ClientUpdate::Frame(key, frame)
                    if stage == "desktop_frame" && Some(key) == desktop_key =>
                {
                    ensure!(frame.validate(), "invalid desktop frame");
                    passed.push("explicit full-desktop source decodes a live frame".into());
                    send(ClientEvent::ViewMode(ViewMode::Windows))?;
                    stage = "back_to_windows";
                }
                ClientUpdate::ViewMode {
                    error: Some(error), ..
                } => anyhow::bail!(error),
                _ => {}
            }
        }
        anyhow::bail!("fixture test timed out at stage {stage}")
    })();
    let _ = events.send(ClientEvent::ReleaseInput);
    let _ = events.send(ClientEvent::Disconnected);
    let report = serde_json::json!({"passed":passed,"measurements":measurements,"failure":result.as_ref().err().map(ToString::to_string),"pixels_persisted":false});
    std::fs::write(report_path, serde_json::to_vec_pretty(&report)?)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    runtime.block_on(async {
        let _ = tokio::time::timeout(Duration::from_secs(5), job).await;
    });
    result
}
