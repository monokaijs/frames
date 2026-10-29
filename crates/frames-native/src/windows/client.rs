#![allow(unsafe_op_in_unsafe_fn)]
use super::{host::vk_usage, wide};
use crate::{ClientEvent, ClientUpdate};
use base64::Engine;
use frames_core::{
    input::{InputEvent, remap_modifier},
    media::RawFrame,
    model::*,
};
use std::collections::HashMap;
use windows::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::{
            Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, StructuredStorage::PROPVARIANT},
            LibraryLoader::GetModuleHandleW,
        },
        UI::{
            HiDpi::*,
            Input::KeyboardAndMouse::*,
            Shell::PropertiesSystem::{IPropertyStore, SHGetPropertyStoreForWindow},
            WindowsAndMessaging::*,
        },
    },
    core::*,
};

const MODE_RESIZE: usize = 0xA001;
const MODE_STRETCH: usize = 0xA002;
const FILL: usize = 0xA003;
const SWAP: usize = 0xA004;
const REMOTE_MENU: usize = 0xA005;
const FETCH: usize = 0xA006;
const PHYSICAL: usize = 0xA007;
const VIEW_WINDOWS: usize = 0xB001;
const VIEW_DESKTOP: usize = 0xB002;
const DISCONNECT: usize = 0xB003;
const ADB_BRIDGE: usize = 0xB004;
const USB_INFO: usize = 0xB005;
#[derive(serde::Serialize, serde::Deserialize, Clone, Default)]
struct Preference {
    mode: SizingMode,
    fill: bool,
    swap: bool,
    #[serde(default)]
    physical: bool,
}
struct Proxy {
    remote: RemoteWindow,
    frame: Option<RawFrame>,
    events: tokio::sync::mpsc::UnboundedSender<ClientEvent>,
    pref: Preference,
    host: String,
    icon: Option<HICON>,
    status: String,
    pending_resize: Option<(f64, f64)>,
    last_size: Option<(i32, i32)>,
    visible: Option<bool>,
    forwarded: std::collections::BTreeSet<u16>,
    surrogate: Option<u16>,
    renderer: Option<super::render::Renderer>,
    ready: bool,
    view_mode: ViewMode,
    adb_enabled: bool,
    menu_items: Vec<MenuItem>,
    menu_ids: HashMap<usize, String>,
    menu_open: bool,
    pending_menu: Option<Vec<MenuItem>>,
}
impl Proxy {
    fn save(&self) {
        if let Ok(dir) = frames_core::trust::config_dir() {
            let path = dir.join("windows.json");
            let mut prefs: HashMap<String, Preference> = std::fs::read(&path)
                .ok()
                .and_then(|v| serde_json::from_slice(&v).ok())
                .unwrap_or_default();
            prefs.insert(
                format!("{}:{}", self.host, self.remote.app_id),
                self.pref.clone(),
            );
            if let Ok(bytes) = serde_json::to_vec_pretty(&prefs) {
                let _ = std::fs::write(path, bytes);
            }
        }
    }
    fn event(&self, event: ClientEvent) {
        let _ = self.events.send(event);
    }
    fn input(&self, event: InputEvent) {
        self.event(ClientEvent::Input {
            window: self.remote.key,
            event,
        });
    }
    fn title(&self) -> String {
        format!(
            "{} — {}{}{}",
            self.remote.title,
            if effective_sizing(self.pref.mode, self.remote.resize) == SizingMode::ResizeRemote {
                "Resize remote"
            } else {
                "Stretch locally"
            },
            if self.pref.physical {
                " / Physical keys"
            } else {
                ""
            },
            if self.status.is_empty() {
                String::new()
            } else {
                format!(" — {}", self.status)
            }
        )
    }
    unsafe fn content(&self, hwnd: HWND) -> RECT {
        let mut rect = RECT::default();
        let _ = GetClientRect(hwnd, &mut rect);
        if !self.pref.fill
            && let Some(frame) = &self.frame
        {
            let aspect = frame.width as f64 / frame.height as f64;
            let w = rect.right as f64;
            let h = rect.bottom as f64;
            if h > 0.0 && w / h > aspect {
                let width = (h * aspect) as i32;
                rect.left = (rect.right - width) / 2;
                rect.right = rect.left + width;
            } else if aspect > 0.0 {
                let height = (w / aspect) as i32;
                rect.top = (rect.bottom - height) / 2;
                rect.bottom = rect.top + height;
            }
        }
        rect
    }
    unsafe fn coords(&self, hwnd: HWND, param: LPARAM, screen: bool) -> (f64, f64) {
        let mut p = POINT {
            x: (param.0 as u16 as i16) as i32,
            y: ((param.0 >> 16) as u16 as i16) as i32,
        };
        if screen {
            let _ = ScreenToClient(hwnd, &mut p);
        }
        let rect = self.content(hwnd);
        (
            ((p.x - rect.left) as f64 / (rect.right - rect.left).max(1) as f64).clamp(0.0, 1.0),
            ((p.y - rect.top) as f64 / (rect.bottom - rect.top).max(1) as f64).clamp(0.0, 1.0),
        )
    }
}

unsafe extern "system" fn procedure(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = &*(lparam.0 as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
    }
    let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Proxy;
    if pointer.is_null() {
        return DefWindowProcW(hwnd, message, wparam, lparam);
    }
    let state = &mut *pointer;
    match message {
        WM_ENTERMENULOOP => {
            state.menu_open = true;
        }
        WM_EXITMENULOOP => {
            state.menu_open = false;
            if let Some(items) = state.pending_menu.take() {
                install_remote_menu(hwnd, state, items);
            }
            return LRESULT(0);
        }
        WM_COMMAND if wparam.0 >> 16 == 0 => {
            let command = wparam.0 & 0xffff;
            match command {
                VIEW_WINDOWS | VIEW_DESKTOP => {
                    state.event(ClientEvent::ReleaseInput);
                    state.event(ClientEvent::ViewMode(if command == VIEW_DESKTOP {
                        ViewMode::Desktop
                    } else {
                        ViewMode::Windows
                    }));
                }
                DISCONNECT => state.event(ClientEvent::Disconnected),
                ADB_BRIDGE => state.event(ClientEvent::AdbBridge {
                    enabled: !state.adb_enabled,
                }),
                USB_INFO => {
                    let _ = crate::show_error(
                        "Frames device forwarding",
                        "Android ADB: start adb on this computer, then enable Android ADB bridge. On the host use adb -H 127.0.0.1 -P 15037. Raw USB, fastboot, DFU, and iOS passthrough require separate driver support and are not enabled.",
                    );
                }
                MODE_RESIZE | MODE_STRETCH | FILL | SWAP | PHYSICAL | REMOTE_MENU | FETCH => {
                    let _ = PostMessageW(Some(hwnd), WM_SYSCOMMAND, WPARAM(command), LPARAM(0));
                }
                _ => {
                    if let Some(id) = state.menu_ids.get(&command).cloned() {
                        state.event(ClientEvent::Action {
                            window: state.remote.key,
                            action: WindowAction::MenuItem { id },
                        });
                    }
                }
            }
            return LRESULT(0);
        }
        WM_DROPFILES => {
            use windows::Win32::UI::Shell::*;
            let drop = HDROP(wparam.0 as *mut _);
            let count = DragQueryFileW(drop, u32::MAX, None);
            let mut paths = vec![];
            for index in 0..count.min(100) {
                let len = DragQueryFileW(drop, index, None);
                let mut path = vec![0u16; len as usize + 1];
                DragQueryFileW(drop, index, Some(&mut path));
                paths.push(std::path::PathBuf::from(String::from_utf16_lossy(
                    &path[..len as usize],
                )));
            }
            let mut point = POINT::default();
            let _ = DragQueryPoint(drop, &mut point);
            DragFinish(drop);
            let rect = state.content(hwnd);
            let x = ((point.x - rect.left) as f64 / (rect.right - rect.left).max(1) as f64)
                .clamp(0., 1.);
            let y = ((point.y - rect.top) as f64 / (rect.bottom - rect.top).max(1) as f64)
                .clamp(0., 1.);
            state.event(ClientEvent::SendFiles {
                window: state.remote.key,
                paths,
                x,
                y,
            });
            return LRESULT(0);
        }
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            let dc = BeginPaint(hwnd, &mut paint);
            let mut rect = RECT::default();
            let _ = GetClientRect(hwnd, &mut rect);
            let content = state.content(hwnd);
            if let (Some(renderer), Some(frame)) = (&mut state.renderer, &state.frame) {
                match renderer.draw(hwnd, frame, content) {
                    Ok(()) => {
                        let _ = EndPaint(hwnd, &paint);
                        return LRESULT(0);
                    }
                    Err(error) => {
                        tracing::warn!(%error,"D3D11 presentation unavailable; using GDI");
                        state.renderer = None;
                    }
                }
            }
            // Avoid queued GDI fills overpainting the D3D presentation at EndPaint.
            FillRect(dc, &rect, HBRUSH(GetStockObject(BLACK_BRUSH).0));
            if let Some(frame) = &state.frame {
                let rect = state.content(hwnd);
                let bmi = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: frame.width as i32,
                        biHeight: -(frame.height as i32),
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let _ = SetStretchBltMode(dc, HALFTONE);
                StretchDIBits(
                    dc,
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    0,
                    0,
                    frame.width as i32,
                    frame.height as i32,
                    Some(frame.bgra.as_ptr().cast()),
                    &bmi,
                    DIB_RGB_COLORS,
                    SRCCOPY,
                );
            }
            let _ = EndPaint(hwnd, &paint);
            return LRESULT(0);
        }
        WM_ERASEBKGND => return LRESULT(1),
        WM_CLOSE => {
            if state.remote.role == WindowRole::Desktop {
                state.event(ClientEvent::Disconnected);
                return LRESULT(0);
            }
            state.event(ClientEvent::Action {
                window: state.remote.key,
                action: WindowAction::Close,
            });
            state.status = "Closing remote window…".into();
            let _ = SetWindowTextW(hwnd, PCWSTR(wide(&state.title()).as_ptr()));
            return LRESULT(0);
        }
        WM_SETFOCUS => {
            state.event(ClientEvent::Action {
                window: state.remote.key,
                action: WindowAction::Focus,
            });
            state.event(ClientEvent::Visibility {
                window: state.remote.key,
                visible: true,
                focused: true,
            });
            if state.remote.role != WindowRole::Desktop {
                state.event(ClientEvent::Action {
                    window: state.remote.key,
                    action: WindowAction::Menu,
                });
            }
        }
        WM_KILLFOCUS => {
            state.event(ClientEvent::ReleaseInput);
            state.forwarded.clear();
            state.surrogate = None;
            state.event(ClientEvent::Visibility {
                window: state.remote.key,
                visible: !IsIconic(hwnd).as_bool(),
                focused: false,
            });
        }
        WM_SIZE => {
            let visible = wparam.0 != SIZE_MINIMIZED as usize;
            state.event(ClientEvent::Visibility {
                window: state.remote.key,
                visible,
                focused: GetForegroundWindow() == hwnd,
            });
            if state.ready
                && visible
                && effective_sizing(state.pref.mode, state.remote.resize)
                    == SizingMode::ResizeRemote
            {
                let width = (lparam.0 as u16) as i32;
                let height = ((lparam.0 >> 16) as u16) as i32;
                if width > 32 && height > 32 && state.last_size != Some((width, height)) {
                    state.last_size = Some((width, height));
                    let scale = GetDpiForWindow(hwnd).max(96) as f64 / 96.;
                    state.pending_resize = Some((width as f64 / scale, height as f64 / scale));
                    let _ = SetTimer(Some(hwnd), 2, 50, None);
                }
            }
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
        WM_TIMER => {
            if wparam.0 == 2 {
                let _ = KillTimer(Some(hwnd), 2);
                if let Some((width, height)) = state.pending_resize.take() {
                    state.event(ClientEvent::Action {
                        window: state.remote.key,
                        action: WindowAction::Resize { width, height },
                    });
                }
            }
        }
        WM_MOUSEMOVE => {
            let (x, y) = state.coords(hwnd, lparam, false);
            state.input(InputEvent::Pointer { x, y });
            return LRESULT(0);
        }
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONUP
        | WM_MBUTTONUP => {
            let down = matches!(message, WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN);
            if down {
                let _ = SetFocus(Some(hwnd));
                let _ = SetCapture(hwnd);
            } else {
                let _ = ReleaseCapture();
            }
            let button = match message {
                WM_LBUTTONDOWN | WM_LBUTTONUP => 0,
                WM_RBUTTONDOWN | WM_RBUTTONUP => 1,
                _ => 2,
            };
            let (x, y) = state.coords(hwnd, lparam, false);
            state.input(InputEvent::Button { button, down, x, y });
            return LRESULT(0);
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            let (x, y) = state.coords(hwnd, lparam, true);
            let delta = ((wparam.0 >> 16) as u16 as i16) as f64 / 120.0;
            state.input(InputEvent::Scroll {
                x,
                y,
                dx: if message == WM_MOUSEHWHEEL {
                    delta
                } else {
                    0.0
                },
                dy: if message == WM_MOUSEWHEEL {
                    -delta
                } else {
                    0.0
                },
            });
            return LRESULT(0);
        }
        WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => {
            let down = message == WM_KEYDOWN || message == WM_SYSKEYDOWN;
            let vk = wparam.0 as u16;
            let ctrl = GetKeyState(VK_CONTROL.0 as i32) < 0;
            let alt = GetKeyState(VK_MENU.0 as i32) < 0;
            let altgr = GetKeyState(VK_RMENU.0 as i32) < 0;
            if down && vk == VK_F12.0 && ctrl && alt {
                state.event(ClientEvent::ReleaseInput);
                state.forwarded.clear();
                state.surrogate = None;
                let _ = ShowWindow(hwnd, SW_MINIMIZE);
                return LRESULT(0);
            }
            if alt && vk == VK_TAB.0 {
                state.event(ClientEvent::ReleaseInput);
                state.forwarded.clear();
                state.surrogate = None;
                return DefWindowProcW(hwnd, message, wparam, lparam);
            }
            let vk = match vk {
                0x10 => {
                    if (lparam.0 >> 16) & 0xff == 0x36 {
                        0xa1
                    } else {
                        0xa0
                    }
                }
                0x11 => {
                    if (lparam.0 >> 24) & 1 != 0 {
                        0xa3
                    } else {
                        0xa2
                    }
                }
                0x12 => {
                    if (lparam.0 >> 24) & 1 != 0 {
                        0xa5
                    } else {
                        0xa4
                    }
                }
                _ => vk,
            };
            let scan = ((lparam.0 >> 16) & 0xff) as u16
                | if (lparam.0 >> 24) & 1 != 0 { 0xe000 } else { 0 };
            let usage = if state.pref.physical && vk != VK_PAUSE.0 {
                frames_core::input::pc_scan_hid(scan).or_else(|| vk_usage(vk))
            } else {
                vk_usage(vk)
            };
            if let Some(usage) = usage {
                if !state.pref.physical && usage == 0xe6 {
                    // AltGr composes text locally; undo Windows' synthetic left Control.
                    if down && state.forwarded.remove(&0xe0) {
                        state.input(InputEvent::Key {
                            usage: remap_modifier(0xe0, state.pref.swap),
                            down: false,
                        });
                    }
                    return LRESULT(0);
                }
                let printable = (4..=39).contains(&usage) || (44..=56).contains(&usage);
                let forward = if down {
                    let forward = state.pref.physical
                        || !(printable || altgr && usage == 0xe0)
                        || ((ctrl || alt) && !altgr);
                    if forward {
                        state.forwarded.insert(usage);
                    }
                    forward
                } else {
                    state.forwarded.remove(&usage)
                };
                if forward {
                    state.input(InputEvent::Key {
                        usage: remap_modifier(usage, state.pref.swap && !state.pref.physical),
                        down,
                    });
                }
            }
            return LRESULT(0);
        }
        WM_CHAR => {
            if !state.pref.physical
                && ((GetKeyState(VK_CONTROL.0 as i32) >= 0 && GetKeyState(VK_MENU.0 as i32) >= 0)
                    || GetKeyState(VK_RMENU.0 as i32) < 0)
            {
                let unit = wparam.0 as u16;
                if (0xd800..=0xdbff).contains(&unit) {
                    state.surrogate = Some(unit);
                } else {
                    let units = if let Some(high) = state.surrogate.take() {
                        vec![high, unit]
                    } else {
                        vec![unit]
                    };
                    let text = String::from_utf16_lossy(&units);
                    if !text.chars().any(char::is_control) {
                        state.input(InputEvent::Text { text });
                    }
                }
            }
            return LRESULT(0);
        }
        _ => {}
    }
    // Custom menu IDs are intentionally not multiples of 16.
    if message == WM_SYSCOMMAND && wparam.0 == REMOTE_MENU {
        state.event(ClientEvent::Action {
            window: state.remote.key,
            action: WindowAction::Menu,
        });
        return LRESULT(0);
    }
    if message == WM_SYSCOMMAND && wparam.0 == FETCH {
        if let Some(path) = super::files::prompt_path(hwnd) {
            state.event(ClientEvent::DownloadFile { path });
        }
        return LRESULT(0);
    }
    if message == WM_SYSCOMMAND
        && matches!(
            wparam.0,
            MODE_RESIZE | MODE_STRETCH | FILL | SWAP | PHYSICAL
        )
    {
        if matches!(wparam.0, SWAP | PHYSICAL) {
            state.event(ClientEvent::ReleaseInput);
            state.forwarded.clear();
            state.surrogate = None;
        }
        match wparam.0 {
            MODE_RESIZE => {
                if state.remote.resize == ResizeCapability::Supported {
                    state.pref.mode = SizingMode::ResizeRemote;
                }
            }
            MODE_STRETCH => state.pref.mode = SizingMode::StretchLocal,
            FILL => state.pref.fill = !state.pref.fill,
            SWAP => state.pref.swap = !state.pref.swap,
            PHYSICAL => state.pref.physical = !state.pref.physical,
            _ => {}
        }
        state.save();
        let items = state.menu_items.clone();
        install_remote_menu(hwnd, state, items);
        let _ = SetWindowTextW(hwnd, PCWSTR(wide(&state.title()).as_ptr()));
        let _ = InvalidateRect(Some(hwnd), None, false);
        return LRESULT(0);
    }
    if message == WM_NCDESTROY {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        let owned = Box::from_raw(pointer);
        if let Some(icon) = owned.icon {
            let _ = DestroyIcon(icon);
        }
    }
    DefWindowProcW(hwnd, message, wparam, lparam)
}

pub fn run_client(
    mut host: String,
    updates: std::sync::mpsc::Receiver<ClientUpdate>,
    events: tokio::sync::mpsc::UnboundedSender<ClientEvent>,
) -> anyhow::Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let instance = GetModuleHandleW(None)?;
        let class = wide("FramesProxy");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(procedure),
            hInstance: instance.into(),
            lpszClassName: PCWSTR(class.as_ptr()),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        RegisterClassW(&wc);
        let preferences: HashMap<String, Preference> = frames_core::trust::config_dir()
            .ok()
            .and_then(|dir| std::fs::read(dir.join("windows.json")).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let mut windows = HashMap::<WindowId, HWND>::new();
        let mut running = true;
        let mut view_mode = ViewMode::Windows;
        let mut adb_enabled = false;
        let mut session_control = super::session_control::SessionControl::new(events.clone())?;
        let mut shelf = super::files::Shelf::new(events.clone())?;
        let timer = SetTimer(None, 0, 16, None);
        let mut last_visibility = std::time::Instant::now();
        while running {
            for update in updates.try_iter() {
                match update {
                    ClientUpdate::ViewMode { mode, error } => {
                        view_mode = mode;
                        session_control.update(view_mode, adb_enabled);
                        for &hwnd in windows.values() {
                            let state =
                                &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Proxy);
                            state.view_mode = mode;
                            let items = state.menu_items.clone();
                            install_remote_menu(hwnd, state, items);
                        }
                        if let Some(error) = error {
                            let _ = crate::show_error("Frames view mode", &error);
                        }
                    }
                    ClientUpdate::AdbBridge {
                        enabled,
                        address,
                        error,
                    } => {
                        adb_enabled = enabled;
                        session_control.update(view_mode, adb_enabled);
                        for &hwnd in windows.values() {
                            let state =
                                &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Proxy);
                            state.adb_enabled = enabled;
                            let items = state.menu_items.clone();
                            install_remote_menu(hwnd, state, items);
                        }
                        if let Some(error) = error {
                            let _ = crate::show_error("Android ADB bridge", &error);
                        } else if let Some(address) = address {
                            tracing::info!(%address,"ADB bridge enabled on remote host");
                        }
                    }
                    ClientUpdate::TransferProgress { name, done, total } => {
                        shelf.progress(name, done, total)
                    }
                    ClientUpdate::Peer {
                        platform,
                        device_id,
                        ..
                    } => host = format!("{platform}:{device_id}"),
                    ClientUpdate::Menu { window, items } => {
                        if let Some(&hwnd) = windows.get(&window) {
                            let state =
                                &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Proxy);
                            install_remote_menu(hwnd, state, items);
                        }
                    }
                    ClientUpdate::ReceivedFile { path } => shelf.add(path),
                    ClientUpdate::Windows(list) => {
                        let gone: Vec<_> = windows
                            .keys()
                            .filter(|key| !list.iter().any(|w| &w.key == *key))
                            .copied()
                            .collect();
                        for key in gone {
                            if let Some(hwnd) = windows.remove(&key) {
                                let _ = DestroyWindow(hwnd);
                            }
                        }
                        let mut sorted = list;
                        sorted.sort_by_key(|w| w.parent.is_some());
                        for remote in sorted {
                            if let Some(&hwnd) = windows.get(&remote.key) {
                                let state =
                                    &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Proxy);
                                state.remote = remote;
                                let _ = SetWindowTextW(hwnd, PCWSTR(wide(&state.title()).as_ptr()));
                                continue;
                            }
                            let key = remote.key;
                            let parent = remote.parent.and_then(|k| windows.get(&k).copied());
                            let transient = parent.is_some()
                                && matches!(
                                    remote.role,
                                    WindowRole::Menu | WindowRole::Tooltip | WindowRole::Popover
                                );
                            let pref = preferences
                                .get(&format!("{host}:{}", remote.app_id))
                                .cloned()
                                .unwrap_or_else(|| Preference {
                                    swap: host.starts_with("macos:"),
                                    ..Default::default()
                                });
                            let width = remote.bounds.width.min(1400.0) as i32;
                            let height = remote.bounds.height.min(900.0) as i32;
                            let icon = remote.icon_png.as_deref().and_then(|s| make_icon(s).ok());
                            let state = Box::new(Proxy {
                                remote,
                                frame: None,
                                events: events.clone(),
                                pref,
                                host: host.clone(),
                                icon,
                                status: "Connecting stream…".into(),
                                pending_resize: None,
                                last_size: None,
                                visible: None,
                                forwarded: Default::default(),
                                surrogate: None,
                                renderer: None,
                                ready: false,
                                view_mode,
                                adb_enabled,
                                menu_items: vec![],
                                menu_ids: HashMap::new(),
                                menu_open: false,
                                pending_menu: None,
                            });
                            let title = wide(&state.title());
                            let app_id = format!(
                                "Frames.{}.{}",
                                host.replace([':', '.', ' '], "_"),
                                stable_hash(&state.remote.app_id)
                            );
                            let state = Box::into_raw(state);
                            let hwnd = match CreateWindowExW(
                                if transient {
                                    WS_EX_TOOLWINDOW
                                } else {
                                    WS_EX_APPWINDOW
                                },
                                PCWSTR(class.as_ptr()),
                                PCWSTR(title.as_ptr()),
                                if transient {
                                    WS_POPUP | WS_BORDER
                                } else {
                                    WS_OVERLAPPEDWINDOW
                                },
                                CW_USEDEFAULT,
                                CW_USEDEFAULT,
                                width,
                                height,
                                parent,
                                None,
                                Some(instance.into()),
                                Some(state.cast()),
                            ) {
                                Ok(h) => h,
                                Err(e) => {
                                    drop(Box::from_raw(state));
                                    return Err(e.into());
                                }
                            };
                            assign_appid(hwnd, &app_id);
                            if !transient {
                                install_remote_menu(hwnd, &mut *state, vec![]);
                            }
                            let dpi = GetDpiForWindow(hwnd).max(96);
                            let mut monitor = MONITORINFO {
                                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                                ..Default::default()
                            };
                            let _ = GetMonitorInfoW(
                                MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST),
                                &mut monitor,
                            );
                            let scale = dpi as f64 / 96.;
                            let max_width =
                                (monitor.rcWork.right - monitor.rcWork.left).max(640) as f64 * 0.9;
                            let max_height =
                                (monitor.rcWork.bottom - monitor.rcWork.top).max(480) as f64 * 0.85;
                            let mut picture_width =
                                ((*state).remote.bounds.width.min(1400.) * scale).min(max_width);
                            let mut picture_height =
                                ((*state).remote.bounds.height.min(900.) * scale).min(max_height);
                            if (*state).remote.role == WindowRole::Desktop {
                                let fit = (picture_width / (*state).remote.bounds.width)
                                    .min(picture_height / (*state).remote.bounds.height);
                                picture_width = (*state).remote.bounds.width * fit;
                                picture_height = (*state).remote.bounds.height * fit;
                            }
                            let mut initial = RECT {
                                left: 0,
                                top: 0,
                                right: picture_width as i32,
                                bottom: picture_height as i32,
                            };
                            let _ = AdjustWindowRectExForDpi(
                                &mut initial,
                                if transient {
                                    WS_POPUP | WS_BORDER
                                } else {
                                    WS_OVERLAPPEDWINDOW
                                },
                                !transient,
                                if transient {
                                    WS_EX_TOOLWINDOW
                                } else {
                                    WS_EX_APPWINDOW
                                },
                                dpi,
                            );
                            let _ = SetWindowPos(
                                hwnd,
                                None,
                                0,
                                0,
                                initial.right - initial.left,
                                initial.bottom - initial.top,
                                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                            );
                            if let Some(parent) = parent {
                                let owner =
                                    &*(GetWindowLongPtrW(parent, GWLP_USERDATA) as *const Proxy);
                                let mut origin = POINT::default();
                                let _ = ClientToScreen(parent, &mut origin);
                                let scale = dpi as f64 / 96.;
                                let x = origin.x
                                    + (((*state).remote.bounds.x - owner.remote.bounds.x) * scale)
                                        as i32;
                                let y = origin.y
                                    + (((*state).remote.bounds.y - owner.remote.bounds.y) * scale)
                                        as i32;
                                let _ = SetWindowPos(
                                    hwnd,
                                    None,
                                    x,
                                    y,
                                    0,
                                    0,
                                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                                );
                            }
                            (*state).renderer = super::render::Renderer::new(hwnd).ok();
                            windows::Win32::UI::Shell::DragAcceptFiles(hwnd, true);
                            if let Some(icon) = (*state).icon {
                                SendMessageW(
                                    hwnd,
                                    WM_SETICON,
                                    Some(WPARAM(ICON_BIG as usize)),
                                    Some(LPARAM(icon.0 as isize)),
                                );
                                SendMessageW(
                                    hwnd,
                                    WM_SETICON,
                                    Some(WPARAM(ICON_SMALL as usize)),
                                    Some(LPARAM(icon.0 as isize)),
                                );
                            }
                            let menu = GetSystemMenu(hwnd, false);
                            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
                            let _ = AppendMenuW(
                                menu,
                                if (*state).remote.resize == ResizeCapability::Supported {
                                    MF_STRING
                                } else {
                                    MF_STRING | MF_GRAYED
                                },
                                MODE_RESIZE,
                                PCWSTR(wide("Resize remote app").as_ptr()),
                            );
                            let _ = AppendMenuW(
                                menu,
                                MF_STRING,
                                MODE_STRETCH,
                                PCWSTR(wide("Stretch locally").as_ptr()),
                            );
                            let _ = AppendMenuW(
                                menu,
                                MF_STRING,
                                FILL,
                                PCWSTR(wide("Toggle aspect fit / fill").as_ptr()),
                            );
                            let _ = AppendMenuW(
                                menu,
                                MF_STRING,
                                SWAP,
                                PCWSTR(wide("Toggle Control / Command swap").as_ptr()),
                            );
                            let _ = AppendMenuW(
                                menu,
                                MF_STRING,
                                PHYSICAL,
                                PCWSTR(wide("Toggle physical keys (terminal)").as_ptr()),
                            );
                            let _ = AppendMenuW(
                                menu,
                                MF_STRING,
                                REMOTE_MENU,
                                PCWSTR(wide("Remote application menu…").as_ptr()),
                            );
                            let _ = AppendMenuW(
                                menu,
                                MF_STRING,
                                FETCH,
                                PCWSTR(wide("Fetch remote file…").as_ptr()),
                            );
                            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                            (*state).ready = true;
                            windows.insert(key, hwnd);
                            (*state).event(ClientEvent::Visibility {
                                window: key,
                                visible: true,
                                focused: false,
                            });
                        }
                    }
                    ClientUpdate::Frame(key, frame) => {
                        if let Some(&hwnd) = windows.get(&key) {
                            let state =
                                &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Proxy);
                            state.frame = Some(frame);
                            if !state.status.is_empty() {
                                state.status.clear();
                                let _ = SetWindowTextW(hwnd, PCWSTR(wide(&state.title()).as_ptr()));
                            }
                            let _ = InvalidateRect(Some(hwnd), None, false);
                        }
                    }
                    ClientUpdate::Status(key, status) => {
                        if let Some(&hwnd) = windows.get(&key) {
                            let state =
                                &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Proxy);
                            state.status = status;
                            let _ = SetWindowTextW(hwnd, PCWSTR(wide(&state.title()).as_ptr()));
                        }
                    }
                    ClientUpdate::Ack(key, actual, error) => {
                        if let Some(&hwnd) = windows.get(&key) {
                            let state =
                                &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Proxy);
                            if let Some(actual) = actual {
                                state.remote = actual;
                            }
                            if let Some(error) = error {
                                state.status = error;
                            }
                        }
                    }
                    ClientUpdate::Shutdown => running = false,
                }
            }
            if last_visibility.elapsed() > std::time::Duration::from_millis(100) {
                last_visibility = std::time::Instant::now();
                for (&key, &hwnd) in &windows {
                    let visible = IsWindowVisible(hwnd).as_bool()
                        && !IsIconic(hwnd).as_bool()
                        && on_desktop(hwnd)
                        && content_visible(hwnd);
                    let state = &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Proxy);
                    if state.visible != Some(visible) {
                        state.visible = Some(visible);
                        state.event(ClientEvent::Visibility {
                            window: key,
                            visible,
                            focused: visible && GetForegroundWindow() == hwnd,
                        });
                    }
                }
            }
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    running = false;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            MsgWaitForMultipleObjects(None, false, 16, QS_ALLINPUT);
        }
        let _ = KillTimer(None, timer);
        for (_, hwnd) in windows {
            let _ = DestroyWindow(hwnd);
        }
        let _ = events.send(ClientEvent::Disconnected);
        Ok(())
    }
}

fn stable_hash(value: &str) -> u64 {
    value.bytes().fold(0xcbf29ce484222325, |h, b| {
        (h ^ b as u64).wrapping_mul(0x100000001b3)
    })
}
unsafe fn install_remote_menu(hwnd: HWND, state: &mut Proxy, items: Vec<MenuItem>) {
    if state.menu_open {
        state.pending_menu = Some(items);
        return;
    }
    let Ok(bar) = CreateMenu() else {
        return;
    };
    let mut ids = HashMap::new();
    let mut next = 0x100usize;
    unsafe fn remote_items(
        menu: HMENU,
        items: &[MenuItem],
        ids: &mut HashMap<usize, String>,
        next: &mut usize,
        depth: usize,
    ) {
        if depth >= 8 {
            return;
        }
        for item in items.iter().take(200) {
            if item.title.is_empty() {
                let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
                continue;
            }
            let flags = MF_STRING
                | if item.enabled {
                    MENU_ITEM_FLAGS(0)
                } else {
                    MF_GRAYED
                }
                | if item.checked {
                    MF_CHECKED
                } else {
                    MENU_ITEM_FLAGS(0)
                };
            if !item.children.is_empty() {
                if let Ok(child) = CreatePopupMenu() {
                    remote_items(child, &item.children, ids, next, depth + 1);
                    let _ = AppendMenuW(
                        menu,
                        flags | MF_POPUP,
                        child.0 as usize,
                        PCWSTR(wide(&item.title).as_ptr()),
                    );
                }
            } else if *next < 0x7000 {
                let id = *next;
                *next += 1;
                ids.insert(id, item.id.clone());
                let _ = AppendMenuW(menu, flags, id, PCWSTR(wide(&item.title).as_ptr()));
            }
        }
    }
    let local = CreatePopupMenu().unwrap();
    let checked = |yes| if yes { MF_CHECKED } else { MENU_ITEM_FLAGS(0) };
    let _ = AppendMenuW(
        local,
        MF_STRING | checked(state.view_mode == ViewMode::Windows),
        VIEW_WINDOWS,
        PCWSTR(wide("Per-window view").as_ptr()),
    );
    let _ = AppendMenuW(
        local,
        MF_STRING | checked(state.view_mode == ViewMode::Desktop),
        VIEW_DESKTOP,
        PCWSTR(wide("Full desktop view").as_ptr()),
    );
    let _ = AppendMenuW(local, MF_SEPARATOR, 0, PCWSTR::null());
    for (id, title) in [
        (MODE_RESIZE, "Resize remote app"),
        (MODE_STRETCH, "Stretch locally"),
        (FILL, "Toggle aspect fit / fill"),
        (SWAP, "Toggle Control / Command swap"),
        (PHYSICAL, "Toggle physical keys (terminal)"),
        (REMOTE_MENU, "Refresh application menus"),
        (FETCH, "Fetch remote file..."),
    ] {
        let disabled = (id == MODE_RESIZE && state.remote.resize != ResizeCapability::Supported)
            || (id == REMOTE_MENU && state.remote.role == WindowRole::Desktop);
        let _ = AppendMenuW(
            local,
            MF_STRING
                | if disabled {
                    MF_GRAYED
                } else {
                    MENU_ITEM_FLAGS(0)
                },
            id,
            PCWSTR(wide(title).as_ptr()),
        );
    }
    let _ = AppendMenuW(local, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(
        local,
        MF_STRING,
        DISCONNECT,
        PCWSTR(wide("Disconnect").as_ptr()),
    );
    let _ = AppendMenuW(
        bar,
        MF_POPUP,
        local.0 as usize,
        PCWSTR(wide("Frames").as_ptr()),
    );
    remote_items(bar, &items, &mut ids, &mut next, 0);
    let devices = CreatePopupMenu().unwrap();
    let _ = AppendMenuW(
        devices,
        MF_STRING | checked(state.adb_enabled),
        ADB_BRIDGE,
        PCWSTR(wide("Android ADB bridge").as_ptr()),
    );
    let _ = AppendMenuW(
        devices,
        MF_STRING,
        USB_INFO,
        PCWSTR(wide("Device forwarding support...").as_ptr()),
    );
    let _ = AppendMenuW(
        bar,
        MF_POPUP,
        devices.0 as usize,
        PCWSTR(wide("Devices").as_ptr()),
    );
    let old = GetMenu(hwnd);
    if SetMenu(hwnd, Some(bar)).is_ok() {
        state.menu_items = items;
        state.menu_ids = ids;
        if !old.is_invalid() {
            let _ = DestroyMenu(old);
        }
        let _ = DrawMenuBar(hwnd);
    } else {
        let _ = DestroyMenu(bar);
    }
}

unsafe fn assign_appid(hwnd: HWND, id: &str) {
    if let Ok(store) = SHGetPropertyStoreForWindow::<IPropertyStore>(hwnd) {
        let key = PROPERTYKEY {
            fmtid: GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3),
            pid: 5,
        };
        // PROPVARIANT owns the string allocation and clears it on drop.
        let value = PROPVARIANT::from(id);
        let _ = store.SetValue(&key, &value);
        let _ = store.Commit();
    }
}
unsafe fn on_desktop(hwnd: HWND) -> bool {
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
    use windows::Win32::UI::Shell::{IVirtualDesktopManager, VirtualDesktopManager};
    CoCreateInstance::<_, IVirtualDesktopManager>(
        &VirtualDesktopManager,
        None,
        CLSCTX_INPROC_SERVER,
    )
    .and_then(|m| m.IsWindowOnCurrentVirtualDesktop(hwnd))
    .map(|v| v.as_bool())
    .unwrap_or(true)
}
/// Subtract only confirmed opaque interiors. Rounded corners, transparent and
/// layered windows cannot cause a false "fully covered" decision.
unsafe fn content_visible(hwnd: HWND) -> bool {
    use windows::Win32::Graphics::Dwm::*;
    let mut rect = RECT::default();
    if GetClientRect(hwnd, &mut rect).is_err() {
        return true;
    }
    let mut origin = POINT::default();
    let _ = ClientToScreen(hwnd, &mut origin);
    let remaining = CreateRectRgn(
        origin.x,
        origin.y,
        origin.x + rect.right,
        origin.y + rect.bottom,
    );
    if remaining.is_invalid() {
        return true;
    }
    let mut window = GetWindow(hwnd, GW_HWNDPREV).unwrap_or_default();
    let mut visible = true;
    for _ in 0..512 {
        if window.is_invalid() {
            break;
        }
        let next = GetWindow(window, GW_HWNDPREV).unwrap_or_default();
        let ex = GetWindowLongPtrW(window, GWL_EXSTYLE) as u32;
        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(window, DWMWA_CLOAKED, &mut cloaked as *mut _ as *mut _, 4);
        if IsWindowVisible(window).as_bool()
            && !IsIconic(window).as_bool()
            && cloaked == 0
            && ex & (WS_EX_LAYERED.0 | WS_EX_TRANSPARENT.0 | WS_EX_NOREDIRECTIONBITMAP.0) == 0
        {
            let mut bounds = RECT::default();
            if DwmGetWindowAttribute(
                window,
                DWMWA_EXTENDED_FRAME_BOUNDS,
                &mut bounds as *mut _ as *mut _,
                std::mem::size_of::<RECT>() as u32,
            )
            .is_ok()
            {
                bounds.left += 12;
                bounds.top += 12;
                bounds.right -= 12;
                bounds.bottom -= 12;
                if bounds.right > bounds.left && bounds.bottom > bounds.top {
                    let region =
                        CreateRectRgn(bounds.left, bounds.top, bounds.right, bounds.bottom);
                    let result =
                        CombineRgn(Some(remaining), Some(remaining), Some(region), RGN_DIFF);
                    let _ = DeleteObject(region.into());
                    if result == NULLREGION {
                        visible = false;
                        break;
                    }
                }
            }
        }
        window = next;
    }
    let _ = DeleteObject(remaining.into());
    visible
}
unsafe fn make_icon(png: &str) -> anyhow::Result<HICON> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(png)?;
    anyhow::ensure!(bytes.len() <= 1024 * 1024, "icon too large");
    let image = super::decode_png(&bytes, 1024, 16 * 1024 * 1024)?
        .resize(64, 64, image::imageops::FilterType::Lanczos3)
        .to_rgba8();
    let (w, h) = image.dimensions();
    let mut bgra = image.into_raw();
    for p in bgra.chunks_mut(4) {
        p.swap(0, 2);
    }
    let dc = GetDC(None);
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w as i32,
            biHeight: -(h as i32),
            biPlanes: 1,
            biBitCount: 32,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits = std::ptr::null_mut();
    let bitmap = CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)?;
    ReleaseDC(None, dc);
    std::ptr::copy_nonoverlapping(bgra.as_ptr(), bits.cast(), bgra.len());
    let mask = CreateBitmap(w as i32, h as i32, 1, 1, None);
    let info = ICONINFO {
        fIcon: TRUE,
        hbmColor: bitmap,
        hbmMask: mask,
        ..Default::default()
    };
    let icon = CreateIconIndirect(&info);
    let _ = DeleteObject(bitmap.into());
    let _ = DeleteObject(mask.into());
    Ok(icon?)
}
