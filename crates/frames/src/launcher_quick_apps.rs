//! Quick Apps stay off the Slint thread; only decoded icons enter the view.
use super::{Action, Launcher, QuickAppRow, device_catalog, model};
use base64::Engine as _;
use frames_next::directory::{self, QuickApp, RemoteQuickApp};
use slint::ComponentHandle;
use std::{cell::Cell, rc::Rc, sync::mpsc, time::Duration};

pub(super) enum Reply {
    Local(Result<Vec<QuickApp>, String>, bool),
    Remote(u64, String, Result<Vec<RemoteQuickApp>, String>),
}
enum LocalJob {
    Load,
    Add(String, String),
    Remove(String),
}

fn edit(job: LocalJob) -> anyhow::Result<Vec<QuickApp>> {
    let mut apps = directory::quick_apps()?;
    match job {
        LocalJob::Load => return Ok(apps),
        LocalJob::Add(name, command) => {
            let command = command.trim().trim_matches('"');
            let path = std::path::Path::new(command);
            anyhow::ensure!(
                path.is_absolute() && path.exists(),
                "Choose the full path of an application on this computer."
            );
            anyhow::ensure!(!name.trim().is_empty(), "Enter an application name.");
            apps.push(QuickApp {
                id: uuid::Uuid::new_v4().simple().to_string(),
                name: name.trim().into(),
                command: command.into(),
                args: vec![],
                path: command.into(),
            });
        }
        LocalJob::Remove(id) => apps.retain(|app| app.id != id),
    }
    directory::save_quick_apps(&apps)?;
    Ok(apps)
}

pub(super) fn fetch(request: u64, original: String, target: String, reply: mpsc::Sender<Reply>) {
    std::thread::spawn(move || {
        let result = (|| -> anyhow::Result<_> {
            let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
            runtime.block_on(directory::remote_quick_apps(&target, Duration::from_secs(15)))
        })().map_err(|error| format!("Couldn't load Quick Apps: {error}. Connect and trust this computer first, then try again."));
        let _ = reply.send(Reply::Remote(request, original, result));
    });
}

pub(super) fn bind(ui: &Launcher, actions: mpsc::Sender<Action>) -> slint::Timer {
    let (reply, replies) = mpsc::channel();
    let (jobs, incoming) = mpsc::channel();
    let local_reply = reply.clone();
    std::thread::spawn(move || {
        for job in incoming {
            let close_editor = matches!(job, LocalJob::Add(..));
            let result = edit(job).map_err(|error| format!("{error:#}"));
            if local_reply
                .send(Reply::Local(result, close_editor))
                .is_err()
            {
                break;
            }
        }
    });
    let _ = jobs.send(LocalJob::Load);
    let generation = Rc::new(Cell::new(0u64));
    let request = generation.clone();
    let tx = actions.clone();
    let results = reply;
    let weak = ui.as_weak();
    ui.on_load_quick_apps(move |target| {
        if let Some(ui) = weak.upgrade() {
            request.set(request.get().wrapping_add(1));
            ui.set_quick_apps_busy(true);
            ui.set_quick_app_error("".into());
            ui.set_quick_app_selected("".into());
            ui.set_remote_quick_apps(model(vec![]));
            if tx
                .send(Action::LoadQuickApps(
                    target.to_string(),
                    request.get(),
                    results.clone(),
                ))
                .is_err()
            {
                ui.set_quick_apps_busy(false);
                ui.set_quick_app_error("The connection worker is unavailable.".into());
            }
        }
    });
    let weak = ui.as_weak();
    ui.on_launch_quick_app(move |target, app, only| {
        if let Some(ui) = weak.upgrade() {
            match actions.send(Action::Connect(
                target.to_string(),
                String::new(),
                ui.get_audio(),
                false,
                Some((app.to_string(), only)),
            )) {
                Ok(()) => ui.set_quick_apps_open(false),
                Err(_) => ui.set_quick_app_error("The connection worker is unavailable.".into()),
            }
        }
    });
    let weak = ui.as_weak();
    let save = jobs.clone();
    ui.on_save_quick_app(move |name, path| {
        if let Some(ui) = weak.upgrade() {
            if ui.get_quick_app_saving() {
                return;
            }
            ui.set_quick_app_saving(true);
            ui.set_quick_app_editor_error("".into());
            let _ = save.send(LocalJob::Add(name.to_string(), path.to_string()));
        }
    });
    let weak = ui.as_weak();
    ui.on_remove_quick_app(move |id| {
        if let Some(ui) = weak.upgrade() {
            if ui.get_quick_app_saving() {
                return;
            }
            ui.set_quick_app_saving(true);
            let _ = jobs.send(LocalJob::Remove(id.to_string()));
        }
    });
    let weak = ui.as_weak();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(100),
        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            for reply in replies.try_iter() {
                match reply {
                    Reply::Local(result, close_editor) => {
                        ui.set_quick_app_saving(false);
                        match result {
                            Ok(apps) => {
                                ui.set_local_quick_apps(model(
                                    apps.into_iter()
                                        .map(|app| QuickAppRow {
                                            id: app.id.into(),
                                            name: app.name.into(),
                                            command: app.command.into(),
                                            ..Default::default()
                                        })
                                        .collect(),
                                ));
                                if close_editor {
                                    ui.set_quick_app_editor_open(false);
                                }
                            }
                            Err(error) if ui.get_quick_app_editor_open() => {
                                ui.set_quick_app_editor_error(error.into())
                            }
                            Err(error) => {
                                ui.set_settings_status(
                                    format!("Couldn't update Quick Apps: {error}").into(),
                                );
                                ui.set_settings_error(true);
                            }
                        }
                    }
                    Reply::Remote(request, target, result) => {
                        if request != generation.get()
                            || !ui.get_quick_apps_open()
                            || ui.get_quick_app_target().as_str() != target
                        {
                            continue;
                        }
                        ui.set_quick_apps_busy(false);
                        match result {
                            Ok(apps) => ui.set_remote_quick_apps(model(
                                apps.into_iter()
                                    .map(|app| {
                                        let icon = app
                                            .icon
                                            .filter(|icon| icon.len() <= 128 * 1024)
                                            .and_then(|icon| {
                                                base64::engine::general_purpose::STANDARD
                                                    .decode(icon)
                                                    .ok()
                                            })
                                            .map(|bytes| device_catalog::image(&bytes))
                                            .unwrap_or_default();
                                        QuickAppRow {
                                            id: app.id.into(),
                                            name: app.name.into(),
                                            icon,
                                            ..Default::default()
                                        }
                                    })
                                    .collect(),
                            )),
                            Err(error) => ui.set_quick_app_error(error.into()),
                        }
                    }
                }
            }
        },
    );
    timer
}
