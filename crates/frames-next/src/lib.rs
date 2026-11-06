//! `frames-next`: the v2 stack end to end (docs/V2_PLAN.md, Phase 3). Also
//! available as `frames next ...` inside the Frames app.
//!
//! ```text
//! frames-next host [--source monitor|qa] [--window TITLE]... [--codec hevc|h264|av1] [--encoder nvenc|amf|qsv|mf|software]
//!                  [--fps 60] [--mbps 30]
//!                  [--hold N] [--seconds N] [--relay-only]
//! frames-next discover | peers | revoke | windows <device-id> [--config-dir DIR]\n       frames-next connect <endpoint-id> [--vsync] [--synthetic-input] [--seconds N] [--relay-only]
//! ```
//!
//! The host prints its endpoint id; the client dials it by id (iroh finds a
//! direct path or relays). Once a second the client prints delivery,
//! capture-to-present latency, input-to-wire latency, recovery, and the path.
//! `--source qa` captures a generated test window instead of the screen, and
//! input then goes to that window by message instead of `SendInput`.
//! `--synthetic-input` makes the viewer generate its own mouse and key
//! messages, for unattended runs.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::time::Duration;

mod admit;
mod audio;
mod catalog;
mod clip;
pub mod control;
pub mod directory;
mod doctor;
mod impair;
#[cfg(windows)]
mod isolated;
mod lane;
pub mod launcher;
#[cfg(target_os = "macos")]
mod mac_client;
#[cfg(target_os = "macos")]
mod mac_host;
mod menus;
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
mod overlay;
#[cfg(windows)]
mod overlay_raster;
mod pacing;
mod pads;
pub mod password;
mod preview;
mod quick_apps;
mod resize;
mod services;
mod stats;
mod streams;
mod transfer;
mod usb;
#[cfg(windows)]
mod win;
#[cfg(windows)]
mod win_files;
#[cfg(windows)]
mod win_overlay;
#[cfg(windows)]
mod windows_client;
#[cfg(windows)]
mod windows_host;

/// Sent by the host on the first channel before any media.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StreamInfo {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// The host's OS ("windows", "macos"); viewers swap Control and Command
    /// when it differs from theirs.
    #[serde(default)]
    pub platform: String,
    /// The host's device name.
    #[serde(default)]
    pub name: String,
    /// One window with isolated input ("One window", games): v1's "Window
    /// compatibility" text for its executable. Viewers send physical keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<String>,
}

/// This machine's OS, as [`StreamInfo::platform`] names it.
pub fn platform() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "other"
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

impl Codec {
    /// How the debug overlay names it.
    pub fn label(self) -> &'static str {
        match self {
            Self::H264 => "H.264",
            Self::Hevc => "HEVC",
            Self::Av1 => "AV1",
        }
    }
}

impl From<Codec> for frames_media::api::Codec {
    fn from(codec: Codec) -> Self {
        match codec {
            Codec::H264 => Self::H264,
            Codec::Hevc => Self::Hevc,
            Codec::Av1 => Self::Av1,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Options {
    pub qa_source: bool,
    pub codec: Codec,
    pub fps: u32,
    pub mbps: u32,
    /// GPU clock hold by dummy load (clears per ms); 0 = off.
    pub hold: u32,
    /// NVIDIA driver profile "prefer maximum performance" for this program.
    pub gpu_profile: bool,
    pub vsync: bool,
    pub seconds: u64,
    pub relay_only: bool,
    /// Self-hosted relays to use instead of n0's (repeatable).
    pub relays: Vec<String>,
    /// No relays and no internet discovery: only addresses given or found
    /// on the LAN (impairment tests, isolated networks).
    pub direct_only: bool,
    /// Viewer: dial this address for the device id (e.g. an impair proxy).
    pub addr: Option<std::net::SocketAddr>,
    /// Bind this address only (e.g. 127.0.0.1:0 behind an impair proxy, so
    /// no direct path bypasses it).
    pub bind: Option<std::net::SocketAddr>,
    /// Viewer: offer this device's v1 identity so a host that trusted it in
    /// v1 trusts it now.
    pub migrate_v1: bool,
    /// Host: pacing rate as a multiple of the target (experiments).
    pub pacing: Option<f64>,
    /// Clipboard sharing (both ends must turn it on).
    pub clipboard: clip::Mode,
    /// Host: share these windows (each the first whose title contains the
    /// text) instead of the screen, one viewer session per window.
    pub windows: Vec<String>,
    /// Host, set at admission: the window this session shares, by title.
    pub window: Option<String>,
    /// Host, set at admission: the window this session shares, by catalog
    /// id; viewer: the window to show (`--apps` sets it per session).
    pub window_id: Option<u64>,
    /// Viewer: open each of the host's application windows in its own
    /// window (`crate::catalog`), instead of one view of its screen.
    pub apps: bool,
    /// Viewer: the host's Quick App to start and show (`--app ID`), and
    /// whether only its windows are shared (`--only-app`).
    pub quick_app: Option<String>,
    pub only_app: bool,
    /// Host: the most application windows one viewer is offered.
    pub max_windows: usize,
    /// Viewer: how many of the host's shared windows to open.
    pub open_windows: u32,
    /// ADB bridge: the viewer forwards its ADB server to the host.
    pub adb: bool,
    /// Viewer: the ADB server to forward (default 127.0.0.1:5037).
    pub adb_server: std::net::SocketAddr,
    /// Host: where the bridge listens (default 127.0.0.1:15037).
    pub adb_listen: std::net::SocketAddr,
    /// Name shown to nearby devices (default: the computer's name).
    pub name: Option<String>,
    /// Host audio (both ends must turn it on).
    pub audio: audio::Mode,
    /// Viewer: files to send to the host once connected.
    pub send_files: Vec<std::path::PathBuf>,
    /// Viewer: files to fetch from the host's home folder.
    pub get_files: Vec<String>,
    /// Host: where received files go (default ~/Downloads/Frames).
    pub receive_dir: Option<std::path::PathBuf>,
    /// Viewer: post generated mouse and key messages to its own window.
    pub synthetic_input: bool,
    /// Viewer: start in relative mouse mode without locking the pointer
    /// (unattended tests; users press Control+Option+F11).
    pub relative_qa: bool,
    /// Viewer: connection-menu commands to run as if chosen, each
    /// `<seconds>=<Command JSON>`; `{window}` stands for the first listed
    /// window's id (unattended tests of the menus).
    pub qa_commands: Vec<(f64, String)>,
    /// Host: `nvenc` (direct), or an FFmpeg backend: `amf`, `qsv`, `mf`,
    /// `software`.
    pub encoder: String,
    /// `--encoder` was given: use that one or fail, no fallback.
    pub encoder_explicit: bool,
    /// Host: try this encoder first, then the usual order (Settings).
    pub prefer_encoder: Option<String>,
    /// Where the device key and trust list live (default: v1's directory).
    pub config_dir: Option<std::path::PathBuf>,
    /// Host: approve unknown devices without asking (unattended tests).
    pub approve: Option<admit::Approve>,
    /// Host: ask the launcher that started it about new devices
    /// (`crate::launcher`).
    pub ask_launcher: bool,
    /// Host: keep sharing after the last viewer leaves (the launcher stops
    /// it); otherwise the host ends with its last session.
    pub stay: bool,
    /// Host, set at admission: this viewer may send input.
    pub control: bool,
    /// `--codec` was given; otherwise the host picks (see [`choose_codec`]).
    pub codec_explicit: bool,
    /// Host, set at admission: what the viewer decodes, best first.
    pub viewer_decoders: Vec<Codec>,
    /// Viewer: the most it wants to receive (its quality setting), sent to
    /// the host; host, set at admission: that cap for this session.
    pub max_mbps_cap: Option<u32>,
    /// Viewer: the window title (default: the host's name).
    pub title: Option<String>,
    /// Viewer: the shared window's application (its options are saved per
    /// application, `crate::control`).
    pub app: Option<String>,
    /// Viewer: this window's id in the connection menus (`crate::control`):
    /// the host's window id, or 0 for the desktop.
    pub menu_id: u64,
    /// Viewer: this window is a host menu, tooltip or sheet, shown over its
    /// parent window's picture: (it, its parent) from the catalog.
    pub popup: Option<(catalog::Window, catalog::Window)>,
    /// Viewer: the catalog entry of the host window this session shows
    /// (its role, owner, place and icon).
    pub window_info: Option<catalog::Window>,
    /// Viewer: the window an owned popup or dialog belongs to (its owner,
    /// or for an ownerless menu its application's front window).
    pub parent_info: Option<catalog::Window>,
    /// Viewer: start with the host's cursor drawn into the picture
    /// ("Show remote cursor", remembered per device by the launcher).
    pub remote_cursor: bool,
    /// Viewer: show live statistics in the window title (diagnostics).
    pub stats_title: bool,
    /// Viewer: send keys as the same physical keys on the host (Control as
    /// Control) instead of swapping Control and Command across platforms.
    pub physical_keys: bool,
    /// Viewer: forward this device's gamepads to the host (`crate::pads`).
    pub gamepads: bool,
    /// Viewer: USB devices (bus ids) to pass through to a Windows host
    /// (`crate::usb`; Windows viewers).
    pub usb: Vec<String>,
    /// Windows host sharing one window: the viewer's input goes into that
    /// window's process only (`crate::isolated`), not the host's mouse and
    /// keyboard.
    pub isolated: bool,
    /// Viewer: the host's access password, from `FRAMES_ACCESS_PASSWORD`
    /// (an environment variable, so it never shows in process lists).
    pub password: Option<String>,
}

impl Options {
    fn parse(args: &[String]) -> Result<Self> {
        let mut o = Self {
            qa_source: false,
            codec: Codec::Hevc,
            fps: 60,
            mbps: 30,
            hold: 0,
            gpu_profile: true,
            vsync: false,
            seconds: 0,
            relay_only: false,
            relays: Vec::new(),
            direct_only: false,
            addr: None,
            bind: None,
            migrate_v1: false,
            pacing: None,
            clipboard: clip::Mode::Off,
            windows: Vec::new(),
            window: None,
            window_id: None,
            apps: false,
            quick_app: None,
            only_app: false,
            max_windows: catalog::MAX_WINDOWS,
            open_windows: 1,
            adb: false,
            adb_server: ([127, 0, 0, 1], 5037).into(),
            adb_listen: ([127, 0, 0, 1], 15037).into(),
            name: None,
            audio: audio::Mode::Off,
            send_files: Vec::new(),
            get_files: Vec::new(),
            receive_dir: None,
            synthetic_input: false,
            relative_qa: false,
            qa_commands: Vec::new(),
            encoder: "nvenc".into(),
            encoder_explicit: false,
            prefer_encoder: None,
            config_dir: None,
            approve: None,
            ask_launcher: false,
            stay: false,
            control: false,
            codec_explicit: false,
            viewer_decoders: Vec::new(),
            max_mbps_cap: None,
            title: None,
            app: None,
            menu_id: 0,
            popup: None,
            window_info: None,
            parent_info: None,
            remote_cursor: false,
            stats_title: false,
            physical_keys: false,
            gamepads: false,
            usb: Vec::new(),
            isolated: false,
            password: std::env::var("FRAMES_ACCESS_PASSWORD")
                .ok()
                .filter(|p| !p.is_empty()),
        };
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let mut value = || {
                args.next()
                    .with_context(|| format!("{arg} needs a value"))
                    .map(String::as_str)
            };
            match arg.as_str() {
                "--source" => o.qa_source = value()? == "qa",
                "--codec" => {
                    o.codec_explicit = true;
                    o.codec = match value()? {
                        "h264" => Codec::H264,
                        "hevc" => Codec::Hevc,
                        "av1" => Codec::Av1,
                        other => bail!("unknown codec {other}"),
                    }
                }
                "--fps" => o.fps = value()?.parse()?,
                "--mbps" => o.mbps = value()?.parse()?,
                "--hold" => o.hold = value()?.parse()?,
                "--no-gpu-profile" => o.gpu_profile = false,
                "--seconds" => o.seconds = value()?.parse()?,
                "--vsync" => o.vsync = true,
                "--relay-only" => o.relay_only = true,
                "--relay" => o.relays.push(value()?.to_string()),
                "--direct-only" => o.direct_only = true,
                "--addr" => o.addr = Some(value()?.parse()?),
                "--bind" => o.bind = Some(value()?.parse()?),
                "--migrate-v1" => o.migrate_v1 = true,
                "--pacing" => o.pacing = Some(value()?.parse()?),
                "--audio" => o.audio = audio::parse_mode(value()?)?,
                "--name" => o.name = Some(value()?.to_string()),
                "--adb" => o.adb = true,
                "--window" => o.windows.push(value()?.to_string()),
                "--windows" => o.open_windows = value()?.parse::<u32>()?.clamp(1, 16),
                "--adb-server" => o.adb_server = value()?.parse()?,
                "--adb-listen" => {
                    let address: std::net::SocketAddr = value()?.parse()?;
                    anyhow::ensure!(
                        address.ip().is_loopback(),
                        "the ADB bridge listens on loopback only"
                    );
                    o.adb_listen = address;
                }
                "--send" => o.send_files.push(value()?.into()),
                "--get" => o.get_files.push(value()?.to_string()),
                "--receive-dir" => o.receive_dir = Some(value()?.into()),
                "--clipboard" => {
                    o.clipboard = match value()? {
                        "on" => clip::Mode::On,
                        "off" => clip::Mode::Off,
                        "qa" => clip::Mode::Qa,
                        other => bail!("--clipboard takes on, off or qa, not {other}"),
                    }
                }
                "--synthetic-input" => o.synthetic_input = true,
                "--relative-qa" => o.relative_qa = true,
                "--qa-command" => {
                    let value = value()?;
                    let (at, json) = value
                        .split_once('=')
                        .context("--qa-command takes <seconds>=<command JSON>")?;
                    o.qa_commands.push((at.parse()?, json.to_string()));
                }
                "--encoder" => {
                    o.encoder = value()?.to_string();
                    o.encoder_explicit = true;
                }
                "--config-dir" => o.config_dir = Some(value()?.into()),
                "--ask-launcher" => o.ask_launcher = true,
                "--title" => o.title = Some(value()?.to_string()),
                "--apps" => o.apps = true,
                // A Quick App opens in per-window view.
                "--app" => {
                    o.quick_app = Some(value()?.to_string());
                    o.apps = true;
                }
                "--only-app" => o.only_app = true,
                "--max-windows" => o.max_windows = value()?.parse::<usize>()?.clamp(1, 32),
                "--stats-title" => o.stats_title = true,
                "--physical-keys" => o.physical_keys = true,
                "--gamepads" => o.gamepads = true,
                "--usb" => {
                    let bus = value()?;
                    anyhow::ensure!(
                        frames_devices::protocol::valid_bus_id(bus),
                        "--usb takes a USB bus id (such as 1-4), not {bus}"
                    );
                    o.usb.push(bus.to_string());
                }
                "--isolated" => o.isolated = true,
                "--prefer-encoder" => {
                    let name = value()?;
                    anyhow::ensure!(
                        ["nvenc", "amf", "qsv", "mf", "software"].contains(&name),
                        "--prefer-encoder takes nvenc, amf, qsv, mf or software, not {name}"
                    );
                    o.prefer_encoder = Some(name.to_string());
                }
                "--max-mbps" => o.max_mbps_cap = Some(value()?.parse::<u32>()?.max(1)),
                "--remote-cursor" => o.remote_cursor = true,
                "--stay" => o.stay = true,
                "--approve" => {
                    let text = value()?;
                    o.approve = Some(admit::Approve::parse(text).with_context(|| {
                        format!(
                            "--approve takes view, control, view-once or control-once, not {text}"
                        )
                    })?)
                }
                other => bail!("unknown option {other}"),
            }
        }
        Ok(o)
    }

    /// Host: the rate control ceiling, twice `--mbps` (at least 20), under
    /// the viewer's cap.
    pub fn max_mbps(&self) -> u32 {
        let ceiling = (self.mbps * 2).max(20);
        self.max_mbps_cap.map_or(ceiling, |cap| ceiling.min(cap))
    }

    /// Host: the first bitrate, `--mbps` under the viewer's cap.
    pub fn start_mbps(&self) -> u32 {
        self.mbps.min(self.max_mbps())
    }

    /// How long to run; 0 runs until the peer leaves.
    pub fn deadline(&self) -> Option<Duration> {
        (self.seconds > 0).then(|| Duration::from_secs(self.seconds))
    }
}

fn node_config(o: &Options) -> Result<frames_net::NodeConfig> {
    let relays = if o.relays.is_empty() {
        frames_net::Relays::N0
    } else {
        let urls = o
            .relays
            .iter()
            .map(|url| {
                url.parse()
                    .with_context(|| format!("invalid relay URL {url}"))
            })
            .collect::<Result<Vec<_>>>()?;
        frames_net::Relays::Custom(urls)
    };
    Ok(frames_net::NodeConfig {
        relay_only: o.relay_only,
        relays: if o.direct_only {
            frames_net::Relays::Disabled
        } else {
            relays
        },
        discovery: !o.direct_only,
        bind: o.bind,
        ..frames_net::NodeConfig::default()
    })
}

async fn host(o: Options) -> Result<()> {
    let config = admit::config_dir(o.config_dir.as_ref())?;
    let key = admit::device_key(&config)?;
    let node = frames_net::Node::bind(&key, node_config(&o)?).await?;
    // Without internet, relays are unreachable; LAN viewers still find this
    // host through the local advertisement.
    if let Err(error) = node.online(Duration::from_secs(10)).await {
        println!("no relay reachable ({error:#}); reachable on this network only");
    }
    println!("endpoint id: {}", node.id());
    for socket in node.bound_sockets() {
        println!("listening on {socket}");
    }
    let _advertised = frames_net::local::advertise(&node, &device_name(&o))
        .map_err(|error| println!("not advertised on the local network: {error:#}"))
        .ok();
    // Once per process: every session's encoder benefits, and nested
    // per-session guards would restore each other's settings.
    #[cfg(windows)]
    let _gpu_profile = (o.encoder == "nvenc" && o.gpu_profile).then(|| {
        match frames_platform_windows::nvprofile::PreferMaxPerformance::apply() {
            Ok(guard) => {
                println!("GPU power: prefer maximum performance (restored on exit)");
                Some(guard)
            }
            Err(error) => {
                println!("GPU power profile unavailable: {error}");
                None
            }
        }
    });
    // Each viewer session (one per shared window) runs on its own; the host
    // ends when the last one has.
    let mut sessions = tokio::task::JoinSet::new();
    // One session at a time carries host audio; it is free again when that
    // session ends.
    let audio_free = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let admission = std::sync::Arc::new(tokio::sync::Mutex::new(admit::HostState::default()));
    // Connections being accepted: each handshake and admission runs on its
    // own, so a peer whose handshake stalls, or a new device waiting for
    // its approval, holds up nobody else (other viewers, or another window
    // of the same viewer).
    let mut arriving = tokio::task::JoinSet::new();
    loop {
        let (session, admitted) = tokio::select! {
            () = launcher::gone() => {
                println!("stopped sharing: the launcher is gone");
                return Ok(());
            }
            incoming = node.incoming() => {
                let incoming = incoming.context("node closed")?;
                let (config, admission) = (config.clone(), admission.clone());
                let (approve, ask_launcher) = (o.approve, o.ask_launcher);
                arriving.spawn(async move {
                    let session = match incoming.session().await {
                        Ok(session) => session,
                        Err(error) => {
                            println!("{error:#}");
                            return None;
                        }
                    };
                    // One device's admission failing (say, the trust list
                    // cannot be written) refuses that device, not every
                    // later one.
                    match admit::decide(&config, &session, approve, ask_launcher, &admission).await {
                        Ok(Ok(admitted)) => Some((session, admitted)),
                        Ok(Err(reason)) => {
                            session.close(reason);
                            None
                        }
                        Err(error) => {
                            println!("could not admit {}: {error:#}", session.remote());
                            session.close(admit::NOT_APPROVED);
                            None
                        }
                    }
                });
                continue;
            }
            Some(arrived) = arriving.join_next(), if !arriving.is_empty() => {
                match arrived {
                    Ok(Some(admitted)) => admitted,
                    _ => continue,
                }
            }
            Some(done) = sessions.join_next(), if !sessions.is_empty() => {
                if let Ok(Err(error)) = done {
                    println!("session ended: {error:#}");
                }
                if sessions.is_empty() && arriving.is_empty() && !o.stay {
                    return Ok(());
                }
                continue;
            }
        };
        if admitted.trusted {
            tokio::spawn(admit::watch_trust(config.clone(), session.clone()));
        }
        let mut session_options = o.clone();
        session_options.control = admitted.control;
        session_options.viewer_decoders = admitted.decoders;
        session_options.max_mbps_cap = admitted.max_mbps;
        if admitted.preview {
            sessions.spawn(preview::serve(session));
            continue;
        }
        if admitted.quick_apps {
            sessions.spawn(quick_apps::serve_list(
                session,
                config.clone(),
                o.windows.clone(),
                o.max_windows,
            ));
            continue;
        }
        // A Quick App: only one this host lists.
        let quick_app = match &admitted.app {
            Some(id) => {
                match quick_apps::load(&config).map(|apps| quick_apps::find(&apps, id).cloned()) {
                    Ok(Some(app)) => Some(app),
                    Ok(None) => {
                        session.close(quick_apps::NO_SUCH_APP);
                        continue;
                    }
                    Err(error) => {
                        println!("Quick Apps unreadable: {error:#}");
                        session.close(quick_apps::NO_SUCH_APP);
                        continue;
                    }
                }
            }
            None => None,
        };
        let only_app = quick_app.as_ref().filter(|_| admitted.only_app).cloned();
        if admitted.catalog {
            let (filters, limit) = (o.windows.clone(), o.max_windows);
            sessions.spawn(async move {
                let scope = match quick_app {
                    Some(app) => Some(catalog::Scope {
                        launched: quick_apps::start_if_needed(
                            &app,
                            admitted.control,
                            filters.clone(),
                            limit,
                        )
                        .await,
                        only: admitted.only_app,
                        app,
                    }),
                    None => None,
                };
                catalog::serve(session, filters, limit, scope).await
            });
            continue;
        }
        // Checked on the session's own task (below), never in this loop.
        let mut shared_check = None;
        if let Some(id) = admitted.window_id {
            shared_check = Some((id, only_app.clone()));
            // The secure desktop streams the screen (`catalog::SECURE_DESKTOP`).
            session_options.window_id = (id != catalog::SECURE_DESKTOP).then_some(id);
            session_options.window = None;
        } else if only_app.is_some() {
            // "Only stream this app": never the desktop.
            session.close(quick_apps::ONLY_THE_APP);
            continue;
        } else {
            let index = admitted.window.unwrap_or(0) as usize;
            session_options.window = match (o.windows.get(index), o.windows.is_empty()) {
                (Some(title), _) => Some(title.clone()),
                (None, true) => None,
                (None, false) => {
                    session.close("no such window");
                    continue;
                }
            };
        }
        let wants_audio = admitted.audio && o.audio != audio::Mode::Off;
        let audio_free = audio_free.clone();
        let audio_source = audio_source(&o, &config, only_app.as_ref());
        let (filters, limit, host_audio) = (o.windows.clone(), o.max_windows, o.audio);
        sessions.spawn(async move {
            if let Some((id, scope)) = shared_check {
                // Only a window this host shares, by its current catalog
                // (and within the Quick App with "Only stream this app");
                // its other windows' sessions never start it again.
                let only = scope.is_some();
                let shared = tokio::task::spawn_blocking(move || {
                    catalog::is_shared(&filters, limit, id, scope.as_ref())
                })
                .await?;
                if !shared {
                    session.close(if only {
                        quick_apps::NOT_IN_APP
                    } else {
                        "no such window"
                    });
                    return Ok(());
                }
            }
            let audio_mode =
                if wants_audio && audio_free.swap(false, std::sync::atomic::Ordering::AcqRel) {
                    host_audio
                } else {
                    audio::Mode::Off
                };
            let audio = tokio::spawn({
                let session = session.clone();
                async move {
                    let source = match audio_source {
                        Ok(source) => source,
                        Err(reason) => {
                            println!("audio: {reason}");
                            return;
                        }
                    };
                    if let Err(error) = audio::host(session, audio_mode, source).await {
                        println!("audio stopped: {error:#}");
                    }
                }
            });
            let result = run_host(session, session_options).await;
            audio.abort();
            if audio_mode != audio::Mode::Off {
                audio_free.store(true, std::sync::atomic::Ordering::Release);
            }
            result
        });
    }
}

#[cfg(windows)]
async fn run_host(session: frames_net::Session, o: Options) -> Result<()> {
    windows_host::run(session, o).await
}

#[cfg(target_os = "macos")]
async fn run_host(session: frames_net::Session, o: Options) -> Result<()> {
    mac_host::run(session, o).await
}

#[cfg(not(any(windows, target_os = "macos")))]
async fn run_host(_: frames_net::Session, _: Options) -> Result<()> {
    bail!("hosting needs the Windows or macOS backends")
}

async fn connect(id: &str, o: Options) -> Result<()> {
    println!("Frames status: connecting");
    let config = admit::config_dir(o.config_dir.as_ref())?;
    let node = frames_net::Node::bind(&admit::device_key(&config)?, node_config(&o)?).await?;
    let started = std::time::Instant::now();
    // A host on this network answers its local advertisement: dial its
    // addresses directly (works without internet). Otherwise iroh's
    // discovery finds it.
    // A full id stops browsing as soon as that host answers; a prefix
    // waits the whole window to be sure it is unambiguous.
    let nearby = frames_net::local::find(id, Duration::from_millis(1500))
        .await
        .unwrap_or_default();
    let target: iroh::EndpointAddr = match (&nearby, o.addr) {
        (_, Some(addr)) => iroh::EndpointAddr::from_parts(
            id.parse::<iroh::EndpointId>()
                .context("--addr needs the full device id")?,
            [iroh::TransportAddr::Ip(addr)],
        ),
        (Some(host), None) => {
            println!("found {} on this network", host.name);
            host.addr.clone()
        }
        (None, None) => id
            .parse::<iroh::EndpointId>()
            .context("not a device id, and no single nearby device starts with it")?
            .into(),
    };
    let peer = nearby
        .as_ref()
        .map(|h| h.name.clone())
        .unwrap_or_else(|| id.chars().take(8).collect());
    let quality = match o.max_mbps_cap {
        Some(cap) if cap < u32::from(quality_mbps(1)) => 2,
        Some(cap) if cap < u32::from(quality_mbps(0)) => 1,
        _ => 0,
    };
    let mut view = if o.apps {
        control::View::Windows
    } else {
        control::View::Desktop
    };
    control::start(&target.id.to_string(), &peer, view, quality, o.adb, &config);
    control::update(|s| {
        s.remote_cursor = o.remote_cursor;
        s.quick_app = o.quick_app.clone();
        s.only_app = o.only_app && o.quick_app.is_some();
    });
    for (at, json) in o.qa_commands.clone() {
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs_f64(at)).await;
            let window = control::snapshot()
                .windows
                .iter()
                .find(|w| !w.popup)
                .map_or(0, |w| w.id);
            let json = json.replace("{window}", &window.to_string());
            println!("QA menu command: {json}");
            control::send_json(&json);
        });
    }
    // "Per-window view" / "Full desktop view" in the connection menu end
    // the current view and open the other one on the same host.
    let mut first = true;
    loop {
        let result = match view {
            control::View::Windows => {
                catalog::view(&node, target.clone(), &o, started, first).await
            }
            control::View::Desktop => desktop(&node, target.clone(), &o, started, first).await,
        };
        first = false;
        if control::switching() && !DISCONNECT.load(std::sync::atomic::Ordering::Relaxed) {
            control::switched();
            view = control::snapshot().view;
            println!("switching to the {view:?} view");
            launcher::report("view", &serde_json::json!({ "view": view }));
            continue;
        }
        return result;
    }
}

/// Viewer: the host's screen in one window (or `--windows N` shared
/// windows, one window each).
async fn desktop(
    node: &frames_net::Node,
    target: iroh::EndpointAddr,
    o: &Options,
    started: std::time::Instant,
    first: bool,
) -> Result<()> {
    let mut viewers = tokio::task::JoinSet::new();
    for index in 0..o.open_windows {
        let session = node.connect(target.clone()).await?;
        let window = (o.open_windows > 1).then_some(index);
        admit::send_hello(
            &session,
            &node.id(),
            o.migrate_v1,
            admit::Asks {
                decoders: viewer_decoders(),
                window,
                name: device_name(o),
                audio: index == 0 && o.audio != audio::Mode::Off,
                max_mbps: o.max_mbps_cap,
                catalog: false,
                window_id: o.window_id,
                password: o.password.clone(),
                preview: false,
                app: o.quick_app.clone(),
                only_app: o.only_app,
                quick_apps: false,
            },
        )
        .await?;
        if index == 0 {
            if first {
                start_transfers(&session, o);
            }
            start_devices(&session, o);
        }
        let code = session.pairing_code()?;
        println!(
            "connected in {:.0} ms, pairing code {code} (a new host shows the same code)",
            started.elapsed().as_secs_f64() * 1e3,
        );
        if index == 0 {
            launcher::report("code", &serde_json::json!({ "code": code }));
        }
        let id = u64::from(index);
        control::set_windows(vec![control::WindowEntry {
            id,
            title: "Remote desktop — primary display".into(),
            app: "Remote desktop".into(),
            shown: true,
            desktop: o.window_id.is_none() && window.is_none(),
            popup: false,
            parent: None,
            modal: false,
            resizable: false,
            has_menus: false,
            prefs: Default::default(),
        }]);
        control::update(|s| s.connected = true);
        let mut session_options = o.clone();
        session_options.app = Some("Remote desktop".into());
        session_options.menu_id = id;
        viewers.spawn(async move {
            let result = run_client(session.clone(), session_options).await;
            match (&result, session.close_reason()) {
                (Err(_), Some(reason)) if reason == admit::NOT_APPROVED => {
                    anyhow::bail!("the host refused this device (not approved there)")
                }
                (Err(_), Some(reason)) if reason == admit::WRONG_PASSWORD => {
                    anyhow::bail!("the host refused this device: wrong access password")
                }
                (Err(_), Some(reason)) if reason == "no such window" => {
                    anyhow::bail!("the host shares fewer windows than asked for")
                }
                _ => result,
            }
        });
    }
    let mut outcome = Ok(());
    while let Some(done) = viewers.join_next().await {
        if let Err(error) = done? {
            println!("viewer ended: {error:#}");
            outcome = Err(error);
        }
    }
    outcome
}

/// Host: whose audio a session gets. Sharing selected windows (or one)
/// sends only their application's, when they are all one process (v1);
/// otherwise none, since everything else stays private. Within a Quick App
/// ("Only stream this app") it is that app's process tree's or nothing,
/// never the whole computer's.
fn audio_source(
    o: &Options,
    config: &std::path::Path,
    scope: Option<&quick_apps::QuickApp>,
) -> std::result::Result<audio::Source, String> {
    if o.windows.is_empty() && scope.is_none() {
        return Ok(audio::Source::System);
    }
    let pids: Vec<u32> = catalog::shared_in(&o.windows, o.max_windows, scope)
        .iter()
        .map(|w| w.pid)
        .collect();
    let processes = quick_apps::Processes::now();
    choose_audio(scope, pids, &processes).map(|pid| audio::Source::Process {
        pid,
        dir: config.to_path_buf(),
    })
}

/// The process whose audio (with its children's) a restricted share
/// sends: the shared windows' one process, or within a Quick App the one
/// that all its windows' processes descend from.
fn choose_audio(
    scope: Option<&quick_apps::QuickApp>,
    mut pids: Vec<u32>,
    processes: &quick_apps::Processes,
) -> std::result::Result<u32, String> {
    pids.retain(|&pid| pid != 0);
    pids.sort_unstable();
    pids.dedup();
    match (scope, pids.as_slice()) {
        (Some(app), []) => Err(format!(
            "{}'s audio is off: none of its windows is open yet",
            app.name
        )),
        (_, [pid]) => Ok(*pid),
        (Some(app), _) => processes.common_root(&pids).ok_or_else(|| {
            format!(
                "{}'s audio is off: its windows come from unrelated processes",
                app.name
            )
        }),
        (None, _) => {
            Err("Selected-window audio is unavailable for this combination of applications".into())
        }
    }
}

/// Viewer: gamepads and USB devices (the Devices menu, `--gamepads`,
/// `--usb`), on one streaming session at a time.
pub(crate) fn start_devices(session: &frames_net::Session, o: &Options) {
    let (session, gamepads, usb) = (session.clone(), o.gamepads, o.usb.clone());
    tokio::spawn(async move {
        if let Err(error) = pads::viewer(session, gamepads, usb).await {
            println!("devices stopped: {error:#}");
        }
    });
}

/// The files given with --get and --send, on the first session.
pub(crate) fn start_transfers(session: &frames_net::Session, o: &Options) {
    for path in o.get_files.clone() {
        let session = session.clone();
        let dir = o
            .receive_dir
            .clone()
            .unwrap_or_else(transfer::default_receive_dir);
        tokio::spawn(async move {
            let started = std::time::Instant::now();
            match transfer::fetch(&session, &path, dir).await {
                Ok(saved) => println!(
                    "FILE fetched {path} in {:.1} s to {}",
                    started.elapsed().as_secs_f64(),
                    saved.display()
                ),
                Err(error) => println!("FILE fetching {path} failed: {error:#}"),
            }
        });
    }
    for path in o.send_files.clone() {
        let session = session.clone();
        tokio::spawn(async move {
            let started = std::time::Instant::now();
            match transfer::send(&session, &path).await {
                Ok(saved) => println!(
                    "FILE sent {} in {:.1} s (saved as {saved})",
                    path.display(),
                    started.elapsed().as_secs_f64()
                ),
                Err(error) => println!("FILE sending {} failed: {error:#}", path.display()),
            }
        });
    }
}

#[cfg(windows)]
async fn run_client(session: frames_net::Session, o: Options) -> Result<()> {
    windows_client::run(session, o).await
}

#[cfg(target_os = "macos")]
async fn run_client(session: frames_net::Session, o: Options) -> Result<()> {
    mac_client::run(session, o).await
}

#[cfg(not(any(windows, target_os = "macos")))]
async fn run_client(_: frames_net::Session, _: Options) -> Result<()> {
    bail!("viewing needs the Windows or macOS backends")
}

/// The name other devices see: `--name`, else the name chosen in the
/// launcher's Settings (`device-name.json`, shared with v1), else the
/// computer's name.
fn device_name(o: &Options) -> String {
    o.name
        .clone()
        .or_else(|| {
            let config = admit::config_dir(o.config_dir.as_ref()).ok()?;
            let bytes = std::fs::read(config.join("device-name.json")).ok()?;
            serde_json::from_slice::<Option<String>>(&bytes)
                .ok()
                .flatten()
                .map(|name| name.trim().to_string())
                .filter(|name| !name.is_empty())
        })
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .or_else(|| {
            std::process::Command::new("scutil")
                .args(["--get", "ComputerName"])
                .output()
                .ok()
                .filter(|out| out.status.success())
                .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        })
        .unwrap_or_else(|| "Frames device".into())
}

/// What this viewer can decode, best first. Windows decodes AV1 in
/// hardware on recent GPUs and with dav1d otherwise; VideoToolbox on the
/// Macs Frames supports has no AV1 decoder.
fn viewer_decoders() -> Vec<Codec> {
    if cfg!(target_os = "macos") {
        vec![Codec::Hevc, Codec::H264]
    } else {
        vec![Codec::Hevc, Codec::Av1, Codec::H264]
    }
}

/// The codec for a stream: `--codec` if given; otherwise the most
/// efficient one both ends support. HEVC needs about 30-40% less bitrate
/// than H.264 at the same quality; on a relayed path, where bitrate is
/// scarce, AV1 comes first when both ends have it.
pub fn choose_codec(o: &Options, encoders: &[Codec], relayed: bool) -> Codec {
    if o.codec_explicit || o.viewer_decoders.is_empty() {
        return o.codec;
    }
    let order: &[Codec] = if relayed {
        &[Codec::Av1, Codec::Hevc, Codec::H264]
    } else {
        &[Codec::Hevc, Codec::Av1, Codec::H264]
    };
    order
        .iter()
        .copied()
        .find(|codec| encoders.contains(codec) && o.viewer_decoders.contains(codec))
        .unwrap_or(o.codec)
}

/// Writes one JSON line on a fresh channel.
pub async fn send_info(session: &frames_net::Session, info: StreamInfo) -> Result<()> {
    let (mut send, _recv) = session.open_channel().await?;
    let mut line = serde_json::to_vec(&info)?;
    line.push(b'\n');
    send.write_all(&line).await?;
    send.finish()?;
    Ok(())
}

/// Viewer: set by Disconnect in a Frames menu; every window of this viewer
/// ends.
pub static DISCONNECT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The most a viewer receives for an image quality (0 high, 1 medium, 2
/// low), in Mbps, as the launcher's Settings describe them.
pub fn quality_mbps(quality: u8) -> u16 {
    match quality {
        0 => 50,
        1 => 12,
        _ => 4,
    }
}

/// Viewer: its window's title, `--title` or else the host's name.
pub fn viewer_title(o: &Options, info: &StreamInfo) -> String {
    o.title.clone().unwrap_or_else(|| {
        if info.name.is_empty() {
            "Frames".into()
        } else {
            info.name.clone()
        }
    })
}

/// Viewer: waits for the host to approve this device and describe its
/// stream. Prints `Frames status:` lines for the launcher's Sessions list.
pub async fn await_stream(session: &frames_net::Session) -> Result<StreamInfo> {
    // A host seeing this device for the first time asks its user.
    println!("Frames status: waiting");
    println!("waiting for the host (it may be asking to approve this device)");
    let info = tokio::time::timeout(Duration::from_secs(150), receive_info(session))
        .await
        .context("the host did not start the stream within 150 s")??;
    println!(
        "stream: {:?} {}x{} at {} fps",
        info.codec, info.width, info.height, info.fps
    );
    launcher::report(
        "host",
        &serde_json::json!({"device": session.remote().to_string(), "name": info.name}),
    );
    println!("Frames status: connected");
    Ok(info)
}

pub async fn receive_info(session: &frames_net::Session) -> Result<StreamInfo> {
    let (_send, mut recv) = session.accept_channel().await?;
    let bytes = recv.read_to_end(4096).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// Runs a `frames-next` command line (the arguments after the program
/// name). The `frames-next` binary and `frames next` both call this, before
/// starting any other threads.
pub fn main_from(args: Vec<String>) -> Result<()> {
    // The viewer's controller reader (`frames_devices::gamepad::Capture`):
    // SDL needs this process's main thread.
    if args.first().map(String::as_str) == Some("gamepad-worker") {
        return frames_devices::gamepad::capture_worker();
    }
    // v1's identity and trust store read FRAMES_CONFIG_DIR; point them at
    // --config-dir before any other thread exists.
    if let Some(dir) = args
        .iter()
        .position(|a| a == "--config-dir")
        .and_then(|i| args.get(i + 1))
    {
        // SAFETY: single-threaded here (the runtime starts below).
        unsafe { std::env::set_var("FRAMES_CONFIG_DIR", dir) };
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    #[cfg(target_os = "macos")]
    {
        // AppKit (the viewer window) needs the main thread: run the session on
        // a worker and NSApplication here until the session ends.
        let worker = std::thread::spawn(move || {
            let result = runtime.block_on(run(args));
            frames_platform_macos::system::stop_app();
            result
        });
        frames_platform_macos::system::run_app(0.0);
        worker.join().expect("session thread")
    }
    #[cfg(not(target_os = "macos"))]
    runtime.block_on(run(args))
}

/// Whether `args` (what follows `frames next`) is a `host` or `connect`
/// command line this build accepts. For callers that build them, such as
/// the launcher, to test against.
pub fn valid_command(args: &[String]) -> bool {
    match args.first().map(String::as_str) {
        Some("host") => Options::parse(&args[1..]).is_ok(),
        Some("connect") => args.len() >= 2 && Options::parse(&args[2..]).is_ok(),
        _ => false,
    }
}

async fn run(args: Vec<String>) -> Result<()> {
    launcher::watch();
    match args.first().map(String::as_str) {
        Some("host") => host(Options::parse(&args[1..])?).await,
        #[cfg(windows)]
        Some("windows") => {
            for (_, title) in win::shareable_windows() {
                println!("{title}");
            }
            Ok(())
        }
        Some("impair") => impair::run(&args[1..]).await,
        Some("preview") => {
            let id = args.get(1).context("preview needs a device id")?;
            let out = args.get(2).context("preview needs a file to write")?;
            let png = directory::preview(id, Duration::from_secs(15)).await?;
            std::fs::write(out, &png)?;
            println!("preview saved to {out} ({} bytes)", png.len());
            Ok(())
        }
        Some("password") => {
            let o = Options::parse(args.get(2..).unwrap_or_default())?;
            let config = admit::config_dir(o.config_dir.as_ref())?;
            match args.get(1).map(String::as_str) {
                Some("set") => {
                    // From stdin, so it stays out of shell history.
                    println!("type the access password and press Enter:");
                    let mut line = String::new();
                    std::io::stdin().read_line(&mut line)?;
                    let id = admit::device_key(&config)?.id();
                    password::set(&config, &id, line.trim_end_matches(['\r', '\n']))?;
                    println!("access password set: devices that give it connect without asking");
                }
                Some("clear") => {
                    password::clear(&config)?;
                    println!("access password removed");
                }
                Some("status") => println!(
                    "access password {}",
                    if password::is_set(&config) { "set" } else { "not set" }
                ),
                _ => bail!("password takes set, clear or status"),
            }
            Ok(())
        }
        Some("doctor") => doctor::run(Options::parse(&args[1..])?).await,
        Some("quick-apps") => quick_apps::cli(&args[1..]).await,
        Some("discover") => {
            for device in frames_net::local::browse(Duration::from_secs(3)).await? {
                let addrs: Vec<String> = device.addr.ip_addrs().map(ToString::to_string).collect();
                println!("{}  {}  {}", device.id, device.name, addrs.join(" "));
            }
            Ok(())
        }
        Some("peers") => {
            let o = Options::parse(&args[1..])?;
            let config = admit::config_dir(o.config_dir.as_ref())?;
            let list = frames_net::trust::TrustList::load(&frames_net::trust::TrustList::path_in(
                &config,
            ))?;
            println!("this device: {}", admit::device_key(&config)?.id());
            if list.is_empty() {
                println!("no trusted devices");
            }
            for (id, device) in list.devices() {
                println!(
                    "{id}  {}  {}{}",
                    device.name,
                    if device.allow_control {
                        "view and control"
                    } else {
                        "view only"
                    },
                    if device.legacy_fingerprint.is_some() {
                        "  (from v1)"
                    } else {
                        ""
                    }
                );
            }
            Ok(())
        }
        Some("revoke") => {
            let prefix = args
                .get(1)
                .context("revoke needs a device id (or its start)")?;
            let o = Options::parse(&args[2..])?;
            let config = admit::config_dir(o.config_dir.as_ref())?;
            let path = frames_net::trust::TrustList::path_in(&config);
            let mut list = frames_net::trust::TrustList::load(&path)?;
            let removed = list.remove_by_prefix(prefix)?;
            list.save(&path)?;
            println!("removed {removed}; it must be approved again to connect");
            Ok(())
        }
        Some("connect") => {
            let id = args.get(1).context("connect needs an endpoint id")?;
            if args.iter().any(|a| a == "--only-app") && !args.iter().any(|a| a == "--app") {
                bail!("--only-app needs --app ID (the host's Quick App)");
            }
            let o = Options::parse(&args[2..])?;
            tokio::select! {
                result = connect(id, o) => result,
                () = launcher::gone() => {
                    println!("disconnected: the launcher is gone");
                    Ok(())
                }
            }
        }
        _ => bail!(
            "usage: frames-next doctor [--config-dir DIR] [--relay URL]
       frames-next host [--source monitor|qa] [--codec hevc|h264|av1] [--encoder nvenc|amf|qsv|mf|software] [--fps N] [--mbps N] [--hold N] [--seconds N] [--approve view|control[-once]] [--config-dir DIR] [--relay URL] [--relay-only] [--receive-dir DIR] [--clipboard on|off|qa] [--audio on|off|qa]\n       frames-next connect <endpoint-id> [--vsync] [--seconds N]"
        ),
    }
}

#[cfg(test)]
mod codec_tests {
    use super::*;

    #[test]
    fn selected_windows_send_their_one_applications_audio() {
        let none = quick_apps::Processes::with_parents(&[]);
        assert_eq!(choose_audio(None, vec![42, 42], &none), Ok(42));
        for mixed in [vec![42, 7], vec![], vec![0]] {
            assert_eq!(
                choose_audio(None, mixed, &none).unwrap_err(),
                "Selected-window audio is unavailable for this combination of applications"
            );
        }
        // Sharing everything sends everything.
        let o = Options::parse(&[]).unwrap();
        assert!(matches!(
            audio_source(&o, std::path::Path::new("/tmp"), None),
            Ok(audio::Source::System)
        ));
    }

    #[test]
    fn only_this_app_never_sends_the_computers_audio() {
        let app = quick_apps::QuickApp {
            id: "game".into(),
            name: "Game".into(),
            command: "/x/game".into(),
            args: Vec::new(),
            path: "/x/game".into(),
        };
        // 10 the app, 11 and 12 its children, 20 a stranger.
        let processes = quick_apps::Processes::with_parents(&[(11, 10), (12, 11), (20, 1)]);
        // A Quick App on a host with no --window filters: still only the
        // app, and nothing while it has no window.
        let o = Options::parse(&[]).unwrap();
        assert!(o.windows.is_empty());
        assert!(
            !matches!(
                audio_source(&o, std::path::Path::new("/tmp"), Some(&app)),
                Ok(audio::Source::System)
            ),
            "never the whole computer's audio"
        );
        assert!(choose_audio(Some(&app), vec![], &processes).is_err());
        assert_eq!(
            choose_audio(Some(&app), vec![12, 10, 11], &processes),
            Ok(10)
        );
        assert!(
            choose_audio(Some(&app), vec![10, 20], &processes).is_err(),
            "unrelated processes: no audio rather than a guess"
        );
    }

    fn options(decoders: Vec<Codec>) -> Options {
        let mut o = Options::parse(&[]).unwrap();
        o.viewer_decoders = decoders;
        o
    }

    #[test]
    fn the_best_shared_codec_wins_and_an_explicit_one_is_kept() {
        let all = [Codec::Hevc, Codec::H264, Codec::Av1];
        let mac_viewer = options(vec![Codec::Hevc, Codec::H264]);
        assert_eq!(choose_codec(&mac_viewer, &all, true), Codec::Hevc);
        let pc_viewer = options(vec![Codec::Hevc, Codec::Av1, Codec::H264]);
        assert_eq!(choose_codec(&pc_viewer, &all, true), Codec::Av1, "relayed");
        assert_eq!(choose_codec(&pc_viewer, &all, false), Codec::Hevc, "direct");
        let h264_host = [Codec::H264];
        assert_eq!(choose_codec(&pc_viewer, &h264_host, false), Codec::H264);
        let mut explicit = options(vec![Codec::Hevc]);
        explicit.codec = Codec::H264;
        explicit.codec_explicit = true;
        assert_eq!(choose_codec(&explicit, &all, false), Codec::H264);
        let silent = options(Vec::new());
        assert_eq!(
            choose_codec(&silent, &all, true),
            silent.codec,
            "old viewer"
        );
    }
}
