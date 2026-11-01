use mdns_sd::{ServiceDaemon, ServiceInfo};
pub const SERVICE: &str = "_frames._udp.local.";
pub fn advertise(name: &str, device_id: &str, port: u16) -> anyhow::Result<ServiceDaemon> {
    let info = service_info(name, device_id, port)?;
    let daemon = ServiceDaemon::new()?;
    daemon.register(info)?;
    Ok(daemon)
}

fn service_info(name: &str, device_id: &str, port: u16) -> anyhow::Result<ServiceInfo> {
    let name = if name.trim().is_empty() {
        "Frames device"
    } else {
        name.trim()
    };
    let short_id = utf8_prefix(device_id, 8);
    let host = format!("frames-{short_id}.local.");
    // Keep same-named computers distinct and the DNS label within 63 bytes.
    // The TXT property preserves punctuation and longer names for display.
    let suffix = format!(" ({short_id})");
    let label = name.replace('.', "-");
    let instance = format!("{}{suffix}", utf8_prefix(&label, 63 - suffix.len()));
    let version = crate::PROTOCOL_VERSION.to_string();
    let properties = [
        ("version", version.as_str()),
        ("device", device_id),
        ("name", utf8_prefix(name, 250)),
        ("preview", "1"),
    ];
    Ok(ServiceInfo::new(SERVICE, &instance, &host, "", port, &properties[..])?.enable_addr_auto())
}

fn utf8_prefix(value: &str, max_bytes: usize) -> &str {
    let mut end = value.len().min(max_bytes);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn display_name<'a>(fullname: &'a str, name: Option<&'a str>) -> &'a str {
    name.filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| {
            fullname
                .strip_suffix(SERVICE)
                .and_then(|name| name.strip_suffix('.'))
                .unwrap_or(fullname)
        })
}
pub async fn browse(seconds: u64) -> anyhow::Result<Vec<serde_json::Value>> {
    let daemon = ServiceDaemon::new()?;
    let receiver = daemon.browse(SERVICE)?;
    let end = tokio::time::Instant::now() + std::time::Duration::from_secs(seconds);
    let mut services = std::collections::BTreeMap::new();
    while let Ok(Ok(event)) = tokio::time::timeout_at(end, receiver.recv_async()).await {
        if let mdns_sd::ServiceEvent::ServiceResolved(info) = event {
            services.insert(
                info.get_fullname().to_string(),
                serde_json::json!({
                    "name": display_name(info.get_fullname(), info.get_property_val_str("name")),
                    "port": info.get_port(),
                    "addresses": info.get_addresses().iter().map(ToString::to_string).collect::<Vec<_>>(),
                    "device": info.get_property_val_str("device"),
                    "preview": info.get_property_val_str("preview") == Some("1"),
                }),
            );
        }
    }
    daemon.shutdown()?;
    Ok(services.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertisement_keeps_the_computer_name_and_device_identity() {
        let name = "Linh’s Mac mini.local";
        let info = service_info(name, "12345678abcdef", 47990).unwrap();
        assert_eq!(info.get_property_val_str("name"), Some(name));
        assert_eq!(info.get_property_val_str("device"), Some("12345678abcdef"));
        assert_eq!(info.get_hostname(), "frames-12345678.local.");
        assert_eq!(info.get_port(), 47990);
        assert_eq!(
            info.get_fullname(),
            "Linh’s Mac mini-local (12345678)._frames._udp.local."
        );
        assert_eq!(
            display_name(info.get_fullname(), info.get_property_val_str("name")),
            name
        );
        let other = service_info(name, "87654321abcdef", 47990).unwrap();
        assert_ne!(info.get_fullname(), other.get_fullname());
    }

    #[test]
    fn long_unicode_names_fit_dns_and_txt_limits() {
        let info = service_info(&"電".repeat(100), "12345678abcdef", 47990).unwrap();
        let instance = info.get_fullname().strip_suffix(SERVICE).unwrap();
        assert!(instance.trim_end_matches('.').len() <= 63);
        let name = info.get_property_val_str("name").unwrap();
        assert_eq!(name, "電".repeat(83));
        assert!(name.len() + "name=".len() <= 255);
    }

    #[test]
    fn discovery_falls_back_for_older_hosts_without_a_name_property() {
        let fullname = "Frames-12345678._frames._udp.local.";
        assert_eq!(display_name(fullname, None), "Frames-12345678");
        assert_eq!(display_name(fullname, Some("  ")), "Frames-12345678");
        // A computer name that happens to end in the service suffix is literal.
        assert_eq!(display_name(fullname, Some(fullname)), fullname);
    }
}
