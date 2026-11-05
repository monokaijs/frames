//! Messages between a session process and the Frames launcher that started
//! it. The launcher reads the process's output (one `Frames <kind>: <json>`
//! line per event, next to `Frames status: <phase>` lines) and answers
//! pairing requests on the process's stdin, one line per request.
//!
//! The launcher holds the only write end of that stdin and sets
//! [`LIFETIME_ENV`]: when stdin closes, the launcher is gone (quit, crashed
//! or force quit) and the session ends through its normal path.

use crate::admit::Approve;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::{Mutex, Notify, mpsc};

/// Set by the launcher together with a piped stdin.
pub const LIFETIME_ENV: &str = "FRAMES_LAUNCHER_LIFETIME";
/// A session that has not ended this long after the launcher went is cut
/// short.
const FORCED_EXIT: Duration = Duration::from_secs(10);

static GONE: AtomicBool = AtomicBool::new(false);
static GONE_NOTIFY: Notify = Notify::const_new();

/// Starts watching for the launcher when it started this process.
pub fn watch() {
    if std::env::var_os(LIFETIME_ENV).is_some() {
        answers();
    }
}

/// Resolves once the launcher that started this process is gone; never for
/// processes started otherwise.
pub async fn gone() {
    if std::env::var_os(LIFETIME_ENV).is_none() {
        return std::future::pending().await;
    }
    loop {
        let notified = GONE_NOTIFY.notified();
        if GONE.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }
}

/// How long the launcher's pairing prompt waits for the person at the host.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(120);

/// One event for the launcher.
pub fn report(kind: &str, value: &serde_json::Value) {
    println!("Frames {kind}: {value}");
}

/// Lines from stdin, read by one thread for the life of the process, so a
/// request that timed out leaves no reader behind to take the next answer.
fn answers() -> &'static Mutex<mpsc::UnboundedReceiver<String>> {
    static ANSWERS: OnceLock<Mutex<mpsc::UnboundedReceiver<String>>> = OnceLock::new();
    ANSWERS.get_or_init(|| {
        let (tx, rx) = mpsc::unbounded_channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            while std::io::stdin().read_line(&mut line).is_ok_and(|n| n > 0) {
                // Answers nobody waits for are dropped (the receiver lives
                // as long as the process).
                let _ = tx.send(line.trim().to_string());
                line.clear();
            }
            if std::env::var_os(LIFETIME_ENV).is_some() {
                println!("the launcher is gone; ending this session");
                GONE.store(true, Ordering::Release);
                GONE_NOTIFY.notify_waiters();
                std::thread::sleep(FORCED_EXIT);
                println!("the session did not end after the launcher quit; exiting");
                std::process::exit(0);
            }
        });
        Mutex::new(rx)
    })
}

/// Asks the launcher whether to let `device` in. Requests come one at a
/// time (the host admits sequentially), so the next line answers this one:
/// `view`, `control`, `view-once` or `control-once`; anything else, a
/// closed stdin or no answer within two minutes refuses.
pub async fn ask_pairing(device: &iroh::EndpointId, name: &str, code: &str) -> Option<Approve> {
    report(
        "pairing",
        &serde_json::json!({"device": device.to_string(), "name": name, "code": code}),
    );
    let answer = async {
        let mut answers = answers().lock().await;
        // Answers to earlier requests that came after their timeout.
        while answers.try_recv().is_ok() {}
        tokio::time::timeout(ANSWER_TIMEOUT, answers.recv())
            .await
            .ok()
            .flatten()
    };
    let choice = answer.await.as_deref().and_then(Approve::parse);
    report(
        "pairing-done",
        &serde_json::json!({"device": device.to_string(), "approved": choice.is_some()}),
    );
    choice
}
