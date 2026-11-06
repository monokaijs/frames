//! Devices for the Frames launcher: this device's id, hosts sharing on this
//! network, and the trust list (`trusted-v2.json` in the config directory,
//! `FRAMES_CONFIG_DIR` overriding it, as for the session processes).

use crate::admit;
use anyhow::{Context, Result};
use frames_net::trust::{TrustList, TrustedDevice};
use std::net::IpAddr;
use std::time::Duration;

/// A host sharing on this network.
#[derive(Clone, Debug)]
pub struct Nearby {
    pub id: String,
    pub name: String,
    pub addresses: Vec<IpAddr>,
}

/// A device allowed to connect to this one.
#[derive(Clone, Debug)]
pub struct Trusted {
    pub id: String,
    pub name: String,
    pub control: bool,
}

/// This device's id (created on first use), as other devices dial it.
pub fn own_id() -> Result<String> {
    let config = admit::config_dir(None)?;
    Ok(admit::device_key(&config)?.id().to_string())
}

/// Hosts that answer within `wait`, without this device.
pub async fn nearby(wait: Duration) -> Result<Vec<Nearby>> {
    let own = own_id()?;
    Ok(frames_net::local::browse(wait)
        .await?
        .into_iter()
        .filter(|device| device.id.to_string() != own)
        .map(|device| Nearby {
            id: device.id.to_string(),
            name: device.name,
            addresses: device.addr.ip_addrs().map(|a| a.ip()).collect(),
        })
        .collect())
}

fn list() -> Result<(TrustList, std::path::PathBuf)> {
    let path = TrustList::path_in(&admit::config_dir(None)?);
    Ok((TrustList::load(&path)?, path))
}

fn endpoint(id: &str) -> Result<iroh::EndpointId> {
    id.parse().context("not a device id")
}

pub fn trusted() -> Result<Vec<Trusted>> {
    Ok(list()?
        .0
        .devices()
        .map(|(id, device)| Trusted {
            id: id.to_string(),
            name: device.name.clone(),
            control: device.allow_control,
        })
        .collect())
}

/// Saves `id` as trusted, keeping a v1 alias it already had.
pub fn trust(id: &str, name: &str, control: bool) -> Result<()> {
    let device = endpoint(id)?;
    let (mut list, path) = list()?;
    let legacy_fingerprint = list.get(&device).and_then(|d| d.legacy_fingerprint.clone());
    list.insert(
        &device,
        TrustedDevice {
            name: name.to_string(),
            allow_control: control,
            legacy_fingerprint,
        },
    );
    list.save(&path)
}

/// Removes `id`; false when it was not trusted.
pub fn revoke(id: &str) -> Result<bool> {
    let device = endpoint(id)?;
    let (mut list, path) = list()?;
    let removed = list.remove(&device);
    if removed {
        list.save(&path)?;
    }
    Ok(removed)
}

/// Sets this device's access password: devices that give it connect
/// without anyone approving them here (`crate::password`).
pub fn set_access_password(password: &str) -> Result<()> {
    let config = admit::config_dir(None)?;
    let id = admit::device_key(&config)?.id();
    crate::password::set(&config, &id, password)
}

pub fn clear_access_password() -> Result<()> {
    crate::password::clear(&admit::config_dir(None)?)
}

pub fn access_password_set() -> bool {
    admit::config_dir(None).is_ok_and(|config| crate::password::is_set(&config))
}

/// This device's Quick Apps (`crate::quick_apps`), as the launcher's
/// Settings edit them.
pub use crate::quick_apps::{Listed as RemoteQuickApp, QuickApp};

pub fn quick_apps() -> Result<Vec<QuickApp>> {
    crate::quick_apps::load(&admit::config_dir(None)?)
}

/// Replaces this device's Quick Apps (ids, names and paths are checked).
pub fn save_quick_apps(apps: &[QuickApp]) -> Result<()> {
    crate::quick_apps::save(&admit::config_dir(None)?, apps)
}

/// A trusted host's Quick Apps (untrusted devices are refused without
/// anyone being asked). Gives up after `wait`.
pub async fn remote_quick_apps(id: &str, wait: Duration) -> Result<Vec<RemoteQuickApp>> {
    crate::quick_apps::remote(id, wait).await
}

/// Where to reach host `id` (a full id or an unambiguous start of one):
/// its LAN addresses when it is nearby, else by id (relay and discovery).
pub async fn target(id: &str) -> Result<iroh::EndpointAddr> {
    Ok(
        match frames_net::local::find(id, Duration::from_millis(1500))
            .await
            .ok()
            .flatten()
        {
            Some(nearby) => nearby.addr,
            None => endpoint(id)?.into(),
        },
    )
}

/// A PNG preview of a trusted host's screen (`crate::preview`); the host
/// refuses devices it does not trust. Gives up after `wait`.
pub async fn preview(id: &str, wait: Duration) -> Result<Vec<u8>> {
    let config = admit::config_dir(None)?;
    let key = admit::device_key(&config)?;
    let fetch = async {
        let node = frames_net::Node::bind(&key, frames_net::NodeConfig::default()).await?;
        let session = node.connect(target(id).await?).await?;
        admit::send_hello(
            &session,
            &node.id(),
            false,
            admit::Asks {
                decoders: Vec::new(),
                window: None,
                name: String::new(),
                audio: false,
                max_mbps: None,
                catalog: false,
                window_id: None,
                password: None,
                preview: true,
                app: None,
                only_app: false,
                quick_apps: false,
            },
        )
        .await?;
        let png = crate::preview::receive(&session).await;
        session.close("preview received");
        node.close().await;
        png
    };
    tokio::time::timeout(wait, fetch)
        .await
        .context("no preview in time")?
}

/// This device's short code: the first 8 characters of its id, as
/// "9710-3C89", to read out or type. Devices on the same network find it
/// by that prefix; across the internet the full id is needed.
pub fn own_code() -> Result<String> {
    Ok(short_code(&own_id()?))
}

/// The short code of a device id.
pub fn short_code(id: &str) -> String {
    let code = id.get(..8).unwrap_or(id).to_ascii_uppercase();
    format!(
        "{}-{}",
        &code[..4.min(code.len())],
        &code[4.min(code.len())..]
    )
}

/// What someone typed as a device code or id, without the spaces and
/// dashes a code is shown with, in lower case.
pub fn normalize_code(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect::<String>()
        .to_ascii_lowercase()
}

/// This computer's address on its local network (the one routes leave
/// by), for connecting on the same network.
pub fn local_ip() -> Option<IpAddr> {
    // Connecting a UDP socket picks a route without sending anything.
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    Some(socket.local_addr().ok()?.ip())
}

/// Puts `text` on this computer's clipboard (for Copy ID).
pub fn copy_text(text: &str) -> Result<()> {
    use frames_clipboard::{Clipboard, Content};
    #[cfg(target_os = "macos")]
    let mut clipboard = frames_platform_macos::clipboard::MacClipboard;
    #[cfg(windows)]
    let mut clipboard = frames_platform_windows::clipboard::WindowsClipboard;
    #[cfg(not(any(windows, target_os = "macos")))]
    let mut clipboard = frames_clipboard::MemoryClipboard::default();
    clipboard
        .write(&Content::Text(text.to_string()))
        .map_err(anyhow::Error::msg)
}

/// Whether `text` is a full device id (64 hex digits).
pub fn is_device_id(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit())
}
