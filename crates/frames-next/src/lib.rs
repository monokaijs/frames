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
mod clip;
mod doctor;
mod impair;
mod lane;
#[cfg(target_os = "macos")]
mod mac_client;
#[cfg(target_os = "macos")]
mod mac_host;
mod services;
mod stats;
mod streams;
mod transfer;
#[cfg(windows)]
mod win;
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
    /// Host, set at admission: the window this session shares.
    pub window: Option<String>,
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
    /// Host: `nvenc` (direct), or an FFmpeg backend: `amf`, `qsv`, `mf`,
    /// `software`.
    pub encoder: String,
    /// Where the device key and trust list live (default: v1's directory).
    pub config_dir: Option<std::path::PathBuf>,
    /// Host: approve unknown devices without asking (unattended tests).
    pub approve: Option<admit::Approve>,
    /// Host, set at admission: this viewer may send input.
    pub control: bool,
    /// `--codec` was given; otherwise the host picks (see [`choose_codec`]).
    pub codec_explicit: bool,
    /// Host, set at admission: what the viewer decodes, best first.
    pub viewer_decoders: Vec<Codec>,
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
            encoder: "nvenc".into(),
            config_dir: None,
            approve: None,
            control: false,
            codec_explicit: false,
            viewer_decoders: Vec::new(),
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
                "--encoder" => o.encoder = value()?.to_string(),
                "--config-dir" => o.config_dir = Some(value()?.into()),
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
    let mut audio_taken = false;
    loop {
        let session = tokio::select! {
            accepted = node.accept() => accepted.context("node closed")??,
            Some(done) = sessions.join_next(), if !sessions.is_empty() => {
                if let Ok(Err(error)) = done {
                    println!("session ended: {error:#}");
                }
                if sessions.is_empty() {
                    return Ok(());
                }
                continue;
            }
        };
        let Some(admitted) = admit::decide(&config, &session, o.approve).await? else {
            session.close("not approved");
            continue;
        };
        let mut session_options = o.clone();
        session_options.control = admitted.control;
        session_options.viewer_decoders = admitted.decoders;
        let index = admitted.window.unwrap_or(0) as usize;
        session_options.window = match (o.windows.get(index), o.windows.is_empty()) {
            (Some(title), _) => Some(title.clone()),
            (None, true) => None,
            (None, false) => {
                session.close("no such window");
                continue;
            }
        };
        // Audio goes with the first session only.
        let audio_mode = if audio_taken {
            audio::Mode::Off
        } else {
            o.audio
        };
        audio_taken |= o.audio != audio::Mode::Off;
        sessions.spawn(async move {
            let audio = tokio::spawn({
                let session = session.clone();
                async move {
                    if let Err(error) = audio::host(session, audio_mode).await {
                        println!("audio stopped: {error:#}");
                    }
                }
            });
            let result = run_host(session, session_options).await;
            audio.abort();
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
    // One session per shared window the viewer opens (--windows N).
    let mut viewers = tokio::task::JoinSet::new();
    for index in 0..o.open_windows {
        let session = node.connect(target.clone()).await?;
        let window = (o.open_windows > 1).then_some(index);
        admit::send_hello(
            &session,
            &node.id(),
            o.migrate_v1,
            viewer_decoders(),
            window,
        )
        .await?;
        if index == 0 {
            start_transfers(&session, &o);
        }
        println!(
            "connected in {:.0} ms, pairing code {} (a new host shows the same code)",
            started.elapsed().as_secs_f64() * 1e3,
            session.pairing_code()?
        );
        let session_options = o.clone();
        viewers.spawn(async move {
            let result = run_client(session.clone(), session_options).await;
            match (&result, session.close_reason()) {
                (Err(_), Some(reason)) if reason == "not approved" => {
                    anyhow::bail!("the host refused this device (not approved there)")
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

/// The files given with --get and --send, on the first session.
fn start_transfers(session: &frames_net::Session, o: &Options) {
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

/// The name nearby devices see.
fn device_name(o: &Options) -> String {
    o.name
        .clone()
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

pub async fn receive_info(session: &frames_net::Session) -> Result<StreamInfo> {
    let (_send, mut recv) = session.accept_channel().await?;
    let bytes = recv.read_to_end(4096).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// Runs a `frames-next` command line (the arguments after the program
/// name). The `frames-next` binary and `frames next` both call this, before
/// starting any other threads.
pub fn main_from(args: Vec<String>) -> Result<()> {
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

async fn run(args: Vec<String>) -> Result<()> {
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
        Some("doctor") => doctor::run(Options::parse(&args[1..])?).await,
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
            connect(id, Options::parse(&args[2..])?).await
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
