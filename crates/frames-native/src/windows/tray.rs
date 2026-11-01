//! Notification icons and icon-bearing menus shared by the launcher and sessions.
#![allow(unsafe_op_in_unsafe_fn)]
use super::wide;
use std::{collections::HashMap, sync::mpsc, thread};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{Shell::*, WindowsAndMessaging::*},
    },
    core::*,
};

const CALLBACK: u32 = WM_APP + 74;
const UPDATE: u32 = WM_APP + 75;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TrayGlyph {
    App,
    Window,
    Show,
    Hide,
    Settings,
    Display,
    Cursor,
    Keyboard,
    Refresh,
    Folder,
    Devices,
    Network,
    Power,
    Info,
}
impl TrayGlyph {
    fn character(self) -> char {
        match self {
            Self::App => '\u{e8a7}',
            Self::Window => '\u{e737}',
            Self::Show => '\u{e890}',
            Self::Hide => '\u{e8a7}',
            Self::Settings => '\u{e713}',
            Self::Display => '\u{e7f4}',
            Self::Cursor => '\u{e962}',
            Self::Keyboard => '\u{e765}',
            Self::Refresh => '\u{e72c}',
            Self::Folder => '\u{e8b7}',
            Self::Devices => '\u{e772}',
            Self::Network => '\u{e968}',
            Self::Power => '\u{e7e8}',
            Self::Info => '\u{e946}',
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrayItem {
    pub id: u32,
    pub title: String,
    pub glyph: TrayGlyph,
    pub checked: bool,
    pub enabled: bool,
    pub children: Vec<TrayItem>,
}
impl TrayItem {
    pub fn new(id: u32, title: impl Into<String>, glyph: TrayGlyph) -> Self {
        Self {
            id,
            title: title.into(),
            glyph,
            checked: false,
            enabled: true,
            children: vec![],
        }
    }
    pub fn checked(mut self, checked: bool) -> Self {
        self.checked = checked;
        self
    }
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
    pub fn children(mut self, children: Vec<Self>) -> Self {
        self.children = children;
        self
    }
    pub fn separator() -> Self {
        Self::new(0, "", TrayGlyph::App)
    }
}
#[derive(Clone, PartialEq, Eq)]
struct State {
    items: Vec<TrayItem>,
    active: bool,
    tip: String,
}

/// Owns its message loop so a popup never blocks video delivery or Slint.
pub struct Tray {
    hwnd: usize,
    updates: mpsc::Sender<State>,
    pub commands: mpsc::Receiver<u32>,
    worker: Option<thread::JoinHandle<()>>,
    last: Option<State>,
}
impl Tray {
    pub fn new(title: &str) -> anyhow::Result<Self> {
        let (updates, receiver) = mpsc::channel();
        let (commands_tx, commands) = mpsc::channel();
        let (ready_tx, ready) = mpsc::sync_channel(1);
        let title = title.to_owned();
        let worker = thread::spawn(move || unsafe {
            let result = (|| -> anyhow::Result<_> {
                let instance = GetModuleHandleW(None)?;
                RegisterClassW(&WNDCLASSW {
                    lpfnWndProc: Some(procedure),
                    hInstance: instance.into(),
                    lpszClassName: w!("FramesTray"),
                    ..Default::default()
                });
                let mut context = Box::new(Context {
                    state: State {
                        items: vec![],
                        active: false,
                        tip: title.clone(),
                    },
                    receiver,
                    commands: commands_tx,
                    icons: [app_icon(false)?, app_icon(true)?],
                    bitmaps: HashMap::new(),
                    taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
                    added: false,
                });
                // A hidden top-level window receives Explorer's TaskbarCreated broadcast.
                let hwnd = CreateWindowExW(
                    WS_EX_TOOLWINDOW,
                    w!("FramesTray"),
                    PCWSTR(wide(&title).as_ptr()),
                    WINDOW_STYLE(0),
                    0,
                    0,
                    0,
                    0,
                    None,
                    None,
                    Some(instance.into()),
                    Some((&mut *context as *mut Context).cast()),
                )?;
                context.notify(hwnd, NIM_ADD);
                SetTimer(Some(hwnd), 1, 2000, None);
                Ok((hwnd, context))
            })();
            match result {
                Ok((hwnd, _context)) => {
                    let _ = ready_tx.send(Ok(hwnd.0 as usize));
                    let mut message = MSG::default();
                    while GetMessageW(&mut message, None, 0, 0).as_bool() {
                        let _ = TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error.to_string()));
                }
            }
        });
        let hwnd = ready
            .recv()
            .map_err(|_| anyhow::anyhow!("Tray thread stopped"))?
            .map_err(anyhow::Error::msg)?;
        Ok(Self {
            hwnd,
            updates,
            commands,
            worker: Some(worker),
            last: None,
        })
    }
    pub fn update(&mut self, items: Vec<TrayItem>, active: bool, tip: impl Into<String>) {
        let state = State {
            items,
            active,
            tip: tip.into(),
        };
        if self.last.as_ref() == Some(&state) {
            return;
        }
        self.last = Some(state.clone());
        if self.updates.send(state).is_ok() {
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(self.hwnd as *mut _)),
                    UPDATE,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
    }
}
impl Drop for Tray {
    fn drop(&mut self) {
        unsafe {
            let _ = PostMessageW(
                Some(HWND(self.hwnd as *mut _)),
                WM_CLOSE,
                WPARAM(0),
                LPARAM(0),
            );
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
struct Context {
    state: State,
    receiver: mpsc::Receiver<State>,
    commands: mpsc::Sender<u32>,
    icons: [HICON; 2],
    bitmaps: HashMap<TrayGlyph, HBITMAP>,
    taskbar_created: u32,
    added: bool,
}
impl Context {
    unsafe fn notify(&mut self, hwnd: HWND, action: NOTIFY_ICON_MESSAGE) {
        let mut icon = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: CALLBACK,
            hIcon: self.icons[usize::from(self.state.active)],
            ..Default::default()
        };
        let tip: Vec<_> = self
            .state
            .tip
            .encode_utf16()
            .take(icon.szTip.len() - 1)
            .collect();
        icon.szTip[..tip.len()].copy_from_slice(&tip);
        self.added = Shell_NotifyIconW(action, &icon).as_bool();
    }
    unsafe fn menu(&mut self, items: &[TrayItem]) -> Result<HMENU> {
        let menu = CreatePopupMenu()?;
        for item in items {
            if item.title.is_empty() {
                let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
                continue;
            }
            let flags = MF_STRING
                | if item.checked {
                    MF_CHECKED
                } else {
                    MF_UNCHECKED
                }
                | if item.enabled { MF_ENABLED } else { MF_GRAYED };
            let child = if item.children.is_empty() {
                None
            } else {
                Some(self.menu(&item.children)?)
            };
            let _ = AppendMenuW(
                menu,
                flags | if child.is_some() { MF_POPUP } else { MF_STRING },
                child.map_or(item.id as usize, |m| m.0 as usize),
                PCWSTR(wide(&item.title.replace('&', "&&")).as_ptr()),
            );
            if let std::collections::hash_map::Entry::Vacant(slot) = self.bitmaps.entry(item.glyph)
                && let Ok(bitmap) = glyph_bitmap(item.glyph)
            {
                slot.insert(bitmap);
            }
            if let Some(&bitmap) = self.bitmaps.get(&item.glyph) {
                let info = MENUITEMINFOW {
                    cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                    fMask: MIIM_BITMAP,
                    hbmpItem: bitmap,
                    ..Default::default()
                };
                let _ =
                    SetMenuItemInfoW(menu, (GetMenuItemCount(Some(menu)) - 1) as u32, true, &info);
            }
        }
        Ok(menu)
    }
}
impl Drop for Context {
    fn drop(&mut self) {
        unsafe {
            for icon in self.icons {
                let _ = DestroyIcon(icon);
            }
            for bitmap in self.bitmaps.values() {
                let _ = DeleteObject((*bitmap).into());
            }
        }
    }
}
unsafe extern "system" fn procedure(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if message == WM_NCCREATE {
        let create = &*(l.0 as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
    }
    let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Context;
    if !pointer.is_null() {
        if message == UPDATE {
            let context = &mut *pointer;
            if let Some(state) = context.receiver.try_iter().last() {
                context.state = state;
                context.notify(hwnd, if context.added { NIM_MODIFY } else { NIM_ADD });
            }
            return LRESULT(0);
        }
        if message == (*pointer).taskbar_created || message == WM_TIMER && !(*pointer).added {
            (*pointer).notify(hwnd, NIM_ADD);
            return LRESULT(0);
        }
        if message == CALLBACK
            && matches!(
                l.0 as u32,
                WM_LBUTTONUP | WM_RBUTTONUP | WM_CONTEXTMENU | NIN_SELECT
            )
        {
            let items = (*pointer).state.items.clone();
            if let Ok(menu) = (*pointer).menu(&items) {
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
                if selected != 0 {
                    let _ = (*pointer).commands.send(selected as u32);
                }
                let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
                let _ = DestroyMenu(menu);
            }
            return LRESULT(0);
        }
        if message == WM_CLOSE {
            let data = NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: hwnd,
                uID: 1,
                ..Default::default()
            };
            let _ = Shell_NotifyIconW(NIM_DELETE, &data);
            let _ = DestroyWindow(hwnd);
            return LRESULT(0);
        }
        if message == WM_DESTROY {
            PostQuitMessage(0);
            return LRESULT(0);
        }
    }
    DefWindowProcW(hwnd, message, w, l)
}

unsafe fn bitmap(width: i32, height: i32) -> Result<(HBITMAP, *mut u8)> {
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
    let mut bits = std::ptr::null_mut();
    let handle = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
    Ok((handle, bits.cast()))
}
unsafe fn glyph_bitmap(glyph: TrayGlyph) -> Result<HBITMAP> {
    let (handle, bits) = bitmap(20, 20)?;
    let dc = CreateCompatibleDC(None);
    let old = SelectObject(dc, handle.into());
    let font = CreateFontW(
        16,
        0,
        0,
        0,
        400,
        0,
        0,
        0,
        DEFAULT_CHARSET,
        OUT_DEFAULT_PRECIS,
        CLIP_DEFAULT_PRECIS,
        ANTIALIASED_QUALITY,
        0,
        w!("Segoe MDL2 Assets"),
    );
    let old_font = SelectObject(dc, font.into());
    SetBkColor(dc, COLORREF(0));
    SetTextColor(dc, COLORREF(0xffffff));
    if glyph == TrayGlyph::Hide {
        PatBlt(dc, 0, 0, 20, 20, BLACKNESS).ok()?;
        super::caption::draw_eye_slash(dc, 2, 2, 96);
    } else {
        let mut text = wide(&glyph.character().to_string());
        text.pop();
        DrawTextW(
            dc,
            &mut text,
            &mut RECT {
                left: 0,
                top: 0,
                right: 20,
                bottom: 20,
            },
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );
    }
    SelectObject(dc, old_font);
    SelectObject(dc, old);
    let _ = DeleteObject(font.into());
    let _ = DeleteDC(dc);
    for pixel in std::slice::from_raw_parts_mut(bits, 20 * 20 * 4).chunks_exact_mut(4) {
        let alpha = pixel[0].max(pixel[1]).max(pixel[2]);
        let shade = (70 * alpha as u16 / 255) as u8;
        pixel.copy_from_slice(&[shade, shade, shade, alpha]);
    }
    Ok(handle)
}
unsafe fn app_icon(active: bool) -> anyhow::Result<HICON> {
    let image = image::load_from_memory(include_bytes!("../../../../assets/icons/Frames.png"))?
        .resize_exact(32, 32, image::imageops::FilterType::Lanczos3)
        .to_rgba8();
    let (color, bits) = bitmap(32, 32)?;
    let output = std::slice::from_raw_parts_mut(bits, 32 * 32 * 4);
    for (index, pixel) in image.pixels().enumerate() {
        let x = index % 32;
        let y = index / 32;
        let dot = (x as i32 - 25).pow(2) + (y as i32 - 25).pow(2) <= 36;
        let [r, g, b, a] = if dot {
            if active {
                [34, 173, 112, 255]
            } else {
                [148, 153, 163, 255]
            }
        } else {
            pixel.0
        };
        output[index * 4..index * 4 + 4].copy_from_slice(&[
            (b as u16 * a as u16 / 255) as u8,
            (g as u16 * a as u16 / 255) as u8,
            (r as u16 * a as u16 / 255) as u8,
            a,
        ]);
    }
    let mask = CreateBitmap(32, 32, 1, 1, None);
    let icon = CreateIconIndirect(&ICONINFO {
        fIcon: true.into(),
        hbmColor: color,
        hbmMask: mask,
        ..Default::default()
    });
    let _ = DeleteObject(color.into());
    let _ = DeleteObject(mask.into());
    Ok(icon?)
}
