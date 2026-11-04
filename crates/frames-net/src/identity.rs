//! Device identity for v2.
//!
//! The v2 device key (ed25519, iroh's endpoint key) is derived from the v1
//! identity's P-256 private key with HKDF-SHA256. Deriving instead of
//! generating means: nothing new to store or protect (the v1 key already
//! lives in the Keychain or DPAPI), the Windows service identity derives the
//! same key, and there is no first-run race.

use anyhow::{Context, Result, ensure};
use iroh::{EndpointId, SecretKey};
use ring::hkdf;

const INFO: &[u8] = b"frames-iroh-v1";

#[derive(Clone)]
pub struct DeviceKey(SecretKey);

impl DeviceKey {
    /// Derives the v2 key from the v1 identity's PKCS#8 private key.
    pub fn from_legacy_pkcs8(pkcs8: &[u8]) -> Result<Self> {
        ensure!(pkcs8.len() >= 32, "legacy key is too short");
        let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, &[]);
        let prk = salt.extract(pkcs8);
        let info = [INFO];
        let okm = prk.expand(&info, Seed).ok().context("HKDF expand failed")?;
        let mut seed = [0u8; 32];
        okm.fill(&mut seed).ok().context("HKDF fill failed")?;
        Ok(Self(SecretKey::from_bytes(&seed)))
    }

    /// A fresh random key, for tests and devices without a v1 identity.
    pub fn generate() -> Self {
        let mut seed = [0u8; 32];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut seed)
            .expect("system randomness is available");
        Self(SecretKey::from_bytes(&seed))
    }

    /// Loads this device's key from `path`, creating it on first use. The
    /// file holds the 32-byte seed and is readable only by this user.
    ///
    /// Until the v2 cutover moves identity to [`Self::from_legacy_pkcs8`]
    /// (the v1 key lives in the Keychain or DPAPI, and reading it from a
    /// test binary prompts), `frames-next` keeps its key here.
    pub fn load_or_create(path: &std::path::Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let seed: [u8; 32] = bytes
                    .as_slice()
                    .try_into()
                    .ok()
                    .with_context(|| format!("{} is not a device key", path.display()))?;
                Ok(Self(SecretKey::from_bytes(&seed)))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let key = Self::generate();
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let temporary = path.with_extension("tmp");
                write_private(&temporary, &key.0.to_bytes())?;
                std::fs::rename(&temporary, path)?;
                Ok(key)
            }
            Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn id(&self) -> EndpointId {
        self.0.public()
    }

    pub(crate) fn secret(&self) -> SecretKey {
        self.0.clone()
    }
}

fn write_private(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

struct Seed;

impl hkdf::KeyType for Seed {
    fn len(&self) -> usize {
        32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derivation_is_stable_and_key_specific() {
        let a = rcgen::KeyPair::generate().unwrap().serialize_der();
        let b = rcgen::KeyPair::generate().unwrap().serialize_der();
        let first = DeviceKey::from_legacy_pkcs8(&a).unwrap().id();
        assert_eq!(first, DeviceKey::from_legacy_pkcs8(&a).unwrap().id());
        assert_ne!(first, DeviceKey::from_legacy_pkcs8(&b).unwrap().id());
        assert!(DeviceKey::from_legacy_pkcs8(&[1; 8]).is_err());
        assert_ne!(DeviceKey::generate().id(), DeviceKey::generate().id());
    }

    #[test]
    fn a_stored_key_is_created_once_and_reloaded() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("device-v2.key");
        let first = DeviceKey::load_or_create(&path).unwrap().id();
        assert_eq!(DeviceKey::load_or_create(&path).unwrap().id(), first);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::write(&path, b"short").unwrap();
        assert!(DeviceKey::load_or_create(&path).is_err());
    }
}
