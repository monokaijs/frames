//! Persistent per-connection tray controls. Hidden proxies remain in this list.
use super::{
    devices::DeviceMenu,
    tray::{Tray, TrayGlyph as G, TrayItem as Item},
};
use crate::ClientEvent;
use frames_core::{
    devices::DeviceAction,
    model::{ViewMode, WindowId},
};
use std::collections::HashMap;
use windows::{Win32::UI::WindowsAndMessaging::*, core::PWSTR};

pub struct SessionWindow {
    pub key: WindowId,
    pub title: String,
    pub shown: bool,
    pub options: Vec<Item>,
}
#[derive(Clone)]
pub enum SessionAction {
    Visibility(WindowId, bool),
    WindowCommand(WindowId, u32),
    Device(DeviceAction),
}
pub struct SessionControl {
    tray: Tray,
    events: tokio::sync::mpsc::UnboundedSender<ClientEvent>,
    mode: ViewMode,
    adb: bool,
    cursor: bool,
    shared: bool,
    connected: bool,
    peer: String,
    devices: DeviceMenu,
    actions: HashMap<u32, SessionAction>,
    ids: HashMap<String, u32>,
    next: u32,
}
impl SessionControl {
    pub fn new(events: tokio::sync::mpsc::UnboundedSender<ClientEvent>) -> anyhow::Result<Self> {
        Ok(Self {
            tray: Tray::new("Frames connection")?,
            events,
            mode: ViewMode::Windows,
            adb: false,
            cursor: true,
            shared: false,
            connected: false,
            peer: "Connecting".into(),
            devices: Default::default(),
            actions: HashMap::new(),
            ids: HashMap::new(),
            next: 0x10000,
        })
    }
    pub fn update(&mut self, mode: ViewMode, adb: bool, cursor: bool) {
        self.mode = mode;
        self.adb = adb;
        self.cursor = cursor;
    }
    pub fn set_shared(&mut self, shared: bool) {
        self.shared = shared;
    }
    pub fn set_peer(&mut self, peer: String) {
        self.connected = true;
        self.peer = peer;
    }
    pub fn update_devices(&mut self, state: frames_core::devices::DeviceList) {
        self.devices.state = state;
    }
    fn register(&mut self, key: String, action: SessionAction) -> u32 {
        let id = *self.ids.entry(key).or_insert_with(|| {
            self.next += 1;
            self.next
        });
        self.actions.insert(id, action);
        id
    }
    pub fn sync_windows(&mut self, mut windows: Vec<SessionWindow>) {
        windows.sort_by(|a, b| {
            a.title
                .to_lowercase()
                .cmp(&b.title.to_lowercase())
                .then(a.key.cmp(&b.key))
        });
        let shown = windows.iter().filter(|w| w.shown).count();
        let mut list = Vec::new();
        let mut options = Vec::new();
        for window in windows {
            let id = self.register(
                format!("toggle:{:?}:{}", window.key, !window.shown),
                SessionAction::Visibility(window.key, !window.shown),
            );
            list.push(
                Item::new(
                    id,
                    &window.title,
                    if window.shown { G::Show } else { G::Hide },
                )
                .checked(window.shown),
            );
            let children = self.window_items(window.key, window.options);
            options.push(Item::new(0, window.title, G::Window).children(children));
        }
        let has_windows = !list.is_empty();
        if list.is_empty() {
            list.push(Item::new(0, "No remote windows available", G::Window).enabled(false));
        }
        let mut items = vec![
            Item::new(
                0,
                if self.connected {
                    format!("Connected · {}", self.peer)
                } else {
                    self.peer.clone()
                },
                G::Network,
            )
            .enabled(false),
            Item::separator(),
            Item::new(0, "Windows", G::Window).children(list),
            Item::new(6, "Show all windows", G::Show).enabled(has_windows),
            Item::new(7, "Hide all windows", G::Hide).enabled(shown > 0),
            Item::separator(),
        ];
        if !options.is_empty() {
            items.push(Item::new(0, "Window options", G::Settings).children(options));
        }
        if !self.shared {
            items.push(Item::new(0, "View", G::Display).children(vec![
                    Item::new(1, "Per-window view", G::Window)
                        .checked(self.mode == ViewMode::Windows),
                    Item::new(2, "Full desktop view", G::Display)
                        .checked(self.mode == ViewMode::Desktop),
                ]));
            let devices = unsafe { self.device_items() };
            items.push(Item::new(0, "Devices", G::Devices).children(devices));
            items.push(Item::new(3, "Android ADB bridge", G::Network).checked(self.adb));
        }
        items.push(Item::new(5, "Show remote cursor", G::Cursor).checked(self.cursor));
        items.push(Item::separator());
        items.push(Item::new(4, "Disconnect", G::Power));
        self.tray.update(
            items,
            self.connected,
            format!("Frames · {} · {shown} windows shown", self.peer),
        );
    }
    pub fn poll(&mut self, windows: &[SessionWindow]) -> Vec<SessionAction> {
        let mut result = Vec::new();
        for command in self.tray.commands.try_iter().collect::<Vec<_>>() {
            let _ = self.events.send(ClientEvent::ReleaseInput);
            match command {
                1 | 2 => {
                    let _ = self.events.send(ClientEvent::ViewMode(if command == 1 {
                        ViewMode::Windows
                    } else {
                        ViewMode::Desktop
                    }));
                }
                3 => {
                    let _ = self
                        .events
                        .send(ClientEvent::AdbBridge { enabled: !self.adb });
                }
                4 => {
                    let _ = self.events.send(ClientEvent::Disconnected);
                }
                5 => {
                    let _ = self.events.send(ClientEvent::RemoteCursor {
                        visible: !self.cursor,
                    });
                }
                6 | 7 => result.extend(
                    windows
                        .iter()
                        .map(|window| SessionAction::Visibility(window.key, command == 6)),
                ),
                _ => {
                    if let Some(action) = self.actions.get(&command) {
                        result.push(action.clone());
                    }
                }
            }
        }
        result
    }
    pub fn dispatch_device(&self, action: DeviceAction) {
        DeviceMenu::dispatch(action, &self.events);
    }
    fn window_items(&mut self, window: WindowId, items: Vec<Item>) -> Vec<Item> {
        items
            .into_iter()
            .map(|mut item| {
                if item.id != 0 {
                    item.id = self.register(
                        format!("command:{window:?}:{}", item.id),
                        SessionAction::WindowCommand(window, item.id),
                    );
                }
                item.children = self.window_items(window, item.children);
                item
            })
            .collect()
    }
    unsafe fn device_items(&mut self) -> Vec<Item> {
        unsafe fn read(owner: &mut SessionControl, menu: HMENU) -> Vec<Item> {
            let mut items = Vec::new();
            for index in 0..unsafe { GetMenuItemCount(Some(menu)) } {
                let mut text = [0u16; 512];
                let mut info = MENUITEMINFOW {
                    cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                    fMask: MIIM_STRING | MIIM_ID | MIIM_STATE | MIIM_SUBMENU | MIIM_FTYPE,
                    dwTypeData: PWSTR(text.as_mut_ptr()),
                    cch: 511,
                    ..Default::default()
                };
                if unsafe { GetMenuItemInfoW(menu, index as u32, true, &mut info) }.is_err() {
                    continue;
                }
                if info.fType.contains(MFT_SEPARATOR) {
                    items.push(Item::separator());
                    continue;
                }
                let mut item = Item::new(
                    0,
                    String::from_utf16_lossy(&text[..info.cch as usize]),
                    G::Devices,
                )
                .checked(info.fState.contains(MFS_CHECKED))
                .enabled(!info.fState.contains(MFS_DISABLED));
                if !info.hSubMenu.is_invalid() {
                    item.children = unsafe { read(owner, info.hSubMenu) };
                } else if let Some(action) = owner.devices.selection(info.wID as usize) {
                    item.id = owner.register(
                        format!(
                            "device:{}",
                            serde_json::to_string(&action).unwrap_or_default()
                        ),
                        SessionAction::Device(action),
                    );
                }
                items.push(item);
            }
            items
        }
        unsafe {
            let Ok(menu) = CreatePopupMenu() else {
                return vec![];
            };
            self.devices.append(menu);
            let result = read(self, menu);
            let _ = DestroyMenu(menu);
            result
        }
    }
}
