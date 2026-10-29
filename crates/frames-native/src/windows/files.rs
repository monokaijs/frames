#![allow(unsafe_op_in_unsafe_fn)]
use super::wide;
use std::path::PathBuf;
use windows::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
    },
    core::*,
};
struct ShelfState {
    files: Vec<PathBuf>,
    events: tokio::sync::mpsc::UnboundedSender<crate::ClientEvent>,
}
pub struct Shelf {
    hwnd: HWND,
}
impl Shelf {
    pub unsafe fn new(
        events: tokio::sync::mpsc::UnboundedSender<crate::ClientEvent>,
    ) -> anyhow::Result<Self> {
        unsafe {
            let instance = GetModuleHandleW(None)?;
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(shelf_proc),
                hInstance: instance.into(),
                lpszClassName: w!("FramesFileShelf"),
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                hbrBackground: HBRUSH((COLOR_WINDOW.0 + 1) as *mut _),
                ..Default::default()
            });
            let state = Box::into_raw(Box::new(ShelfState {
                files: vec![],
                events,
            }));
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                w!("FramesFileShelf"),
                w!("Frames received files — drag a file to another app"),
                WS_OVERLAPPEDWINDOW,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                600,
                320,
                None,
                None,
                Some(instance.into()),
                Some(state.cast()),
            )?;
            let _ = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("BUTTON"),
                w!("Cancel transfers"),
                WS_CHILD | WS_VISIBLE,
                12,
                8,
                160,
                28,
                Some(hwnd),
                Some(HMENU(std::ptr::dangling_mut())),
                Some(instance.into()),
                None,
            );
            Ok(Self { hwnd })
        }
    }
    pub unsafe fn add(&mut self, path: PathBuf) {
        unsafe {
            let state = &mut *(GetWindowLongPtrW(self.hwnd, GWLP_USERDATA) as *mut ShelfState);
            state.files.push(path);
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            let _ = InvalidateRect(Some(self.hwnd), None, true);
        }
    }
    pub unsafe fn progress(&mut self, name: String, done: u64, total: u64) {
        unsafe {
            let percent = if total == 0 {
                0
            } else {
                done.saturating_mul(100) / total
            };
            let title = wide(&format!("Frames transfers — {name} ({percent}%)"));
            let _ = SetWindowTextW(self.hwnd, PCWSTR(title.as_ptr()));
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
        }
    }
}
impl Drop for Shelf {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}
unsafe extern "system" fn shelf_proc(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if message == WM_NCCREATE {
        let create = &*(l.0 as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
    }
    let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ShelfState;
    if pointer.is_null() {
        return DefWindowProcW(hwnd, message, w, l);
    }
    match message {
        WM_COMMAND if w.0 & 0xffff == 1 => {
            let _ = (&*pointer).events.send(crate::ClientEvent::CancelTransfers);
            return LRESULT(0);
        }
        WM_CLOSE => {
            let _ = ShowWindow(hwnd, SW_HIDE);
            return LRESULT(0);
        }
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            let dc = BeginPaint(hwnd, &mut paint);
            for (index, path) in (&*pointer).files.iter().enumerate() {
                let name = wide(&path.file_name().unwrap_or_default().to_string_lossy());
                let _ = TextOutW(dc, 14, 50 + index as i32 * 32, &name[..name.len() - 1]);
            }
            let _ = EndPaint(hwnd, &paint);
            return LRESULT(0);
        }
        WM_LBUTTONDOWN => {
            let row = (((l.0 >> 16) as u16 as i16) as i32 - 50) / 32;
            if row >= 0
                && let Some(path) = (&*pointer).files.get(row as usize).cloned()
                && let Err(error) = super::drag::begin_local_file(&path)
            {
                tracing::warn!(%error,"file drag");
            }
            return LRESULT(0);
        }
        WM_NCDESTROY => {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            drop(Box::from_raw(pointer));
        }
        _ => {}
    }
    DefWindowProcW(hwnd, message, w, l)
}

struct Prompt {
    edit: HWND,
    result: Option<String>,
    done: bool,
}
unsafe extern "system" fn prompt_proc(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Prompt;
    if !pointer.is_null() {
        let state = &mut *pointer;
        match message {
            WM_CLOSE => {
                state.done = true;
                return LRESULT(0);
            }
            WM_COMMAND if w.0 & 0xffff == 1 => {
                let mut text = vec![0u16; GetWindowTextLengthW(state.edit).max(0) as usize + 1];
                let n = GetWindowTextW(state.edit, &mut text);
                if n > 0 {
                    state.result = Some(String::from_utf16_lossy(&text[..n as usize]));
                }
                state.done = true;
                return LRESULT(0);
            }
            _ => {}
        }
    }
    DefWindowProcW(hwnd, message, w, l)
}
pub unsafe fn prompt_path(owner: HWND) -> Option<String> {
    unsafe {
        let instance = GetModuleHandleW(None).ok()?;
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(prompt_proc),
            hInstance: instance.into(),
            lpszClassName: w!("FramesFilePrompt"),
            hbrBackground: HBRUSH((COLOR_WINDOW.0 + 1) as *mut _),
            ..Default::default()
        });
        let hwnd = CreateWindowExW(
            WS_EX_DLGMODALFRAME,
            w!("FramesFilePrompt"),
            w!("Fetch file — path relative to the remote home folder"),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
            200,
            200,
            580,
            145,
            Some(owner),
            None,
            Some(instance.into()),
            None,
        )
        .ok()?;
        let edit = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            w!("EDIT"),
            w!("Downloads/"),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
            12,
            15,
            540,
            25,
            Some(hwnd),
            None,
            Some(instance.into()),
            None,
        )
        .ok()?;
        let _ = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("BUTTON"),
            w!("Download"),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            435,
            55,
            115,
            28,
            Some(hwnd),
            Some(HMENU(std::ptr::dangling_mut())),
            Some(instance.into()),
            None,
        );
        let mut state = Prompt {
            edit,
            result: None,
            done: false,
        };
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, &mut state as *mut _ as isize);
        let _ = EnableWindow(owner, false);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetFocus(Some(edit));
        let mut message = MSG::default();
        while !state.done && GetMessageW(&mut message, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        let _ = EnableWindow(owner, true);
        let _ = DestroyWindow(hwnd);
        state.result
    }
}
