//! A host's per-session services fed by the viewer's tagged streams:
//! input events, clipboard, incoming files, and file fetches.

use crate::streams::{Routes, Streams, route};
use crate::{Options, clip, transfer};
use frames_input::{Event, EventReader};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub struct HostServices {
    /// Decoded input events, for the streaming thread to apply.
    pub events: std::sync::mpsc::Receiver<Event>,
    /// The clipboard stream, once the viewer opens one.
    pub clipboard: std::sync::mpsc::Receiver<Streams>,
    /// Input events received (applied or not); the macOS host reports it.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub event_count: Arc<AtomicU64>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl HostServices {
    /// Starts routing the viewer's streams. Files are accepted only from a
    /// device trusted for control (they are written to this disk).
    pub fn start(session: &frames_net::Session, o: &Options) -> Self {
        let (events_tx, events) = std::sync::mpsc::channel();
        let event_count = Arc::new(AtomicU64::new(0));
        let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel::<Streams>();
        let (clipboard_tx, clipboard) = std::sync::mpsc::channel();
        let (file_tx, mut file_rx) = tokio::sync::mpsc::unbounded_channel::<Streams>();
        let (fetch_tx, mut fetch_rx) = tokio::sync::mpsc::unbounded_channel::<Streams>();
        let (devices_tx, mut devices_rx) = tokio::sync::mpsc::unbounded_channel::<Streams>();
        // Sharing only selected windows shares nothing else (as v1): no
        // clipboard, files or devices.
        let whole = o.windows.is_empty();
        let routes = Routes {
            input: Some(input_tx),
            clipboard: (whole && o.clipboard != clip::Mode::Off).then_some(clipboard_tx),
            file: (whole && o.control).then_some(file_tx),
            // Reading this user's files also needs control trust.
            fetch: (whole && o.control).then_some(fetch_tx),
            // Virtual controllers are input: control trust only.
            devices: (whole && o.control).then_some(devices_tx),
        };
        let mut tasks = vec![tokio::spawn(route(session.clone(), routes))];
        let count = event_count.clone();
        tasks.push(tokio::spawn(async move {
            while let Some((_send, mut recv)) = input_rx.recv().await {
                let mut decoder = EventReader::default();
                let mut buffer = [0u8; 1024];
                while let Ok(Some(n)) = recv.read(&mut buffer).await {
                    for event in decoder.push(&buffer[..n]) {
                        count.fetch_add(1, Ordering::Relaxed);
                        if events_tx.send(event).is_err() {
                            return;
                        }
                    }
                }
            }
        }));
        let dir = o
            .receive_dir
            .clone()
            .unwrap_or_else(transfer::default_receive_dir);
        let (window, qa) = (o.window_id, o.qa_source);
        tasks.push(tokio::spawn(async move {
            while let Some(streams) = file_rx.recv().await {
                let dir = dir.clone();
                tokio::spawn(async move {
                    let after = move |path: &std::path::Path, point| {
                        drop_into_application(window, qa, path, point)
                    };
                    match transfer::receive_then(streams, dir, after).await {
                        Ok(path) => println!("received {}", path.display()),
                        Err(error) => println!("file transfer failed: {error:#}"),
                    }
                });
            }
        }));
        let device_session = session.clone();
        tasks.push(tokio::spawn(async move {
            while let Some(streams) = devices_rx.recv().await {
                let session = device_session.clone();
                tokio::spawn(async move {
                    if let Err(error) = crate::pads::host(session, streams).await {
                        println!("gamepads stopped: {error:#}");
                    }
                });
            }
        }));
        tasks.push(tokio::spawn(async move {
            while let Some(streams) = fetch_rx.recv().await {
                tokio::spawn(async move {
                    match transfer::serve_fetch(streams).await {
                        Ok(saved) => println!("sent a file to the viewer ({saved})"),
                        Err(error) => println!("file fetch failed: {error:#}"),
                    }
                });
            }
        }));
        Self {
            events,
            clipboard,
            event_count,
            tasks,
        }
    }
}

/// A file dropped on the viewer's window, now saved here: dropped into the
/// shared window (or the screen) at the same place, as v1 did. The note
/// goes back to the viewer.
#[cfg(target_os = "macos")]
fn drop_into_application(
    window: Option<u64>,
    qa: bool,
    path: &std::path::Path,
    point: Option<(u16, u16)>,
) -> Option<String> {
    let (x, y) = point?;
    // A QA source has no application to take it.
    if qa {
        return None;
    }
    let (left, top, width, height) = match window {
        Some(id) => {
            crate::mac_host::captured_rect(id).or_else(|| crate::catalog::mac_bounds(id))?
        }
        None => frames_platform_macos::capture::main_display_bounds(),
    };
    let at = |offset: f64, extent: f64, v: u16| offset + extent * f64::from(v) / 65535.0;
    let id = window.and_then(|id| u32::try_from(id).ok()).unwrap_or(0);
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    if frames_platform_macos::capture::drop_file(id, path, at(left, width, x), at(top, height, y)) {
        println!("dropped {} into the application", path.display());
        Some(format!("Dropped {name} into the application"))
    } else {
        println!("the application did not take {}", path.display());
        Some(
            "application did not accept the file drop; file remains in Frames receive folder"
                .into(),
        )
    }
}

/// Host: drags a file the viewer dropped into the shared window at the
/// point it was dropped (v1's OLE drop); the note goes back to the viewer.
#[cfg(windows)]
fn drop_into_application(
    window: Option<u64>,
    qa: bool,
    path: &std::path::Path,
    point: Option<(u16, u16)>,
) -> Option<String> {
    use windows::Win32::Foundation::{HWND, POINT};
    let ((x, y), window) = (point?, window?);
    if qa {
        return None;
    }
    let hwnd = HWND(window as usize as *mut _);
    // The picture the viewer showed: the client area unless the header is
    // shown (then a drop near the top lands a title bar's height higher).
    let rect = frames_platform_windows::input::captured_rect(hwnd, false);
    let at = POINT {
        x: rect.left + frames_input::denormalize(x, (rect.right - rect.left).max(1) as u32),
        y: rect.top + frames_input::denormalize(y, (rect.bottom - rect.top).max(1) as u32),
    };
    match crate::win_files::drop_file(path, hwnd, at) {
        Ok(()) => Some(format!(
            "dropped {} into the window",
            path.file_name().unwrap_or_default().to_string_lossy()
        )),
        Err(error) => Some(error.to_string()),
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
fn drop_into_application(
    _window: Option<u64>,
    _qa: bool,
    _path: &std::path::Path,
    _point: Option<(u16, u16)>,
) -> Option<String> {
    None
}

impl Drop for HostServices {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}
