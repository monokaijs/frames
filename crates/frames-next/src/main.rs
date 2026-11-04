//! `frames-next`: the v2 stack end to end (docs/V2_PLAN.md, Phase 3).
//!
//! ```text
//! frames-next host [--source monitor|qa] [--codec hevc|h264|av1] [--encoder nvenc|amf|qsv|mf|software]
//!                  [--fps 60] [--mbps 30]
//!                  [--hold N] [--seconds N] [--relay-only]
//! frames-next connect <endpoint-id> [--vsync] [--synthetic-input] [--seconds N] [--relay-only]
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

#[cfg(target_os = "macos")]
mod mac_client;
#[cfg(target_os = "macos")]
mod mac_host;
mod stats;
#[cfg(windows)]
mod win;
#[cfg(windows)]
mod windows_client;
#[cfg(windows)]
mod windows_host;

/// Sent by the host on the first channel before any media.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct StreamInfo {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
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
    pub hold: u32,
    pub vsync: bool,
    pub seconds: u64,
    pub relay_only: bool,
    /// Viewer: post generated mouse and key messages to its own window.
    pub synthetic_input: bool,
    /// Host: `nvenc` (direct), or an FFmpeg backend: `amf`, `qsv`, `mf`,
    /// `software`.
    pub encoder: String,
}

impl Options {
    fn parse(args: &[String]) -> Result<Self> {
        let mut o = Self {
            qa_source: false,
            codec: Codec::Hevc,
            fps: 60,
            mbps: 30,
            hold: 48,
            vsync: false,
            seconds: 0,
            relay_only: false,
            synthetic_input: false,
            encoder: "nvenc".into(),
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
                "--seconds" => o.seconds = value()?.parse()?,
                "--vsync" => o.vsync = true,
                "--relay-only" => o.relay_only = true,
                "--synthetic-input" => o.synthetic_input = true,
                "--encoder" => o.encoder = value()?.to_string(),
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

fn node_config(o: &Options) -> frames_net::NodeConfig {
    frames_net::NodeConfig {
        relay_only: o.relay_only,
        ..frames_net::NodeConfig::default()
    }
}

async fn host(o: Options) -> Result<()> {
    let node = frames_net::Node::bind(&frames_net::DeviceKey::generate(), node_config(&o)).await?;
    node.online(Duration::from_secs(10)).await?;
    println!("endpoint id: {}", node.id());
    let session = node.accept().await.context("node closed")??;
    println!(
        "connected: {} pairing code {}",
        session.remote(),
        session.pairing_code()?
    );
    run_host(session, o).await
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
    let node = frames_net::Node::bind(&frames_net::DeviceKey::generate(), node_config(&o)).await?;
    let started = std::time::Instant::now();
    let session = node.connect(id.parse::<iroh::EndpointId>()?).await?;
    println!(
        "connected in {:.0} ms, pairing code {}",
        started.elapsed().as_secs_f64() * 1e3,
        session.pairing_code()?
    );
    run_client(session, o).await
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
        Some("connect") => {
            let id = args.get(1).context("connect needs an endpoint id")?;
            connect(id, Options::parse(&args[2..])?).await
        }
        _ => bail!(
            "usage: frames-next host [--source monitor|qa] [--codec hevc|h264|av1] [--encoder nvenc|amf|qsv|mf|software] [--fps N] [--mbps N] [--hold N] [--seconds N]\n       frames-next connect <endpoint-id> [--vsync] [--seconds N]"
        ),
    }
}
