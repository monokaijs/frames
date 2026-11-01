use super::{Action, Event, Launcher, ShareWindowRow, Worker, model};
use slint::{ComponentHandle, Model};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
    mpsc,
};

pub fn bind(ui: &Launcher, actions: mpsc::Sender<Action>) {
    ui.on_filter_share_windows(|rows, query| {
        let query = query.to_lowercase();
        model(
            rows.iter()
                .filter(|row| {
                    format!("{} {}", row.title, row.app)
                        .to_lowercase()
                        .contains(&query)
                })
                .collect(),
        )
    });
    let weak = ui.as_weak();
    let tx = actions.clone();
    ui.on_load_share_windows(move || {
        if let Some(ui) = weak.upgrade() {
            let request = ui.get_window_picker_generation().wrapping_add(1).max(1);
            ui.set_window_picker_generation(request);
            ui.set_window_picker_loading(true);
            ui.set_window_picker_error("".into());
            ui.set_share_selected_count(0);
            ui.set_share_windows(model(vec![]));
            let _ = tx.send(Action::LoadShareWindows(request as u64));
        }
    });
    let weak = ui.as_weak();
    let tx = actions.clone();
    ui.on_cancel_share_windows(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_window_picker_open(false);
            ui.set_share_windows(model(vec![]));
            ui.set_window_picker_generation(
                ui.get_window_picker_generation().wrapping_add(1).max(1),
            );
            let _ = tx.send(Action::CancelWindowPicker);
        }
    });
    let weak = ui.as_weak();
    ui.on_toggle_share_window(move |id| {
        if let Some(ui) = weak.upgrade() {
            let mut rows: Vec<_> = ui.get_share_windows().iter().collect();
            let current = rows.iter().filter(|r| r.selected).count();
            let Some(index) = rows.iter().position(|row| row.id == id && row.available) else {
                return;
            };
            let selected = !rows[index].selected;
            if ui.get_share_mode() == 2 {
                for row in &mut rows {
                    row.selected = false;
                }
            } else if selected && current >= 32 {
                ui.set_window_picker_error("Choose up to 32 windows.".into());
                return;
            }
            rows[index].selected = selected;
            ui.set_share_selected_count(rows.iter().filter(|r| r.selected).count() as i32);
            ui.set_share_windows(model(rows));
        }
    });
    let weak = ui.as_weak();
    ui.on_confirm_share_windows(move || {
        if let Some(ui) = weak.upgrade() {
            let selected: Vec<_> = ui
                .get_share_windows()
                .iter()
                .filter(|row| row.selected && row.available)
                .map(|row| row.id.to_string())
                .collect();
            if selected.is_empty() || (ui.get_share_mode() == 2 && selected.len() != 1) {
                return;
            }
            ui.set_window_picker_open(false);
            let _ = actions.send(Action::ShareWindows(ui.get_share_mode(), selected));
        }
    });
}
pub fn load(worker: &mut Worker, request: u64) {
    if worker.sessions.iter().any(|s| s.host) {
        let _ = worker.events.send(Event::ShareWindows(
            request,
            Err("Stop sharing before choosing other windows.".into()),
        ));
        return;
    }
    worker.sharing_generation.store(request, Ordering::Release);
    let result = (|| -> anyhow::Result<_> {
        let source = frames_native::desktop(vec![])?;
        let windows = frames_native::sharing_windows(source.as_ref())?;
        Ok((source, windows))
    })();
    let (source, windows) = match result {
        Ok(value) => value,
        Err(error) => {
            let _ = worker
                .events
                .send(Event::ShareWindows(request, Err(format!("{error:#}"))));
            return;
        }
    };
    worker.sharing_choices = windows.clone();
    let _ = worker
        .events
        .send(Event::ShareWindows(request, Ok(windows.clone())));
    let queue = Arc::new(Mutex::new(std::collections::VecDeque::from(windows)));
    for _ in 0..2 {
        let (source, queue, events, generation) = (
            source.clone(),
            queue.clone(),
            worker.events.clone(),
            worker.sharing_generation.clone(),
        );
        std::thread::spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            while generation.load(Ordering::Acquire) == request {
                let Some(choice) = queue.lock().unwrap().pop_front() else {
                    break;
                };
                let id = choice
                    .selection
                    .as_ref()
                    .map_or_else(|| choice.window.key.id.to_string(), |s| s.argument());
                let bytes = if choice.window.minimized {
                    None
                } else {
                    runtime
                        .block_on(crate::device_preview::capture(
                            source.clone(),
                            Some(choice.window.key),
                        ))
                        .ok()
                };
                if generation.load(Ordering::Acquire) != request {
                    break;
                }
                let _ = events.send(Event::ShareWindowPreview(request, id, bytes));
            }
        });
    }
}
pub fn apply_windows(
    ui: &Launcher,
    request: u64,
    result: Result<Vec<frames_native::SharingWindow>, String>,
) {
    if !ui.get_window_picker_open() || ui.get_window_picker_generation() as u64 != request {
        return;
    }
    ui.set_window_picker_loading(false);
    match result {
        Ok(mut windows) => {
            windows.sort_by_key(|c| {
                (
                    c.window.app_name.to_lowercase(),
                    c.window.title.to_lowercase(),
                )
            });
            ui.set_share_windows(model(
                windows
                    .into_iter()
                    .map(|choice| ShareWindowRow {
                        id: choice
                            .selection
                            .as_ref()
                            .map_or_else(|| choice.window.key.id.to_string(), |s| s.argument())
                            .into(),
                        title: choice.window.title.into(),
                        app: choice.window.app_name.into(),
                        available: choice.selection.is_some(),
                        detail: if choice.window.minimized {
                            "Minimized"
                        } else if choice.selection.is_none() {
                            "Unavailable for control"
                        } else {
                            "Loading preview..."
                        }
                        .into(),
                        ..Default::default()
                    })
                    .collect(),
            ));
        }
        Err(error) => ui.set_window_picker_error(error.into()),
    }
}
pub fn apply_preview(ui: &Launcher, request: u64, id: String, bytes: Option<Vec<u8>>) {
    if !ui.get_window_picker_open() || ui.get_window_picker_generation() as u64 != request {
        return;
    }
    let mut rows: Vec<_> = ui.get_share_windows().iter().collect();
    let Some(row) = rows.iter_mut().find(|row| row.id == id) else {
        return;
    };
    if let Some(image) = bytes.and_then(|bytes| crate::device_preview::decode(&bytes).ok()) {
        let image = image.thumbnail(320, 180).to_rgba8();
        row.preview = slint::Image::from_rgba8(
            slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                image.as_raw(),
                image.width(),
                image.height(),
            ),
        );
        row.detail = "".into();
    } else if row.detail != "Minimized" {
        row.detail = "Preview unavailable".into();
    }
    ui.set_share_windows(model(rows));
}
pub fn generation() -> Arc<AtomicU64> {
    Arc::new(AtomicU64::new(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn picker_preserves_scope_selection_and_ignores_stale_previews() {
        struct Headless;
        impl slint::platform::Platform for Headless {
            fn create_window_adapter(
                &self,
            ) -> Result<std::rc::Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError>
            {
                Ok(
                    slint::platform::software_renderer::MinimalSoftwareWindow::new(
                        Default::default(),
                    ),
                )
            }
        }
        slint::platform::set_platform(Box::new(Headless)).unwrap();
        let ui = Launcher::new().unwrap();
        let (actions, received) = mpsc::channel();
        bind(&ui, actions);
        assert_eq!(ui.get_share_mode(), 0);
        ui.set_share_mode(2);
        ui.set_window_picker_open(true);
        ui.invoke_load_share_windows();
        let request = match received.recv().unwrap() {
            Action::LoadShareWindows(request) => request,
            _ => panic!("picker did not request windows"),
        };
        ui.set_share_windows(model(vec![
            ShareWindowRow {
                id: "one".into(),
                title: "Game".into(),
                available: true,
                ..Default::default()
            },
            ShareWindowRow {
                id: "two".into(),
                title: "Notes".into(),
                available: true,
                ..Default::default()
            },
        ]));
        ui.invoke_toggle_share_window("one".into());
        ui.invoke_toggle_share_window("two".into());
        assert_eq!(ui.get_share_selected_count(), 1);
        assert!(!ui.get_share_windows().row_data(0).unwrap().selected);
        ui.set_share_mode(1);
        ui.invoke_toggle_share_window("one".into());
        assert_eq!(ui.get_share_selected_count(), 2);
        ui.invoke_confirm_share_windows();
        assert!(
            matches!(received.recv().unwrap(),Action::ShareWindows(1,windows) if windows==["one","two"])
        );
        ui.set_window_picker_open(true);
        ui.invoke_load_share_windows();
        let _ = received.recv().unwrap();
        ui.set_share_windows(model(vec![ShareWindowRow {
            id: "one".into(),
            title: "Current".into(),
            available: true,
            ..Default::default()
        }]));
        apply_preview(&ui, request, "one".into(), None);
        assert_eq!(ui.get_share_windows().row_data(0).unwrap().detail, "");
        ui.invoke_cancel_share_windows();
        assert!(!ui.get_window_picker_open());
        assert_eq!(ui.get_share_windows().row_count(), 0);
        assert!(matches!(
            received.recv().unwrap(),
            Action::CancelWindowPicker
        ));
    }
}
