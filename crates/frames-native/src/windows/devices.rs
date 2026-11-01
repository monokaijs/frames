//! Shared device menus for remote windows and the persistent session menu.
use super::wide;
use crate::ClientEvent;
use frames_core::devices::{DeviceAction, DeviceList};
use std::collections::HashMap;
use windows::{Win32::UI::WindowsAndMessaging::*, core::PCWSTR};

#[derive(Default)]
pub struct DeviceMenu {
    pub state: DeviceList,
    actions: HashMap<usize, DeviceAction>,
}
impl DeviceMenu {
    pub fn new(state: DeviceList) -> Self {
        Self {
            state,
            actions: HashMap::new(),
        }
    }
    pub fn action(
        &self,
        id: usize,
        events: &tokio::sync::mpsc::UnboundedSender<ClientEvent>,
    ) -> bool {
        if let Some(action) = self.actions.get(&id) {
            Self::dispatch(action.clone(), events);
            true
        } else {
            false
        }
    }
    pub fn selection(&self, id: usize) -> Option<DeviceAction> {
        self.actions.get(&id).cloned()
    }
    pub fn dispatch(
        action: DeviceAction,
        events: &tokio::sync::mpsc::UnboundedSender<ClientEvent>,
    ) {
        if matches!(&action,DeviceAction::Setup { target } if target == "usb-export")
            && super::dialogs::message(
                "USB forwarding setup",
                "This installs optional USB/IP support and restricts its service to local connections for encrypted Frames forwarding. Existing USB/IP LAN clients will lose access. Disconnect active USB work before installing drivers. Continue?",
                MB_OKCANCEL | MB_ICONINFORMATION,
            ) != IDOK
        {
            return;
        }
        let _ = events.send(ClientEvent::Devices(action));
    }
    /// # Safety
    /// `parent` must be a live menu owned by the current UI thread.
    pub unsafe fn append(&mut self, parent: HMENU) {
        unsafe {
            self.actions.clear();
            let Ok(gamepads) = CreatePopupMenu() else {
                return;
            };
            let Ok(usb) = CreatePopupMenu() else {
                let _ = DestroyMenu(gamepads);
                return;
            };
            let mut next = 0xB100;
            let mut item = |menu, title: &str, flags, action| {
                next += 1;
                let _ = AppendMenuW(menu, MF_STRING | flags, next, PCWSTR(wide(title).as_ptr()));
                if let Some(action) = action {
                    self.actions.insert(next, action);
                }
            };
            if self.state.gamepads.is_empty() {
                item(gamepads, "No mapped gamepads found", MF_GRAYED, None);
            }
            for device in self.state.gamepads.iter().take(32) {
                let name: String = device
                    .name
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(80)
                    .collect();
                item(
                    gamepads,
                    &format!(
                        "{}{}",
                        name,
                        device
                            .status
                            .as_ref()
                            .map(|status| format!(" — {status}"))
                            .unwrap_or_default()
                    ),
                    if device.forwarded {
                        MF_CHECKED
                    } else {
                        MENU_ITEM_FLAGS(0)
                    },
                    Some(DeviceAction::Gamepad {
                        id: device.id.clone(),
                        enabled: !device.forwarded,
                    }),
                );
            }
            if let Some(error) = &self.state.capabilities.gamepad_error {
                item(
                    gamepads,
                    &error.chars().take(140).collect::<String>(),
                    MF_GRAYED,
                    None,
                );
            }
            item(
                gamepads,
                "Set up gamepad driver on host...",
                MENU_ITEM_FLAGS(0),
                Some(DeviceAction::Setup {
                    target: "gamepad".into(),
                }),
            );
            if self.state.usb.is_empty() {
                item(
                    usb,
                    "No USB devices found; refresh after setup",
                    MF_GRAYED,
                    None,
                );
            }
            if let Some(error) = &self.state.usb_local_error {
                item(
                    usb,
                    &error.chars().take(140).collect::<String>(),
                    MF_GRAYED,
                    None,
                );
            }
            for device in self.state.usb.iter().take(32) {
                let name: String = device
                    .name
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(70)
                    .collect();
                item(
                    usb,
                    &format!(
                        "{} — {}{}",
                        device.id,
                        name,
                        device
                            .status
                            .as_ref()
                            .map(|status| format!(" ({status})"))
                            .unwrap_or_default()
                    ),
                    if device.forwarded {
                        MF_CHECKED
                    } else if device.error.is_some() {
                        MF_GRAYED
                    } else {
                        MENU_ITEM_FLAGS(0)
                    },
                    Some(DeviceAction::Usb {
                        id: device.id.clone(),
                        enabled: !device.forwarded,
                    }),
                );
                if let Some(error) = &device.error {
                    item(
                        usb,
                        &error.chars().take(140).collect::<String>(),
                        MF_GRAYED,
                        None,
                    );
                }
            }
            if let Some(error) = &self.state.capabilities.usb_error {
                item(
                    usb,
                    &error.chars().take(140).collect::<String>(),
                    MF_GRAYED,
                    None,
                );
            }
            item(
                usb,
                "Set up local USB forwarding...",
                MENU_ITEM_FLAGS(0),
                Some(DeviceAction::Setup {
                    target: "usb-export".into(),
                }),
            );
            item(
                usb,
                "Set up USB driver on host...",
                MENU_ITEM_FLAGS(0),
                Some(DeviceAction::Setup {
                    target: "usb-host".into(),
                }),
            );
            let _ = AppendMenuW(
                parent,
                MF_POPUP,
                gamepads.0 as usize,
                PCWSTR(wide("Gamepads").as_ptr()),
            );
            let _ = AppendMenuW(
                parent,
                MF_POPUP,
                usb.0 as usize,
                PCWSTR(wide("USB devices").as_ptr()),
            );
            item(
                parent,
                "Refresh devices",
                MENU_ITEM_FLAGS(0),
                Some(DeviceAction::Refresh),
            );
        }
    }
}
