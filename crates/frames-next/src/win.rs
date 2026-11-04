//! Win32 windows for the host's QA source and the client's viewer.

use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
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

unsafe extern "system" fn procedure(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if message == WM_CLOSE {
        CLOSED.fetch_add(1, Ordering::Relaxed);
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, message, w, l) }
}

/// A top-level window with a client area of `width` x `height`.
pub fn window(title: PCWSTR, x: i32, width: u32, height: u32) -> Result<HWND> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = w!("FramesNext");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(procedure),
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
        let hwnd = window(w!("Frames QA source"), 0, width, height)?;
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
