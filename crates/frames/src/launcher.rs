//! Slint owns the main thread; network discovery, helpers and child supervision
//! run off-thread. Remote application windows still use the native clients.
use anyhow::{Context, Result};
use frames_core::trust::TrustStore;
use serde::{Deserialize, Serialize};
use slint::winit_030::{
    EventResult as WinitEventResult, WinitWindowAccessor, winit::event::WindowEvent,
};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use std::{
    collections::BTreeSet,
    fs,
    net::{IpAddr, SocketAddr, UdpSocket},
    path::PathBuf,
    process::{Child, Command, Stdio},
    rc::Rc,
    sync::mpsc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[path = "window_style.rs"]
mod window_style;

slint::include_modules!();

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Preferences {
    audio: bool,
    desktop: bool,
    recent: Vec<Recent>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Recent {
    address: String,
    opened: u64,
}
impl Preferences {
    fn path() -> Result<PathBuf> {
        Ok(frames_core::trust::config_dir()?.join("launcher.json"))
    }
    fn load() -> Result<Self> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        Ok(serde_json::from_slice(&fs::read(path)?)?)
    }
    fn save(&self) -> Result<()> {
        let path = Self::path()?;
        let temporary = path.with_extension("tmp");
        fs::write(&temporary, serde_json::to_vec_pretty(self)?)?;
        fs::rename(temporary, path)?;
        Ok(())
    }
    fn remember(&mut self, address: SocketAddr) {
        let address = address.to_string();
        self.recent.retain(|item| item.address != address);
        self.recent.insert(
            0,
            Recent {
                address,
                opened: now(),
            },
        );
        self.recent.truncate(20);
    }
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Only numeric IP endpoints are accepted by the transport. Supply the normal
/// Frames port when omitted, including for bare IPv6 addresses.
fn endpoint(value: &str) -> Result<SocketAddr> {
    let value = value.trim();
    if let Ok(address) = value.parse::<SocketAddr>() {
        anyhow::ensure!(address.port() != 0, "Use a port between 1 and 65535.");
        return Ok(address);
    }
    if let Ok(ip) = value.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, 47990));
    }
    anyhow::bail!("Enter a numeric IP address, such as 192.168.1.10:47990 or [::1]:47990.")
}

#[derive(Clone)]
struct Device {
    name: String,
    address: String,
    detail: String,
    fingerprint: String,
    trusted: bool,
}
impl From<Device> for DeviceRow {
    fn from(value: Device) -> Self {
        Self {
            name: value.name.into(),
            address: value.address.into(),
            detail: value.detail.into(),
            fingerprint: value.fingerprint.into(),
            trusted: value.trusted,
        }
    }
}
fn model<T: Clone + 'static>(rows: Vec<T>) -> ModelRc<T> {
    Rc::new(VecModel::from(rows)).into()
}
fn recent_rows(prefs: &Preferences) -> Vec<Device> {
    prefs
        .recent
        .iter()
        .map(|item| {
            let minutes = now().saturating_sub(item.opened) / 60;
            let when = match minutes {
                0 => "just now".into(),
                1..=59 => format!("{minutes} min ago"),
                60..=1439 => format!("{} hr ago", minutes / 60),
                _ => format!("{} days ago", minutes / 1440),
            };
            Device {
                name: item.address.clone(),
                address: item.address.clone(),
                detail: when,
                fingerprint: String::new(),
                trusted: false,
            }
        })
        .collect()
}
fn trusted_rows() -> Result<Vec<Device>> {
    Ok(TrustStore::load()?
        .peers
        .into_iter()
        .map(|(id, peer)| Device {
            name: peer.name,
            address: String::new(),
            detail: if !peer.allow_control {
                "Host only"
            } else if peer.unattended {
                "Unattended"
            } else {
                "Ask to connect"
            }
            .into(),
            fingerprint: id,
            trusted: true,
        })
        .collect())
}

// Keep Slint types on the UI thread; only plain Rust values cross channels.
enum Action {
    Connect(String, bool, bool),
    ToggleSharing(bool),
    Stop(i32),
    Discover,
    Refresh,
    Permission(String),
    Revoke(String),
    ClearHistory,
    LoginItem,
    Shutdown,
}
enum Event {
    Notice(String, bool),
    #[cfg(target_os = "macos")]
    SharingBlocked(String),
    Recent(Vec<Device>),
    Trusted(Vec<Device>),
    Nearby(Result<Vec<Device>, String>),
    Permissions(Result<serde_json::Value, String>),
    Sessions(Vec<(i32, String, String, bool)>),
}
struct Session {
    id: i32,
    name: String,
    detail: String,
    host: bool,
    child: Child,
    log: PathBuf,
}
impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
struct Worker {
    sessions: Vec<Session>,
    next_id: i32,
    preferences: Preferences,
    events: mpsc::Sender<Event>,
    discovering: bool,
    requested_permissions: BTreeSet<String>,
    discovery_done: mpsc::Receiver<()>,
    discovery_tx: mpsc::Sender<()>,
}
impl Worker {
    fn notice(&self, message: impl Into<String>, error: bool) {
        let _ = self.events.send(Event::Notice(message.into(), error));
    }
    fn publish_sessions(&self) {
        let _ = self.events.send(Event::Sessions(
            self.sessions
                .iter()
                .map(|s| (s.id, s.name.clone(), s.detail.clone(), s.host))
                .collect(),
        ));
    }
    fn refresh(&self) {
        match trusted_rows() {
            Ok(rows) => {
                let _ = self.events.send(Event::Trusted(rows));
            }
            Err(error) => self.notice(format!("Couldn't load trusted devices: {error:#}"), true),
        }
        #[cfg(target_os = "macos")]
        {
            let events = self.events.clone();
            std::thread::spawn(move || {
                let result = frames_native::permission_status(false).map_err(|e| format!("{e:#}"));
                let _ = events.send(Event::Permissions(result));
            });
        }
        #[cfg(not(target_os = "macos"))]
        let _ = self.events.send(Event::Permissions(Ok(
            serde_json::json!({"screen_capture":true,"accessibility":true}),
        )));
    }
    fn spawn(&mut self, args: &[String], name: String, host: bool, detail: String) -> Result<()> {
        let id = self.next_id;
        self.next_id += 1;
        let log = frames_core::trust::config_dir()?.join(if host {
            "sharing.log".into()
        } else {
            format!("connection-{id}.log")
        });
        let mut command = Command::new(std::env::current_exe()?);
        command
            .args(args)
            .env_remove("FRAMES_GUI_CHILD")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(fs::File::create(&log)?);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000); // CREATE_NO_WINDOW, native pairing still works.
        }
        let child = command.spawn().context("couldn't start session")?;
        self.sessions.push(Session {
            id,
            name,
            detail,
            host,
            child,
            log,
        });
        self.publish_sessions();
        Ok(())
    }
    fn handle(&mut self, action: Action) -> Result<()> {
        match action {
            Action::Connect(value, audio, desktop) => {
                let address = endpoint(&value)?;
                anyhow::ensure!(
                    !self
                        .sessions
                        .iter()
                        .any(|s| !s.host && s.name == address.to_string()),
                    "A connection to this address is already running. Manage it under Your sessions."
                );
                let mut args = vec!["connect".into(), address.to_string()];
                if audio {
                    args.push("--audio".into());
                }
                if desktop {
                    args.extend(["--view".into(), "desktop".into()]);
                }
                self.spawn(
                    &args,
                    address.to_string(),
                    false,
                    format!(
                        "Starting · {}{}",
                        if desktop {
                            "full desktop"
                        } else {
                            "per-window view"
                        },
                        if audio { " · host audio" } else { "" }
                    ),
                )?;
                self.preferences.audio = audio;
                self.preferences.desktop = desktop;
                self.preferences.remember(address);
                let _ = self
                    .events
                    .send(Event::Recent(recent_rows(&self.preferences)));
                if let Err(error) = self.preferences.save() {
                    self.notice(
                        format!("Session started, but history couldn't be saved: {error:#}"),
                        true,
                    );
                } else {
                    self.notice("Connection started. If prompted, compare the pairing code on both devices.", false);
                }
            }
            Action::ToggleSharing(unattended) => {
                if let Some(index) = self.sessions.iter().position(|s| s.host) {
                    self.sessions.remove(index);
                    self.notice(
                        "Sharing stopped. This computer is no longer accepting connections.",
                        false,
                    );
                } else {
                    #[cfg(target_os = "macos")]
                    {
                        let permissions = frames_native::permission_status(false)?;
                        if permissions["sharing_at_login"].as_bool() == Some(true) {
                            let _ = self.events.send(Event::Permissions(Ok(permissions)));
                            self.notice(
                                "Sharing is managed at login. Use Manage sharing to configure it.",
                                false,
                            );
                            return Ok(());
                        }
                        if let Some(message) = sharing_permission_error(&permissions) {
                            let _ = self.events.send(Event::SharingBlocked(message));
                            self.refresh();
                            return Ok(());
                        }
                    }
                    let mut args = vec!["host".into(), "--pair".into()];
                    if unattended {
                        args.push("--unattended".into());
                    }
                    self.spawn(
                        &args,
                        "This device".into(),
                        true,
                        if unattended {
                            "Unattended"
                        } else {
                            "Ask to connect"
                        }
                        .into(),
                    )?;
                    self.notice("Sharing started. New devices must be approved with a matching pairing code.", false);
                }
                self.publish_sessions();
            }
            Action::Stop(id) => {
                if let Some(index) = self.sessions.iter().position(|s| s.id == id) {
                    self.sessions.remove(index);
                    self.publish_sessions();
                    self.notice("Session stopped. The remote applications stay open.", false);
                }
            }
            Action::Discover => {
                if self.discovering {
                    return Ok(());
                }
                self.discovering = true;
                let events = self.events.clone();
                let done = self.discovery_tx.clone();
                std::thread::spawn(move || {
                    let result = (|| -> Result<Vec<Device>> {
                        let runtime = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()?;
                        let services = runtime.block_on(frames_core::discovery::browse(3))?;
                        let mut devices = Vec::new();
                        for service in services {
                            let name = service["name"]
                                .as_str()
                                .unwrap_or("Frames device")
                                .trim_end_matches("._frames._udp.local.")
                                .to_owned();
                            let Some(port) =
                                service["port"].as_u64().and_then(|p| u16::try_from(p).ok())
                            else {
                                continue;
                            };
                            if let Some(addresses) = service["addresses"].as_array() {
                                for value in addresses {
                                    if let Some(ip) =
                                        value.as_str().and_then(|s| s.parse::<IpAddr>().ok())
                                    {
                                        devices.push(Device {
                                            name: name.clone(),
                                            address: SocketAddr::new(ip, port).to_string(),
                                            detail: "Nearby · verify pairing before connecting"
                                                .into(),
                                            fingerprint: service["device"]
                                                .as_str()
                                                .unwrap_or_default()
                                                .into(),
                                            trusted: false,
                                        });
                                    }
                                }
                            }
                        }
                        devices.sort_by(|a, b| a.name.cmp(&b.name).then(a.address.cmp(&b.address)));
                        devices.dedup_by(|a, b| a.address == b.address);
                        Ok(devices)
                    })()
                    .map_err(|e| format!("{e:#}"));
                    let _ = events.send(Event::Nearby(result));
                    let _ = done.send(());
                });
            }
            Action::Refresh => self.refresh(),
            Action::Permission(kind) => {
                // Prompt once per permission in this launch; later clicks open Settings.
                let request = self.requested_permissions.insert(kind.clone());
                let events = self.events.clone();
                std::thread::spawn(move || {
                    let result = (|| -> Result<()> {
                        #[cfg(target_os = "macos")]
                        if request {
                            frames_native::request_permission(&kind)?;
                        }
                        #[cfg(not(target_os = "macos"))]
                        let _ = request;
                        open_permission(&kind)
                    })();
                    if let Err(error) = result {
                        let _ = events.send(Event::Notice(format!("{error:#}"), true));
                    }
                });
            }
            Action::Revoke(id) => {
                anyhow::ensure!(
                    TrustStore::load()?.revoke(&id)?,
                    "That device is no longer in the trust store."
                );
                self.refresh();
                self.notice(
                    "Device access revoked. Disconnect any running session separately.",
                    false,
                );
            }
            Action::ClearHistory => {
                let mut preferences = self.preferences.clone();
                preferences.recent.clear();
                preferences.save()?;
                self.preferences = preferences;
                let _ = self.events.send(Event::Recent(Vec::new()));
                self.notice("Connection history cleared from this computer.", false);
            }
            Action::LoginItem => {
                #[cfg(target_os = "macos")]
                {
                    let events = self.events.clone();
                    std::thread::spawn(move || {
                        if let Err(error) = frames_native::configure_login_item() {
                            let _ = events.send(Event::Notice(format!("{error:#}"), true));
                        }
                        let result =
                            frames_native::permission_status(false).map_err(|e| format!("{e:#}"));
                        let _ = events.send(Event::Permissions(result));
                    });
                }
            }
            Action::Shutdown => unreachable!(),
        }
        Ok(())
    }
    fn poll(&mut self) {
        if self.discovery_done.try_recv().is_ok() {
            self.discovering = false;
        }
        let mut changed = false;
        let mut index = 0;
        while index < self.sessions.len() {
            match self.sessions[index].child.try_wait() {
                Ok(Some(status)) => {
                    let session = self.sessions.remove(index);
                    changed = true;
                    if status.success() {
                        self.notice(format!("{} ended.", session.name), false);
                    } else {
                        let log = fs::read_to_string(&session.log).unwrap_or_default();
                        let last_error = log.lines().rev().find(|line| line.starts_with("Frames:"))
                            .unwrap_or("Session exited. Check the address, network access, and permission settings.");
                        self.notice(
                            format!(
                                "{}: {}",
                                session.name,
                                last_error.chars().take(500).collect::<String>()
                            ),
                            true,
                        );
                    }
                }
                Ok(None) => index += 1,
                Err(error) => {
                    self.notice(format!("Couldn't inspect session: {error}"), true);
                    index += 1;
                }
            }
        }
        if changed {
            self.publish_sessions();
            self.refresh();
        }
    }
}

fn sharing_permission_error(value: &serde_json::Value) -> Option<String> {
    let mut missing = Vec::new();
    if value["screen_capture"].as_bool() != Some(true) {
        missing.push("Screen Recording");
    }
    if value["accessibility"].as_bool() != Some(true) {
        missing.push(
            value["control_permission_name"]
                .as_str()
                .unwrap_or("Accessibility"),
        );
    }
    if missing.is_empty() {
        None
    } else {
        Some(format!(
            "Sharing needs {}. Open Permissions to set it up.",
            missing.join(" and ")
        ))
    }
}

fn open_permission(kind: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    if kind == "network" {
        return frames_native::open_network_settings();
    }
    #[cfg(target_os = "macos")]
    let mut command = {
        let pane = match kind {
            "screen" => "Privacy_ScreenCapture",
            "control" => "Privacy_Accessibility",
            _ => anyhow::bail!("Unknown permission settings: {kind}"),
        };
        let mut command = Command::new("/usr/bin/open");
        command.arg(format!(
            "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?{pane}"
        ));
        command
    };
    #[cfg(windows)]
    let mut command = {
        let uri = match kind {
            "screen" => "ms-settings:display",
            "control" => "ms-settings:easeofaccess-mouse",
            _ => "windowsdefender://network",
        };
        let mut command = Command::new("explorer.exe");
        command.arg(uri);
        command
    };
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = kind;
        anyhow::bail!("System settings are supported on Windows and macOS.");
    }
    #[cfg(any(windows, target_os = "macos"))]
    {
        anyhow::ensure!(
            command.status()?.success(),
            "Couldn't open system settings."
        );
        Ok(())
    }
}
fn local_address() -> String {
    // UDP connect selects a route without sending traffic or contacting a server.
    UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("192.0.2.1:47990")?;
            s.local_addr()
        })
        .map(|a| format!("{}:47990", a.ip()))
        .unwrap_or_else(|_| "Port 47990".into())
}

pub fn run() -> Result<()> {
    window_style::initialize()?;
    let ui = Launcher::new()?;
    ui.on_filter_devices(|rows, query| {
        let query = query.trim().to_lowercase();
        model(
            rows.iter()
                .filter(|row| {
                    format!("{} {} {}", row.name, row.address, row.fingerprint)
                        .to_lowercase()
                        .contains(&query)
                })
                .collect(),
        )
    });
    ui.set_mac(cfg!(target_os = "macos"));
    ui.set_device_name(
        std::env::var("COMPUTERNAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_else(|_| {
                if cfg!(target_os = "macos") {
                    "This Mac"
                } else {
                    "This PC"
                }
                .into()
            })
            .into(),
    );
    ui.set_local_address(local_address().into());
    let preferences = match Preferences::load() {
        Ok(prefs) => prefs,
        Err(error) => {
            ui.set_notice(format!("Couldn't load local preferences: {error:#}").into());
            ui.set_notice_error(true);
            Preferences::default()
        }
    };
    ui.set_audio(preferences.audio);
    ui.set_desktop(preferences.desktop);
    ui.set_recent(model(
        recent_rows(&preferences)
            .into_iter()
            .map(Into::into)
            .collect(),
    ));
    let (actions, action_rx) = mpsc::channel();
    let (events, event_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let (discovery_tx, discovery_done) = mpsc::channel();
        let mut worker = Worker {
            sessions: Vec::new(),
            next_id: 1,
            preferences,
            events,
            discovering: false,
            requested_permissions: BTreeSet::new(),
            discovery_tx,
            discovery_done,
        };
        worker.refresh();
        loop {
            match action_rx.recv_timeout(Duration::from_millis(250)) {
                Ok(Action::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Ok(action) => {
                    if let Err(error) = worker.handle(action) {
                        worker.notice(format!("{error:#}"), true);
                    }
                }
            }
            worker.poll();
        }
        // Dropping supervised sessions closes their pipes and native helpers.
    });
    let focus_actions = actions.clone();
    ui.window().on_winit_window_event(move |_, event| {
        if matches!(event, WindowEvent::Focused(true)) {
            let _ = focus_actions.send(Action::Refresh);
        }
        WinitEventResult::Propagate
    });
    let weak = ui.as_weak();
    let tx = actions.clone();
    ui.on_connect(move |address| {
        if let Some(ui) = weak.upgrade() {
            let _ = tx.send(Action::Connect(
                address.to_string(),
                ui.get_audio(),
                ui.get_desktop(),
            ));
        }
    });
    let weak = ui.as_weak();
    let tx = actions.clone();
    ui.on_toggle_sharing(move || {
        if let Some(ui) = weak.upgrade() {
            let _ = tx.send(Action::ToggleSharing(ui.get_unattended()));
        }
    });
    let weak = ui.as_weak();
    let tx = actions.clone();
    ui.on_discover(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_discovering(true);
            let _ = tx.send(Action::Discover);
        }
    });
    let tx = actions.clone();
    ui.on_stop_session(move |id| {
        let _ = tx.send(Action::Stop(id));
    });
    let tx = actions.clone();
    ui.on_refresh(move || {
        let _ = tx.send(Action::Refresh);
    });
    let tx = actions.clone();
    ui.on_permission(move |kind| {
        let _ = tx.send(Action::Permission(kind.to_string()));
    });
    let tx = actions.clone();
    ui.on_revoke(move |id| {
        let _ = tx.send(Action::Revoke(id.to_string()));
    });
    let tx = actions.clone();
    ui.on_clear_history(move || {
        let _ = tx.send(Action::ClearHistory);
    });
    let tx = actions.clone();
    ui.on_login_settings(move || {
        let _ = tx.send(Action::LoginItem);
    });
    let weak = ui.as_weak();
    let timer = slint::Timer::default();
    let mut permission_notice = false;
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(100), move || {
        let Some(ui) = weak.upgrade() else { return; };
        for event in event_rx.try_iter() {
            match event {
                Event::Notice(message, error) => {
                    permission_notice = false;
                    ui.set_notice(message.into()); ui.set_notice_error(error);
                    ui.set_status(if error { "Action needed — see the message above" } else { "Ready · direct encrypted connections" }.into());
                    ui.set_status_error(error);
                }
                #[cfg(target_os = "macos")]
                Event::SharingBlocked(message) => {
                    permission_notice = true;
                    ui.set_notice(message.into());
                    ui.set_notice_error(true);
                }
                Event::Recent(rows) => ui.set_recent(model(rows.into_iter().map(Into::into).collect())),
                Event::Trusted(rows) => ui.set_trusted(model(rows.into_iter().map(Into::into).collect())),
                Event::Nearby(result) => {
                    ui.set_discovering(false);
                    match result {
                        Ok(rows) => {
                            if rows.is_empty() { ui.set_notice("No nearby hosts found. Start sharing on the other device, or enter its IP address.".into()); ui.set_notice_error(false); }
                            ui.set_nearby(model(rows.into_iter().map(Into::into).collect()));
                        }
                        Err(error) => { ui.set_notice(format!("Network discovery failed: {error}").into()); ui.set_notice_error(true); }
                    }
                }
                Event::Permissions(result) => match result {
                    Ok(value) => {
                        ui.set_screen_permission(value["screen_capture"].as_bool().map_or(-1, i32::from));
                        ui.set_control_permission(value["accessibility"].as_bool().map_or(-1, i32::from));
                        ui.set_background_sharing(value["sharing_at_login"].as_bool().unwrap_or(false));
                        if let Some(label) = value["control_permission_name"].as_str() { ui.set_control_label(label.into()); }
                        if permission_notice {
                            if ui.get_notice().is_empty() {
                                permission_notice = false;
                            } else if let Some(message) = sharing_permission_error(&value) {
                                ui.set_notice(message.into());
                            } else {
                                permission_notice = false;
                                ui.set_notice("".into());
                                ui.set_notice_error(false);
                                ui.set_status_error(false);
                            }
                        }

                    }
                    Err(error) => {
                        ui.set_screen_permission(-1); ui.set_control_permission(-1);
                        ui.set_notice(format!("Couldn't check macOS permissions: {error}").into()); ui.set_notice_error(true);
                    }
                },
                Event::Sessions(rows) => {
                    if rows.iter().any(|row| !row.3) { ui.set_connection_tab(1); }
                    ui.set_sharing(rows.iter().any(|s| s.3));
                    ui.set_sessions(model(rows.into_iter().map(|(id, name, detail, hosting)| SessionRow { id, name: name.into(), detail: detail.into(), hosting }).collect()));
                }
            }
        }
    });
    let result = ui.run();
    let _ = actions.send(Action::Shutdown);
    let _ = worker.join();
    result?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sharing_errors_only_name_permissions_still_missing() {
        let granted_screen = serde_json::json!({"screen_capture":true,"accessibility":false,"control_permission_name":"Device Control and Data Access"});
        let message = sharing_permission_error(&granted_screen).unwrap();
        assert!(!message.contains("Screen Recording"));
        assert!(message.contains("Device Control and Data Access"));
        assert!(
            sharing_permission_error(
                &serde_json::json!({"screen_capture":true,"accessibility":true})
            )
            .is_none()
        );
        assert!(
            sharing_permission_error(
                &serde_json::json!({"screen_capture":false,"accessibility":true})
            )
            .unwrap()
            .contains("Screen Recording")
        );
    }

    #[test]
    fn endpoints_supply_the_default_port_without_accepting_hostnames() {
        assert_eq!(
            endpoint(" 192.168.1.10 ").unwrap().to_string(),
            "192.168.1.10:47990"
        );
        assert_eq!(endpoint("::1").unwrap().to_string(), "[::1]:47990");
        assert_eq!(endpoint("[::1]:48000").unwrap().port(), 48000);
        assert!(endpoint("example.com:47990").is_err());
        assert!(endpoint("127.0.0.1:0").is_err());
        assert!(endpoint("127.0.0.1:99999").is_err());
    }
    #[test]
    fn history_deduplicates_and_caps_recent_addresses() {
        let mut prefs = Preferences::default();
        for port in 1..=25 {
            prefs.remember(SocketAddr::from(([127, 0, 0, 1], port)));
        }
        assert_eq!(prefs.recent.len(), 20);
        prefs.remember(SocketAddr::from(([127, 0, 0, 1], 20)));
        assert_eq!(prefs.recent.len(), 20);
        assert_eq!(prefs.recent[0].address, "127.0.0.1:20");
        assert_eq!(
            prefs
                .recent
                .iter()
                .filter(|r| r.address == "127.0.0.1:20")
                .count(),
            1
        );
    }
}
