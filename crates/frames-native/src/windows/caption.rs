//! An icon-only Hide control beside the untouched Windows caption controls.
#![allow(unsafe_op_in_unsafe_fn)]
use windows::{
    Win32::{
        Foundation::*,
        Graphics::{Dwm::*, Gdi::*},
        UI::{Controls::*, HiDpi::*, Input::KeyboardAndMouse::*, Shell::*, WindowsAndMessaging::*},
    },
    core::*,
};
pub const HIDE: usize = 0xA009;
pub const LAYOUT: u32 = WM_APP + 77;

pub unsafe fn create(parent: HWND) -> Result<HWND> {
    // Child controls are clipped to the client area. A nonactivating owned
    // native BUTTON can sit in the caption without replacing any system chrome.
    let button = CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED,
        w!("BUTTON"),
        w!("Hide to tray"),
        WS_POPUP | WINDOW_STYLE(BS_PUSHBUTTON as u32),
        0,
        0,
        0,
        0,
        Some(parent),
        None,
        None,
        None,
    )?;
    SendMessageW(
        button,
        WM_SETFONT,
        Some(WPARAM(GetStockObject(DEFAULT_GUI_FONT).0 as usize)),
        Some(LPARAM(0)),
    );
    SetWindowSubclass(button, Some(button_procedure), 1, parent.0 as usize).ok()?;
    let tooltip = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_NOACTIVATE,
        TOOLTIPS_CLASSW,
        PCWSTR::null(),
        WS_POPUP | WINDOW_STYLE(TTS_ALWAYSTIP),
        0,
        0,
        0,
        0,
        Some(button),
        None,
        None,
        None,
    )?;
    let tool = TTTOOLINFOW {
        cbSize: std::mem::size_of::<TTTOOLINFOW>() as u32,
        uFlags: TTF_IDISHWND | TTF_SUBCLASS,
        hwnd: button,
        uId: button.0 as usize,
        lpszText: PWSTR(w!("Hide to tray").as_ptr() as *mut _),
        ..Default::default()
    };
    SendMessageW(
        tooltip,
        TTM_ADDTOOLW,
        None,
        Some(LPARAM((&tool as *const TTTOOLINFOW) as isize)),
    );
    layout(parent, button);
    Ok(button)
}
pub unsafe fn layout(parent: HWND, button: HWND) {
    if button.is_invalid() {
        return;
    }
    if !IsWindowVisible(parent).as_bool() || IsIconic(parent).as_bool() {
        let _ = ShowWindow(button, SW_HIDE);
        return;
    }
    let mut info = TITLEBARINFOEX {
        cbSize: std::mem::size_of::<TITLEBARINFOEX>() as u32,
        ..Default::default()
    };
    SendMessageW(
        parent,
        WM_GETTITLEBARINFOEX,
        Some(WPARAM(0)),
        Some(LPARAM((&mut info as *mut TITLEBARINFOEX) as isize)),
    );
    let minimize = info.rgrect[2];
    if minimize.right <= minimize.left || minimize.bottom <= minimize.top {
        let _ = ShowWindow(button, SW_HIDE);
        return;
    }
    let width = minimize.right - minimize.left;
    let _ = SetWindowPos(
        button,
        None,
        minimize.left - width,
        minimize.top,
        width,
        minimize.bottom - minimize.top,
        SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW,
    );
    draw_icon(button, parent);
}

/// Alpha 1 keeps the whole button clickable while leaving the native caption
/// visible beneath it. Only the glyph and hover feedback have visible opacity.
unsafe fn draw_icon(hwnd: HWND, owner: HWND) {
    let mut bounds = RECT::default();
    if GetWindowRect(hwnd, &mut bounds).is_err() {
        return;
    }
    let width = bounds.right - bounds.left;
    let height = bounds.bottom - bounds.top;
    if width <= 0 || height <= 0 {
        return;
    }
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut pixels = std::ptr::null_mut();
    let Ok(bitmap) = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut pixels, None, 0) else {
        return;
    };
    let dc = CreateCompatibleDC(None);
    let previous = SelectObject(dc, bitmap.into());
    let buffer = std::slice::from_raw_parts_mut(pixels.cast::<u8>(), (width * height * 4) as usize);
    buffer.fill(0);
    let scale = GetDpiForWindow(owner).max(96) as i32;
    let unit = |value: i32| (value * scale + 48) / 96;
    let x = (width - unit(18)) / 2;
    let y = (height - unit(18)) / 2;
    draw_eye_slash(dc, x, y, scale * 18 / 16);
    let mut dark = BOOL(0);
    let _ = DwmGetWindowAttribute(
        owner,
        DWMWA_USE_IMMERSIVE_DARK_MODE,
        (&mut dark as *mut BOOL).cast(),
        std::mem::size_of::<BOOL>() as u32,
    );
    let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
    let background_alpha = if state & 2 != 0 {
        42
    } else if state & 1 != 0 {
        24
    } else {
        1
    };
    let active = GetForegroundWindow() == owner;
    for pixel in buffer.chunks_exact_mut(4) {
        let coverage = pixel[0] as u16;
        let foreground_alpha = if active { 235 } else { 135 };
        let alpha = ((coverage * foreground_alpha + (255 - coverage) * background_alpha as u16)
            / 255) as u8;
        let shade = if dark.as_bool() { alpha } else { 0 };
        pixel.copy_from_slice(&[shade, shade, shade, alpha]);
    }
    let destination = POINT {
        x: bounds.left,
        y: bounds.top,
    };
    let size = SIZE {
        cx: width,
        cy: height,
    };
    let source = POINT::default();
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
        ..Default::default()
    };
    let _ = UpdateLayeredWindow(
        hwnd,
        None,
        Some(&destination),
        Some(&size),
        Some(dc),
        Some(&source),
        COLORREF(0),
        Some(&blend),
        ULW_ALPHA,
    );
    SelectObject(dc, previous);
    let _ = DeleteObject(bitmap.into());
    let _ = DeleteDC(dc);
}
/// Render the original Heroicons solid SVG with antialiasing at the target DPI.
/// Caption and tray controls use the same source; no glyph paths are recreated.
pub(super) unsafe fn draw_eye_slash(dc: HDC, x: i32, y: i32, dpi: i32) {
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex, OnceLock},
    };
    static CACHE: OnceLock<Mutex<HashMap<u32, Arc<Vec<u8>>>>> = OnceLock::new();
    let size = ((16 * dpi + 48) / 96).clamp(1, 512) as u32;
    let pixels = CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .entry(size)
        .or_insert_with(|| {
            let tree = resvg::usvg::Tree::from_data(
                include_bytes!("../../../../assets/icons/eye-slash.svg"),
                &resvg::usvg::Options::default(),
            )
            .expect("bundled Heroicons SVG must be valid");
            let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size).unwrap();
            resvg::render(
                &tree,
                resvg::tiny_skia::Transform::from_scale(
                    size as f32 / tree.size().width(),
                    size as f32 / tree.size().height(),
                ),
                &mut pixmap.as_mut(),
            );
            // GDI receives a white coverage mask on black. The caller applies its
            // light/dark foreground color and keeps the SVG's antialiased edges.
            Arc::new(
                pixmap
                    .data()
                    .chunks_exact(4)
                    .flat_map(|pixel| [pixel[3], pixel[3], pixel[3], 0])
                    .collect(),
            )
        })
        .clone();
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size as i32,
            biHeight: -(size as i32),
            biPlanes: 1,
            biBitCount: 32,
            ..Default::default()
        },
        ..Default::default()
    };
    StretchDIBits(
        dc,
        x,
        y,
        size as i32,
        size as i32,
        0,
        0,
        size as i32,
        size as i32,
        Some(pixels.as_ptr().cast()),
        &info,
        DIB_RGB_COLORS,
        SRCCOPY,
    );
}

unsafe extern "system" fn button_procedure(
    hwnd: HWND,
    message: u32,
    w: WPARAM,
    l: LPARAM,
    _id: usize,
    owner: usize,
) -> LRESULT {
    let parent = HWND(owner as *mut _);
    if message == WM_MOUSEACTIVATE {
        return LRESULT(MA_NOACTIVATE as isize);
    }
    if message == WM_ERASEBKGND {
        return LRESULT(1);
    }
    if message == WM_PAINT {
        let mut paint = PAINTSTRUCT::default();
        BeginPaint(hwnd, &mut paint);
        let _ = EndPaint(hwnd, &paint);
        draw_icon(hwnd, parent);
        return LRESULT(0);
    }
    if message == WM_MOUSEMOVE {
        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
        if state & 1 == 0 {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, state | 1);
            let mut tracking = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                ..Default::default()
            };
            let _ = TrackMouseEvent(&mut tracking);
            draw_icon(hwnd, parent);
        }
    }
    if message == WM_MOUSELEAVE {
        SetWindowLongPtrW(
            hwnd,
            GWLP_USERDATA,
            GetWindowLongPtrW(hwnd, GWLP_USERDATA) & !1,
        );
        draw_icon(hwnd, parent);
    }
    if message == WM_LBUTTONDOWN {
        SetWindowLongPtrW(
            hwnd,
            GWLP_USERDATA,
            GetWindowLongPtrW(hwnd, GWLP_USERDATA) | 2,
        );
        draw_icon(hwnd, parent);
    }
    if message == WM_LBUTTONUP {
        let pressed = GetWindowLongPtrW(hwnd, GWLP_USERDATA) & 2 != 0;
        SetWindowLongPtrW(
            hwnd,
            GWLP_USERDATA,
            GetWindowLongPtrW(hwnd, GWLP_USERDATA) & !2,
        );
        let result = DefSubclassProc(hwnd, message, w, l);
        draw_icon(hwnd, parent);
        let mut rect = RECT::default();
        let _ = GetClientRect(hwnd, &mut rect);
        let x = l.0 as u16 as i16 as i32;
        let y = (l.0 >> 16) as u16 as i16 as i32;
        if pressed && x >= 0 && y >= 0 && x < rect.right && y < rect.bottom {
            let _ = PostMessageW(
                Some(HWND(owner as *mut _)),
                WM_COMMAND,
                WPARAM(HIDE),
                LPARAM(0),
            );
        }
        return result;
    }
    DefSubclassProc(hwnd, message, w, l)
}
