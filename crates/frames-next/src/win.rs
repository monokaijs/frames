//! Win32 windows for the host's QA source and the client's viewer.

use anyhow::Result;
use frames_input::{Button, Event, keys, normalize};
use frames_media::api::MediaClock;
use frames_media::clock::SystemClock;
use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::VK_F12;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

pub fn dpi_aware() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// Set when the user closes a window created here.
static CLOSED: AtomicU32 = AtomicU32::new(0);

pub fn close_requested() -> bool {
    CLOSED.load(Ordering::Relaxed) > 0
}

fn procedure(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if message == WM_CLOSE {
        CLOSED.fetch_add(1, Ordering::Relaxed);
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
        WM_KEYDOWN | WM_SYSKEYDOWN if w.0 == usize::from(VK_F12.0) && release_shortcut_held() => {
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
        WM_KILLFOCUS => Some(Captured::Event {
            event: Event::ReleaseAll,
            at_us,
        }),
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
        Ok(CreateWindowExW(
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
        )?)
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
