use mdns_sd::{ServiceDaemon, ServiceInfo};
pub const SERVICE: &str = "_frames._udp.local.";
pub fn advertise(name: &str, device_id: &str, port: u16) -> anyhow::Result<ServiceDaemon> {
    let daemon = ServiceDaemon::new()?;
    let host = format!("frames-{}.local.", &device_id[..device_id.len().min(8)]);
    let version = crate::PROTOCOL_VERSION.to_string();
    let properties = [("version", version.as_str()), ("device", device_id)];
    let info =
        ServiceInfo::new(SERVICE, name, &host, "", port, &properties[..])?.enable_addr_auto();
    daemon.register(info)?;
    Ok(daemon)
}
pub async fn browse(seconds: u64) -> anyhow::Result<Vec<serde_json::Value>> {
    let daemon = ServiceDaemon::new()?;
    let receiver = daemon.browse(SERVICE)?;
    let end = tokio::time::Instant::now() + std::time::Duration::from_secs(seconds);
    let mut services = std::collections::BTreeMap::new();
    while let Ok(Ok(event)) = tokio::time::timeout_at(end, receiver.recv_async()).await {
        if let mdns_sd::ServiceEvent::ServiceResolved(info) = event {
            services.insert(info.get_fullname().to_string(), serde_json::json!({"name":info.get_fullname(),"port":info.get_port(),"addresses":info.get_addresses().iter().map(ToString::to_string).collect::<Vec<_>>(),"device":info.get_property_val_str("device")}));
        }
    }
    daemon.shutdown()?;
    Ok(services.into_values().collect())
}
