//! Win32 windows for the host's QA source and the client's viewer.

use anyhow::Result;
use frames_input::{Button, Event, keys, normalize};
use frames_media::api::MediaClock;
use frames_media::clock::SystemClock;
use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_F10, VK_F11, VK_F12};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

#[path = "win_menus.rs"]
pub mod menus;

pub fn dpi_aware() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

thread_local! {
    /// The picture's size: the viewer window keeps its shape when resized,
    /// so the picture fills it (no bars).
    static ASPECT: Cell<Option<(u32, u32)>> = const { Cell::new(None) };
    /// Live statistics in the title, flipped by Ctrl+Alt+F10.
    static STATS: Cell<bool> = const { Cell::new(false) };
    /// Files dropped on the viewer, for the host.
    static DROPPED: RefCell<Vec<std::path::PathBuf>> = const { RefCell::new(Vec::new()) };
    /// Set when the user closes a window this thread created (each viewer
    /// window runs on its own thread, so closing one ends only its session).
    static CLOSED: Cell<bool> = const { Cell::new(false) };
}

pub fn close_requested() -> bool {
    CLOSED.with(Cell::get)
}

/// Forgets a close request (it was turned into a request to the host).
pub fn clear_close() {
    CLOSED.with(|closed| closed.set(false));
}

fn procedure(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if message == WM_CLOSE {
        CLOSED.with(|closed| closed.set(true));
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, message, w, l) }
}

pub use crate::lane::Captured;

thread_local! {
    static CAPTURED: RefCell<Vec<Captured>> = const { RefCell::new(Vec::new()) };
    /// The host's cursor as shown over the viewer; `None` hides it.
    static CURSOR: RefCell<Option<HCURSOR>> = const { RefCell::new(None) };
    static CURSOR_SHAPE: RefCell<Option<HCURSOR>> = const { RefCell::new(None) };
    /// Relative mouse: `Some(locked)` while on; `locked` when the pointer is
    /// confined and hidden (not in unattended tests).
    static RELATIVE: Cell<Option<bool>> = const { Cell::new(None) };
}

/// Turns relative mouse (games) on or off, as Ctrl+Alt+F11 does, and tells
/// the host. With `lock`, motion comes from raw input and the pointer is
/// confined to the viewer and hidden; without it (unattended tests) the
/// system pointer is left alone.
pub fn set_relative(hwnd: HWND, on: bool, lock: bool) {
    use windows::Win32::UI::Input::{RAWINPUTDEVICE, RIDEV_REMOVE, RegisterRawInputDevices};
    let current = RELATIVE.with(Cell::get);
    if current.is_some() == on {
        return;
    }
    let device = |flags, target: Option<HWND>| RAWINPUTDEVICE {
        usUsagePage: 0x01,
        usUsage: 0x02,
        dwFlags: flags,
        hwndTarget: target.unwrap_or_default(),
    };
    let size = std::mem::size_of::<RAWINPUTDEVICE>() as u32;
    if on {
        if lock {
            // SAFETY: plain Win32 calls on this thread's window.
            unsafe {
                if RegisterRawInputDevices(&[device(Default::default(), Some(hwnd))], size).is_err()
                {
                    return;
                }
                let mut rect = RECT::default();
                let _ = GetClientRect(hwnd, &mut rect);
                let mut corners = [
                    windows::Win32::Foundation::POINT {
                        x: rect.left,
                        y: rect.top,
                    },
                    windows::Win32::Foundation::POINT {
                        x: rect.right,
                        y: rect.bottom,
                    },
                ];
                MapWindowPoints(Some(hwnd), None, &mut corners);
                let screen = RECT {
                    left: corners[0].x,
                    top: corners[0].y,
                    right: corners[1].x,
                    bottom: corners[1].y,
                };
                let _ = ClipCursor(Some(&screen));
                ShowCursor(false);
            }
        }
        RELATIVE.with(|r| r.set(Some(lock)));
    } else {
        if current == Some(true) {
            // SAFETY: undoes exactly what turning it on did.
            unsafe {
                let _ = RegisterRawInputDevices(&[device(RIDEV_REMOVE, None)], size);
                let _ = ClipCursor(None);
                ShowCursor(true);
            }
        }
        RELATIVE.with(|r| r.set(None));
    }
    let at_us = SystemClock::new().now_us();
    CAPTURED.with(|c| {
        c.borrow_mut().push(Captured::Event {
            event: Event::Mode { relative: on },
            at_us,
        })
    });
}

pub fn relative() -> bool {
    RELATIVE.with(Cell::get).is_some()
}

/// Generated relative motion for unattended tests (raw input cannot be
/// posted); ignored unless relative mode is on.
pub fn synthetic_motion(dx: i32, dy: i32) {
    if relative() {
        let at_us = SystemClock::new().now_us();
        CAPTURED.with(|c| c.borrow_mut().push(Captured::Motion { dx, dy, at_us }));
    }
}

/// The relative motion in a WM_INPUT message, if it is mouse motion.
fn raw_motion(l: LPARAM) -> Option<(i32, i32)> {
    use windows::Win32::UI::Input::{
        GetRawInputData, HRAWINPUT, RAWINPUT, RAWINPUTHEADER, RID_INPUT, RIM_TYPEMOUSE,
    };
    let mut input = RAWINPUT::default();
    let mut size = std::mem::size_of::<RAWINPUT>() as u32;
    // SAFETY: `input` is large enough for one mouse report.
    let read = unsafe {
        GetRawInputData(
            HRAWINPUT(l.0 as *mut _),
            RID_INPUT,
            Some((&mut input as *mut RAWINPUT).cast()),
            &mut size,
            std::mem::size_of::<RAWINPUTHEADER>() as u32,
        )
    };
    if read == u32::MAX || input.header.dwType != RIM_TYPEMOUSE.0 {
        return None;
    }
    // SAFETY: a mouse report, per the header.
    let mouse = unsafe { input.data.mouse };
    // MOUSE_MOVE_ABSOLUTE (tablets, remote desktop) carries positions.
    if mouse.usFlags.0 & 1 != 0 || (mouse.lLastX == 0 && mouse.lLastY == 0) {
        return None;
    }
    Some((mouse.lLastX, mouse.lLastY))
}

/// Applies a cursor message from the host (viewer thread only).
pub fn apply_cursor(message: &frames_input::cursor::CursorMessage) {
    use frames_input::cursor::CursorMessage;
    match message {
        CursorMessage::Shape {
            width,
            height,
            hotspot_x,
            hotspot_y,
            bgra,
        } => {
            let Some(cursor) = make_cursor(*width, *height, *hotspot_x, *hotspot_y, bgra) else {
                eprintln!("could not create a {width}x{height} cursor from the host's shape");
                return;
            };
            let old = CURSOR_SHAPE.with(|c| c.borrow_mut().replace(cursor));
            CURSOR.with(|c| *c.borrow_mut() = Some(cursor));
            unsafe {
                let _ = SetCursor(Some(cursor));
                if let Some(old) = old {
                    let _ = DestroyCursor(old);
                }
            }
        }
        CursorMessage::Visible(visible) => {
            let shape = CURSOR_SHAPE.with(|c| *c.borrow());
            let shown = if *visible { shape } else { None };
            CURSOR.with(|c| *c.borrow_mut() = shown);
            unsafe {
                let _ = SetCursor(shown);
            }
        }
    }
}

/// An OS cursor from premultiplied BGRA (Windows wants straight alpha).
fn make_cursor(
    width: u16,
    height: u16,
    hotspot_x: u16,
    hotspot_y: u16,
    bgra: &[u8],
) -> Option<HCURSOR> {
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS,
        DeleteObject,
    };
    let (w, h) = (i32::from(width), i32::from(height));
    unsafe {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let color = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        let pixels = std::slice::from_raw_parts_mut(bits as *mut u8, bgra.len());
        for (out, pixel) in pixels.chunks_exact_mut(4).zip(bgra.chunks_exact(4)) {
            let alpha = u32::from(pixel[3]);
            for channel in 0..3 {
                out[channel] = if alpha == 0 {
                    0
                } else {
                    (u32::from(pixel[channel]) * 255 / alpha).min(255) as u8
                };
            }
            out[3] = pixel[3];
        }
        // All-zero AND mask: the colour bitmap's alpha decides transparency.
        let mask_bytes = vec![0u8; (w as usize).div_ceil(16) * 2 * h as usize];
        let mask = CreateBitmap(w, h, 1, 1, Some(mask_bytes.as_ptr().cast()));
        let icon = ICONINFO {
            fIcon: false.into(),
            xHotspot: u32::from(hotspot_x),
            yHotspot: u32::from(hotspot_y),
            hbmMask: mask,
            hbmColor: color,
        };
        let cursor = CreateIconIndirect(&icon).ok();
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        cursor.map(|icon| HCURSOR(icon.0))
    }
}

/// A 16x16 arrow, for QA runs whose source has no real cursor over it.
pub fn qa_cursor() -> frames_input::cursor::CursorMessage {
    let (width, height) = (16u16, 16u16);
    let mut bgra = vec![0u8; 16 * 16 * 4];
    for y in 0..16usize {
        for x in 0..=y.min(10) {
            let edge = x == 0 || x == y.min(10) || y == 15;
            let value = if edge { 0 } else { 255 };
            bgra[(y * 16 + x) * 4..][..4].copy_from_slice(&[value, value, value, 255]);
        }
    }
    frames_input::cursor::CursorMessage::Shape {
        width,
        height,
        hotspot_x: 0,
        hotspot_y: 0,
        bgra,
    }
}

/// Input captured since the last call (viewer thread only).
pub fn take_input() -> Vec<Captured> {
    CAPTURED.with(|c| std::mem::take(&mut *c.borrow_mut()))
}

fn client_extent(hwnd: HWND) -> (u32, u32) {
    let mut rect = RECT::default();
    let _ = unsafe { GetClientRect(hwnd, &mut rect) };
    (
        (rect.right - rect.left).max(1) as u32,
        (rect.bottom - rect.top).max(1) as u32,
    )
}

fn position(hwnd: HWND, l: LPARAM) -> (u16, u16) {
    let (width, height) = client_extent(hwnd);
    let x = (l.0 & 0xffff) as i16 as i32;
    let y = ((l.0 >> 16) & 0xffff) as i16 as i32;
    (normalize(x, width), normalize(y, height))
}

fn viewer_input(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> Option<Captured> {
    let at_us = SystemClock::new().now_us();
    let button = |button, down| {
        let (x, y) = position(hwnd, l);
        Some(Captured::Event {
            event: Event::Button { button, down, x, y },
            at_us,
        })
    };
    let xbutton = |down| {
        let which = if (w.0 >> 16) & 0xffff == 2 {
            Button::Forward
        } else {
            Button::Back
        };
        button(which, down)
    };
    match message {
        WM_INPUT if relative() => {
            let (dx, dy) = raw_motion(l)?;
            Some(Captured::Motion { dx, dy, at_us })
        }
        // Positions mean nothing while relative.
        WM_MOUSEMOVE if relative() => None,
        WM_MOUSEMOVE => {
            let (x, y) = position(hwnd, l);
            Some(Captured::Move { x, y, at_us })
        }
        WM_LBUTTONDOWN => button(Button::Left, true),
        WM_LBUTTONUP => button(Button::Left, false),
        WM_RBUTTONDOWN => button(Button::Right, true),
        WM_RBUTTONUP => button(Button::Right, false),
        WM_MBUTTONDOWN => button(Button::Middle, true),
        WM_MBUTTONUP => button(Button::Middle, false),
        WM_XBUTTONDOWN => xbutton(true),
        WM_XBUTTONUP => xbutton(false),
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            let delta = ((w.0 >> 16) & 0xffff) as u16 as i16;
            let (dx, dy) = if message == WM_MOUSEWHEEL {
                (0, delta)
            } else {
                (delta, 0)
            };
            Some(Captured::Event {
                event: Event::Wheel { dx, dy },
                at_us,
            })
        }
        // Ctrl+Alt+F12: release remote input and minimize (as in v1); the
        // shortcut itself never reaches the host.
        // Ctrl+Alt+F11: relative mouse on or off (as in v1); never sent.
        // Ctrl+Alt+F10: live statistics on or off (v1's debug overlay
        // shortcut); never sent.
        WM_KEYDOWN | WM_SYSKEYDOWN if w.0 == usize::from(VK_F10.0) && release_shortcut_held() => {
            STATS.with(|stats| stats.set(!stats.get()));
            None
        }
        WM_KEYDOWN | WM_SYSKEYDOWN if w.0 == usize::from(VK_F11.0) && release_shortcut_held() => {
            set_relative(hwnd, !relative(), true);
            None
        }
        WM_KEYDOWN | WM_SYSKEYDOWN if w.0 == usize::from(VK_F12.0) && release_shortcut_held() => {
            set_relative(hwnd, false, true);
            unsafe {
                let _ = ShowWindow(hwnd, SW_MINIMIZE);
            }
            Some(Captured::Event {
                event: Event::ReleaseAll,
                at_us,
            })
        }
        WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => {
            let code = ((l.0 >> 16) & 0xff) as u16;
            let extended = (l.0 >> 24) & 1 == 1;
            let scan = if extended { 0xe000 | code } else { code };
            let usage = keys::scan_to_usage(scan)?;
            let down = matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN);
            Some(Captured::Event {
                event: Event::Key { usage, down },
                at_us,
            })
        }
        WM_KILLFOCUS => {
            // Leaving the window gives the pointer back.
            set_relative(hwnd, false, true);
            Some(Captured::Event {
                event: Event::ReleaseAll,
                at_us,
            })
        }
        _ => None,
    }
}

fn release_shortcut_held() -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_MENU};
    let down = |key: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY| unsafe {
        GetKeyState(i32::from(key.0)) < 0
    };
    down(VK_CONTROL) && down(VK_MENU)
}

unsafe extern "system" fn viewer_procedure(
    hwnd: HWND,
    message: u32,
    w: WPARAM,
    l: LPARAM,
) -> LRESULT {
    // Over the video, show the host's cursor (or none) instead of the arrow.
    if message == WM_SETCURSOR && (l.0 & 0xffff) as u32 == HTCLIENT {
        let cursor = CURSOR.with(|c| *c.borrow());
        let shape_known = CURSOR_SHAPE.with(|c| c.borrow().is_some());
        if shape_known {
            unsafe {
                let _ = SetCursor(cursor);
            }
            return LRESULT(1);
        }
    }
    // An item of the host application's menus (`set_host_menu`).
    if message == WM_COMMAND && (w.0 & 0xffff) >= HOST_MENU_BASE && (l.0 == 0) {
        let id = ((w.0 & 0xffff) - HOST_MENU_BASE) as u32;
        let at_us = SystemClock::new().now_us();
        CAPTURED.with(|c| {
            c.borrow_mut().push(Captured::Event {
                event: Event::Menu { id },
                at_us,
            })
        });
        return LRESULT(0);
    }
    if message == WM_SYSCOMMAND {
        let at_us = SystemClock::new().now_us();
        let quality = |quality: u8| Captured::Event {
            event: Event::MaxBitrate {
                mbps: crate::quality_mbps(quality),
            },
            at_us,
        };
        let handled = match w.0 & 0xfff0 {
            MENU_HIGH => Some(Some(quality(0))),
            MENU_MEDIUM => Some(Some(quality(1))),
            MENU_LOW => Some(Some(quality(2))),
            MENU_SEND => {
                choose_files(hwnd);
                Some(None)
            }
            MENU_STATS => {
                STATS.with(|stats| stats.set(!stats.get()));
                Some(None)
            }
            MENU_RELEASE => {
                set_relative(hwnd, false, true);
                unsafe {
                    let _ = ShowWindow(hwnd, SW_MINIMIZE);
                }
                Some(Some(Captured::Event {
                    event: Event::ReleaseAll,
                    at_us,
                }))
            }
            MENU_DISCONNECT => {
                crate::DISCONNECT.store(true, Ordering::Relaxed);
                Some(None)
            }
            _ => None,
        };
        if let Some(captured) = handled {
            if let Some(captured) = captured {
                CAPTURED.with(|c| c.borrow_mut().push(captured));
            }
            return LRESULT(0);
        }
    }
    if message == WM_DROPFILES {
        let files = dropped_files(windows::Win32::UI::Shell::HDROP(w.0 as *mut _));
        DROPPED.with(|dropped| dropped.borrow_mut().extend(files));
        return LRESULT(0);
    }
    if message == WM_SIZING
        && let Some(aspect) = ASPECT.with(Cell::get)
    {
        // SAFETY: WM_SIZING carries the proposed window rectangle.
        let rect = unsafe { &mut *(l.0 as *mut RECT) };
        keep_aspect(hwnd, rect, w.0 as u32, aspect);
        return LRESULT(1);
    }
    if let Some(captured) = viewer_input(hwnd, message, w, l) {
        CAPTURED.with(|c| c.borrow_mut().push(captured));
        // Keys go to the host only; F10/Alt must not open the system menu.
        if matches!(message, WM_SYSKEYDOWN | WM_SYSKEYUP) {
            return LRESULT(0);
        }
    }
    procedure(hwnd, message, w, l)
}

/// Messages the QA source window received from the host's input injector.
pub struct QaInputCounts {
    pub moves: AtomicU64,
    pub buttons: AtomicU64,
    pub wheels: AtomicU64,
    pub keys: AtomicU64,
}

pub static QA_INPUT: QaInputCounts = QaInputCounts {
    moves: AtomicU64::new(0),
    buttons: AtomicU64::new(0),
    wheels: AtomicU64::new(0),
    keys: AtomicU64::new(0),
};

unsafe extern "system" fn qa_procedure(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let counter = match message {
        WM_MOUSEMOVE => Some(&QA_INPUT.moves),
        WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN
        | WM_MBUTTONUP | WM_XBUTTONDOWN | WM_XBUTTONUP => Some(&QA_INPUT.buttons),
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => Some(&QA_INPUT.wheels),
        WM_KEYDOWN | WM_KEYUP => Some(&QA_INPUT.keys),
        _ => None,
    };
    if let Some(counter) = counter {
        counter.fetch_add(1, Ordering::Relaxed);
        return LRESULT(0);
    }
    procedure(hwnd, message, w, l)
}

#[derive(Clone, Copy)]
pub enum Kind {
    Viewer,
    QaSource,
}

/// A top-level window with a client area of `width` x `height`.
pub fn window(title: PCWSTR, kind: Kind, x: i32, width: u32, height: u32) -> Result<HWND> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let (class, handler): (PCWSTR, WNDPROC) = match kind {
            Kind::Viewer => (w!("FramesNextViewer"), Some(viewer_procedure)),
            Kind::QaSource => (w!("FramesNextQa"), Some(qa_procedure)),
        };
        let wc = WNDCLASSW {
            lpfnWndProc: handler,
            hInstance: instance.into(),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        RegisterClassW(&wc);
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        AdjustWindowRect(&mut rect, WS_OVERLAPPEDWINDOW, false)?;
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class,
            title,
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            x,
            0,
            rect.right - rect.left,
            rect.bottom - rect.top,
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        // Files dropped on the viewer go to the host; the window menu has
        // the Frames commands.
        if matches!(kind, Kind::Viewer) {
            windows::Win32::UI::Shell::DragAcceptFiles(hwnd, true);
            add_frames_menu(hwnd);
        }
        Ok(hwnd)
    }
}

// Frames commands in the viewer's window menu (title bar icon, Alt+Space).
// WM_SYSCOMMAND reserves the low four bits, so ids step by 16.
const MENU_HIGH: usize = 0x100;
const MENU_MEDIUM: usize = 0x110;
const MENU_LOW: usize = 0x120;
const MENU_SEND: usize = 0x130;
const MENU_STATS: usize = 0x140;
const MENU_RELEASE: usize = 0x150;
const MENU_DISCONNECT: usize = 0x160;

fn add_frames_menu(hwnd: HWND) {
    unsafe {
        let menu = GetSystemMenu(hwnd, false);
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        for (id, label) in [
            (MENU_HIGH, w!("High quality")),
            (MENU_MEDIUM, w!("Medium quality")),
            (MENU_LOW, w!("Low quality")),
            (MENU_SEND, w!("Send file...")),
            (MENU_STATS, w!("Show statistics\tCtrl+Alt+F10")),
            (MENU_RELEASE, w!("Release input\tCtrl+Alt+F12")),
            (MENU_DISCONNECT, w!("Disconnect")),
        ] {
            let _ = AppendMenuW(menu, MF_STRING, id, label);
        }
    }
}

/// Files chosen with Send file..., by viewer window (the dialog runs on its
/// own thread so the video keeps playing).
static CHOSEN: std::sync::Mutex<Vec<(usize, std::path::PathBuf)>> =
    std::sync::Mutex::new(Vec::new());

/// Shows the file dialog for `hwnd` on a new thread.
fn choose_files(hwnd: HWND) {
    let owner = hwnd.0 as usize;
    std::thread::spawn(move || {
        use windows::Win32::System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
            CoTaskMemFree,
        };
        use windows::Win32::UI::Shell::{
            FOS_ALLOWMULTISELECT, FOS_FILEMUSTEXIST, FileOpenDialog, IFileOpenDialog,
            SIGDN_FILESYSPATH,
        };
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let Ok(dialog) =
                CoCreateInstance::<_, IFileOpenDialog>(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)
            else {
                return;
            };
            if let Ok(options) = dialog.GetOptions() {
                let _ = dialog.SetOptions(options | FOS_ALLOWMULTISELECT | FOS_FILEMUSTEXIST);
            }
            let _ = dialog.SetTitle(w!("Send files to the remote computer"));
            // Err when cancelled.
            if dialog.Show(Some(HWND(owner as *mut _))).is_err() {
                return;
            }
            let Ok(items) = dialog.GetResults() else {
                return;
            };
            let mut chosen = Vec::new();
            for index in 0..items.GetCount().unwrap_or(0) {
                if let Ok(item) = items.GetItemAt(index)
                    && let Ok(name) = item.GetDisplayName(SIGDN_FILESYSPATH)
                {
                    if let Ok(path) = name.to_string() {
                        chosen.push((owner, std::path::PathBuf::from(path)));
                    }
                    CoTaskMemFree(Some(name.0 as *const _));
                }
            }
            CHOSEN.lock().unwrap().extend(chosen);
        }
    });
}

/// Whether Ctrl+Alt+F10 turned live statistics on for this thread's viewer.
pub fn stats_toggled() -> bool {
    STATS.with(Cell::get)
}

/// Files dropped on (or chosen for) this viewer window since the last call.
pub fn take_dropped(viewer: HWND) -> Vec<std::path::PathBuf> {
    let mut files = DROPPED.with(|dropped| std::mem::take(&mut *dropped.borrow_mut()));
    let mut chosen = CHOSEN.lock().unwrap();
    let owner = viewer.0 as usize;
    files.extend(
        chosen
            .iter()
            .filter(|(o, _)| *o == owner)
            .map(|(_, p)| p.clone()),
    );
    chosen.retain(|(o, _)| *o != owner);
    files
}

/// The files of a WM_DROPFILES message.
fn dropped_files(drop: windows::Win32::UI::Shell::HDROP) -> Vec<std::path::PathBuf> {
    use windows::Win32::UI::Shell::{DragFinish, DragQueryFileW};
    let mut files = Vec::new();
    unsafe {
        let count = DragQueryFileW(drop, u32::MAX, None);
        for index in 0..count {
            let length = DragQueryFileW(drop, index, None) as usize;
            let mut name = vec![0u16; length + 1];
            let copied = DragQueryFileW(drop, index, Some(&mut name)) as usize;
            files.push(std::path::PathBuf::from(String::from_utf16_lossy(
                &name[..copied],
            )));
        }
        DragFinish(drop);
    }
    files
}

/// Border, title bar and menu bar sizes of a viewer window (left+right,
/// top+bottom).
fn frame_extra(hwnd: HWND) -> (i32, i32) {
    let mut rect = RECT::default();
    unsafe {
        let has_menu = !GetMenu(hwnd).is_invalid();
        let _ = AdjustWindowRect(&mut rect, WS_OVERLAPPEDWINDOW, has_menu);
    }
    (rect.right - rect.left, rect.bottom - rect.top)
}

/// Menu bar command ids of the host application's items (`Event::Menu`).
const HOST_MENU_BASE: usize = 0x8000;

/// Shows the host application's menus as this viewer window's menu bar,
/// keeping its client size.
pub fn set_host_menu(hwnd: HWND, items: &[crate::menus::Item]) {
    fn build(items: &[crate::menus::Item]) -> HMENU {
        unsafe {
            let menu = CreatePopupMenu().unwrap_or_default();
            for item in items {
                if item.separator {
                    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
                    continue;
                }
                let title: Vec<u16> = item
                    .title
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .collect();
                let mut flags = MF_STRING;
                if !item.enabled {
                    flags |= MF_GRAYED;
                }
                if item.checked {
                    flags |= MF_CHECKED;
                }
                if item.children.is_empty() {
                    let id = if item.id > 0 && (item.id as usize) < 0x7fff {
                        HOST_MENU_BASE + item.id as usize
                    } else {
                        0
                    };
                    let _ = AppendMenuW(menu, flags, id, PCWSTR(title.as_ptr()));
                } else {
                    let submenu = build(&item.children);
                    let _ = AppendMenuW(
                        menu,
                        flags | MF_POPUP,
                        submenu.0 as usize,
                        PCWSTR(title.as_ptr()),
                    );
                }
            }
            menu
        }
    }
    let client = client_size(hwnd);
    unsafe {
        let old = GetMenu(hwnd);
        let bar = if items.is_empty() {
            HMENU::default()
        } else {
            let bar = CreateMenu().unwrap_or_default();
            for top in items {
                let title: Vec<u16> = top.title.encode_utf16().chain(std::iter::once(0)).collect();
                let submenu = build(&top.children);
                let _ = AppendMenuW(
                    bar,
                    MF_STRING | MF_POPUP,
                    submenu.0 as usize,
                    PCWSTR(title.as_ptr()),
                );
            }
            bar
        };
        let _ = SetMenu(hwnd, Some(bar));
        if !old.is_invalid() {
            let _ = DestroyMenu(old);
        }
    }
    // The menu bar sits outside the client area: keep the picture's size.
    set_client_size(hwnd, client.0, client.1);
}

/// Adjusts a window rectangle being dragged so its client area has the
/// picture's shape: the dragged edge decides, the opposite one follows.
fn keep_aspect(hwnd: HWND, rect: &mut RECT, edge: u32, (width, height): (u32, u32)) {
    let (extra_w, extra_h) = frame_extra(hwnd);
    let client_w = (rect.right - rect.left - extra_w).max(1);
    let client_h = (rect.bottom - rect.top - extra_h).max(1);
    let ratio = f64::from(width.max(1)) / f64::from(height.max(1));
    if matches!(edge, WMSZ_TOP | WMSZ_BOTTOM) {
        let new_w = (f64::from(client_h) * ratio).round() as i32;
        rect.right = rect.left + new_w + extra_w;
    } else {
        let new_h = (f64::from(client_w) / ratio).round() as i32;
        if matches!(edge, WMSZ_TOP | WMSZ_TOPLEFT | WMSZ_TOPRIGHT) {
            rect.top = rect.bottom - new_h - extra_h;
        } else {
            rect.bottom = rect.top + new_h + extra_h;
        }
    }
}

/// The viewer window keeps this picture shape while the user resizes it.
pub fn set_aspect(width: u32, height: u32) {
    ASPECT.with(|aspect| aspect.set(Some((width, height))));
}

/// The size of a window's client area.
pub fn client_size(hwnd: HWND) -> (u32, u32) {
    let mut rect = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rect);
    }
    (
        (rect.right - rect.left).max(0) as u32,
        (rect.bottom - rect.top).max(0) as u32,
    )
}

/// The usable desktop area (without the taskbar), for sizing viewers.
pub fn work_area() -> (u32, u32) {
    let mut rect = RECT::default();
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut rect as *mut RECT as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    if ok.is_err() || rect.right <= rect.left {
        return (1280, 720);
    }
    (
        (rect.right - rect.left) as u32,
        (rect.bottom - rect.top) as u32,
    )
}

/// Gives a window this client size, keeping its position.
pub fn set_client_size(hwnd: HWND, width: u32, height: u32) {
    let (extra_w, extra_h) = frame_extra(hwnd);
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            width as i32 + extra_w,
            height as i32 + extra_h,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// Dispatches pending messages for windows on this thread.
pub fn pump() {
    unsafe {
        let mut message = MSG::default();
        while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

/// A generated test window repainted at `fps` on its own thread, so the
/// host captures moving content without showing the real screen.
pub struct QaSource {
    hwnd: HWND,
    running: Arc<AtomicBool>,
    painter: Option<JoinHandle<()>>,
}

impl QaSource {
    pub fn start(width: u32, height: u32, fps: u32) -> Result<Self> {
        let hwnd = window(w!("Frames QA source"), Kind::QaSource, 0, width, height)?;
        pump();
        paint(hwnd, 0, width, height);
        let running = Arc::new(AtomicBool::new(true));
        let handle = hwnd.0 as usize;
        let flag = running.clone();
        let painter = std::thread::spawn(move || {
            let hwnd = HWND(handle as *mut _);
            let start = Instant::now();
            let interval = Duration::from_micros(1_000_000 / u64::from(fps.max(1)));
            let mut n = 0u32;
            while flag.load(Ordering::Relaxed) {
                if let Some(wait) = (start + interval * n).checked_duration_since(Instant::now()) {
                    std::thread::sleep(wait);
                }
                paint(hwnd, u64::from(n), width, height);
                n += 1;
            }
        });
        Ok(Self {
            hwnd,
            running,
            painter: Some(painter),
        })
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }
}

impl Drop for QaSource {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(painter) = self.painter.take() {
            let _ = painter.join();
        }
    }
}

fn paint(hwnd: HWND, n: u64, width: u32, height: u32) {
    unsafe {
        let dc = GetDC(Some(hwnd));
        let background = CreateSolidBrush(COLORREF(0x0020_2020 + ((n as u32 * 3) & 0x3f)));
        FillRect(
            dc,
            &RECT {
                left: 0,
                top: 0,
                right: width as i32,
                bottom: height as i32,
            },
            background,
        );
        let _ = DeleteObject(background.into());
        for stripe in 0..24u32 {
            let brush = CreateSolidBrush(COLORREF(
                (stripe * 0x000a_0b0c + n as u32 * 0x0001_0203) & 0x00ff_ffff,
            ));
            let y = (stripe * height / 24) as i32;
            let x = ((n as u32 * (stripe + 3) * 7) % width) as i32;
            FillRect(
                dc,
                &RECT {
                    left: x,
                    top: y,
                    right: x + 240,
                    bottom: y + (height / 24) as i32,
                },
                brush,
            );
            let _ = DeleteObject(brush.into());
        }
        let text: Vec<u16> = format!("Frames QA source  frame {n:08}  ")
            .encode_utf16()
            .collect();
        SetBkColor(dc, COLORREF(0x00ff_ffff));
        for row in 0..20 {
            let _ = TextOutW(dc, 40, 40 + row * 48, &text);
        }
        ReleaseDC(Some(hwnd), dc);
    }
}

/// Visible top-level windows with a title, in z order: `(handle, title)`.
pub fn shareable_windows() -> Vec<(HWND, String)> {
    unsafe extern "system" fn each(hwnd: HWND, found: LPARAM) -> windows::core::BOOL {
        let found = unsafe { &mut *(found.0 as *mut Vec<(HWND, String)>) };
        let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
        let style = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) } as u32;
        let tool = style & WS_EX_TOOLWINDOW.0 != 0;
        let mut title = [0u16; 512];
        let len = unsafe { GetWindowTextW(hwnd, &mut title) } as usize;
        if visible && !tool && len > 0 {
            found.push((hwnd, String::from_utf16_lossy(&title[..len])));
        }
        true.into()
    }
    let mut found: Vec<(HWND, String)> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM(&mut found as *mut _ as isize));
    }
    found
}

/// The first shareable window whose title contains `needle` (any case).
pub fn find_window(needle: &str) -> Option<(HWND, String)> {
    let needle = needle.to_lowercase();
    shareable_windows()
        .into_iter()
        .find(|(_, title)| title.to_lowercase().contains(&needle))
}

/// Sets a window's title.
pub fn set_title(hwnd: HWND, title: &str) {
    let wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let _ = SetWindowTextW(hwnd, PCWSTR(wide.as_ptr()));
    }
}
