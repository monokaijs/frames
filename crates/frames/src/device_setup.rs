//! Optional, pinned upstream driver installers. No installer runs at app startup.
use serde::Deserialize;

pub const PACKAGES: &str = include_str!("../../../assets/device-packages.json");
#[derive(Deserialize)]
struct Package {
    target: String,
    file: String,
    url: String,
    sha256: String,
}

pub fn launch(target: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        ["gamepad", "usb-export", "usb-host"].contains(&target),
        "Unknown device setup target"
    );
    frames_native::usb::elevate(&format!("devices setup {target}"))
}

pub async fn install(target: &str) -> anyhow::Result<()> {
    anyhow::ensure!(cfg!(windows), "Device driver setup requires Windows");
    let package = serde_json::from_str::<Vec<Package>>(PACKAGES)?
        .into_iter()
        .find(|p| p.target == target)
        .ok_or_else(|| anyhow::anyhow!("Unknown device setup target"))?;
    #[cfg(windows)]
    {
        if !unsafe { windows::Win32::UI::Shell::IsUserAnAdmin() }.as_bool() {
            return launch(target);
        }
    }
    let present = match target {
        "gamepad" => frames_native::gamepad::host_available().is_ok(),
        "usb-host" => frames_native::usb::host_available().is_ok(),
        "usb-export" => frames_native::usb::inventory().is_ok(),
        _ => false,
    };
    if !present {
        anyhow::ensure!(
            !frames_native::usb::package_installed(target)?,
            "Device support is already installed but unavailable. Repair it through Windows Installed Apps, then refresh devices. Frames will not downgrade an existing package."
        );
        use sha2::{Digest, Sha256};
        use tokio::io::AsyncWriteExt;
        let directory = frames_core::trust::config_dir()?.join("device-setup");
        tokio::fs::create_dir_all(&directory).await?;
        let path = directory.join(&package.file);
        let mut response = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(180))
            .build()?
            .get(&package.url)
            .send()
            .await?
            .error_for_status()?;
        let temporary = directory.join(format!("{}.partial", package.file));
        let mut file = tokio::fs::File::create(&temporary).await?;
        let mut hash = Sha256::new();
        let mut total = 0usize;
        while let Some(bytes) = response.chunk().await? {
            total += bytes.len();
            anyhow::ensure!(total <= 128 * 1024 * 1024, "Device installer is too large");
            hash.update(&bytes);
            file.write_all(&bytes).await?;
        }
        file.sync_all().await?;
        drop(file);
        anyhow::ensure!(
            hex::encode(hash.finalize()) == package.sha256,
            "Device installer checksum verification failed"
        );
        tokio::fs::rename(&temporary, &path).await?;
        tokio::task::spawn_blocking(move || run_installer(&path)).await??;
    }
    if target == "usb-export" {
        tokio::task::spawn_blocking(frames_native::usb::configure_isolation).await??;
    }
    Ok(())
}

fn run_installer(path: &std::path::Path) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let verified = std::process::Command::new(frames_native::usb::system_tool("WindowsPowerShell/v1.0/powershell.exe")?).args(["-NoProfile", "-NonInteractive", "-Command",
            "if ((Get-AuthenticodeSignature -LiteralPath $env:FRAMES_DEVICE_INSTALLER).Status -ne 'Valid') { throw 'Device installer signature is invalid' }"])
            .env("FRAMES_DEVICE_INSTALLER", path).creation_flags(0x08000000).output()?;
        anyhow::ensure!(
            verified.status.success(),
            "Device installer signature verification failed"
        );
        let status = if path.extension().is_some_and(|e| e == "msi") {
            std::process::Command::new(frames_native::usb::system_tool("msiexec.exe")?)
                .arg("/i")
                .arg(path)
                .status()?
        } else {
            std::process::Command::new(path).status()?
        };
        anyhow::ensure!(
            status.success() || status.code() == Some(3010),
            "Device installation was cancelled or failed"
        );
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        anyhow::bail!("Driver installation requires Windows")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_driver_packages_are_pinned_https_release_assets() {
        let packages: Vec<Package> = serde_json::from_str(PACKAGES).unwrap();
        assert_eq!(packages.len(), 3);
        for package in packages {
            assert!(package.url.starts_with("https://github.com/"));
            assert!(package.url.contains("/releases/download/"));
            assert_eq!(package.sha256.len(), 64);
            assert!(package.sha256.bytes().all(|b| b.is_ascii_hexdigit()));
            assert!(!package.file.contains(['/', '\\']));
        }
    }
}
