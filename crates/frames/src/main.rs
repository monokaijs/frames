#![cfg_attr(windows, windows_subsystem = "windows")]

mod connection_history;
mod connection_preferences;
mod device_preview;
mod device_session;
mod device_setup;
mod file_worker;
mod launcher;
mod media_sender;
#[cfg(windows)]
mod service;
mod session;
mod streaming;
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
    },
    /// Open the host's application windows as native local windows.
    Connect {
        address: SocketAddr,
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
    /// Probe real encoders/decoders and native window capture support.
    Doctor,
    /// Enumerate shareable application windows without capturing them.
    Windows,
    /// Print reviewed game compatibility evidence bundled with this build.
    Games,
    /// Transfer one file into the host's configured receive folder.
    Send { address: SocketAddr, path: PathBuf },
    /// Download a regular file, relative to the host user's home folder.
    Get { address: SocketAddr, path: String },
    /// List approved device certificate fingerprints.
    Peers,
    /// Revoke a device's future access.
    Revoke { fingerprint: String },
    /// Display local identity fingerprint and configuration location.
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
enum ViewChoice {
    Windows,
    Desktop,
}
impl CodecChoice {
    fn codec(self) -> Option<frames_core::codec::Codec> {
        use frames_core::codec::Codec;
        match self {
            Self::Auto => None,
            Self::H264 => Some(Codec::H264),
            Self::Hevc => Some(Codec::Hevc),
            Self::Av1 => Some(Codec::Av1),
            Self::Vp9 => Some(Codec::Vp9),
        }
    }
}

fn main() {
    #[cfg(windows)]
    attach_cli_console();
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
        return frames_native::gamepad::capture_worker();
    }
    if let Some(Command::DeviceLease {
        bus_id,
        port,
        token,
    }) = &args.command
    {
        let _lease = update::runtime_lease()?;
        return frames_native::usb::lease_worker(bus_id, *port, token);
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
            ServiceCommand::Agent => service::agent_environment()?,
        }
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
        #[cfg(windows)] Command::Service{action:ServiceCommand::Agent}=>runtime.block_on(session::host("0.0.0.0:47990".parse()?,false,true,vec![],None,8,true)),
        #[cfg(windows)] Command::Service{..}=>unreachable!(),
        Command::Host{listen,pair,unattended,exclude,receive_dir,max_streams,no_clipboard,shared_game,shared_window,share_windows}=>{if !share_windows.is_empty(){runtime.block_on(session::selected_host(listen,pair,unattended,exclude,share_windows,shared_game,max_streams))}else if shared_game{runtime.block_on(session::shared_host(listen,pair,unattended,exclude,shared_window))}else{runtime.block_on(session::host(listen,pair,unattended,exclude,receive_dir,max_streams,!no_clipboard))}},
        Command::Connect{address,codec,no_clipboard,audio,view,bandwidth_mbps,quality}=>{
            let (updates_tx,updates_rx)=frames_native::client_channel(16);let(events_tx,events_rx)=tokio::sync::mpsc::unbounded_channel();
            let shutdown=updates_tx.clone();let task=runtime.spawn(async move{let result=session::client(address,codec.codec(),updates_tx,events_rx,!no_clipboard,session::MediaOptions{audio,bandwidth_mbps,quality},match view{ViewChoice::Windows=>frames_core::model::ViewMode::Windows,ViewChoice::Desktop=>frames_core::model::ViewMode::Desktop}).await;if let Err(e)=&result{tracing::error!(error=%e,"connection ended");}let _=shutdown.send(frames_native::ClientUpdate::Shutdown);result});
            let ui=frames_native::run_client(address.to_string(),updates_rx,events_tx);let network=runtime.block_on(task)?;ui?;network
        },
        Command::Discover{seconds}=>{println!("{}",serde_json::to_string_pretty(&runtime.block_on(frames_core::discovery::browse(seconds))?)?);Ok(())},
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
        Command::Doctor=>runtime.block_on(async{let capabilities=frames_core::codec::probe().await;let native=frames_native::desktop(vec![]).and_then(|d|d.windows());println!("{}",serde_json::to_string_pretty(&serde_json::json!({"protocol":frames_core::PROTOCOL_VERSION,"platform":std::env::consts::OS,"architecture":std::env::consts::ARCH,"media_runtime":frames_core::codec::runtime_status().map_err(|e|e.to_string()),"codecs":capabilities,"windows":native.as_ref().ok().map(|w|w.len()),"native_error":native.err().map(|e|e.to_string()),"capture":"native per-window; CPU readback","release_gates":["hardware latency measurements","macOS build and permissions","secure desktop and login service integration","drag application compatibility","macOS prelogin handoff","end-to-end zero-copy media"]}))?);anyhow::ensure!(!capabilities.is_empty(),"no bundled codec passed the encode/decode probe; run Frames doctor for details");Ok(())}),
        Command::Games=>{println!("{}",serde_json::to_string_pretty(&frames_core::shared_game::Catalog::bundled()?)?);Ok(())},
        Command::Windows=>{println!("{}",serde_json::to_string_pretty(&frames_native::desktop(vec![])?.windows()?)?);Ok(())},
        Command::Send{address,path}=>runtime.block_on(session::send_file(address,path)),
        Command::Get{address,path}=>runtime.block_on(session::fetch_file(address,path)),
        Command::Peers=>{println!("{}",serde_json::to_string_pretty(&frames_core::trust::TrustStore::load()?)?);Ok(())},
        Command::Revoke{fingerprint}=>{anyhow::ensure!(frames_core::trust::TrustStore::load()?.revoke(&fingerprint)?,"unknown fingerprint");println!("Revoked {fingerprint}");Ok(())},
        Command::Identity=>{let identity=frames_core::trust::Identity::load()?;println!("{}\n{}",identity.device_id,frames_core::trust::config_dir()?.display());Ok(())},
    }
}
