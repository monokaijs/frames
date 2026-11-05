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

/// Whether `text` is a full device id (64 hex digits).
pub fn is_device_id(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit())
}
