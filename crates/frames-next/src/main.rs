//! `frames-next`: the v2 stack end to end (docs/V2_PLAN.md, Phase 3).
//!
//! ```text
//! frames-next host [--source monitor|qa] [--codec hevc|h264|av1] [--encoder nvenc|amf|qsv|mf|software]
//!                  [--fps 60] [--mbps 30]
//!                  [--hold N] [--seconds N] [--relay-only]
//! frames-next discover | peers | revoke <device-id> [--config-dir DIR]\n       frames-next connect <endpoint-id> [--vsync] [--synthetic-input] [--seconds N] [--relay-only]
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
    /// Viewer: offer this device's v1 identity so a host that trusted it in
    /// v1 trusts it now.
    pub migrate_v1: bool,
    /// Host: pacing rate as a multiple of the target (experiments).
    pub pacing: Option<f64>,
    /// Clipboard sharing (both ends must turn it on).
    pub clipboard: clip::Mode,
    /// Name shown to nearby devices (default: the computer's name).
    pub name: Option<String>,
    /// Host audio (both ends must turn it on).
    pub audio: audio::Mode,
    /// Viewer: files to send to the host once connected.
    pub send_files: Vec<std::path::PathBuf>,
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
            migrate_v1: false,
            pacing: None,
            clipboard: clip::Mode::Off,
            name: None,
            audio: audio::Mode::Off,
            send_files: Vec::new(),
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
                "--migrate-v1" => o.migrate_v1 = true,
                "--pacing" => o.pacing = Some(value()?.parse()?),
                "--audio" => o.audio = audio::parse_mode(value()?)?,
                "--name" => o.name = Some(value()?.to_string()),
                "--send" => o.send_files.push(value()?.into()),
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
                    o.approve = Some(match value()? {
                        "view" => admit::Approve::View,
                        "control" => admit::Approve::Control,
                        other => bail!("--approve takes view or control, not {other}"),
                    })
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
        relays,
        ..frames_net::NodeConfig::default()
    })
}

async fn host(mut o: Options) -> Result<()> {
    let config = admit::config_dir(o.config_dir.as_ref())?;
    let key = admit::device_key(&config)?;
    let node = frames_net::Node::bind(&key, node_config(&o)?).await?;
    // Without internet, relays are unreachable; LAN viewers still find this
    // host through the local advertisement.
    if let Err(error) = node.online(Duration::from_secs(10)).await {
        println!("no relay reachable ({error:#}); reachable on this network only");
    }
    println!("endpoint id: {}", node.id());
    let _advertised = frames_net::local::advertise(&node, &device_name(&o))
        .map_err(|error| println!("not advertised on the local network: {error:#}"))
        .ok();
    loop {
        let session = node.accept().await.context("node closed")??;
        match admit::decide(&config, &session, o.approve).await? {
            Some(admitted) => {
                o.control = admitted.control;
                o.viewer_decoders = admitted.decoders;
                let audio = tokio::spawn({
                    let session = session.clone();
                    let mode = o.audio;
                    async move {
                        if let Err(error) = audio::host(session, mode).await {
                            println!("audio stopped: {error:#}");
                        }
                    }
                });
                let result = run_host(session, o).await;
                audio.abort();
                return result;
            }
            None => session.close("not approved"),
        }
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
    let session = match &nearby {
        Some(host) => {
            println!("found {} on this network", host.name);
            node.connect(host.addr.clone()).await?
        }
        _ => {
            node.connect(
                id.parse::<iroh::EndpointId>()
                    .context("not a device id, and no single nearby device starts with it")?,
            )
            .await?
        }
    };
    admit::send_hello(&session, &node.id(), o.migrate_v1, viewer_decoders()).await?;
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
    println!(
        "connected in {:.0} ms, pairing code {} (a new host shows the same code)",
        started.elapsed().as_secs_f64() * 1e3,
        session.pairing_code()?
    );
    let result = run_client(session.clone(), o).await;
    match (&result, session.close_reason()) {
        (Err(_), Some(reason)) if reason == "not approved" => {
            anyhow::bail!("the host refused this device (not approved there)")
        }
        _ => result,
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

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
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
            "usage: frames-next host [--source monitor|qa] [--codec hevc|h264|av1] [--encoder nvenc|amf|qsv|mf|software] [--fps N] [--mbps N] [--hold N] [--seconds N] [--approve view|control] [--config-dir DIR] [--relay URL] [--relay-only] [--receive-dir DIR] [--clipboard on|off|qa] [--audio on|off|qa]\n       frames-next connect <endpoint-id> [--vsync] [--seconds N]"
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
