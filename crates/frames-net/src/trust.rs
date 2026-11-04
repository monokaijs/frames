//! Which v2 devices this one trusts, by device id (docs/V2_PLAN.md, Phase 4).
//!
//! A host lets a device in only if it is listed here, or after the person at
//! the host compares the pairing code both screens show and approves. Only a
//! device listed with `allow_control` may send input. The file is written
//! atomically (temporary file, then rename) so a crash never leaves a
//! half-written list that locks every device out.

use anyhow::{Context, Result};
use iroh::EndpointId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedDevice {
    pub name: String,
    /// May send input (otherwise view only).
    pub allow_control: bool,
    /// The v1 certificate fingerprint this entry was migrated from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_fingerprint: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustList {
    /// Keyed by the device id's hex form.
    devices: BTreeMap<String, TrustedDevice>,
}

impl TrustList {
    pub fn path_in(config_dir: &Path) -> PathBuf {
        config_dir.join("trusted-v2.json")
    }

    /// Loads the list; a missing file is an empty list. A file that cannot
    /// be read as a list is an error rather than silently empty, so a
    /// corrupted file never looks like "trust nobody" and gets overwritten.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("{} is not a valid trust list", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&temporary, path).with_context(|| format!("replacing {}", path.display()))
    }

    pub fn get(&self, device: &EndpointId) -> Option<&TrustedDevice> {
        self.devices.get(&device.to_string())
    }

    pub fn insert(&mut self, device: &EndpointId, entry: TrustedDevice) {
        self.devices.insert(device.to_string(), entry);
    }

    pub fn remove(&mut self, device: &EndpointId) -> bool {
        self.devices.remove(&device.to_string()).is_some()
    }

    pub fn len(&self) -> usize {
        self.devices.len()
    }

    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }
}

/// What a host does with a connecting device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    /// Listed: stream, with input only if `control`.
    Trusted { control: bool },
    /// Unknown: ask the person at the host.
    Ask,
}

pub fn admission(list: &TrustList, device: &EndpointId) -> Admission {
    match list.get(device) {
        Some(entry) => Admission::Trusted {
            control: entry.allow_control,
        },
        None => Admission::Ask,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DeviceKey;

    #[test]
    fn devices_round_trip_and_unknown_ones_are_asked_about() {
        let directory = tempfile::tempdir().unwrap();
        let path = TrustList::path_in(directory.path());
        let (known, viewer, stranger) = (
            DeviceKey::generate().id(),
            DeviceKey::generate().id(),
            DeviceKey::generate().id(),
        );
        let mut list = TrustList::load(&path).unwrap();
        assert!(list.is_empty());
        list.insert(
            &known,
            TrustedDevice {
                name: "Desk PC".into(),
                allow_control: true,
                legacy_fingerprint: None,
            },
        );
        list.insert(
            &viewer,
            TrustedDevice {
                name: "Tablet".into(),
                allow_control: false,
                legacy_fingerprint: Some("ab".repeat(32)),
            },
        );
        list.save(&path).unwrap();
        let list = TrustList::load(&path).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(
            admission(&list, &known),
            Admission::Trusted { control: true }
        );
        assert_eq!(
            admission(&list, &viewer),
            Admission::Trusted { control: false }
        );
        assert_eq!(admission(&list, &stranger), Admission::Ask);
    }

    #[test]
    fn a_corrupted_list_is_an_error_not_an_empty_list() {
        let directory = tempfile::tempdir().unwrap();
        let path = TrustList::path_in(directory.path());
        std::fs::write(&path, b"{ not json").unwrap();
        assert!(TrustList::load(&path).is_err());
    }
}
