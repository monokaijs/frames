//! File-manager sessions: browse and copy inside the sharing user's home.
//! Admission and scope checks happen before this service starts; no video
//! capture, clipboard, input, or remote application is opened by this session.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

const KIND: u8 = 18;
const MAX_JSON: usize = 4 * 1024 * 1024;
const MAX_ENTRIES: usize = 10_000;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    /// Slash-separated, relative to the home folder on this side.
    pub path: String,
    pub is_dir: bool,
    #[serde(default)]
    pub hidden: bool,
    pub size: u64,
    pub modified: u64,
}

impl Entry {
    pub fn is_hidden(&self) -> bool {
        self.hidden || self.name.starts_with('.')
    }
}

fn hidden_attribute(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_HIDDEN.0
            != 0
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::macos::fs::MetadataExt;
        metadata.st_flags() & libc::UF_HIDDEN != 0
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = metadata;
        false
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Listing {
    pub root: String,
    pub path: String,
    pub parent: Option<String>,
    pub entries: Vec<Entry>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Request {
    List { path: String },
    Upload { path: String },
    Download { path: String },
    Mkdir { path: String },
}

#[derive(Default, Serialize, Deserialize)]
struct Reply {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    listing: Option<Listing>,
}

async fn write<T: Serialize>(send: &mut iroh::endpoint::SendStream, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() <= MAX_JSON, "Directory listing is too large");
    send.write_all(&(bytes.len() as u32).to_be_bytes()).await?;
    send.write_all(&bytes).await?;
    Ok(())
}

async fn read<T: for<'a> Deserialize<'a>>(recv: &mut iroh::endpoint::RecvStream) -> Result<T> {
    let mut size = [0; 4];
    recv.read_exact(&mut size).await?;
    let size = u32::from_be_bytes(size) as usize;
    ensure!(size <= MAX_JSON, "File-sharing message is too large");
    let mut bytes = vec![0; size];
    recv.read_exact(&mut bytes).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub fn home_dir() -> Result<PathBuf> {
    directories::UserDirs::new()
        .map(|dirs| dirs.home_dir().to_path_buf())
        .context("No home folder is available")
}

fn relative(path: &str) -> Result<PathBuf> {
    ensure!(
        path.len() <= 16_384 && !path.contains(['\\', '\0', ':']),
        "Invalid relative path"
    );
    let path = Path::new(path);
    ensure!(
        path.components()
            .all(|part| matches!(part, Component::Normal(_))),
        "Choose a path inside the home folder"
    );
    Ok(path.to_path_buf())
}

/// Refuse symlinks and reparse points at every component, including ones
/// that point back inside home: tree traversal must not loop or escape.
pub fn resolve(root: &Path, path: &str) -> Result<PathBuf> {
    let mut target = root.canonicalize()?;
    for part in relative(path)?.components() {
        target.push(part);
        let metadata = std::fs::symlink_metadata(&target)
            .context("This file or folder is no longer available")?;
        ensure!(
            !is_link(&metadata),
            "Links cannot be browsed or transferred"
        );
    }
    Ok(target)
}

fn is_link(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

pub fn list(root: &Path, path: &str) -> Result<Listing> {
    let directory = resolve(root, path)?;
    ensure!(directory.is_dir(), "Not a folder");
    let mut entries = Vec::new();
    for item in std::fs::read_dir(directory)? {
        let item = item?;
        let Some(name) = item.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let metadata = std::fs::symlink_metadata(item.path())?;
        if is_link(&metadata) || (!metadata.is_dir() && !metadata.is_file()) {
            continue;
        }
        entries.push(Entry {
            path: join(path, &name),
            name,
            is_dir: metadata.is_dir(),
            hidden: hidden_attribute(&metadata),
            size: metadata.len(),
            modified: metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |time| time.as_secs()),
        });
        ensure!(
            entries.len() <= MAX_ENTRIES,
            "This folder has too many entries to display"
        );
    }
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(Listing {
        root: display_path(&root.canonicalize()?),
        path: path.into(),
        parent: (!path.is_empty()).then(|| {
            path.rsplit_once('/')
                .map_or("", |(parent, _)| parent)
                .into()
        }),
        entries,
    })
}

pub fn display_path(path: &Path) -> String {
    let path = path.to_string_lossy();
    if let Some(unc) = path.strip_prefix("\\\\?\\UNC\\") {
        format!("\\\\{unc}")
    } else {
        path.strip_prefix("\\\\?\\").unwrap_or(&path).to_string()
    }
}

pub fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.into()
    } else {
        format!("{parent}/{name}")
    }
}

pub fn mkdir(root: &Path, path: &str) -> Result<()> {
    let path = relative(path)?;
    let name = path.file_name().context("Enter a folder name")?;
    let parent = path.parent().unwrap_or(Path::new(""));
    let parent = resolve(root, parent.to_str().context("Invalid folder name")?)?;
    let target = parent.join(name);
    match std::fs::create_dir(&target) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            ensure!(
                !is_link(&std::fs::symlink_metadata(&target)?) && target.is_dir(),
                "A file already has that folder name"
            );
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

/// The host calls this only after granting control and checking that its
/// sharing scope allows filesystem access. Existing clients are unchanged.
pub(crate) async fn serve(
    session: frames_net::Session,
    root: PathBuf,
    config: Option<PathBuf>,
) -> Result<()> {
    let (mut ready, _) = crate::streams::open(&session, KIND).await?;
    write(&mut ready, &Reply::default()).await?;
    ready.finish()?;
    let mut tasks = tokio::task::JoinSet::new();
    let mut check = tokio::time::interval(Duration::from_secs(1));
    loop {
        if tasks.len() >= 4 {
            let _ = tasks.join_next().await;
        }
        let streams = tokio::select! {
            streams = session.accept_channel() => streams,
            _ = check.tick() => {
                if let Some(config) = &config {
                    let allowed = frames_net::trust::TrustList::load(&frames_net::trust::TrustList::path_in(config))
                        .ok().and_then(|list| list.get(&session.remote()).map(|peer| peer.allow_control)).unwrap_or(false);
                    if !allowed { session.close("File sharing control permission revoked"); break }
                }
                continue
            }
        };
        let Ok((send, mut recv)) = streams else {
            break;
        };
        let mut kind = [0];
        if tokio::time::timeout(Duration::from_secs(10), recv.read_exact(&mut kind))
            .await
            .is_err()
            || kind[0] != KIND
        {
            continue;
        }
        let root = root.clone();
        tasks.spawn(async move {
            if let Err(error) = serve_request((send, recv), root).await {
                println!("file sharing: {error:#}");
            }
        });
    }
    tasks.abort_all();
    Ok(())
}

async fn serve_request((mut send, mut recv): crate::streams::Streams, root: PathBuf) -> Result<()> {
    let request: Request = tokio::time::timeout(Duration::from_secs(10), read(&mut recv)).await??;
    let prepared = tokio::task::spawn_blocking(move || -> Result<_> {
        let (path, listing) = match &request {
            Request::List { path } => (None, Some(list(&root, path)?)),
            Request::Mkdir { path } => {
                mkdir(&root, path)?;
                (None, None)
            }
            Request::Upload { path } => {
                let path = resolve(&root, path)?;
                ensure!(path.is_dir(), "Upload destination is not a folder");
                (Some(path), None)
            }
            Request::Download { path } => {
                let path = resolve(&root, path)?;
                ensure!(path.is_file(), "Choose a regular file");
                (Some(path), None)
            }
        };
        Ok((request, path, listing))
    })
    .await?;
    let (request, path, listing) = match prepared {
        Ok(value) => value,
        Err(error) => {
            write(
                &mut send,
                &Reply {
                    error: Some(format!("{error:#}")),
                    ..Default::default()
                },
            )
            .await?;
            send.finish()?;
            return Ok(());
        }
    };
    write(
        &mut send,
        &Reply {
            listing,
            error: None,
        },
    )
    .await?;
    match request {
        Request::Upload { .. } => {
            crate::transfer::receive((send, recv), path.unwrap()).await?;
        }
        Request::Download { .. } => {
            crate::transfer::send_on((send, recv), &path.unwrap()).await?;
        }
        _ => {
            send.finish()?;
        }
    }
    Ok(())
}

pub struct Client {
    node: frames_net::Node,
    session: frames_net::Session,
}

impl Client {
    pub async fn connect(id: &str, wait: Duration) -> Result<Self> {
        let config = crate::admit::config_dir(None)?;
        let key = crate::admit::device_key(&config)?;
        let node = frames_net::Node::bind(&key, frames_net::NodeConfig::default()).await?;
        let result = tokio::time::timeout(wait, async {
            let ip = id.parse::<std::net::IpAddr>().ok().or_else(|| id.parse::<std::net::SocketAddr>().ok().map(|address| address.ip()));
            let target = if let Some(ip) = ip {
                let matching: Vec<_> = frames_net::local::browse(Duration::from_millis(1500)).await?.into_iter().filter(|peer| peer.addr.ip_addrs().any(|address| address.ip() == ip)).collect();
                ensure!(matching.len() == 1, "No unique device sharing at this IP was found. Choose a nearby device or enter its full ID.");
                matching.into_iter().next().unwrap().addr
            } else { crate::directory::target(&crate::directory::normalize_code(id)).await? };
            let session = node.connect(target).await?;
            crate::admit::send_hello(
                &session,
                &node.id(),
                false,
                crate::admit::Asks {
                    decoders: vec![],
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
                    quick_apps: false,
                    files: true,
                },
            )
            .await?;
            let (_, mut recv) = session.accept_channel().await?;
            let mut kind = [0];
            recv.read_exact(&mut kind).await?;
            ensure!(kind[0] == KIND, "The host does not support File sharing");
            let reply: Reply = read(&mut recv).await?;
            if let Some(error) = reply.error {
                bail!(error)
            }
            Ok::<_, anyhow::Error>(session)
        })
        .await
        .context(
            "File sharing timed out. Update the host and trust this device for control first.",
        )?;
        match result {
            Ok(session) => Ok(Self { node, session }),
            Err(error) => {
                node.close().await;
                Err(error.context("Cannot open File sharing. The host must trust this device for control and share the full device."))
            }
        }
    }

    pub fn cancel(&self) {
        self.session.close("File sharing closed");
    }

    pub async fn close(&self) {
        self.cancel();
        self.node.close().await;
    }

    async fn request(&self, request: Request) -> Result<(crate::streams::Streams, Reply)> {
        let (mut send, mut recv) = crate::streams::open(&self.session, KIND).await?;
        write(&mut send, &request).await?;
        let reply: Reply = tokio::time::timeout(Duration::from_secs(30), read(&mut recv))
            .await
            .context("The remote folder did not respond")??;
        if let Some(error) = &reply.error {
            bail!("{error}")
        }
        Ok(((send, recv), reply))
    }

    pub async fn list(&self, path: &str) -> Result<Listing> {
        self.request(Request::List { path: path.into() })
            .await?
            .1
            .listing
            .context("No directory listing was returned")
    }

    pub async fn mkdir(&self, path: &str) -> Result<()> {
        self.request(Request::Mkdir { path: path.into() }).await?;
        Ok(())
    }

    pub async fn upload(&self, local: &Path, remote_dir: &str) -> Result<String> {
        let (streams, _) = self
            .request(Request::Upload {
                path: remote_dir.into(),
            })
            .await?;
        tokio::select! {
            result = crate::transfer::send_on(streams, local) => result,
            _ = self.session.closed() => bail!("Transfer cancelled or disconnected"),
        }
    }

    pub async fn download(&self, remote: &str, local_dir: &Path) -> Result<PathBuf> {
        let (streams, _) = self
            .request(Request::Download {
                path: remote.into(),
            })
            .await?;
        tokio::select! {
            result = crate::transfer::receive(streams, local_dir.to_path_buf()) => result,
            _ = self.session.closed() => bail!("Transfer cancelled or disconnected"),
        }
    }

    /// Copy files and folders without following links. Existing files keep
    /// their bytes; the transfer protocol chooses an unused name for copies.
    pub async fn upload_tree(
        &self,
        local_root: &Path,
        source: &str,
        remote_dir: &str,
    ) -> Result<()> {
        let mut pending = vec![(source.to_string(), remote_dir.to_string(), 0)];
        let mut visited = 0;
        while let Some((source, destination, depth)) = pending.pop() {
            visited += 1;
            ensure!(
                depth <= 64 && visited <= 100_000,
                "Folder tree is too large or too deeply nested"
            );
            let path = resolve(local_root, &source)?;
            if path.is_dir() {
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .context("Invalid folder name")?;
                let target = join(&destination, name);
                self.mkdir(&target).await?;
                for entry in list(local_root, &source)?.entries.into_iter().rev() {
                    pending.push((entry.path, target.clone(), depth + 1));
                }
            } else {
                self.upload(&path, &destination).await?;
            }
        }
        Ok(())
    }

    pub async fn download_tree(
        &self,
        local_root: &Path,
        source: &str,
        local_dir: &str,
    ) -> Result<()> {
        let parent = source.rsplit_once('/').map_or("", |(parent, _)| parent);
        let entry = self
            .list(parent)
            .await?
            .entries
            .into_iter()
            .find(|entry| entry.path == source)
            .context("The remote file is no longer available")?;
        let mut pending = vec![(entry, local_dir.to_string(), 0)];
        let mut visited = 0;
        while let Some((entry, destination, depth)) = pending.pop() {
            visited += 1;
            ensure!(
                depth <= 64 && visited <= 100_000,
                "Folder tree is too large or too deeply nested"
            );
            if entry.is_dir {
                let target = join(&destination, &entry.name);
                mkdir(local_root, &target)?;
                for child in self.list(&entry.path).await?.entries.into_iter().rev() {
                    pending.push((child, target.clone(), depth + 1));
                }
            } else {
                self.download(&entry.path, &resolve(local_root, &destination)?)
                    .await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddr};

    fn local() -> frames_net::NodeConfig {
        frames_net::NodeConfig {
            relays: frames_net::Relays::Disabled,
            discovery: false,
            bind: Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))),
            ..Default::default()
        }
    }

    #[test]
    fn browsing_and_creation_stay_in_home_without_following_links() {
        let home = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join("Docs")).unwrap();
        std::fs::write(home.path().join("z.txt"), b"hello").unwrap();
        assert_eq!(list(home.path(), "").unwrap().entries[0].name, "Docs");
        assert_eq!(
            list(home.path(), "Docs").unwrap().parent.as_deref(),
            Some("")
        );
        for path in ["../", "/etc", "Docs/../", "C:/Windows", "Docs\\.."] {
            assert!(resolve(home.path(), path).is_err(), "{path}");
            assert!(mkdir(home.path(), path).is_err(), "{path}");
        }
        mkdir(home.path(), "Docs/Empty").unwrap();
        mkdir(home.path(), "Docs/Empty").unwrap();
        assert!(mkdir(home.path(), "z.txt").is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.path(), home.path().join("link")).unwrap();
            assert!(resolve(home.path(), "link").is_err());
            assert!(mkdir(home.path(), "link/escaped").is_err());
            assert!(
                !list(home.path(), "")
                    .unwrap()
                    .entries
                    .iter()
                    .any(|entry| entry.name == "link")
            );
        }
    }

    #[test]
    fn hidden_entries_include_dotfiles_and_system_attributes() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join(".env"), b"dotfile").unwrap();
        let flagged = home.path().join("flagged.txt");
        std::fs::write(&flagged, b"system hidden file").unwrap();
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_HIDDEN, SetFileAttributesW};
            let path: Vec<u16> = flagged.as_os_str().encode_wide().chain(Some(0)).collect();
            unsafe {
                SetFileAttributesW(windows::core::PCWSTR(path.as_ptr()), FILE_ATTRIBUTE_HIDDEN)
                    .unwrap();
            }
        }
        #[cfg(target_os = "macos")]
        {
            use std::os::unix::ffi::OsStrExt;
            let path = std::ffi::CString::new(flagged.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::chflags(path.as_ptr(), libc::UF_HIDDEN) }, 0);
        }
        let listing = list(home.path(), "").unwrap();
        assert!(
            listing
                .entries
                .iter()
                .find(|entry| entry.name == ".env")
                .unwrap()
                .is_hidden()
        );
        #[cfg(any(windows, target_os = "macos"))]
        assert!(
            listing
                .entries
                .iter()
                .find(|entry| entry.name == "flagged.txt")
                .unwrap()
                .is_hidden()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn copies_both_directions_checks_hashes_and_never_overwrites() {
        let home = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join("Documents")).unwrap();
        let bytes: Vec<u8> = (0..2_000_000).map(|i| (i % 251) as u8).collect();
        std::fs::write(source.path().join("sample.bin"), &bytes).unwrap();
        std::fs::write(home.path().join("Documents/sample.bin"), b"original").unwrap();
        let host = frames_net::Node::bind(&frames_net::DeviceKey::generate(), local())
            .await
            .unwrap();
        let viewer = frames_net::Node::bind(&frames_net::DeviceKey::generate(), local())
            .await
            .unwrap();
        let port = host
            .bound_sockets()
            .into_iter()
            .find(SocketAddr::is_ipv4)
            .unwrap()
            .port();
        let address = iroh::EndpointAddr::new(host.id())
            .with_ip_addr(SocketAddr::from((Ipv4Addr::LOCALHOST, port)));
        let (outgoing, incoming) = tokio::join!(viewer.connect(address), host.accept());
        let at_viewer = outgoing.unwrap();
        let at_host = incoming.unwrap().unwrap();
        let server = tokio::spawn(serve(at_host, home.path().into(), None));
        let (_, mut ready) = at_viewer.accept_channel().await.unwrap();
        let mut kind = [0];
        ready.read_exact(&mut kind).await.unwrap();
        assert_eq!(kind[0], KIND);
        let _: Reply = read(&mut ready).await.unwrap();
        let client = Client {
            node: viewer,
            session: at_viewer,
        };
        assert_eq!(client.list("").await.unwrap().entries[0].name, "Documents");
        let saved = client
            .upload(&source.path().join("sample.bin"), "Documents")
            .await
            .unwrap();
        assert_eq!(saved, "sample (2).bin");
        assert_eq!(
            std::fs::read(home.path().join("Documents/sample.bin")).unwrap(),
            b"original"
        );
        let downloaded = client
            .download("Documents/sample (2).bin", destination.path())
            .await
            .unwrap();
        assert_eq!(std::fs::read(downloaded).unwrap(), bytes);
        let long = format!("{}.pdf", "a".repeat(251));
        let mut names = vec![
            "Báo cáo cuối cùng 📁.pdf".to_string(),
            ".gitignore".into(),
            long.clone(),
        ];
        #[cfg(any(windows, target_os = "macos"))]
        names.extend([
            format!("{}.txt", "ế".repeat(200)),
            format!("{}.png", "📁".repeat(100)),
        ]);
        for name in &names {
            let content = format!("Contents of {name}");
            let original = source.path().join(name);
            std::fs::write(&original, content.as_bytes()).unwrap();
            assert_eq!(client.upload(&original, "Documents").await.unwrap(), *name);
            let copy = client
                .download(&format!("Documents/{name}"), destination.path())
                .await
                .unwrap();
            assert_eq!(copy.file_name().unwrap().to_str().unwrap(), name);
            assert_eq!(std::fs::read(copy).unwrap(), content.as_bytes());
        }
        let renamed = client
            .upload(&source.path().join(&long), "Documents")
            .await
            .unwrap();
        assert!(renamed.ends_with(" (2).pdf"));
        assert!(home.path().join("Documents").join(renamed).is_file());
        #[cfg(unix)]
        {
            let outside = source.path().join("outside.txt");
            std::fs::write(&outside, b"keep this file").unwrap();
            let hash = crate::transfer::sha256_of(&source.path().join("sample.bin"))
                .await
                .unwrap();
            let part = home
                .path()
                .join("Documents")
                .join(format!(".frames-{}.part", &hash[..32]));
            std::os::unix::fs::symlink(&outside, &part).unwrap();
            assert!(
                client
                    .upload(&source.path().join("sample.bin"), "Documents")
                    .await
                    .is_err()
            );
            assert_eq!(std::fs::read(&outside).unwrap(), b"keep this file");
            std::fs::remove_file(part).unwrap();
        }
        client.mkdir("Documents/Nested").await.unwrap();
        assert!(home.path().join("Documents/Nested").is_dir());
        assert!(client.list("../").await.is_err());
        assert!(
            client
                .download("Documents", destination.path())
                .await
                .is_err()
        );
        assert!(
            client
                .upload(&source.path().join("sample.bin"), "missing")
                .await
                .is_err()
        );
        std::fs::create_dir_all(source.path().join("Bundle/Nested/Empty")).unwrap();
        std::fs::write(
            source.path().join("Bundle/Nested/report.txt"),
            b"nested content",
        )
        .unwrap();
        client
            .upload_tree(source.path(), "Bundle", "Documents")
            .await
            .unwrap();
        assert!(home.path().join("Documents/Bundle/Nested/Empty").is_dir());
        client
            .download_tree(destination.path(), "Documents/Bundle", "")
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(destination.path().join("Bundle/Nested/report.txt")).unwrap(),
            b"nested content"
        );
        assert!(destination.path().join("Bundle/Nested/Empty").is_dir());
        client.close().await;
        server.await.unwrap().unwrap();
        host.close().await;
    }
}
