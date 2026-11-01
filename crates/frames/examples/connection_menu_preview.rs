//! Synthetic native connection windows for menu/visibility UI regression checks.
#![cfg_attr(windows, windows_subsystem = "windows")]
use frames_core::{adaptive::QualityPreset, media::RawFrame, model::*};
use frames_native::{ClientEvent, ClientUpdate};
fn main() -> anyhow::Result<()> {
    let (ui, updates) = frames_native::client_channel(32);
    let (events, mut incoming) = tokio::sync::mpsc::unbounded_channel();
    ui.send(ClientUpdate::Peer {
        platform: "fixture".into(),
        device_id: "menu-fixture".into(),
        name: "Frames QA connection".into(),
    })?;
    ui.send(ClientUpdate::Quality(QualityPreset::High))?;
    let windows: Vec<_> = (1..=2)
        .map(|id| RemoteWindow {
            key: WindowId { id, generation: 1 },
            app_id: "frames.qa.menu".into(),
            app_name: "Frames QA menu".into(),
            title: format!("Frames QA menu {id}"),
            icon_png: None,
            parent: None,
            role: WindowRole::Window,
            bounds: Bounds {
                width: 640.,
                height: 400.,
                ..Default::default()
            },
            scale: 1.,
            resize: ResizeCapability::Fixed,
            minimized: false,
            visible: true,
            modal: false,
            show_header: false,
        })
        .collect();
    ui.send(ClientUpdate::Windows(windows.clone()))?;
    let worker = std::thread::spawn(move || {
        for window in &windows {
            let _ = ui.send(ClientUpdate::Frame(
                window.key,
                RawFrame {
                    width: 640,
                    height: 400,
                    bgra: vec![45; 640 * 400 * 4].into(),
                    captured_at: std::time::Instant::now(),
                    media_generation: 1,
                    frame_number: 0,
                    #[cfg(windows)]
                    gpu: None,
                },
            ));
        }
        while let Some(event) = incoming.blocking_recv() {
            eprintln!("fixture event: {event:?}");
            match event {
                ClientEvent::Quality(preset) => {
                    let _ = ui.send(ClientUpdate::Quality(preset));
                }
                ClientEvent::Disconnected => {
                    let _ = ui.send(ClientUpdate::Shutdown);
                    break;
                }
                _ => {}
            }
        }
    });
    frames_native::run_client("Frames QA connection".into(), updates, events)?;
    let _ = worker.join();
    Ok(())
}
