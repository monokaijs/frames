//! Quick Apps (as Moonlight's app list): a host's allowlist of applications
//! a viewer can ask for by id. The host starts the application when none of
//! its windows is open, then shares only that application's windows (its
//! own dialogs, menus and tooltips included), never the desktop or other
//! applications.
//!
//! An application is identified by where it is installed, not by its name:
//! its executable's full path on Windows, its outermost `.app` bundle on
//! macOS (helpers inside the bundle count). A window is the app's when its
//! process, or one of that process's ancestors, has that identity or was
//! started for the app by this host (apps whose window comes from a child
//! process). Another program with the same name is never included.
//!
//! The list is `quick-apps.json` in the host's config folder, managed with
//! `frames next quick-apps list|add|remove`.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuickApp {
    /// What a viewer asks for (`connect --app <id>`): lowercase letters,
    /// digits, '-' and '_'.
    pub id: String,
    /// Shown to viewers.
    pub name: String,
    /// Started when the viewer asks and no window of it is open: an
    /// executable (Windows), or an .app path or bundle id (macOS, through
    /// `open`).
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Which windows are its: the executable's full path (Windows) or the
    /// `.app` bundle's path (macOS) of the process that shows them, or of
    /// an ancestor of it.
    pub path: String,
}

/// What a host tells viewers about one of its Quick Apps.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listed {
    pub id: String,
    pub name: String,
    /// Its icon, a PNG of at most 64x64, base64, while one of its windows
    /// is open (Windows hosts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

/// The processes running now: their parents, and their identity (as
/// [`QuickApp::path`]) and start time looked up when needed.
pub struct Processes {
    parents: std::collections::HashMap<u32, u32>,
    identities: std::cell::RefCell<std::collections::HashMap<u32, Option<String>>>,
    started: std::cell::RefCell<std::collections::HashMap<u32, Option<u64>>>,
}

impl Processes {
    pub fn now() -> Self {
        Self {
            parents: parents(),
            identities: Default::default(),
            started: Default::default(),
        }
    }

    /// `pid`'s parent. Windows keeps the id of a parent that exited, and a
    /// later process can be given that id: a "parent" started after its
    /// child is not its parent (macOS hands orphans to launchd instead).
    fn parent(&self, pid: u32) -> Option<u32> {
        let parent = *self.parents.get(&pid)?;
        if parent == pid {
            return None;
        }
        match (self.started(parent), self.started(pid)) {
            (Some(parent_started), Some(started)) if parent_started > started => None,
            _ => Some(parent),
        }
    }

    fn started(&self, pid: u32) -> Option<u64> {
        *self
            .started
            .borrow_mut()
            .entry(pid)
            .or_insert_with(|| process_started(pid))
    }

    /// The one of `pids` that all the others descend from, if any (its
    /// process tree is theirs).
    pub fn common_root(&self, pids: &[u32]) -> Option<u32> {
        pids.iter().copied().find(|&root| {
            pids.iter().all(|&pid| {
                let mut at = pid;
                for _ in 0..ANCESTRY {
                    if at == root {
                        return true;
                    }
                    match self.parent(at) {
                        Some(parent) => at = parent,
                        None => return false,
                    }
                }
                false
            })
        })
    }

    /// Processes with these parents (tests).
    #[cfg(test)]
    pub fn with_parents(parents: &[(u32, u32)]) -> Self {
        Self {
            parents: parents.iter().copied().collect(),
            identities: Default::default(),
            started: Default::default(),
        }
    }

    fn identity(&self, pid: u32) -> Option<String> {
        self.identities
            .borrow_mut()
            .entry(pid)
            .or_insert_with(|| process_path(pid).map(|path| normalize(&path)))
            .clone()
    }
}

/// A path as identities compare it: macOS's outermost `.app` bundle (or
/// the path itself outside a bundle); on Windows, backslashes and any
/// case.
fn normalize(path: &str) -> String {
    #[cfg(windows)]
    {
        path.replace('/', "\\")
            .trim_end_matches('\\')
            .to_lowercase()
    }
    #[cfg(not(windows))]
    {
        let path = path.trim_end_matches('/');
        match path.find(".app/") {
            Some(end) => path[..end + 4].to_string(),
            None => path.to_string(),
        }
    }
}

/// A configured path as identities compare it. macOS reports processes by
/// their real path (`/tmp` is `/private/tmp`), so an existing path is
/// resolved first; one that does not exist (yet) is compared as given.
/// Windows paths are compared as given (resolving adds a `\\?\` prefix,
/// and executables are reported by the path they were started from).
fn configured_identity(path: &str) -> String {
    #[cfg(not(windows))]
    if let Ok(real) = std::fs::canonicalize(path) {
        return normalize(&real.to_string_lossy());
    }
    normalize(path)
}

/// Processes this host started for each Quick App: app id, process id and
/// its start time (an id alone could since be another process's).
static LAUNCHED: std::sync::Mutex<Vec<(String, u32, Option<u64>)>> =
    std::sync::Mutex::new(Vec::new());

/// How far up a window's process ancestry its app is looked for.
const ANCESTRY: usize = 16;

impl QuickApp {
    /// Whether `window` belongs to this application (see the module).
    pub fn owns(&self, window: &crate::catalog::Window, processes: &Processes) -> bool {
        let identity = configured_identity(&self.path);
        let launched: Vec<(u32, Option<u64>)> = LAUNCHED
            .lock()
            .unwrap()
            .iter()
            .filter(|(id, _, _)| *id == self.id)
            .map(|&(_, pid, started)| (pid, started))
            .collect();
        let started_for_it = |pid: u32| {
            launched.iter().any(|&(launched, started)| {
                launched == pid && (started.is_none() || started == processes.started(pid))
            })
        };
        let mut pid = window.pid;
        for _ in 0..ANCESTRY {
            if pid == 0 {
                return false;
            }
            if started_for_it(pid) || processes.identity(pid).as_deref() == Some(&identity) {
                return true;
            }
            match processes.parent(pid) {
                Some(parent) => pid = parent,
                None => return false,
            }
        }
        false
    }

    pub fn listed(&self) -> Listed {
        Listed {
            id: self.id.clone(),
            name: self.name.clone(),
            icon: None,
        }
    }

    /// Starts it, detached: it keeps running when the session ends.
    pub fn launch(&self) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            let mut open = std::process::Command::new("open");
            if self.command.ends_with(".app") || self.command.contains('/') {
                open.arg("-a").arg(&self.command);
            } else {
                open.arg("-b").arg(&self.command);
            }
            if !self.args.is_empty() {
                open.arg("--args").args(&self.args);
            }
            let status = open
                .status()
                .with_context(|| format!("starting {}", self.name))?;
            anyhow::ensure!(status.success(), "macOS could not open {}", self.command);
            Ok(())
        }
        #[cfg(not(target_os = "macos"))]
        {
            use std::process::Stdio;
            let child = std::process::Command::new(&self.command)
                .args(&self.args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .with_context(|| format!("starting {} ({})", self.name, self.command))?;
            // Its windows count even if it is a launcher whose child shows
            // them under another executable.
            LAUNCHED.lock().unwrap().push((
                self.id.clone(),
                child.id(),
                process_started(child.id()),
            ));
            Ok(())
        }
    }
}

/// Why a host closes a session for a Quick App (its close reason).
pub const NO_SUCH_APP: &str = "no such Quick App";
pub const NOT_IN_APP: &str = "not part of the Quick App";
pub const ONLY_THE_APP: &str = "only the Quick App is shared";

/// What became of starting a Quick App for a viewer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Launch {
    /// A view-only viewer never starts anything.
    NotAllowed,
    /// One of its windows was already shared.
    Running,
    Started(std::time::Instant),
    /// The host could not start it (why, for the viewer).
    Failed(String),
}

/// Host: starts `app` for a viewer allowed to control when none of its
/// windows is shared now (a view-only viewer never starts anything).
pub async fn start_if_needed(
    app: &QuickApp,
    control: bool,
    filters: Vec<String>,
    limit: usize,
) -> Launch {
    if !control {
        println!("Quick App {}: not started for a view-only viewer", app.id);
        return Launch::NotAllowed;
    }
    // Sessions are admitted side by side: one check-then-start at a time.
    static STARTING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _one = STARTING.lock().await;
    let check = app.clone();
    let running = tokio::task::spawn_blocking(move || {
        !crate::catalog::shared_in(&filters, limit, Some(&check)).is_empty()
    })
    .await
    .unwrap_or(false);
    if running {
        println!("Quick App {}: already open", app.id);
        return Launch::Running;
    }
    let start = app.clone();
    match tokio::task::spawn_blocking(move || start.launch()).await {
        Ok(Ok(())) => {
            println!("Quick App {}: started {}", app.id, app.command);
            Launch::Started(std::time::Instant::now())
        }
        Ok(Err(error)) => {
            println!("Quick App {}: {error:#}", app.id);
            Launch::Failed(format!("{error:#}"))
        }
        Err(error) => Launch::Failed(error.to_string()),
    }
}

/// Host: sends its Quick Apps to a trusted viewer (`QUICK_APPS` stream).
pub async fn serve_list(
    session: frames_net::Session,
    config: PathBuf,
    filters: Vec<String>,
    limit: usize,
) -> Result<()> {
    let apps = load(&config)?;
    // Icons from the apps' open windows.
    let listed = tokio::task::spawn_blocking(move || {
        apps.iter()
            .map(|app| Listed {
                icon: crate::catalog::shared_in(&filters, limit, Some(app))
                    .into_iter()
                    .find_map(|w| w.icon),
                ..app.listed()
            })
            .collect::<Vec<_>>()
    })
    .await?;
    let (mut send, _recv) = crate::streams::open(&session, crate::streams::QUICK_APPS).await?;
    send.write_all(&serde_json::to_vec(&listed)?).await?;
    send.finish()?;
    // Let the viewer read it before the session goes.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), session.closed()).await;
    Ok(())
}

/// Viewer: a host's Quick Apps (trusted devices only; nobody is asked).
pub async fn remote(id: &str, wait: std::time::Duration) -> Result<Vec<Listed>> {
    let config = crate::admit::config_dir(None)?;
    let key = crate::admit::device_key(&config)?;
    let fetch = async {
        let node = frames_net::Node::bind(&key, frames_net::NodeConfig::default()).await?;
        let session = node.connect(crate::directory::target(id).await?).await?;
        crate::admit::send_hello(
            &session,
            &node.id(),
            false,
            crate::admit::Asks {
                decoders: Vec::new(),
                window: None,
                name: String::new(),
                audio: false,
                max_mbps: None,
                catalog: false,
                window_id: None,
                password: None,
                preview: false,
                app: None,
                only_app: false,
                quick_apps: true,
                files: false,
            },
        )
        .await?;
        let read = async {
            let (_send, mut recv) = session.accept_channel().await?;
            let mut kind = [0u8; 1];
            recv.read_exact(&mut kind).await?;
            anyhow::ensure!(
                kind[0] == crate::streams::QUICK_APPS,
                "not a Quick Apps list"
            );
            let bytes = recv.read_to_end(256 * 1024).await?;
            Ok(serde_json::from_slice::<Vec<Listed>>(&bytes)?)
        };
        let listed = read.await.map_err(|error| match session.close_reason() {
            Some(reason) => anyhow::anyhow!("the host refused: {reason}"),
            None => error,
        });
        session.close("Quick Apps received");
        node.close().await;
        listed
    };
    tokio::time::timeout(wait, fetch)
        .await
        .context("no Quick Apps list in time")?
}

/// Every process's parent.
#[cfg(windows)]
fn parents() -> std::collections::HashMap<u32, u32> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    let mut parents = std::collections::HashMap::new();
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return parents;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut more = Process32FirstW(snapshot, &mut entry).is_ok();
        while more {
            parents.insert(entry.th32ProcessID, entry.th32ParentProcessID);
            more = Process32NextW(snapshot, &mut entry).is_ok();
        }
        let _ = CloseHandle(snapshot);
    }
    parents
}

#[cfg(target_os = "macos")]
fn parents() -> std::collections::HashMap<u32, u32> {
    let mut parents = std::collections::HashMap::new();
    // Room for every process now and some started meanwhile.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    let mut pids = vec![0i32; count.max(0) as usize + 256];
    let bytes = unsafe {
        libc::proc_listallpids(
            pids.as_mut_ptr().cast(),
            (pids.len() * std::mem::size_of::<i32>()) as i32,
        )
    };
    for &pid in pids.iter().take(bytes.max(0) as usize) {
        if pid <= 0 {
            continue;
        }
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
        let read = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                size,
            )
        };
        if read == size {
            parents.insert(pid as u32, info.pbi_ppid);
        }
    }
    parents
}

#[cfg(not(any(windows, target_os = "macos")))]
fn parents() -> std::collections::HashMap<u32, u32> {
    Default::default()
}

/// When a process started (comparable between processes of this computer).
#[cfg(windows)]
fn process_started(pid: u32) -> Option<u64> {
    use windows::Win32::Foundation::{CloseHandle, FILETIME};
    use windows::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let (mut created, mut exited, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        let read = GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user);
        let _ = CloseHandle(process);
        read.ok()?;
        Some((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
    }
}

#[cfg(target_os = "macos")]
fn process_started(pid: u32) -> Option<u64> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    let read = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    (read == size).then(|| info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
}

#[cfg(not(any(windows, target_os = "macos")))]
fn process_started(_pid: u32) -> Option<u64> {
    None
}

/// A process's executable.
#[cfg(windows)]
fn process_path(pid: u32) -> Option<String> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut path = vec![0u16; 32768];
        let mut size = path.len() as u32;
        let named = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(path.as_mut_ptr()),
            &mut size,
        );
        let _ = CloseHandle(process);
        named.ok()?;
        Some(String::from_utf16_lossy(&path[..size as usize]))
    }
}

#[cfg(target_os = "macos")]
fn process_path(pid: u32) -> Option<String> {
    let mut path = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let length =
        unsafe { libc::proc_pidpath(pid as i32, path.as_mut_ptr().cast(), path.len() as u32) };
    (length > 0).then(|| String::from_utf8_lossy(&path[..length as usize]).into_owned())
}

#[cfg(not(any(windows, target_os = "macos")))]
fn process_path(_pid: u32) -> Option<String> {
    None
}

pub fn path(config: &Path) -> PathBuf {
    config.join("quick-apps.json")
}

/// The host's Quick Apps (none when the file does not exist).
pub fn load(config: &Path) -> Result<Vec<QuickApp>> {
    let apps: Vec<QuickApp> = match std::fs::read(path(config)) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("reading quick-apps.json")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("reading quick-apps.json"),
    };
    validate(&apps)?;
    Ok(apps)
}

pub fn save(config: &Path, apps: &[QuickApp]) -> Result<()> {
    validate(apps)?;
    std::fs::create_dir_all(config)?;
    let target = path(config);
    let temporary = target.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(apps)?)?;
    std::fs::rename(&temporary, &target)?;
    Ok(())
}

pub fn find<'a>(apps: &'a [QuickApp], id: &str) -> Option<&'a QuickApp> {
    apps.iter().find(|app| app.id == id)
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

fn validate(apps: &[QuickApp]) -> Result<()> {
    for (index, app) in apps.iter().enumerate() {
        if !valid_id(&app.id) {
            bail!(
                "Quick App id {:?}: use up to 32 lowercase letters, digits, '-' or '_'",
                app.id
            );
        }
        if apps[..index].iter().any(|other| other.id == app.id) {
            bail!("Quick App id {:?} is listed twice", app.id);
        }
        if app.name.trim().is_empty() || app.command.trim().is_empty() {
            bail!("Quick App {:?} needs a name and a command", app.id);
        }
        if !Path::new(&app.path).is_absolute() {
            bail!(
                "Quick App {:?}: its path must be the full path of its executable (Windows) or .app (macOS), not {:?}",
                app.id,
                app.path
            );
        }
    }
    Ok(())
}

/// `frames next quick-apps list|add|remove [--config-dir DIR] [--json]`, and
/// `quick-apps remote HOST [--json]` (a host's list, for the launcher).
pub async fn cli(args: &[String]) -> Result<()> {
    if args.first().map(String::as_str) == Some("remote") {
        let id = args.get(1).context("quick-apps remote needs a host id")?;
        let json = args.iter().any(|a| a == "--json");
        let listed = remote(id, std::time::Duration::from_secs(15)).await?;
        if json {
            println!("{}", serde_json::to_string(&listed)?);
        } else if listed.is_empty() {
            println!("the host has no Quick Apps");
        } else {
            for app in listed {
                println!("{}  {}", app.id, app.name);
            }
        }
        return Ok(());
    }
    let mut config_dir = None;
    let mut json = false;
    let mut fields: std::collections::HashMap<&str, String> = std::collections::HashMap::new();
    let mut extra = Vec::new();
    let mut rest = args.iter().skip(1);
    while let Some(arg) = rest.next() {
        let mut value = || {
            rest.next()
                .cloned()
                .with_context(|| format!("{arg} needs a value"))
        };
        match arg.as_str() {
            "--config-dir" => config_dir = Some(PathBuf::from(value()?)),
            "--id" => drop(fields.insert("id", value()?)),
            "--name" => drop(fields.insert("name", value()?)),
            "--path" => drop(fields.insert("path", value()?)),
            "--command" => drop(fields.insert("command", value()?)),
            "--arg" => extra.push(value()?),
            "--json" => json = true,
            other => bail!("quick-apps does not take {other}"),
        }
    }
    let config = crate::admit::config_dir(config_dir.as_ref())?;
    let mut apps = load(&config)?;
    match args.first().map(String::as_str) {
        Some("list") if json => println!("{}", serde_json::to_string(&apps)?),
        Some("list") => {
            if apps.is_empty() {
                println!(
                    "no Quick Apps (add one with: quick-apps add --id ID --name NAME --command PATH [--path APP] [--arg A]...)"
                );
            }
            for app in &apps {
                println!(
                    "{}  {}  identity {}  runs {}{}",
                    app.id,
                    app.name,
                    app.path,
                    app.command,
                    if app.args.is_empty() {
                        String::new()
                    } else {
                        format!(" {}", app.args.join(" "))
                    }
                );
            }
        }
        Some("add") => {
            let given_path = fields.remove("path");
            let mut take = |key: &str| {
                fields
                    .remove(key)
                    .with_context(|| format!("quick-apps add needs --{key}"))
            };
            let command = take("command")?;
            // Its identity: given, else the command when that is the
            // program itself (an executable, or an .app on macOS).
            let path = match given_path {
                Some(path) => path,
                None if Path::new(&command).is_absolute() => command.clone(),
                None => bail!(
                    "quick-apps add needs --path (the full path of its executable or .app) when --command is not one"
                ),
            };
            let app = QuickApp {
                id: take("id")?,
                name: take("name")?,
                command,
                args: extra,
                path,
            };
            apps.retain(|other| other.id != app.id);
            println!("Quick App {} added: {}", app.id, app.name);
            apps.push(app);
            save(&config, &apps)?;
        }
        Some("remove") => {
            let id = fields
                .remove("id")
                .context("quick-apps remove needs --id")?;
            let before = apps.len();
            apps.retain(|app| app.id != id);
            if apps.len() == before {
                bail!("no Quick App {id}");
            }
            save(&config, &apps)?;
            println!("Quick App {id} removed");
        }
        _ => bail!("quick-apps takes list, add or remove"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    const PATH: &str = r"C:\Windows\notepad.exe";
    #[cfg(windows)]
    const OTHER: &str = r"D:\Tools\notepad.exe";
    #[cfg(not(windows))]
    const PATH: &str = "/Applications/Notes.app/Contents/MacOS/Notes";
    #[cfg(not(windows))]
    const OTHER: &str = "/Users/me/Downloads/Notes.app/Contents/MacOS/Notes";

    fn notes() -> QuickApp {
        QuickApp {
            id: "notes".into(),
            name: "Notes".into(),
            command: PATH.into(),
            args: Vec::new(),
            #[cfg(windows)]
            path: PATH.to_uppercase(),
            #[cfg(not(windows))]
            path: "/Applications/Notes.app".into(),
        }
    }

    fn window(pid: u32) -> crate::catalog::Window {
        let mut window: crate::catalog::Window =
            serde_json::from_value(serde_json::json!({"id": pid, "title": "", "app": "notes"}))
                .unwrap();
        window.pid = pid;
        window
    }

    /// Processes: 10 the app, 11 its child (another executable), 20 a
    /// program with the same name elsewhere, 30 a stranger, 40 a launcher's
    /// child whose parent exited.
    fn processes() -> Processes {
        let identities = [
            (10, Some(PATH.to_string())),
            (11, Some("/x/helper".to_string())),
            (20, Some(OTHER.to_string())),
            (30, Some("/x/stranger".to_string())),
            (40, Some("/x/game".to_string())),
            (1, Some("/x/init".to_string())),
        ]
        .into_iter()
        .map(|(pid, path)| (pid, path.map(|p| normalize(&p))))
        .collect();
        Processes {
            parents: [(10, 1), (11, 10), (20, 1), (30, 1), (40, 39)]
                .into_iter()
                .collect(),
            identities: std::cell::RefCell::new(identities),
            started: Default::default(),
        }
    }

    #[test]
    fn only_the_installed_app_and_its_children_are_its() {
        let (app, processes) = (notes(), processes());
        assert!(app.owns(&window(10), &processes));
        assert!(
            app.owns(&window(11), &processes),
            "a child process's window"
        );
        assert!(
            !app.owns(&window(20), &processes),
            "same name, other program"
        );
        assert!(!app.owns(&window(30), &processes));
        assert!(!app.owns(&window(0), &processes), "unknown process");
        // A launcher this host started (its child shows the window, under
        // another executable, after the launcher exited).
        assert!(!app.owns(&window(40), &processes));
        LAUNCHED.lock().unwrap().push(("notes".into(), 39, None));
        assert!(app.owns(&window(40), &processes));
        LAUNCHED.lock().unwrap().retain(|(id, _, _)| id != "notes");
    }

    /// Process ids are reused: neither a process that took the id of the
    /// app's exited parent, nor one that took the id of a process this
    /// host started, makes a stranger's windows the app's.
    #[test]
    fn reused_process_ids_do_not_make_strangers_the_apps() {
        // Its own id: tests run in parallel and share the started list.
        let app = QuickApp {
            id: "reused".into(),
            ..notes()
        };
        let processes = processes();
        // 50: a stranger whose parent exited; the app (10) later got that
        // id. Started before its "parent", so it is not its child.
        let mut processes = Processes {
            parents: processes.parents.into_iter().chain([(50, 10)]).collect(),
            ..processes
        };
        processes
            .identities
            .get_mut()
            .insert(50, Some("/x/stranger".into()));
        processes.started = std::cell::RefCell::new(
            [(10, Some(500)), (50, Some(100)), (11, Some(600))]
                .into_iter()
                .collect(),
        );
        assert!(
            !app.owns(&window(50), &processes),
            "an orphan is not the app's"
        );
        assert!(app.owns(&window(11), &processes), "a real child still is");
        // 30 has the id of a process this host started earlier, which
        // exited: another start time, another process.
        processes.started.get_mut().insert(30, Some(900));
        LAUNCHED
            .lock()
            .unwrap()
            .push(("reused".into(), 30, Some(200)));
        assert!(!app.owns(&window(30), &processes));
        LAUNCHED.lock().unwrap().retain(|(id, _, _)| id != "reused");
    }

    /// The real process table: a child of this test is found with its
    /// parent, its executable and a start time after its parent's.
    #[test]
    #[cfg(any(windows, target_os = "macos"))]
    fn a_real_child_process_is_found_by_its_executable_and_parent() {
        #[cfg(windows)]
        let (program, args) = (r"C:\Windows\System32\PING.EXE", ["-n", "6", "127.0.0.1"]);
        #[cfg(target_os = "macos")]
        let (program, args) = ("/bin/sleep", ["6", "", ""]);
        let mut child = std::process::Command::new(program)
            .args(args.iter().filter(|a| !a.is_empty()))
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let processes = Processes::now();
        let pid = child.id();
        assert_eq!(processes.parents.get(&pid), Some(&std::process::id()));
        assert_eq!(processes.parent(pid), Some(std::process::id()));
        assert_eq!(processes.identity(pid), Some(normalize(program)));
        assert!(processes.started(pid) >= processes.started(std::process::id()));
        let app = QuickApp {
            path: program.into(),
            ..notes()
        };
        assert!(app.owns(&window(pid), &processes));
        assert!(!notes().owns(&window(pid), &processes));
        let _ = child.kill();
        let _ = child.wait();
    }

    /// A Quick App configured through a symbolic link (macOS's `/tmp` is
    /// `/private/tmp`) matches the process the system reports by its real
    /// path, and only that one.
    #[test]
    #[cfg(unix)]
    fn a_configured_path_through_a_link_matches_the_real_path() {
        let base = std::env::temp_dir().join(format!("frames-qa-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let real = base.join("real");
        let binary = real.join("Fixture.app/Contents/MacOS");
        std::fs::create_dir_all(&binary).unwrap();
        let link = base.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        // What the system reports: the real path, links resolved.
        let reported = std::fs::canonicalize(&binary).unwrap().join("Fixture");
        let processes = Processes {
            parents: [(70, 1), (71, 1)].into_iter().collect(),
            identities: std::cell::RefCell::new(
                [
                    (70, Some(normalize(&reported.to_string_lossy()))),
                    (71, Some("/Applications/Other.app".to_string())),
                ]
                .into_iter()
                .collect(),
            ),
            started: Default::default(),
        };
        let app = QuickApp {
            id: "linked".into(),
            path: link.join("Fixture.app").to_string_lossy().into_owned(),
            ..notes()
        };
        assert!(
            app.owns(&window(70), &processes),
            "configured through the link"
        );
        assert!(!app.owns(&window(71), &processes));
        // A path that does not exist is compared as given (no match here).
        let missing = QuickApp {
            path: base
                .join("missing/Fixture.app")
                .to_string_lossy()
                .into_owned(),
            ..app
        };
        assert!(!missing.owns(&window(70), &processes));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A configured app that is not there: starting it fails, with the
    /// reason kept for the viewer.
    #[tokio::test]
    #[cfg(any(windows, target_os = "macos"))]
    async fn a_missing_app_fails_to_start_with_its_reason() {
        let app = QuickApp {
            id: "missing".into(),
            command: "/nonexistent/frames-qa/Missing.app".into(),
            path: "/nonexistent/frames-qa/Missing.app".into(),
            ..notes()
        };
        assert_eq!(
            start_if_needed(&app, false, Vec::new(), 8).await,
            Launch::NotAllowed
        );
        match start_if_needed(&app, true, Vec::new(), 8).await {
            Launch::Failed(reason) => assert!(!reason.is_empty()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn identities_compare_by_install_location() {
        #[cfg(windows)]
        assert_eq!(
            normalize("C:/Windows/NOTEPAD.EXE"),
            r"c:\windows\notepad.exe"
        );
        #[cfg(not(windows))]
        assert_eq!(
            normalize("/Applications/Notes.app/Contents/Frameworks/Helper.app/Contents/MacOS/H"),
            "/Applications/Notes.app"
        );
    }

    #[test]
    fn the_list_round_trips_and_rejects_bad_entries() {
        let dir = std::env::temp_dir().join(format!("frames-quick-apps-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(load(&dir).unwrap(), Vec::new(), "no file: no Quick Apps");
        save(&dir, &[notes()]).unwrap();
        assert_eq!(load(&dir).unwrap(), vec![notes()]);
        let mut bad = notes();
        bad.id = "Note Pad".into();
        assert!(save(&dir, &[bad]).is_err());
        assert!(save(&dir, &[notes(), notes()]).is_err());
        let mut relative = notes();
        relative.path = "notes".into();
        assert!(
            save(&dir, &[relative]).is_err(),
            "a name is not an identity"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
