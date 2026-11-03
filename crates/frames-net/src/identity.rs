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

    pub fn id(&self) -> EndpointId {
        self.0.public()
    }

    pub(crate) fn secret(&self) -> SecretKey {
        self.0.clone()
    }
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
}
