use frames_core::model::{MenuItem, WindowId};
use parking_lot::Mutex;
use std::collections::HashMap;
use windows::{
    Win32::{Foundation::*, UI::WindowsAndMessaging::*},
    core::*,
};

#[derive(Clone)]
struct Target {
    root: u64,
    menu: u64,
    position: u32,
    command: u32,
    title: String,
}
#[derive(Default)]
pub struct Projection {
    actions: Mutex<HashMap<WindowId, HashMap<String, Target>>>,
}
impl Projection {
    pub fn read(&self, key: WindowId, hwnd: HWND) -> anyhow::Result<Vec<MenuItem>> {
        unsafe {
            let root = GetMenu(hwnd);
            if root.is_invalid() {
                return Ok(vec![]);
            }
            let mut targets = HashMap::new();
            let mut count = 0;
            let _ = SendMessageTimeoutW(
                hwnd,
                WM_INITMENU,
                WPARAM(root.0 as usize),
                LPARAM(0),
                SMTO_ABORTIFHUNG,
                20,
                None,
            );
            let items = walk(
                hwnd,
                root,
                root,
                0,
                &mut count,
                &mut targets,
                std::time::Instant::now(),
            )?;
            self.actions.lock().insert(key, targets);
            Ok(items)
        }
    }
    pub fn invoke(&self, key: WindowId, hwnd: HWND, id: &str) -> anyhow::Result<()> {
        let target = self
            .actions
            .lock()
            .get(&key)
            .and_then(|items| items.get(id))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Menu item expired; refresh application menus"))?;
        unsafe {
            anyhow::ensure!(
                GetMenu(hwnd).0 as u64 == target.root,
                "Application menu changed; refresh its menus"
            );
            let menu = HMENU(target.menu as *mut _);
            let info = item_info(menu, target.position)?;
            anyhow::ensure!(
                info.wID == target.command
                    && item_title(&info) == target.title
                    && info.fState.0 & MFS_DISABLED.0 == 0,
                "Menu item changed or is disabled"
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
            Ok(())
        }
    }
}
#[allow(non_snake_case)]
struct NativeItem {
    wID: u32,
    fState: MENU_ITEM_STATE,
    fType: MENU_ITEM_TYPE,
    hSubMenu: HMENU,
    title: String,
}
unsafe fn item_info(menu: HMENU, position: u32) -> anyhow::Result<NativeItem> {
    unsafe {
        let mut text = vec![0u16; 1025];
        let mut info = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_STRING | MIIM_STATE | MIIM_ID | MIIM_SUBMENU | MIIM_FTYPE,
            dwTypeData: PWSTR(text.as_mut_ptr()),
            cch: 1024,
            ..Default::default()
        };
        GetMenuItemInfoW(menu, position, true, &mut info)?;
        Ok(NativeItem {
            wID: info.wID,
            fState: info.fState,
            fType: info.fType,
            hSubMenu: info.hSubMenu,
            title: String::from_utf16_lossy(&text[..info.cch.min(1024) as usize]),
        })
    }
}
fn item_title(info: &NativeItem) -> String {
    info.title.clone()
}
unsafe fn walk(
    hwnd: HWND,
    root: HMENU,
    menu: HMENU,
    depth: usize,
    count: &mut usize,
    targets: &mut HashMap<String, Target>,
    started: std::time::Instant,
) -> anyhow::Result<Vec<MenuItem>> {
    unsafe {
        if depth >= 8 {
            return Ok(vec![]);
        }
        let mut result = vec![];
        for position in 0..GetMenuItemCount(Some(menu)).clamp(0, 200) as u32 {
            *count += 1;
            if *count > 1024 || started.elapsed() > std::time::Duration::from_millis(200) {
                break;
            }
            let info = item_info(menu, position)?;
            if info.fType.0 & MFT_SEPARATOR.0 != 0 {
                result.push(MenuItem {
                    id: String::new(),
                    title: String::new(),
                    enabled: false,
                    checked: false,
                    children: vec![],
                });
                continue;
            }
            if info.title.is_empty() {
                continue;
            }
            let children = if !info.hSubMenu.is_invalid() {
                let _ = SendMessageTimeoutW(
                    hwnd,
                    WM_INITMENUPOPUP,
                    WPARAM(info.hSubMenu.0 as usize),
                    LPARAM(position as isize),
                    SMTO_ABORTIFHUNG,
                    20,
                    None,
                );
                walk(
                    hwnd,
                    root,
                    info.hSubMenu,
                    depth + 1,
                    count,
                    targets,
                    started,
                )?
            } else {
                vec![]
            };
            let id = uuid_token();
            let enabled =
                info.fState.0 & MFS_DISABLED.0 == 0 && (!children.is_empty() || info.wID <= 0xffff);
            if children.is_empty() && enabled {
                targets.insert(
                    id.clone(),
                    Target {
                        root: root.0 as u64,
                        menu: menu.0 as u64,
                        position,
                        command: info.wID,
                        title: info.title.clone(),
                    },
                );
            }
            result.push(MenuItem {
                id,
                title: info.title,
                enabled,
                checked: info.fState.0 & MFS_CHECKED.0 != 0,
                children,
            });
        }
        Ok(result)
    }
}
fn uuid_token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!("win-menu-{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    static COMMAND: AtomicU32 = AtomicU32::new(0);
    unsafe extern "system" fn proc(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        if message == WM_COMMAND {
            COMMAND.store(w.0 as u32, Ordering::SeqCst);
            return LRESULT(0);
        }
        if message == WM_MENUCOMMAND {
            COMMAND.store(1000 + w.0 as u32, Ordering::SeqCst);
            return LRESULT(0);
        }
        unsafe { DefWindowProcW(hwnd, message, w, l) }
    }
    #[test]
    #[ignore = "uses a temporary native Windows menu fixture on an interactive desktop"]
    fn projects_native_menus_and_rejects_changed_actions() {
        unsafe {
            let instance = windows::Win32::System::LibraryLoader::GetModuleHandleW(None).unwrap();
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(proc),
                hInstance: instance.into(),
                lpszClassName: w!("FramesMenuSourceFixture"),
                ..Default::default()
            });
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                w!("FramesMenuSourceFixture"),
                w!("Frames native menu source fixture"),
                WS_OVERLAPPEDWINDOW,
                100,
                100,
                400,
                300,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .unwrap();
            let root = CreateMenu().unwrap();
            let file = CreatePopupMenu().unwrap();
            AppendMenuW(file, MF_STRING, 100, w!("Open")).unwrap();
            AppendMenuW(file, MF_STRING | MF_GRAYED, 101, w!("Disabled")).unwrap();
            AppendMenuW(root, MF_POPUP, file.0 as usize, w!("File")).unwrap();
            SetMenu(hwnd, Some(root)).unwrap();
            let key = WindowId {
                id: 1,
                generation: 1,
            };
            let projection = Projection::default();
            let items = projection.read(key, hwnd).unwrap();
            assert_eq!(items[0].title, "File");
            assert_eq!(items[0].children[0].title, "Open");
            assert!(!items[0].children[1].enabled);
            assert!(
                projection
                    .invoke(key, hwnd, &items[0].children[1].id)
                    .is_err()
            );
            projection
                .invoke(key, hwnd, &items[0].children[0].id)
                .unwrap();
            let mut message = MSG::default();
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                DispatchMessageW(&message);
            }
            assert_eq!(COMMAND.load(Ordering::SeqCst), 100);
            SetMenuInfo(
                file,
                &MENUINFO {
                    cbSize: std::mem::size_of::<MENUINFO>() as u32,
                    fMask: MIM_STYLE,
                    dwStyle: MNS_NOTIFYBYPOS,
                    ..Default::default()
                },
            )
            .unwrap();
            projection
                .invoke(key, hwnd, &items[0].children[0].id)
                .unwrap();
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                DispatchMessageW(&message);
            }
            assert_eq!(COMMAND.load(Ordering::SeqCst), 1000);
            ModifyMenuW(
                file,
                0,
                MF_BYPOSITION | MF_STRING,
                102,
                w!("Different command"),
            )
            .unwrap();
            assert!(
                projection
                    .invoke(key, hwnd, &items[0].children[0].id)
                    .is_err()
            );
            DestroyWindow(hwnd).unwrap();
        }
    }
}
