//! Production-session acceptance client. Targets the generated shared-input fixture only.
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
use frames_core::{
    input::InputEvent,
    model::*,
    shared_game::{Compatibility, Phase},
};
use frames_native::{ClientEvent, ClientUpdate};
use std::{
    io::Write,
    time::{Duration, Instant},
};

fn stage(name: &str) {
    println!("{}", serde_json::json!({"stage":name}));
    std::io::stdout().flush().unwrap();
}
fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let address = std::env::args()
        .nth(1)
        .context("shared_probe IP:PORT")?
        .parse()?;
    let runtime = tokio::runtime::Runtime::new()?;
    let network_loss = std::env::args().nth(2).as_deref() == Some("--network-loss");
    let require_capture = std::env::var("FRAMES_TEST_INPUT_ONLY").as_deref() != Ok("1");
    if let Some(mode) = std::env::args().nth(2).filter(|_| !network_loss) {
        return runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(12), policy_probe(address, &mode)).await?
        });
    }
    let (ui, updates) = frames_native::client_channel(32);
    let (events, receiver) = tokio::sync::mpsc::unbounded_channel();
    let requested = std::env::var("FRAMES_TEST_CODEC")
        .ok()
        .map(|name| serde_json::from_value(serde_json::Value::String(name)))
        .transpose()?;
    // These preferences must be narrowed by the host's policy before actors start.
    let job = runtime.spawn(session::client(
        address,
        requested,
        ui,
        receiver,
        true,
        session::MediaOptions {
            audio: true,
            bandwidth_mbps: 0,
            quality: Default::default(),
        },
        ViewMode::Desktop,
    ));
    let started = Instant::now();
    let mut last_alive = started;
    let mut policy = None;
    let mut target = None;
    let mut video = false;
    let mut cursor = false;
    let mut held_at = None;
    let mut stalled_at = None;
    let mut released_at = None;
    let mut paused = false;
    let result = (|| -> anyhow::Result<()> {
        while started.elapsed() < Duration::from_secs(40) {
            if (stalled_at.is_none() || network_loss)
                && last_alive.elapsed() >= Duration::from_millis(100)
            {
                events.send(ClientEvent::SharedAlive {
                    created: Instant::now(),
                })?;
                last_alive = Instant::now();
            }
            if held_at.is_some_and(|t: Instant| t.elapsed() > Duration::from_millis(450))
                && stalled_at.is_none()
            {
                stalled_at = Some(Instant::now());
                stage(if network_loss {
                    "network_loss"
                } else {
                    "ui_stalled"
                });
            }
            if paused && stalled_at.is_some() && released_at.is_none() {
                events.send(ClientEvent::ReleaseInput)?;
                let stale = Instant::now() - Duration::from_secs(2);
                events.send(ClientEvent::ResumeInput { created: stale })?;
                events.send(ClientEvent::TimedInput {
                    window: target.unwrap(),
                    event: InputEvent::Key {
                        usage: 0x1a,
                        down: true,
                    },
                    created: stale,
                })?;
                released_at = Some(Instant::now());
                stage("released");
            }
            if released_at.is_some_and(|t: Instant| t.elapsed() > Duration::from_millis(300)) {
                ensure!(
                    (video || !require_capture) && cursor && policy.is_some(),
                    "missing shared capture or policy"
                );
                stage("passed");
                return Ok(());
            }
            ensure!(
                !job.is_finished(),
                "production client ended before shared checkpoints"
            );
            let update = match updates.recv_timeout(Duration::from_millis(20)) {
                Ok(v) => v,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(e) => return Err(e.into()),
            };
            match update {
                ClientUpdate::SharedSession(info) => {
                    ensure!(
                        released_at.is_none() || info.phase != Phase::Active,
                        "stale resume reactivated control"
                    );
                    eprintln!("shared state: {:?} {}", info.phase, info.detail);
                    ensure!(
                        info.compatibility.status == Compatibility::Untested,
                        "generated fixture must never be verified as a game"
                    );
                    ensure!(info.generation != 0, "missing generation");
                    if info.phase == Phase::Paused && stalled_at.is_some() {
                        paused = true;
                    }
                    policy = Some(info);
                }
                ClientUpdate::Windows(windows) => {
                    eprintln!("shared windows: {}", windows.len());
                    ensure!(
                        windows.len() == 1 && windows[0].title == "Frames shared input fixture",
                        "shared snapshot exposed another window"
                    );
                    let window = windows[0].key;
                    ensure!(
                        policy.as_ref().is_some_and(|p| p.window == window),
                        "policy target differs from capture"
                    );
                    if target.is_none() {
                        events.send(ClientEvent::Visibility {
                            window,
                            visible: true,
                            focused: true,
                        })?;
                        target = Some(window);
                    }
                }
                ClientUpdate::Frame(window, frame) => {
                    if !video {
                        eprintln!("shared first frame: {}x{}", frame.width, frame.height);
                        stage("capture_received");
                    }
                    ensure!(
                        Some(window) == target && frame.width > 0 && frame.height > 0,
                        "unexpected capture target"
                    );
                    video = true;
                }
                ClientUpdate::SharedCursor(value) => {
                    if !cursor {
                        eprintln!("shared first cursor");
                    }
                    ensure!(
                        value.x.is_finite() && value.y.is_finite(),
                        "invalid virtual cursor"
                    );
                    cursor = true;
                }
                ClientUpdate::Ack(_, _, Some(error)) => {
                    anyhow::bail!("shared input failed: {error}")
                }
                ClientUpdate::Status(_, message) => eprintln!("shared status: {message}"),
                _ => {}
            }
            if (video || !require_capture)
                && cursor
                && held_at.is_none()
                && let Some(window) = target
            {
                events.send(ClientEvent::ResumeInput {
                    created: std::time::Instant::now(),
                })?;
                events.send(ClientEvent::Input {
                    window,
                    event: InputEvent::Key {
                        usage: 0x1a,
                        down: true,
                    },
                })?;
                events.send(ClientEvent::Input {
                    window,
                    event: InputEvent::RelativeMotion { dx: 20, dy: -10 },
                })?;
                events.send(ClientEvent::Input {
                    window,
                    event: InputEvent::RelativeButton {
                        button: 0,
                        down: true,
                    },
                })?;
                // The production client must suppress these before they reach the host.
                events.send(ClientEvent::ViewMode(ViewMode::Desktop))?;
                events.send(ClientEvent::Action {
                    window,
                    action: WindowAction::Close,
                })?;
                held_at = Some(Instant::now());
                stage("key_down");
            }
        }
        anyhow::bail!("shared session acceptance timed out")
    })();
    let _ = events.send(ClientEvent::Disconnected);
    runtime.block_on(async { tokio::time::timeout(Duration::from_secs(8), job).await?? })?;
    result
}

/// Bypass the viewer's guards to check that the host enforces its own boundary.
async fn policy_probe(address: std::net::SocketAddr, mode: &str) -> anyhow::Result<()> {
    use frames_core::{
        protocol::{self, Hello, Message, SessionPurpose},
        transport,
        trust::Identity,
    };
    let identity = Identity::load()?;
    let endpoint = transport::client(&identity)?;
    let connection = endpoint.connect(address, "frames.local")?.await?;
    let (mut send, mut receive) = connection.open_bi().await?;
    protocol::send_json(
        &mut send,
        &Message::Hello(Hello {
            version: frames_core::PROTOCOL_VERSION,
            device_id: identity.device_id,
            name: "Shared policy fixture".into(),
            platform: "windows".into(),
            codecs: vec![],
            shared_game: if mode == "--incompatible" {
                None
            } else {
                Some(frames_core::shared_game::Capabilities::V1)
            },
            purpose: SessionPurpose::Control,
        }),
    )
    .await?;
    ensure!(
        matches!(protocol::recv_json(&mut receive).await?, Message::Hello(_)),
        "missing host hello"
    );
    protocol::send_json(&mut send, &Message::PairApproved).await?;
    ensure!(
        matches!(
            protocol::recv_json(&mut receive).await?,
            Message::PairApproved
        ),
        "missing approval"
    );
    if mode == "--incompatible" {
        let reason = connection.closed().await.to_string();
        ensure!(
            reason.contains("shared-game"),
            "unexpected rejection: {reason}"
        );
        stage("incompatible_rejected");
        return Ok(());
    }
    ensure!(mode == "--policy", "unknown fixture mode");
    let session = loop {
        if let Message::SessionPolicy {
            shared: Some(value),
        } = protocol::recv_json(&mut receive).await?
        {
            break value;
        }
    };
    for message in [
        Message::SetViewMode {
            mode: ViewMode::Desktop,
        },
        Message::Command {
            request: 1,
            window: session.window,
            action: WindowAction::Close,
        },
        Message::DownloadFile {
            path: "Downloads/private".into(),
        },
        Message::AdbBridge { enabled: true },
        Message::Input {
            window: session.window,
            event: InputEvent::Key {
                usage: 0x1a,
                down: true,
            },
            elapsed_ms: 0,
        },
    ] {
        protocol::send_json(&mut send, &message).await?;
    }
    let mut input = connection.open_uni().await?;
    input.write_all(b"I").await?;
    protocol::send_json(
        &mut input,
        &Message::SharedInput {
            window: session.window,
            generation: session.generation.wrapping_add(1),
            event: InputEvent::Key {
                usage: 0x1a,
                down: true,
            },
            elapsed_ms: 0,
        },
    )
    .await?;
    let mut rejected = 0;
    while rejected < 6 {
        if let Message::Error { message } = protocol::recv_json(&mut receive).await? {
            ensure!(
                message == "This operation is unavailable when sharing one window",
                "unexpected host error: {message}"
            );
            rejected += 1;
        }
    }
    connection.close(0u32.into(), b"policy fixture complete");
    stage("host_policy_passed");
    Ok(())
}
