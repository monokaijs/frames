#![cfg_attr(windows, windows_subsystem = "windows")]

mod connection_history;
mod connection_preferences;
mod device_preview;
mod device_setup;
mod launcher;
mod launcher_status;
#[cfg(windows)]
mod service;
mod update;
use clap::{Parser, Subcommand, ValueEnum};
use std::{net::SocketAddr, path::PathBuf};

#[derive(Parser)]
#[command(
    name = "frames",
    version,
    about = "Native per-window remote control for Windows and macOS"
)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Subcommand)]
enum Command {
    #[command(hide = true)]
    GamepadWorker,
    /// Install optional drivers used by device forwarding.
    Devices {
        #[command(subcommand)]
        action: DeviceCommand,
    },
    #[command(hide = true)]
    DeviceLease {
        #[arg(long)]
        bus_id: String,
        #[arg(long)]
        port: u16,
        #[arg(long)]
        token: String,
    },
    /// Verify bundled codecs without capturing the desktop or playing audio.
    MediaCheck,
    /// Print bundled library licenses and source/build references.
    Licenses,
    /// Check for a signed release, or download and install it after sessions close.
    Update {
        #[arg(long)]
        check: bool,
    },
    #[command(hide = true)]
    ApplyUpdate {
        plan: PathBuf,
        #[arg(long)]
        relaunch: bool,
        #[arg(long)]
        stop_sessions: bool,
    },
    /// Inspect macOS permissions, optionally opening the system permission prompts.
    #[cfg(target_os = "macos")]
    Permissions {
        #[arg(long)]
        request: bool,
    },
    #[cfg(windows)]
    Service {
        #[command(subcommand)]
        action: ServiceCommand,
    },
    /// Share this desktop. New devices require --pair and local confirmation.
    Host {
        /// Share only the locally selected window identities.
        #[arg(long = "share-window")]
        share_windows: Vec<String>,
        /// Share one window without taking over the host's input on Windows.
        #[arg(long = "one-window", alias = "shared-game")]
        shared_game: bool,
        /// Local HWND for automation; choose windows in the launcher preview picker.
        #[arg(long, requires = "shared_game")]
        shared_window: Option<u64>,
        #[arg(long, default_value = "0.0.0.0:47990")]
        listen: SocketAddr,
        #[arg(long)]
        pair: bool,
        #[arg(long)]
        unattended: bool,
        #[arg(long)]
        exclude: Vec<String>,
        #[arg(long)]
        receive_dir: Option<PathBuf>,
        #[arg(long, default_value_t = 8)]
        max_streams: usize,
        #[arg(long)]
        no_clipboard: bool,
        /// Encoder to use; automatic again when it does not work here.
        #[arg(long, value_enum, default_value = "auto")]
        encoder: EncoderChoice,
    },
    /// Open the host's application windows as native local windows.
    /// Connect to a device by its device ID (Frames Next).
    Connect {
        address: String,
        #[arg(long, default_value = "high")]
        quality: frames_core::adaptive::QualityPreset,
        /// Total media limit in Mbps (0 selects automatic bandwidth).
        #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u32).range(0..=200))]
        bandwidth_mbps: u32,
        #[arg(long, value_enum, default_value = "auto")]
        codec: CodecChoice,
        #[arg(long)]
        no_clipboard: bool,
        #[arg(long)]
        audio: bool,
        #[arg(long, value_enum, default_value = "windows")]
        view: ViewChoice,
    },
    /// Find Frames hosts on the local network.
    Discover {
        #[arg(long, default_value_t = 3)]
        seconds: u64,
    },
    /// What this device can do as a host or viewer, and what is in the way.
    Doctor,
    /// Enumerate shareable application windows without capturing them.
    Windows,
    /// Print reviewed game compatibility evidence bundled with this build.
    Games,
    /// This device's ID and the devices trusted to connect to it.
    Peers,
    /// Stop trusting a device (its ID, or the start of it).
    Revoke { fingerprint: String },
    /// This device's ID (as `peers`).
    Identity,
}
#[derive(Subcommand)]
enum DeviceCommand {
    /// Install gamepad, usb-export (local devices), or usb-host (remote receiving) support.
    Setup { target: String },
}
#[cfg(windows)]
#[derive(Subcommand)]
enum ServiceCommand {
    Install,
    Uninstall,
    #[command(hide = true)]
    Run,
    #[command(hide = true)]
    Agent,
}
#[derive(Clone, Copy, ValueEnum)]
enum CodecChoice {
    Auto,
    H264,
    Hevc,
    Av1,
    Vp9,
}
#[derive(Clone, Copy, ValueEnum)]
enum EncoderChoice {
    Auto,
    Nvenc,
    Amf,
    Qsv,
    Mf,
    Software,
}
impl EncoderChoice {
    fn backend(self) -> Option<&'static str> {
        match self {
            Self::Auto => None,
            Self::Nvenc => Some("nvenc"),
            Self::Amf => Some("amf"),
            Self::Qsv => Some("qsv"),
            Self::Mf => Some("mf"),
            Self::Software => Some("software"),
        }
    }
}
#[derive(Clone, Copy, ValueEnum)]
enum ViewChoice {
    Windows,
    Desktop,
}

/// `frames host` as a Frames Next command line: shares until stopped;
/// trusted devices connect, and new ones are asked about on a terminal (or
/// refused without one, as at login and in the service).
fn next_host_args(
    encoder: EncoderChoice,
    no_clipboard: bool,
    receive_dir: Option<&std::path::Path>,
) -> Vec<String> {
    let mut args: Vec<String> = ["host", "--stay", "--audio", "on", "--clipboard"]
        .map(String::from)
        .into();
    args.push(if no_clipboard { "off" } else { "on" }.into());
    if let Some(backend) = encoder.backend() {
        args.extend(["--prefer-encoder".into(), backend.into()]);
    }
    if let Some(dir) = receive_dir {
        args.extend(["--receive-dir".into(), dir.display().to_string()]);
    }
    args
}

/// `frames connect` as a Frames Next command line.
fn next_connect_args(
    target: &str,
    quality: frames_core::adaptive::QualityPreset,
    bandwidth_mbps: u32,
    codec: CodecChoice,
    no_clipboard: bool,
    audio: bool,
    view: ViewChoice,
) -> Vec<String> {
    use frames_core::adaptive::QualityPreset;
    let preset: u32 = match quality {
        QualityPreset::High => 50,
        QualityPreset::Medium => 12,
        QualityPreset::Low => 4,
    };
    let max = if bandwidth_mbps == 0 {
        preset
    } else {
        preset.min(bandwidth_mbps)
    };
    let on_off = |on: bool| if on { "on" } else { "off" }.to_string();
    let mut args = vec![
        "connect".into(),
        target.into(),
        "--migrate-v1".into(),
        "--audio".into(),
        on_off(audio),
        "--clipboard".into(),
        on_off(!no_clipboard),
        "--max-mbps".into(),
        max.to_string(),
    ];
    if matches!(view, ViewChoice::Windows) {
        args.push("--apps".into());
    }
    let codec = match codec {
        CodecChoice::H264 => Some("h264"),
        CodecChoice::Hevc => Some("hevc"),
        CodecChoice::Av1 => Some("av1"),
        CodecChoice::Auto | CodecChoice::Vp9 => None,
    };
    if let Some(codec) = codec {
        args.extend(["--codec".into(), codec.into()]);
    }
    args
}

fn main() {
    #[cfg(windows)]
    attach_cli_console();
    // The v2 stack (docs/V2_PLAN.md): `frames next host|connect|...`. Runs
    // before anything else starts threads; it sets up its own runtime.
    if std::env::args().nth(1).as_deref() == Some("next") {
        // A running session defers update downloads (see `update`).
        let _lease = matches!(std::env::args().nth(2).as_deref(), Some("host" | "connect"))
            .then(update::session_lease);
        if let Err(error) = frames_next::main_from(std::env::args().skip(2).collect()) {
            eprintln!("Frames Next: {error:#}");
            std::process::exit(1);
        }
        return;
    }
    if let Err(error) = run() {
        eprintln!("Frames: {error:#}");
        if std::env::var_os("FRAMES_GUI_CHILD").is_some()
            || std::env::args_os().len() == 1
            || matches!(
                std::env::args().nth(1).as_deref(),
                Some("connect" | "devices")
            )
        {
            let _ = frames_native::show_error("Frames", &format!("{error:#}"));
        }
        std::process::exit(1);
    }
}
#[cfg(windows)]
fn attach_cli_console() {
    if std::env::args_os().len() == 1 {
        return;
    }
    use windows::Win32::{
        Storage::FileSystem::{FILE_TYPE_UNKNOWN, GetFileType},
        System::Console::{
            ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
            STD_OUTPUT_HANDLE, SetStdHandle,
        },
    };
    // Reuse an existing terminal for CLI commands; never allocate a new one.
    // Preserve the pipes/files used by the launcher, updater, and shell redirects.
    unsafe {
        let handles = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE].map(|kind| {
            let handle = GetStdHandle(kind)
                .ok()
                .filter(|handle| !handle.is_invalid() && GetFileType(*handle) != FILE_TYPE_UNKNOWN);
            (kind, handle)
        });
        if AttachConsole(ATTACH_PARENT_PROCESS).is_ok() {
            for (kind, handle) in handles {
                if let Some(handle) = handle {
                    let _ = SetStdHandle(kind, handle);
                }
            }
        }
    }
}

fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "frames=info,frames_core=info,frames_native=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let args = Args::parse();
    if matches!(args.command, Some(Command::GamepadWorker)) {
        let _lease = update::runtime_lease()?;
        return frames_devices::gamepad::capture_worker();
    }
    if let Some(Command::DeviceLease {
        bus_id,
        port,
        token,
    }) = &args.command
    {
        let _lease = update::runtime_lease()?;
        return frames_devices::usb::lease_worker(bus_id, *port, token);
    }
    if let Some(Command::ApplyUpdate {
        plan,
        relaunch,
        stop_sessions,
    }) = &args.command
    {
        return update::apply(plan, *relaunch, *stop_sessions);
    }
    let _lease = update::runtime_lease()?;
    let Some(command) = args.command else {
        let result = launcher::run();
        if let Err(error) = update::install_on_exit() {
            tracing::warn!(%error, "update installation deferred");
        }
        return result;
    };
    #[cfg(windows)]
    if let Command::Service { action } = &command {
        match action {
            ServiceCommand::Install => return service::install(),
            ServiceCommand::Uninstall => return service::uninstall(),
            ServiceCommand::Run => return service::run(),
            // Shares this desktop for trusted devices, on Frames Next, with
            // the service's identity and trust list (FRAMES_CONFIG_DIR).
            ServiceCommand::Agent => {
                service::agent_environment()?;
                return frames_next::main_from(next_host_args(EncoderChoice::Auto, false, None));
            }
        }
    }
    // Sharing and connecting run on Frames Next (`frames next host|connect`),
    // also from the command line and the macOS sharing-at-login agent.
    match &command {
        Command::Host {
            encoder,
            no_clipboard,
            receive_dir,
            ..
        } => {
            let _session = update::session_lease()?;
            return frames_next::main_from(next_host_args(
                *encoder,
                *no_clipboard,
                receive_dir.as_deref(),
            ));
        }
        Command::Connect {
            address,
            quality,
            bandwidth_mbps,
            codec,
            no_clipboard,
            audio,
            view,
        } => {
            let _session = update::session_lease()?;
            return frames_next::main_from(next_connect_args(
                address,
                *quality,
                *bandwidth_mbps,
                *codec,
                *no_clipboard,
                *audio,
                *view,
            ));
        }
        // Devices, trust and diagnostics are Frames Next's.
        Command::Discover { .. } => return frames_next::main_from(vec!["discover".into()]),
        Command::Peers | Command::Identity => {
            return frames_next::main_from(vec!["peers".into()]);
        }
        Command::Revoke { fingerprint } => {
            return frames_next::main_from(vec!["revoke".into(), fingerprint.clone()]);
        }
        Command::Doctor => return frames_next::main_from(vec!["doctor".into()]),
        _ => {}
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    match command{
        Command::GamepadWorker=>unreachable!(),
        Command::DeviceLease{..}=>unreachable!(),
        Command::Devices{action:DeviceCommand::Setup{target}}=>runtime.block_on(device_setup::install(&target)),
        Command::Update{check}=>update::manual(check),
        Command::ApplyUpdate{..}=>unreachable!(),
        #[cfg(target_os="macos")] Command::Permissions{request}=>{println!("{}",serde_json::to_string_pretty(&frames_native::permission_status(request)?)?);Ok(())},
        #[cfg(windows)] Command::Service{..}=>unreachable!(),
        // Run on Frames Next above.
        Command::Host{..}|Command::Connect{..}=>unreachable!(),
        Command::Discover{..}|Command::Peers|Command::Revoke{..}|Command::Identity|Command::Doctor=>unreachable!(),
        Command::MediaCheck=>runtime.block_on(async{
            let codecs=frames_core::codec::probe().await;
            let decoders=frames_core::codec::probe_decoders().await;
            anyhow::ensure!(!codecs.is_empty() && decoders.len()==4,"Bundled video codec verification failed");
            let format=frames_core::audio::PcmFormat{rate:48000,channels:2};
            let (tx,rx)=tokio::sync::mpsc::channel(1);
            let mut encoder=frames_core::audio::encode(format,rx).await?;
            let mut decoder=frames_core::audio::decode(format).await?;
            let pixels:Vec<u8>=(0..480).flat_map(|i|{
                let sample=(i as f32*440.*std::f32::consts::TAU/48000.).sin()*0.2;
                [sample,sample].into_iter().flat_map(f32::to_le_bytes)
            }).collect();
            tx.send(pixels).await?;
            let packet=tokio::time::timeout(std::time::Duration::from_secs(3),encoder.packets.recv()).await?.ok_or_else(||anyhow::anyhow!("Opus encoder stopped"))?;
            decoder.input.send(packet).await?;
            let pcm=tokio::time::timeout(std::time::Duration::from_secs(3),decoder.pcm.recv()).await?.ok_or_else(||anyhow::anyhow!("Opus decoder stopped"))?;
            anyhow::ensure!(pcm.len()==frames_core::audio::packet_bytes(format) && pcm.chunks_exact(4).any(|v|f32::from_le_bytes(v.try_into().unwrap()).abs()>0.05),"Opus roundtrip failed");
            encoder.shutdown().await;decoder.shutdown().await;
            println!("{}",serde_json::to_string_pretty(&serde_json::json!({"protocol":frames_core::PROTOCOL_VERSION,"runtime":frames_core::codec::runtime_status()?,"encoders":codecs,"decoders":decoders,"opus_10ms_roundtrip":true}))?);Ok(())
        }),
        Command::Licenses=>{println!("{}",frames_core::codec::licenses());Ok(())},
        Command::Games=>{println!("{}",serde_json::to_string_pretty(&frames_core::shared_game::Catalog::bundled()?)?);Ok(())},
        Command::Windows=>{println!("{}",serde_json::to_string_pretty(&frames_native::desktop(vec![])?.windows()?)?);Ok(())},
    }
}

#[cfg(test)]
mod next_command_tests {
    use super::*;

    #[test]
    fn frames_host_and_connect_become_valid_frames_next_command_lines() {
        let host = next_host_args(
            EncoderChoice::Qsv,
            true,
            Some(std::path::Path::new("/tmp/in")),
        );
        assert!(frames_next::valid_command(&host), "{host:?}");
        assert!(host.windows(2).any(|w| w == ["--prefer-encoder", "qsv"]));
        assert!(host.windows(2).any(|w| w == ["--clipboard", "off"]));
        let id = "b".repeat(64);
        let connect = next_connect_args(
            &id,
            frames_core::adaptive::QualityPreset::Medium,
            8,
            CodecChoice::Vp9,
            false,
            true,
            ViewChoice::Windows,
        );
        assert!(frames_next::valid_command(&connect), "{connect:?}");
        // The bandwidth limit caps Medium's 12 Mbps; v2 has no VP9.
        assert!(connect.windows(2).any(|w| w == ["--max-mbps", "8"]));
        assert!(connect.iter().any(|a| a == "--apps") && !connect.iter().any(|a| a == "--codec"));
        // The clap surface still parses what the README shows.
        assert!(
            Args::try_parse_from([
                "frames", "connect", &id, "--audio", "--view", "desktop", "--codec", "hevc"
            ])
            .is_ok()
        );
        assert!(Args::try_parse_from(["frames", "host"]).is_ok());
    }
}
