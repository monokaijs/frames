//! Nearby v2 devices on the local network (mDNS), so a viewer can find a
//! host and connect to it directly without internet discovery.
//!
//! Service `_frames2._udp.local.` (v1 uses `_frames._udp`): the port is the
//! node's IPv4 socket, TXT `id` is the device id, `name` its display name,
//! and `port6` the IPv6 socket's port when there is one.

use anyhow::{Context, Result};
use iroh::{EndpointAddr, EndpointId, TransportAddr};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

pub const SERVICE: &str = "_frames2._udp.local.";

/// Keeps the advertisement up; it ends when dropped.
pub struct Advertisement {
    daemon: ServiceDaemon,
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        let _ = self.daemon.shutdown();
    }
}

fn prefix(value: &str, max_bytes: usize) -> &str {
    let mut end = value.len().min(max_bytes);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

/// Advertises this node on the local network.
pub fn advertise(node: &crate::Node, name: &str) -> Result<Advertisement> {
    let sockets = node.bound_sockets();
    let port4 = sockets
        .iter()
        .find(|s| s.is_ipv4())
        .map(SocketAddr::port)
        .context("no IPv4 socket to advertise")?;
    let port6 = sockets.iter().find(|s| s.is_ipv6()).map(SocketAddr::port);
    let id = node.id().to_string();
    let short = &id[..8];
    let name = if name.trim().is_empty() {
        "Frames device"
    } else {
        name.trim()
    };
    let suffix = format!(" ({short})");
    let instance = format!(
        "{}{suffix}",
        prefix(&name.replace('.', "-"), 63 - suffix.len())
    );
    let host = format!("frames2-{short}.local.");
    let port6 = port6.map(|p| p.to_string()).unwrap_or_default();
    let properties = [
        ("id", id.as_str()),
        ("name", prefix(name, 250)),
        ("port6", port6.as_str()),
    ];
    let info =
        ServiceInfo::new(SERVICE, &instance, &host, "", port4, &properties[..])?.enable_addr_auto();
    let daemon = ServiceDaemon::new()?;
    daemon.register(info)?;
    Ok(Advertisement { daemon })
}

/// Whether an advertised address can be dialed from another machine. IPv6
/// link-local addresses (`fe80::/10`) come without the interface they
/// belong to, so they cannot. A Mac advertises a dozen of them, and dialing
/// them along with the usable addresses kept the connection from forming.
/// The IPv4 and unique-local or global IPv6 addresses reach the same host.
fn dialable(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !v4.is_unspecified() && !v4.is_multicast(),
        IpAddr::V6(v6) => {
            (v6.segments()[0] & 0xffc0) != 0xfe80 && !v6.is_unspecified() && !v6.is_multicast()
        }
    }
}

/// A device seen on the local network.
#[derive(Clone, Debug)]
pub struct Nearby {
    pub id: EndpointId,
    pub name: String,
    pub addr: EndpointAddr,
}

/// Listens for `duration` and returns the devices that answered.
pub async fn browse(duration: Duration) -> Result<Vec<Nearby>> {
    browse_until(duration, |_| false).await
}

/// The nearby device whose id starts with `prefix`, as soon as it answers
/// (or `None` after `timeout`). Ambiguous prefixes wait out the timeout and
/// return `None` unless exactly one device matched.
pub async fn find(prefix: &str, timeout: Duration) -> Result<Option<Nearby>> {
    let found = browse_until(timeout, |device| {
        device.id.to_string().starts_with(prefix) && prefix.len() >= 64
    })
    .await?;
    let mut matching = found
        .into_iter()
        .filter(|d| d.id.to_string().starts_with(prefix));
    match (matching.next(), matching.next()) {
        (Some(device), None) => Ok(Some(device)),
        _ => Ok(None),
    }
}

async fn browse_until(duration: Duration, done: impl Fn(&Nearby) -> bool) -> Result<Vec<Nearby>> {
    let daemon = ServiceDaemon::new()?;
    let receiver = daemon.browse(SERVICE)?;
    let end = tokio::time::Instant::now() + duration;
    let mut found = std::collections::BTreeMap::new();
    while let Ok(Ok(event)) = tokio::time::timeout_at(end, receiver.recv_async()).await {
        let ServiceEvent::ServiceResolved(info) = event else {
            continue;
        };
        let Some(id) = info
            .get_property_val_str("id")
            .and_then(|id| id.parse::<EndpointId>().ok())
        else {
            continue;
        };
        let port6 = info
            .get_property_val_str("port6")
            .and_then(|p| p.parse::<u16>().ok());
        let addrs: Vec<TransportAddr> = info
            .get_addresses()
            .iter()
            .map(|ip| ip.to_ip_addr())
            .filter(|ip| dialable(*ip))
            .filter_map(|ip| {
                let port = if ip.is_ipv4() {
                    Some(info.get_port())
                } else {
                    port6
                };
                port.map(|port| TransportAddr::Ip(SocketAddr::new(ip, port)))
            })
            .collect();
        let name = info
            .get_property_val_str("name")
            .unwrap_or("Frames device")
            .to_string();
        let device = Nearby {
            id,
            name,
            addr: EndpointAddr::from_parts(id, addrs),
        };
        let stop = done(&device);
        found.insert(id, device);
        if stop {
            break;
        }
    }
    let _ = daemon.shutdown();
    Ok(found.into_values().collect())
}

#[cfg(test)]
mod dial_tests {
    use super::dialable;

    #[test]
    fn link_local_ipv6_is_not_dialed_but_the_lan_addresses_are() {
        for usable in [
            "192.168.0.169",
            "192.168.0.131",
            "127.0.0.1",
            "169.254.10.20",
            "fd67:7ef9:5384:4ee7:1c0f:36a6:44b2:ee81",
            "2001:db8::1",
            "::1",
        ] {
            assert!(dialable(usable.parse().unwrap()), "{usable}");
        }
        for unusable in [
            "fe80::1",
            "fe80::1449:4e94:c775:d0a8",
            "febf::1",
            "::",
            "0.0.0.0",
            "ff02::fb",
        ] {
            assert!(!dialable(unusable.parse().unwrap()), "{unusable}");
        }
    }
}
