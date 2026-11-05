//! The host's application windows, for viewers that open each one in its
//! own local window ("Application windows" in the launcher).
//!
//! A viewer opens one session that asks for the catalog (its hello says
//! so): the host lists the windows it shares on a stream it opens, one JSON
//! line per change, until the session ends. The viewer then opens a session
//! per window, naming the window by id; each streams that window alone.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// How often the host looks for opened, closed and renamed windows.
const POLL: Duration = Duration::from_millis(500);
/// The most windows one viewer is offered.
pub const MAX_WINDOWS: usize = 32;

/// One shareable window.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    /// HWND on Windows, CGWindowID on macOS; stable while the window exists.
    pub id: u64,
    pub title: String,
}

/// The windows this host shares now, at most `limit` (`--max-windows`):
/// with `--window` given, only windows whose title contains one of those
/// texts.
pub fn shared(filters: &[String], limit: usize) -> Vec<Window> {
    let filters: Vec<String> = filters.iter().map(|f| f.to_lowercase()).collect();
    let mut windows: Vec<Window> = platform_windows()
        .into_iter()
        .filter(|w| {
            filters.is_empty() || {
                let title = w.title.to_lowercase();
                filters.iter().any(|f| title.contains(f))
            }
        })
        .collect();
    windows.truncate(limit.min(MAX_WINDOWS));
    windows
}

/// Whether `id` is one of the windows this host shares now.
pub fn is_shared(filters: &[String], limit: usize, id: u64) -> bool {
    shared(filters, limit).iter().any(|w| w.id == id)
}

#[cfg(windows)]
fn platform_windows() -> Vec<Window> {
    use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
    let own = std::process::id();
    crate::win::shareable_windows()
        .into_iter()
        // Not this host's own windows (its test source, a viewer).
        .filter(|(hwnd, _)| {
            let mut pid = 0;
            unsafe { GetWindowThreadProcessId(*hwnd, Some(&mut pid)) };
            pid != own
        })
        .map(|(hwnd, title)| Window {
            id: hwnd.0 as usize as u64,
            title,
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn platform_windows() -> Vec<Window> {
    serde_json::from_str(&frames_platform_macos::capture::windows_json()).unwrap_or_default()
}

/// A shared window's frame in global points (x, y, width, height).
#[cfg(target_os = "macos")]
pub fn mac_bounds(id: u64) -> Option<(f64, f64, f64, f64)> {
    let list: Vec<serde_json::Value> =
        serde_json::from_str(&frames_platform_macos::capture::windows_json()).ok()?;
    let window = list.iter().find(|w| w["id"].as_u64() == Some(id))?;
    let number = |key: &str| window[key].as_f64();
    Some((
        number("x")?,
        number("y")?,
        number("width")?,
        number("height")?,
    ))
}

#[cfg(not(any(windows, target_os = "macos")))]
fn platform_windows() -> Vec<Window> {
    Vec::new()
}

/// Host: sends the shared windows to a catalog viewer until it leaves.
pub async fn serve(session: frames_net::Session, filters: Vec<String>, limit: usize) -> Result<()> {
    let (mut send, _recv) = crate::streams::open(&session, crate::streams::CATALOG).await?;
    let mut last: Option<Vec<Window>> = None;
    loop {
        if session.is_closed() {
            return Ok(());
        }
        let filters = filters.clone();
        let now = tokio::task::spawn_blocking(move || shared(&filters, limit)).await?;
        if last.as_ref() != Some(&now) {
            let mut line = serde_json::to_vec(&now)?;
            line.push(b'\n');
            if send.write_all(&line).await.is_err() {
                return Ok(());
            }
            last = Some(now);
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Viewer: the host's window lists as they change, from its catalog stream.
pub async fn receive(
    session: &frames_net::Session,
    lists: tokio::sync::mpsc::UnboundedSender<Vec<Window>>,
) -> Result<()> {
    let recv = loop {
        let (_send, mut recv) = session.accept_channel().await?;
        let mut kind = [0u8; 1];
        recv.read_exact(&mut kind).await?;
        if kind[0] == crate::streams::CATALOG {
            break recv;
        }
    };
    let mut lines = tokio::io::BufReader::new(recv);
    let mut line = String::new();
    loop {
        line.clear();
        if tokio::io::AsyncBufReadExt::read_line(&mut lines, &mut line).await? == 0 {
            return Ok(());
        }
        let windows: Vec<Window> = serde_json::from_str(line.trim())?;
        if lists.send(windows).is_err() {
            return Ok(());
        }
    }
}

/// Viewer (`--apps`): opens each of the host's windows in its own window
/// as the host's catalog lists them, closes them as they go away, and ends
/// when the host does or the user has closed every window.
pub async fn view(
    node: &frames_net::Node,
    target: iroh::EndpointAddr,
    o: &crate::Options,
    started: std::time::Instant,
) -> Result<()> {
    use std::collections::{HashMap, HashSet};
    let asks = |catalog: bool, window_id: Option<u64>, audio: bool| crate::admit::Asks {
        decoders: crate::viewer_decoders(),
        window: None,
        name: crate::device_name(o),
        audio,
        max_mbps: o.max_mbps_cap,
        catalog,
        window_id,
        password: o.password.clone(),
        preview: false,
    };
    let catalog = node.connect(target.clone()).await?;
    crate::admit::send_hello(&catalog, &node.id(), o.migrate_v1, asks(true, None, false)).await?;
    let code = catalog.pairing_code()?;
    println!(
        "connected in {:.0} ms, pairing code {code} (a new host shows the same code)",
        started.elapsed().as_secs_f64() * 1e3,
    );
    crate::launcher::report("code", &serde_json::json!({ "code": code }));
    println!("Frames status: waiting");
    let (lists_tx, mut lists) = tokio::sync::mpsc::unbounded_channel();
    let receiver = tokio::spawn({
        let catalog = catalog.clone();
        async move { receive(&catalog, lists_tx).await }
    });
    // Open windows by host id; windows the user closed stay closed until
    // the host closes them too.
    let mut open: HashMap<u64, frames_net::Session> = HashMap::new();
    let mut dismissed: HashSet<u64> = HashSet::new();
    let mut viewers = tokio::task::JoinSet::new();
    let mut audio_given = false;
    let mut services_started = false;
    let mut listed = false;
    // A host seeing this device for the first time asks its user first.
    let first_list = tokio::time::sleep(std::time::Duration::from_secs(150));
    tokio::pin!(first_list);
    loop {
        tokio::select! {
            () = &mut first_list, if !listed => {
                catalog.close("viewer left");
                receiver.abort();
                anyhow::bail!("the host did not list its windows within 150 s");
            }
            list = lists.recv() => {
                let Some(list) = list else { break };
                if !listed {
                    listed = true;
                    println!("Frames status: connected");
                    if list.is_empty() {
                        println!("the host shares no windows right now");
                    }
                }
                open.retain(|id, session| {
                    let keep = list.iter().any(|w| w.id == *id);
                    if !keep {
                        session.close("window closed");
                    }
                    keep
                });
                dismissed.retain(|id| list.iter().any(|w| w.id == *id));
                for window in list {
                    if open.contains_key(&window.id) || dismissed.contains(&window.id) {
                        continue;
                    }
                    let audio = !audio_given && o.audio != crate::audio::Mode::Off;
                    let session = node.connect(target.clone()).await?;
                    crate::admit::send_hello(
                        &session,
                        &node.id(),
                        false,
                        asks(false, Some(window.id), audio),
                    )
                    .await?;
                    // Files and gamepads go with the first window's session
                    // (the host serves them on streaming sessions).
                    if !audio_given && !services_started {
                        crate::start_services(&session, o);
                        services_started = true;
                    }
                    audio_given |= audio;
                    println!("opening window {}: {}", window.id, window.title);
                    let mut options = o.clone();
                    options.title = Some(window.title.clone());
                    options.window_id = Some(window.id);
                    if !audio {
                        options.audio = crate::audio::Mode::Off;
                    }
                    open.insert(window.id, session.clone());
                    let id = window.id;
                    viewers.spawn(async move { (id, crate::run_client(session, options).await) });
                }
            }
            Some(done) = viewers.join_next(), if !viewers.is_empty() => {
                let (id, result) = done?;
                if let Err(error) = result {
                    println!("window {id} ended: {error:#}");
                }
                // Still listed: the user closed it here.
                if let Some(session) = open.remove(&id) {
                    session.close("closed by the viewer");
                    dismissed.insert(id);
                }
                if open.is_empty() && !dismissed.is_empty() {
                    println!("every window was closed");
                    break;
                }
            }
        }
    }
    for session in open.values() {
        session.close("viewer left");
    }
    viewers.shutdown().await;
    receiver.abort();
    let reason = catalog.close_reason();
    catalog.close("viewer left");
    match reason.as_deref() {
        Some(crate::admit::NOT_APPROVED) => {
            anyhow::bail!("the host refused this device (not approved there)")
        }
        Some(crate::admit::WRONG_PASSWORD) => {
            anyhow::bail!("the host refused this device: wrong access password")
        }
        _ => {}
    }
    Ok(())
}
