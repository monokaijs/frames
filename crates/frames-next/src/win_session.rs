//! A Windows viewer's connection menu, as v1's: the chevron beside the
//! title bar's minimize button opens the session menu (`crate::control`),
//! with "This window" options, the window list, image quality, view,
//! devices and disconnect. "Fetch remote file…" asks for a path in its own
//! window, and while every window is hidden a small "Frames connection"
//! window keeps the session reachable.

use crate::control::{Command, Snapshot, View, WindowAction};
use std::sync::atomic::{AtomicBool, Ordering};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmGetWindowAttribute};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    EM_SETSEL, TOOLTIPS_CLASSW, TTF_IDISHWND, TTF_SUBCLASS, TTM_ADDTOOLW, TTS_ALWAYSTIP,
    TTTOOLINFOW, WM_MOUSELEAVE,
};
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SetFocus, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, PCWSTR, PWSTR, w};

/// The WM_COMMAND id the caption button sends its window.
pub const MENU_BUTTON: usize = 0xA009;
pub const HIDE_BUTTON: usize = 0xA00A;
const HIDE_ROLE: isize = 0x100;

/// What a menu item does.
#[derive(Clone, Debug)]
pub enum Choice {
    Command(Command),
    /// An item of the host application's menu, by the host's id.
    Host(u32),
    /// "Fetch remote file…" for this window.
    Fetch(u64),
    /// "Window compatibility": the shared game's record (one window).
    Compatibility(String),
}

/// What the menu knows about the window it opens from.
#[derive(Clone, Default)]
pub struct Context {
    /// The window's id in the menus; `None` from the connection window.
    pub window: Option<u64>,
    /// The host's platform (Control/Command swapping follows it).
    pub host_platform: String,
    /// The host application's menus for this window.
    pub host_items: Vec<crate::menus::Item>,
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// v1's menu glyphs (Segoe MDL2 Assets, `frames-native` tray_model).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Glyph {
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

impl Glyph {
    fn character(self) -> char {
        match self {
            Self::Window => '\u{e737}',
            Self::Show => '\u{e890}',
            Self::Hide => '\u{ed1a}',
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

    /// The glyph v1 gave the item with this label, if any.
    fn for_label(label: &str) -> Option<Self> {
        Some(match label {
            "Application menu"
            | "Show remote window header"
            | "Windows"
            | "Per-window view"
            | "No remote windows available"
            | "No remote window" => Self::Window,
            "Show all windows" => Self::Show,
            "Hide all windows" | "Ignore / hide this window" => Self::Hide,
            "This window" | "Window options" => Self::Settings,
            "Fill window" | "Resize remote app" | "Stretch locally" | "Image quality" | "View"
            | "Full desktop view" => Self::Display,
            "Relative mouse · Ctrl+Alt+F11" | "Show remote cursor" => Self::Cursor,
            "Swap Control / Command" | "Physical keys" => Self::Keyboard,
            "Refresh application menus" | "Refresh devices" => Self::Refresh,
            "Fetch remote file…" => Self::Folder,
            "Devices" | "Gamepads" | "USB devices" => Self::Devices,
            "Android ADB bridge" | "Connecting" => Self::Network,
            "Disconnect" => Self::Power,
            "Window compatibility" | "Debug overlay · Ctrl+Alt+F10" => Self::Info,
            _ if label.starts_with("Connected · ") => Self::Network,
            _ if ["High (", "Medium (", "Low ("]
                .iter()
                .any(|p| label.starts_with(p)) =>
            {
                Self::Display
            }
            _ => return None,
        })
    }

    /// The glyph drawn white-on-black into a 32-bit bitmap of `size` pixels,
    /// then turned into v1's grey with the glyph as alpha.
    unsafe fn bitmap(self, size: i32) -> Option<HBITMAP> {
        unsafe {
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: size,
                    biHeight: -size,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            let bitmap = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
            let dc = CreateCompatibleDC(None);
            let old = SelectObject(dc, bitmap.into());
            let font = CreateFontW(
                size * 4 / 5,
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
            let _ = PatBlt(dc, 0, 0, size, size, BLACKNESS);
            let mut text: Vec<u16> = self.character().to_string().encode_utf16().collect();
            DrawTextW(
                dc,
                &mut text,
                &mut RECT {
                    left: 0,
                    top: 0,
                    right: size,
                    bottom: size,
                },
                DT_CENTER | DT_VCENTER | DT_SINGLELINE,
            );
            let _ = GdiFlush();
            SelectObject(dc, old_font);
            SelectObject(dc, old);
            let _ = DeleteObject(font.into());
            let _ = DeleteDC(dc);
            let pixels =
                std::slice::from_raw_parts_mut(bits.cast::<u8>(), (size * size * 4) as usize);
            for pixel in pixels.chunks_exact_mut(4) {
                let alpha = pixel[0].max(pixel[1]).max(pixel[2]);
                // Premultiplied grey (v1's 70).
                let shade = (70 * u16::from(alpha) / 255) as u8;
                pixel.copy_from_slice(&[shade, shade, shade, alpha]);
            }
            Some(bitmap)
        }
    }
}

/// Builds popup menus whose item ids index `choices`.
struct Builder {
    choices: Vec<Choice>,
    /// Glyph bitmaps made for this menu, deleted with it.
    bitmaps: std::collections::HashMap<Glyph, HBITMAP>,
    /// Glyph bitmaps' size in pixels (at the menu window's DPI).
    glyph_size: i32,
    /// The next item's glyph when its label has none of its own (a
    /// window row's shown or hidden eye).
    next_glyph: Option<Glyph>,
}

impl Drop for Builder {
    fn drop(&mut self) {
        for bitmap in self.bitmaps.values() {
            let _ = unsafe { DeleteObject((*bitmap).into()) };
        }
    }
}

impl Builder {
    fn new(owner: HWND) -> Self {
        let dpi = unsafe { GetDpiForWindow(owner) }.max(96) as i32;
        Self {
            choices: Vec::new(),
            bitmaps: std::collections::HashMap::new(),
            glyph_size: (16 * dpi + 48) / 96,
            next_glyph: None,
        }
    }

    /// Gives the item just appended to `menu` its glyph (`label`'s, or the
    /// one set for it).
    fn glyph(&mut self, menu: HMENU, label: &str) {
        let Some(glyph) = self.next_glyph.take().or_else(|| Glyph::for_label(label)) else {
            return;
        };
        let size = self.glyph_size;
        let bitmap = match self.bitmaps.get(&glyph) {
            Some(bitmap) => *bitmap,
            None => match unsafe { glyph.bitmap(size) } {
                Some(bitmap) => *self.bitmaps.entry(glyph).or_insert(bitmap),
                None => return,
            },
        };
        unsafe {
            let info = MENUITEMINFOW {
                cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_BITMAP,
                hbmpItem: bitmap,
                ..Default::default()
            };
            let last = GetMenuItemCount(Some(menu)) - 1;
            if last >= 0 {
                let _ = SetMenuItemInfoW(menu, last as u32, true, &info);
            }
        }
    }

    fn item(&mut self, menu: HMENU, title: &str, choice: Option<Choice>, checked: bool) {
        let mut flags = MF_STRING;
        if checked {
            flags |= MF_CHECKED;
        }
        let id = match choice {
            Some(choice) => {
                self.choices.push(choice);
                self.choices.len()
            }
            None => {
                flags |= MF_GRAYED;
                0
            }
        };
        let label = title;
        let title = wide(title);
        unsafe {
            let _ = AppendMenuW(menu, flags, id, PCWSTR(title.as_ptr()));
        }
        self.glyph(menu, label);
    }

    fn command(&mut self, menu: HMENU, title: &str, command: Command, checked: bool) {
        self.item(menu, title, Some(Choice::Command(command)), checked);
    }

    fn submenu(&mut self, menu: HMENU, title: &str, child: HMENU) {
        let label = title;
        let title = wide(title);
        unsafe {
            let _ = AppendMenuW(
                menu,
                MF_STRING | MF_POPUP,
                child.0 as usize,
                PCWSTR(title.as_ptr()),
            );
        }
        self.glyph(menu, label);
    }

    fn separator(menu: HMENU) {
        unsafe {
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        }
    }

    fn popup() -> HMENU {
        unsafe { CreatePopupMenu().unwrap_or_default() }
    }

    /// A window's own options (v1's "This window" / "Window options").
    fn window_options(
        &mut self,
        snapshot: &Snapshot,
        id: u64,
        context: &Context,
        host_items: &[crate::menus::Item],
    ) -> HMENU {
        let menu = Self::popup();
        let Some(window) = snapshot.windows.iter().find(|w| w.id == id) else {
            self.item(menu, "No remote window", None, false);
            return menu;
        };
        let prefs = &window.prefs;
        let action = |action| Command::Window { id, action };
        self.command(
            menu,
            "Fill window",
            action(WindowAction::ToggleFill),
            prefs.fill,
        );
        // One window with isolated input (games), as v1's shared sessions:
        // relative mouse and the game's compatibility, keys stay physical.
        if let Some(compatibility) = &snapshot.compatibility {
            self.command(
                menu,
                "Relative mouse · Ctrl+Alt+F11",
                action(WindowAction::ToggleRelative),
                crate::win::relative(),
            );
            self.item(
                menu,
                "Window compatibility",
                Some(Choice::Compatibility(compatibility.clone())),
                false,
            );
            self.command(
                menu,
                "Debug overlay · Ctrl+Alt+F10",
                Command::Stats,
                snapshot.stats,
            );
            return menu;
        }
        if window.desktop {
            self.item(menu, "Show remote window header", None, false);
        } else {
            self.command(
                menu,
                "Show remote window header",
                action(WindowAction::ToggleHeader),
                prefs.header,
            );
        }
        let resizable = window.resizable && !window.desktop;
        if resizable {
            self.command(
                menu,
                "Resize remote app",
                action(WindowAction::ResizeRemote),
                prefs.resize_remote,
            );
        } else {
            self.item(menu, "Resize remote app", None, false);
        }
        self.command(
            menu,
            "Stretch locally",
            action(WindowAction::StretchLocal),
            !prefs.resize_remote || !resizable,
        );
        self.command(
            menu,
            "Swap Control / Command",
            action(WindowAction::ToggleSwap),
            prefs.swapped(&context.host_platform),
        );
        self.command(
            menu,
            "Physical keys",
            action(WindowAction::TogglePhysical),
            prefs.physical,
        );
        if window.desktop {
            self.item(menu, "Refresh application menus", None, false);
        } else {
            self.command(
                menu,
                "Refresh application menus",
                action(WindowAction::RefreshMenus),
                false,
            );
        }
        self.item(menu, "Fetch remote file…", Some(Choice::Fetch(id)), false);
        self.command(
            menu,
            "Debug overlay · Ctrl+Alt+F10",
            Command::Stats,
            snapshot.stats,
        );
        if !host_items.is_empty() {
            let application = self.host_menu(host_items, 0);
            self.submenu(menu, "Application menu", application);
        }
        menu
    }

    /// The host application's menu bar as a submenu tree.
    fn host_menu(&mut self, items: &[crate::menus::Item], depth: usize) -> HMENU {
        let menu = Self::popup();
        if depth >= 8 {
            return menu;
        }
        for item in items.iter().take(200) {
            if item.separator || item.title.is_empty() {
                Self::separator(menu);
            } else if !item.children.is_empty() {
                let child = self.host_menu(&item.children, depth + 1);
                self.submenu(menu, &item.title, child);
            } else {
                let choice = (item.enabled && item.id > 0).then_some(Choice::Host(item.id));
                self.item(menu, &item.title, choice, item.checked);
            }
        }
        menu
    }

    /// The whole session menu, in v1's order.
    fn session(&mut self, snapshot: &Snapshot, context: &Context) -> HMENU {
        let menu = Self::popup();
        let header = if snapshot.connected {
            format!("Connected · {}", snapshot.peer)
        } else {
            "Connecting".to_string()
        };
        self.item(menu, &header, None, false);
        Self::separator(menu);
        let per_window = snapshot.view == View::Windows;
        if let Some(id) = context.window {
            if per_window {
                self.command(
                    menu,
                    "Ignore / hide this window",
                    Command::Show { id, shown: false },
                    false,
                );
            } else {
                self.item(menu, "Ignore / hide this window", None, false);
            }
            let options = self.window_options(snapshot, id, context, &context.host_items);
            self.submenu(menu, "This window", options);
        }
        let list = Self::popup();
        if !snapshot.windows.iter().any(|w| !w.popup) {
            self.item(list, "No remote windows available", None, false);
        }
        for window in snapshot.windows.iter().filter(|w| !w.popup) {
            let title = if window.app.is_empty() || window.desktop {
                window.title.clone()
            } else {
                format!("{} · {}", window.title, window.app)
            };
            // v1: an open eye for a shown window, a closed one for a hidden.
            self.next_glyph = Some(if window.shown {
                Glyph::Show
            } else {
                Glyph::Hide
            });
            if per_window {
                self.command(
                    list,
                    &title,
                    Command::Show {
                        id: window.id,
                        shown: !window.shown,
                    },
                    window.shown,
                );
            } else {
                self.item(list, &title, None, window.shown);
            }
        }
        self.submenu(menu, "Windows", list);
        // Menus and tooltips come and go with their window: not listed.
        let listed = snapshot.windows.iter().filter(|w| !w.popup).count();
        let shown = snapshot
            .windows
            .iter()
            .filter(|w| w.shown && !w.popup)
            .count();
        if per_window && shown < listed {
            self.command(
                menu,
                "Show all windows",
                Command::ShowAll { shown: true },
                false,
            );
        } else {
            self.item(menu, "Show all windows", None, false);
        }
        if per_window && shown > 0 {
            self.command(
                menu,
                "Hide all windows",
                Command::ShowAll { shown: false },
                false,
            );
        } else {
            self.item(menu, "Hide all windows", None, false);
        }
        Self::separator(menu);
        let quality = Self::popup();
        for (index, title) in [
            "High (sharp text, native resolution)",
            "Medium (balanced)",
            "Low (lower bandwidth)",
        ]
        .into_iter()
        .enumerate()
        {
            self.command(
                quality,
                title,
                Command::Quality {
                    quality: index as u8,
                },
                snapshot.quality == index as u8,
            );
        }
        self.submenu(menu, "Image quality", quality);
        if snapshot.windows.iter().any(|w| !w.popup) {
            let options = Self::popup();
            for window in snapshot.windows.iter().filter(|w| !w.popup) {
                // Only open windows have host menus here.
                let items = if Some(window.id) == context.window {
                    context.host_items.as_slice()
                } else {
                    &[]
                };
                let child = self.window_options(snapshot, window.id, context, items);
                self.submenu(options, &window.title, child);
            }
            self.submenu(menu, "Window options", options);
        }
        let view = Self::popup();
        self.command(
            view,
            "Per-window view",
            Command::View {
                view: View::Windows,
            },
            per_window,
        );
        // "Only stream this app" (a Quick App): the host refuses the desktop.
        if snapshot.only_app {
            self.item(view, "Full desktop view", None, false);
        } else {
            self.command(
                view,
                "Full desktop view",
                Command::View {
                    view: View::Desktop,
                },
                !per_window,
            );
        }
        self.submenu(menu, "View", view);
        let devices = self.devices(snapshot);
        self.submenu(menu, "Devices", devices);
        self.command(
            menu,
            "Android ADB bridge",
            Command::Adb { on: !snapshot.adb },
            snapshot.adb,
        );
        self.command(
            menu,
            "Show remote cursor",
            Command::RemoteCursor {
                show: !snapshot.remote_cursor,
            },
            snapshot.remote_cursor,
        );
        Self::separator(menu);
        self.command(menu, "Disconnect", Command::Disconnect, false);
        menu
    }

    /// v1's Devices submenu.
    fn devices(&mut self, snapshot: &Snapshot) -> HMENU {
        let menu = Self::popup();
        let gamepads = Self::popup();
        if snapshot.gamepads.is_empty() {
            self.item(gamepads, "No mapped gamepads found", None, false);
        }
        for pad in snapshot.gamepads.iter().take(32) {
            let title = match &pad.status {
                Some(status) => format!("{} — {status}", pad.name),
                None => pad.name.clone(),
            };
            self.command(
                gamepads,
                &title,
                Command::Gamepad {
                    id: pad.id.clone(),
                    on: !pad.forwarded,
                },
                pad.forwarded,
            );
        }
        if let Some(error) = &snapshot.gamepad_error {
            self.item(gamepads, error, None, false);
        }
        self.submenu(menu, "Gamepads", gamepads);
        let usb = Self::popup();
        if snapshot.usb.is_empty() {
            self.item(
                usb,
                "No USB devices found; refresh after setup",
                None,
                false,
            );
        }
        for device in snapshot.usb.iter().take(32) {
            let title = match &device.status {
                Some(status) => format!("{} — {} ({status})", device.id, device.name),
                None => format!("{} — {}", device.id, device.name),
            };
            self.command(
                usb,
                &title,
                Command::Usb {
                    id: device.id.clone(),
                    on: !device.forwarded,
                },
                device.forwarded,
            );
        }
        if let Some(error) = &snapshot.usb_error {
            self.item(usb, error, None, false);
        }
        self.submenu(menu, "USB devices", usb);
        self.command(menu, "Refresh devices", Command::RefreshDevices, false);
        menu
    }
}

/// Shows the session menu at screen point `at` and returns what was chosen.
/// It runs a menu loop on this thread (this window's video waits).
pub fn show_menu(owner: HWND, at: POINT, context: &Context) -> Option<Choice> {
    let snapshot = crate::control::snapshot();
    let mut builder = Builder::new(owner);
    let menu = builder.session(&snapshot, context);
    let chosen = unsafe {
        let _ = SetForegroundWindow(owner);
        let id = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON | TPM_LEFTALIGN | TPM_TOPALIGN,
            at.x,
            at.y,
            None,
            owner,
            None,
        );
        // The menu loop needs a message afterwards to close cleanly.
        let _ = PostMessageW(Some(owner), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        id.0 as usize
    };
    (chosen > 0)
        .then(|| builder.choices.get(chosen - 1).cloned())
        .flatten()
}

/// Runs a choice that needs no window of its own (host menu items are the
/// caller's).
pub fn run(choice: Choice, owner: HWND) {
    match choice {
        Choice::Command(command) => crate::control::send(command),
        Choice::Fetch(id) => prompt_fetch(owner, id),
        // On its own thread: a message box on this one would stall video.
        Choice::Compatibility(text) => {
            std::thread::spawn(move || {
                let text: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
                unsafe {
                    MessageBoxW(
                        None,
                        PCWSTR(text.as_ptr()),
                        w!("Window compatibility"),
                        MB_OK | MB_ICONINFORMATION,
                    );
                }
            });
        }
        Choice::Host(_) => {}
    }
}

/// Under the caption button, or at the pointer when there is none.
pub fn menu_point(button: HWND) -> POINT {
    let mut rect = RECT::default();
    unsafe {
        if !button.is_invalid()
            && GetWindowRect(button, &mut rect).is_ok()
            && rect.right > rect.left
        {
            return POINT {
                x: rect.left,
                y: rect.bottom,
            };
        }
        let mut point = POINT::default();
        let _ = GetCursorPos(&mut point);
        point
    }
}

// --- The caption button ------------------------------------------------

/// A chevron left of the window's minimize button: a non-activating owned
/// layered window, so the native caption stays untouched (v1).
pub fn create_button(parent: HWND) -> HWND {
    create_caption_button(parent, false)
}

pub fn create_hide_button(parent: HWND) -> HWND {
    create_caption_button(parent, true)
}

fn create_caption_button(parent: HWND, hide: bool) -> HWND {
    unsafe {
        let Ok(instance) = GetModuleHandleW(None) else {
            return HWND::default();
        };
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(button_procedure),
            hInstance: instance.into(),
            lpszClassName: w!("FramesNextCaptionButton"),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        });
        let Ok(button) = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED,
            w!("FramesNextCaptionButton"),
            w!("Connection menu"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            Some(parent),
            None,
            Some(instance.into()),
            None,
        ) else {
            return HWND::default();
        };
        if let Ok(tooltip) = CreateWindowExW(
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
        ) {
            let text = wide("Connection menu");
            let tool = TTTOOLINFOW {
                cbSize: std::mem::size_of::<TTTOOLINFOW>() as u32,
                uFlags: TTF_IDISHWND | TTF_SUBCLASS,
                hwnd: button,
                uId: button.0 as usize,
                lpszText: PWSTR(text.as_ptr() as *mut _),
                ..Default::default()
            };
            SendMessageW(
                tooltip,
                TTM_ADDTOOLW,
                None,
                Some(LPARAM((&tool as *const TTTOOLINFOW) as isize)),
            );
        }
        if hide {
            SetWindowLongPtrW(button, GWLP_USERDATA, HIDE_ROLE);
        }
        layout_button(parent, button);
        button
    }
}

/// Puts the button beside the minimize button (call when the window moves,
/// resizes, activates or changes DPI).
pub fn layout_button(parent: HWND, button: HWND) {
    if button.is_invalid() {
        return;
    }
    unsafe {
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
            minimize.left
                - width
                    * if GetWindowLongPtrW(button, GWLP_USERDATA) & HIDE_ROLE != 0 {
                        2
                    } else {
                        1
                    },
            minimize.top,
            width,
            minimize.bottom - minimize.top,
            SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
        draw_icon(button, parent);
    }
}

/// Alpha 1 keeps the whole button clickable while the native caption shows
/// beneath it; only the chevron and hover feedback are visible.
unsafe fn draw_icon(hwnd: HWND, owner: HWND) {
    unsafe {
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
        let buffer =
            std::slice::from_raw_parts_mut(pixels.cast::<u8>(), (width * height * 4) as usize);
        buffer.fill(0);
        let scale = GetDpiForWindow(owner).max(96) as i32;
        let unit = |value: i32| (value * scale + 48) / 96;
        let x = (width - unit(18)) / 2;
        let y = (height - unit(18)) / 2;
        let pen = CreatePen(PS_SOLID, unit(2), COLORREF(0xffffff));
        let old_pen = SelectObject(dc, pen.into());
        if GetWindowLongPtrW(hwnd, GWLP_USERDATA) & HIDE_ROLE != 0 {
            let _ = MoveToEx(dc, x + unit(2), y + unit(9), None);
            for (dx, dy) in [(6, 5), (12, 5), (16, 9), (12, 13), (6, 13), (2, 9)] {
                let _ = LineTo(dc, x + unit(dx), y + unit(dy));
            }
            let _ = MoveToEx(dc, x + unit(3), y + unit(2), None);
            let _ = LineTo(dc, x + unit(15), y + unit(16));
        } else {
            let _ = MoveToEx(dc, x + unit(4), y + unit(6), None);
            let _ = LineTo(dc, x + unit(9), y + unit(11));
            let _ = LineTo(dc, x + unit(14), y + unit(6));
        }
        SelectObject(dc, old_pen);
        let _ = DeleteObject(pen.into());
        let mut dark = BOOL(0);
        let _ = DwmGetWindowAttribute(
            owner,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            (&mut dark as *mut BOOL).cast(),
            std::mem::size_of::<BOOL>() as u32,
        );
        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
        let background_alpha: u16 = if state & 2 != 0 {
            42
        } else if state & 1 != 0 {
            24
        } else {
            1
        };
        let active = GetForegroundWindow() == owner;
        for pixel in buffer.chunks_exact_mut(4) {
            let coverage = u16::from(pixel[0]);
            let foreground_alpha: u16 = if active { 235 } else { 135 };
            let alpha =
                ((coverage * foreground_alpha + (255 - coverage) * background_alpha) / 255) as u8;
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
}

unsafe extern "system" fn button_procedure(
    hwnd: HWND,
    message: u32,
    w: WPARAM,
    l: LPARAM,
) -> LRESULT {
    unsafe {
        let owner = GetWindow(hwnd, GW_OWNER).unwrap_or_default();
        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
        match message {
            WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
            WM_ERASEBKGND => return LRESULT(1),
            WM_PAINT => {
                let mut paint = PAINTSTRUCT::default();
                BeginPaint(hwnd, &mut paint);
                let _ = EndPaint(hwnd, &paint);
                draw_icon(hwnd, owner);
                return LRESULT(0);
            }
            WM_MOUSEMOVE if state & 1 == 0 => {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, state | 1);
                let mut tracking = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    ..Default::default()
                };
                let _ = TrackMouseEvent(&mut tracking);
                draw_icon(hwnd, owner);
            }
            WM_MOUSELEAVE => {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, state & !1);
                draw_icon(hwnd, owner);
            }
            WM_LBUTTONDOWN => {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, state | 2);
                draw_icon(hwnd, owner);
                return LRESULT(0);
            }
            WM_LBUTTONUP => {
                let pressed = state & 2 != 0;
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, state & !2);
                draw_icon(hwnd, owner);
                let mut rect = RECT::default();
                let _ = GetClientRect(hwnd, &mut rect);
                let x = l.0 as u16 as i16 as i32;
                let y = (l.0 >> 16) as u16 as i16 as i32;
                if pressed && x >= 0 && y >= 0 && x < rect.right && y < rect.bottom {
                    let _ = PostMessageW(
                        Some(owner),
                        WM_COMMAND,
                        WPARAM(if state & HIDE_ROLE != 0 {
                            HIDE_BUTTON
                        } else {
                            MENU_BUTTON
                        }),
                        LPARAM(0),
                    );
                }
                return LRESULT(0);
            }
            _ => {}
        }
        DefWindowProcW(hwnd, message, w, l)
    }
}

// --- Fetch remote file -------------------------------------------------

const ID_OK: usize = 1;
const ID_CANCEL: usize = 2;

/// Asks for a path in the host user's home folder on its own thread (the
/// video keeps playing), then fetches it ("Fetch remote file…").
pub fn prompt_fetch(owner: HWND, window: u64) {
    let owner = owner.0 as usize;
    std::thread::spawn(move || {
        if let Some(path) = prompt_path(HWND(owner as *mut _)) {
            let path = path.trim().to_string();
            if !path.is_empty() {
                crate::control::send(Command::Window {
                    id: window,
                    action: WindowAction::Fetch { path },
                });
            }
        }
    });
}

thread_local! {
    static PROMPT: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

unsafe extern "system" fn prompt_procedure(
    hwnd: HWND,
    message: u32,
    w: WPARAM,
    l: LPARAM,
) -> LRESULT {
    match message {
        WM_COMMAND if matches!(w.0 & 0xffff, ID_OK | ID_CANCEL) => {
            PROMPT.with(|p| p.set(Some(w.0 & 0xffff == ID_OK)));
            LRESULT(0)
        }
        WM_CLOSE => {
            PROMPT.with(|p| p.set(Some(false)));
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, message, w, l) },
    }
}

fn prompt_path(owner: HWND) -> Option<String> {
    unsafe {
        let instance = GetModuleHandleW(None).ok()?;
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(prompt_procedure),
            hInstance: instance.into(),
            lpszClassName: w!("FramesNextFetchPrompt"),
            hbrBackground: HBRUSH((COLOR_WINDOW.0 + 1) as *mut _),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        });
        let dpi = GetDpiForWindow(owner).max(96) as i32;
        let px = |value: i32| value * dpi / 96;
        let mut at = RECT::default();
        let _ = GetWindowRect(owner, &mut at);
        let hwnd = CreateWindowExW(
            WS_EX_DLGMODALFRAME,
            w!("FramesNextFetchPrompt"),
            w!("Fetch file — path relative to the remote home folder"),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
            at.left + px(40),
            at.top + px(60),
            px(580),
            px(145),
            None,
            None,
            Some(instance.into()),
            None,
        )
        .ok()?;
        let font = GetStockObject(DEFAULT_GUI_FONT);
        let child =
            |class: PCWSTR, text: PCWSTR, style: WINDOW_STYLE, ex, x, y, cx, cy, id: usize| {
                let control = CreateWindowExW(
                    ex,
                    class,
                    text,
                    WS_CHILD | WS_VISIBLE | WS_TABSTOP | style,
                    px(x),
                    px(y),
                    px(cx),
                    px(cy),
                    Some(hwnd),
                    Some(HMENU(id as *mut _)),
                    Some(instance.into()),
                    None,
                )
                .unwrap_or_default();
                SendMessageW(
                    control,
                    WM_SETFONT,
                    Some(WPARAM(font.0 as usize)),
                    Some(LPARAM(1)),
                );
                control
            };
        let edit = child(
            w!("EDIT"),
            w!("Downloads/"),
            WINDOW_STYLE(ES_AUTOHSCROLL as u32),
            WS_EX_CLIENTEDGE,
            12,
            15,
            540,
            25,
            100,
        );
        child(
            w!("BUTTON"),
            w!("Download"),
            WINDOW_STYLE(BS_DEFPUSHBUTTON as u32),
            WINDOW_EX_STYLE(0),
            435,
            55,
            115,
            28,
            ID_OK,
        );
        child(
            w!("BUTTON"),
            w!("Cancel"),
            WINDOW_STYLE(BS_PUSHBUTTON as u32),
            WINDOW_EX_STYLE(0),
            310,
            55,
            115,
            28,
            ID_CANCEL,
        );
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(edit));
        SendMessageW(edit, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
        PROMPT.with(|p| p.set(None));
        let mut message = MSG::default();
        while PROMPT.with(|p| p.get()).is_none() && GetMessageW(&mut message, None, 0, 0).as_bool()
        {
            // Enter presses Download, Escape cancels, Tab moves.
            if !IsDialogMessageW(hwnd, &message).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        let accepted = PROMPT.with(|p| p.get()) == Some(true);
        let mut text = vec![0u16; 2048];
        let length = GetWindowTextW(edit, &mut text) as usize;
        let _ = DestroyWindow(hwnd);
        accepted.then(|| String::from_utf16_lossy(&text[..length]))
    }
}

// --- The connection window ----------------------------------------------

static CONNECTION_WINDOW: AtomicBool = AtomicBool::new(false);
const ID_SHOW_ALL: usize = 1;
const ID_MENU: usize = 2;

unsafe extern "system" fn connection_procedure(
    hwnd: HWND,
    message: u32,
    w: WPARAM,
    l: LPARAM,
) -> LRESULT {
    match message {
        WM_COMMAND if w.0 & 0xffff == ID_SHOW_ALL => {
            crate::control::send(Command::ShowAll { shown: true });
            LRESULT(0)
        }
        WM_COMMAND if w.0 & 0xffff == ID_MENU => {
            let button = unsafe { GetDlgItem(Some(hwnd), ID_MENU as i32) }.unwrap_or_default();
            let at = menu_point(button);
            if let Some(choice) = show_menu(hwnd, at, &Context::default()) {
                run(choice, hwnd);
            }
            LRESULT(0)
        }
        // Closing it ends the connection: nothing else would show it.
        WM_CLOSE => {
            crate::control::send(Command::Disconnect);
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, message, w, l) },
    }
}

/// Starts (once per process) the "Frames connection" window, shown while
/// every remote window is hidden, so the session stays reachable (v1).
pub fn start_connection_window() {
    if CONNECTION_WINDOW.swap(true, Ordering::Relaxed) {
        return;
    }
    std::thread::spawn(|| unsafe {
        crate::win::dpi_aware();
        let Ok(instance) = GetModuleHandleW(None) else {
            return;
        };
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(connection_procedure),
            hInstance: instance.into(),
            lpszClassName: w!("FramesNextConnection"),
            hbrBackground: HBRUSH((COLOR_WINDOW.0 + 1) as *mut _),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        });
        let Ok(hwnd) = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("FramesNextConnection"),
            w!("Frames connection"),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            100,
            100,
            None,
            None,
            Some(instance.into()),
            None,
        ) else {
            return;
        };
        let dpi = GetDpiForWindow(hwnd).max(96) as i32;
        let px = |value: i32| value * dpi / 96;
        let mut frame = RECT {
            left: 0,
            top: 0,
            right: px(440),
            bottom: px(112),
        };
        let _ = AdjustWindowRectExForDpi(
            &mut frame,
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX,
            false,
            WINDOW_EX_STYLE(0),
            dpi as u32,
        );
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            frame.right - frame.left,
            frame.bottom - frame.top,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
        let font = GetStockObject(DEFAULT_GUI_FONT);
        for (class, text, style, x, y, cx, cy, id) in [
            (
                w!("STATIC"),
                w!("Remote windows are hidden. Restore them or open the connection menu."),
                WINDOW_STYLE(0),
                16,
                16,
                408,
                36,
                0usize,
            ),
            (
                w!("BUTTON"),
                w!("Show all windows"),
                WINDOW_STYLE(BS_DEFPUSHBUTTON as u32) | WS_TABSTOP,
                128,
                64,
                140,
                30,
                ID_SHOW_ALL,
            ),
            (
                w!("BUTTON"),
                w!("Connection menu"),
                WINDOW_STYLE(BS_PUSHBUTTON as u32) | WS_TABSTOP,
                280,
                64,
                140,
                30,
                ID_MENU,
            ),
        ] {
            if let Ok(control) = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                text,
                WS_CHILD | WS_VISIBLE | style,
                px(x),
                px(y),
                px(cx),
                px(cy),
                Some(hwnd),
                Some(HMENU(id as *mut _)),
                Some(instance.into()),
                None,
            ) {
                SendMessageW(
                    control,
                    WM_SETFONT,
                    Some(WPARAM(font.0 as usize)),
                    Some(LPARAM(1)),
                );
            }
        }
        let mut shown = false;
        let mut version = 0;
        loop {
            if crate::DISCONNECT.load(Ordering::Relaxed) {
                break;
            }
            if crate::control::version() != version {
                version = crate::control::version();
                let snapshot = crate::control::snapshot();
                let hidden = snapshot.connected
                    && snapshot.view == View::Windows
                    && snapshot.windows.iter().any(|w| !w.popup)
                    && snapshot
                        .windows
                        .iter()
                        .filter(|w| !w.popup)
                        .all(|w| !w.shown);
                if hidden != shown {
                    shown = hidden;
                    let _ = ShowWindow(hwnd, if shown { SW_SHOWNORMAL } else { SW_HIDE });
                    if shown {
                        let _ = SetForegroundWindow(hwnd);
                    }
                }
            }
            let mut message = MSG::default();
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                if !IsDialogMessageW(hwnd, &message).as_bool() {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let _ = DestroyWindow(hwnd);
        CONNECTION_WINDOW.store(false, Ordering::Relaxed);
    });
}
