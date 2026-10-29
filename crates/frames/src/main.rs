#[cfg(windows)]
mod service;
mod session;
mod streaming;
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
    if let Err(error) = run() {
        eprintln!("Frames: {error:#}");
        if std::env::var_os("FRAMES_GUI_CHILD").is_some() || std::env::args_os().len() == 1 {
            let _ = frames_native::show_error("Frames", &format!("{error:#}"));
        }
        std::process::exit(1);
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
    let Some(command) = args.command else {
        return frames_native::launcher();
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
        #[cfg(target_os="macos")] Command::Permissions{request}=>{println!("{}",serde_json::to_string_pretty(&frames_native::permission_status(request)?)?);Ok(())},
        #[cfg(windows)] Command::Service{action:ServiceCommand::Agent}=>runtime.block_on(session::host("0.0.0.0:47990".parse()?,false,true,vec![],None,8,true)),
        #[cfg(windows)] Command::Service{..}=>unreachable!(),
        Command::Host{listen,pair,unattended,exclude,receive_dir,max_streams,no_clipboard}=>runtime.block_on(session::host(listen,pair,unattended,exclude,receive_dir,max_streams,!no_clipboard)),
        Command::Connect{address,codec,no_clipboard,audio,view}=>{
            let (updates_tx,updates_rx)=std::sync::mpsc::sync_channel(16);let(events_tx,events_rx)=tokio::sync::mpsc::unbounded_channel();
            let shutdown=updates_tx.clone();let task=runtime.spawn(async move{let result=session::client(address,codec.codec(),updates_tx,events_rx,!no_clipboard,audio,match view{ViewChoice::Windows=>frames_core::model::ViewMode::Windows,ViewChoice::Desktop=>frames_core::model::ViewMode::Desktop}).await;if let Err(e)=&result{tracing::error!(error=%e,"connection ended");}let _=shutdown.send(frames_native::ClientUpdate::Shutdown);result});
            let ui=frames_native::run_client(address.to_string(),updates_rx,events_tx);let network=runtime.block_on(task)?;ui?;network
        },
        Command::Discover{seconds}=>{println!("{}",serde_json::to_string_pretty(&runtime.block_on(frames_core::discovery::browse(seconds))?)?);Ok(())},
        Command::Doctor=>runtime.block_on(async{let capabilities=frames_core::codec::probe().await;let native=frames_native::desktop(vec![]);println!("{}",serde_json::to_string_pretty(&serde_json::json!({"protocol":frames_core::PROTOCOL_VERSION,"platform":std::env::consts::OS,"architecture":std::env::consts::ARCH,"ffmpeg":frames_core::codec::ffmpeg_path().to_string_lossy(),"codecs":capabilities,"windows":native.as_ref().ok().and_then(|d|d.windows().ok()).map(|w|w.len()),"native_error":native.err().map(|e|e.to_string()),"capture":"native per-window; CPU readback","release_gates":["hardware latency measurements","macOS build and permissions","secure desktop and login service integration","drag application compatibility","macOS prelogin handoff","end-to-end zero-copy media"]}))?);anyhow::ensure!(!capabilities.is_empty(),"no codec passed the encode/decode probe; install FFmpeg or set FRAMES_FFMPEG");Ok(())}),
        Command::Windows=>{println!("{}",serde_json::to_string_pretty(&frames_native::desktop(vec![])?.windows()?)?);Ok(())},
        Command::Send{address,path}=>runtime.block_on(session::send_file(address,path)),
        Command::Get{address,path}=>runtime.block_on(session::fetch_file(address,path)),
        Command::Peers=>{println!("{}",serde_json::to_string_pretty(&frames_core::trust::TrustStore::load()?)?);Ok(())},
        Command::Revoke{fingerprint}=>{anyhow::ensure!(frames_core::trust::TrustStore::load()?.revoke(&fingerprint)?,"unknown fingerprint");println!("Revoked {fingerprint}");Ok(())},
        Command::Identity=>{let identity=frames_core::trust::Identity::load()?;println!("{}\n{}",identity.device_id,frames_core::trust::config_dir()?.display());Ok(())},
    }
}
