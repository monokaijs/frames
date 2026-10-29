#![allow(unsafe_op_in_unsafe_fn)]
use super::wide;
use crate::{CaptureStream, Desktop};
use base64::Engine;
use frames_core::{
    input::{InputEvent, PressedInput},
    model::*,
};
use parking_lot::Mutex;
use std::{collections::HashMap, io::Cursor};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::{DataExchange::*, Memory::*, Threading::*},
        UI::{HiDpi::GetDpiForWindow, Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
    },
    core::*,
};

// WGC/DWM rectangles are physical pixels, including on mixed-DPI desktops.
// Tokio workers may have a different inherited thread DPI context from the UI.
pub(super) struct PhysicalDpi(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT);
impl PhysicalDpi {
    pub(super) unsafe fn enter() -> Self {
        use windows::Win32::UI::HiDpi::*;
        Self(unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) })
    }
}
impl Drop for PhysicalDpi {
    fn drop(&mut self) {
        if !self.0.0.is_null() {
            unsafe {
                windows::Win32::UI::HiDpi::SetThreadDpiAwarenessContext(self.0);
            }
        }
    }
}
fn physical_point(rect: RECT, x: f64, y: f64) -> anyhow::Result<POINT> {
    anyhow::ensure!(
        x.is_finite() && y.is_finite() && rect.right > rect.left && rect.bottom > rect.top,
        "invalid pointer coordinates or capture bounds"
    );
    Ok(POINT {
        x: rect.left + ((rect.right - rect.left - 1) as f64 * x.clamp(0., 1.)).round() as i32,
        y: rect.top + ((rect.bottom - rect.top - 1) as f64 * y.clamp(0., 1.)).round() as i32,
    })
}

pub struct Host {
    registry: Mutex<WindowRegistry>,
    known: Mutex<HashMap<WindowId, RemoteWindow>>,
    exclusions: Vec<String>,
    pressed: Mutex<PressedInput>,
    icons: Mutex<HashMap<String, Option<String>>>,
    menus: super::menus::Projection,
}
impl Host {
    pub fn new(exclusions: Vec<String>) -> anyhow::Result<Self> {
        unsafe {
            let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
                windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            );
        }
        Ok(Self {
            registry: Mutex::new(WindowRegistry::default()),
            known: Mutex::new(HashMap::new()),
            exclusions: exclusions.into_iter().map(|s| s.to_lowercase()).collect(),
            pressed: Mutex::new(PressedInput::default()),
            icons: Mutex::new(HashMap::new()),
            menus: Default::default(),
        })
    }
    fn hwnd(&self, key: WindowId) -> anyhow::Result<HWND> {
        self.registry
            .lock()
            .handle(key)
            .map(|h| HWND(h as *mut _))
            .ok_or_else(|| anyhow::anyhow!("window no longer exists"))
    }
}

struct Candidate {
    handle: u64,
    pid: u32,
    path: String,
    title: String,
    class: String,
    rect: RECT,
    owner: u64,
    resize: bool,
    minimized: bool,
    scale: f64,
}
unsafe extern "system" fn enumerate(hwnd: HWND, param: LPARAM) -> BOOL {
    let list = &mut *(param.0 as *mut Vec<Candidate>);
    let mut pid = 0;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == std::process::id() || !IsWindowVisible(hwnd).as_bool() {
        return TRUE;
    }
    let mut class = [0u16; 256];
    let n = GetClassNameW(hwnd, &mut class);
    let class = String::from_utf16_lossy(&class[..n.max(0) as usize]);
    if matches!(
        class.as_str(),
        "Progman"
            | "WorkerW"
            | "Shell_TrayWnd"
            | "Shell_SecondaryTrayWnd"
            | "TaskManagerWindow"
            | "FramesProxy"
            | "FramesLauncher"
            | "FramesSessionControl"
            | "FramesFileShelf"
            | "FramesFilePrompt"
    ) {
        return TRUE;
    }
    let mut title = vec![0; GetWindowTextLengthW(hwnd).clamp(0, 4096) as usize + 1];
    let n = GetWindowTextW(hwnd, &mut title);
    let title = String::from_utf16_lossy(&title[..n.max(0) as usize]);
    let owner = GetWindow(hwnd, GW_OWNER).unwrap_or_default().0 as u64;
    if title.is_empty() && owner == 0 && class != "#32768" {
        return TRUE;
    }
    let Ok(rect) = capture_bounds(hwnd) else {
        return TRUE;
    };
    if rect.right <= rect.left || rect.bottom <= rect.top {
        return TRUE;
    }
    let mut path = String::new();
    if let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
        let mut buffer = vec![0u16; 32768];
        let mut size = buffer.len() as u32;
        if QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut size,
        )
        .is_ok()
        {
            path = String::from_utf16_lossy(&buffer[..size as usize]);
        }
        let _ = CloseHandle(process);
    }
    if path.is_empty() {
        path = format!("pid:{pid}");
    }
    let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
    list.push(Candidate {
        handle: hwnd.0 as u64,
        pid,
        path,
        title,
        class,
        rect,
        owner,
        resize: style & WS_THICKFRAME.0 != 0,
        minimized: IsIconic(hwnd).as_bool(),
        scale: GetDpiForWindow(hwnd) as f64 / 96.0,
    });
    TRUE
}

impl Desktop for Host {
    fn menu(&self, key: WindowId) -> anyhow::Result<Vec<MenuItem>> {
        self.menus.read(key, self.hwnd(key)?)
    }
    fn allow_device_bridge(&self) -> bool {
        !super::secure::enabled()
    }
    fn desktop_windows(&self) -> anyhow::Result<Vec<RemoteWindow>> {
        if super::secure::active() {
            return self.windows();
        }
        let key = self
            .registry
            .lock()
            .reconcile(&[(super::capture::DESKTOP_HANDLE, 0)])[&super::capture::DESKTOP_HANDLE];
        let rect = unsafe { primary_bounds()? };
        let window = RemoteWindow {
            key,
            app_id: "app.frames.desktop-view".into(),
            app_name: "Remote desktop".into(),
            title: "Remote desktop — primary display".into(),
            icon_png: None,
            parent: None,
            role: WindowRole::Desktop,
            bounds: Bounds {
                x: rect.left as f64,
                y: rect.top as f64,
                width: (rect.right - rect.left) as f64,
                height: (rect.bottom - rect.top) as f64,
            },
            scale: 1.,
            resize: ResizeCapability::Fixed,
            minimized: false,
            visible: true,
            modal: false,
        };
        *self.known.lock() = HashMap::from([(key, window.clone())]);
        Ok(vec![window])
    }
    fn allow_file_transfer(&self) -> bool {
        !super::secure::enabled()
    }
    fn drop_files(
        &self,
        key: WindowId,
        paths: &[std::path::PathBuf],
        x: f64,
        y: f64,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.allow_file_transfer() && x.is_finite() && y.is_finite(),
            "file drop unavailable"
        );
        let hwnd = self.hwnd(key)?;
        let rect = unsafe { capture_bounds(hwnd)? };
        let point = physical_point(rect, x, y)?;
        for path in paths {
            super::drag::drop_file(path, hwnd, point)?;
        }
        Ok(())
    }
    fn windows(&self) -> anyhow::Result<Vec<RemoteWindow>> {
        unsafe {
            if super::secure::active() {
                let handles = if super::secure::enabled() {
                    vec![(super::secure::HANDLE, 0)]
                } else {
                    vec![]
                };
                let keys = self.registry.lock().reconcile(&handles);
                let windows = keys
                    .get(&super::secure::HANDLE)
                    .map(|&key| vec![super::secure::window(key)])
                    .unwrap_or_default();
                *self.known.lock() = windows.iter().map(|w| (w.key, w.clone())).collect();
                return Ok(windows);
            }
            let mut candidates = Vec::<Candidate>::new();
            EnumWindows(Some(enumerate), LPARAM(&mut candidates as *mut _ as isize))?;
            candidates.retain(|w| {
                !self
                    .exclusions
                    .iter()
                    .any(|ex| w.path.to_lowercase().contains(ex))
            });
            let keys = self.registry.lock().reconcile(
                &candidates
                    .iter()
                    .map(|w| (w.handle, w.pid as u64))
                    .collect::<Vec<_>>(),
            );
            let mut icons = self.icons.lock();
            let windows: Vec<_> = candidates
                .into_iter()
                .filter_map(|w| {
                    let icon = icons
                        .entry(w.path.clone())
                        .or_insert_with(|| icon_png(HWND(w.handle as *mut _)))
                        .clone();
                    let scale = w.scale.max(1.0);
                    let rect = w.rect;
                    let role = match w.class.as_str() {
                        "#32768" => WindowRole::Menu,
                        "tooltips_class32" => WindowRole::Tooltip,
                        "#32770" => WindowRole::Dialog,
                        _ => {
                            if w.owner != 0 {
                                WindowRole::Popover
                            } else {
                                WindowRole::Window
                            }
                        }
                    };
                    let window = RemoteWindow {
                        key: keys[&w.handle],
                        app_name: std::path::Path::new(&w.path)
                            .file_stem()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into(),
                        app_id: w.path,
                        title: w.title,
                        icon_png: icon,
                        parent: keys.get(&w.owner).copied(),
                        role,
                        bounds: Bounds {
                            x: rect.left as f64 / scale,
                            y: rect.top as f64 / scale,
                            width: (rect.right - rect.left) as f64 / scale,
                            height: (rect.bottom - rect.top) as f64 / scale,
                        },
                        scale,
                        resize: if w.resize {
                            ResizeCapability::Supported
                        } else {
                            ResizeCapability::Fixed
                        },
                        minimized: w.minimized,
                        visible: !w.minimized,
                        modal: w.owner != 0 && !IsWindowEnabled(HWND(w.owner as *mut _)).as_bool(),
                    };
                    window.validate().then_some(window)
                })
                .collect();
            *self.known.lock() = windows.iter().map(|w| (w.key, w.clone())).collect();
            Ok(windows)
        }
    }
    fn action(&self, key: WindowId, action: &WindowAction) -> anyhow::Result<()> {
        unsafe {
            let _dpi = PhysicalDpi::enter();
            if matches!(
                self.registry.lock().handle(key),
                Some(super::secure::HANDLE | super::capture::DESKTOP_HANDLE)
            ) {
                return match action {
                    WindowAction::Focus | WindowAction::Restore => Ok(()),
                    _ => Err(anyhow::anyhow!(
                        "desktop surfaces do not support application window commands"
                    )),
                };
            }
            let hwnd = self.hwnd(key)?;
            match action {
                WindowAction::Focus => {
                    if IsIconic(hwnd).as_bool() {
                        let _ = ShowWindow(hwnd, SW_RESTORE);
                    }
                    anyhow::ensure!(
                        SetForegroundWindow(hwnd).as_bool() || GetForegroundWindow() == hwnd,
                        "host OS denied focus activation"
                    );
                }
                WindowAction::Restore => {
                    let _ = ShowWindow(hwnd, SW_RESTORE);
                }
                WindowAction::Close => {
                    PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0))?;
                }
                WindowAction::Resize { width, height } => {
                    anyhow::ensure!(
                        width.is_finite()
                            && height.is_finite()
                            && *width >= 32.0
                            && *height >= 32.0
                            && *width <= 16384.0
                            && *height <= 16384.0,
                        "invalid window size"
                    );
                    anyhow::ensure!(
                        GetWindowLongPtrW(hwnd, GWL_STYLE) as u32 & WS_THICKFRAME.0 != 0,
                        "window is not resizable"
                    );
                    let scale = GetDpiForWindow(hwnd).max(96) as f64 / 96.;
                    let visible = capture_bounds(hwnd)?;
                    let mut outer = RECT::default();
                    GetWindowRect(hwnd, &mut outer)?;
                    let mut limits = MINMAXINFO::default();
                    let _ = SendMessageTimeoutW(
                        hwnd,
                        WM_GETMINMAXINFO,
                        WPARAM(0),
                        LPARAM(&mut limits as *mut _ as isize),
                        SMTO_ABORTIFHUNG,
                        10,
                        None,
                    );
                    let requested_width = (*width * scale) as i32 + (outer.right - outer.left)
                        - (visible.right - visible.left);
                    let requested_height = (*height * scale) as i32 + (outer.bottom - outer.top)
                        - (visible.bottom - visible.top);
                    let clamp = |value: i32, min: i32, max: i32| {
                        value.max(min.max(1)).min(if max > 0 { max } else { 32768 })
                    };
                    anyhow::ensure!(
                        !(limits.ptMinTrackSize.x > 0
                            && limits.ptMinTrackSize == limits.ptMaxTrackSize),
                        "window has a fixed size"
                    );
                    SetWindowPos(
                        hwnd,
                        None,
                        0,
                        0,
                        clamp(
                            requested_width,
                            limits.ptMinTrackSize.x,
                            limits.ptMaxTrackSize.x,
                        ),
                        clamp(
                            requested_height,
                            limits.ptMinTrackSize.y,
                            limits.ptMaxTrackSize.y,
                        ),
                        SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                    )?;
                }
                WindowAction::Menu => anyhow::bail!("use the captured application's menu"),
                WindowAction::MenuItem { id } => self.menus.invoke(key, hwnd, id)?,
            }
            Ok(())
        }
    }
    fn input(&self, window_key: WindowId, event: &InputEvent) -> anyhow::Result<()> {
        unsafe {
            let _dpi = PhysicalDpi::enter();
            let secure = self.registry.lock().handle(window_key) == Some(super::secure::HANDLE);
            let desktop =
                self.registry.lock().handle(window_key) == Some(super::capture::DESKTOP_HANDLE);
            let _scope = if secure {
                Some(super::secure::DesktopScope::enter()?)
            } else {
                None
            };
            let hwnd = if secure || desktop {
                HWND::default()
            } else {
                self.hwnd(window_key)?
            };
            let mut rect = RECT::default();
            if desktop {
                rect = primary_bounds()?;
            } else if secure {
                rect.right = GetSystemMetrics(SM_CXSCREEN);
                rect.bottom = GetSystemMetrics(SM_CYSCREEN);
            } else {
                rect = capture_bounds(hwnd)?;
                if matches!(event, InputEvent::Key { .. } | InputEvent::Text { .. })
                    && GetForegroundWindow() != hwnd
                {
                    let _ = SetForegroundWindow(hwnd);
                    anyhow::ensure!(
                        GetForegroundWindow() == hwnd,
                        "host OS denied keyboard focus; click the remote application first"
                    );
                }
            }
            let position = |x: f64, y: f64| -> anyhow::Result<()> {
                let point = physical_point(rect, x, y)?;
                SetPhysicalCursorPos(point.x, point.y)?;
                Ok(())
            };
            let check_target = || -> anyhow::Result<()> {
                if !desktop && !secure {
                    let mut point = POINT::default();
                    GetPhysicalCursorPos(&mut point)?;
                    let hit = WindowFromPhysicalPoint(point);
                    let correct = GetAncestor(hit, GA_ROOT) == GetAncestor(hwnd, GA_ROOT);
                    if std::env::var_os("FRAMES_INPUT_DIAGNOSTICS").is_some() {
                        tracing::info!(
                            window = window_key.id,
                            left = rect.left,
                            top = rect.top,
                            width = rect.right - rect.left,
                            height = rect.bottom - rect.top,
                            screen_x = point.x,
                            screen_y = point.y,
                            target_matches = correct,
                            "physical input target"
                        );
                    }
                    anyhow::ensure!(
                        correct,
                        "Input blocked: the host window is covered or inactive; focus it on the host or use full desktop view"
                    );
                }
                Ok(())
            };
            let mut inputs = vec![];
            match event {
                InputEvent::Pointer { x, y } => position(*x, *y)?,
                InputEvent::Button { button, down, x, y } => {
                    if !*down && !self.pressed.lock().button_is_down(*button) {
                        return Ok(());
                    }
                    if *down && !desktop && !secure {
                        // Raising without activation is allowed even when the
                        // foreground lock refuses keyboard activation. The real
                        // click can then activate this window, rather than land
                        // on an unrelated window covering the capture surface.
                        let _ = SetWindowPos(
                            hwnd,
                            Some(HWND_TOP),
                            0,
                            0,
                            0,
                            0,
                            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                        );
                        let _ = SetForegroundWindow(hwnd);
                    }
                    position(*x, *y)?;
                    if *down {
                        check_target()?;
                    }
                    let flags = match (*button, *down) {
                        (0, true) => MOUSEEVENTF_LEFTDOWN,
                        (0, false) => MOUSEEVENTF_LEFTUP,
                        (1, true) => MOUSEEVENTF_RIGHTDOWN,
                        (1, false) => MOUSEEVENTF_RIGHTUP,
                        (2, true) => MOUSEEVENTF_MIDDLEDOWN,
                        (2, false) => MOUSEEVENTF_MIDDLEUP,
                        _ => anyhow::bail!("unsupported button"),
                    };
                    inputs.push(mouse(flags, 0));
                }
                InputEvent::Scroll { dx, dy, x, y } => {
                    position(*x, *y)?;
                    check_target()?;
                    if *dy != 0.0 {
                        inputs.push(mouse(MOUSEEVENTF_WHEEL, (-dy * 120.0) as i32 as u32));
                    }
                    if *dx != 0.0 {
                        inputs.push(mouse(MOUSEEVENTF_HWHEEL, (*dx * 120.0) as i32 as u32));
                    }
                }
                InputEvent::Key { usage, down } => {
                    if let Some(input) = physical_key(*usage, *down) {
                        inputs.push(input);
                    }
                }
                InputEvent::Text { text } => {
                    anyhow::ensure!(text.len() <= 65536, "text event too large");
                    for unit in text.encode_utf16() {
                        inputs.push(key(0, unit, KEYEVENTF_UNICODE));
                        inputs.push(key(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
                    }
                }
            }
            if !inputs.is_empty() {
                anyhow::ensure!(
                    SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) == inputs.len() as u32,
                    "input denied (possibly secure desktop or elevation)"
                );
            }
            self.pressed.lock().track(event);
            Ok(())
        }
    }
    fn release_input(&self) {
        unsafe {
            for event in self.pressed.lock().release_all() {
                match event {
                    InputEvent::Key { usage, .. } => {
                        if let Some(input) = physical_key(usage, false) {
                            let _ = SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
                        }
                    }
                    InputEvent::Button { button, .. } => {
                        let flag = match button {
                            0 => MOUSEEVENTF_LEFTUP,
                            1 => MOUSEEVENTF_RIGHTUP,
                            _ => MOUSEEVENTF_MIDDLEUP,
                        };
                        let _ = SendInput(&[mouse(flag, 0)], std::mem::size_of::<INPUT>() as i32);
                    }
                    _ => {}
                }
            }
        }
    }
    fn capture(&self, key: WindowId) -> anyhow::Result<CaptureStream> {
        if self.registry.lock().handle(key) == Some(super::secure::HANDLE) {
            super::secure::capture()
        } else if self.registry.lock().handle(key) == Some(super::capture::DESKTOP_HANDLE) {
            super::capture::start(super::capture::DESKTOP_HANDLE)
        } else {
            super::capture::start(self.hwnd(key)?.0 as u64)
        }
    }
    fn clipboard(&self) -> anyhow::Result<Option<String>> {
        unsafe {
            if OpenClipboard(None).is_err() {
                return Ok(None);
            }
            let result = (|| -> anyhow::Result<_> {
                let handle = GetClipboardData(13)?;
                let global = HGLOBAL(handle.0);
                let len = GlobalSize(global) / 2;
                anyhow::ensure!(len <= 2 * 1024 * 1024, "clipboard too large");
                let ptr = GlobalLock(global) as *const u16;
                anyhow::ensure!(!ptr.is_null(), "clipboard lock failed");
                let slice = std::slice::from_raw_parts(ptr, len);
                let count = slice.iter().position(|&v| v == 0).unwrap_or(len);
                let text = String::from_utf16_lossy(&slice[..count]);
                let _ = GlobalUnlock(global);
                Ok(Some(text))
            })();
            let _ = CloseClipboard();
            result
        }
    }
    fn set_clipboard(&self, text: &str) -> anyhow::Result<()> {
        unsafe {
            anyhow::ensure!(text.len() <= 2 * 1024 * 1024, "clipboard too large");
            let _owner = ClipboardOwner::new()?;
            EmptyClipboard()?;
            let bytes = wide(text);
            write_clipboard_bytes(
                13,
                std::slice::from_raw_parts(bytes.as_ptr().cast(), bytes.len() * 2),
            )?;
            Ok(())
        }
    }
    fn clipboard_image(&self) -> anyhow::Result<Option<Vec<u8>>> {
        unsafe {
            if OpenClipboard(None).is_err() {
                return Ok(None);
            }
            let result = (|| -> anyhow::Result<Option<Vec<u8>>> {
                let png = RegisterClipboardFormatW(w!("PNG"));
                if let Ok(handle) = GetClipboardData(png) {
                    return Ok(Some(clipboard_bytes(handle)?));
                }
                for format in [17, 8] {
                    if let Ok(handle) = GetClipboardData(format) {
                        let bytes = clipboard_bytes(handle)?;
                        if bytes.len() < 40 {
                            continue;
                        }
                        let header =
                            std::ptr::read_unaligned(bytes.as_ptr() as *const BITMAPINFOHEADER);
                        let w = header.biWidth;
                        let h = header.biHeight;
                        if w <= 0
                            || h == 0
                            || w > 8192
                            || h.unsigned_abs() > 8192
                            || !matches!(header.biBitCount, 24 | 32)
                            || !matches!(header.biCompression, 0 | 3)
                        {
                            continue;
                        }
                        let mask = if header.biSize == 40 && header.biCompression == 3 {
                            12
                        } else {
                            0
                        };
                        let offset = header.biSize as usize + mask;
                        let stride = (w as usize * header.biBitCount as usize).div_ceil(32) * 4;
                        let height = h.unsigned_abs() as usize;
                        if offset + stride * height > bytes.len() {
                            continue;
                        }
                        let mut rgba = vec![0; w as usize * height * 4];
                        for row in 0..height {
                            let source = if h > 0 { height - 1 - row } else { row };
                            for col in 0..w as usize {
                                let p = offset
                                    + source * stride
                                    + col * (header.biBitCount as usize / 8);
                                let target = (row * w as usize + col) * 4;
                                rgba[target..target + 4].copy_from_slice(&[
                                    bytes[p + 2],
                                    bytes[p + 1],
                                    bytes[p],
                                    255,
                                ]);
                            }
                        }
                        let image =
                            image::RgbaImage::from_raw(w as u32, height as u32, rgba).unwrap();
                        let mut out = Cursor::new(Vec::new());
                        image.write_to(&mut out, image::ImageFormat::Png)?;
                        return Ok(Some(out.into_inner()));
                    }
                }
                Ok(None)
            })();
            let _ = CloseClipboard();
            result
        }
    }
    fn set_clipboard_image(&self, png: &[u8]) -> anyhow::Result<()> {
        unsafe {
            anyhow::ensure!(png.len() <= 2 * 1024 * 1024, "clipboard image too large");
            let image = super::decode_png(png, 8192, 256 * 1024 * 1024)?.to_rgba8();
            let (w, h) = image.dimensions();
            anyhow::ensure!(w <= 8192 && h <= 8192, "clipboard dimensions too large");
            let header = BITMAPINFOHEADER {
                biSize: 40,
                biWidth: w as i32,
                biHeight: -(h as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            };
            let mut dib = std::slice::from_raw_parts(&header as *const _ as *const u8, 40).to_vec();
            let mut bgra = image.into_raw();
            for pixel in bgra.chunks_mut(4) {
                pixel.swap(0, 2);
            }
            dib.extend(bgra);
            let _owner = ClipboardOwner::new()?;
            EmptyClipboard()?;
            write_clipboard_bytes(RegisterClipboardFormatW(w!("PNG")), png)?;
            write_clipboard_bytes(8, &dib)?;
            Ok(())
        }
    }
}

/// Physical screen rectangle of the app's client area. Standard non-client
/// borders, title bars and menu bars are represented by the native proxy UI.
pub(super) unsafe fn capture_bounds(hwnd: HWND) -> anyhow::Result<RECT> {
    let _dpi = PhysicalDpi::enter();
    let mut rect = RECT::default();
    GetClientRect(hwnd, &mut rect)?;
    let mut origin = POINT::default();
    anyhow::ensure!(
        ClientToScreen(hwnd, &mut origin).as_bool(),
        "client origin unavailable"
    );
    rect.left += origin.x;
    rect.right += origin.x;
    rect.top += origin.y;
    rect.bottom += origin.y;
    Ok(rect)
}

pub(super) unsafe fn frame_bounds(hwnd: HWND) -> anyhow::Result<RECT> {
    let _dpi = PhysicalDpi::enter();
    let mut rect = RECT::default();
    if windows::Win32::Graphics::Dwm::DwmGetWindowAttribute(
        hwnd,
        windows::Win32::Graphics::Dwm::DWMWA_EXTENDED_FRAME_BOUNDS,
        &mut rect as *mut _ as *mut _,
        std::mem::size_of::<RECT>() as u32,
    )
    .is_err()
    {
        GetWindowRect(hwnd, &mut rect)?;
    }
    Ok(rect)
}

unsafe fn primary_bounds() -> anyhow::Result<RECT> {
    let monitor = MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY);
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    anyhow::ensure!(
        GetMonitorInfoW(monitor, &mut info).as_bool(),
        "primary display unavailable"
    );
    Ok(info.rcMonitor)
}

struct ClipboardOwner(HWND);
impl ClipboardOwner {
    unsafe fn new() -> anyhow::Result<Self> {
        unsafe {
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                w!("STATIC"),
                w!("Frames clipboard"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                None,
                None,
            )?;
            if let Err(error) = OpenClipboard(Some(hwnd)) {
                let _ = DestroyWindow(hwnd);
                return Err(error.into());
            }
            Ok(Self(hwnd))
        }
    }
}
impl Drop for ClipboardOwner {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
            let _ = DestroyWindow(self.0);
        }
    }
}

unsafe fn clipboard_bytes(handle: HANDLE) -> anyhow::Result<Vec<u8>> {
    let global = HGLOBAL(handle.0);
    let size = GlobalSize(global);
    anyhow::ensure!(size <= 128 * 1024 * 1024, "clipboard data too large");
    let pointer = GlobalLock(global) as *const u8;
    anyhow::ensure!(!pointer.is_null(), "clipboard lock failed");
    let bytes = std::slice::from_raw_parts(pointer, size).to_vec();
    let _ = GlobalUnlock(global);
    Ok(bytes)
}
unsafe fn write_clipboard_bytes(format: u32, bytes: &[u8]) -> anyhow::Result<()> {
    let handle = GlobalAlloc(GMEM_MOVEABLE, bytes.len())?;
    let pointer = GlobalLock(handle) as *mut u8;
    if pointer.is_null() {
        let _ = GlobalFree(Some(handle));
        anyhow::bail!("clipboard allocation failed");
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer, bytes.len());
    let _ = GlobalUnlock(handle);
    if let Err(error) = SetClipboardData(format, Some(HANDLE(handle.0))) {
        let _ = GlobalFree(Some(handle));
        return Err(error.into());
    }
    Ok(())
}

fn mouse(flags: MOUSE_EVENT_FLAGS, data: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dwFlags: flags,
                mouseData: data,
                ..Default::default()
            },
        },
    }
}
fn key(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: scan,
                dwFlags: flags,
                ..Default::default()
            },
        },
    }
}

fn physical_key(usage: u16, down: bool) -> Option<INPUT> {
    let release = if down {
        KEYBD_EVENT_FLAGS(0)
    } else {
        KEYEVENTF_KEYUP
    };
    if let Some(scan) = frames_core::input::hid_pc_scan(usage) {
        let extended = if scan >> 8 == 0xe0 {
            KEYEVENTF_EXTENDEDKEY
        } else {
            KEYBD_EVENT_FLAGS(0)
        };
        Some(key(0, scan & 0xff, KEYEVENTF_SCANCODE | extended | release))
    } else {
        usage_vk(usage).map(|vk| key(vk, 0, release))
    }
}

pub fn usage_vk(usage: u16) -> Option<u16> {
    Some(match usage {
        4..=29 => 0x41 + usage - 4,
        30..=38 => 0x31 + usage - 30,
        39 => 0x30,
        40 => 0x0d,
        41 => 0x1b,
        42 => 8,
        43 => 9,
        44 => 0x20,
        45 => 0xbd,
        46 => 0xbb,
        47 => 0xdb,
        48 => 0xdd,
        49 => 0xdc,
        51 => 0xba,
        52 => 0xde,
        53 => 0xc0,
        54 => 0xbc,
        55 => 0xbe,
        56 => 0xbf,
        57 => 0x14,
        58..=69 => 0x70 + usage - 58,
        70 => 0x2c,
        71 => 0x91,
        72 => 0x13,
        73 => 0x2d,
        74 => 0x24,
        75 => 0x21,
        76 => 0x2e,
        77 => 0x23,
        78 => 0x22,
        79 => 0x27,
        80 => 0x25,
        81 => 0x28,
        82 => 0x26,
        0xe0 => 0xa2,
        0xe1 => 0xa0,
        0xe2 => 0xa4,
        0xe3 => 0x5b,
        0xe4 => 0xa3,
        0xe5 => 0xa1,
        0xe6 => 0xa5,
        0xe7 => 0x5c,
        _ => return None,
    })
}
pub fn vk_usage(vk: u16) -> Option<u16> {
    (4..=0xe7).find(|&u| usage_vk(u) == Some(vk))
}

unsafe fn icon_png(hwnd: HWND) -> Option<String> {
    let mut result = 0usize;
    let _ = SendMessageTimeoutW(
        hwnd,
        WM_GETICON,
        WPARAM(ICON_BIG as usize),
        LPARAM(0),
        SMTO_ABORTIFHUNG,
        50,
        Some(&mut result),
    );
    let icon = HICON(if result != 0 {
        result as *mut _
    } else {
        GetClassLongPtrW(hwnd, GCLP_HICON) as *mut _
    });
    if icon.is_invalid() {
        return None;
    }
    let mut info = ICONINFO::default();
    if GetIconInfo(icon, &mut info).is_err() {
        return None;
    }
    let output = (|| -> Option<String> {
        let mut bitmap = BITMAP::default();
        if GetObjectW(
            info.hbmColor.into(),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bitmap as *mut _ as *mut _),
        ) == 0
        {
            return None;
        }
        let w = bitmap.bmWidth as u32;
        let h = bitmap.bmHeight as u32;
        if w == 0 || h == 0 || w > 256 || h > 256 {
            return None;
        }
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let dc = GetDC(None);
        let ok = GetDIBits(
            dc,
            info.hbmColor,
            0,
            h,
            Some(pixels.as_mut_ptr().cast()),
            &mut bmi,
            DIB_RGB_COLORS,
        );
        ReleaseDC(None, dc);
        if ok == 0 {
            return None;
        }
        let alpha = pixels.chunks(4).any(|p| p[3] != 0);
        for p in pixels.chunks_mut(4) {
            p.swap(0, 2);
            if !alpha {
                p[3] = 255;
            }
        }
        let image = image::RgbaImage::from_raw(w, h, pixels)?;
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, image::ImageFormat::Png).ok()?;
        Some(base64::engine::general_purpose::STANDARD.encode(bytes.into_inner()))
    })();
    if !info.hbmColor.is_invalid() {
        let _ = DeleteObject(info.hbmColor.into());
    }
    if !info.hbmMask.is_invalid() {
        let _ = DeleteObject(info.hbmMask.into());
    }
    output
}

#[cfg(test)]
mod coordinate_tests {
    use super::*;
    #[test]
    fn physical_coordinates_support_negative_origins_and_stay_inside_edges() {
        let rect = RECT {
            left: -1920,
            top: -200,
            right: -920,
            bottom: 600,
        };
        let p = physical_point(rect, 0., 0.).unwrap();
        assert_eq!((p.x, p.y), (-1920, -200));
        let p = physical_point(rect, 1., 1.).unwrap();
        assert_eq!((p.x, p.y), (-921, 599));
        let p = physical_point(rect, 0.5, 0.5).unwrap();
        assert_eq!((p.x, p.y), (-1420, 200));
        assert!(physical_point(rect, f64::NAN, 0.).is_err());
    }
    #[test]
    #[ignore = "moves the cursor inside a temporary fixture and restores it; requires interactive Windows"]
    fn pointer_coordinates_remain_physical_on_an_unaware_worker_thread() {
        use windows::Win32::UI::HiDpi::*;
        unsafe {
            let _restore = PhysicalDpi(SetThreadDpiAwarenessContext(
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            ));
            let mut original = POINT::default();
            GetPhysicalCursorPos(&mut original).unwrap();
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                windows::core::w!("STATIC"),
                windows::core::w!("Frames physical pointer fixture"),
                WS_OVERLAPPEDWINDOW,
                100,
                100,
                400,
                300,
                None,
                None,
                None,
                None,
            )
            .unwrap();
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            let bounds = capture_bounds(hwnd).unwrap();
            let expected = physical_point(bounds, 0.5, 0.5).unwrap();
            let host = Host::new(vec![]).unwrap();
            let key = host.registry.lock().reconcile(&[(hwnd.0 as u64, 1)])[&(hwnd.0 as u64)];
            SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_UNAWARE);
            let result = host.input(key, &InputEvent::Pointer { x: 0.5, y: 0.5 });
            let mut actual = POINT::default();
            let read = GetPhysicalCursorPos(&mut actual);
            let restored = GetAwarenessFromDpiAwarenessContext(GetThreadDpiAwarenessContext());
            let _ = SetPhysicalCursorPos(original.x, original.y);
            let _ = DestroyWindow(hwnd);
            result.unwrap();
            read.unwrap();
            assert_eq!((actual.x, actual.y), (expected.x, expected.y));
            assert_eq!(restored, DPI_AWARENESS_UNAWARE);
        }
    }
}
