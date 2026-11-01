use super::*;
use std::collections::BTreeMap;

pub(super) fn image(bytes: &[u8]) -> slint::Image {
    let Ok(image) = crate::device_preview::decode(bytes) else {
        return slint::Image::default();
    };
    let rgba = image.to_rgba8();
    slint::Image::from_rgba8(
        slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
            rgba.as_raw(),
            rgba.width(),
            rgba.height(),
        ),
    )
}

fn decorate(
    mut device: Device,
    store: &TrustStore,
    history: &[crate::connection_history::Connection],
    preferences: &Preferences,
    saved: &crate::connection_preferences::Devices,
) -> Device {
    let choices = saved.get(&device.fingerprint);
    device.connection_audio = choices.map_or(preferences.options.audio, |p| p.audio);
    device.connection_desktop = choices.map_or(preferences.options.desktop, |p| p.desktop);
    device.trusted = store.peers.contains_key(&device.fingerprint);
    device.can_trust = !device.trusted && history.iter().any(|c| c.device_id == device.fingerprint);
    device.preview = crate::connection_history::preview_path(&device.fingerprint, !device.trusted)
        .ok()
        .and_then(|path| fs::read(path).ok())
        .unwrap_or_default();
    device.preview_live &= device.trusted && device.online;
    device
}

pub(super) fn nearby(devices: &[Device], prefs: &Preferences) -> Result<Vec<Device>> {
    let store = TrustStore::load()?;
    let history = crate::connection_history::load()?;
    let saved = crate::connection_preferences::load()?;
    Ok(devices
        .iter()
        .filter(|d| d.online)
        .cloned()
        .map(|d| decorate(d, &store, &history, prefs, &saved))
        .collect())
}

pub(super) fn rows(prefs: &Preferences, devices: &[Device]) -> Result<(Vec<Device>, Vec<Device>)> {
    let store = TrustStore::load()?;
    let history = crate::connection_history::load()?;
    let saved = crate::connection_preferences::load()?;
    let mut recent = Vec::new();
    for item in &history {
        let device = devices
            .iter()
            .find(|d| d.fingerprint == item.device_id)
            .cloned()
            .unwrap_or_else(|| Device {
                name: item.name.clone(),
                address: item.address.clone(),
                fingerprint: item.device_id.clone(),
                ..Default::default()
            });
        recent.push(decorate(device, &store, &history, prefs, &saved));
    }
    for old in recent_rows(prefs) {
        if !recent.iter().any(|d| d.address == old.address) {
            let device = devices
                .iter()
                .find(|d| d.address == old.address)
                .cloned()
                .unwrap_or(old);
            recent.push(decorate(device, &store, &history, prefs, &saved));
        }
    }
    recent.truncate(20);
    let mut trusted = Vec::new();
    for mut device in trusted_rows()? {
        if let Some(item) = history.iter().find(|h| h.device_id == device.fingerprint) {
            device.address = item.address.clone();
        }
        if let Some(found) = devices.iter().find(|d| d.fingerprint == device.fingerprint) {
            let detail = device.detail;
            device = found.clone();
            device.detail = detail;
        }
        trusted.push(decorate(device, &store, &history, prefs, &saved));
    }
    Ok((recent, trusted))
}

pub(super) fn scan() -> Result<Vec<Device>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let local = frames_core::trust::Identity::load()?.device_id;
        let store = TrustStore::load()?;
        let history = crate::connection_history::load()?;
        let mut candidates = BTreeMap::<String, (String, Vec<SocketAddr>)>::new();
        for service in frames_core::discovery::browse(3).await? {
            let Some(id) = service["device"].as_str().filter(|id| {
                id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()) && *id != local
            }) else {
                continue;
            };
            let Some(port) = service["port"].as_u64().and_then(|p| u16::try_from(p).ok()) else {
                continue;
            };
            let addresses = service["addresses"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|ip| ip.as_str()?.parse::<IpAddr>().ok())
                .map(|ip| SocketAddr::new(ip, port))
                .collect();
            candidates.insert(
                id.into(),
                (
                    service["name"].as_str().unwrap_or("Remote device").into(),
                    addresses,
                ),
            );
        }
        for item in &history {
            if item.device_id == local {
                continue;
            }
            let candidate = candidates
                .entry(item.device_id.clone())
                .or_insert_with(|| (item.name.clone(), Vec::new()));
            if let Ok(address) = item.address.parse() {
                candidate.1.push(address);
            }
        }
        let slots = Arc::new(tokio::sync::Semaphore::new(4));
        let mut tasks = tokio::task::JoinSet::new();
        for (id, (name, mut addresses)) in candidates.into_iter().take(64) {
            addresses.sort_by_key(|a| (a.is_ipv6(), a.to_string()));
            addresses.dedup();
            let trusted = store.peers.contains_key(&id);
            let slots = slots.clone();
            tasks.spawn(async move {
                let _permit = slots.acquire_owned().await.ok();
                let mut device = Device {
                    name,
                    address: addresses
                        .first()
                        .map(ToString::to_string)
                        .unwrap_or_default(),
                    fingerprint: id.clone(),
                    trusted,
                    ..Default::default()
                };
                for address in addresses.into_iter().take(2) {
                    if let Ok(probe) = crate::session::probe_device(address, &id, trusted).await {
                        device.name = probe.name;
                        device.fingerprint = probe.device_id;
                        device.address = address.to_string();
                        device.online = true;
                        let _ = crate::connection_history::update_discovered(
                            &id,
                            &device.name,
                            address,
                        );
                        if let Some(bytes) = probe.png {
                            // Discovery refreshes the clear preview only. The blurred copy
                            // always comes from the last accepted interactive session.
                            if let Ok(path) = crate::connection_history::preview_path(&id, false) {
                                device.preview_live =
                                    crate::connection_history::write_atomic(path, &bytes).is_ok();
                            }
                        }
                        break;
                    }
                }
                device
            });
        }
        let mut devices = Vec::new();
        while let Some(result) = tasks.join_next().await {
            if let Ok(device) = result {
                devices.push(device);
            }
        }
        devices.sort_by(|a, b| a.name.cmp(&b.name).then(a.fingerprint.cmp(&b.fingerprint)));
        Ok(devices)
    })
}

use std::sync::Arc;
