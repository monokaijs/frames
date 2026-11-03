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
            Input::{KeyboardAndMouse::*, *},
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
const SHOW_HEADER: usize = 0xA008;
const VIEW_WINDOWS: usize = 0xB001;
const VIEW_DESKTOP: usize = 0xB002;
const DISCONNECT: usize = 0xB003;
const ADB_BRIDGE: usize = 0xB004;
const USB_INFO: usize = 0xB005;
const REMOTE_CURSOR: usize = 0xB006;
const RELATIVE_MOUSE: usize = 0xB007;
const COMPATIBILITY: usize = 0xB008;
const DEBUG_OVERLAY: usize = 0xB009;
/// Posted to the UI thread after the debug overlay is toggled from any window.
const DEBUG_TOGGLED: u32 = WM_APP + 44;
/// The debug overlay is session-wide: every viewer shows it or none does.
static DEBUG_ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
fn debug_label() -> String {
    format!("Debug overlay · {}", crate::DEBUG_OVERLAY_SHORTCUT)
}
#[derive(serde::Serialize, serde::Deserialize, Clone, Default)]
struct Preference {
    mode: SizingMode,
    fill: bool,
    swap: bool,
    #[serde(default)]
    physical: bool,
}
struct Proxy {
    hidden: bool,
    header: HWND,
    compatibility: frames_core::shared_game::CompatibilityInfo,
    shared: bool,
    relative: bool,
    virtual_cursor: Option<super::virtual_cursor::VirtualCursor>,
    devices: super::devices::DeviceMenu,
    remote: RemoteWindow,
    frame: Option<RawFrame>,
    events: tokio::sync::mpsc::UnboundedSender<ClientEvent>,
    pref: Preference,
    host: String,
    icon: Option<HICON>,
    status: String,
    last_title: String,
    command_status: Option<String>,
    shared_notice: Option<String>,
    pending_resize: Option<(f64, f64)>,
    last_size: Option<(i32, i32)>,
    visible: Option<bool>,
    forwarded: std::collections::BTreeSet<u16>,
    surrogate: Option<u16>,
    renderer: Option<super::render::Renderer>,
    renderer_failed: bool,
    ready: bool,
    user_sizing: bool,
    maximized: bool,
    view_mode: ViewMode,
    show_remote_cursor: bool,
    adb_enabled: bool,
    menu_items: Vec<MenuItem>,
    menu_ids: HashMap<usize, String>,
    menu_open: bool,
    pending_menu: Option<Vec<MenuItem>>,
    debug_panel: Option<super::debug_overlay::DebugPanel>,
    debug_snapshot: Option<Box<crate::DebugOverlay>>,
}
impl Proxy {
    /// Shows, moves or hides this window's debug panel for the current state.
    unsafe fn refresh_debug(&mut self, hwnd: HWND) {
        let enabled = DEBUG_ENABLED.load(std::sync::atomic::Ordering::Relaxed);
        if !enabled {
            self.debug_snapshot = None;
        }
        let eligible = enabled
            && !self.hidden
            && !IsIconic(hwnd).as_bool()
            && matches!(
                self.remote.role,
                WindowRole::Desktop | WindowRole::Window | WindowRole::Dialog
            );
        let Some(snapshot) = self.debug_snapshot.as_ref().filter(|_| eligible) else {
            if let Some(panel) = &self.debug_panel {
                panel.hide();
            }
            return;
        };
        if self.debug_panel.is_none() {
            match super::debug_overlay::DebugPanel::new(hwnd) {
                Ok(panel) => self.debug_panel = Some(panel),
                Err(error) => {
                    tracing::warn!(%error, "debug overlay unavailable");
                    return;
                }
            }
        }
        let content = self.content(hwnd);
        if let Err(error) = self.debug_panel.as_ref().unwrap().show(snapshot, content) {
            tracing::debug!(%error, "debug overlay draw failed");
        }
    }
    fn tray_window(&self) -> super::session_control::SessionWindow {
        use super::tray::{TrayGlyph as G, TrayItem as Item};
        let item = |id: usize, title: &str, glyph| Item::new(id as u32, title, glyph);
        let mut options = vec![item(FILL, "Fill window", G::Display).checked(self.pref.fill)];
        if self.shared {
            options.push(
                item(RELATIVE_MOUSE, "Relative mouse · Ctrl+Alt+F11", G::Cursor)
                    .checked(self.relative),
            );
            options.push(item(COMPATIBILITY, "Window compatibility", G::Info));
            options.push(
                item(DEBUG_OVERLAY, &debug_label(), G::Info)
                    .checked(DEBUG_ENABLED.load(std::sync::atomic::Ordering::Relaxed)),
            );
        } else {
            options.extend([
                item(SHOW_HEADER, "Show remote window header", G::Window)
                    .checked(self.remote.show_header)
                    .enabled(self.remote.role != WindowRole::Desktop),
                item(MODE_RESIZE, "Resize remote app", G::Display)
                    .checked(self.pref.mode == SizingMode::ResizeRemote)
                    .enabled(self.remote.resize == ResizeCapability::Supported),
                item(MODE_STRETCH, "Stretch locally", G::Display)
                    .checked(self.pref.mode == SizingMode::StretchLocal),
                item(SWAP, "Swap Control / Command", G::Keyboard).checked(self.pref.swap),
                item(PHYSICAL, "Physical keys", G::Keyboard).checked(self.pref.physical),
                item(REMOTE_MENU, "Refresh application menus", G::Refresh)
                    .enabled(self.remote.role != WindowRole::Desktop),
                item(FETCH, "Fetch remote file…", G::Folder),
                item(DEBUG_OVERLAY, &debug_label(), G::Info)
                    .checked(DEBUG_ENABLED.load(std::sync::atomic::Ordering::Relaxed)),
            ]);
            fn remote_items(
                items: &[MenuItem],
                ids: &HashMap<usize, String>,
                depth: usize,
            ) -> Vec<Item> {
                if depth >= 8 {
                    return vec![];
                }
                items
                    .iter()
                    .take(200)
                    .map(|entry| {
                        if entry.title.is_empty() {
                            return Item::separator();
                        }
                        let id = ids
                            .iter()
                            .find(|(_, value)| **value == entry.id)
                            .map_or(0, |(id, _)| *id as u32);
                        Item::new(id, &entry.title, G::Window)
                            .checked(entry.checked)
                            .enabled(entry.enabled)
                            .children(remote_items(&entry.children, ids, depth + 1))
                    })
                    .collect()
            }
            if !self.menu_items.is_empty() {
                options.push(
                    Item::new(0, "Application menu", G::Window).children(remote_items(
                        &self.menu_items,
                        &self.menu_ids,
                        0,
                    )),
                );
            }
        }
        super::session_control::SessionWindow {
            key: self.remote.key,
            title: if self.remote.title.is_empty() {
                self.remote.app_name.clone()
            } else {
                format!("{} · {}", self.remote.title, self.remote.app_name)
            },
            shown: !self.hidden,
            options,
        }
    }
    unsafe fn set_shown(&mut self, hwnd: HWND, shown: bool) {
        if self.hidden != shown {
            return;
        }
        self.hidden = !shown;
        if !shown {
            self.release_mouse();
            self.event(ClientEvent::ReleaseInput);
            self.forwarded.clear();
            self.surrogate = None;
            self.pending_resize = None;
            let _ = KillTimer(Some(hwnd), 2);
            self.frame = None;
            self.renderer = None;
            self.visible = Some(false);
            self.event(ClientEvent::Visibility {
                window: self.remote.key,
                visible: false,
                focused: false,
            });
            if !self.header.is_invalid() {
                let _ = ShowWindow(self.header, SW_HIDE);
            }
            let _ = ShowWindow(hwnd, SW_HIDE);
        } else {
            self.status = "Resuming stream…".into();
            self.renderer = None;
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(hwnd);
            super::caption::layout(hwnd, self.header);
            self.visible = Some(true);
            self.event(ClientEvent::Visibility {
                window: self.remote.key,
                visible: true,
                focused: true,
            });
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }
    unsafe fn release_mouse(&mut self) {
        unsafe {
            let _ = ReleaseCapture();
        }
        if self.relative {
            unsafe {
                let _ = ClipCursor(None);
                let device = RAWINPUTDEVICE {
                    usUsagePage: 1,
                    usUsage: 2,
                    dwFlags: RIDEV_REMOVE,
                    hwndTarget: HWND::default(),
                };
                let _ = RegisterRawInputDevices(
                    &[device],
                    std::mem::size_of::<RAWINPUTDEVICE>() as u32,
                );
            }
            self.relative = false;
        }
    }
    unsafe fn toggle_relative(&mut self, hwnd: HWND) {
        if self.relative {
            unsafe {
                self.release_mouse();
            }
            self.event(ClientEvent::ReleaseInput);
            return;
        }
        if !self.shared {
            return;
        }
        unsafe {
            let device = RAWINPUTDEVICE {
                usUsagePage: 1,
                usUsage: 2,
                dwFlags: RAWINPUTDEVICE_FLAGS(0),
                hwndTarget: hwnd,
            };
            if RegisterRawInputDevices(&[device], std::mem::size_of::<RAWINPUTDEVICE>() as u32)
                .is_err()
            {
                return;
            }
            let mut rect = self.content(hwnd);
            let _ = MapWindowPoints(
                Some(hwnd),
                None,
                std::slice::from_raw_parts_mut((&mut rect as *mut RECT).cast::<POINT>(), 2),
            );
            let _ = ClipCursor(Some(&rect));
            let _ = SetCapture(hwnd);
            self.relative = true;
        }
        self.event(ClientEvent::ResumeInput {
            created: std::time::Instant::now(),
        });
    }
    unsafe fn cursor_geometry(&self, hwnd: HWND) -> (RECT, (f64, f64)) {
        let rect = unsafe { self.content(hwnd) };
        (
            rect,
            (
                (rect.right - rect.left) as f64
                    / (self.remote.bounds.width * self.remote.scale).max(1.),
                (rect.bottom - rect.top) as f64
                    / (self.remote.bounds.height * self.remote.scale).max(1.),
            ),
        )
    }
    fn save(&self) {
        if self.shared {
            return;
        }
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
    unsafe fn update_remote(&mut self, hwnd: HWND, remote: RemoteWindow) {
        let header_changed =
            self.remote.show_header != remote.show_header || self.remote.role != remote.role;
        self.remote = remote;
        if header_changed {
            let menu = GetSystemMenu(hwnd, false);
            CheckMenuItem(
                menu,
                SHOW_HEADER as u32,
                MF_BYCOMMAND.0 | (header_menu_flags(self) & MF_CHECKED).0,
            );
            let _ = EnableMenuItem(
                menu,
                SHOW_HEADER as u32,
                MF_BYCOMMAND | (header_menu_flags(self) & MF_GRAYED),
            );
            let items = self.menu_items.clone();
            install_remote_menu(hwnd, self, items);
        }
        self.refresh_title(hwnd);
    }
    fn input(&self, event: InputEvent) {
        self.event(ClientEvent::TimedInput {
            window: self.remote.key,
            event,
            created: std::time::Instant::now(),
        });
    }
    fn title(&self) -> String {
        let mut title = format!(
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
            if let Some(status) = &self.command_status {
                format!(" — {status}")
            } else if self.status.is_empty() {
                String::new()
            } else {
                format!(" — {}", self.status)
            }
        );
        if let Some(notice) = &self.shared_notice {
            title.push_str(" · ");
            title.push_str(notice);
        }
        title
    }
    unsafe fn refresh_title(&mut self, hwnd: HWND) {
        let title = self.title();
        if title != self.last_title && SetWindowTextW(hwnd, PCWSTR(wide(&title).as_ptr())).is_ok() {
            self.last_title = title;
            if !self.header.is_invalid() {
                let _ = InvalidateRect(Some(self.header), None, false);
            }
        }
    }
    unsafe fn content(&self, hwnd: HWND) -> RECT {
        let mut rect = RECT::default();
        let _ = GetClientRect(hwnd, &mut rect);
        if !self.pref.fill
            && let Some(frame) = &self.frame
        {
            let aspect = frame.width as f64 / frame.height as f64;
            let w = rect.right as f64;
            let h = (rect.bottom - rect.top) as f64;
            if h > 0.0 && w / h > aspect {
                let width = (h * aspect) as i32;
                rect.left = (rect.right - width) / 2;
                rect.right = rect.left + width;
            } else if aspect > 0.0 {
                let height = (w / aspect) as i32;
                rect.top += ((h as i32) - height) / 2;
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
        super::caption::LAYOUT => {
            super::caption::layout(hwnd, state.header);
            return LRESULT(0);
        }
        WM_WINDOWPOSCHANGED | WM_SHOWWINDOW | WM_NCACTIVATE => {
            let _ = PostMessageW(Some(hwnd), super::caption::LAYOUT, WPARAM(0), LPARAM(0));
        }
        WM_DPICHANGED => {
            let rect = &*(lparam.0 as *const RECT);
            let _ = SetWindowPos(
                hwnd,
                None,
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            super::caption::layout(hwnd, state.header);
        }
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
            if state.devices.action(command, &state.events) {
                return LRESULT(0);
            }
            match command {
                super::caption::MENU => {
                    state.release_mouse();
                    state.event(ClientEvent::ReleaseInput);
                    state.forwarded.clear();
                    state.surrogate = None;
                    let _ = PostThreadMessageW(
                        GetWindowThreadProcessId(hwnd, None),
                        super::session_control::OPEN,
                        WPARAM(hwnd.0 as usize),
                        LPARAM(0),
                    );
                }
                RELATIVE_MOUSE => {
                    state.event(ClientEvent::ReleaseInput);
                    state.forwarded.clear();
                    state.toggle_relative(hwnd);
                }
                COMPATIBILITY => {
                    super::dialogs::message(
                        "Window compatibility",
                        &state.compatibility.description(),
                        MB_OK | MB_ICONINFORMATION,
                    );
                }
                VIEW_WINDOWS | VIEW_DESKTOP => {
                    state.event(ClientEvent::ReleaseInput);
                    state.event(ClientEvent::ViewMode(if command == VIEW_DESKTOP {
                        ViewMode::Desktop
                    } else {
                        ViewMode::Windows
                    }));
                }
                DISCONNECT => state.event(ClientEvent::Disconnected),
                DEBUG_OVERLAY => toggle_debug_overlay(hwnd, state),
                REMOTE_CURSOR => state.event(ClientEvent::RemoteCursor {
                    visible: !state.show_remote_cursor,
                }),
                ADB_BRIDGE => state.event(ClientEvent::AdbBridge {
                    enabled: !state.adb_enabled,
                }),
                USB_INFO => {
                    let _ = crate::show_error(
                        "Frames device forwarding",
                        "Choose a locally connected USB/Bluetooth controller under Devices > Gamepads. Receiving requires ViGEmBus on the Windows host. For generic wired USB between Windows computers, set up the local exporter and host receiver under Devices > USB devices, refresh, then select a device. USB audio/video is excluded. The Android ADB bridge remains available.",
                    );
                }
                MODE_RESIZE | MODE_STRETCH | FILL | SWAP | PHYSICAL | REMOTE_MENU | FETCH
                | SHOW_HEADER => {
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
            if state.renderer.is_none() && state.frame.is_some() && !state.renderer_failed {
                match super::render::Renderer::new(hwnd) {
                    Ok(renderer) => state.renderer = Some(renderer),
                    Err(error) => {
                        state.renderer_failed = true;
                        tracing::warn!(%error,"presentation worker unavailable");
                    }
                }
            }
            if let (Some(renderer), Some(frame)) = (&mut state.renderer, &state.frame) {
                renderer.set_feedback(state.remote.key, state.events.clone());
                match renderer.draw(hwnd, frame, content) {
                    Ok(()) => {
                        let _ = EndPaint(hwnd, &paint);
                        return LRESULT(0);
                    }
                    Err(error) => {
                        tracing::warn!(%error,"D3D11 presentation unavailable; using GDI");
                        state.renderer_failed = true;
                        state.event(ClientEvent::DecoderFailed {
                            window: state.remote.key,
                            generation: frame.media_generation,
                            reason: error.to_string(),
                        });
                        state.renderer = None;
                    }
                }
            }
            if state
                .frame
                .as_ref()
                .is_some_and(|frame| frame.bgra.is_empty())
            {
                let _ = EndPaint(hwnd, &paint);
                return LRESULT(0);
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
            } else {
                let _ = SetBkMode(dc, TRANSPARENT);
                SetTextColor(dc, COLORREF(0x00b0b0b0));
                let mut label = wide(
                    if state.status == "Connecting stream…" || state.status.is_empty() {
                        "Waiting for video…"
                    } else {
                        &state.status
                    },
                );
                DrawTextW(
                    dc,
                    &mut label,
                    &mut rect,
                    DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
                );
            }
            let _ = EndPaint(hwnd, &paint);
            return LRESULT(0);
        }
        WM_ERASEBKGND => return LRESULT(1),
        WM_CLOSE => {
            if state.shared || state.remote.role == WindowRole::Desktop {
                state.release_mouse();
                state.event(ClientEvent::Disconnected);
                return LRESULT(0);
            }
            if state.command_status.as_deref() == Some("Closing remote window…") {
                return LRESULT(0);
            }
            state.event(ClientEvent::Action {
                window: state.remote.key,
                action: WindowAction::Close,
            });
            state.command_status = Some("Closing remote window…".into());
            state.refresh_title(hwnd);
            return LRESULT(0);
        }
        WM_SETFOCUS => {
            if state.hidden {
                return LRESULT(0);
            }
            if state.shared {
                state.event(ClientEvent::ResumeInput {
                    created: std::time::Instant::now(),
                });
            }
            if !state.shared {
                state.event(ClientEvent::Action {
                    window: state.remote.key,
                    action: WindowAction::Focus,
                });
            }
            state.event(ClientEvent::Visibility {
                window: state.remote.key,
                visible: true,
                focused: true,
            });
            if !state.shared && state.remote.role != WindowRole::Desktop {
                state.event(ClientEvent::Action {
                    window: state.remote.key,
                    action: WindowAction::Menu,
                });
            }
        }
        WM_KILLFOCUS => {
            state.release_mouse();
            state.event(ClientEvent::ReleaseInput);
            state.forwarded.clear();
            state.surrogate = None;
            state.event(ClientEvent::Visibility {
                window: state.remote.key,
                visible: !state.hidden
                    && IsWindowVisible(hwnd).as_bool()
                    && !IsIconic(hwnd).as_bool(),
                focused: false,
            });
        }
        WM_ENTERSIZEMOVE => state.user_sizing = true,
        WM_EXITSIZEMOVE => state.user_sizing = false,
        WM_SIZE => {
            super::caption::layout(hwnd, state.header);
            state.refresh_debug(hwnd);
            if state.relative {
                state.release_mouse();
                state.event(ClientEvent::ReleaseInput);
            }
            let visible = !state.hidden && wparam.0 != SIZE_MINIMIZED as usize;
            let maximized = wparam.0 == SIZE_MAXIMIZED as usize;
            let user_resize = state.user_sizing || maximized || state.maximized;
            state.maximized = maximized;
            state.event(ClientEvent::Visibility {
                window: state.remote.key,
                visible,
                focused: GetForegroundWindow() == hwnd,
            });
            if !state.shared
                && state.ready
                && user_resize
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
            let (rect, scale) = state.cursor_geometry(hwnd);
            if let Some(overlay) = state.virtual_cursor.as_mut() {
                let _ = overlay.reposition(rect, scale);
            }
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
        WM_INPUT if state.shared && state.relative => {
            let mut raw = RAWINPUT::default();
            let mut bytes = std::mem::size_of::<RAWINPUT>() as u32;
            if GetRawInputData(
                HRAWINPUT(lparam.0 as *mut _),
                RID_INPUT,
                Some((&mut raw as *mut RAWINPUT).cast()),
                &mut bytes,
                std::mem::size_of::<RAWINPUTHEADER>() as u32,
            ) != u32::MAX
                && raw.header.dwType == RIM_TYPEMOUSE.0
            {
                let mouse = raw.data.mouse;
                if mouse.usFlags.0 & MOUSE_MOVE_ABSOLUTE.0 == 0 {
                    state.input(InputEvent::RelativeMotion {
                        dx: mouse.lLastX,
                        dy: mouse.lLastY,
                    });
                }
            }
            return DefWindowProcW(hwnd, message, wparam, lparam);
        }
        WM_SETCURSOR if state.shared && (lparam.0 as u16 as u32) == HTCLIENT => {
            let _ = SetCursor(None);
            return LRESULT(1);
        }
        WM_MOUSEMOVE => {
            if state.relative {
                return LRESULT(0);
            }
            let (x, y) = state.coords(hwnd, lparam, false);
            state.input(InputEvent::Pointer { x, y });
            return LRESULT(0);
        }
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONUP
        | WM_MBUTTONUP | WM_XBUTTONDOWN | WM_XBUTTONUP => {
            let down = matches!(
                message,
                WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN
            );
            if down {
                if state.shared {
                    state.event(ClientEvent::ResumeInput {
                        created: std::time::Instant::now(),
                    });
                }
                if state.command_status.as_deref() != Some("Closing remote window…") {
                    state.command_status = None;
                    state.refresh_title(hwnd);
                }
                let _ = SetFocus(Some(hwnd));
                let _ = SetCapture(hwnd);
            } else if !state.relative {
                let _ = ReleaseCapture();
            }
            let button = match message {
                WM_LBUTTONDOWN | WM_LBUTTONUP => 0,
                WM_RBUTTONDOWN | WM_RBUTTONUP => 1,
                WM_XBUTTONDOWN | WM_XBUTTONUP => {
                    if wparam.0 >> 16 == XBUTTON1 as usize {
                        3
                    } else {
                        4
                    }
                }
                _ => 2,
            };
            let (x, y) = state.coords(hwnd, lparam, false);
            state.input(if state.relative {
                InputEvent::RelativeButton { button, down }
            } else {
                InputEvent::Button { button, down, x, y }
            });
            return LRESULT(if message == WM_XBUTTONDOWN || message == WM_XBUTTONUP {
                1
            } else {
                0
            });
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
            if state.shared && down && vk == VK_F11.0 && ctrl && alt {
                state.event(ClientEvent::ReleaseInput);
                state.forwarded.clear();
                state.toggle_relative(hwnd);
                return LRESULT(0);
            }
            if vk == VK_F10.0 && ctrl && alt {
                if down && lparam.0 & (1 << 30) == 0 {
                    toggle_debug_overlay(hwnd, state);
                }
                return LRESULT(0);
            }
            if down && vk == VK_F12.0 && ctrl && alt {
                state.release_mouse();
                state.event(ClientEvent::ReleaseInput);
                state.forwarded.clear();
                state.surrogate = None;
                if !state.shared {
                    let _ = ShowWindow(hwnd, SW_MINIMIZE);
                }
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
        // TranslateMessage also emits WM_SYSCHAR for Alt combinations. Those
        // shortcuts were already forwarded above; letting DefWindowProc handle
        // them would additionally activate the client's local menu. AltGr text
        // still follows the composed Unicode path.
        WM_CHAR | WM_SYSCHAR => {
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
    if message == WM_SYSCOMMAND && wparam.0 == SHOW_HEADER {
        if state.shared || state.remote.role == WindowRole::Desktop {
            return LRESULT(0);
        }
        state.event(ClientEvent::ReleaseInput);
        state.forwarded.clear();
        state.surrogate = None;
        state.event(ClientEvent::Action {
            window: state.remote.key,
            action: WindowAction::SetHeader {
                visible: !state.remote.show_header,
            },
        });
        return LRESULT(0);
    }
    if state.shared
        && message == WM_SYSCOMMAND
        && matches!(
            wparam.0,
            MODE_RESIZE | SWAP | PHYSICAL | REMOTE_MENU | FETCH | SHOW_HEADER
        )
    {
        return LRESULT(0);
    }
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
        state.refresh_title(hwnd);
        let _ = InvalidateRect(Some(hwnd), None, false);
        return LRESULT(0);
    }
    if message == WM_NCDESTROY {
        state.release_mouse();
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
    updates: crate::ClientReceiver,
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
        // Rust channel writes do not wake MsgWaitForMultipleObjects. Create the
        // thread's message queue before publishing the coalesced wake callback.
        let mut initial_message = MSG::default();
        let _ = PeekMessageW(&mut initial_message, None, 0, 0, PM_NOREMOVE);
        let ui_thread = windows::Win32::System::Threading::GetCurrentThreadId();
        updates.set_waker(move || {
            let _ = PostThreadMessageW(ui_thread, WM_APP + 43, WPARAM(0), LPARAM(0));
        });
        let preferences: HashMap<String, Preference> = frames_core::trust::config_dir()
            .ok()
            .and_then(|dir| std::fs::read(dir.join("windows.json")).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let mut windows = HashMap::<WindowId, HWND>::new();
        let mut running = true;
        if std::env::var("FRAMES_DEBUG_OVERLAY").as_deref() == Ok("1") {
            DEBUG_ENABLED.store(true, std::sync::atomic::Ordering::Relaxed);
            let _ = events.send(ClientEvent::DebugOverlay { enabled: true });
        }
        let mut shared: Option<frames_core::shared_game::SharedSession> = None;
        let mut shared_cursor = frames_core::shared_game::Cursor::default();
        let mut view_mode = ViewMode::Windows;
        let mut show_remote_cursor = true;
        let mut adb_enabled = false;
        let mut device_list = frames_core::devices::DeviceList::default();
        let mut session_control = super::session_control::SessionControl::new(events.clone())?;
        let mut shelf = super::files::Shelf::new(events.clone())?;
        let timer = SetTimer(None, 0, 16, None);
        let mut last_visibility = std::time::Instant::now();
        let mut last_shared_alive = std::time::Instant::now();
        while running {
            if shared.is_some()
                && last_shared_alive.elapsed() >= std::time::Duration::from_millis(100)
            {
                let _ = events.send(ClientEvent::SharedAlive {
                    created: std::time::Instant::now(),
                });
                last_shared_alive = std::time::Instant::now();
            }
            for update in updates.try_iter() {
                match update {
                    ClientUpdate::Quality(preset) => session_control.set_quality(preset),
                    ClientUpdate::SharedSession(info) => {
                        let info = *info;
                        shared = Some(info.clone());
                        session_control.set_shared(true);
                        for &hwnd in windows.values() {
                            let state =
                                &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Proxy);
                            state.shared = true;
                            state.compatibility = info.compatibility.clone();
                            if info.phase != frames_core::shared_game::Phase::Active {
                                state.release_mouse();
                                state.event(ClientEvent::ReleaseInput);
                                state.forwarded.clear();
                            }
                            state.pref.physical = true;
                            state.pref.swap = false;
                            state.pref.mode = SizingMode::StretchLocal;
                            state.command_status = Some(info.detail.clone());
                            state.refresh_title(hwnd);
                        }
                    }
                    ClientUpdate::SharedCursor(cursor) => {
                        let mut latest = cursor.clone();
                        if latest.png_base64.is_none() {
                            latest.png_base64 = shared_cursor.png_base64.clone();
                        }
                        shared_cursor = latest;
                        for &hwnd in windows.values() {
                            let state =
                                &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Proxy);
                            let (rect, scale) = state.cursor_geometry(hwnd);
                            if let Some(overlay) = state.virtual_cursor.as_mut() {
                                let _ = overlay.update(&cursor, rect, scale);
                            }
                        }
                    }
                    ClientUpdate::Devices(list) => {
                        device_list = list.clone();
                        session_control.update_devices(list.clone());
                        if let Some(error) = &list.error {
                            let _ = crate::show_error("Device forwarding", error);
                        }
                        for &hwnd in windows.values() {
                            let state =
                                &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Proxy);
                            state.devices.state = list.clone();
                            let items = state.menu_items.clone();
                            install_remote_menu(hwnd, state, items);
                        }
                    }
                    ClientUpdate::RemoteCursor { visible } => {
                        show_remote_cursor = visible;
                        session_control.update(view_mode, adb_enabled, show_remote_cursor);
                        for &hwnd in windows.values() {
                            let state =
                                &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Proxy);
                            state.show_remote_cursor = visible;
                            let items = state.menu_items.clone();
                            install_remote_menu(hwnd, state, items);
                        }
                    }
                    ClientUpdate::ViewMode { mode, error } => {
                        view_mode = mode;
                        session_control.update(view_mode, adb_enabled, show_remote_cursor);
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
                        session_control.update(view_mode, adb_enabled, show_remote_cursor);
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
                        name,
                    } => {
                        host = format!("{platform}:{device_id}");
                        session_control.set_peer(name);
                    }
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
                                state.update_remote(hwnd, remote);
                                continue;
                            }
                            let key = remote.key;
                            let parent = remote.parent.and_then(|k| windows.get(&k).copied());
                            let transient = parent.is_some()
                                && matches!(
                                    remote.role,
                                    WindowRole::Menu | WindowRole::Tooltip | WindowRole::Popover
                                );
                            let mut pref = preferences
                                .get(&format!("{host}:{}", remote.app_id))
                                .cloned()
                                .unwrap_or_else(|| Preference {
                                    swap: host.starts_with("macos:"),
                                    ..Default::default()
                                });
                            let width = remote.bounds.width.min(1400.0) as i32;
                            let height = remote.bounds.height.min(900.0) as i32;
                            let icon = remote.icon_png.as_deref().and_then(|s| make_icon(s).ok());
                            if shared.is_some() {
                                pref.physical = true;
                                pref.swap = false;
                                pref.mode = SizingMode::StretchLocal;
                            }
                            let state = Box::new(Proxy {
                                hidden: false,
                                header: HWND::default(),
                                shared: shared.is_some(),
                                relative: false,
                                virtual_cursor: None,
                                compatibility: shared
                                    .as_ref()
                                    .map(|s| s.compatibility.clone())
                                    .unwrap_or_default(),
                                devices: super::devices::DeviceMenu::new(device_list.clone()),
                                remote,
                                frame: None,
                                events: events.clone(),
                                pref,
                                host: host.clone(),
                                icon,
                                status: "Connecting stream…".into(),
                                last_title: String::new(),
                                command_status: shared.as_ref().map(|s| s.detail.clone()),
                                shared_notice: None,
                                pending_resize: None,
                                last_size: None,
                                visible: None,
                                forwarded: Default::default(),
                                surrogate: None,
                                renderer: None,
                                renderer_failed: false,
                                ready: false,
                                user_sizing: false,
                                maximized: false,
                                view_mode,
                                show_remote_cursor,
                                adb_enabled,
                                menu_items: vec![],
                                menu_ids: HashMap::new(),
                                menu_open: false,
                                pending_menu: None,
                                debug_panel: None,
                                debug_snapshot: None,
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
                                    WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN
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
                            if (*state).shared {
                                (*state).virtual_cursor =
                                    super::virtual_cursor::VirtualCursor::new(hwnd).ok();
                                let (rect, scale) = (*state).cursor_geometry(hwnd);
                                if let Some(overlay) = (*state).virtual_cursor.as_mut() {
                                    let _ = overlay.update(&shared_cursor, rect, scale);
                                }
                            }
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
                            // Apply one scale to both axes, including app windows.
                            // Independent caps otherwise create black letterboxing at startup.
                            let fit = (picture_width / (*state).remote.bounds.width)
                                .min(picture_height / (*state).remote.bounds.height);
                            picture_width = (*state).remote.bounds.width * fit;
                            picture_height = (*state).remote.bounds.height * fit;
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
                                    WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN
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

                            windows::Win32::UI::Shell::DragAcceptFiles(hwnd, !(*state).shared);
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
                            if !transient {
                                (*state).header = super::caption::create(hwnd)?;
                                let _ = SetWindowPos(
                                    hwnd,
                                    None,
                                    0,
                                    0,
                                    0,
                                    0,
                                    SWP_FRAMECHANGED
                                        | SWP_NOMOVE
                                        | SWP_NOSIZE
                                        | SWP_NOZORDER
                                        | SWP_NOACTIVATE,
                                );
                            }

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
                            if state.hidden {
                                continue;
                            }
                            state.frame = Some(frame);
                            if !state.status.is_empty() {
                                state.status.clear();
                                state.refresh_title(hwnd);
                            }
                            let _ = InvalidateRect(Some(hwnd), None, false);
                        }
                    }
                    ClientUpdate::DebugOverlay(key, overlay) => {
                        if DEBUG_ENABLED.load(std::sync::atomic::Ordering::Relaxed)
                            && let Some(&hwnd) = windows.get(&key)
                        {
                            let state =
                                &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Proxy);
                            state.debug_snapshot = Some(overlay);
                            state.refresh_debug(hwnd);
                        }
                    }
                    ClientUpdate::Status(key, status) => {
                        if let Some(&hwnd) = windows.get(&key) {
                            let state =
                                &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Proxy);
                            if state.shared {
                                state.shared_notice = Some(status);
                            } else {
                                state.status = status;
                            }
                            state.refresh_title(hwnd);
                            let _ = InvalidateRect(Some(hwnd), None, false);
                        }
                    }
                    ClientUpdate::Ack(key, actual, error) => {
                        if let Some(&hwnd) = windows.get(&key) {
                            let state =
                                &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Proxy);
                            if let Some(actual) = actual {
                                state.update_remote(hwnd, actual);
                            }
                            if let Some(error) = error {
                                state.command_status = Some(error);
                            }
                            state.refresh_title(hwnd);
                            let _ = InvalidateRect(Some(hwnd), None, false);
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
                session_control.sync_windows(
                    windows
                        .values()
                        .map(|&hwnd| {
                            (&*(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Proxy))
                                .tray_window()
                        })
                        .collect(),
                );
            }
            let tray_windows: Vec<_> = windows
                .values()
                .map(|&hwnd| {
                    (&*(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Proxy)).tray_window()
                })
                .collect();
            for action in session_control.poll(&tray_windows) {
                use super::session_control::SessionAction;
                match action {
                    SessionAction::Visibility(key, shown) => {
                        if let Some(&hwnd) = windows.get(&key) {
                            (&mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Proxy))
                                .set_shown(hwnd, shown);
                        }
                    }
                    SessionAction::WindowCommand(key, command) => {
                        if let Some(&hwnd) = windows.get(&key) {
                            let _ = PostMessageW(
                                Some(hwnd),
                                WM_COMMAND,
                                WPARAM(command as usize),
                                LPARAM(0),
                            );
                        }
                    }
                    SessionAction::Device(action) => session_control.dispatch_device(action),
                }
            }
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    running = false;
                }
                if msg.message == DEBUG_TOGGLED {
                    for &hwnd in windows.values() {
                        (&mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Proxy))
                            .refresh_debug(hwnd);
                    }
                    continue;
                }
                if msg.message == super::session_control::OPEN {
                    let hwnd = HWND(msg.wParam.0 as *mut _);
                    if let Some((&key, _)) = windows.iter().find(|(_, value)| **value == hwnd) {
                        let mut anchor = RECT::default();
                        let proxy = &*(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Proxy);
                        let _ = GetWindowRect(proxy.header, &mut anchor);
                        session_control.open(key, tray_windows.clone(), anchor.left, anchor.bottom);
                    }
                    continue;
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

unsafe fn toggle_debug_overlay(hwnd: HWND, state: &Proxy) {
    let enabled = !DEBUG_ENABLED.fetch_xor(true, std::sync::atomic::Ordering::Relaxed);
    state.event(ClientEvent::DebugOverlay { enabled });
    let _ = PostThreadMessageW(
        GetWindowThreadProcessId(hwnd, None),
        DEBUG_TOGGLED,
        WPARAM(0),
        LPARAM(0),
    );
}
fn stable_hash(value: &str) -> u64 {
    value.bytes().fold(0xcbf29ce484222325, |h, b| {
        (h ^ b as u64).wrapping_mul(0x100000001b3)
    })
}

fn header_menu_flags(state: &Proxy) -> MENU_ITEM_FLAGS {
    MF_STRING
        | if state.remote.show_header {
            MF_CHECKED
        } else {
            MF_UNCHECKED
        }
        | if state.shared || state.remote.role == WindowRole::Desktop {
            MF_GRAYED
        } else {
            MF_ENABLED
        }
}

unsafe fn install_remote_menu(_hwnd: HWND, state: &mut Proxy, items: Vec<MenuItem>) {
    fn collect(
        items: &[MenuItem],
        next: &mut usize,
        ids: &mut HashMap<usize, String>,
        depth: usize,
    ) {
        if depth >= 8 {
            return;
        }
        for item in items.iter().take(200) {
            if item.children.is_empty() && !item.title.is_empty() && *next < 0x7000 {
                ids.insert(*next, item.id.clone());
                *next += 1;
            } else {
                collect(&item.children, next, ids, depth + 1);
            }
        }
    }
    let mut ids = HashMap::new();
    collect(&items, &mut 0x100usize, &mut ids, 0);
    state.menu_items = items;
    state.menu_ids = ids;
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

#[cfg(test)]
mod header_tests {
    use super::*;

    #[test]
    fn header_toggle_changes_remote_capture_and_preserves_local_window() {
        unsafe {
            let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
            let mut state = Box::new(Proxy {
                hidden: false,
                header: HWND::default(),
                shared: false,
                compatibility: Default::default(),
                relative: false,
                virtual_cursor: None,
                devices: Default::default(),
                remote: RemoteWindow {
                    key: WindowId {
                        id: 42,
                        generation: 1,
                    },
                    app_id: "test".into(),
                    app_name: "Test".into(),
                    title: "Remote window".into(),
                    icon_png: None,
                    parent: None,
                    role: WindowRole::Window,
                    bounds: Bounds {
                        width: 440.,
                        height: 320.,
                        ..Default::default()
                    },
                    scale: 1.,
                    resize: ResizeCapability::Supported,
                    minimized: false,
                    visible: true,
                    modal: false,
                    show_header: false,
                },
                frame: None,
                events,
                pref: Default::default(),
                host: "test".into(),
                icon: None,
                status: String::new(),
                last_title: String::new(),
                command_status: None,
                shared_notice: None,
                pending_resize: None,
                last_size: None,
                visible: None,
                forwarded: Default::default(),
                surrogate: None,
                renderer: None,
                renderer_failed: false,
                ready: false,
                user_sizing: false,
                maximized: false,
                view_mode: ViewMode::Windows,
                show_remote_cursor: true,
                adb_enabled: false,
                menu_items: vec![],
                menu_ids: HashMap::new(),
                menu_open: false,
                pending_menu: None,
                debug_panel: None,
                debug_snapshot: None,
            });
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                windows::core::w!("STATIC"),
                windows::core::w!("Header regression"),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                640,
                480,
                None,
                None,
                None,
                None,
            )
            .unwrap();
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, (&mut *state as *mut Proxy) as isize);
            install_remote_menu(hwnd, &mut state, vec![]);
            let system = GetSystemMenu(hwnd, false);
            AppendMenuW(
                system,
                header_menu_flags(&state),
                SHOW_HEADER,
                windows::core::w!("Show remote window header"),
            )
            .unwrap();
            let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
            let mut frame = RECT::default();
            GetWindowRect(hwnd, &mut frame).unwrap();
            for visible in [true, false] {
                state.forwarded.insert(4);
                state.surrogate = Some(0xd800);
                procedure(hwnd, WM_SYSCOMMAND, WPARAM(SHOW_HEADER), LPARAM(0));
                assert!(matches!(
                    received.try_recv().unwrap(),
                    ClientEvent::ReleaseInput
                ));
                assert!(matches!(received.try_recv().unwrap(), ClientEvent::Action {
                    window, action: WindowAction::SetHeader { visible: value }
                } if window == state.remote.key && value == visible));
                assert!(state.forwarded.is_empty() && state.surrogate.is_none());
                // The host acknowledgement, rather than the click, owns the checkmark.
                assert_eq!(state.remote.show_header, !visible);
                let mut actual = state.remote.clone();
                actual.show_header = visible;
                state.update_remote(hwnd, actual);
                {
                    let menu = system;
                    assert_eq!(
                        GetMenuState(menu, SHOW_HEADER as u32, MF_BYCOMMAND) & MF_CHECKED.0,
                        if visible { MF_CHECKED.0 } else { 0 }
                    );
                }
                assert_eq!(GetWindowLongPtrW(hwnd, GWL_STYLE), style);
                let mut after = RECT::default();
                GetWindowRect(hwnd, &mut after).unwrap();
                assert_eq!(
                    (after.left, after.top, after.right, after.bottom),
                    (frame.left, frame.top, frame.right, frame.bottom)
                );
            }
            for shared in [false, true] {
                state.shared = shared;
                let mut actual = state.remote.clone();
                actual.role = if shared {
                    WindowRole::Window
                } else {
                    WindowRole::Desktop
                };
                state.update_remote(hwnd, actual);
                assert_ne!(header_menu_flags(&state) & MF_GRAYED, MENU_ITEM_FLAGS(0));
                procedure(hwnd, WM_SYSCOMMAND, WPARAM(SHOW_HEADER), LPARAM(0));
                assert!(received.try_recv().is_err());
            }
            // Shared viewers never emit remote window actions, even if stale
            // normal-mode preferences still request remote resizing.
            state.shared = true;
            state.ready = true;
            state.user_sizing = true;
            state.pref.mode = SizingMode::ResizeRemote;
            state.forwarded.insert(0x1a);
            procedure(hwnd, WM_SETFOCUS, WPARAM(0), LPARAM(0));
            procedure(hwnd, WM_SIZE, WPARAM(0), LPARAM((320 << 16) | 440));
            procedure(hwnd, WM_KILLFOCUS, WPARAM(0), LPARAM(0));
            procedure(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
            let events: Vec<_> = std::iter::from_fn(|| received.try_recv().ok()).collect();
            assert!(
                events
                    .iter()
                    .all(|event| !matches!(event, ClientEvent::Action { .. }))
            );
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, ClientEvent::Disconnected))
            );
            assert!(state.forwarded.is_empty());
            assert!(state.pending_resize.is_none());
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            DestroyWindow(hwnd).unwrap();
        }
    }
}
