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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Window {
    /// HWND on Windows, CGWindowID on macOS; stable while the window exists.
    pub id: u64,
    pub title: String,
    /// The application it belongs to (its options are saved per app).
    #[serde(default)]
    pub app: String,
    /// It can be resized ("Resize remote app").
    #[serde(default = "resizable_default")]
    pub resizable: bool,
    /// The window that owns it (its dialog, menu or tooltip), by id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<u64>,
    /// "window", "dialog", "menu", "tooltip" or "popover" (an owned popup),
    /// as v1's roles.
    #[serde(default = "role_default")]
    pub role: String,
    /// Where the part the viewer sees is on the host's screen: pixels on
    /// Windows, points on macOS (viewers place owned popups by it).
    #[serde(default)]
    pub x: f64,
    #[serde(default)]
    pub y: f64,
    #[serde(default)]
    pub width: f64,
    #[serde(default)]
    pub height: f64,
    /// The application's icon, a PNG of at most 64x64, base64 (Windows).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// It blocks its parent until it closes (a modal dialog or a sheet):
    /// the viewer sends input for the parent to it instead (v1).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub modal: bool,
}

fn resizable_default() -> bool {
    true
}

fn role_default() -> String {
    "window".into()
}

impl Window {
    /// A menu, tooltip or owned popup: shown without a frame or focus.
    pub fn is_popup(&self) -> bool {
        matches!(self.role.as_str(), "menu" | "tooltip" | "popover" | "sheet")
    }
}

/// The catalog id of "Windows login / secure desktop": while UAC or the
/// sign-in screen is up, a privileged Windows host lists only it, and a
/// session for it streams the screen (v1's secure desktop surface).
pub const SECURE_DESKTOP: u64 = u64::MAX - 1;

/// Whether the secure desktop replaces the windows now (Windows hosts).
fn secure_desktop() -> bool {
    #[cfg(windows)]
    {
        frames_platform_windows::secure::secure_desktop_active()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// The windows this host shares now, at most `limit` (`--max-windows`):
/// with `--window` given, only windows whose title contains one of those
/// texts.
pub fn shared(filters: &[String], limit: usize) -> Vec<Window> {
    if secure_desktop() {
        #[cfg(windows)]
        if frames_platform_windows::secure::privileged() && filters.is_empty() {
            return vec![Window {
                id: SECURE_DESKTOP,
                title: "Windows login / secure desktop".into(),
                app: "Windows".into(),
                resizable: false,
                parent: None,
                role: "window".into(),
                x: 0.0,
                y: 0.0,
                width: 0.0,
                height: 0.0,
                icon: None,
                modal: false,
            }];
        }
        return Vec::new();
    }
    let filters: Vec<String> = filters.iter().map(|f| f.to_lowercase()).collect();
    let all = platform_windows();
    let chosen = |w: &Window| {
        filters.is_empty() || {
            let title = w.title.to_lowercase();
            filters.iter().any(|f| title.contains(f))
        }
    };
    // A selected window's own menus and tooltips come with it (untitled,
    // so no filter matches them): by their parent, or for ownerless menus
    // by their application.
    let mut windows: Vec<Window> = all
        .iter()
        .filter(|w| {
            chosen(w)
                || (w.is_popup()
                    && match w.parent {
                        Some(p) => all.iter().any(|o| o.id == p && chosen(o)),
                        None => {
                            !w.app.is_empty()
                                && all
                                    .iter()
                                    .any(|o| !o.is_popup() && chosen(o) && o.app == w.app)
                        }
                    })
        })
        .cloned()
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
    use windows::Win32::Foundation::{CloseHandle, RECT};
    use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };
    use windows::Win32::UI::WindowsAndMessaging::*;
    let own = std::process::id();
    // The shell's own windows, as v1 left out.
    const SHELL: [&str; 5] = [
        "Progman",
        "WorkerW",
        "Shell_TrayWnd",
        "Shell_SecondaryTrayWnd",
        "TaskManagerWindow",
    ];
    crate::win::shareable_windows()
        .into_iter()
        .filter_map(|(hwnd, title)| unsafe {
            let mut pid = 0;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            // Not this host's own windows (its test source, a viewer).
            if pid == own {
                return None;
            }
            // Windows on other virtual desktops and suspended apps are
            // cloaked: listed as visible, but nothing is drawn.
            let mut cloaked = 0u32;
            let _ =
                DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut u32 as *mut _, 4);
            let mut class = [0u16; 128];
            let length = GetClassNameW(hwnd, &mut class) as usize;
            let class = String::from_utf16_lossy(&class[..length]);
            let mut rect = RECT::default();
            let _ = GetWindowRect(hwnd, &mut rect);
            if cloaked != 0
                || SHELL.contains(&class.as_str())
                || rect.right - rect.left < 2
                || rect.bottom - rect.top < 2
            {
                return None;
            }
            let mut app = String::new();
            if let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
                let mut path = [0u16; 1024];
                let mut size = path.len() as u32;
                if QueryFullProcessImageNameW(
                    process,
                    PROCESS_NAME_WIN32,
                    windows::core::PWSTR(path.as_mut_ptr()),
                    &mut size,
                )
                .is_ok()
                {
                    let path = String::from_utf16_lossy(&path[..size as usize]);
                    app = std::path::Path::new(&path)
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                }
                let _ = CloseHandle(process);
            }
            // Frames' own windows (the launcher, another viewer), as v1.
            if app.eq_ignore_ascii_case("frames") || app.eq_ignore_ascii_case("frames-next") {
                return None;
            }
            let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
            let owner = GetWindow(hwnd, GW_OWNER).unwrap_or_default();
            // v1's roles, by window class and owner.
            let role = match class.as_str() {
                "#32768" => "menu",
                "tooltips_class32" => "tooltip",
                "#32770" => "dialog",
                _ if !owner.is_invalid() && style & WS_CAPTION.0 == WS_CAPTION.0 => "dialog",
                _ if !owner.is_invalid() => "popover",
                _ => "window",
            };
            // What the viewer sees by default: the client area.
            let shown = frames_platform_windows::input::captured_rect(hwnd, false);
            Some(Window {
                id: hwnd.0 as usize as u64,
                title,
                resizable: style & WS_THICKFRAME.0 != 0,
                parent: (!owner.is_invalid()).then_some(owner.0 as usize as u64),
                role: role.into(),
                x: f64::from(shown.left),
                y: f64::from(shown.top),
                width: f64::from(shown.right - shown.left),
                height: f64::from(shown.bottom - shown.top),
                icon: (role == "window" || role == "dialog")
                    .then(|| crate::win::icon_png(hwnd, &app))
                    .flatten(),
                // v1: an owned dialog whose owner is disabled is modal.
                modal: role == "dialog"
                    && !owner.is_invalid()
                    && !windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled(owner)
                        .as_bool(),
                app,
            })
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
    let mut last_problem: Option<&'static str> = None;
    loop {
        if session.is_closed() {
            return Ok(());
        }
        let filters = filters.clone();
        let (now, problem) =
            tokio::task::spawn_blocking(move || (shared(&filters, limit), session_problem()))
                .await?;
        // Why there is nothing to share (a locked Mac), as v1 said it.
        if problem != last_problem {
            if let Some(reason) = problem {
                let mut line = serde_json::to_vec(&serde_json::json!({ "reason": reason }))?;
                line.push(b'\n');
                if send.write_all(&line).await.is_err() {
                    return Ok(());
                }
            }
            last_problem = problem;
            // The list goes again after a reason (it clears it).
            last = None;
        }
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

#[derive(Deserialize)]
struct Problem {
    reason: String,
}

/// Why this host cannot share its windows now, if it cannot.
fn session_problem() -> Option<&'static str> {
    #[cfg(target_os = "macos")]
    {
        frames_platform_macos::capture::session_problem()
    }
    #[cfg(windows)]
    {
        (secure_desktop() && !frames_platform_windows::secure::privileged()).then_some(
            "Windows is showing a secure screen (UAC or sign-in). Only the Frames service shows it: turn on Unattended access on the host.",
        )
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        None
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
        // A host that cannot share explains why (`{"reason"}`); a list
        // clears it.
        if let Ok(problem) = serde_json::from_str::<Problem>(line.trim()) {
            crate::control::notice(problem.reason.clone());
            crate::control::update(|s| s.empty_reason = Some(problem.reason));
            continue;
        }
        let windows: Vec<Window> = serde_json::from_str(line.trim())?;
        crate::control::update(|s| s.empty_reason = None);
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
    first: bool,
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
    // Open windows by host id; windows the user hid (or closed here) stay
    // hidden until shown from the menus or the host closes them.
    let mut open: HashMap<u64, frames_net::Session> = HashMap::new();
    let mut hidden: HashSet<u64> = HashSet::new();
    let mut viewers = tokio::task::JoinSet::new();
    let mut audio_given = false;
    let mut transfers_started = !first;
    // The window whose session carries the devices (gamepads, USB).
    let mut devices_on: Option<u64> = None;
    let mut listed = false;
    let mut current: Vec<Window> = Vec::new();
    // A host seeing this device for the first time asks its user first.
    let first_list = tokio::time::sleep(std::time::Duration::from_secs(150));
    tokio::pin!(first_list);
    loop {
        if crate::control::ending() || catalog.is_closed() {
            break;
        }
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
                    crate::control::update(|s| s.connected = true);
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
                hidden.retain(|id| list.iter().any(|w| w.id == *id));
                current = list;
            }
            () = crate::control::windows_changed() => {
                for command in crate::control::take_window_commands() {
                    match command {
                        crate::control::Command::Show { id, shown } => {
                            if shown {
                                hidden.remove(&id);
                            } else if let Some(session) = open.remove(&id) {
                                session.close("hidden by the viewer");
                                hidden.insert(id);
                            }
                        }
                        crate::control::Command::ShowAll { shown } => {
                            if shown {
                                hidden.clear();
                            } else {
                                for (id, session) in open.drain() {
                                    session.close("hidden by the viewer");
                                    hidden.insert(id);
                                }
                            }
                        }
                        _ => {}
                    }
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
                    hidden.insert(id);
                }
            }
        }
        if crate::control::ending() {
            continue;
        }
        // Windows before the menus and sheets shown over them.
        let windows_first = current.iter().filter(|w| w.parent.is_none());
        for window in windows_first.chain(current.iter().filter(|w| w.parent.is_some())) {
            if open.contains_key(&window.id)
                || hidden.contains(&window.id)
                || window
                    .parent
                    .is_some_and(|parent| !open.contains_key(&parent))
            {
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
            // Files go with the first window's session (the host serves
            // them on streaming sessions).
            if !transfers_started {
                crate::start_transfers(&session, o);
                transfers_started = true;
            }
            audio_given |= audio;
            println!("opening window {}: {}", window.id, window.title);
            let mut options = o.clone();
            options.window_info = Some(window.clone());
            options.parent_info = window
                .parent
                .and_then(|p| current.iter().find(|w| w.id == p))
                .or_else(|| {
                    window
                        .is_popup()
                        .then(|| {
                            current
                                .iter()
                                .find(|w| !w.is_popup() && w.app == window.app)
                        })
                        .flatten()
                })
                .cloned();
            options.title = Some(window.title.clone());
            options.window_id = Some(window.id);
            options.app = Some(window.app.clone());
            options.menu_id = window.id;
            // A popup (not an owned dialog, which is a window of its own)
            // is placed over its window's picture.
            options.popup = options
                .parent_info
                .clone()
                .filter(|_| window.is_popup())
                .map(|parent| (window.clone(), parent));
            if !audio {
                options.audio = crate::audio::Mode::Off;
            }
            open.insert(window.id, session.clone());
            let id = window.id;
            viewers.spawn(async move { (id, crate::run_client(session, options).await) });
        }
        // Devices move to another window's session when theirs is hidden.
        if devices_on.is_none_or(|id| !open.contains_key(&id))
            && let Some((&id, session)) = open.iter().next()
        {
            crate::start_devices(session, o);
            devices_on = Some(id);
        }
        crate::control::set_windows(
            current
                .iter()
                .map(|w| crate::control::WindowEntry {
                    id: w.id,
                    title: w.title.clone(),
                    app: w.app.clone(),
                    shown: open.contains_key(&w.id),
                    desktop: false,
                    popup: w.is_popup(),
                    parent: w.parent,
                    modal: w.modal || (w.role == "sheet" && w.parent.is_some()),
                    resizable: w.resizable,
                    has_menus: false,
                    prefs: Default::default(),
                })
                .collect(),
        );
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

#[cfg(test)]
mod tests {
    #[test]
    fn macos_entries_carry_popups_and_frames() {
        let list: Vec<super::Window> = serde_json::from_str(
            r#"[{"id":1,"title":"Notes","app":"Notes","x":10,"y":20.5,"width":800,"height":600},
                {"id":2,"title":"Notes","app":"Notes","x":30,"y":40,"width":200,"height":300,"role":"menu","parent":1}]"#,
        )
        .unwrap();
        assert_eq!(
            (list[0].parent, list[0].role.as_str(), list[0].y),
            (None, "window", 20.5)
        );
        assert!(
            list[0].resizable,
            "resizable unless the host says otherwise"
        );
        assert_eq!((list[1].parent, list[1].role.as_str()), (Some(1), "menu"));
        // Older hosts send only id and title.
        let old: Vec<super::Window> = serde_json::from_str(r#"[{"id":3,"title":"A"}]"#).unwrap();
        assert_eq!((old[0].x, old[0].width, old[0].parent), (0.0, 0.0, None));
    }
}
