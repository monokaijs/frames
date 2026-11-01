/// Use the system's computer name even when launched without shell environment
/// variables (for example, from Finder or as a Windows service).
pub fn device_name() -> String {
    device_name_override()
        .ok()
        .flatten()
        .unwrap_or_else(system_device_name)
}

pub fn system_device_name() -> String {
    system_name()
        .and_then(usable_name)
        .or_else(|| std::env::var("COMPUTERNAME").ok().and_then(usable_name))
        .or_else(|| std::env::var("HOSTNAME").ok().and_then(usable_name))
        .unwrap_or_else(|| "Frames device".into())
}

pub fn device_name_override() -> anyhow::Result<Option<String>> {
    let path = frames_core::trust::config_dir()?.join("device-name.json");
    match std::fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice::<Option<String>>(&bytes)?.and_then(usable_name)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub fn set_device_name(name: &str) -> anyhow::Result<()> {
    let name = name.trim();
    anyhow::ensure!(name.len() <= 250, "Use a device name of at most 250 bytes.");
    anyhow::ensure!(
        !name.chars().any(char::is_control),
        "Device names cannot contain control characters."
    );
    let path = frames_core::trust::config_dir()?.join("device-name.json");
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let value = if name.is_empty() { None } else { Some(name) };
    std::fs::write(&temporary, serde_json::to_vec(&value)?)?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

fn usable_name(name: String) -> Option<String> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    // Leave room for "name=" in a 255-byte mDNS TXT entry, and stay within
    // the protocol hello's 256-byte name limit without splitting UTF-8.
    let mut end = name.len().min(250);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    Some(name[..end].to_owned())
}

#[cfg(windows)]
fn system_name() -> Option<String> {
    use windows::Win32::System::SystemInformation::{
        ComputerNamePhysicalDnsHostname, GetComputerNameExW,
    };
    use windows::core::PWSTR;

    let mut length = 0;
    unsafe {
        let _ = GetComputerNameExW(ComputerNamePhysicalDnsHostname, None, &mut length);
        if length == 0 {
            return None;
        }
        let mut buffer = vec![0; length as usize];
        GetComputerNameExW(
            ComputerNamePhysicalDnsHostname,
            Some(PWSTR(buffer.as_mut_ptr())),
            &mut length,
        )
        .ok()?;
        String::from_utf16(&buffer[..length as usize]).ok()
    }
}

#[cfg(target_os = "macos")]
fn system_name() -> Option<String> {
    // ComputerName is the friendly name in System Settings, whereas HostName
    // can be unset or supplied by the network.
    ["ComputerName", "LocalHostName"]
        .into_iter()
        .find_map(|key| {
            let output = std::process::Command::new("/usr/sbin/scutil")
                .args(["--get", key])
                .output()
                .ok()?;
            if !output.status.success() {
                return None;
            }
            usable_name(String::from_utf8(output.stdout).ok()?)
        })
}

#[cfg(not(any(windows, target_os = "macos")))]
fn system_name() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_preserve_unicode_and_punctuation() {
        assert_eq!(
            usable_name("  Linh’s Mac mini.local\n".into()).as_deref(),
            Some("Linh’s Mac mini.local")
        );
        assert_eq!(usable_name(" \r\n".into()), None);
    }

    #[test]
    fn long_names_fit_the_wire_without_splitting_unicode() {
        let name = usable_name("電".repeat(100)).unwrap();
        assert_eq!(name, "電".repeat(83));
    }
}
