//! Generated red source under a blue cover, standard menu and editable text.
//! Only intended for interactive LAN acceptance tests; closes after five minutes.
#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use windows::{
        Win32::{
            Foundation::*,
            Graphics::Gdi::*,
            System::LibraryLoader::GetModuleHandleW,
            UI::{HiDpi::*, Input::KeyboardAndMouse::SetFocus, WindowsAndMessaging::*},
        },
        core::w,
    };
    unsafe extern "system" fn proc(hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        unsafe {
            if message == WM_COMMAND {
                if wp.0 & 0xffff == 101 {
                    let _ = SetWindowTextW(hwnd, w!("Frames QA source marked"));
                }
                if (wp.0 >> 16) as u32 == EN_CHANGE {
                    let edit = HWND(lp.0 as *mut _);
                    let mut text = [0u16; 256];
                    let count = GetWindowTextW(edit, &mut text);
                    let title = format!(
                        "Frames QA source text: {}",
                        String::from_utf16_lossy(&text[..count as usize])
                    );
                    let wide: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
                    let _ = SetWindowTextW(hwnd, windows::core::PCWSTR(wide.as_ptr()));
                }
            }
            if message == WM_SETFOCUS {
                let _ = SetFocus(Some(GetDlgItem(Some(hwnd), 201).unwrap_or_default()));
            }
            if message == WM_CLOSE {
                PostQuitMessage(0);
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, message, wp, lp)
        }
    }
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let instance = GetModuleHandleW(None)?;
        let red = CreateSolidBrush(COLORREF(0x0d0dd9));
        let blue = CreateSolidBrush(COLORREF(0xd90d0d));
        for (class, brush) in [(w!("FramesQASource"), red), (w!("FramesQACover"), blue)] {
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(proc),
                hInstance: instance.into(),
                lpszClassName: class,
                hbrBackground: brush,
                hIcon: LoadIconW(None, IDI_INFORMATION)?,
                ..Default::default()
            });
        }
        let menu = CreateMenu()?;
        let file = CreatePopupMenu()?;
        AppendMenuW(file, MF_STRING, 101, w!("Mark QA"))?;
        AppendMenuW(file, MF_STRING | MF_GRAYED, 102, w!("Unavailable QA"))?;
        AppendMenuW(menu, MF_POPUP, file.0 as usize, w!("File"))?;
        let source = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("FramesQASource"),
            w!("Frames QA source"),
            WS_OVERLAPPEDWINDOW,
            140,
            140,
            480,
            360,
            None,
            Some(menu),
            Some(instance.into()),
            None,
        )?;
        let edit = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            w!("EDIT"),
            w!(""),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            20,
            240,
            340,
            32,
            Some(source),
            Some(HMENU(201usize as *mut _)),
            Some(instance.into()),
            None,
        )?;
        let _ = ShowWindow(source, SW_SHOW);
        let _ = SetFocus(Some(edit));
        let _ = UpdateWindow(source);
        let cover = CreateWindowExW(
            WS_EX_NOACTIVATE,
            w!("FramesQACover"),
            w!("Frames QA cover"),
            WS_OVERLAPPEDWINDOW,
            140,
            140,
            480,
            360,
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        let _ = ShowWindow(cover, SW_SHOWNOACTIVATE);
        SetWindowPos(
            cover,
            Some(HWND_TOPMOST),
            140,
            140,
            480,
            360,
            SWP_NOACTIVATE,
        )?;
        let _ = UpdateWindow(cover);
        let end = std::time::Instant::now() + std::time::Duration::from_secs(300);
        let mut msg = MSG::default();
        while std::time::Instant::now() < end {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    let _ = DestroyWindow(cover);
                    let _ = DestroyWindow(source);
                    let _ = DeleteObject(red.into());
                    let _ = DeleteObject(blue.into());
                    return Ok(());
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let _ = DestroyWindow(cover);
        let _ = DestroyWindow(source);
        let _ = DeleteObject(red.into());
        let _ = DeleteObject(blue.into());
    }
    Ok(())
}
#[cfg(not(windows))]
fn main() {
    eprintln!("Use native/macos/Tests/WindowFixture.swift on macOS")
}
