//! Sessions the launcher starts end when the launcher does, however it ends.
//!
//! The launcher holds the only write end of each session's stdin. On macOS a
//! Quit (Cmd+Q, Dock menu) lets AppKit call `exit()` right after
//! `applicationWillTerminate`, so the launcher's own shutdown code never runs;
//! crashes and force quits skip it everywhere. In every case the pipe closes,
//! and the session shuts down through its normal path instead of leaving its
//! remote windows, Dock icons and helpers behind.
use std::{io::Read, sync::OnceLock, time::Duration};
use tokio::sync::watch;

/// Set by the launcher together with a piped stdin. CLI use is unaffected.
pub const ENV: &str = "FRAMES_LAUNCHER_LIFETIME";
/// A graceful shutdown that has not finished by then is cut short.
const FORCED_EXIT: Duration = Duration::from_secs(10);

static GONE: OnceLock<watch::Receiver<bool>> = OnceLock::new();

/// Starts watching for the launcher when this process was started by it.
/// `on_gone` asks the session to end; `gone()` also resolves at that point.
pub fn watch(on_gone: impl FnOnce() + Send + 'static) {
    if std::env::var_os(ENV).is_none() {
        return;
    }
    let (sender, receiver) = watch::channel(false);
    if GONE.set(receiver).is_err() {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("frames-launcher-lifetime".into())
        .spawn(move || {
            let mut stdin = std::io::stdin().lock();
            let mut sink = [0u8; 64];
            while matches!(stdin.read(&mut sink), Ok(read) if read > 0) {}
            tracing::info!("launcher is gone; ending this session");
            let _ = sender.send(true);
            on_gone();
            std::thread::sleep(FORCED_EXIT);
            tracing::warn!("session did not end after the launcher quit; exiting");
            std::process::exit(0);
        });
    if let Err(error) = spawned {
        tracing::warn!("cannot watch the launcher: {error}");
    }
}

/// Resolves once the launcher that started this process is gone; never
/// resolves for processes started from the command line.
pub async fn gone() {
    match GONE.get() {
        Some(receiver) => {
            let mut receiver = receiver.clone();
            let _ = receiver.wait_for(|gone| *gone).await;
        }
        None => std::future::pending().await,
    }
}
