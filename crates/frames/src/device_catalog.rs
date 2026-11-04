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
                // Probe every advertised address at once and keep the fastest.
                // Picking the first that answers chose by address order: a Mac
                // with Wi-Fi and Ethernet on one network was used over Wi-Fi,
                // where video bursts lost a quarter or more of their packets.
                let probes = futures_probe(&addresses, &id, trusted).await;
                if let Some((address, probe)) = fastest(probes) {
                    device.name = probe.name;
                    device.fingerprint = probe.device_id;
                    device.address = address.to_string();
                    device.online = true;
                    let _ =
                        crate::connection_history::update_discovered(&id, &device.name, address);
                    if let Some(bytes) = probe.png {
                        // Discovery refreshes the clear preview only. The blurred copy
                        // always comes from the last accepted interactive session.
                        if let Ok(path) = crate::connection_history::preview_path(&id, false) {
                            device.preview_live =
                                crate::connection_history::write_atomic(path, &bytes).is_ok();
                        }
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

/// Probes up to four addresses of one device concurrently.
async fn futures_probe(
    addresses: &[SocketAddr],
    id: &str,
    trusted: bool,
) -> Vec<(SocketAddr, crate::session::DeviceProbe)> {
    let mut probes = tokio::task::JoinSet::new();
    for (order, &address) in addresses.iter().take(4).enumerate() {
        let id = id.to_string();
        probes.spawn(async move {
            (
                order,
                address,
                crate::session::probe_device(address, &id, trusted).await,
            )
        });
    }
    let mut answered = Vec::new();
    while let Some(result) = probes.join_next().await {
        if let Ok((order, address, Ok(probe))) = result {
            answered.push((order, address, probe));
        }
    }
    answered.sort_by_key(|(order, ..)| *order);
    answered
        .into_iter()
        .map(|(_, address, probe)| (address, probe))
        .collect()
}

/// The answering address with the lowest round-trip time. Differences under
/// half a millisecond are noise, so the earlier address (IPv4 first) wins them.
fn fastest<T: HasRtt>(answered: Vec<(SocketAddr, T)>) -> Option<(SocketAddr, T)> {
    const NOISE: std::time::Duration = std::time::Duration::from_micros(500);
    let mut best: Option<(SocketAddr, T)> = None;
    for (address, probe) in answered {
        match &best {
            Some((_, current)) if probe.rtt() + NOISE >= current.rtt() => {}
            _ => best = Some((address, probe)),
        }
    }
    best
}

trait HasRtt {
    fn rtt(&self) -> std::time::Duration;
}
impl HasRtt for crate::session::DeviceProbe {
    fn rtt(&self) -> std::time::Duration {
        self.rtt
    }
}

#[cfg(test)]
mod address_tests {
    use super::*;
    use std::time::Duration;
    struct Probe(Duration);
    impl HasRtt for Probe {
        fn rtt(&self) -> Duration {
            self.0
        }
    }
    fn at(ip: &str) -> SocketAddr {
        SocketAddr::new(ip.parse().unwrap(), 47990)
    }
    #[test]
    fn a_dual_homed_host_is_used_through_its_fastest_address() {
        // Wi-Fi sorts first by address but answers slower than Ethernet.
        let wifi = (at("192.168.0.131"), Probe(Duration::from_millis(5)));
        let wired = (at("192.168.0.169"), Probe(Duration::from_micros(400)));
        assert_eq!(fastest(vec![wifi, wired]).unwrap().0, at("192.168.0.169"));
    }
    #[test]
    fn near_equal_round_trips_keep_the_earlier_address() {
        let first = (at("192.168.0.20"), Probe(Duration::from_micros(700)));
        let second = (at("192.168.0.30"), Probe(Duration::from_micros(500)));
        assert_eq!(fastest(vec![first, second]).unwrap().0, at("192.168.0.20"));
        assert!(fastest::<Probe>(vec![]).is_none());
    }
}
