//! Who may connect to this host, and this device's stable identity.
//!
//! The host lets a device in when the trust list says so, or after the
//! person at the host compares the pairing code shown on both screens and
//! approves it here. A host without a terminal rejects unknown devices unless
//! it was started with `--approve view|control`, which is for unattended
//! test runs only and says so when used.

use anyhow::{Context, Result};
use base64::Engine as _;
use frames_net::migration::{self, LegacyProof};
use frames_net::trust::{Admission, TrustList, TrustedDevice, admission};
use serde::{Deserialize, Serialize};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Duration;

/// Same directory as v1 (`FRAMES_CONFIG_DIR` overrides it), so the two
/// generations share one place for keys and trust.
pub fn config_dir(explicit: Option<&PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path.clone());
    }
    if let Some(path) = std::env::var_os("FRAMES_CONFIG_DIR") {
        return Ok(PathBuf::from(path));
    }
    Ok(directories::ProjectDirs::from("app", "Frames", "Frames")
        .context("no user configuration directory")?
        .config_dir()
        .to_path_buf())
}

pub fn device_key(config: &std::path::Path) -> Result<frames_net::DeviceKey> {
    frames_net::DeviceKey::load_or_create(&config.join("device-v2.key"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Approve {
    View,
    Control,
}

/// The viewer's first stream: an optional proof that it is a device this
/// host already trusted in v1 (docs/V2_PLAN.md, Phase 4 identity migration).
#[derive(Default, Serialize, Deserialize)]
struct Hello {
    /// Codecs the viewer can decode, best first.
    #[serde(default)]
    decoders: Vec<crate::Codec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    v1_certificate: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    v1_signature: Option<String>,
}

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// Viewer: always opens the hello stream first (it fixes the order of the
/// viewer's streams); with `migrate`, it carries a proof made with this
/// device's v1 key, bound to this connection and this device's v2 id.
pub async fn send_hello(
    session: &frames_net::Session,
    own_id: &iroh::EndpointId,
    migrate: bool,
    decoders: Vec<crate::Codec>,
) -> Result<()> {
    let mut hello = Hello {
        decoders,
        ..Hello::default()
    };
    if migrate {
        match v1_proof(session, own_id) {
            Ok(proof) => {
                hello.v1_certificate = Some(B64.encode(&proof.certificate));
                hello.v1_signature = Some(B64.encode(&proof.signature));
                println!("offering this device's v1 identity for trust migration");
            }
            Err(error) => println!("no v1 identity to offer: {error:#}"),
        }
    }
    let (mut send, _) = session.open_channel().await?;
    send.write_all(&serde_json::to_vec(&hello)?).await?;
    send.finish()?;
    Ok(())
}

fn v1_proof(session: &frames_net::Session, own_id: &iroh::EndpointId) -> Result<LegacyProof> {
    // FRAMES_CONFIG_DIR (set from --config-dir at start-up) selects the
    // directory v1 reads.
    let identity = frames_core::trust::Identity::load()?;
    let pkcs8 = identity.key.secret_der();
    let exporter = session.exporter(migration::EXPORTER_LABEL)?;
    migration::prove(&identity.certificate, pkcs8, own_id, &exporter)
}

async fn receive_hello(session: &frames_net::Session) -> Hello {
    let read = async {
        let (_send, mut recv) = session.accept_channel().await.ok()?;
        let bytes = recv.read_to_end(64 * 1024).await.ok()?;
        serde_json::from_slice::<Hello>(&bytes).ok()
    };
    tokio::time::timeout(Duration::from_secs(5), read)
        .await
        .ok()
        .flatten()
        .unwrap_or_default()
}

fn proof(hello: &Hello) -> Option<LegacyProof> {
    Some(LegacyProof {
        certificate: B64.decode(hello.v1_certificate.as_ref()?).ok()?,
        signature: B64.decode(hello.v1_signature.as_ref()?).ok()?,
    })
}

/// A device the host lets in.
pub struct Admitted {
    pub control: bool,
    /// What the viewer can decode, best first (empty if it did not say).
    pub decoders: Vec<crate::Codec>,
}

/// Host: a device that proves it is a v1-trusted device gets that trust
/// under its v2 id, with v1's name and control permission.
fn migrate(
    session: &frames_net::Session,
    proof: &LegacyProof,
    list: &mut TrustList,
    path: &std::path::Path,
) -> Result<Option<bool>> {
    let device = session.remote();
    let exporter = session.exporter(migration::EXPORTER_LABEL)?;
    let fingerprint = match migration::verify(proof, &device, &exporter) {
        Ok(fingerprint) => fingerprint,
        Err(error) => {
            println!("v1 identity proof rejected: {error:#}");
            return Ok(None);
        }
    };
    let v1 = frames_core::trust::TrustStore::load()?;
    let Some(peer) = v1.peers.get(&fingerprint) else {
        println!("its v1 identity is not trusted here");
        return Ok(None);
    };
    list.insert(
        &device,
        TrustedDevice {
            name: peer.name.clone(),
            allow_control: peer.allow_control,
            legacy_fingerprint: Some(fingerprint),
        },
    );
    list.save(path)?;
    println!(
        "migrated v1 trust for {} ({}): {}",
        peer.name,
        short(&device),
        if peer.allow_control {
            "view and control"
        } else {
            "view only"
        }
    );
    Ok(Some(peer.allow_control))
}

/// Whether to stream to this device, and how.
pub async fn decide(
    config: &std::path::Path,
    session: &frames_net::Session,
    preset: Option<Approve>,
) -> Result<Option<Admitted>> {
    let hello = receive_hello(session).await;
    let decoders = hello.decoders.clone();
    let admit = |control| Admitted {
        control,
        decoders: decoders.clone(),
    };
    Ok(admit_device(config, session, preset, &hello)
        .await?
        .map(admit))
}

async fn admit_device(
    config: &std::path::Path,
    session: &frames_net::Session,
    preset: Option<Approve>,
    hello: &Hello,
) -> Result<Option<bool>> {
    let path = TrustList::path_in(config);
    let mut list = TrustList::load(&path)?;
    let device = session.remote();
    let proof = proof(hello);
    if let Admission::Trusted { control } = admission(&list, &device) {
        let name = list
            .get(&device)
            .map(|d| d.name.clone())
            .unwrap_or_default();
        println!(
            "trusted device {name} ({}): {}",
            short(&device),
            if control {
                "view and control"
            } else {
                "view only"
            }
        );
        return Ok(Some(control));
    }
    if let Some(proof) = &proof
        && let Some(control) = migrate(session, proof, &mut list, &path)?
    {
        return Ok(Some(control));
    }
    let code = session.pairing_code()?;
    println!(
        "new device {} wants to connect. Pairing code {} {} (it must match the viewer's).",
        short(&device),
        &code[..4],
        &code[4..]
    );
    let choice = match preset {
        Some(choice) => {
            println!("approved by --approve {choice:?} (unattended test runs only)");
            Some(choice)
        }
        None if std::io::stdin().is_terminal() => ask().await,
        None => {
            println!("refused: no terminal to approve it here (see --approve)");
            None
        }
    };
    let Some(choice) = choice else {
        return Ok(None);
    };
    let control = choice == Approve::Control;
    list.insert(
        &device,
        TrustedDevice {
            name: format!("device {}", short(&device)),
            allow_control: control,
            legacy_fingerprint: None,
        },
    );
    list.save(&path)?;
    println!("saved to {}", path.display());
    Ok(Some(control))
}

async fn ask() -> Option<Approve> {
    println!("Allow it? [v] view only  [c] view and control  [anything else] refuse (60 s)");
    let line = tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).map(|_| line)
    });
    match tokio::time::timeout(Duration::from_secs(60), line).await {
        Ok(Ok(Ok(line))) => match line.trim() {
            "v" | "V" => Some(Approve::View),
            "c" | "C" => Some(Approve::Control),
            _ => None,
        },
        _ => {
            println!("no answer: refused");
            None
        }
    }
}

fn short(device: &iroh::EndpointId) -> String {
    device.to_string()[..8].to_string()
}
