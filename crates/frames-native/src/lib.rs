use frames_core::{input::InputEvent, media::RawFrame, model::*};
use std::sync::Arc;
use tokio::sync::{mpsc, watch};
pub mod audio;

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

#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

pub trait Capture: Send {
    fn stop(&mut self);
}
pub struct CaptureStream {
    pub frames: watch::Receiver<Option<RawFrame>>,
    pub guard: Box<dyn Capture>,
}
pub trait Desktop: Send + Sync {
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
    fn capture(&self, key: WindowId) -> anyhow::Result<CaptureStream>;
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
    ViewMode(ViewMode),
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
    Ack(WindowId, Option<RemoteWindow>, Option<String>),
    Shutdown,
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
    updates: std::sync::mpsc::Receiver<ClientUpdate>,
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

pub fn confirm_pairing(name: &str, code: &str, reason: &str) -> anyhow::Result<bool> {
    #[cfg(windows)]
    {
        use ::windows::{Win32::UI::WindowsAndMessaging::*, core::PCWSTR};
        let name: String = name.chars().filter(|c| !c.is_control()).take(80).collect();
        let message = format!(
            "{reason}\n\nVerification code: {code}\n\nDevice: {name}\n\nApprove only if the other device shows this same code."
        );
        let text: Vec<_> = message.encode_utf16().chain(Some(0)).collect();
        let title: Vec<_> = "Frames device pairing"
            .encode_utf16()
            .chain(Some(0))
            .collect();
        Ok(unsafe {
            MessageBoxW(
                None,
                PCWSTR(text.as_ptr()),
                PCWSTR(title.as_ptr()),
                MB_YESNO | MB_DEFBUTTON2 | MB_ICONQUESTION,
            )
        } == IDYES)
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
        use ::windows::{Win32::UI::WindowsAndMessaging::*, core::PCWSTR};
        let title: Vec<_> = title.encode_utf16().chain(Some(0)).collect();
        let message: Vec<_> = message.encode_utf16().chain(Some(0)).collect();
        unsafe {
            MessageBoxW(
                None,
                PCWSTR(message.as_ptr()),
                PCWSTR(title.as_ptr()),
                MB_OK | MB_ICONERROR,
            );
        }
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
