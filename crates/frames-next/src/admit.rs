//! Who may connect to this host, and this device's stable identity.
//!
//! The host lets a device in when the trust list says so, or after the
//! person at the host compares the pairing code shown on both screens and
//! approves it here, or in the Frames launcher that started the host
//! (`--ask-launcher`, see `crate::launcher`). Otherwise a host without a
//! terminal rejects unknown devices unless it was started with
//! `--approve view|control`, which is for unattended test runs only and says
//! so when used.

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

/// What the person at the host allowed a new device to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Approve {
    pub control: bool,
    /// Saved to the trust list; otherwise this session only, and the device
    /// is asked about again next time.
    pub remember: bool,
}

impl Approve {
    /// `view`, `control`, or either with `-once` (this session only).
    pub fn parse(text: &str) -> Option<Self> {
        let (kind, remember) = match text.strip_suffix("-once") {
            Some(kind) => (kind, false),
            None => (text, true),
        };
        let control = match kind {
            "view" => false,
            "control" => true,
            _ => return None,
        };
        Some(Self { control, remember })
    }

    fn describe(self) -> &'static str {
        match (self.control, self.remember) {
            (false, true) => "view only",
            (true, true) => "view and control",
            (false, false) => "view only, this session only",
            (true, false) => "view and control, this session only",
        }
    }
}

/// The viewer's first stream: an optional proof that it is a device this
/// host already trusted in v1 (docs/V2_PLAN.md, Phase 4 identity migration).
#[derive(Default, Serialize, Deserialize)]
struct Hello {
    /// Codecs the viewer can decode, best first.
    #[serde(default)]
    decoders: Vec<crate::Codec>,
    /// Which of the host's shared windows this session shows (0-based).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    window: Option<u32>,
    /// The viewer's device name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    /// Whether the viewer wants host audio (absent: older viewers, which
    /// took it whenever the host offered it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    audio: Option<bool>,
    /// The most the viewer wants to receive, in Mbps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_mbps: Option<u32>,
    /// This session lists the host's windows instead of streaming.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    catalog: bool,
    /// The host window this session streams, by catalog id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    window_id: Option<u64>,
    /// Proof that the viewer knows the host's access password
    /// (`crate::password`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    password: Option<String>,
    /// Asks for a preview image only (`crate::preview`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    preview: bool,
    /// The host's Quick App this connection is for (`crate::quick_apps`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    app: Option<String>,
    /// Only that app's windows ("Only stream this app").
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    only_app: bool,
    /// Asks for the host's Quick Apps list only.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    quick_apps: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    v1_certificate: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    v1_signature: Option<String>,
}

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// What a viewer tells the host about itself and its session.
pub struct Asks {
    /// Codecs it decodes, best first.
    pub decoders: Vec<crate::Codec>,
    /// The host's shared window to show (0-based).
    pub window: Option<u32>,
    /// Its device name.
    pub name: String,
    /// It wants host audio.
    pub audio: bool,
    /// The most it wants to receive, in Mbps.
    pub max_mbps: Option<u32>,
    /// Lists the host's windows instead of streaming (`crate::catalog`).
    pub catalog: bool,
    /// The host window to stream, by catalog id.
    pub window_id: Option<u64>,
    /// The host's access password, when the viewer was given one.
    pub password: Option<String>,
    /// Asks for a preview image only (`crate::preview`).
    pub preview: bool,
    /// The host's Quick App this connection is for, and whether only its
    /// windows are shared (`crate::quick_apps`).
    pub app: Option<String>,
    pub only_app: bool,
    /// Asks for the host's Quick Apps list only.
    pub quick_apps: bool,
}

/// Viewer: always opens the hello stream first (it fixes the order of the
/// viewer's streams); with `migrate`, it carries a proof made with this
/// device's v1 key, bound to this connection and this device's v2 id.
pub async fn send_hello(
    session: &frames_net::Session,
    own_id: &iroh::EndpointId,
    migrate: bool,
    asks: Asks,
) -> Result<()> {
    let mut hello = Hello {
        decoders: asks.decoders,
        window: asks.window,
        name: Some(asks.name),
        audio: Some(asks.audio),
        max_mbps: asks.max_mbps,
        catalog: asks.catalog,
        window_id: asks.window_id,
        preview: asks.preview,
        app: asks.app,
        only_app: asks.only_app,
        quick_apps: asks.quick_apps,
        ..Hello::default()
    };
    if let Some(password) = &asks.password {
        hello.password = Some(crate::password::proof(session, password)?);
    }
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
    /// The shared window the viewer asked for.
    pub window: Option<u32>,
    /// The viewer wants host audio.
    pub audio: bool,
    /// The viewer's bitrate cap, in Mbps.
    pub max_mbps: Option<u32>,
    /// The viewer asks for the window catalog.
    pub catalog: bool,
    /// The host window the viewer asks for.
    pub window_id: Option<u64>,
    /// The viewer asks for a preview image only.
    pub preview: bool,
    /// The Quick App the viewer asks for, and whether only its windows.
    pub app: Option<String>,
    pub only_app: bool,
    /// The viewer asks for the Quick Apps list only.
    pub quick_apps: bool,
    /// Let in by the trust list: removing it there ends the session
    /// ([`watch_trust`]).
    pub trusted: bool,
}

/// Host: ends `session` within a second of its device being removed from
/// the trust list (as v1: "device access revoked").
pub async fn watch_trust(config: std::path::PathBuf, session: frames_net::Session) {
    let path = TrustList::path_in(&config);
    let device = session.remote();
    loop {
        tokio::select! {
            () = session.closed() => return,
            () = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
        // An unreadable list (being written) is checked again next time.
        if let Ok(list) = TrustList::load(&path)
            && list.get(&device).is_none()
        {
            println!("{}: device access revoked", short(&device));
            session.close("device access revoked");
            return;
        }
    }
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
/// What a host remembers between admissions while it runs.
#[derive(Default)]
pub struct HostState {
    /// Devices let in for this session only, with whether they may
    /// control; they are not asked again (a viewer of several windows opens
    /// a session for each). Trusted devices are checked against the trust
    /// list every time, so removing one takes effect at once.
    approved: std::collections::HashMap<iroh::EndpointId, bool>,
    /// Recent wrong access passwords.
    attempts: crate::password::Attempts,
    /// The last device was let in by the access password: not remembered,
    /// so every session proves it again (and changing the password takes
    /// effect at once).
    by_password: bool,
}

/// Why a device was not let in, for its close reason.
pub const NOT_APPROVED: &str = "not approved";
pub const WRONG_PASSWORD: &str = "wrong password";

/// `launcher`: the Frames launcher started this host and asks its user.
/// `Err` (inside `Ok`) is the reason a device was refused.
pub async fn decide(
    config: &std::path::Path,
    session: &frames_net::Session,
    preset: Option<Approve>,
    launcher: bool,
    state: &mut HostState,
) -> Result<std::result::Result<Admitted, &'static str>> {
    let hello = receive_hello(session).await;
    // Previews and the Quick Apps list go to trusted devices only, and
    // nobody is asked for them.
    if (hello.preview || hello.quick_apps)
        && !matches!(
            admission(
                &TrustList::load(&TrustList::path_in(config))?,
                &session.remote()
            ),
            Admission::Trusted { .. }
        )
    {
        return Ok(Err(NOT_APPROVED));
    }
    let control = match state.approved.get(&session.remote()) {
        Some(&control) => control,
        None => {
            state.by_password = false;
            let control =
                match admit_device(config, session, preset, launcher, &hello, state).await? {
                    Ok(control) => control,
                    Err(reason) => return Ok(Err(reason)),
                };
            let list = TrustList::load(&TrustList::path_in(config))?;
            if list.get(&session.remote()).is_none() && !state.by_password {
                state.approved.insert(session.remote(), control);
            }
            control
        }
    };
    let trusted = TrustList::load(&TrustList::path_in(config))?
        .get(&session.remote())
        .is_some();
    let name = hello.name.clone().unwrap_or_default();
    // A preview or the Quick Apps list is not a connection to announce.
    if !hello.preview && !hello.quick_apps {
        crate::launcher::report(
            "viewer",
            &serde_json::json!({
                "device": session.remote().to_string(),
                "name": name,
                "control": control,
            }),
        );
    }
    Ok(Ok(Admitted {
        control,
        decoders: hello.decoders,
        window: hello.window,
        audio: hello.audio.unwrap_or(true),
        max_mbps: hello.max_mbps.map(|cap| cap.max(1)),
        catalog: hello.catalog,
        window_id: hello.window_id,
        preview: hello.preview,
        app: hello.app,
        only_app: hello.only_app,
        quick_apps: hello.quick_apps,
        trusted,
    }))
}

/// The name to save for a device: what it calls itself, else its id.
fn saved_name(hello: &Hello, device: &iroh::EndpointId) -> String {
    hello
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| name.chars().take(250).collect())
        .unwrap_or_else(|| format!("device {}", short(device)))
}

async fn admit_device(
    config: &std::path::Path,
    session: &frames_net::Session,
    preset: Option<Approve>,
    launcher: bool,
    hello: &Hello,
    state: &mut HostState,
) -> Result<std::result::Result<bool, &'static str>> {
    let path = TrustList::path_in(config);
    let mut list = TrustList::load(&path)?;
    let device = session.remote();
    let proof = proof(hello);
    if let Admission::Trusted { control } = admission(&list, &device) {
        let mut name = list
            .get(&device)
            .map(|d| d.name.clone())
            .unwrap_or_default();
        // Keep the saved name current when the device was renamed.
        let current = saved_name(hello, &device);
        if hello.name.is_some()
            && current != name
            && let Some(mut entry) = list.get(&device).cloned()
        {
            entry.name = current.clone();
            list.insert(&device, entry);
            list.save(&path)?;
            name = current;
        }
        println!(
            "trusted device {name} ({}): {}",
            short(&device),
            if control {
                "view and control"
            } else {
                "view only"
            }
        );
        return Ok(Ok(control));
    }
    if let Some(proof) = &proof
        && let Some(control) = migrate(session, proof, &mut list, &path)?
    {
        return Ok(Ok(control));
    }
    // A viewer that knows the access password needs nobody here.
    let mut wrong_password = false;
    if let Some(proof) = &hello.password
        && crate::password::is_set(config)
    {
        let now = std::time::Instant::now();
        if !state.attempts.allowed(now) {
            println!("access password not checked: too many wrong ones in the last minute");
            wrong_password = true;
        } else if crate::password::verify(config, session, proof)? {
            println!(
                "{} ({}) gave the access password: view and control",
                saved_name(hello, &device),
                short(&device)
            );
            state.by_password = true;
            return Ok(Ok(true));
        } else {
            state.attempts.failed(now);
            println!("wrong access password from {}", short(&device));
            wrong_password = true;
        }
    }
    let code = session.pairing_code()?;
    let name = saved_name(hello, &device);
    println!(
        "new device {name} ({}) wants to connect. Pairing code {} {} (it must match the viewer's).",
        short(&device),
        &code[..4],
        &code[4..]
    );
    let choice = match preset {
        Some(choice) => {
            println!(
                "approved by --approve: {} (unattended test runs only)",
                choice.describe()
            );
            Some(choice)
        }
        None if launcher => crate::launcher::ask_pairing(&device, &name, &code).await,
        None if std::io::stdin().is_terminal() => ask().await,
        None => {
            println!("refused: no terminal to approve it here (see --approve)");
            None
        }
    };
    let Some(choice) = choice else {
        return Ok(Err(if wrong_password {
            WRONG_PASSWORD
        } else {
            NOT_APPROVED
        }));
    };
    let control = choice.control;
    if !choice.remember {
        println!("not saved: this device will be asked about again next time");
        return Ok(Ok(control));
    }
    list.insert(
        &device,
        TrustedDevice {
            name,
            allow_control: control,
            legacy_fingerprint: None,
        },
    );
    list.save(&path)?;
    println!("saved to {}", path.display());
    Ok(Ok(control))
}

async fn ask() -> Option<Approve> {
    println!(
        "Allow it? [v] view only  [c] view and control  (vo, co: this session only, not remembered)  [anything else] refuse (60 s)"
    );
    let line = tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).map(|_| line)
    });
    match tokio::time::timeout(Duration::from_secs(60), line).await {
        Ok(Ok(Ok(line))) => answer(&line),
        _ => {
            println!("no answer: refused");
            None
        }
    }
}

/// The host's typed answer: `v`/`c`, or `vo`/`co` for this session only.
fn answer(line: &str) -> Option<Approve> {
    let line = line.trim().to_ascii_lowercase();
    let (kind, remember) = match line.strip_suffix('o') {
        Some(kind) => (kind, false),
        None => (line.as_str(), true),
    };
    let control = match kind {
        "v" => false,
        "c" => true,
        _ => return None,
    };
    Some(Approve { control, remember })
}

fn short(device: &iroh::EndpointId) -> String {
    device.to_string()[..8].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_answers() {
        let a = |control, remember| Some(Approve { control, remember });
        assert_eq!(answer("v\n"), a(false, true));
        assert_eq!(answer(" C "), a(true, true));
        assert_eq!(answer("vo"), a(false, false));
        assert_eq!(answer("CO\r\n"), a(true, false));
        for refused in ["", "o", "yes", "x", "voo", "cv"] {
            assert_eq!(answer(refused), None, "{refused:?}");
        }
        assert_eq!(Approve::parse("control-once"), a(true, false));
        assert_eq!(Approve::parse("view"), a(false, true));
        assert_eq!(Approve::parse("once"), None);
    }
}
