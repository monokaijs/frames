//! Who may connect to this host, and this device's stable identity.
//!
//! The host lets a device in when the trust list says so, or after the
//! person at the host compares the pairing code shown on both screens and
//! approves it here. A host without a terminal rejects unknown devices unless
//! it was started with `--approve view|control`, which is for unattended
//! test runs only and says so when used.

use anyhow::{Context, Result};
use frames_net::trust::{Admission, TrustList, TrustedDevice, admission};
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

/// `Some(control)` to stream (with input when `control`), `None` to refuse.
pub async fn decide(
    config: &std::path::Path,
    session: &frames_net::Session,
    preset: Option<Approve>,
) -> Result<Option<bool>> {
    let path = TrustList::path_in(config);
    let mut list = TrustList::load(&path)?;
    let device = session.remote();
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
