//! A persistent session menu remains available when the host has no app windows.
use super::wide;
use crate::ClientEvent;
use frames_core::model::ViewMode;
use windows::{
    Win32::{
        Foundation::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{Shell::*, WindowsAndMessaging::*},
    },
    core::*,
};

const CALLBACK: u32 = WM_APP + 73;
struct Context {
    events: tokio::sync::mpsc::UnboundedSender<ClientEvent>,
    mode: ViewMode,
    adb: bool,
}
pub struct SessionControl {
    hwnd: HWND,
    context: Box<Context>,
}
impl SessionControl {
    pub fn new(events: tokio::sync::mpsc::UnboundedSender<ClientEvent>) -> anyhow::Result<Self> {
        unsafe {
            let instance = GetModuleHandleW(None)?;
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(procedure),
                hInstance: instance.into(),
                lpszClassName: w!("FramesSessionControl"),
                ..Default::default()
            });
            let mut context = Box::new(Context {
                events,
                mode: ViewMode::Windows,
                adb: false,
            });
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("FramesSessionControl"),
                w!("Frames session"),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                Some(instance.into()),
                Some((&mut *context as *mut Context).cast()),
            )?;
            let mut icon = NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: hwnd,
                uID: 1,
                uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
                uCallbackMessage: CALLBACK,
                hIcon: LoadIconW(None, IDI_APPLICATION)?,
                ..Default::default()
            };
            let tip = wide("Frames session — view mode and devices");
            icon.szTip[..tip.len()].copy_from_slice(&tip);
            if !Shell_NotifyIconW(NIM_ADD, &icon).as_bool() {
                let _ = DestroyWindow(hwnd);
                anyhow::bail!("Unable to create the Frames session notification icon");
            }
            Ok(Self { hwnd, context })
        }
    }
    pub fn update(&mut self, mode: ViewMode, adb: bool) {
        self.context.mode = mode;
        self.context.adb = adb;
    }
}
impl Drop for SessionControl {
    fn drop(&mut self) {
        unsafe {
            let icon = NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: self.hwnd,
                uID: 1,
                ..Default::default()
            };
            let _ = Shell_NotifyIconW(NIM_DELETE, &icon);
            let _ = DestroyWindow(self.hwnd);
        }
    }
}
unsafe extern "system" fn procedure(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        if message == WM_NCCREATE {
            let create = &*(l.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        }
        let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Context;
        if !pointer.is_null()
            && message == CALLBACK
            && matches!(l.0 as u32, WM_LBUTTONUP | WM_RBUTTONUP | WM_CONTEXTMENU)
        {
            let context = &*pointer;
            if let Ok(menu) = CreatePopupMenu() {
                let checked = |yes| if yes { MF_CHECKED } else { MENU_ITEM_FLAGS(0) };
                let _ = AppendMenuW(
                    menu,
                    MF_STRING | checked(context.mode == ViewMode::Windows),
                    1,
                    w!("Per-window view"),
                );
                let _ = AppendMenuW(
                    menu,
                    MF_STRING | checked(context.mode == ViewMode::Desktop),
                    2,
                    w!("Full desktop view"),
                );
                let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
                let _ = AppendMenuW(
                    menu,
                    MF_STRING | checked(context.adb),
                    3,
                    w!("Android ADB bridge"),
                );
                let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
                let _ = AppendMenuW(menu, MF_STRING, 4, w!("Disconnect"));
                let mut point = POINT::default();
                let _ = GetCursorPos(&mut point);
                let _ = SetForegroundWindow(hwnd);
                let selected = TrackPopupMenu(
                    menu,
                    TPM_RETURNCMD | TPM_NONOTIFY,
                    point.x,
                    point.y,
                    Some(0),
                    hwnd,
                    None,
                )
                .0;
                let _ = context.events.send(ClientEvent::ReleaseInput);
                match selected {
                    1 | 2 => {
                        let _ = context.events.send(ClientEvent::ViewMode(if selected == 1 {
                            ViewMode::Windows
                        } else {
                            ViewMode::Desktop
                        }));
                    }
                    3 => {
                        let _ = context.events.send(ClientEvent::AdbBridge {
                            enabled: !context.adb,
                        });
                    }
                    4 => {
                        let _ = context.events.send(ClientEvent::Disconnected);
                    }
                    _ => {}
                }
                let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
                let _ = DestroyMenu(menu);
            }
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, message, w, l)
    }
}
