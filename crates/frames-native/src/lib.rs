use frames_core::{input::InputEvent, media::RawFrame, model::*};
use std::sync::Arc;
use tokio::sync::{mpsc, watch};
pub mod audio;
mod client_updates;
mod device_name;
mod window_selection;
pub use window_selection::{
    SharingWindow, WindowSelection, parse_selection, selected_desktop, sharing_windows,
};
pub mod gamepad;
pub mod usb;
pub use client_updates::{ClientReceiver, ClientSendError, ClientSender, client_channel};
pub use device_name::{device_name, device_name_override, set_device_name, system_device_name};
#[cfg(all(windows, feature = "media-diagnostics"))]
pub use windows::diagnostic_capture_window;
#[cfg(target_os = "macos")]
mod macos_tray;
#[cfg(any(windows, target_os = "macos"))]
mod tray_model;
#[cfg(target_os = "macos")]
pub use macos_tray::Tray;
#[cfg(any(windows, target_os = "macos"))]
pub use tray_model::{TrayGlyph, TrayItem};
#[cfg(windows)]
pub use windows::tray::Tray;

#[cfg(target_os = "macos")]
pub fn permission_status(request: bool) -> anyhow::Result<serde_json::Value> {
    macos::permission_status(request)
}

#[cfg(target_os = "macos")]
pub fn configure_login_item() -> anyhow::Result<()> {
    macos::configure_login_item()
}

#[cfg(target_os = "macos")]
pub fn open_network_settings() -> anyhow::Result<()> {
    macos::open_network_settings()
}

#[cfg(target_os = "macos")]
pub fn request_permission(kind: &str) -> anyhow::Result<()> {
    macos::request_permission(kind)
}

#[cfg(any(windows, test))]
#[cfg_attr(not(windows), allow(dead_code))]
mod debug_raster;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(any(target_os = "macos", test))]
mod native_media;
#[cfg(windows)]
mod windows;

pub trait Capture: Send {
    fn stop(&mut self);
}
pub struct CaptureStream {
    pub frames: watch::Receiver<Option<RawFrame>>,
    pub guard: Box<dyn Capture>,
}
pub struct NativeEncodedFrame {
    pub packet: frames_core::codec::EncodedPacket,
    pub width: u32,
    pub height: u32,
    pub source_width: u32,
    pub source_height: u32,
}
pub enum NativeEncoderCommand {
    Keyframe,
    Configure { bitrate: u32, fps: u32, scale: u32 },
}
pub struct NativeEncodedCapture {
    pub packets: mpsc::Receiver<NativeEncodedFrame>,
    pub control: std::sync::mpsc::SyncSender<NativeEncoderCommand>,
    pub delivery: std::sync::mpsc::SyncSender<NativeFrameDelivery>,
    pub guard: Box<dyn Capture>,
}
pub struct NativeFrameDelivery {
    pub number: u64,
    pub refresh: bool,
}
impl NativeEncodedCapture {
    pub fn acknowledge(&self, number: u64, refresh: bool) -> anyhow::Result<()> {
        self.delivery
            .try_send(NativeFrameDelivery { number, refresh })
            .map_err(|error| anyhow::anyhow!("native delivery acknowledgement failed: {error}"))
    }
}
pub trait Desktop: Send + Sync {
    /// Local picker identity; never transmitted in the remote window graph.
    fn local_window_handle(&self, _key: WindowId) -> anyhow::Result<u64> {
        anyhow::bail!("Window selection hosting currently requires Windows")
    }
    fn allow_desktop(&self) -> bool {
        true
    }
    /// A one-shot PNG capture, independent of active video streams when available.
    fn preview_image(&self, _window: Option<WindowId>) -> anyhow::Result<Option<Vec<u8>>> {
        Ok(None)
    }
    fn preview_capture(&self, window: Option<WindowId>) -> anyhow::Result<CaptureStream> {
        let key = match window {
            Some(key) => key,
            None => {
                self.surfaces(if self.shared_session().is_some() {
                    ViewMode::Windows
                } else {
                    ViewMode::Desktop
                })?
                .into_iter()
                .find(|w| w.visible && !w.minimized)
                .ok_or_else(|| anyhow::anyhow!("no screen available for preview"))?
                .key
            }
        };
        self.capture(key, false)
    }
    fn shared_session(&self) -> Option<frames_core::shared_game::SharedSession> {
        None
    }
    fn begin_control(&self) -> anyhow::Result<()> {
        Ok(())
    }
    fn prepare_control(&self) -> anyhow::Result<Option<frames_core::shared_game::SharedSession>> {
        Ok(None)
    }
    fn end_control(&self) {}
    fn shared_heartbeat(&self, _generation: u64) -> anyhow::Result<()> {
        anyhow::bail!("shared input unavailable")
    }
    fn shared_resume(&self, _generation: u64) -> anyhow::Result<()> {
        Ok(())
    }
    fn shared_input(
        &self,
        _key: WindowId,
        _generation: u64,
        _event: &InputEvent,
    ) -> anyhow::Result<()> {
        anyhow::bail!("shared input unavailable")
    }
    fn shared_release(&self, _generation: u64) {}
    fn shared_cursor(&self) -> anyhow::Result<Option<frames_core::shared_game::Cursor>> {
        Ok(None)
    }
    fn allow_clipboard(&self) -> bool {
        true
    }
    /// Detect a lost native backend even when no remote session is connected.
    fn check_health(&self) -> anyhow::Result<()> {
        Ok(())
    }
    fn windows(&self) -> anyhow::Result<Vec<RemoteWindow>>;
    fn desktop_windows(&self) -> anyhow::Result<Vec<RemoteWindow>> {
        anyhow::bail!("full-desktop capture is unavailable")
    }
    fn surfaces(&self, mode: ViewMode) -> anyhow::Result<Vec<RemoteWindow>> {
        match mode {
            ViewMode::Windows => self.windows(),
            ViewMode::Desktop => self.desktop_windows(),
        }
    }
    fn allow_device_bridge(&self) -> bool {
        true
    }
    fn action(&self, key: WindowId, action: &WindowAction) -> anyhow::Result<()>;
    fn input(&self, key: WindowId, event: &InputEvent) -> anyhow::Result<()>;
    fn release_input(&self);
    fn capture(&self, key: WindowId, show_cursor: bool) -> anyhow::Result<CaptureStream>;
    fn encoded_capture(
        &self,
        _key: WindowId,
        _cursor: bool,
        _capability: &frames_core::codec::CodecCapability,
        _fps: u32,
    ) -> anyhow::Result<Option<NativeEncodedCapture>> {
        Ok(None)
    }
    fn capture_for_encoding(
        &self,
        key: WindowId,
        show_cursor: bool,
        _hardware: bool,
    ) -> anyhow::Result<CaptureStream> {
        self.capture(key, show_cursor)
    }
    fn clipboard(&self) -> anyhow::Result<Option<String>>;
    fn set_clipboard(&self, text: &str) -> anyhow::Result<()>;
    fn clipboard_image(&self) -> anyhow::Result<Option<Vec<u8>>> {
        Ok(None)
    }
    fn set_clipboard_image(&self, _png: &[u8]) -> anyhow::Result<()> {
        anyhow::bail!("image clipboard unavailable")
    }
    fn menu(&self, _key: WindowId) -> anyhow::Result<Vec<MenuItem>> {
        anyhow::bail!("use the menu inside the captured application window")
    }
    fn audio(&self) -> anyhow::Result<audio::AudioCapture> {
        audio::loopback()
    }
    fn allow_file_transfer(&self) -> bool {
        true
    }
    fn drop_files(
        &self,
        _window: WindowId,
        _paths: &[std::path::PathBuf],
        _x: f64,
        _y: f64,
    ) -> anyhow::Result<()> {
        anyhow::bail!("native file drop unavailable")
    }
}

#[derive(Clone, Debug)]
pub enum ClientEvent {
    SharedAlive {
        created: std::time::Instant,
    },
    ResumeInput {
        created: std::time::Instant,
    },
    Devices(frames_core::devices::DeviceAction),
    Decoded {
        window: WindowId,
        generation: u32,
        number: u64,
        micros: u64,
    },
    DecoderFailed {
        window: WindowId,
        generation: u32,
        reason: String,
    },
    MediaRecovery {
        window: WindowId,
        generation: u32,
    },
    Presented {
        window: WindowId,
        generation: u32,
        number: u64,
        at: std::time::Instant,
    },
    TimedInput {
        window: WindowId,
        event: InputEvent,
        created: std::time::Instant,
    },
    ViewMode(ViewMode),
    Quality(frames_core::adaptive::QualityPreset),
    RemoteCursor {
        visible: bool,
    },
    AdbBridge {
        enabled: bool,
    },
    CancelTransfers,
    Visibility {
        window: WindowId,
        visible: bool,
        focused: bool,
    },
    Action {
        window: WindowId,
        action: WindowAction,
    },
    Input {
        window: WindowId,
        event: InputEvent,
    },
    ReleaseInput,
    /// Starts or stops per-window media diagnostics for the debug overlay.
    DebugOverlay {
        enabled: bool,
    },
    Disconnected,
    SendFiles {
        window: WindowId,
        paths: Vec<std::path::PathBuf>,
        x: f64,
        y: f64,
    },
    DownloadFile {
        path: String,
    },
}
pub enum ClientUpdate {
    Quality(frames_core::adaptive::QualityPreset),
    SharedSession(Box<frames_core::shared_game::SharedSession>),
    SharedCursor(frames_core::shared_game::Cursor),
    Devices(frames_core::devices::DeviceList),
    #[cfg(target_os = "macos")]
    EncodedVideo {
        window: WindowId,
        generation: u32,
        codec: frames_core::codec::Codec,
        width: u32,
        height: u32,
        packet: frames_core::codec::EncodedPacket,
    },
    RemoteCursor {
        visible: bool,
    },
    ViewMode {
        mode: ViewMode,
        error: Option<String>,
    },
    AdbBridge {
        enabled: bool,
        address: Option<String>,
        error: Option<String>,
    },
    TransferProgress {
        name: String,
        done: u64,
        total: u64,
    },
    Peer {
        platform: String,
        device_id: String,
        name: String,
    },
    Menu {
        window: WindowId,
        items: Vec<MenuItem>,
    },
    ReceivedFile {
        path: std::path::PathBuf,
    },
    Windows(Vec<RemoteWindow>),
    Frame(WindowId, RawFrame),
    Status(WindowId, String),
    /// A complete, ready-to-draw debug overlay for one window. Sent only while
    /// the overlay is enabled; each snapshot replaces the previous one.
    DebugOverlay(WindowId, Box<DebugOverlay>),
    Ack(WindowId, Option<RemoteWindow>, Option<String>),
    Shutdown,
}

pub const DEBUG_OVERLAY_SHORTCUT: &str = if cfg!(target_os = "macos") {
    "⌃⌥F10"
} else {
    "Ctrl+Alt+F10"
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DebugLevel {
    #[default]
    Normal,
    Dim,
    Warn,
    Bad,
}
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct DebugLine {
    pub text: String,
    pub level: DebugLevel,
}
/// Media diagnostics formatted by the session so every platform draws the
/// same panel. Text is laid out for a monospaced font.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct DebugOverlay {
    pub lines: Vec<DebugLine>,
    /// One entry per presented frame, oldest first:
    /// `[interval since previous frame, capture-to-present latency]` in µs
    /// (0 when unknown).
    pub frames: Vec<[u32; 2]>,
    /// Target frame interval in µs, drawn as a reference line.
    pub frame_budget_us: u32,
    /// Received video bitrate per sample in kb/s, oldest first.
    pub bitrate_kbps: Vec<u32>,
    pub sample_ms: u32,
}

pub fn desktop(exclusions: Vec<String>) -> anyhow::Result<Arc<dyn Desktop>> {
    #[cfg(windows)]
    {
        Ok(Arc::new(windows::Host::new(exclusions)?))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(Arc::new(macos::Host::new(exclusions)?))
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = exclusions;
        anyhow::bail!("Frames currently supports Windows and macOS")
    }
}
pub fn run_client(
    host: String,
    updates: ClientReceiver,
    events: mpsc::UnboundedSender<ClientEvent>,
) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        windows::run_client(host, updates, events)
    }
    #[cfg(target_os = "macos")]
    {
        macos::run_client(host, updates, events)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (host, updates, events);
        anyhow::bail!("unsupported client platform")
    }
}

pub fn shared_desktop(
    exclusions: Vec<String>,
    hwnd: Option<u64>,
) -> anyhow::Result<Arc<dyn Desktop>> {
    #[cfg(all(windows, frames_shared_input))]
    {
        windows::shared_game::create(exclusions, hwnd)
    }
    #[cfg(not(all(windows, frames_shared_input)))]
    {
        let _ = (exclusions, hwnd);
        anyhow::bail!("Shared game hosting requires Windows x64 with the MSVC adapter")
    }
}
pub fn shared_selected_desktop(
    exclusions: Vec<String>,
    argument: String,
) -> anyhow::Result<Arc<dyn Desktop>> {
    let (id, marker) = parse_selection(&argument)?;
    #[cfg(all(windows, frames_shared_input))]
    {
        windows::shared_game::create_marked(exclusions, id, marker)
    }
    #[cfg(target_os = "macos")]
    {
        let _ = (id, marker);
        selected_desktop(exclusions, vec![argument])
    }
    #[cfg(not(any(all(windows, frames_shared_input), target_os = "macos")))]
    {
        let _ = (id, marker, exclusions);
        anyhow::bail!("Isolated window control requires Windows x64 with the MSVC adapter")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionDecision {
    Decline,
    Accept,
    Trust,
}

pub fn confirm_pairing(name: &str, code: &str, reason: &str) -> anyhow::Result<ConnectionDecision> {
    #[cfg(windows)]
    {
        let name: String = name.chars().filter(|c| !c.is_control()).take(80).collect();
        let message = format!(
            "{reason}\n\nVerification code: {code}\n\nDevice: {name}\n\nApprove only if the other device shows this same code."
        );
        windows::dialogs::connection(&name, &message)
    }
    #[cfg(target_os = "macos")]
    {
        macos::confirm_pairing(name, code, reason)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (name, code, reason);
        anyhow::bail!("unsupported platform")
    }
}
pub fn launcher() -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        windows::launcher::run()
    }
    #[cfg(target_os = "macos")]
    {
        macos::launcher()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        anyhow::bail!("unsupported platform")
    }
}

pub fn show_error(title: &str, message: &str) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        use ::windows::Win32::UI::WindowsAndMessaging::*;
        windows::dialogs::message(title, message, MB_OK | MB_ICONERROR);
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        macos::show_error(title, message)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (title, message);
        Ok(())
    }
}
