//! The viewer's connection menu, shared by every window of one viewer
//! process (ported from v1's session menu and per-window options).
//!
//! The platform viewers build their menus (the menu bar, each window's
//! title-bar dropdown, the Windows tray) from [`snapshot`] whenever
//! [`version`] changes, and send what the user chose as a [`Command`]
//! (JSON from Swift). Session-wide choices apply here; window commands are
//! taken by that window's view loop ([`take_window`]); showing, hiding and
//! the view mode by the connect loop (`crate::catalog`); devices by
//! `crate::pads`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// How the host is shown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum View {
    /// Each application window in its own window ("Per-window view").
    #[default]
    Windows,
    /// The host's primary display in one window ("Full desktop view").
    Desktop,
}

/// One window's options, saved per host application (as v1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowPrefs {
    /// Resizing the window here resizes the host's window ("Resize remote
    /// app"); otherwise the picture is scaled ("Stretch locally").
    pub resize_remote: bool,
    /// Stretch the picture to the window instead of fitting it.
    pub fill: bool,
    /// Show the host window's title bar and borders too.
    pub header: bool,
    /// Swap Control and Command; `None` swaps when the platforms differ.
    pub swap: Option<bool>,
    /// Physical keys (terminals): never swap.
    pub physical: bool,
}

impl Default for WindowPrefs {
    fn default() -> Self {
        Self {
            resize_remote: true,
            fill: false,
            header: false,
            swap: None,
            physical: false,
        }
    }
}

impl WindowPrefs {
    /// Whether Control and Command trade places for a host on `host_platform`.
    pub fn swapped(&self, host_platform: &str) -> bool {
        !self.physical
            && self
                .swap
                .unwrap_or_else(|| !host_platform.is_empty() && host_platform != crate::platform())
    }
}

/// One of the host's windows, as the menus list it.
#[derive(Clone, Debug, Default, Serialize)]
pub struct WindowEntry {
    /// The host's window id; 0 for the full desktop view.
    pub id: u64,
    pub title: String,
    pub app: String,
    /// Open here (hidden windows are not streamed).
    pub shown: bool,
    /// The full desktop view (no header, menus or remote resizing).
    pub desktop: bool,
    /// A menu, tooltip or sheet over another window (not in the Windows
    /// menu).
    pub popup: bool,
    /// The window it belongs to (a dialog's owner, a popup's window).
    pub parent: Option<u64>,
    /// It blocks `parent` until it closes: input for the parent goes to it.
    pub modal: bool,
    /// The host window can be resized.
    pub resizable: bool,
    /// The host window has application menus.
    pub has_menus: bool,
    pub prefs: WindowPrefs,
}

/// A local device that can be forwarded (Devices menu).
#[derive(Clone, Debug, Default, Serialize)]
pub struct DeviceEntry {
    pub id: String,
    pub name: String,
    pub forwarded: bool,
    pub status: Option<String>,
}

/// Everything the menus show.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Snapshot {
    /// The host's name.
    pub peer: String,
    pub connected: bool,
    pub view: View,
    /// 0 high, 1 medium, 2 low (`crate::quality_mbps`).
    pub quality: u8,
    pub remote_cursor: bool,
    /// The statistics / debug overlay (Control+Option+F10).
    pub stats: bool,
    pub adb: bool,
    pub windows: Vec<WindowEntry>,
    pub gamepads: Vec<DeviceEntry>,
    /// Why gamepads cannot be forwarded, if they cannot.
    pub gamepad_error: Option<String>,
    pub usb: Vec<DeviceEntry>,
    pub usb_error: Option<String>,
    /// The last thing worth telling the user (a fetched file, an error).
    pub notice: Option<String>,
    /// One window with isolated input (games): v1's "Window compatibility"
    /// text. The menus offer it, relative mouse, and keys stay physical.
    pub compatibility: Option<String>,
    /// Why the host lists no windows (a locked Mac), if it said.
    pub empty_reason: Option<String>,
    /// The host's platform ("windows", "macos"), once a stream says.
    pub host_platform: String,
    /// Files being fetched from the host now (as asked for).
    pub transfers: Vec<String>,
    /// Files fetched this session, where they were saved (the received
    /// files shelf).
    pub received: Vec<String>,
}

/// What a window's own options can do.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum WindowAction {
    ResizeRemote,
    StretchLocal,
    ToggleFill,
    ToggleHeader,
    ToggleSwap,
    TogglePhysical,
    RefreshMenus,
    /// Relative mouse on or off (games; also Control+Option+F11).
    ToggleRelative,
    /// Fetch this file (relative to the host user's home folder).
    Fetch {
        path: String,
    },
}

/// What the user chose in a connection menu.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    /// Show or hide (stop streaming) one window.
    Show {
        id: u64,
        shown: bool,
    },
    ShowAll {
        shown: bool,
    },
    View {
        view: View,
    },
    Quality {
        quality: u8,
    },
    RemoteCursor {
        show: bool,
    },
    Stats,
    Adb {
        on: bool,
    },
    Gamepad {
        id: String,
        on: bool,
    },
    Usb {
        id: String,
        on: bool,
    },
    RefreshDevices,
    Window {
        id: u64,
        action: WindowAction,
    },
    Disconnect,
    /// Stop the file fetches in progress ("Cancel transfers").
    CancelTransfers,
}

struct State {
    snapshot: Snapshot,
    /// Show / hide / view commands for the connect loop.
    windows: Vec<Command>,
    /// Gamepad / USB commands for the devices session.
    devices: Vec<Command>,
    /// Per-window actions, by window id.
    actions: Vec<(u64, WindowAction)>,
    prefs_file: Option<PathBuf>,
    /// Saved options by "<host>:<app>".
    prefs: HashMap<String, WindowPrefs>,
    host: String,
    /// The host's platform ("windows", "macos"), once a stream says.
    host_platform: String,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
static VERSION: AtomicU64 = AtomicU64::new(1);
/// The view mode is changing: every view loop ends, then the connect loop
/// opens the other view.
static SWITCHING: AtomicBool = AtomicBool::new(false);
/// A viewer window of this process has focus (gamepads are live only then,
/// as v1); true until a platform window says otherwise.
static FOCUSED: AtomicBool = AtomicBool::new(true);
static WINDOWS_WAKE: tokio::sync::Notify = tokio::sync::Notify::const_new();
static DEVICES_WAKE: tokio::sync::Notify = tokio::sync::Notify::const_new();
/// Ends the file fetches in progress.
static CANCEL_TRANSFERS: tokio::sync::Notify = tokio::sync::Notify::const_new();

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    let mut state = STATE.lock().unwrap();
    let state = state.get_or_insert_with(|| State {
        snapshot: Snapshot::default(),
        windows: Vec::new(),
        devices: Vec::new(),
        actions: Vec::new(),
        prefs_file: None,
        prefs: HashMap::new(),
        host: String::new(),
        host_platform: String::new(),
    });
    f(state)
}

fn changed() {
    VERSION.fetch_add(1, Ordering::Relaxed);
}

/// Starts a viewer's menus: `host` is the host's device id (options are
/// saved per host application in `config_dir`).
pub fn start(
    host: &str,
    peer: &str,
    view: View,
    quality: u8,
    adb: bool,
    config_dir: &std::path::Path,
) {
    let file = config_dir.join("viewer-windows.json");
    let prefs = std::fs::read(&file)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    with(|s| {
        s.host = host.to_string();
        s.prefs_file = Some(file);
        s.prefs = prefs;
        s.snapshot.peer = peer.to_string();
        s.snapshot.view = view;
        s.snapshot.quality = quality.min(2);
        s.snapshot.adb = adb;
        // As v1: the debug overlay from the start.
        s.snapshot.stats = std::env::var("FRAMES_DEBUG_OVERLAY").as_deref() == Ok("1");
    });
    changed();
}

/// The host's platform, from its stream description.
pub fn set_host_platform(platform: &str) {
    with(|s| {
        s.host_platform = platform.to_string();
        s.snapshot.host_platform = platform.to_string();
    });
}

/// A viewer window gained or lost focus (any of this process's windows).
pub fn set_focused(focused: bool) {
    if FOCUSED.swap(focused, Ordering::Relaxed) != focused {
        DEVICES_WAKE.notify_one();
    }
}

pub fn focused() -> bool {
    FOCUSED.load(Ordering::Relaxed)
}

/// Whether the ADB bridge is on.
pub fn adb_on() -> bool {
    with(|s| s.snapshot.adb)
}

/// Changes since the menus were last built (rebuild when it differs).
pub fn version() -> u64 {
    VERSION.load(Ordering::Relaxed)
}

pub fn snapshot() -> Snapshot {
    with(|s| s.snapshot.clone())
}

pub fn snapshot_json() -> String {
    serde_json::to_string(&snapshot()).unwrap_or_default()
}

/// Changes what the menus show.
pub fn update(f: impl FnOnce(&mut Snapshot)) {
    with(|s| f(&mut s.snapshot));
    changed();
}

pub fn notice(text: impl Into<String>) {
    let text = text.into();
    println!("{text}");
    update(|s| s.notice = Some(text));
}

/// A window's saved options (defaults when none).
pub fn prefs(app: &str) -> WindowPrefs {
    with(|s| {
        let key = format!("{}:{app}", s.host);
        s.prefs.get(&key).cloned().unwrap_or_default()
    })
}

fn save_prefs(s: &mut State, app: &str, prefs: &WindowPrefs) {
    let key = format!("{}:{app}", s.host);
    s.prefs.insert(key, prefs.clone());
    if let (Some(file), Ok(bytes)) = (&s.prefs_file, serde_json::to_vec_pretty(&s.prefs)) {
        let _ = std::fs::write(file, bytes);
    }
}

/// The window list of the menus, from the host's catalog (each keeps its
/// options and whether it is shown here).
pub fn set_windows(list: Vec<WindowEntry>) {
    with(|s| {
        let windows = list
            .into_iter()
            .map(|mut w| {
                let key = format!("{}:{}", s.host, w.app);
                w.prefs = s.prefs.get(&key).cloned().unwrap_or_default();
                if let Some(old) = s.snapshot.windows.iter().find(|o| o.id == w.id) {
                    w.has_menus |= old.has_menus;
                }
                w
            })
            .collect();
        s.snapshot.windows = windows;
    });
    changed();
}

/// Marks a listed window shown or hidden here.
pub fn set_shown(id: u64, shown: bool) {
    update(|s| {
        if let Some(w) = s.windows.iter_mut().find(|w| w.id == id) {
            w.shown = shown;
        }
    });
}

/// A listed window's details as the host reports them.
pub fn set_window_details(id: u64, resizable: Option<bool>, has_menus: Option<bool>) {
    update(|s| {
        if let Some(w) = s.windows.iter_mut().find(|w| w.id == id) {
            if let Some(resizable) = resizable {
                w.resizable = resizable;
            }
            if let Some(has_menus) = has_menus {
                w.has_menus = has_menus;
            }
        }
    });
}

/// Applies a command from a menu.
pub fn send(command: Command) {
    match command {
        Command::Quality { quality } => {
            update(|s| s.quality = quality.min(2));
            report_choices();
        }
        Command::RemoteCursor { show } => {
            update(|s| s.remote_cursor = show);
            report_choices();
        }
        Command::Stats => update(|s| s.stats = !s.stats),
        Command::Adb { on } => update(|s| s.adb = on),
        Command::Disconnect => {
            crate::DISCONNECT.store(true, Ordering::Relaxed);
            WINDOWS_WAKE.notify_one();
        }
        Command::CancelTransfers => CANCEL_TRANSFERS.notify_waiters(),
        Command::View { view } => {
            if snapshot().view != view {
                update(|s| s.view = view);
                SWITCHING.store(true, Ordering::Relaxed);
                WINDOWS_WAKE.notify_one();
            }
        }
        Command::Show { .. } | Command::ShowAll { .. } => {
            with(|s| s.windows.push(command));
            WINDOWS_WAKE.notify_one();
        }
        Command::Gamepad { .. } | Command::Usb { .. } | Command::RefreshDevices => {
            with(|s| s.devices.push(command));
            DEVICES_WAKE.notify_one();
        }
        Command::Window { id, action } => {
            // Options are saved here; the window's view loop applies them.
            with(|s| {
                let Some(window) = s.snapshot.windows.iter_mut().find(|w| w.id == id) else {
                    s.actions.push((id, action));
                    return;
                };
                let prefs = &mut window.prefs;
                let saved = match &action {
                    WindowAction::ResizeRemote => {
                        prefs.resize_remote = true;
                        true
                    }
                    WindowAction::StretchLocal => {
                        prefs.resize_remote = false;
                        true
                    }
                    WindowAction::ToggleFill => {
                        prefs.fill = !prefs.fill;
                        true
                    }
                    WindowAction::ToggleHeader => {
                        prefs.header = !prefs.header;
                        true
                    }
                    WindowAction::ToggleSwap => {
                        let auto =
                            !s.host_platform.is_empty() && s.host_platform != crate::platform();
                        prefs.swap = Some(!prefs.swap.unwrap_or(auto));
                        true
                    }
                    WindowAction::TogglePhysical => {
                        prefs.physical = !prefs.physical;
                        true
                    }
                    WindowAction::RefreshMenus
                    | WindowAction::ToggleRelative
                    | WindowAction::Fetch { .. } => false,
                };
                if saved {
                    let (app, prefs) = (window.app.clone(), prefs.clone());
                    // Every open window of the same application follows.
                    for other in s.snapshot.windows.iter_mut().filter(|w| w.app == app) {
                        other.prefs = prefs.clone();
                    }
                    save_prefs(s, &app, &prefs);
                }
                s.actions.push((id, action));
            });
            changed();
        }
    }
}

/// The launcher remembers these per device for the next connection.
fn report_choices() {
    let s = snapshot();
    crate::launcher::report(
        "choices",
        &serde_json::json!({ "quality": s.quality, "remote_cursor": s.remote_cursor }),
    );
}

/// A command as JSON (from Swift); invalid ones are ignored.
pub fn send_json(json: &str) {
    match serde_json::from_str::<Command>(json) {
        Ok(command) => send(command),
        Err(error) => eprintln!("menu command {json}: {error}"),
    }
}

/// Window `id`'s pending actions (its options are already in the snapshot).
pub fn take_window(id: u64) -> Vec<WindowAction> {
    with(|s| {
        let (mine, rest) = s.actions.drain(..).partition(|(w, _)| *w == id);
        s.actions = rest;
        mine.into_iter().map(|(_, a)| a).collect()
    })
}

/// Window `id`'s options now.
pub fn window_prefs(id: u64) -> Option<WindowPrefs> {
    with(|s| {
        s.snapshot
            .windows
            .iter()
            .find(|w| w.id == id)
            .map(|w| w.prefs.clone())
    })
}

/// The connect loop's pending show / hide commands.
pub fn take_window_commands() -> Vec<Command> {
    with(|s| std::mem::take(&mut s.windows))
}

/// Waits for a show / hide / view / disconnect command.
pub async fn windows_changed() {
    WINDOWS_WAKE.notified().await
}

/// The devices session's pending commands.
pub fn take_device_commands() -> Vec<Command> {
    with(|s| std::mem::take(&mut s.devices))
}

pub async fn devices_changed() {
    DEVICES_WAKE.notified().await
}

/// Whether the view mode is changing (view loops end).
pub fn switching() -> bool {
    SWITCHING.load(Ordering::Relaxed)
}

/// The connect loop opened the new view.
pub fn switched() {
    SWITCHING.store(false, Ordering::Relaxed);
}

/// Whether a view loop should end now (disconnect or a view switch).
pub fn ending() -> bool {
    switching() || crate::DISCONNECT.load(Ordering::Relaxed)
}

/// What a window's view loop sends the host as the menus change: the
/// session-wide quality and remote cursor, and the window's header.
pub struct Applied {
    id: u64,
    quality: Option<u8>,
    remote_cursor: Option<bool>,
    header: Option<bool>,
}

impl Applied {
    pub fn new(id: u64) -> Self {
        Self {
            id,
            quality: None,
            remote_cursor: None,
            header: None,
        }
    }

    /// Events for what changed since the last call (everything, the first
    /// time).
    pub fn events(&mut self) -> Vec<frames_input::Event> {
        let snapshot = with(|s| {
            let header = s
                .snapshot
                .windows
                .iter()
                .find(|w| w.id == self.id && !w.desktop)
                .map(|w| w.prefs.header);
            (s.snapshot.quality, s.snapshot.remote_cursor, header)
        });
        let (quality, remote_cursor, header) = snapshot;
        let mut events = Vec::new();
        if self.quality != Some(quality) {
            // The first one is the launcher's choice, already in the hello.
            if self.quality.is_some() {
                events.push(frames_input::Event::MaxBitrate {
                    mbps: crate::quality_mbps(quality),
                });
            }
            self.quality = Some(quality);
        }
        if self.remote_cursor != Some(remote_cursor) {
            if self.remote_cursor.is_some() || remote_cursor {
                events.push(frames_input::Event::RemoteCursor {
                    show: remote_cursor,
                });
            }
            self.remote_cursor = Some(remote_cursor);
        }
        if let Some(header) = header
            && self.header != Some(header)
        {
            if self.header.is_some() || header {
                events.push(frames_input::Event::Header { show: header });
            }
            self.header = Some(header);
        }
        events
    }
}

/// Debounces "Resize remote app": the window's picture size here, sent to
/// the host once it has held for 150 ms.
#[derive(Default)]
pub struct RemoteResize {
    sent: Option<(u32, u32)>,
    pending: Option<((u32, u32), std::time::Instant)>,
}

impl RemoteResize {
    /// The stream's size is `stream`: a resize to it needs no request.
    pub fn streaming(&mut self, stream: (u32, u32)) {
        if self.sent.is_none() {
            self.sent = Some(stream);
        }
    }

    /// The user resized the window to `size` pixels; returns the request
    /// to send once it settles.
    pub fn local(
        &mut self,
        size: (u32, u32),
        now: std::time::Instant,
    ) -> Option<frames_input::Event> {
        if size.0 < 64 || size.1 < 64 {
            return None;
        }
        if self.sent == Some(size) {
            self.pending = None;
            return None;
        }
        match self.pending {
            Some((pending, since)) if pending == size => {
                if now.duration_since(since) >= std::time::Duration::from_millis(150) {
                    self.pending = None;
                    self.sent = Some(size);
                    return Some(frames_input::Event::ResizeWindow {
                        width: size.0.min(u32::from(u16::MAX)) as u16,
                        height: size.1.min(u32::from(u16::MAX)) as u16,
                    });
                }
            }
            _ => self.pending = Some((size, now)),
        }
        None
    }
}

/// What a window's view loop does with the menus each turn
/// ([`WindowLoop::tick`]).
pub struct WindowLoop {
    pub id: u64,
    applied: Applied,
    host_platform: String,
    prefs: Option<WindowPrefs>,
    pub resize: RemoteResize,
    handle: tokio::runtime::Handle,
    receive_dir: PathBuf,
    /// One window with isolated input: keys stay physical (v1).
    shared: bool,
}

/// What changed for the platform window to apply.
#[derive(Default)]
pub struct Changes {
    /// The window's options changed (fill, resize mode, keys).
    pub prefs: Option<WindowPrefs>,
    /// Ctrl/Cmd swapping now (when it changed).
    pub swap: Option<bool>,
    /// Turn relative mouse on or off ("Relative mouse" in the menus).
    pub toggle_relative: bool,
}

impl WindowLoop {
    /// Call from the runtime (it keeps a handle for file fetches).
    pub fn new(o: &crate::Options, info: &crate::StreamInfo) -> Self {
        let host_platform = info.platform.as_str();
        set_host_platform(host_platform);
        if info.compatibility.is_some() {
            update(|s| s.compatibility = info.compatibility.clone());
        }
        Self {
            shared: info.compatibility.is_some(),
            id: o.menu_id,
            applied: Applied::new(o.menu_id),
            host_platform: host_platform.to_string(),
            prefs: None,
            resize: RemoteResize::default(),
            handle: tokio::runtime::Handle::current(),
            receive_dir: o
                .receive_dir
                .clone()
                .unwrap_or_else(crate::transfer::default_receive_dir),
        }
    }

    /// The window's options (defaults before the catalog lists it).
    pub fn prefs(&self) -> WindowPrefs {
        self.prefs.clone().unwrap_or_default()
    }

    /// Host events to send now (`send`), and what the window must change.
    pub fn tick(
        &mut self,
        session: &frames_net::Session,
        mut send: impl FnMut(frames_input::Event),
    ) -> Changes {
        let mut changes = Changes::default();
        for event in self.applied.events() {
            send(event);
        }
        let prefs = window_prefs(self.id).unwrap_or_default();
        if self.prefs.as_ref() != Some(&prefs) {
            let shared = self.shared;
            let swapped = |p: &WindowPrefs| !shared && p.swapped(&self.host_platform);
            let swap = swapped(&prefs);
            if self.prefs.as_ref().map(swapped) != Some(swap) {
                changes.swap = Some(swap);
            }
            self.prefs = Some(prefs.clone());
            changes.prefs = Some(prefs);
        }
        for action in take_window(self.id) {
            match action {
                WindowAction::RefreshMenus => send(frames_input::Event::RefreshMenus),
                WindowAction::ToggleRelative => changes.toggle_relative = true,
                WindowAction::Fetch { path } => {
                    let (session, dir) = (session.clone(), self.receive_dir.clone());
                    self.handle.spawn(async move {
                        notice(format!("Fetching {path}…"));
                        update(|s| s.transfers.push(path.clone()));
                        let result = tokio::select! {
                            result = crate::transfer::fetch(&session, &path, dir) => Some(result),
                            () = CANCEL_TRANSFERS.notified() => None,
                        };
                        update(|s| {
                            if let Some(at) = s.transfers.iter().position(|t| *t == path) {
                                s.transfers.remove(at);
                            }
                        });
                        match result {
                            Some(Ok(saved)) => {
                                notice(format!("Fetched {path} to {}", saved.display()));
                                update(|s| s.received.push(saved.display().to_string()));
                            }
                            Some(Err(error)) => {
                                notice(format!("Fetching {path} failed: {error:#}"))
                            }
                            None => notice(format!("Fetching {path} cancelled")),
                        }
                    });
                }
                _ => {}
            }
        }
        changes
    }

    /// The window is `size` pixels here: in "Resize remote app" the host
    /// window follows once the size settles.
    pub fn local_size(&mut self, size: (u32, u32), mut send: impl FnMut(frames_input::Event)) {
        if self.prefs().resize_remote
            && let Some(event) = self.resize.local(size, std::time::Instant::now())
        {
            send(event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_parse_from_json() {
        assert_eq!(
            serde_json::from_str::<Command>(r#"{"command":"show","id":7,"shown":false}"#).unwrap(),
            Command::Show {
                id: 7,
                shown: false
            }
        );
        assert_eq!(
            serde_json::from_str::<Command>(
                r#"{"command":"window","id":7,"action":{"action":"fetch","path":"Downloads/a.txt"}}"#
            )
            .unwrap(),
            Command::Window {
                id: 7,
                action: WindowAction::Fetch {
                    path: "Downloads/a.txt".into()
                }
            }
        );
        assert_eq!(
            serde_json::from_str::<Command>(r#"{"command":"view","view":"desktop"}"#).unwrap(),
            Command::View {
                view: View::Desktop
            }
        );
    }

    #[test]
    fn swapping_follows_the_platforms_unless_chosen() {
        let mut prefs = WindowPrefs::default();
        let other = if crate::platform() == "macos" {
            "windows"
        } else {
            "macos"
        };
        assert!(prefs.swapped(other));
        assert!(!prefs.swapped(crate::platform()));
        prefs.physical = true;
        assert!(!prefs.swapped(other));
        prefs.physical = false;
        prefs.swap = Some(false);
        assert!(!prefs.swapped(other));
    }

    #[test]
    fn remote_resizes_wait_for_the_size_to_settle() {
        let mut resize = RemoteResize::default();
        let start = std::time::Instant::now();
        resize.streaming((800, 600));
        assert_eq!(resize.local((800, 600), start), None);
        assert_eq!(resize.local((900, 600), start), None);
        let later = start + std::time::Duration::from_millis(200);
        assert_eq!(
            resize.local((900, 600), later),
            Some(frames_input::Event::ResizeWindow {
                width: 900,
                height: 600
            })
        );
        assert_eq!(resize.local((900, 600), later), None);
    }
}
