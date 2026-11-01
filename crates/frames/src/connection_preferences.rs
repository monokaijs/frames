//! Last-used client choices, keyed by the authenticated device identity.
use anyhow::Result;
use frames_core::adaptive::QualityPreset;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, path::Path};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConnectionPreferences {
    pub audio: bool,
    pub desktop: bool,
    pub quality: QualityPreset,
    pub remote_cursor: bool,
}

impl Default for ConnectionPreferences {
    fn default() -> Self {
        Self {
            audio: false,
            desktop: false,
            quality: QualityPreset::default(),
            remote_cursor: true,
        }
    }
}

pub type Devices = BTreeMap<String, ConnectionPreferences>;

fn read(path: &Path) -> Result<Devices> {
    match fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Devices::new()),
        Err(error) => Err(error.into()),
    }
}

pub fn load() -> Result<Devices> {
    read(&frames_core::trust::config_dir()?.join("connection-preferences.json"))
}

pub fn update(id: &str, edit: impl FnOnce(&mut ConnectionPreferences)) -> Result<()> {
    update_at(
        &frames_core::trust::config_dir()?.join("connection-preferences.json"),
        id,
        edit,
    )
}

fn update_at(path: &Path, id: &str, edit: impl FnOnce(&mut ConnectionPreferences)) -> Result<()> {
    // Each session is a separate process. Merge each edit under a lock so live
    // view/quality changes cannot overwrite another device's saved choices.
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path.with_extension("lock"))?;
    fs2::FileExt::lock_exclusive(&lock)?;
    let mut devices = read(path)?;
    edit(devices.entry(id.into()).or_default());
    crate::connection_history::write_atomic(path.into(), &serde_json::to_vec_pretty(&devices)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_choices_survive_reload_and_independent_session_edits() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("connection-preferences.json");
        update_at(&path, "first", |p| {
            p.audio = true;
            p.desktop = true;
        })
        .unwrap();
        update_at(&path, "second", |p| p.quality = QualityPreset::Low).unwrap();
        update_at(&path, "first", |p| p.quality = QualityPreset::Medium).unwrap();
        let devices = read(&path).unwrap();
        assert_eq!(
            devices["first"],
            ConnectionPreferences {
                audio: true,
                desktop: true,
                quality: QualityPreset::Medium,
                ..Default::default()
            }
        );
        assert_eq!(
            devices["second"],
            ConnectionPreferences {
                quality: QualityPreset::Low,
                ..Default::default()
            }
        );
    }

    #[test]
    fn simultaneous_sessions_merge_their_preferences() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("connection-preferences.json");
        std::thread::scope(|scope| {
            for device in 0..8 {
                let path = &path;
                scope.spawn(move || {
                    update_at(path, &device.to_string(), |p| p.desktop = true).unwrap();
                });
            }
        });
        let devices = read(&path).unwrap();
        assert_eq!(devices.len(), 8);
        assert!(devices.values().all(|p| p.desktop));
    }
}
