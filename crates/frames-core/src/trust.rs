use anyhow::Context;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub fn config_dir() -> anyhow::Result<PathBuf> {
    let dir = if let Some(path) = std::env::var_os("FRAMES_CONFIG_DIR") {
        PathBuf::from(path)
    } else {
        directories::ProjectDirs::from("app", "Frames", "Frames")
            .context("no user configuration directory")?
            .config_dir()
            .to_path_buf()
    };
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}
pub fn fingerprint(cert: &[u8]) -> String {
    hex::encode(Sha256::digest(cert))
}

pub struct Identity {
    pub certificate: CertificateDer<'static>,
    pub key: PrivateKeyDer<'static>,
    pub device_id: String,
}
impl Identity {
    #[cfg(windows)]
    pub fn install_service_identity(&self, destination: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(destination)?;
        std::fs::write(
            destination.join("identity.machine"),
            b"Frames machine-scoped DPAPI identity v1",
        )?;
        save_secret(destination, &self.device_id, self.key.secret_der())?;
        std::fs::write(destination.join("identity.der"), &self.certificate)?;
        std::fs::write(
            destination.join("trusted.json"),
            serde_json::to_vec_pretty(&TrustStore::load()?)?,
        )?;
        Ok(())
    }
    pub fn load() -> anyhow::Result<Self> {
        let dir = config_dir()?;
        // Launcher children may start host and client together on first run.
        // Serialize key/certificate creation so a pinned identity remains usable.
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join("identity.lock"))?;
        fs2::FileExt::lock_exclusive(&lock)?;
        let cert_path = dir.join("identity.der");
        if cert_path.exists() {
            let cert = std::fs::read(cert_path)?;
            let id = fingerprint(&cert);
            let key = load_secret(&dir, &id)
                .context("identity key unavailable; do not silently replace a trusted identity")?;
            return Ok(Self {
                certificate: cert.into(),
                key: PrivatePkcs8KeyDer::from(key).into(),
                device_id: id,
            });
        }
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["frames.local".into()])?;
        let certificate = cert.der().clone();
        let device_id = fingerprint(&certificate);
        save_secret(&dir, &device_id, &signing_key.serialize_der())?;
        std::fs::write(cert_path, &certificate)?;
        Ok(Self {
            certificate,
            key: PrivatePkcs8KeyDer::from(signing_key.serialize_der()).into(),
            device_id,
        })
    }
}

#[cfg(windows)]
fn save_secret(dir: &Path, _: &str, secret: &[u8]) -> anyhow::Result<()> {
    use windows::Win32::{
        Foundation::{HLOCAL, LocalFree},
        Security::Cryptography::*,
    };
    unsafe {
        let input = CRYPT_INTEGER_BLOB {
            cbData: secret.len() as u32,
            pbData: secret.as_ptr() as *mut u8,
        };
        let mut output = CRYPT_INTEGER_BLOB::default();
        let flags = CRYPTPROTECT_UI_FORBIDDEN
            | if dir.join("identity.machine").exists() {
                CRYPTPROTECT_LOCAL_MACHINE
            } else {
                0
            };
        CryptProtectData(&input, None, None, None, None, flags, &mut output)?;
        let bytes = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(output.pbData.cast())));
        std::fs::write(dir.join("identity.dpapi"), bytes)?;
    }
    Ok(())
}
#[cfg(windows)]
fn load_secret(dir: &Path, _: &str) -> anyhow::Result<Vec<u8>> {
    use windows::Win32::{
        Foundation::{HLOCAL, LocalFree},
        Security::Cryptography::*,
    };
    let encrypted = std::fs::read(dir.join("identity.dpapi"))?;
    unsafe {
        let input = CRYPT_INTEGER_BLOB {
            cbData: encrypted.len() as u32,
            pbData: encrypted.as_ptr() as *mut u8,
        };
        let mut output = CRYPT_INTEGER_BLOB::default();
        CryptUnprotectData(
            &input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )?;
        let bytes = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(output.pbData.cast())));
        Ok(bytes)
    }
}
#[cfg(target_os = "macos")]
fn save_secret(_: &Path, id: &str, secret: &[u8]) -> anyhow::Result<()> {
    Ok(security_framework::passwords::set_generic_password(
        "app.frames.identity",
        id,
        secret,
    )?)
}
#[cfg(target_os = "macos")]
fn load_secret(_: &Path, id: &str) -> anyhow::Result<Vec<u8>> {
    Ok(security_framework::passwords::get_generic_password(
        "app.frames.identity",
        id,
    )?)
}
#[cfg(not(any(windows, target_os = "macos")))]
fn save_secret(_: &Path, _: &str, _: &[u8]) -> anyhow::Result<()> {
    anyhow::bail!("protected identity storage is implemented for Windows and macOS only")
}
#[cfg(not(any(windows, target_os = "macos")))]
fn load_secret(_: &Path, _: &str) -> anyhow::Result<Vec<u8>> {
    anyhow::bail!("unsupported platform")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustedPeer {
    pub name: String,
    pub unattended: bool,
    #[serde(default)]
    pub allow_control: bool,
}
pub fn incoming_needs_approval(peer: Option<&TrustedPeer>, unattended: bool) -> bool {
    !unattended || !peer.is_some_and(|p| p.allow_control && p.unattended)
}
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct TrustStore {
    pub peers: BTreeMap<String, TrustedPeer>,
}
impl TrustStore {
    pub fn load() -> anyhow::Result<Self> {
        let path = config_dir()?.join("trusted.json");
        if !path.exists() {
            return Ok(Self::default());
        }
        Ok(serde_json::from_slice(&std::fs::read(path)?)?)
    }
    fn save(&self) -> anyhow::Result<()> {
        let path = config_dir()?.join("trusted.json");
        let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
        let data = serde_json::to_vec_pretty(self)?;
        let mut file = std::fs::File::create(&temporary)?;
        use std::io::Write;
        file.write_all(&data)?;
        file.sync_all()?;
        std::fs::rename(temporary, path)?;
        Ok(())
    }
    pub fn approve(
        &mut self,
        id: String,
        name: String,
        unattended: bool,
        incoming: bool,
    ) -> anyhow::Result<()> {
        let _lock = store_lock()?;
        let mut latest = Self::load()?;
        let existing = latest.peers.get(&id);
        let peer = TrustedPeer {
            name,
            allow_control: incoming || existing.is_some_and(|p| p.allow_control),
            unattended: if incoming {
                unattended
            } else {
                existing.is_some_and(|p| p.unattended)
            },
        };
        latest.peers.insert(id, peer);
        latest.save()?;
        *self = latest;
        Ok(())
    }
    pub fn revoke(&mut self, id: &str) -> anyhow::Result<bool> {
        let _lock = store_lock()?;
        let mut latest = Self::load()?;
        let existed = latest.peers.remove(id).is_some();
        latest.save()?;
        *self = latest;
        Ok(existed)
    }
}
fn store_lock() -> anyhow::Result<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(config_dir()?.join("trusted.lock"))?;
    fs2::FileExt::lock_exclusive(&file)?;
    Ok(file)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn trusting_a_host_does_not_grant_it_unattended_control() {
        let pin = TrustedPeer {
            name: "known host".into(),
            unattended: true,
            allow_control: false,
        };
        assert!(incoming_needs_approval(Some(&pin), true));
        let allowed = TrustedPeer {
            allow_control: true,
            ..pin
        };
        assert!(!incoming_needs_approval(Some(&allowed), true));
        assert!(incoming_needs_approval(Some(&allowed), false));
        assert!(incoming_needs_approval(None, true));
    }
}
