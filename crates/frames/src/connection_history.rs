use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    net::SocketAddr,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Connection {
    pub device_id: String,
    pub name: String,
    pub address: String,
    pub connected: u64,
    #[serde(default)]
    pub incoming: bool,
}

pub fn load() -> Result<Vec<Connection>> {
    let path = frames_core::trust::config_dir()?.join("connections.json");
    match fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

pub fn remember(address: SocketAddr, device_id: &str, name: &str, incoming: bool) -> Result<()> {
    let root = frames_core::trust::config_dir()?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("connections.lock"))?;
    fs2::FileExt::lock_exclusive(&lock)?;
    let mut connections = load()?;
    let incoming = incoming
        || connections
            .iter()
            .any(|item| item.device_id == device_id && item.incoming);
    connections.retain(|item| item.device_id != device_id);
    connections.insert(
        0,
        Connection {
            device_id: device_id.into(),
            name: name.into(),
            address: address.to_string(),
            connected: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
            incoming,
        },
    );
    connections.truncate(20);
    write_atomic(
        root.join("connections.json"),
        &serde_json::to_vec_pretty(&connections)?,
    )
}

pub fn clear() -> Result<()> {
    let root = frames_core::trust::config_dir()?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("connections.lock"))?;
    fs2::FileExt::lock_exclusive(&lock)?;
    for connection in load()? {
        for blurred in [false, true] {
            if let Ok(path) = preview_path(&connection.device_id, blurred) {
                let _ = fs::remove_file(path);
            }
        }
    }
    write_atomic(root.join("connections.json"), b"[]")
}

pub fn update_discovered(device_id: &str, name: &str, address: SocketAddr) -> Result<()> {
    let root = frames_core::trust::config_dir()?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("connections.lock"))?;
    fs2::FileExt::lock_exclusive(&lock)?;
    let mut connections = load()?;
    if let Some(item) = connections
        .iter_mut()
        .find(|item| item.device_id == device_id)
    {
        let address = address.to_string();
        if item.name != name || item.address != address {
            item.name = name.into();
            item.address = address;
            write_atomic(
                root.join("connections.json"),
                &serde_json::to_vec_pretty(&connections)?,
            )?;
        }
    }
    Ok(())
}

pub fn preview_path(device_id: &str, blurred: bool) -> Result<PathBuf> {
    anyhow::ensure!(
        device_id.len() == 64 && device_id.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid device fingerprint"
    );
    let root = frames_core::trust::config_dir()?.join("previews");
    fs::create_dir_all(&root)?;
    Ok(root.join(format!(
        "{device_id}{}.png",
        if blurred { "-blurred" } else { "" }
    )))
}

pub fn write_atomic(path: PathBuf, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    fs::write(&temporary, bytes)?;
    fs::rename(temporary, path)?;
    Ok(())
}
