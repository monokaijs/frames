//! One-window host whose only input path is the process-local ProtoInput adaptation.
use super::{Host, host::PhysicalDpi, wide};
use crate::{CaptureStream, Desktop};
use anyhow::{Context, Result};
use base64::Engine;
use frames_core::{
    input::{InputEvent, hid_pc_scan},
    model::*,
    shared_game::*,
};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use std::{
    ffi::c_void,
    io::{Cursor as IoCursor, Read},
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    path::PathBuf,
    sync::{Arc, OnceLock},
};
use windows::{
    Win32::{
        Foundation::*,
        System::{LibraryLoader::*, Threading::*},
        UI::{Controls::*, WindowsAndMessaging::*},
    },
    core::*,
};

const ARTIFACT: &[u8] = include_bytes!(env!("FRAMES_INPUT_ARTIFACT"));
#[repr(C)]
#[derive(Default)]
pub(super) struct Command {
    magic: u32,
    size: u32,
    abi: u32,
    kind: u32,
    generation: u64,
    sequence: u64,
    a: i32,
    b: i32,
    c: i32,
    d: i32,
    reserved: [u64; 2],
}
#[repr(C)]
pub(super) struct NativeStatus {
    magic: u32,
    size: u32,
    abi: u32,
    phase: u32,
    error: u32,
    width: u32,
    height: u32,
    hot_x: u32,
    hot_y: u32,
    visible: u32,
    x: i32,
    y: i32,
    generation: u64,
    image_generation: u64,
    delivered: u64,
    latency_max_us: u64,
    latency_histogram: [u64; 32],
    rgba: [u8; 65536],
}
const _: () = {
    assert!(std::mem::size_of::<Command>() == 64);
    assert!(std::mem::size_of::<NativeStatus>() == 65872);
};
type Attach = unsafe extern "C" fn(*const u16, u64, u64, *mut u32) -> *mut c_void;
type SendCommand = unsafe extern "C" fn(*mut c_void, *const Command) -> BOOL;
type Poll = unsafe extern "C" fn(*mut c_void, *mut NativeStatus) -> BOOL;
type Close = unsafe extern "C" fn(*mut c_void);
pub(super) struct Library {
    pub(super) module: HMODULE,
    path: PathBuf,
    attach: Attach,
    send: SendCommand,
    poll: Poll,
    close: Close,
}
unsafe impl Send for Library {}
unsafe impl Sync for Library {}
pub(super) fn library() -> Result<Arc<Library>> {
    static LIBRARY: OnceLock<std::result::Result<Arc<Library>, String>> = OnceLock::new();
    LIBRARY
        .get_or_init(|| load().map(Arc::new).map_err(|e| format!("{e:#}")))
        .clone()
        .map_err(anyhow::Error::msg)
}
fn load() -> Result<Library> {
    let hash = format!("{:x}", Sha256::digest(ARTIFACT));
    let directory = frames_core::trust::config_dir()?
        .join("shared-input")
        .join(&hash);
    std::fs::create_dir_all(&directory)?;
    let path = directory.join("FramesInput.dll");
    if !path.exists() {
        let temp = directory.join(format!("{}.tmp", uuid::Uuid::new_v4()));
        std::fs::write(&temp, ARTIFACT)?;
        if let Err(e) = std::fs::rename(&temp, &path) {
            let _ = std::fs::remove_file(&temp);
            if !path.exists() {
                return Err(e.into());
            }
        }
    }
    anyhow::ensure!(
        Sha256::digest(std::fs::read(&path)?).as_slice() == Sha256::digest(ARTIFACT).as_slice(),
        "shared input library checksum mismatch"
    );
    unsafe {
        let module = LoadLibraryExW(
            PCWSTR(wide(&path.to_string_lossy()).as_ptr()),
            None,
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
        )?;
        macro_rules! symbol {
            ($name:literal,$ty:ty) => {{
                let address = GetProcAddress(module, PCSTR(concat!($name, "\0").as_ptr()))
                    .context(concat!("missing ", $name))?;
                std::mem::transmute::<unsafe extern "system" fn() -> isize, $ty>(address)
            }};
        }
        Ok(Library {
            module,
            path,
            attach: symbol!("FramesBrokerAttach", Attach),
            send: symbol!("FramesBrokerSend", SendCommand),
            poll: symbol!("FramesBrokerPoll", Poll),
            close: symbol!("FramesBrokerClose", Close),
        })
    }
}
struct Broker {
    library: Arc<Library>,
    handle: *mut c_void,
}
unsafe impl Send for Broker {}
impl Broker {
    fn attach(library: Arc<Library>, hwnd: u64, generation: u64) -> Result<Self> {
        let mut error = 0;
        let handle = unsafe {
            (library.attach)(
                wide(&library.path.to_string_lossy()).as_ptr(),
                hwnd,
                generation,
                &mut error,
            )
        };
        anyhow::ensure!(
            !handle.is_null(),
            "Cannot attach shared input adapter (Windows error {error}). The game must be x64, in this user session and at the same privilege level."
        );
        Ok(Self { library, handle })
    }
    fn send(&mut self, command: Command) -> Result<()> {
        anyhow::ensure!(
            unsafe { (self.library.send)(self.handle, &command) }.as_bool(),
            "shared input pipe stopped"
        );
        Ok(())
    }
    fn poll(&mut self) -> Result<Box<NativeStatus>> {
        let mut status = Box::new(unsafe { std::mem::zeroed::<NativeStatus>() });
        anyhow::ensure!(
            unsafe { (self.library.poll)(self.handle, &mut *status) }.as_bool(),
            "game input adapter disconnected"
        );
        Ok(status)
    }
}
impl Drop for Broker {
    fn drop(&mut self) {
        unsafe { (self.library.close)(self.handle) }
    }
}
struct Runtime {
    info: SharedSession,
    broker: Option<Broker>,
    last_image: Option<[u8; 32]>,
}
pub struct SharedHost {
    host: Host,
    key: WindowId,
    hwnd: u64,
    pid: u32,
    process: OwnedHandle,
    property: Vec<u16>,
    property_token: usize,
    hash: String,
    executable: String,
    remote: Mutex<RemoteWindow>,
    runtime: Mutex<Runtime>,
}
fn hash_file(path: &str) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut bytes = [0; 65536];
    loop {
        let n = file.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        hash.update(&bytes[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

pub fn create(exclusions: Vec<String>, selected: Option<u64>) -> Result<Arc<dyn Desktop>> {
    create_inner(exclusions, selected, None)
}
pub(crate) fn create_marked(
    exclusions: Vec<String>,
    id: u64,
    marker: String,
) -> Result<Arc<dyn Desktop>> {
    create_inner(exclusions, Some(id), Some(marker))
}
fn create_inner(
    exclusions: Vec<String>,
    selected: Option<u64>,
    marker: Option<String>,
) -> Result<Arc<dyn Desktop>> {
    let host = Host::new(exclusions)?;
    let list: Vec<_> = host
        .windows()?
        .into_iter()
        .filter(|w| w.parent.is_none() && w.role == WindowRole::Window)
        .collect();
    anyhow::ensure!(!list.is_empty(), "Open a window before starting sharing");
    let key = if let Some(handle) = selected {
        list.iter()
            .find(|w| host.hwnd(w.key).is_ok_and(|h| h.0 as u64 == handle))
            .map(|w| w.key)
            .context("selected window is unavailable")?
    } else {
        choose(&list)?
    };
    let remote = list.iter().find(|w| w.key == key).unwrap();
    anyhow::ensure!(!remote.minimized, "Restore the game before sharing it");
    let hwnd = host.hwnd(key)?;
    let mut pid = 0;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    let raw = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            false,
            pid,
        )?
    };
    let process = unsafe { OwnedHandle::from_raw_handle(raw.0) };
    let mut executable = vec![0u16; 32768];
    let mut n = executable.len() as u32;
    unsafe {
        QueryFullProcessImageNameW(
            raw,
            PROCESS_NAME_WIN32,
            PWSTR(executable.as_mut_ptr()),
            &mut n,
        )?;
    }
    let executable = String::from_utf16_lossy(&executable[..n as usize]);
    let hash = hash_file(&executable)?;
    let compatibility = Catalog::bundled()?.lookup(&hash);
    let provided = marker.is_some();
    let property =
        wide(&marker.unwrap_or_else(|| format!("FramesShared-{}", uuid::Uuid::new_v4())));
    let property_token = if provided {
        1
    } else {
        (uuid::Uuid::new_v4().as_u128() as usize).max(1)
    };
    unsafe {
        if provided {
            anyhow::ensure!(
                GetPropW(hwnd, PCWSTR(property.as_ptr())).0 as usize == property_token,
                "The selected window closed; choose it again"
            );
        } else {
            SetPropW(
                hwnd,
                PCWSTR(property.as_ptr()),
                Some(HANDLE(property_token as *mut c_void)),
            )?;
        }
    }
    Ok(Arc::new(SharedHost {
        host,
        key,
        hwnd: hwnd.0 as u64,
        pid,
        process,
        property,
        property_token,
        hash,
        executable,
        remote: Mutex::new(remote.clone()),
        runtime: Mutex::new(Runtime {
            info: SharedSession {
                window: key,
                generation: 0,
                phase: Phase::Attaching,
                detail: format!("{} · waiting for a viewer", compatibility.status.label()),
                compatibility,
            },
            broker: None,
            last_image: None,
        }),
    }))
}
fn choose(list: &[RemoteWindow]) -> Result<WindowId> {
    let labels: Vec<_> = list
        .iter()
        .map(|w| wide(&format!("{} — {}", w.app_name, w.title)))
        .collect();
    let buttons: Vec<_> = labels
        .iter()
        .enumerate()
        .map(|(i, s)| TASKDIALOG_BUTTON {
            nButtonID: i as i32 + 100,
            pszButtonText: PCWSTR(s.as_ptr()),
        })
        .collect();
    let title = wide("Frames · One window");
    let instruction = wide("Choose the window to share");
    let content = wide(
        "Your friend controls this window while you use other applications. Input compatibility varies by application. Close the viewer to disconnect.",
    );
    let config = TASKDIALOGCONFIG {
        cbSize: std::mem::size_of::<TASKDIALOGCONFIG>() as u32,
        dwFlags: TDF_ALLOW_DIALOG_CANCELLATION | TDF_SIZE_TO_CONTENT,
        dwCommonButtons: TDCBF_OK_BUTTON | TDCBF_CANCEL_BUTTON,
        pszWindowTitle: PCWSTR(title.as_ptr()),
        pszMainInstruction: PCWSTR(instruction.as_ptr()),
        pszContent: PCWSTR(content.as_ptr()),
        cRadioButtons: buttons.len() as u32,
        pRadioButtons: buttons.as_ptr(),
        nDefaultRadioButton: 100,
        ..Default::default()
    };
    let (mut button, mut selected) = (0, 0);
    unsafe {
        TaskDialogIndirect(&config, Some(&mut button), Some(&mut selected), None)?;
    }
    anyhow::ensure!(button == IDOK.0, "Window selection canceled");
    list.get((selected - 100) as usize)
        .map(|w| w.key)
        .context("choose a window")
}
struct WindowDpi(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT);
impl WindowDpi {
    unsafe fn enter(hwnd: HWND) -> Self {
        unsafe {
            use windows::Win32::UI::HiDpi::*;
            Self(SetThreadDpiAwarenessContext(GetWindowDpiAwarenessContext(
                hwnd,
            )))
        }
    }
}
impl Drop for WindowDpi {
    fn drop(&mut self) {
        unsafe {
            if !self.0.0.is_null() {
                windows::Win32::UI::HiDpi::SetThreadDpiAwarenessContext(self.0);
            }
        }
    }
}
impl SharedHost {
    fn verify(&self) -> Result<HWND> {
        let h = HWND(self.hwnd as *mut c_void);
        let mut pid = 0;
        unsafe {
            GetWindowThreadProcessId(h, Some(&mut pid));
            anyhow::ensure!(
                IsWindow(Some(h)).as_bool()
                    && GetPropW(h, PCWSTR(self.property.as_ptr())).0 as usize
                        == self.property_token
                    && pid == self.pid
                    && WaitForSingleObject(HANDLE(self.process.as_raw_handle()), 0) == WAIT_TIMEOUT,
                "selected window closed"
            );
        }
        Ok(h)
    }
    fn send(&self, generation: u64, c: Command) -> Result<()> {
        self.verify()?;
        let mut runtime = self.runtime.lock();
        anyhow::ensure!(
            runtime.info.generation == generation,
            "stale shared session"
        );
        runtime
            .broker
            .as_mut()
            .context("shared game is not attached")?
            .send(c)
    }
    fn point(&self, x: f64, y: f64) -> Result<POINT> {
        let hwnd = self.verify()?;
        unsafe {
            let _scope = WindowDpi::enter(hwnd);
            let mut rect = RECT::default();
            GetClientRect(hwnd, &mut rect)?;
            Ok(POINT {
                x: ((rect.right - rect.left - 1).max(0) as f64 * x).round() as i32,
                y: ((rect.bottom - rect.top - 1).max(0) as f64 * y).round() as i32,
            })
        }
    }
}
impl Desktop for SharedHost {
    fn check_health(&self) -> Result<()> {
        self.verify().map(|_| ())
    }
    fn shared_session(&self) -> Option<SharedSession> {
        Some(self.runtime.lock().info.clone())
    }
    fn prepare_control(&self) -> Result<Option<SharedSession>> {
        self.verify()?;
        anyhow::ensure!(
            hash_file(&self.executable)? == self.hash,
            "The game executable changed; start sharing again"
        );
        let mut runtime = self.runtime.lock();
        runtime.broker.take();
        runtime.info.generation = (uuid::Uuid::new_v4().as_u128() as u64).max(1);
        runtime.info.phase = Phase::Attaching;
        runtime.info.detail = format!(
            "{} · attaching input adapter",
            runtime.info.compatibility.status.label()
        );
        Ok(Some(runtime.info.clone()))
    }
    fn begin_control(&self) -> Result<()> {
        self.verify()?;
        let prepared = {
            let runtime = self.runtime.lock();
            runtime.info.phase == Phase::Attaching && runtime.info.generation != 0
        };
        if !prepared {
            self.prepare_control()?;
        }
        let mut runtime = self.runtime.lock();
        let generation = runtime.info.generation;
        let library = library()?;
        match Broker::attach(library, self.hwnd, generation) {
            Ok(broker) => {
                runtime.broker = Some(broker);
                runtime.info.generation = generation;
                runtime.info.phase = Phase::Paused;
                runtime.info.detail = format!(
                    "{} · click the viewer to control",
                    runtime.info.compatibility.status.label()
                );
                runtime.last_image = None;
                Ok(())
            }
            Err(e) => {
                runtime.info.phase = Phase::Error;
                runtime.info.detail = e.to_string();
                Err(e)
            }
        }
    }
    fn end_control(&self) {
        let mut runtime = self.runtime.lock();
        runtime.broker.take();
        runtime.info.phase = Phase::Paused;
        runtime.info.detail = "Viewer disconnected".into();
    }
    fn shared_resume(&self, generation: u64) -> Result<()> {
        self.verify()?;
        let mut runtime = self.runtime.lock();
        anyhow::ensure!(
            runtime.info.generation == generation,
            "stale shared session"
        );
        runtime
            .broker
            .as_mut()
            .context("shared game is not attached")?
            .send(Command {
                kind: 8,
                ..Default::default()
            })?;
        runtime.info.phase = Phase::Active;
        runtime.info.detail = format!(
            "{} \u{00b7} sharing one window",
            runtime.info.compatibility.status.label()
        );
        Ok(())
    }
    fn shared_heartbeat(&self, generation: u64) -> Result<()> {
        anyhow::ensure!(
            generation == self.runtime.lock().info.generation,
            "stale shared session"
        );
        self.send(
            generation,
            Command {
                kind: 7,
                ..Default::default()
            },
        )
    }
    fn shared_cursor(&self) -> Result<Option<Cursor>> {
        let hwnd = self.verify()?;
        let mut r = self.runtime.lock();
        let Some(b) = r.broker.as_mut() else {
            return Ok(None);
        };
        let status = b.poll()?;
        r.info.phase = match status.phase {
            1 => Phase::Active,
            2 => Phase::Paused,
            _ => Phase::Error,
        };
        if status.error != 0 {
            r.info.detail = format!(
                "Input paused (Windows error {}). Refocus the viewer to resume.",
                status.error
            );
        } else {
            r.info.detail = format!(
                "{} · {}",
                r.info.compatibility.status.label(),
                if r.info.phase == Phase::Active {
                    "sharing one window"
                } else {
                    "input paused · click the viewer to resume"
                }
            );
        }
        let len = (status.width * status.height * 4) as usize;
        anyhow::ensure!(len <= 65536, "invalid cursor image");
        let mut image_hash = Sha256::new();
        image_hash.update(status.width.to_le_bytes());
        image_hash.update(status.height.to_le_bytes());
        image_hash.update(&status.rgba[..len]);
        let hash: [u8; 32] = image_hash.finalize().into();
        let png_base64 = if len > 0 && r.last_image != Some(hash) {
            let mut out = IoCursor::new(Vec::new());
            image::RgbaImage::from_raw(status.width, status.height, status.rgba[..len].to_vec())
                .context("invalid cursor")?
                .write_to(&mut out, image::ImageFormat::Png)?;
            r.last_image = Some(hash);
            Some(base64::engine::general_purpose::STANDARD.encode(out.into_inner()))
        } else {
            None
        };
        unsafe {
            let _scope = WindowDpi::enter(hwnd);
            let mut bounds = RECT::default();
            GetClientRect(hwnd, &mut bounds)?;
            Ok(Some(Cursor {
                x: (status.x as f64 / (bounds.right - bounds.left).max(1) as f64).clamp(0., 1.),
                y: (status.y as f64 / (bounds.bottom - bounds.top).max(1) as f64).clamp(0., 1.),
                visible: status.visible != 0,
                hotspot_x: status.hot_x,
                hotspot_y: status.hot_y,
                png_base64,
            }))
        }
    }

    fn windows(&self) -> Result<Vec<RemoteWindow>> {
        let hwnd = self.verify()?;
        let mut remote = self.remote.lock();
        unsafe {
            let _dpi = PhysicalDpi::enter();
            remote.minimized = IsIconic(hwnd).as_bool();
            remote.visible = IsWindowVisible(hwnd).as_bool() && !remote.minimized;
            if !remote.minimized {
                let bounds = super::host::capture_bounds(hwnd)?;
                let scale = windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd).max(96) as f64 / 96.;
                remote.scale = scale;
                remote.bounds = Bounds {
                    x: bounds.left as f64 / scale,
                    y: bounds.top as f64 / scale,
                    width: (bounds.right - bounds.left) as f64 / scale,
                    height: (bounds.bottom - bounds.top) as f64 / scale,
                };
            }
        }
        remote.parent = None;
        remote.resize = ResizeCapability::Fixed;
        if remote.minimized && self.runtime.lock().info.phase == Phase::Active {
            self.release_input();
        }
        Ok(vec![remote.clone()])
    }
    fn desktop_windows(&self) -> Result<Vec<RemoteWindow>> {
        anyhow::bail!("Desktop sharing is unavailable in shared game mode")
    }
    fn allow_clipboard(&self) -> bool {
        false
    }
    fn allow_file_transfer(&self) -> bool {
        false
    }
    fn allow_device_bridge(&self) -> bool {
        false
    }
    fn action(&self, _: WindowId, _: &WindowAction) -> Result<()> {
        anyhow::bail!("Window commands are unavailable in shared game mode")
    }
    fn input(&self, _: WindowId, _: &InputEvent) -> Result<()> {
        anyhow::bail!("shared input requires a session generation")
    }
    fn shared_input(&self, key: WindowId, generation: u64, event: &InputEvent) -> Result<()> {
        anyhow::ensure!(
            key == self.key && event.valid(),
            "invalid shared input target or event"
        );
        let mut c = Command::default();
        match event {
            InputEvent::RelativeButton { button, down } => {
                c.kind = 4;
                c.a = i32::from(*button) + 1;
                c.b = i32::from(*down);
                c.reserved[1] = 1;
            }
            InputEvent::Key { usage, down } => {
                c.kind = 1;
                c.a = hid_pc_scan(*usage).context("unsupported physical key")? as i32;
                c.b = i32::from(*down);
            }
            InputEvent::Pointer { x, y } => {
                let p = self.point(*x, *y)?;
                c.kind = 2;
                c.a = p.x;
                c.b = p.y;
            }
            InputEvent::RelativeMotion { dx, dy } => {
                c.kind = 3;
                c.a = *dx;
                c.b = *dy;
            }
            InputEvent::Button { button, down, x, y } => {
                let p = self.point(*x, *y)?;
                c.kind = 4;
                c.a = i32::from(*button) + 1;
                c.b = i32::from(*down);
                c.c = p.x;
                c.d = p.y;
            }
            InputEvent::Scroll { dx, dy, .. } => {
                c.kind = 5;
                c.a = (*dx * 120.).clamp(-32767., 32767.) as i32;
                c.b = (-*dy * 120.).clamp(-32767., 32767.) as i32;
            }
            InputEvent::Text { text } => {
                for unit in text.encode_utf16() {
                    let mut runtime = self.runtime.lock();
                    anyhow::ensure!(
                        runtime.info.generation == generation,
                        "stale shared session"
                    );
                    runtime
                        .broker
                        .as_mut()
                        .context("shared game is not attached")?
                        .send(Command {
                            kind: 10,
                            a: i32::from(unit),
                            ..Default::default()
                        })?;
                }
                return Ok(());
            }
        }
        self.verify()?;
        let mut runtime = self.runtime.lock();
        anyhow::ensure!(
            runtime.info.generation == generation,
            "stale shared session"
        );
        runtime
            .broker
            .as_mut()
            .context("shared game is not attached")?
            .send(c)
    }
    fn shared_release(&self, generation: u64) {
        let mut runtime = self.runtime.lock();
        if runtime.info.generation != generation {
            return;
        }
        if let Some(b) = runtime.broker.as_mut() {
            let _ = b.send(Command {
                kind: 6,
                ..Default::default()
            });
        }
        runtime.info.phase = Phase::Paused;
    }
    fn release_input(&self) {
        let mut runtime = self.runtime.lock();
        if let Some(b) = runtime.broker.as_mut() {
            let _ = b.send(Command {
                kind: 6,
                ..Default::default()
            });
        }
        runtime.info.phase = Phase::Paused;
    }
    fn capture(&self, key: WindowId, _: bool) -> Result<CaptureStream> {
        anyhow::ensure!(key == self.key, "invalid shared capture target");
        self.verify()?;
        self.host.capture(key, false)
    }
    fn capture_for_encoding(
        &self,
        key: WindowId,
        _: bool,
        hardware: bool,
    ) -> Result<CaptureStream> {
        anyhow::ensure!(key == self.key, "invalid shared capture target");
        self.verify()?;
        self.host.capture_for_encoding(key, false, hardware)
    }
    fn clipboard(&self) -> Result<Option<String>> {
        Ok(None)
    }
    fn set_clipboard(&self, _: &str) -> Result<()> {
        anyhow::bail!("Clipboard unavailable in shared game mode")
    }
    fn audio(&self) -> Result<crate::audio::AudioCapture> {
        super::process_audio::start(self.pid)
    }
}
impl Drop for SharedHost {
    fn drop(&mut self) {
        self.end_control();
        unsafe {
            let hwnd = HWND(self.hwnd as *mut c_void);
            if GetPropW(hwnd, PCWSTR(self.property.as_ptr())).0 as usize == self.property_token {
                let _ = RemovePropW(hwnd, PCWSTR(self.property.as_ptr()));
            }
        }
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    struct Fixture {
        child: std::process::Child,
        output: BufReader<std::process::ChildStdout>,
    }
    impl Fixture {
        fn read(&mut self) -> Result<serde_json::Value> {
            let mut line = String::new();
            anyhow::ensure!(self.output.read_line(&mut line)? > 0, "fixture ended");
            Ok(serde_json::from_str(&line)?)
        }
        fn command(&mut self, command: &str) -> Result<serde_json::Value> {
            writeln!(self.child.stdin.as_mut().unwrap(), "{command}")?;
            self.read()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
    #[test]
    #[ignore = "opens an owned nonactivating fixture; requires an interactive Windows desktop"]
    fn selected_game_minimize_reconnect_and_window_lifetime() -> Result<()> {
        use std::os::windows::process::CommandExt;
        let executable = std::path::Path::new(env!("FRAMES_INPUT_ARTIFACT"))
            .with_file_name("shared_input_fixture.exe");
        let mut child = std::process::Command::new(executable)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .creation_flags(0x08000000)
            .spawn()?;
        let output = BufReader::new(child.stdout.take().unwrap());
        let mut fixture = Fixture { child, output };
        let hwnd = fixture.read()?["hwnd"].as_u64().context("fixture handle")?;
        let desktop = create(vec![], Some(hwnd))?;
        let preparing = desktop.prepare_control()?.unwrap();
        assert_eq!(preparing.phase, Phase::Attaching);
        desktop.begin_control()?;
        let session = desktop.shared_session().unwrap();
        let key = InputEvent::Key {
            usage: 0x1a,
            down: true,
        };
        let pause = || std::thread::sleep(std::time::Duration::from_millis(100));
        desktop.shared_resume(session.generation)?;
        desktop.shared_input(session.window, session.generation, &key)?;
        pause();
        assert_eq!(fixture.command("report")?["w_async"], 1);
        fixture.command("minimize")?;
        pause();
        assert!(desktop.windows()?[0].minimized);
        desktop.shared_heartbeat(session.generation)?;
        pause();
        desktop.shared_cursor()?;
        assert_eq!(desktop.shared_session().unwrap().phase, Phase::Paused);
        assert_eq!(fixture.command("report")?["w_async"], 0);
        fixture.command("restore")?;
        pause();
        assert!(!desktop.windows()?[0].minimized);
        desktop.shared_heartbeat(session.generation)?;
        pause();
        desktop.shared_cursor()?;
        assert_eq!(desktop.shared_session().unwrap().phase, Phase::Paused);
        desktop.end_control();
        pause();
        assert_eq!(fixture.command("report")?["registrations"], 2);
        desktop.prepare_control()?;
        desktop.begin_control()?;
        let next = desktop.shared_session().unwrap();
        assert_ne!(session.generation, next.generation);
        desktop.shared_resume(next.generation)?;
        desktop.shared_input(next.window, next.generation, &key)?;
        assert!(
            desktop
                .shared_input(session.window, session.generation, &key)
                .is_err()
        );
        assert!(desktop.shared_resume(session.generation).is_err());
        desktop.shared_release(session.generation);
        pause();
        assert_eq!(fixture.command("report")?["w_async"], 1);
        fixture.command("destroy")?;
        pause();
        assert!(desktop.check_health().is_err());
        assert!(desktop.windows().is_err());
        assert!(
            desktop
                .shared_input(next.window, next.generation, &key)
                .is_err()
        );
        desktop.end_control();
        Ok(())
    }
}
