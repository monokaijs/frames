//! A host's access password: a viewer that knows it is let in without
//! anyone approving it at the host (unattended access, as with sharing at
//! login or the Windows service).
//!
//! The password never crosses the network and is not stored. Both ends
//! derive a key from it with PBKDF2-HMAC-SHA256, salted with the host's
//! device id; the host keeps only that key (`access-password.json`). The
//! viewer proves it knows the key with an HMAC of this session's TLS
//! exporter, so a proof is worthless on any other connection. Five wrong
//! proofs within a minute stop password attempts for a minute.

use anyhow::{Context, Result};
use base64::Engine as _;
use hmac::Mac as _;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

type HmacSha256 = hmac::Hmac<sha2::Sha256>;

const ITERATIONS: u32 = 210_000;
const SALT_PREFIX: &[u8] = b"frames-access-password-v1";
const EXPORTER_LABEL: &[u8] = b"frames-access-password-v1";
/// The shortest password accepted.
pub const MIN_LENGTH: usize = 8;
const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

#[derive(Serialize, Deserialize)]
struct Stored {
    /// The PBKDF2 key, base64.
    key: String,
}

fn path(config: &Path) -> PathBuf {
    config.join("access-password.json")
}

fn derive(password: &str, host: &iroh::EndpointId) -> [u8; 32] {
    let mut salt = SALT_PREFIX.to_vec();
    salt.extend_from_slice(host.as_bytes());
    let mut key = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(password.as_bytes(), &salt, ITERATIONS, &mut key);
    key
}

/// Sets this host's password (`host` is its own device id).
pub fn set(config: &Path, host: &iroh::EndpointId, password: &str) -> Result<()> {
    anyhow::ensure!(
        password.chars().count() >= MIN_LENGTH,
        "Use an access password of at least {MIN_LENGTH} characters."
    );
    let stored = Stored {
        key: B64.encode(derive(password, host)),
    };
    let path = path(config);
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, serde_json::to_vec(&stored)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&temporary, &path).context("saving the access password")?;
    Ok(())
}

pub fn clear(config: &Path) -> Result<()> {
    match std::fs::remove_file(path(config)) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}

fn stored_key(config: &Path) -> Option<Vec<u8>> {
    let bytes = std::fs::read(path(config)).ok()?;
    let stored: Stored = serde_json::from_slice(&bytes).ok()?;
    B64.decode(stored.key).ok().filter(|key| key.len() == 32)
}

pub fn is_set(config: &Path) -> bool {
    stored_key(config).is_some()
}

fn mac(key: &[u8], exporter: &[u8; 32]) -> HmacSha256 {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(exporter);
    mac
}

/// Viewer: the proof of `password` for this session with the host.
pub fn proof(session: &frames_net::Session, password: &str) -> Result<String> {
    let key = derive(password, &session.remote());
    let exporter = session.exporter(EXPORTER_LABEL)?;
    Ok(B64.encode(mac(&key, &exporter).finalize().into_bytes()))
}

/// Host: whether `proof` shows the viewer knows this host's password.
pub fn verify(config: &Path, session: &frames_net::Session, proof: &str) -> Result<bool> {
    let Some(key) = stored_key(config) else {
        return Ok(false);
    };
    let Ok(proof) = B64.decode(proof) else {
        return Ok(false);
    };
    let exporter = session.exporter(EXPORTER_LABEL)?;
    Ok(mac(&key, &exporter).verify_slice(&proof).is_ok())
}

/// Host: recent wrong proofs, to slow down guessing.
#[derive(Default)]
pub struct Attempts {
    failures: VecDeque<Instant>,
}

impl Attempts {
    const WINDOW: Duration = Duration::from_secs(60);
    const LIMIT: usize = 5;

    fn trim(&mut self, now: Instant) {
        while self
            .failures
            .front()
            .is_some_and(|at| now.duration_since(*at) > Self::WINDOW)
        {
            self.failures.pop_front();
        }
    }

    /// Whether a password may be tried now.
    pub fn allowed(&mut self, now: Instant) -> bool {
        self.trim(now);
        self.failures.len() < Self::LIMIT
    }

    pub fn failed(&mut self, now: Instant) {
        self.failures.push_back(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stored_key_depends_on_password_and_host_and_short_ones_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let a = frames_net::DeviceKey::generate().id();
        let b = frames_net::DeviceKey::generate().id();
        assert_ne!(derive("correct horse", &a), derive("correct horse", &b));
        assert_ne!(derive("correct horse", &a), derive("correct horsf", &a));
        assert!(set(dir.path(), &a, "short").is_err());
        assert!(!is_set(dir.path()));
        set(dir.path(), &a, "correct horse").unwrap();
        assert!(is_set(dir.path()));
        // Only the derived key is stored.
        let saved = std::fs::read_to_string(dir.path().join("access-password.json")).unwrap();
        assert!(!saved.contains("correct horse"));
        clear(dir.path()).unwrap();
        clear(dir.path()).unwrap();
        assert!(!is_set(dir.path()));
    }

    #[test]
    fn five_wrong_passwords_in_a_minute_pause_attempts() {
        let t = Instant::now();
        let mut attempts = Attempts::default();
        for i in 0..5 {
            assert!(attempts.allowed(t + Duration::from_secs(i)));
            attempts.failed(t + Duration::from_secs(i));
        }
        assert!(!attempts.allowed(t + Duration::from_secs(10)));
        assert!(attempts.allowed(t + Duration::from_secs(62)));
    }
}
