//! Files on Windows, as v1 (`W/files.rs`, `W/drag.rs`): the viewer's
//! received-files shelf, whose rows drag out to other apps, and the host's
//! drag-drop of a received file into the shared window at the point it was
//! dropped on the viewer.

use std::path::{Path, PathBuf};
use std::sync::Once;
use std::time::Duration;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::{CoTaskMemFree, IDataObject};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::*;
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::{IShellFolder, SHBindToParent, SHParseDisplayName};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, HRESULT, PCWSTR, implement, w};

#[implement(IDropSource)]
struct DropSource;

impl IDropSource_Impl for DropSource_Impl {
    fn QueryContinueDrag(&self, escape: BOOL, keys: MODIFIERKEYS_FLAGS) -> HRESULT {
        if escape.as_bool() {
            DRAGDROP_S_CANCEL
        } else if keys.0 & MK_LBUTTON.0 == 0 {
            DRAGDROP_S_DROP
        } else {
            HRESULT(0)
        }
    }

    fn GiveFeedback(&self, _: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The Shell's data object for one file (copy semantics only), and the
/// PIDL to free after use.
unsafe fn shell_data(path: &Path) -> anyhow::Result<(IDataObject, *mut std::ffi::c_void)> {
    let name = wide(&path.to_string_lossy());
    let mut pidl = std::ptr::null_mut();
    unsafe {
        SHParseDisplayName(PCWSTR(name.as_ptr()), None, &mut pidl, 0, None)?;
        let mut last = std::ptr::null_mut();
        let data = SHBindToParent::<IShellFolder>(pidl, Some(&mut last))
            .and_then(|folder| folder.GetUIObjectOf::<IDataObject>(HWND::default(), &[last], None));
        match data {
            Ok(data) => Ok((data, pidl.cast())),
            Err(error) => {
                CoTaskMemFree(Some(pidl.cast()));
                Err(error.into())
            }
        }
    }
}

/// Viewer: drags `path` out of the shelf (the button is down).
fn drag_out(path: &Path) -> anyhow::Result<()> {
    unsafe {
        OleInitialize(None)?;
        let result = shell_data(path).and_then(|(data, pidl)| {
            let source: IDropSource = DropSource.into();
            let mut effect = DROPEFFECT_NONE;
            let dragged = DoDragDrop(&data, &source, DROPEFFECT_COPY, &mut effect).ok();
            CoTaskMemFree(Some(pidl));
            dragged.map_err(Into::into)
        });
        OleUninitialize();
        result
    }
}

/// Host: drags `path` into `target` at `point` (screen pixels), as if the
/// user dropped it there: the window comes forward and the real pointer
/// does the drop. Errors say why it was not taken.
pub fn drop_file(path: &Path, target: HWND, point: POINT) -> anyhow::Result<()> {
    let path = path.to_path_buf();
    let target = target.0 as usize;
    std::thread::spawn(move || -> anyhow::Result<()> {
        unsafe {
            OleInitialize(None)?;
            let result = shell_data(&path).and_then(|(data, pidl)| {
                let result = (|| -> anyhow::Result<()> {
                    let _ = SetForegroundWindow(HWND(target as *mut _));
                    // A tiny source window under the pointer starts the drag.
                    let source = CreateWindowExW(
                        WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                        w!("STATIC"),
                        w!("Frames file drop"),
                        WS_POPUP,
                        point.x - 1,
                        point.y - 1,
                        3,
                        3,
                        None,
                        None,
                        None,
                        None,
                    )?;
                    let _ = ShowWindow(source, SW_SHOWNOACTIVATE);
                    SetCursorPos(point.x, point.y)?;
                    let button = |flags| INPUT {
                        r#type: INPUT_MOUSE,
                        Anonymous: INPUT_0 {
                            mi: MOUSEINPUT {
                                dwFlags: flags,
                                ..Default::default()
                            },
                        },
                    };
                    let size = std::mem::size_of::<INPUT>() as i32;
                    let _ = SendInput(&[button(MOUSEEVENTF_LEFTDOWN)], size);
                    let raw = source.0 as usize;
                    let release = std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_millis(60));
                        let _ = ShowWindow(HWND(raw as *mut _), SW_HIDE);
                        let _ = SetCursorPos(point.x + 1, point.y);
                        std::thread::sleep(Duration::from_millis(100));
                        let _ = SendInput(&[button(MOUSEEVENTF_LEFTUP)], size);
                    });
                    let drop_source: IDropSource = DropSource.into();
                    let mut effect = DROPEFFECT_NONE;
                    let dragged = DoDragDrop(&data, &drop_source, DROPEFFECT_COPY, &mut effect);
                    let _ = release.join();
                    let _ = DestroyWindow(source);
                    dragged.ok()?;
                    anyhow::ensure!(
                        effect == DROPEFFECT_COPY,
                        "application did not accept the file drop; file remains in Frames receive folder"
                    );
                    Ok(())
                })();
                CoTaskMemFree(Some(pidl));
                result
            });
            OleUninitialize();
            result
        }
    })
    .join()
    .map_err(|_| anyhow::anyhow!("the file drop failed"))?
}

const CANCEL: usize = 1;
const ROWS_TOP: i32 = 50;
const ROW_HEIGHT: i32 = 32;
const TIMER: usize = 1;

thread_local! {
    /// The shelf's rows (received files) and what it last showed.
    static SHELF: std::cell::RefCell<(Vec<PathBuf>, u64, usize)> =
        const { std::cell::RefCell::new((Vec::new(), 0, 0)) };
}

/// Viewer: starts the received-files shelf (once per process) on its own
/// thread. It appears when a fetched file arrives, shows transfers in its
/// title, and its rows drag out to other apps.
pub fn start_shelf() {
    static STARTED: Once = Once::new();
    STARTED.call_once(|| {
        std::thread::spawn(|| {
            if let Err(error) = run_shelf() {
                println!("received-files window unavailable: {error:#}");
            }
        });
    });
}

fn run_shelf() -> anyhow::Result<()> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(shelf_proc),
            hInstance: instance.into(),
            lpszClassName: w!("FramesNextFileShelf"),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hbrBackground: HBRUSH((COLOR_WINDOW.0 + 1) as *mut _),
            ..Default::default()
        });
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            w!("FramesNextFileShelf"),
            w!("Frames received files — drag a file to another app"),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            600,
            320,
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("BUTTON"),
            w!("Cancel transfers"),
            WS_CHILD | WS_VISIBLE,
            12,
            8,
            160,
            28,
            Some(hwnd),
            Some(HMENU(CANCEL as *mut _)),
            Some(instance.into()),
            None,
        )?;
        SetTimer(Some(hwnd), TIMER, 300, None);
        let mut message = MSG::default();
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        Ok(())
    }
}

/// Follows the connection menu's transfers: new files show the shelf.
unsafe fn refresh(hwnd: HWND) {
    let version = crate::control::version();
    let stale = SHELF.with(|s| s.borrow().1 != version);
    if !stale {
        return;
    }
    let snapshot = crate::control::snapshot();
    let (grew, rows) = SHELF.with(|s| {
        let mut s = s.borrow_mut();
        s.1 = version;
        s.0 = snapshot.received.iter().map(PathBuf::from).collect();
        let grew = s.0.len() > s.2;
        s.2 = s.0.len();
        (grew, s.0.len())
    });
    let title = match snapshot.transfers.first() {
        Some(name) if snapshot.transfers.len() == 1 => format!("Frames transfers — {name}"),
        Some(_) => format!("Frames transfers — {} files", snapshot.transfers.len()),
        None => "Frames received files — drag a file to another app".into(),
    };
    let title = wide(&title);
    unsafe {
        let _ = SetWindowTextW(hwnd, PCWSTR(title.as_ptr()));
        if grew || (!snapshot.transfers.is_empty() && rows == 0) {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        let _ = InvalidateRect(Some(hwnd), None, true);
    }
}

unsafe extern "system" fn shelf_proc(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        match message {
            WM_TIMER if w.0 == TIMER => {
                refresh(hwnd);
                return LRESULT(0);
            }
            WM_COMMAND if w.0 & 0xffff == CANCEL => {
                crate::control::send(crate::control::Command::CancelTransfers);
                return LRESULT(0);
            }
            // Closing only hides it (as v1).
            WM_CLOSE => {
                let _ = ShowWindow(hwnd, SW_HIDE);
                return LRESULT(0);
            }
            WM_PAINT => {
                let mut paint = PAINTSTRUCT::default();
                let dc = BeginPaint(hwnd, &mut paint);
                SHELF.with(|s| {
                    for (index, path) in s.borrow().0.iter().enumerate() {
                        let name: Vec<u16> = path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .encode_utf16()
                            .collect();
                        let _ = TextOutW(dc, 14, ROWS_TOP + index as i32 * ROW_HEIGHT, &name);
                    }
                });
                let _ = EndPaint(hwnd, &paint);
                return LRESULT(0);
            }
            WM_LBUTTONDOWN => {
                let y = (l.0 >> 16) as u16 as i16 as i32;
                let row = (y - ROWS_TOP).div_euclid(ROW_HEIGHT);
                let path = SHELF.with(|s| {
                    (row >= 0)
                        .then(|| s.borrow().0.get(row as usize).cloned())
                        .flatten()
                });
                if let Some(path) = path
                    && let Err(error) = drag_out(&path)
                {
                    println!("dragging {} failed: {error:#}", path.display());
                }
                return LRESULT(0);
            }
            _ => {}
        }
        DefWindowProcW(hwnd, message, w, l)
    }
}
