//! LAN acceptance client using the production session, codec and transport paths.
//! Only controls generated windows whose title begins with "Frames QA source".
//! Never persists captured pixels; reports metadata and measured restore time.
#[allow(dead_code)]
#[path = "../src/session.rs"]
mod session;
#[allow(dead_code)]
#[path = "../src/streaming.rs"]
mod streaming;

use anyhow::{Context, ensure};
use frames_core::{input::InputEvent, model::*};
use frames_native::{ClientEvent, ClientUpdate};
use std::time::{Duration, Instant};

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("frames=info,frames_core=info,frames_native=info")
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
    let runtime = tokio::runtime::Runtime::new()?;
    let (ui, updates) = std::sync::mpsc::sync_channel(32);
    let (events, receiver) = tokio::sync::mpsc::unbounded_channel();
    let job = runtime.spawn(session::client(
        address,
        None,
        ui,
        receiver,
        false,
        false,
        ViewMode::Windows,
    ));
    let mut passed = Vec::<String>::new();
    let mut measurements = serde_json::Map::new();
    let result = (|| -> anyhow::Result<()> {
        let start = Instant::now();
        let mut target: Option<RemoteWindow> = None;
        let mut stage = "discover";
        let mut pause_until = start;
        let mut paused_at = start;
        let mut resumed_at = start;
        let mut requested_size = (0., 0.);
        let mut desktop_key = None;
        let send = |event| {
            events
                .send(event)
                .map_err(|_| anyhow::anyhow!("session event queue closed"))
        };
        while start.elapsed() < Duration::from_secs(110) {
            if job.is_finished() {
                anyhow::bail!("session ended before fixture checkpoints; inspect host/client logs");
            }
            if stage == "paused" && Instant::now() >= pause_until {
                passed.push("decoded frames stop while unsubscribed".into());
                resumed_at = Instant::now();
                send(ClientEvent::Visibility {
                    window: target.as_ref().unwrap().key,
                    visible: true,
                    focused: false,
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
                                focused: false,
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
                                focused: false,
                            })?;
                            stage = "desktop_frame";
                        }
                    } else if stage == "back_to_windows"
                        && windows
                            .iter()
                            .any(|w| Some(w.key) == target.as_ref().map(|w| w.key))
                        && windows.iter().all(|w| w.role != WindowRole::Desktop)
                    {
                        passed.push("desktop -> per-window snapshot reconciled".into());
                        send(ClientEvent::Action {
                            window: target.as_ref().unwrap().key,
                            action: WindowAction::Close,
                        })?;
                        stage = "closed";
                    } else if stage == "closed"
                        && windows
                            .iter()
                            .all(|w| Some(w.key) != target.as_ref().map(|w| w.key))
                    {
                        passed.push("remote close removes fixture identity".into());
                        return Ok(());
                    }
                }
                ClientUpdate::Frame(key, frame) if Some(key) == target.as_ref().map(|w| w.key) => {
                    ensure!(frame.validate(), "invalid decoded frame");
                    if stage == "first_frame" {
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
                ClientUpdate::Ack(key, actual, error)
                    if stage == "resized" && Some(key) == target.as_ref().map(|w| w.key) =>
                {
                    ensure!(error.is_none(), "resize rejected: {error:?}");
                    let actual =
                        actual.context("resize acknowledgement missing actual geometry")?;
                    ensure!(
                        (actual.bounds.width - requested_size.0).abs() < 3.
                            && (actual.bounds.height - requested_size.1).abs() < 3.,
                        "resize acknowledgement differs from requested fixture size"
                    );
                    passed.push("host resize acknowledged actual dimensions".into());
                    send(ClientEvent::Action {
                        window: key,
                        action: WindowAction::Focus,
                    })?;
                    std::thread::sleep(Duration::from_millis(250));
                    send(ClientEvent::Input {
                        window: key,
                        event: InputEvent::Text {
                            text: "Frames Việt ✓".into(),
                        },
                    })?;
                    stage = "text";
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
        anyhow::bail!("fixture test timed out")
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
