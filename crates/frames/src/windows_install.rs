//! Keep Installed Apps metadata in sync after replacing a per-user installation.
use anyhow::{Context, Result};
use std::path::Path;
use windows::{
    Win32::{Foundation::ERROR_FILE_NOT_FOUND, System::Registry::*},
    core::{PCWSTR, w},
};

const UNINSTALL_KEY: &str =
    "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\dev.creaton.frames_is1";

struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn string(key: HKEY, name: PCWSTR) -> Result<String> {
    let mut buffer = vec![0u16; 32768];
    let mut size = (buffer.len() * 2) as u32;
    unsafe {
        RegGetValueW(
            key,
            None,
            name,
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut size),
        )
        .ok()?;
    }
    buffer.truncate(size as usize / 2);
    if buffer.last() == Some(&0) {
        buffer.pop();
    }
    Ok(String::from_utf16(&buffer)?)
}

fn set_string(key: HKEY, name: PCWSTR, value: &str) -> Result<()> {
    let bytes: Vec<u8> = wide(value)
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    unsafe {
        RegSetValueExW(key, name, None, REG_SZ, Some(&bytes)).ok()?;
    }
    Ok(())
}

pub fn refresh(target: &Path, version: &str) -> Result<()> {
    refresh_at(UNINSTALL_KEY, target, version)
}

fn refresh_at(subkey: &str, target: &Path, version: &str) -> Result<()> {
    let subkey = wide(subkey);
    let mut key = HKEY::default();
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            None,
            KEY_QUERY_VALUE | KEY_SET_VALUE | KEY_WOW64_64KEY,
            &mut key,
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    status.ok()?;
    let key = Key(key);
    let location = string(key.0, w!("InstallLocation"))?;
    // Portable copies must never change the registered installation's version.
    if Path::new(&location).canonicalize()?
        != target
            .parent()
            .context("missing installation directory")?
            .canonicalize()?
        || !target
            .file_name()
            .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("Frames.exe"))
    {
        return Ok(());
    }
    set_string(key.0, w!("DisplayVersion"), version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Registration(Vec<u16>);
    impl Drop for Registration {
        fn drop(&mut self) {
            unsafe {
                let _ = RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(self.0.as_ptr()));
            }
        }
    }

    #[test]
    fn registered_version_changes_only_for_the_registered_installation() {
        let root = tempfile::tempdir().unwrap();
        let install = root.path().join("installed");
        let portable = root.path().join("portable");
        fs::create_dir(&install).unwrap();
        fs::create_dir(&portable).unwrap();
        let subkey = format!(
            "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\dev.creaton.frames.test-{}_is1",
            uuid::Uuid::new_v4()
        );
        let registration = Registration(wide(&subkey));
        let mut key = HKEY::default();
        unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(registration.0.as_ptr()),
                None,
                None,
                REG_OPTION_NON_VOLATILE,
                KEY_ALL_ACCESS | KEY_WOW64_64KEY,
                None,
                &mut key,
                None,
            )
            .ok()
            .unwrap();
        }
        let key = Key(key);
        set_string(key.0, w!("InstallLocation"), &install.to_string_lossy()).unwrap();
        set_string(key.0, w!("DisplayVersion"), "0.1.1").unwrap();
        refresh_at(&subkey, &portable.join("Frames.exe"), "0.1.2").unwrap();
        assert_eq!(string(key.0, w!("DisplayVersion")).unwrap(), "0.1.1");
        refresh_at(&subkey, &install.join("Frames.exe"), "0.1.2").unwrap();
        assert_eq!(string(key.0, w!("DisplayVersion")).unwrap(), "0.1.2");
    }
}
