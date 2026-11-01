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

#[path = "device_catalog.rs"]
mod device_catalog;
#[path = "launcher_close.rs"]
mod launcher_close;
#[cfg(windows)]
#[path = "launcher_tray.rs"]
mod launcher_tray;
#[path = "sharing_picker.rs"]
mod sharing_picker;
#[path = "window_style.rs"]
mod window_style;

slint::include_modules!();

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Preferences {
    #[serde(flatten)]
    options: SessionOptions,
    recent: Vec<Recent>,
    close_behavior: CloseBehavior,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CloseBehavior {
    #[default]
    Ask,
    Hide,
    Quit,
}
impl CloseBehavior {
    fn index(self) -> i32 {
        match self {
            Self::Ask => 0,
            Self::Hide => 1,
            Self::Quit => 2,
        }
    }
    fn from_index(index: i32) -> Option<Self> {
        match index {
            0 => Some(Self::Ask),
            1 => Some(Self::Hide),
            2 => Some(Self::Quit),
            _ => None,
        }
    }
}
/// Saved defaults are also used to build the CLI arguments for supervised sessions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct SessionOptions {
    audio: bool,
    desktop: bool,
    clipboard: bool,
    unattended: bool,
    codec: String,
    bandwidth_mbps: u32,
    quality: frames_core::adaptive::QualityPreset,
    max_streams: usize,
}
const CODECS: [&str; 5] = ["auto", "h264", "hevc", "av1", "vp9"];
impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            audio: false,
            desktop: false,
            clipboard: true,
            unattended: false,
            codec: "auto".into(),
            bandwidth_mbps: 0,
            quality: Default::default(),
            max_streams: 8,
        }
    }
}
impl SessionOptions {
    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.bandwidth_mbps <= 200,
            "Choose automatic bandwidth or a limit between 1 and 200 Mbps."
        );
        anyhow::ensure!(
            CODECS.contains(&self.codec.as_str()),
            "Choose a supported video codec."
        );
        anyhow::ensure!(
            (1..=32).contains(&self.max_streams),
            "Choose a shared window limit between 1 and 32."
        );
        Ok(())
    }
    fn connection_args(&self, address: SocketAddr) -> Vec<String> {
        let mut args = vec![
            "connect".into(),
            address.to_string(),
            "--codec".into(),
            self.codec.clone(),
        ];
        args.extend(["--quality".into(), self.quality.name().into()]);
        if self.bandwidth_mbps != 0 {
            args.extend(["--bandwidth-mbps".into(), self.bandwidth_mbps.to_string()]);
        }
        if self.audio {
            args.push("--audio".into());
        }
        if self.desktop {
            args.extend(["--view".into(), "desktop".into()]);
        }
        if !self.clipboard {
            args.push("--no-clipboard".into());
        }
        args
    }
    fn sharing_args(&self) -> Vec<String> {
        let mut args = vec![
            "host".into(),
            "--pair".into(),
            "--max-streams".into(),
            self.max_streams.to_string(),
        ];
        if self.unattended {
            args.push("--unattended".into());
        }
        if !self.clipboard {
            args.push("--no-clipboard".into());
        }
        args
    }
    fn from_ui(ui: &Launcher) -> Self {
        Self {
            quality: frames_core::adaptive::QualityPreset::from_index(ui.get_quality())
                .unwrap_or_default(),
            audio: ui.get_default_audio(),
            desktop: ui.get_default_desktop(),
            clipboard: ui.get_clipboard(),
            unattended: ui.get_unattended(),
            bandwidth_mbps: if ui.get_bandwidth_auto() {
                0
            } else {
                ui.get_bandwidth_mbps().max(1) as u32
            },
            codec: CODECS
                .get(ui.get_codec() as usize)
                .unwrap_or(&"auto")
                .to_string(),
            max_streams: ui.get_max_streams() as usize,
        }
    }
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
        let preferences: Self = serde_json::from_slice(&fs::read(path)?)?;
        preferences.options.validate()?;
        Ok(preferences)
    }
    fn save(&self) -> Result<()> {
        let path = Self::path()?;
        self.save_to(&path)
    }
    fn save_to(&self, path: &std::path::Path) -> Result<()> {
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

fn connection_device_id(address: SocketAddr, expected_id: &str) -> String {
    if !expected_id.is_empty() {
        return expected_id.into();
    }
    crate::connection_history::load()
        .unwrap_or_default()
        .into_iter()
        .find(|c| c.address == address.to_string())
        .map(|c| c.device_id)
        .unwrap_or_default()
}

#[derive(Clone, Default)]
struct Device {
    name: String,
    address: String,
    detail: String,
    fingerprint: String,
    trusted: bool,
    online: bool,
    can_trust: bool,
    preview_live: bool,
    preview: Vec<u8>,
    connection_audio: bool,
    connection_desktop: bool,
}
impl From<Device> for DeviceRow {
    fn from(value: Device) -> Self {
        Self {
            name: value.name.into(),
            address: value.address.into(),
            detail: value.detail.into(),
            fingerprint: value.fingerprint.into(),
            trusted: value.trusted,
            online: value.online,
            can_trust: value.can_trust,
            preview_live: value.preview_live,
            preview: device_catalog::image(&value.preview),
            connection_audio: value.connection_audio,
            connection_desktop: value.connection_desktop,
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
                name: "Remote device".into(),
                address: item.address.clone(),
                detail: when,
                fingerprint: String::new(),
                trusted: false,
                ..Default::default()
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
            ..Default::default()
        })
        .collect())
}

// Keep Slint types on the UI thread; only plain Rust values cross channels.
enum Action {
    LoadShareWindows(u64),
    CancelWindowPicker,
    ShareWindows(i32, Vec<String>),
    Connect(String, String, bool, bool),
    SaveConnectionChoices(String, String, bool, bool),
    RestartUpdate,
    ToggleSharing,
    SaveSettings(SessionOptions),
    SaveDeviceName(String),
    SaveCloseBehavior(CloseBehavior, bool),
    Stop(i32),
    Discover,
    Refresh,
    Permission(String),
    Revoke(String),
    Trust(String),
    ClearHistory,
    LoginItem,
    Shutdown,
}
enum Event {
    ShareWindows(u64, Result<Vec<frames_native::SharingWindow>, String>),
    ShareWindowPreview(u64, String, Option<Vec<u8>>),
    Notice(String, bool),
    SettingsSaved(SessionOptions, Result<(), String>),
    DeviceNameSaved(String, Result<(), String>),
    CloseBehaviorSaved(CloseBehavior, bool, Result<(), String>),
    Update(crate::update::Status),
    UpdateRestart(Result<(), String>),
    #[cfg(target_os = "macos")]
    SharingBlocked(String),
    Recent(Vec<Device>),
    Trusted(Vec<Device>),
    Nearby(Result<Vec<Device>, String>),
    Permissions(Result<serde_json::Value, String>),
    Sessions(Vec<(i32, String, String, bool)>),
}

fn bind_settings_autosave(ui: &Launcher, actions: mpsc::Sender<Action>) {
    let weak = ui.as_weak();
    ui.on_settings_edited(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_settings_status("".into());
            ui.set_settings_error(false);
            if actions
                .send(Action::SaveSettings(SessionOptions::from_ui(&ui)))
                .is_err()
            {
                ui.set_settings_error(true);
                ui.set_settings_status(
                    "Couldn't save settings: settings worker unavailable.".into(),
                );
            }
        }
    });
}

fn bind_update_restart(ui: &Launcher, actions: mpsc::Sender<Action>) {
    let weak = ui.as_weak();
    ui.on_restart_update(move || {
        if let Some(ui) = weak.upgrade()
            && ui.get_update_ready()
            && !ui.get_update_restart_busy()
            && !ui.get_update_busy()
        {
            ui.set_update_restart_error("".into());
            ui.set_update_prompt_open(true);
        }
    });
    let weak = ui.as_weak();
    ui.on_confirm_restart_update(move || {
        if let Some(ui) = weak.upgrade()
            && ui.get_update_ready()
            && ui.get_update_prompt_open()
            && !ui.get_update_restart_busy()
            && !ui.get_update_busy()
        {
            ui.set_update_restart_busy(true);
            if actions.send(Action::RestartUpdate).is_err() {
                ui.set_update_restart_busy(false);
                ui.set_update_restart_error("Couldn't restart: update worker unavailable.".into());
            }
        }
    });
}

fn apply_settings_result(ui: &Launcher, options: &SessionOptions, result: Result<(), String>) {
    // Edits remain available while saving; older results must not replace current feedback.
    if *options != SessionOptions::from_ui(ui) {
        return;
    }
    match result {
        Ok(()) => {
            ui.set_audio(options.audio);
            ui.set_desktop(options.desktop);
            ui.set_settings_error(false);
            ui.set_settings_status("".into());
        }
        Err(error) => {
            ui.set_settings_error(true);
            ui.set_settings_status(format!("Couldn't save settings: {error}").into());
        }
    }
}

struct Session {
    id: i32,
    name: String,
    address: String,
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
    sharing_choices: Vec<frames_native::SharingWindow>,
    sharing_generation: std::sync::Arc<std::sync::atomic::AtomicU64>,
    sessions: Vec<Session>,
    next_id: i32,
    preferences: Preferences,
    events: mpsc::Sender<Event>,
    discovering: bool,
    devices: Vec<Device>,
    last_discovery: std::time::Instant,
    last_history_refresh: std::time::Instant,
    requested_permissions: BTreeSet<String>,
    discovery_done: mpsc::Receiver<Result<Vec<Device>, String>>,
    discovery_tx: mpsc::Sender<Result<Vec<Device>, String>>,
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
    fn publish_devices(&self) {
        if let Ok(nearby) = device_catalog::nearby(&self.devices, &self.preferences) {
            let _ = self.events.send(Event::Nearby(Ok(nearby)));
        }
        match device_catalog::rows(&self.preferences, &self.devices) {
            Ok((recent, trusted)) => {
                let _ = self.events.send(Event::Recent(recent));
                let _ = self.events.send(Event::Trusted(trusted));
            }
            Err(error) => self.notice(format!("Couldn't load devices: {error:#}"), true),
        }
    }
    fn refresh(&self) {
        self.publish_devices();
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
    fn spawn(
        &mut self,
        args: &[String],
        name: String,
        host: bool,
        detail: String,
        expected_id: &str,
    ) -> Result<()> {
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
            .env("FRAMES_LAUNCHER_PID", std::process::id().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(fs::File::create(&log)?);
        command.env_remove("FRAMES_EXPECTED_DEVICE_ID");
        if !expected_id.is_empty() {
            command.env("FRAMES_EXPECTED_DEVICE_ID", expected_id);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000); // CREATE_NO_WINDOW, native pairing still works.
        }
        let child = command.spawn().context("couldn't start session")?;
        self.sessions.push(Session {
            id,
            name,
            address: if host {
                String::new()
            } else {
                args.get(1).cloned().unwrap_or_default()
            },
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
            Action::RestartUpdate => {
                let result = crate::update::request_restart().map_err(|error| format!("{error:#}"));
                if result.is_ok() {
                    self.sessions.clear();
                    self.publish_sessions();
                }
                let _ = self.events.send(Event::UpdateRestart(result));
            }
            Action::LoadShareWindows(request) => sharing_picker::load(self, request),
            Action::CancelWindowPicker => {
                self.sharing_generation
                    .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                if !self.sessions.iter().any(|s| s.host) {
                    self.sharing_choices.clear();
                }
            }
            Action::ShareWindows(mode, windows) => {
                anyhow::ensure!(
                    !self.sessions.iter().any(|s| s.host),
                    "Stop the current sharing session first"
                );
                anyhow::ensure!(
                    (mode == 1 && !windows.is_empty() && windows.len() <= 32)
                        || (mode == 2 && windows.len() == 1),
                    "Choose the windows to share"
                );
                anyhow::ensure!(
                    windows
                        .iter()
                        .all(|argument| self.sharing_choices.iter().any(|choice| choice
                            .selection
                            .as_ref()
                            .is_some_and(|selection| selection.argument() == *argument))),
                    "Refresh the window selection before sharing"
                );
                let mut args = self.preferences.options.sharing_args();
                if mode == 2 {
                    args.push("--one-window".into());
                }
                for argument in windows {
                    args.extend(["--share-window".into(), argument]);
                }
                self.spawn(
                    &args,
                    "This device".into(),
                    true,
                    if mode == 2 {
                        "One window"
                    } else {
                        "Selected windows"
                    }
                    .into(),
                    "",
                )?;
                self.notice("Sharing started with the windows you selected.", false);
            }
            Action::Connect(value, expected_id, audio, desktop) => {
                let address = endpoint(&value)?;
                anyhow::ensure!(
                    !self
                        .sessions
                        .iter()
                        .any(|s| !s.host && s.address == address.to_string()),
                    "A connection to this address is already running. Manage it under Your sessions."
                );
                let mut options = self.preferences.options.clone();
                options.audio = audio;
                options.desktop = desktop;
                let device_id = connection_device_id(address, &expected_id);
                if let Some(saved) = crate::connection_preferences::load()?.get(&device_id) {
                    options.quality = saved.quality;
                }
                let args = options.connection_args(address);
                let name = self
                    .devices
                    .iter()
                    .find(|d| d.address == address.to_string())
                    .map(|d| d.name.clone())
                    .or_else(|| {
                        crate::connection_history::load()
                            .ok()?
                            .into_iter()
                            .find(|c| c.address == address.to_string())
                            .map(|c| c.name)
                    })
                    .unwrap_or_else(|| "Remote device".into());
                self.spawn(
                    &args,
                    name,
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
                    &expected_id,
                )?;
                self.preferences.remember(address);
                self.preferences.options.audio = audio;
                self.preferences.options.desktop = desktop;
                self.publish_devices();
                if let Err(error) = self.preferences.save() {
                    self.notice(
                        format!("Session started, but preferences couldn't be saved: {error:#}"),
                        true,
                    );
                } else {
                    self.notice("Connection started. If prompted, compare the pairing code on both devices.", false);
                }
            }
            Action::SaveConnectionChoices(value, expected_id, audio, desktop) => {
                let Ok(address) = endpoint(&value) else {
                    return Ok(());
                };
                let id = connection_device_id(address, &expected_id);
                if !id.is_empty() {
                    let saved = crate::connection_preferences::load()?;
                    crate::connection_preferences::update(&id, |p| {
                        if !saved.contains_key(&id) {
                            p.quality = self.preferences.options.quality;
                        }
                        p.audio = audio;
                        p.desktop = desktop;
                    })?;
                    self.publish_devices();
                }
            }
            Action::SaveSettings(options) => {
                let result = (|| -> Result<()> {
                    options.validate()?;
                    let mut preferences = self.preferences.clone();
                    preferences.options = options.clone();
                    preferences.save()?;
                    self.preferences = preferences;
                    Ok(())
                })()
                .map_err(|error| format!("{error:#}"));
                let _ = self.events.send(Event::SettingsSaved(options, result));
            }
            Action::SaveDeviceName(name) => {
                let result =
                    frames_native::set_device_name(&name).map_err(|error| format!("{error:#}"));
                let _ = self.events.send(Event::DeviceNameSaved(name, result));
            }
            Action::SaveCloseBehavior(behavior, close_after) => {
                let result = (|| -> Result<()> {
                    let mut preferences = self.preferences.clone();
                    preferences.close_behavior = behavior;
                    preferences.save()?;
                    self.preferences = preferences;
                    Ok(())
                })()
                .map_err(|error| format!("{error:#}"));
                let _ = self
                    .events
                    .send(Event::CloseBehaviorSaved(behavior, close_after, result));
            }
            Action::ToggleSharing => {
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
                        if permissions["sharing_running"].as_bool() == Some(true) {
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
                    let args = self.preferences.options.sharing_args();
                    self.spawn(
                        &args,
                        "This device".into(),
                        true,
                        if self.preferences.options.unattended {
                            "Unattended"
                        } else {
                            "Ask to connect"
                        }
                        .into(),
                        "",
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
                self.last_discovery = std::time::Instant::now();
                let done = self.discovery_tx.clone();
                std::thread::spawn(move || {
                    let _ = done.send(device_catalog::scan().map_err(|e| format!("{e:#}")));
                });
            }
            Action::Refresh => self.refresh(),
            Action::Permission(kind) => {
                // Prompt once per permission in this launch; later clicks open Settings.
                let request = self.requested_permissions.insert(kind.clone());
                let events = self.events.clone();
                std::thread::spawn(move || {
                    let result = permission_action(&kind, request);
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
                if let Ok(path) = crate::connection_history::preview_path(&id, false) {
                    let _ = fs::remove_file(path);
                }
                self.notice(
                    "Device access revoked. Disconnect any running session separately.",
                    false,
                );
            }
            Action::Trust(id) => {
                let connection = crate::connection_history::load()?
                    .into_iter()
                    .find(|c| c.device_id == id)
                    .context("Connect and verify this device before saving trust.")?;
                TrustStore::load()?.approve(
                    id,
                    connection.name,
                    self.preferences.options.unattended,
                    connection.incoming,
                )?;
                self.refresh();
                self.notice("Device saved as trusted.", false);
            }
            Action::ClearHistory => {
                crate::connection_history::clear()?;
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
        if let Ok(result) = self.discovery_done.try_recv() {
            self.discovering = false;
            match result {
                Ok(devices) => {
                    self.devices = devices;
                    self.publish_devices();
                }
                Err(error) => {
                    let _ = self.events.send(Event::Nearby(Err(error)));
                }
            }
        }
        if !self.discovering && self.last_discovery.elapsed() >= Duration::from_secs(12) {
            let _ = self.handle(Action::Discover);
        }
        let mut changed = false;
        if self.last_history_refresh.elapsed() >= Duration::from_secs(2) {
            self.last_history_refresh = std::time::Instant::now();
            if let Ok(history) = crate::connection_history::load() {
                for session in &mut self.sessions {
                    if !session.host
                        && let Some(peer) = history.iter().find(|p| p.address == session.address)
                        && session.name != peer.name
                    {
                        session.name = peer.name.clone();
                        changed = true;
                    }
                }
            }
            self.publish_devices();
        }
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

fn permission_action(kind: &str, request: bool) -> Result<()> {
    #[cfg(target_os = "macos")]
    if request {
        frames_native::request_permission(kind)?;
    }
    #[cfg(not(target_os = "macos"))]
    let _ = request;
    open_permission(kind)
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
    #[cfg(windows)]
    let mut tray = launcher_tray::LauncherTray::new()?;
    ui.set_app_version(env!("CARGO_PKG_VERSION").into());
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
    ui.set_device_name(frames_native::device_name().into());
    ui.set_device_name_edit(
        frames_native::device_name_override()?
            .unwrap_or_default()
            .into(),
    );
    ui.set_system_device_name(frames_native::system_device_name().into());
    ui.set_local_address(local_address().into());
    let preferences = match Preferences::load() {
        Ok(prefs) => prefs,
        Err(error) => {
            ui.set_notice(format!("Couldn't load local preferences: {error:#}").into());
            ui.set_notice_error(true);
            Preferences::default()
        }
    };
    ui.set_audio(preferences.options.audio);
    ui.set_desktop(preferences.options.desktop);
    if let Some(recent) = preferences.recent.first() {
        ui.set_address(recent.address.clone().into());
    }
    ui.set_default_audio(preferences.options.audio);
    ui.set_default_desktop(preferences.options.desktop);
    ui.set_clipboard(preferences.options.clipboard);
    ui.set_unattended(preferences.options.unattended);
    ui.set_codec(
        CODECS
            .iter()
            .position(|codec| *codec == preferences.options.codec)
            .unwrap_or(0) as i32,
    );
    ui.set_max_streams(preferences.options.max_streams as i32);
    ui.set_quality(preferences.options.quality.index());
    ui.set_bandwidth_auto(preferences.options.bandwidth_mbps == 0);
    ui.set_bandwidth_mbps(if preferences.options.bandwidth_mbps == 0 {
        50
    } else {
        preferences.options.bandwidth_mbps as i32
    });
    ui.set_recent(model(
        recent_rows(&preferences)
            .into_iter()
            .map(Into::into)
            .collect(),
    ));
    let (actions, action_rx) = mpsc::channel();
    let (events, event_rx) = mpsc::channel();
    let close_behavior = launcher_close::bind(&ui, preferences.close_behavior, actions.clone());
    let update_events = events.clone();
    let update_requests = crate::update::start(move |status| {
        let _ = update_events.send(Event::Update(status));
    });
    let weak = ui.as_weak();
    ui.on_check_updates(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_update_busy(true);
        }
        let _ = update_requests.send(());
    });
    bind_update_restart(&ui, actions.clone());
    let worker = std::thread::spawn(move || {
        let (discovery_tx, discovery_done) = mpsc::channel();
        let mut worker = Worker {
            sharing_choices: Vec::new(),
            sharing_generation: sharing_picker::generation(),
            sessions: Vec::new(),
            next_id: 1,
            preferences,
            events,
            discovering: false,
            devices: Vec::new(),
            last_discovery: std::time::Instant::now() - Duration::from_secs(12),
            last_history_refresh: std::time::Instant::now(),
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
            ui.set_default_audio(ui.get_audio());
            ui.set_default_desktop(ui.get_desktop());
            let _ = tx.send(Action::Connect(
                address.to_string(),
                String::new(),
                ui.get_audio(),
                ui.get_desktop(),
            ));
        }
    });
    let weak = ui.as_weak();
    let tx = actions.clone();
    ui.on_connect_device(move |address, id, audio, desktop| {
        if let Some(ui) = weak.upgrade() {
            ui.set_audio(audio);
            ui.set_desktop(desktop);
            ui.set_default_audio(audio);
            ui.set_default_desktop(desktop);
            let _ = tx.send(Action::Connect(
                address.to_string(),
                id.to_string(),
                audio,
                desktop,
            ));
        }
    });
    let tx = actions.clone();
    ui.on_connection_options_edited(move |address, id, audio, desktop| {
        let _ = tx.send(Action::SaveConnectionChoices(
            address.to_string(),
            id.to_string(),
            audio,
            desktop,
        ));
    });
    let weak = ui.as_weak();
    ui.on_connection_address_edited(move || {
        if let Some(ui) = weak.upgrade()
            && let Ok(address) = endpoint(&ui.get_address())
            && let Ok(saved) = crate::connection_preferences::load()
            && let Some(p) = saved.get(&connection_device_id(address, ""))
        {
            ui.set_audio(p.audio);
            ui.set_desktop(p.desktop);
        }
    });
    ui.invoke_connection_address_edited();
    bind_settings_autosave(&ui, actions.clone());
    let tx = actions.clone();
    let weak = ui.as_weak();
    ui.on_device_name_edited(move || {
        if let Some(ui) = weak.upgrade() {
            let name = ui.get_device_name_edit();
            let weak = weak.clone();
            let tx = tx.clone();
            slint::Timer::single_shot(Duration::from_millis(450), move || {
                if let Some(ui) = weak.upgrade()
                    && ui.get_device_name_edit() == name
                {
                    let _ = tx.send(Action::SaveDeviceName(name.to_string()));
                }
            });
        }
    });
    let tx = actions.clone();
    ui.on_trust(move |id| {
        let _ = tx.send(Action::Trust(id.to_string()));
    });
    let tx = actions.clone();
    sharing_picker::bind(&ui, actions.clone());
    let weak = ui.as_weak();
    ui.on_toggle_sharing(move || {
        if let Some(ui) = weak.upgrade() {
            if ui.get_sharing() || ui.get_share_mode() == 0 {
                let _ = tx.send(Action::ToggleSharing);
            } else {
                ui.set_window_picker_open(true);
                ui.set_share_search("".into());
                ui.invoke_load_share_windows();
            }
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
    #[cfg(windows)]
    let tray_actions = actions.clone();
    let mut permission_notice = false;
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(100), move || {
        let Some(ui) = weak.upgrade() else { return; };
        for event in event_rx.try_iter() {
            match event {
                Event::ShareWindows(request,result)=>sharing_picker::apply_windows(&ui,request,result),
                Event::ShareWindowPreview(request,id,bytes)=>sharing_picker::apply_preview(&ui,request,id,bytes),
                Event::Update(status) => {
                    use crate::update::Status;
                    ui.set_update_busy(matches!(status, Status::Checking | Status::Downloading(..)));
                    ui.set_update_error(matches!(status, Status::Error(_)));
                    let message = match status {
                        Status::Checking => "Checking for Frames updates…".into(),
                        Status::Downloading(done, total) => format!("Downloading Frames update… {}%", done * 100 / total.max(1)),
                        Status::Ready(version) => {
                            ui.set_update_ready(true);
                            format!("Frames {version} is ready. Restart to install the update.")
                        }
                        Status::Current => "Frames is up to date.".into(),
                        Status::Deferred => "Automatic download paused during a session. Choose Check for updates to download now.".into(),
                        Status::Error(message) => format!("Couldn't update Frames: {message}"),
                    };
                    ui.set_update_status(message.into());
                }
                Event::UpdateRestart(result) => match result {
                    Ok(()) => { let _ = slint::quit_event_loop(); }
                    Err(error) => {
                        ui.set_update_restart_busy(false);
                        ui.set_update_restart_error(format!("Couldn't prepare update: {error}").into());
                    }
                },
                Event::Notice(message, error) => {
                    permission_notice = false;
                    ui.set_notice(message.into()); ui.set_notice_error(error);
                    ui.set_status(if error { "Action needed — see the message above" } else { "Ready · direct encrypted connections" }.into());
                    ui.set_status_error(error);
                }
                Event::SettingsSaved(options, result) => apply_settings_result(&ui, &options, result),
                Event::CloseBehaviorSaved(behavior,close_after,result)=>launcher_close::apply_saved(&ui,&close_behavior,behavior,close_after,result),
                Event::DeviceNameSaved(name,result)=>{
                    if ui.get_device_name_edit().as_str()==name {
                        match result {
                            Ok(())=>{ui.set_device_name(frames_native::device_name().into());ui.set_settings_status("".into());ui.set_settings_error(false);}
                            Err(error)=>{ui.set_settings_status(format!("Couldn't save device name: {error}").into());ui.set_settings_error(true);}
                        }
                    }
                },
                #[cfg(target_os = "macos")]
                Event::SharingBlocked(message) => {
                    permission_notice = true;
                    ui.invoke_show_permissions();
                    ui.set_notice(message.into());
                    ui.set_notice_error(true);
                }
                Event::Recent(rows) => {
                    ui.set_recent(model(rows.into_iter().map(Into::into).collect()));
                    ui.invoke_connection_address_edited();
                }
                Event::Trusted(rows) => ui.set_trusted(model(rows.into_iter().map(Into::into).collect())),
                Event::Nearby(result) => {
                    ui.set_discovering(false);
                    match result {
                        Ok(rows) => {
                            ui.set_nearby(model(rows.into_iter().map(Into::into).collect()));
                        }
                        Err(error) => { ui.set_notice(format!("Network discovery failed: {error}").into()); ui.set_notice_error(true); }
                    }
                }
                Event::Permissions(result) => match result {
                    Ok(value) => {
                        ui.set_screen_permission(value["screen_capture"].as_bool().map_or(-1, i32::from));
                        ui.set_control_permission(value["accessibility"].as_bool().map_or(-1, i32::from));
                        ui.set_background_sharing_registered(value["sharing_at_login"].as_bool().unwrap_or(false));
                        ui.set_background_sharing_running(value["sharing_running"].as_bool().unwrap_or(false));
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
        #[cfg(windows)]
        tray.poll(&ui, &tray_actions);
    });
    #[cfg(windows)]
    let result = ui.show().and_then(|_| slint::run_event_loop_until_quit());
    #[cfg(not(windows))]
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
    fn updating_with_active_sessions_requires_explicit_confirmation() {
        struct Headless;
        impl slint::platform::Platform for Headless {
            fn create_window_adapter(
                &self,
            ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
                Ok(
                    slint::platform::software_renderer::MinimalSoftwareWindow::new(
                        Default::default(),
                    ),
                )
            }
        }
        slint::platform::set_platform(Box::new(Headless)).unwrap();
        let ui = Launcher::new().unwrap();
        let (actions, received) = mpsc::channel();
        bind_update_restart(&ui, actions);
        ui.set_update_ready(true);
        ui.set_background_sharing_running(true);
        ui.set_sessions(model(vec![SessionRow {
            id: 1,
            name: "Remote host".into(),
            detail: "Connected".into(),
            hosting: false,
        }]));
        ui.invoke_restart_update();
        assert!(ui.get_update_prompt_open());
        assert!(
            received.try_recv().is_err(),
            "Opening confirmation must not disconnect sessions"
        );
        ui.set_update_prompt_open(false); // Cancel or Escape.
        ui.invoke_confirm_restart_update();
        assert!(
            received.try_recv().is_err(),
            "A dismissed prompt grants no permission"
        );
        assert_eq!(ui.get_sessions().row_count(), 1);
        ui.invoke_restart_update();
        ui.invoke_confirm_restart_update();
        assert!(matches!(
            received.try_recv().unwrap(),
            Action::RestartUpdate
        ));
        assert!(ui.get_update_restart_busy());
        ui.invoke_confirm_restart_update();
        assert!(
            received.try_recv().is_err(),
            "Ignore duplicate confirmation clicks"
        );
    }

    #[test]
    fn settings_autosave_and_respect_running_sharing() {
        struct Headless;
        impl slint::platform::Platform for Headless {
            fn create_window_adapter(
                &self,
            ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
                Ok(
                    slint::platform::software_renderer::MinimalSoftwareWindow::new(
                        slint::platform::software_renderer::RepaintBufferType::ReusedBuffer,
                    ),
                )
            }
        }
        slint::platform::set_platform(Box::new(Headless)).unwrap();
        let ui = Launcher::new().unwrap();
        let (actions, saves) = mpsc::channel();
        bind_settings_autosave(&ui, actions);
        ui.set_default_audio(true);
        ui.invoke_settings_edited();
        let Action::SaveSettings(first) = saves.try_recv().unwrap() else {
            panic!("Every settings edit must immediately queue a save");
        };
        assert!(first.audio);
        ui.set_default_desktop(true);
        ui.set_bandwidth_auto(false);
        ui.set_bandwidth_mbps(50);
        ui.set_quality(2);
        ui.invoke_settings_edited();
        let Action::SaveSettings(latest) = saves.try_recv().unwrap() else {
            panic!("Further edits must remain available while saving");
        };
        assert!(latest.audio && latest.desktop);
        assert_eq!(latest.bandwidth_mbps, 50);
        assert_eq!(latest.quality, frames_core::adaptive::QualityPreset::Low);
        let persisted: SessionOptions =
            serde_json::from_slice(&serde_json::to_vec(&latest).unwrap()).unwrap();
        assert_eq!(persisted.bandwidth_mbps, 50);
        assert_eq!(persisted.quality, frames_core::adaptive::QualityPreset::Low);
        apply_settings_result(&ui, &first, Err("outdated failure".into()));
        assert!(ui.get_settings_status().is_empty());
        apply_settings_result(&ui, &latest, Err("disk full".into()));
        assert!(ui.get_settings_error());
        assert!(ui.get_settings_status().contains("disk full"));
        apply_settings_result(&ui, &first, Ok(()));
        assert!(
            ui.get_settings_error(),
            "Older saves must not clear current errors"
        );
        apply_settings_result(&ui, &latest, Ok(()));
        assert!(ui.get_audio() && ui.get_desktop());
        assert!(!ui.get_settings_error());
        assert!(ui.get_settings_status().is_empty());
        ui.set_background_sharing_registered(true);
        ui.set_background_sharing_running(false);
        assert!(ui.get_unattended_editable());

        ui.set_unattended(true);
        ui.set_max_streams(8);
        let saved = serde_json::to_string(&SessionOptions::from_ui(&ui)).unwrap();
        let options: SessionOptions = serde_json::from_str(&saved).unwrap();
        assert!(options.unattended);
        assert!(
            options
                .sharing_args()
                .iter()
                .any(|arg| arg == "--unattended")
        );

        ui.set_sharing(true);
        assert!(
            !ui.get_unattended_editable(),
            "Running hosts must be restarted to apply changed options"
        );
        ui.set_sharing(false);
        ui.set_background_sharing_running(true);
        assert!(!ui.get_unattended_editable());
    }

    #[test]
    fn old_preferences_keep_connection_defaults_and_gain_safe_settings() {
        let preferences: Preferences = serde_json::from_str(
            r#"{"audio":true,"desktop":true,"recent":[{"address":"127.0.0.1:47990","opened":42}]}"#,
        )
        .unwrap();
        assert!(preferences.options.audio);
        assert!(preferences.options.desktop);
        assert!(preferences.options.clipboard);
        assert!(!preferences.options.unattended);
        assert_eq!(preferences.options.codec, "auto");
        assert_eq!(preferences.options.bandwidth_mbps, 0);
        assert_eq!(
            preferences.options.quality,
            frames_core::adaptive::QualityPreset::High
        );
        assert_eq!(preferences.options.max_streams, 8);
        assert_eq!(preferences.recent.len(), 1);
        assert_eq!(preferences.close_behavior, CloseBehavior::Ask);
        preferences.options.validate().unwrap();
        let restored: Preferences =
            serde_json::from_slice(&serde_json::to_vec(&preferences).unwrap()).unwrap();
        assert_eq!(restored.options, preferences.options);
        assert_eq!(restored.recent[0].address, preferences.recent[0].address);
    }

    #[test]
    fn close_behavior_is_saved_with_existing_connection_preferences() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("launcher.json");
        let mut preferences = Preferences::default();
        preferences.options.audio = true;
        preferences.remember("127.0.0.1:47990".parse().unwrap());
        for behavior in [CloseBehavior::Hide, CloseBehavior::Quit, CloseBehavior::Ask] {
            preferences.close_behavior = behavior;
            preferences.save_to(&path).unwrap();
            let restored: Preferences = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            assert_eq!(restored.close_behavior, behavior);
            assert!(restored.options.audio);
            assert_eq!(restored.recent[0].address, "127.0.0.1:47990");
        }
    }

    #[test]
    fn settings_reach_the_real_host_and_connect_cli() {
        use clap::Parser;
        let options = SessionOptions {
            audio: true,
            desktop: true,
            clipboard: false,
            unattended: true,
            codec: "hevc".into(),
            bandwidth_mbps: 50,
            quality: frames_core::adaptive::QualityPreset::High,
            max_streams: 12,
        };
        let connect = crate::Args::try_parse_from(
            std::iter::once("frames".to_string())
                .chain(options.connection_args(endpoint("127.0.0.1").unwrap())),
        )
        .unwrap();
        assert!(matches!(
            connect.command,
            Some(crate::Command::Connect {
                audio: true,
                no_clipboard: true,
                view: crate::ViewChoice::Desktop,
                codec: crate::CodecChoice::Hevc,
                bandwidth_mbps: 50,
                ..
            })
        ));
        let host = crate::Args::try_parse_from(
            std::iter::once("frames".to_string()).chain(options.sharing_args()),
        )
        .unwrap();
        assert!(matches!(
            host.command,
            Some(crate::Command::Host {
                pair: true,
                unattended: true,
                no_clipboard: true,
                max_streams: 12,
                ..
            })
        ));
        let defaults = SessionOptions::default();
        assert!(
            !defaults
                .sharing_args()
                .iter()
                .any(|arg| arg == "--unattended" || arg == "--no-clipboard")
        );
        assert!(
            !defaults
                .connection_args(endpoint("127.0.0.1").unwrap())
                .iter()
                .any(|arg| arg == "--audio" || arg == "--view" || arg == "--no-clipboard")
        );
    }

    #[test]
    fn invalid_saved_options_are_rejected() {
        use clap::Parser;
        assert!(
            SessionOptions {
                bandwidth_mbps: 201,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            crate::Args::try_parse_from([
                "frames",
                "connect",
                "127.0.0.1:47990",
                "--bandwidth-mbps",
                "201"
            ])
            .is_err()
        );
        for max_streams in [0, 33] {
            assert!(
                SessionOptions {
                    max_streams,
                    ..Default::default()
                }
                .validate()
                .is_err()
            );
        }
        assert!(
            SessionOptions {
                codec: "unknown".into(),
                ..Default::default()
            }
            .validate()
            .is_err()
        );
    }

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
