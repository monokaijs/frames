//! A window's Win32 menu bar, for the viewer (`crate::menus`), ported from
//! v1's menu projection: reading sends the window WM_INITMENU and
//! WM_INITMENUPOPUP (20 ms each at most) so items show their current state;
//! an item runs as its own WM_COMMAND (or WM_MENUCOMMAND for menus that
//! notify by position), after checking it is still the same and enabled.

use crate::menus::{Item, MAX_DEPTH, MAX_ITEMS};
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PWSTR;

#[derive(Clone)]
struct Target {
    root: usize,
    menu: usize,
    position: u32,
    command: u32,
    title: String,
}

/// The items of each window's last tree, by id.
static TARGETS: Mutex<Option<HashMap<u64, HashMap<u32, Target>>>> = Mutex::new(None);

struct NativeItem {
    id: u32,
    state: MENU_ITEM_STATE,
    kind: MENU_ITEM_TYPE,
    submenu: HMENU,
    title: String,
}

unsafe fn item(menu: HMENU, position: u32) -> Result<NativeItem> {
    let mut text = vec![0u16; 1025];
    let mut info = MENUITEMINFOW {
        cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
        fMask: MIIM_STRING | MIIM_STATE | MIIM_ID | MIIM_SUBMENU | MIIM_FTYPE,
        dwTypeData: PWSTR(text.as_mut_ptr()),
        cch: 1024,
        ..Default::default()
    };
    unsafe { GetMenuItemInfoW(menu, position, true, &mut info)? };
    Ok(NativeItem {
        id: info.wID,
        state: info.fState,
        kind: info.fType,
        submenu: info.hSubMenu,
        title: String::from_utf16_lossy(&text[..info.cch.min(1024) as usize]),
    })
}

struct Walk {
    hwnd: HWND,
    root: HMENU,
    next_id: u32,
    count: usize,
    started: Instant,
    targets: HashMap<u32, Target>,
}

impl Walk {
    unsafe fn menu(&mut self, menu: HMENU, depth: usize) -> Result<Vec<Item>> {
        let mut items = Vec::new();
        if depth >= MAX_DEPTH {
            return Ok(items);
        }
        for position in 0..unsafe { GetMenuItemCount(Some(menu)) }.clamp(0, 200) as u32 {
            self.count += 1;
            if self.count > MAX_ITEMS || self.started.elapsed() > Duration::from_millis(300) {
                break;
            }
            let native = unsafe { item(menu, position)? };
            if native.kind.0 & MFT_SEPARATOR.0 != 0 {
                items.push(Item {
                    separator: true,
                    ..Item::default()
                });
                continue;
            }
            // Menu text marks access keys with &; titles show none.
            let title = native.title.replace('&', "");
            if title.is_empty() {
                continue;
            }
            let children = if native.submenu.is_invalid() {
                Vec::new()
            } else {
                unsafe {
                    let _ = SendMessageTimeoutW(
                        self.hwnd,
                        WM_INITMENUPOPUP,
                        WPARAM(native.submenu.0 as usize),
                        LPARAM(position as isize),
                        SMTO_ABORTIFHUNG,
                        20,
                        None,
                    );
                    self.menu(native.submenu, depth + 1)?
                }
            };
            let enabled = native.state.0 & MFS_DISABLED.0 == 0;
            let mut id = 0;
            if children.is_empty() && enabled && native.id <= 0xffff {
                self.next_id += 1;
                id = self.next_id;
                self.targets.insert(
                    id,
                    Target {
                        root: self.root.0 as usize,
                        menu: menu.0 as usize,
                        position,
                        command: native.id,
                        title: native.title.clone(),
                    },
                );
            }
            items.push(Item {
                id,
                title,
                enabled,
                checked: native.state.0 & MFS_CHECKED.0 != 0,
                separator: false,
                children,
            });
        }
        Ok(items)
    }
}

/// The menu bar of `window` (an HWND), empty when it has none.
pub fn read(window: u64) -> Result<Vec<Item>> {
    let hwnd = HWND(window as usize as *mut _);
    unsafe {
        let root = GetMenu(hwnd);
        if root.is_invalid() {
            return Ok(Vec::new());
        }
        let _ = SendMessageTimeoutW(
            hwnd,
            WM_INITMENU,
            WPARAM(root.0 as usize),
            LPARAM(0),
            SMTO_ABORTIFHUNG,
            20,
            None,
        );
        let mut walk = Walk {
            hwnd,
            root,
            next_id: 0,
            count: 0,
            started: Instant::now(),
            targets: HashMap::new(),
        };
        let items = walk.menu(root, 0)?;
        TARGETS
            .lock()
            .unwrap()
            .get_or_insert_with(HashMap::new)
            .insert(window, walk.targets);
        Ok(items)
    }
}

/// Runs item `id` of `window`'s last tree, if it is unchanged and enabled.
pub fn invoke(window: u64, id: u32) -> Result<()> {
    let target = TARGETS
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|windows| windows.get(&window)?.get(&id).cloned())
        .ok_or_else(|| anyhow::anyhow!("that menu item is no longer there"))?;
    let hwnd = HWND(window as usize as *mut _);
    unsafe {
        anyhow::ensure!(
            GetMenu(hwnd).0 as usize == target.root,
            "the application's menu changed"
        );
        let menu = HMENU(target.menu as *mut _);
        let native = item(menu, target.position)?;
        anyhow::ensure!(
            native.id == target.command
                && native.title == target.title
                && native.state.0 & MFS_DISABLED.0 == 0,
            "that menu item changed or is disabled"
        );
        let mut properties = MENUINFO {
            cbSize: std::mem::size_of::<MENUINFO>() as u32,
            fMask: MIM_STYLE,
            ..Default::default()
        };
        let by_position = GetMenuInfo(menu, &mut properties).is_ok()
            && properties.dwStyle.0 & MNS_NOTIFYBYPOS.0 != 0;
        if by_position {
            PostMessageW(
                Some(hwnd),
                WM_MENUCOMMAND,
                WPARAM(target.position as usize),
                LPARAM(menu.0 as isize),
            )?;
        } else {
            PostMessageW(
                Some(hwnd),
                WM_COMMAND,
                WPARAM(target.command as usize),
                LPARAM(0),
            )?;
        }
    }
    Ok(())
}
