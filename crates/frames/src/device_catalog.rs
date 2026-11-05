use super::*;
use std::collections::BTreeSet;

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

/// Ids of the devices allowed to connect to this one (v2 trust list).
fn trusted_ids() -> Result<BTreeSet<String>> {
    Ok(frames_next::directory::trusted()?
        .into_iter()
        .map(|device| device.id)
        .collect())
}

fn decorate(
    mut device: Device,
    trusted: &BTreeSet<String>,
    history: &[crate::connection_history::Connection],
    preferences: &Preferences,
    saved: &crate::connection_preferences::Devices,
) -> Device {
    let choices = saved.get(&device.fingerprint);
    device.connection_audio = choices.map_or(preferences.options.audio, |p| p.audio);
    device.connection_desktop = choices.map_or(preferences.options.desktop, |p| p.desktop);
    device.trusted = trusted.contains(&device.fingerprint);
    device.can_trust = !device.trusted && history.iter().any(|c| c.device_id == device.fingerprint);
    device.preview = crate::connection_history::preview_path(&device.fingerprint, !device.trusted)
        .ok()
        .and_then(|path| fs::read(path).ok())
        .unwrap_or_default();
    device.preview_live &= device.trusted && device.online;
    device
}

pub(super) fn nearby(devices: &[Device], prefs: &Preferences) -> Result<Vec<Device>> {
    let trusted = trusted_ids()?;
    let history = crate::connection_history::load()?;
    let saved = crate::connection_preferences::load()?;
    Ok(devices
        .iter()
        .filter(|d| d.online)
        .cloned()
        .map(|d| decorate(d, &trusted, &history, prefs, &saved))
        .collect())
}

pub(super) fn rows(prefs: &Preferences, devices: &[Device]) -> Result<(Vec<Device>, Vec<Device>)> {
    let trusted_set = trusted_ids()?;
    let history = crate::connection_history::load()?;
    let saved = crate::connection_preferences::load()?;
    let mut recent = Vec::new();
    // Devices met through v2: dialled by id. Older entries (v1 addresses)
    // stay listed until the device is seen again.
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
        recent.push(decorate(device, &trusted_set, &history, prefs, &saved));
    }
    for old in recent_rows(prefs) {
        if !recent.iter().any(|d| d.address == old.address) {
            let device = devices
                .iter()
                .find(|d| d.address == old.address)
                .cloned()
                .unwrap_or(old);
            recent.push(decorate(device, &trusted_set, &history, prefs, &saved));
        }
    }
    recent.truncate(20);
    let mut trusted = Vec::new();
    for mut device in trusted_rows()? {
        if let Some(found) = devices.iter().find(|d| d.fingerprint == device.fingerprint) {
            let detail = device.detail;
            device = found.clone();
            device.detail = detail;
        } else if let Some(item) = history.iter().find(|h| h.device_id == device.fingerprint) {
            device.name = item.name.clone();
        }
        trusted.push(decorate(device, &trusted_set, &history, prefs, &saved));
    }
    Ok((recent, trusted))
}

/// Hosts sharing on this network (v2 local discovery), dialled by id.
pub(super) fn scan() -> Result<Vec<Device>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let found = runtime.block_on(frames_next::directory::nearby(Duration::from_secs(3)))?;
    let trusted = trusted_ids()?;
    let mut devices: Vec<Device> = found
        .into_iter()
        .map(|nearby| {
            let _ =
                crate::connection_history::update_discovered(&nearby.id, &nearby.name, &nearby.id);
            Device {
                name: nearby.name,
                address: nearby.id.clone(),
                trusted: trusted.contains(&nearby.id),
                fingerprint: nearby.id,
                online: true,
                addresses: nearby.addresses,
                ..Default::default()
            }
        })
        .collect();
    devices.sort_by(|a, b| a.name.cmp(&b.name).then(a.fingerprint.cmp(&b.fingerprint)));
    Ok(devices)
}
