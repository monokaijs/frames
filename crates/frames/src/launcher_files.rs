//! The dual-pane file manager. Slint owns selection/view state; one worker
//! owns filesystem/network work so a slow host never blocks the window.
use super::{FileRow, FileSuggestion, FileTransferRow, Launcher, launcher_models, model};
use frames_next::file_sharing::{self as files, Client, Listing};
use slint::{ComponentHandle, Model};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

#[derive(Default)]
struct Pane {
    home: String,
    hidden: bool,
    suggestion_generation: u64,
    path: String,
    cache: BTreeMap<String, Listing>,
    expanded: BTreeSet<String>,
    selected: BTreeSet<String>,
    filter: String,
    mode: i32,
    generation: u64,
}

#[derive(Default)]
struct State {
    panes: [Pane; 2],
    peers: Vec<(String, String)>,
}

enum Job {
    Connect(String),
    List(usize, String, u64),
    Suggest(usize, String, String, bool, u64),
    Copy(usize, Vec<String>, String, String),
    Close,
}

enum Reply {
    Connected(Result<(), String>),
    Listing(usize, String, u64, Result<Listing, String>),
    Suggestions(usize, u64, Vec<FileSuggestion>),
    Progress(String, f32),
    Copied(Vec<FileTransferRow>),
}

fn absolute(home: &str, relative: &str) -> String {
    if relative.is_empty() {
        return home.to_string();
    }
    if home.contains('\\') {
        format!(
            "{}\\{}",
            home.trim_end_matches('\\'),
            relative.replace('/', "\\")
        )
    } else {
        format!("{}/{}", home.trim_end_matches('/'), relative)
    }
}

fn relative_address(input: &str, home: &str) -> anyhow::Result<String> {
    let input = input.trim().replace('\\', "/");
    let base = home.replace('\\', "/");
    let base = base.trim_end_matches('/');
    let windows = home.contains('\\') || home.as_bytes().get(1) == Some(&b':');
    let equal = |a: &str, b: &str| {
        if windows {
            a.eq_ignore_ascii_case(b)
        } else {
            a == b
        }
    };
    let relative = if input == "~" {
        ""
    } else if let Some(path) = input.strip_prefix("~/") {
        path
    } else if input.starts_with('/') || input.as_bytes().get(1) == Some(&b':') {
        anyhow::ensure!(!base.is_empty(), "Wait for the folder to load");
        if equal(&input, base) {
            ""
        } else {
            anyhow::ensure!(
                input.len() > base.len()
                    && input.is_char_boundary(base.len())
                    && equal(&input[..base.len()], base)
                    && input.as_bytes()[base.len()] == b'/',
                "Choose a folder inside {home}"
            );
            &input[base.len() + 1..]
        }
    } else {
        input.as_str()
    };
    anyhow::ensure!(
        !relative.split('/').any(|part| part == "..") && !relative.contains([':', '\0']),
        "Choose a folder inside {home}"
    );
    Ok(relative
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect::<Vec<_>>()
        .join("/"))
}

fn complete(listing: Listing, prefix: &str, hidden: bool) -> Vec<FileSuggestion> {
    let prefix = prefix.to_lowercase();
    let separator = if listing.root.contains('\\') {
        "\\"
    } else {
        "/"
    };
    listing
        .entries
        .into_iter()
        .filter(|entry| {
            entry.is_dir
                && (hidden || !entry.name.starts_with('.'))
                && entry.name.to_lowercase().starts_with(&prefix)
        })
        .take(12)
        .map(|entry| FileSuggestion {
            name: entry.name.into(),
            path: format!("{}{separator}", absolute(&listing.root, &entry.path)).into(),
        })
        .collect()
}

struct ActiveClient {
    client: Client,
    // Keep automatic downloads deferred until the connection and any
    // in-flight operation have released their last reference.
    _lease: Option<std::fs::File>,
}

impl std::ops::Deref for ActiveClient {
    type Target = Client;
    fn deref(&self) -> &Self::Target {
        &self.client
    }
}

type SharedClient = Arc<Mutex<Option<Arc<ActiveClient>>>>;

pub(super) struct Binding {
    _timer: slint::Timer,
    jobs: mpsc::Sender<Job>,
    client: SharedClient,
}
impl Drop for Binding {
    fn drop(&mut self) {
        if let Some(client) = self.client.lock().unwrap().take() {
            client.cancel();
        }
        let _ = self.jobs.send(Job::Close);
    }
}

fn rows(pane: &Pane, hidden: bool) -> Vec<FileRow> {
    fn append(pane: &Pane, path: &str, depth: i32, hidden: bool, rows: &mut Vec<FileRow>) {
        if depth > 32 || rows.len() > 20_000 {
            return;
        }
        let Some(listing) = pane.cache.get(path) else {
            return;
        };
        for entry in &listing.entries {
            if !hidden && entry.name.starts_with('.') {
                continue;
            }
            let matches =
                pane.filter.is_empty() || entry.name.to_lowercase().contains(&pane.filter);
            if matches {
                let (stem, extension) = name_parts(&entry.name, entry.is_dir);
                rows.push(FileRow {
                    path: entry.path.clone().into(),
                    name: entry.name.clone().into(),
                    stem: stem.into(),
                    extension: extension.into(),
                    size: if entry.is_dir {
                        "Folder".into()
                    } else {
                        size(entry.size).into()
                    },
                    modified: modified(entry.modified).into(),
                    detail: String::new().into(),
                    folder: entry.is_dir,
                    selected: pane.selected.contains(&entry.path),
                    depth,
                    expanded: pane.expanded.contains(&entry.path),
                });
            }
            if pane.mode == 2 && pane.expanded.contains(&entry.path) {
                append(pane, &entry.path, depth + 1, hidden, rows);
            }
        }
    }
    let mut result = Vec::new();
    append(pane, &pane.path, 0, hidden, &mut result);
    result
}

fn name_parts(name: &str, folder: bool) -> (String, String) {
    if !folder
        && let Some((stem, extension)) = name.rsplit_once('.')
        && !stem.is_empty()
        && !extension.is_empty()
        && extension.chars().count() <= 16
    {
        return (stem.into(), format!(".{extension}"));
    }
    (name.into(), String::new())
}

fn size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024. * 1024.))
    } else {
        format!("{:.1} GB", bytes as f64 / (1024. * 1024. * 1024.))
    }
}

fn modified(seconds: u64) -> String {
    // Stable UTC date, without depending on the host's locale or timezone.
    let days = (seconds / 86_400) as i64 + 719_468;
    let era = days / 146_097;
    let doe = days - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

fn display(ui: &Launcher, state: &State, side: usize) {
    let pane = &state.panes[side];
    let data = rows(pane, pane.hidden);
    let current = if side == 0 {
        ui.get_files_local_entries()
    } else {
        ui.get_files_remote_entries()
    };
    launcher_models::refresh_rows(
        current,
        data,
        |row| row.path.clone(),
        |rows| {
            if side == 0 {
                ui.set_files_local_entries(rows)
            } else {
                ui.set_files_remote_entries(rows)
            }
        },
    );
    if side == 0 {
        ui.set_files_local_path(absolute(&pane.home, &pane.path).into());
        ui.set_files_local_mode(pane.mode);
        ui.set_files_local_can_up(!pane.path.is_empty());
        ui.set_files_local_selected(pane.selected.len() as i32);
    } else {
        ui.set_files_remote_path(absolute(&pane.home, &pane.path).into());
        ui.set_files_remote_mode(pane.mode);
        ui.set_files_remote_can_up(!pane.path.is_empty());
        ui.set_files_remote_selected(pane.selected.len() as i32);
    }
}

fn load(
    ui: &Launcher,
    state: &mut State,
    tx: &mpsc::Sender<Job>,
    side: usize,
    path: String,
    navigate: bool,
) {
    if side > 1 || ui.get_files_transferring() || (side == 1 && !ui.get_files_connected()) {
        return;
    }
    let pane = &mut state.panes[side];
    if navigate {
        pane.suggestion_generation = pane.suggestion_generation.wrapping_add(1);
        if side == 0 {
            ui.set_files_local_suggestions(model(vec![]));
        } else {
            ui.set_files_remote_suggestions(model(vec![]));
        }
        pane.path = path.clone();
        pane.selected.clear();
        pane.expanded.clear();
        pane.cache.clear();
    }
    pane.generation = pane.generation.wrapping_add(1);
    let _ = tx.send(Job::List(side, path, pane.generation));
    if side == 0 {
        ui.set_files_local_busy(true);
        ui.set_files_local_error("".into());
    } else {
        ui.set_files_remote_busy(true);
        ui.set_files_remote_error("".into());
    }
    display(ui, state, side);
}

fn client(shared: &SharedClient) -> anyhow::Result<Arc<ActiveClient>> {
    shared
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| anyhow::anyhow!("Connect to a remote device first"))
}

fn selections(paths: &[String]) -> Vec<String> {
    // Selecting a folder and its child must not copy that child twice.
    paths
        .iter()
        .filter(|path| {
            !paths
                .iter()
                .any(|parent| *parent != **path && path.starts_with(&format!("{parent}/")))
        })
        .cloned()
        .collect()
}

fn work(incoming: mpsc::Receiver<Job>, reply: mpsc::Sender<Reply>, shared: SharedClient) {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = reply.send(Reply::Connected(Err(error.to_string())));
            return;
        }
    };
    for job in incoming {
        match job {
            Job::Connect(id) => {
                if let Some(old) = shared.lock().unwrap().take() {
                    runtime.block_on(old.close());
                }
                let result = (|| -> anyhow::Result<()> {
                    let lease = crate::update::session_lease()?;
                    let client = runtime.block_on(Client::connect(&id, Duration::from_secs(20)))?;
                    *shared.lock().unwrap() = Some(Arc::new(ActiveClient {
                        client,
                        _lease: lease,
                    }));
                    Ok(())
                })()
                .map_err(|error| format!("{error:#}"));
                let _ = reply.send(Reply::Connected(result));
            }
            Job::List(side, path, generation) => {
                let result = if side == 0 {
                    files::home_dir().and_then(|home| files::list(&home, &path))
                } else {
                    client(&shared).and_then(|client| runtime.block_on(client.list(&path)))
                };
                let _ = reply.send(Reply::Listing(
                    side,
                    path,
                    generation,
                    result.map_err(|error| format!("{error:#}")),
                ));
            }
            Job::Suggest(side, text, home, hidden, generation) => {
                let suggestions = (|| -> anyhow::Result<_> {
                    let relative = relative_address(&text, &home)?;
                    let (parent, prefix) = if text.ends_with(['/', '\\']) {
                        (relative.as_str(), "")
                    } else {
                        relative.rsplit_once('/').unwrap_or(("", &relative))
                    };
                    let listing = if side == 0 {
                        files::list(&files::home_dir()?, parent)?
                    } else {
                        runtime.block_on(client(&shared)?.list(parent))?
                    };
                    Ok(complete(listing, prefix, hidden))
                })()
                .unwrap_or_default();
                let _ = reply.send(Reply::Suggestions(side, generation, suggestions));
            }
            Job::Copy(side, paths, local, remote) => {
                let paths = selections(&paths);
                let total = paths.len().max(1);
                let mut outcomes = Vec::new();
                for (index, path) in paths.iter().enumerate() {
                    let name = path.rsplit('/').next().unwrap_or(path).to_string();
                    let _ = reply.send(Reply::Progress(
                        format!("Copying {name} · {} of {total}", index + 1),
                        index as f32 / total as f32,
                    ));
                    let result = (|| -> anyhow::Result<()> {
                        let client = client(&shared)?;
                        let home = files::home_dir()?;
                        if side == 0 {
                            runtime.block_on(client.upload_tree(&home, path, &remote))
                        } else {
                            runtime.block_on(client.download_tree(&home, path, &local))
                        }
                    })();
                    let failed = result.is_err();
                    outcomes.push(FileTransferRow {
                        name: name.into(),
                        detail: match result {
                            Ok(()) => "Copied".into(),
                            Err(error) => format!("{error:#}").into(),
                        },
                        failed,
                    });
                    if shared.lock().unwrap().is_none() {
                        break;
                    }
                }
                let _ = reply.send(Reply::Copied(outcomes));
            }
            Job::Close => {
                if let Some(client) = shared.lock().unwrap().take() {
                    runtime.block_on(client.close());
                }
                break;
            }
        }
    }
}

pub(super) fn bind(ui: &Launcher) -> Binding {
    ui.set_files_local_entries(model(vec![]));
    ui.set_files_remote_entries(model(vec![]));
    let state = Rc::new(RefCell::new(State::default()));
    let client: SharedClient = Arc::new(Mutex::new(None));
    let (jobs, incoming) = mpsc::channel();
    let (reply, replies) = mpsc::channel();
    let shared = client.clone();
    std::thread::spawn(move || work(incoming, reply, shared));
    load(ui, &mut state.borrow_mut(), &jobs, 0, String::new(), true);
    let weak = ui.as_weak();
    let tx = jobs.clone();
    ui.on_files_connect(move || {
        if let Some(ui) = weak.upgrade() {
            if ui.get_files_connecting() || ui.get_files_connected() {
                return;
            }
            ui.set_files_connecting(true);
            ui.set_files_remote_error("".into());
            ui.set_files_status_error(false);
            ui.set_files_status("".into());
            let _ = tx.send(Job::Connect(ui.get_files_peer().to_string()));
        }
    });
    let weak = ui.as_weak();
    let shared = client.clone();
    let data = state.clone();
    ui.on_files_disconnect(move || {
        if let Some(client) = shared.lock().unwrap().take() {
            client.cancel();
        }
        if let Some(ui) = weak.upgrade() {
            ui.set_files_connected(false);
            ui.set_files_remote_entries(model(vec![]));
            data.borrow_mut().panes[1] = Pane::default();
            ui.set_files_remote_selected(0);
            ui.set_files_remote_error("".into());
            ui.set_files_status("".into());
        }
    });
    let weak = ui.as_weak();
    let shared = client.clone();
    ui.on_files_cancel(move || {
        if let Some(client) = shared.lock().unwrap().take() {
            client.cancel();
        }
        if let Some(ui) = weak.upgrade() {
            ui.set_files_connected(false);
            ui.set_files_status("Cancelling transfer…".into());
        }
    });
    let weak = ui.as_weak();
    let data = state.clone();
    ui.on_files_choose_peer(move |index| {
        if let Some(ui) = weak.upgrade()
            && let Some((id, name)) = data.borrow().peers.get(index as usize)
        {
            ui.set_files_peer(id.clone().into());
            ui.set_files_remote_name(name.clone().into());
        }
    });
    let weak = ui.as_weak();
    let data = state.clone();
    let tx = jobs.clone();
    ui.on_files_navigate(move |side, path| {
        if let Some(ui) = weak.upgrade()
            && (0..=1).contains(&side)
        {
            let mut state = data.borrow_mut();
            match relative_address(&path, &state.panes[side as usize].home) {
                Ok(path) => load(&ui, &mut state, &tx, side as usize, path, true),
                Err(error) => {
                    if side == 0 {
                        ui.set_files_local_error(error.to_string().into());
                    } else {
                        ui.set_files_remote_error(error.to_string().into());
                    }
                }
            }
        }
    });
    let weak = ui.as_weak();
    let data = state.clone();
    let tx = jobs.clone();
    ui.on_files_up(move |side| {
        if let Some(ui) = weak.upgrade()
            && (0..=1).contains(&side)
        {
            let mut state = data.borrow_mut();
            let path = state.panes[side as usize]
                .path
                .rsplit_once('/')
                .map_or("", |(parent, _)| parent)
                .to_string();
            load(&ui, &mut state, &tx, side as usize, path, true);
        }
    });
    let weak = ui.as_weak();
    let data = state.clone();
    let tx = jobs.clone();
    ui.on_files_refresh(move |side| {
        if let Some(ui) = weak.upgrade()
            && (0..=1).contains(&side)
        {
            let mut state = data.borrow_mut();
            let path = state.panes[side as usize].path.clone();
            load(&ui, &mut state, &tx, side as usize, path, true);
        }
    });
    let weak = ui.as_weak();
    let data = state.clone();
    ui.on_files_select(move |side, path, add| {
        if let Some(ui) = weak.upgrade()
            && (0..=1).contains(&side)
        {
            let mut state = data.borrow_mut();
            let pane = &mut state.panes[side as usize];
            let path = path.to_string();
            if add && pane.selected.contains(&path) {
                pane.selected.remove(&path);
            } else {
                if !add && !pane.selected.contains(&path) {
                    pane.selected.clear();
                }
                pane.selected.insert(path);
            }
            display(&ui, &state, side as usize);
        }
    });
    let weak = ui.as_weak();
    let data = state.clone();
    let tx = jobs.clone();
    ui.on_files_expand(move |side, path| {
        if let Some(ui) = weak.upgrade()
            && (0..=1).contains(&side)
            && !ui.get_files_transferring()
        {
            let mut state = data.borrow_mut();
            let pane = &mut state.panes[side as usize];
            let path = path.to_string();
            if !pane.expanded.remove(&path) {
                pane.expanded.insert(path.clone());
                load(&ui, &mut state, &tx, side as usize, path, false);
            } else {
                pane.selected
                    .retain(|selected| !selected.starts_with(&format!("{path}/")));
                display(&ui, &state, side as usize);
            }
        }
    });
    let weak = ui.as_weak();
    let data = state.clone();
    ui.on_files_mode(move |side, mode| {
        if let Some(ui) = weak.upgrade()
            && (0..=1).contains(&side)
            && (0..=2).contains(&mode)
        {
            let mut state = data.borrow_mut();
            state.panes[side as usize].mode = mode;
            display(&ui, &state, side as usize);
        }
    });
    let weak = ui.as_weak();
    let data = state.clone();
    ui.on_files_filter(move |side, text| {
        if let Some(ui) = weak.upgrade()
            && (0..=1).contains(&side)
        {
            let mut state = data.borrow_mut();
            state.panes[side as usize].filter = text.trim().to_lowercase();
            state.panes[side as usize].selected.clear();
            display(&ui, &state, side as usize);
        }
    });
    let weak = ui.as_weak();
    let data = state.clone();
    ui.on_files_hidden_changed(move |side| {
        if let Some(ui) = weak.upgrade()
            && (0..=1).contains(&side)
        {
            let mut state = data.borrow_mut();
            let pane = &mut state.panes[side as usize];
            pane.hidden = if side == 0 {
                ui.get_files_local_hidden()
            } else {
                ui.get_files_remote_hidden()
            };
            pane.selected.clear();
            display(&ui, &state, side as usize);
        }
    });
    let weak = ui.as_weak();
    let data = state.clone();
    let tx = jobs.clone();
    ui.on_files_suggest(move |side, text| {
        if let Some(ui) = weak.upgrade()
            && (0..=1).contains(&side)
            && !ui.get_files_transferring()
        {
            let mut state = data.borrow_mut();
            let pane = &mut state.panes[side as usize];
            pane.suggestion_generation = pane.suggestion_generation.wrapping_add(1);
            let _ = tx.send(Job::Suggest(
                side as usize,
                text.to_string(),
                pane.home.clone(),
                pane.hidden,
                pane.suggestion_generation,
            ));
        }
    });
    let weak = ui.as_weak();
    let data = state.clone();
    let tx = jobs.clone();
    ui.on_files_transfer(move |side, path| {
        if let Some(ui) = weak.upgrade()
            && (0..=1).contains(&side)
            && ui.get_files_connected()
            && !ui.get_files_transferring()
        {
            let state = data.borrow();
            let pane = &state.panes[side as usize];
            let paths = if path.is_empty() || pane.selected.contains(path.as_str()) {
                pane.selected.iter().cloned().collect::<Vec<_>>()
            } else {
                vec![path.to_string()]
            };
            if paths.is_empty() {
                return;
            }
            ui.set_files_transferring(true);
            ui.set_files_progress(0.);
            ui.set_files_status_error(false);
            ui.set_files_transfers(model(vec![]));
            let _ = tx.send(Job::Copy(
                side as usize,
                paths,
                state.panes[0].path.clone(),
                state.panes[1].path.clone(),
            ));
        }
    });
    let weak = ui.as_weak();
    let tx = jobs.clone();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(80),
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let mut state = state.borrow_mut();
            let mut peers = BTreeMap::new();
            for row in ui.get_nearby().iter().chain(ui.get_trusted().iter()) {
                if !row.fingerprint.is_empty() {
                    peers.insert(row.fingerprint.to_string(), row.name.to_string());
                }
            }
            let peers: Vec<_> = peers.into_iter().collect();
            if peers != state.peers {
                ui.set_files_peer_names(model(
                    peers.iter().map(|(_, name)| name.clone().into()).collect(),
                ));
                state.peers = peers;
            }
            for result in replies.try_iter() {
                match result {
                    Reply::Connected(result) => {
                        ui.set_files_connecting(false);
                        ui.set_files_connected(result.is_ok());
                        match result {
                            Ok(()) => {
                                ui.set_files_status("".into());
                                if ui.get_files_remote_name().is_empty() {
                                    ui.set_files_remote_name("Remote device".into());
                                }
                                load(&ui, &mut state, &tx, 1, String::new(), true);
                            }
                            Err(error) => {
                                ui.set_files_remote_error(error.clone().into());
                                ui.set_files_status(error.into());
                                ui.set_files_status_error(true);
                            }
                        }
                    }
                    Reply::Suggestions(side, generation, suggestions) => {
                        if state.panes[side].suggestion_generation == generation {
                            if side == 0 {
                                ui.set_files_local_suggestions(model(suggestions));
                            } else {
                                ui.set_files_remote_suggestions(model(suggestions));
                            }
                        }
                    }
                    Reply::Listing(side, path, generation, result) => {
                        if state.panes[side].generation != generation {
                            continue;
                        }
                        if side == 0 {
                            ui.set_files_local_busy(false);
                        } else {
                            ui.set_files_remote_busy(false);
                        }
                        match result {
                            Ok(listing) => {
                                state.panes[side].home = listing.root.clone();
                                state.panes[side].cache.insert(path, listing);
                            }
                            Err(error) => {
                                if side == 0 {
                                    ui.set_files_local_error(error.into());
                                } else {
                                    ui.set_files_remote_error(error.into());
                                }
                            }
                        }
                        display(&ui, &state, side);
                    }
                    Reply::Progress(status, progress) => {
                        ui.set_files_status(status.into());
                        ui.set_files_progress(progress);
                    }
                    Reply::Copied(outcomes) => {
                        ui.set_files_transferring(false);
                        ui.set_files_progress(1.);
                        let failed = outcomes.iter().filter(|item| item.failed).count();
                        ui.set_files_status(
                            if failed == 0 {
                                format!("Copied {} item(s).", outcomes.len())
                            } else {
                                format!(
                                    "{} copied · {failed} failed. See transfer results.",
                                    outcomes.len() - failed
                                )
                            }
                            .into(),
                        );
                        ui.set_files_status_error(failed > 0);
                        ui.set_files_transfers(model(outcomes));
                        for side in 0..=1 {
                            let path = state.panes[side].path.clone();
                            load(&ui, &mut state, &tx, side, path, true);
                        }
                    }
                }
            }
        },
    );
    Binding {
        _timer: timer,
        jobs,
        client,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_selected_folder_covers_its_descendants() {
        assert_eq!(
            selections(&["Docs".into(), "Docs/file.txt".into(), "Other.txt".into()]),
            vec!["Docs", "Other.txt"]
        );
        assert_eq!(
            selections(&["Docs".into(), "Docs2/file.txt".into()]).len(),
            2
        );
    }
    #[test]
    fn sizes_and_dates_have_stable_labels() {
        assert_eq!(size(512), "512 B");
        assert_eq!(size(1024), "1.0 KB");
        assert_eq!(modified(0), "1970-01-01");
        assert_eq!(modified(1_735_689_600), "2025-01-01");
    }

    #[test]
    fn displayed_name_keeps_the_extension_and_original_unicode() {
        assert_eq!(
            name_parts("Báo cáo cuối cùng 📁.pdf", false),
            ("Báo cáo cuối cùng 📁".into(), ".pdf".into())
        );
        for name in [".gitignore", "README", "Folder.with.dots"] {
            let (stem, suffix) = name_parts(name, name == "Folder.with.dots");
            assert_eq!(stem, name);
            assert!(suffix.is_empty());
        }
        let name = format!("{}.pptx", "Presentation".repeat(20));
        let (stem, suffix) = name_parts(&name, false);
        assert_eq!(suffix, ".pptx");
        assert_eq!(format!("{stem}{suffix}"), name);
    }

    #[test]
    fn absolute_addresses_are_resolved_inside_the_correct_home() {
        assert_eq!(
            relative_address("/Users/qa/Documents/Design", "/Users/qa").unwrap(),
            "Documents/Design"
        );
        assert_eq!(
            relative_address("~/Documents", "/Users/qa").unwrap(),
            "Documents"
        );
        assert_eq!(
            relative_address("c:\\users\\QA\\Documents\\", "C:\\Users\\qa").unwrap(),
            "Documents"
        );
        assert_eq!(
            absolute("C:\\Users\\qa", "Documents/Design"),
            "C:\\Users\\qa\\Documents\\Design"
        );
        assert!(relative_address("/Users/qanother", "/Users/qa").is_err());
        assert!(relative_address("C:\\Windows", "C:\\Users\\qa").is_err());
        assert!(relative_address("~/../secret", "/Users/qa").is_err());
    }

    #[test]
    fn completions_offer_folders_and_obey_the_hidden_filter() {
        let folder = |name: &str| files::Entry {
            name: name.into(),
            path: name.into(),
            is_dir: true,
            size: 0,
            modified: 0,
        };
        let listing = Listing {
            root: "/Users/qa".into(),
            path: String::new(),
            parent: None,
            entries: vec![
                folder("Documents"),
                folder("Downloads"),
                folder(".config"),
                files::Entry {
                    is_dir: false,
                    ..folder("Document.pdf")
                },
            ],
        };
        let suggestions = complete(listing.clone(), "do", false);
        assert_eq!(
            suggestions
                .iter()
                .map(|item| item.path.as_str())
                .collect::<Vec<_>>(),
            vec!["/Users/qa/Documents/", "/Users/qa/Downloads/"]
        );
        assert_eq!(complete(listing.clone(), "", false).len(), 2);
        assert_eq!(complete(listing, ".", true)[0].name, ".config");
    }
}
