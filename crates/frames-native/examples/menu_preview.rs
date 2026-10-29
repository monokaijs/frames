//! Interactive native-menu fixture. No remote connection, capture, input injection, or device access.
#[cfg(any(windows, target_os = "macos"))]
fn main() -> anyhow::Result<()> {
    use frames_core::{media::RawFrame, model::*};
    use frames_native::{ClientEvent, ClientUpdate};
    let (tx, rx) = std::sync::mpsc::sync_channel(16);
    let (events, mut actions) = tokio::sync::mpsc::unbounded_channel();
    let worker = std::thread::spawn(move || {
        let menus = || {
            vec![MenuItem {
                id: "file".into(),
                title: "File".into(),
                enabled: true,
                checked: false,
                children: vec![
                    MenuItem {
                        id: "preview.open".into(),
                        title: "Open fixture".into(),
                        enabled: true,
                        checked: false,
                        children: vec![],
                    },
                    MenuItem {
                        id: "preview.disabled".into(),
                        title: "Unavailable action".into(),
                        enabled: false,
                        checked: false,
                        children: vec![],
                    },
                ],
            }]
        };
        let surface = |mode| RemoteWindow {
            key: WindowId {
                id: if mode == ViewMode::Desktop { 2 } else { 1 },
                generation: 1,
            },
            app_id: "app.frames.menu-preview".into(),
            app_name: "Menu preview".into(),
            title: if mode == ViewMode::Desktop {
                "Frames desktop preview".into()
            } else {
                "Frames menu preview".into()
            },
            icon_png: None,
            parent: None,
            role: if mode == ViewMode::Desktop {
                WindowRole::Desktop
            } else {
                WindowRole::Window
            },
            bounds: Bounds {
                x: 0.,
                y: 0.,
                width: 640.,
                height: 400.,
            },
            scale: 1.,
            resize: ResizeCapability::Fixed,
            visible: true,
            minimized: false,
            modal: false,
        };
        let frame = || {
            let mut bgra = vec![0; 640 * 400 * 4];
            for (y, row) in bgra.chunks_exact_mut(640 * 4).enumerate() {
                for (x, p) in row.chunks_exact_mut(4).enumerate() {
                    p.copy_from_slice(&[90 + (x / 8) as u8, 60 + (y / 8) as u8, 35, 255]);
                }
            }
            RawFrame {
                width: 640,
                height: 400,
                bgra,
            }
        };
        let _ = tx.send(ClientUpdate::Peer {
            platform: "windows".into(),
            device_id: "preview".into(),
            name: "Native menu fixture".into(),
        });
        let mut mode = ViewMode::Windows;
        let mut window = surface(mode);
        let _ = tx.send(ClientUpdate::Windows(vec![window.clone()]));
        let _ = tx.send(ClientUpdate::Frame(window.key, frame()));
        let _ = tx.send(ClientUpdate::Menu {
            window: window.key,
            items: menus(),
        });
        let duration = std::env::var("FRAMES_PREVIEW_SECONDS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(180);
        let end = std::time::Instant::now() + std::time::Duration::from_secs(duration);
        while std::time::Instant::now() < end {
            match actions.try_recv() {
                Ok(ClientEvent::Disconnected) => break,
                Ok(ClientEvent::ViewMode(next)) => {
                    mode = next;
                    window = surface(mode);
                    let _ = tx.send(ClientUpdate::ViewMode { mode, error: None });
                    let _ = tx.send(ClientUpdate::Windows(vec![window.clone()]));
                    let _ = tx.send(ClientUpdate::Frame(window.key, frame()));
                    if mode == ViewMode::Windows {
                        let _ = tx.send(ClientUpdate::Menu {
                            window: window.key,
                            items: menus(),
                        });
                    }
                }
                Ok(ClientEvent::Action {
                    window,
                    action: WindowAction::Menu,
                }) => {
                    let _ = tx.send(ClientUpdate::Menu {
                        window,
                        items: menus(),
                    });
                }
                Ok(ClientEvent::Action {
                    window,
                    action: WindowAction::MenuItem { id },
                }) => {
                    let _ = tx.send(ClientUpdate::Status(
                        window,
                        format!("Menu action received: {id}"),
                    ));
                }
                Ok(ClientEvent::AdbBridge { .. }) => {
                    let _ = tx.send(ClientUpdate::AdbBridge {
                        enabled: false,
                        address: None,
                        error: Some(
                            "This preview does not access devices. Enable ADB in a paired session."
                                .into(),
                        ),
                    });
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
                _ => {}
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = tx.send(ClientUpdate::Shutdown);
    });
    let result = frames_native::run_client("menu-preview".into(), rx, events);
    let _ = worker.join();
    result
}
#[cfg(not(any(windows, target_os = "macos")))]
fn main() {
    eprintln!("Native menu preview requires Windows or macOS");
}
