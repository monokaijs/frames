//! Signed release downloads, applied only after every process in this install exits.
use anyhow::{Context, Result, bail, ensure};
use ed25519_dalek::{Signature, VerifyingKey};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

const REPOSITORY: &str = "monokaijs/frames";
const MAX_ARCHIVE: u64 = 256 * 1024 * 1024;
const MAX_EXPANDED: u64 = 512 * 1024 * 1024;
const PUBLIC_KEY: &str = include_str!("../../../scripts/update-public-key.hex");
static RELAUNCH: AtomicBool = AtomicBool::new(false);

#[cfg(windows)]
#[path = "windows_install.rs"]
mod windows_install;

pub enum Status {
    Checking,
    Downloading(u64, u64),
    Ready(String),
    Deferred,
    Current,
    Error(String),
}

pub fn request_restart() {
    RELAUNCH.store(true, Ordering::Relaxed);
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Asset {
    url: String,
    sha256: String,
    size: u64,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: u32,
    version: String,
    targets: BTreeMap<String, Asset>,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    version: String,
    target: PathBuf,
    bundle: bool,
    archive: PathBuf,
    manifest: PathBuf,
    signature: PathBuf,
    previous_sha256: String,
}

fn root() -> Result<PathBuf> {
    let root = directories::ProjectDirs::from("app", "Frames", "Frames")
        .context("no update cache directory")?
        .cache_dir()
        .join("updates");
    fs::create_dir_all(&root)?;
    Ok(root.canonicalize()?)
}
fn installation() -> Result<(PathBuf, bool)> {
    let executable = std::env::current_exe()?.canonicalize()?;
    #[cfg(target_os = "macos")]
    {
        let app = executable
            .ancestors()
            .find(|p| p.extension().is_some_and(|e| e == "app"))
            .context("automatic updates require a packaged Frames.app")?;
        ensure!(
            executable == app.join("Contents/MacOS/Frames"),
            "unexpected app executable"
        );
        Ok((app.to_owned(), true))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok((executable, false))
    }
}
fn installed_binary(target: &Path, bundle: bool) -> PathBuf {
    if bundle {
        target.join("Contents/MacOS/Frames")
    } else {
        target.to_owned()
    }
}
fn lease_path(root: &Path, target: &Path) -> PathBuf {
    root.join(format!(
        "run-{}.lock",
        hex::encode(Sha256::digest(target.to_string_lossy().as_bytes()))
    ))
}
fn lock_file(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?)
}
pub fn runtime_lease() -> Result<Option<File>> {
    let (target, _) = match installation() {
        Ok(installation) => installation,
        Err(_) if option_env!("FRAMES_RELEASE_BUILD") != Some("1") => return Ok(None),
        Err(error) => return Err(error),
    };
    let file = lock_file(&lease_path(&root()?, &target))?;
    fs2::FileExt::lock_shared(&file)?;
    Ok(Some(file))
}

// Session workers hold this independently of the launcher. An update download
// yields as soon as a session starts, including one from another launcher.
pub fn session_lease() -> Result<Option<File>> {
    let file = lock_file(&root()?.join("sessions.lock"))?;
    fs2::FileExt::lock_shared(&file)?;
    Ok(Some(file))
}

fn require_idle(file: &File) -> Result<()> {
    fs2::FileExt::try_lock_exclusive(file).map_err(|_| SessionActive)?;
    fs2::FileExt::unlock(file)?;
    Ok(())
}

#[derive(Debug)]
struct SessionActive;
impl std::fmt::Display for SessionActive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("update download deferred during a session")
    }
}
impl std::error::Error for SessionActive {}
fn platform() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => Ok("windows-x86_64"),
        ("macos", "aarch64") => Ok("macos-aarch64"),
        _ => bail!("no release package for this platform"),
    }
}
fn digest(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(hex::encode(hash.finalize()))
}
fn verify_with_key(bytes: &[u8], signature: &[u8], key: &[u8; 32]) -> Result<Manifest> {
    ensure!(
        bytes.len() <= 65536 && signature.len() == 64,
        "invalid update manifest size"
    );
    VerifyingKey::from_bytes(key)?.verify_strict(bytes, &Signature::from_slice(signature)?)?;
    let manifest: Manifest = serde_json::from_slice(bytes)?;
    let version = Version::parse(&manifest.version)?;
    ensure!(
        manifest.schema == 1 && version.pre.is_empty() && version.build.is_empty(),
        "unsupported update manifest"
    );
    Ok(manifest)
}
fn verify(bytes: &[u8], signature: &[u8]) -> Result<Manifest> {
    let key: [u8; 32] = hex::decode(PUBLIC_KEY.trim())?
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid update public key"))?;
    verify_with_key(bytes, signature, &key)
}
fn validate_asset(manifest: &Manifest) -> Result<&Asset> {
    let platform = platform()?;
    let asset = manifest
        .targets
        .get(platform)
        .context("release has no package for this platform")?;
    let expected = format!(
        "https://github.com/{REPOSITORY}/releases/download/v{}/Frames-{}-{platform}.zip",
        manifest.version, manifest.version
    );
    ensure!(
        asset.url == expected && asset.size > 0 && asset.size <= MAX_ARCHIVE,
        "invalid release asset"
    );
    ensure!(
        asset.sha256.len() == 64 && asset.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid package checksum"
    );
    Ok(asset)
}
fn http() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .user_agent(concat!("Frames/", env!("CARGO_PKG_VERSION")))
        .https_only(true)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .build()?)
}
fn small_get(client: &reqwest::blocking::Client, url: &str, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    client
        .get(url)
        .send()?
        .error_for_status()?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= limit, "update response too large");
    Ok(bytes)
}
fn check(download: bool, background: bool, notify: &impl Fn(Status)) -> Result<Option<String>> {
    let root = root()?;
    let lock = lock_file(&root.join("download.lock"))?;
    if fs2::FileExt::try_lock_exclusive(&lock).is_err() {
        bail!("Another Frames update check is already running. Try again shortly.");
    }
    let client = http()?;
    let base = format!("https://github.com/{REPOSITORY}/releases/latest/download");
    let bytes = small_get(&client, &format!("{base}/update.json"), 65536)?;
    let signature = small_get(&client, &format!("{base}/update.json.sig"), 64)?;
    let manifest = verify(&bytes, &signature)?;
    if Version::parse(&manifest.version)? <= Version::parse(env!("CARGO_PKG_VERSION"))? {
        return Ok(None);
    }
    let asset = validate_asset(&manifest)?;
    if !download {
        return Ok(Some(manifest.version));
    }
    let idle = if background {
        let file = lock_file(&root.join("sessions.lock"))?;
        require_idle(&file)?;
        Some(file)
    } else {
        None
    };
    let (target, bundle) = installation()?;
    // Test writability before downloading. In particular, a mounted DMG is read-only.
    tempfile::Builder::new().prefix(".frames-write-check-")
        .tempdir_in(target.parent().context("installation has no parent")?)
        .context(if bundle {
            "Frames cannot update here. Move Frames.app to a writable Applications folder and try again"
        } else {
            "Frames cannot update here. Move Frames.exe to a writable folder and try again"
        })?;
    let directory = root.join(&manifest.version);
    fs::create_dir_all(&directory)?;
    let archive = directory.join(format!("{}.zip", platform()?));
    notify(Status::Downloading(0, asset.size));
    if !archive.is_file() || digest(&archive)? != asset.sha256 {
        let mut file = tempfile::NamedTempFile::new_in(&directory)?;
        let response = client.get(&asset.url).send()?.error_for_status()?;
        if let Some(size) = response.content_length() {
            ensure!(size == asset.size, "package size changed");
        }
        let mut response = response.take(MAX_ARCHIVE + 1);
        let mut copied = 0;
        let mut buffer = [0u8; 65536];
        loop {
            if let Some(idle) = &idle {
                require_idle(idle)?;
            }
            let size = response.read(&mut buffer)?;
            if size == 0 {
                break;
            }
            file.write_all(&buffer[..size])?;
            copied += size as u64;
            ensure!(
                copied <= asset.size,
                "update download exceeded the signed size"
            );
            notify(Status::Downloading(copied, asset.size));
        }
        ensure!(
            copied == asset.size && digest(file.path())? == asset.sha256,
            "release package verification failed"
        );
        file.persist(&archive).map_err(|e| e.error)?;
    }
    let manifest_path = directory.join("update.json");
    let signature_path = directory.join("update.json.sig");
    fs::write(&manifest_path, &bytes)?;
    fs::write(&signature_path, &signature)?;
    let pending = Pending {
        version: manifest.version.clone(),
        previous_sha256: digest(&installed_binary(&target, bundle))?,
        target,
        bundle,
        archive,
        manifest: manifest_path,
        signature: signature_path,
    };
    let mut file = tempfile::NamedTempFile::new_in(&root)?;
    file.write_all(&serde_json::to_vec(&pending)?)?;
    file.persist(root.join("pending.json"))
        .map_err(|e| e.error)?;
    Ok(Some(manifest.version))
}
pub fn start(notify: impl Fn(Status) + Send + 'static) -> mpsc::Sender<()> {
    let (requests, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut automatic = option_env!("FRAMES_RELEASE_BUILD") == Some("1");
        let mut interval = Duration::from_secs(6 * 60 * 60);
        loop {
            if !automatic {
                match receiver.recv_timeout(interval) {
                    Ok(()) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if option_env!("FRAMES_RELEASE_BUILD") != Some("1") {
                            continue;
                        }
                    }
                }
            }
            automatic = false;
            interval = Duration::from_secs(6 * 60 * 60);
            notify(Status::Checking);
            match check(true, true, &notify) {
                Ok(Some(version)) => notify(Status::Ready(version)),
                Ok(None) => notify(Status::Current),
                Err(error) if error.is::<SessionActive>() => {
                    interval = Duration::from_secs(60);
                    notify(Status::Deferred);
                }
                Err(error) => notify(Status::Error(format!("{error:#}"))),
            }
        }
    });
    requests
}
pub fn manual(check_only: bool) -> Result<()> {
    match check(!check_only, false, &|_| {})? {
        Some(version) if check_only => println!("Frames {version} is available."),
        Some(version) => {
            println!(
                "Frames {version} is verified and will install after all Frames sessions close."
            );
            install_on_exit()?;
        }
        None => println!("Frames is up to date."),
    }
    Ok(())
}
pub fn install_on_exit() -> Result<()> {
    let root = root()?;
    let pending = root.join("pending.json");
    if !pending.is_file() {
        return Ok(());
    }
    let (target, _) = installation()?;
    let plan: Pending = serde_json::from_slice(&fs::read(&pending)?)?;
    if plan.target != target {
        return Ok(());
    }
    // Remove old helper copies on the next launch; Windows cannot unlink its
    // currently executing helper. No user files outside this cache are touched.
    for entry in fs::read_dir(&root)? {
        let path = entry?.path();
        if path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("apply-helper-"))
            && path.is_file()
        {
            let _ = fs::remove_file(path);
        }
    }
    let helper = prepare_update_helper(&root, &target)?;
    let mut command = Command::new(&helper);
    command
        .arg("apply-update")
        .arg(&pending)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(root.join("install.log"))?,
        ));
    if RELAUNCH.load(Ordering::Relaxed) {
        command.arg("--relaunch");
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command.spawn()?;
    Ok(())
}

fn prepare_update_helper(root: &Path, target: &Path) -> Result<PathBuf> {
    let name = format!("apply-helper-{}", uuid::Uuid::new_v4());
    #[cfg(target_os = "macos")]
    {
        // A signed app executable seals its Info.plist. Copying only the Mach-O
        // makes AMFI reject it before main(), so preserve the complete bundle.
        let bundle = root.join(format!("{name}.app"));
        let result = (|| {
            ensure!(
                Command::new("/usr/bin/ditto")
                    .arg(target)
                    .arg(&bundle)
                    .status()?
                    .success(),
                "could not prepare the updater app bundle"
            );
            ensure!(
                Command::new("/usr/bin/codesign")
                    .args(["--verify", "--deep", "--strict"])
                    .arg(&bundle)
                    .status()?
                    .success(),
                "updater app bundle signature is invalid"
            );
            Ok(installed_binary(&bundle, true))
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&bundle);
        }
        result
    }
    #[cfg(not(target_os = "macos"))]
    {
        let helper = root.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        fs::copy(target, &helper)?;
        Ok(helper)
    }
}
fn extract(archive: &Path, destination: &Path, bundle: bool) -> Result<PathBuf> {
    let mut zip = zip::ZipArchive::new(File::open(archive)?)?;
    ensure!(zip.len() <= 2048, "too many update entries");
    let mut total = 0u64;
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index)?;
        let path = entry
            .enclosed_name()
            .context("unsafe update archive path")?;
        ensure!(
            entry
                .unix_mode()
                .is_none_or(|mode| mode & 0o170000 != 0o120000),
            "update symlinks are not accepted"
        );
        if bundle {
            if path.starts_with("__MACOSX") {
                continue;
            }
            ensure!(
                path.starts_with("Frames.app"),
                "unexpected app archive entry"
            );
        } else if path != Path::new("Frames.exe") {
            continue;
        }
        total = total
            .checked_add(entry.size())
            .context("archive size overflow")?;
        ensure!(total <= MAX_EXPANDED, "expanded update is too large");
        let output = destination.join(path);
        if entry.is_dir() {
            fs::create_dir_all(&output)?;
            continue;
        }
        fs::create_dir_all(output.parent().context("missing archive parent")?)?;
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&output)?;
        let expected = entry.size();
        ensure!(
            std::io::copy(&mut entry.by_ref().take(MAX_EXPANDED + 1), &mut file)? == expected,
            "truncated update entry"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                &output,
                fs::Permissions::from_mode(entry.unix_mode().unwrap_or(0o644) & 0o755),
            )?;
        }
    }
    let replacement = destination.join(if bundle { "Frames.app" } else { "Frames.exe" });
    ensure!(
        installed_binary(&replacement, bundle).is_file(),
        "release package has no Frames executable"
    );
    Ok(replacement)
}
pub fn apply(path: &Path, relaunch: bool) -> Result<()> {
    apply_in(&root()?, path, relaunch, verify)
}

fn apply_in(
    root: &Path,
    path: &Path,
    relaunch: bool,
    verify_manifest: impl Fn(&[u8], &[u8]) -> Result<Manifest>,
) -> Result<()> {
    ensure!(
        path.canonicalize()? == root.join("pending.json"),
        "unexpected update plan"
    );
    let download_lock = lock_file(&root.join("download.lock"))?;
    if fs2::FileExt::try_lock_exclusive(&download_lock).is_err() {
        return Ok(());
    }
    let plan: Pending = serde_json::from_slice(&fs::read(path)?)?;
    for file in [&plan.archive, &plan.manifest, &plan.signature] {
        ensure!(
            file.canonicalize()?.starts_with(root),
            "update file outside cache"
        );
    }
    let manifest = verify_manifest(&fs::read(&plan.manifest)?, &fs::read(&plan.signature)?)?;
    ensure!(manifest.version == plan.version, "update version changed");
    ensure!(
        Version::parse(&manifest.version)? > Version::parse(env!("CARGO_PKG_VERSION"))?,
        "update is not newer than the installed version"
    );
    let asset = validate_asset(&manifest)?;
    ensure!(
        fs::metadata(&plan.archive)?.len() == asset.size && digest(&plan.archive)? == asset.sha256,
        "cached package verification failed"
    );
    ensure!(
        plan.bundle == cfg!(target_os = "macos"),
        "wrong installation type"
    );
    ensure!(
        plan.target.is_absolute() && plan.target.canonicalize()? == plan.target,
        "invalid installation path"
    );
    let lease = lock_file(&lease_path(root, &plan.target))?;
    let deadline = Instant::now() + Duration::from_secs(120);
    while fs2::FileExt::try_lock_exclusive(&lease).is_err() {
        if Instant::now() >= deadline {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    ensure!(
        digest(&installed_binary(&plan.target, plan.bundle))? == plan.previous_sha256,
        "installation changed since download"
    );
    let parent = plan.target.parent().context("installation has no parent")?;
    let temporary = tempfile::Builder::new()
        .prefix(".frames-update-")
        .tempdir_in(parent)?;
    let replacement = extract(&plan.archive, temporary.path(), plan.bundle)?;
    #[cfg(target_os = "macos")]
    {
        let requirement = "=anchor apple generic and identifier \"dev.creaton.frames\" and certificate leaf[subject.OU] = \"V59Z6S9A36\"";
        ensure!(
            Command::new("/usr/bin/codesign")
                .args(["--verify", "--deep", "--strict", "-R", requirement])
                .arg(&replacement)
                .status()?
                .success(),
            "updated app signature is invalid"
        );
    }
    let backup = parent.join(format!(".frames-backup-{}", uuid::Uuid::new_v4()));
    replace_installation(&plan.target, &replacement, &backup)?;
    #[cfg(windows)]
    if let Err(error) = windows_install::refresh(&plan.target, &plan.version) {
        tracing::warn!(%error, "could not refresh Frames Installed Apps version");
    }
    ensure!(
        backup.parent() == Some(parent)
            && backup
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(".frames-backup-")),
        "invalid backup path"
    );
    if plan.bundle {
        fs::remove_dir_all(&backup)?;
    } else {
        fs::remove_file(&backup)?;
    }
    fs::remove_file(path)?;
    for file in [&plan.archive, &plan.manifest, &plan.signature] {
        let _ = fs::remove_file(file);
    }
    if let Some(directory) = plan.archive.parent() {
        let _ = fs::remove_dir(directory);
    }
    // Release the installation lease before launching the replacement, which
    // acquires its own shared lease before opening any windows.
    drop(lease);
    drop(download_lock);
    if relaunch {
        #[cfg(target_os = "macos")]
        ensure!(
            Command::new("/usr/bin/open")
                .arg("-n")
                .arg(&plan.target)
                .status()?
                .success(),
            "could not reopen Frames"
        );
        #[cfg(not(target_os = "macos"))]
        Command::new(&plan.target)
            .spawn()
            .context("could not reopen Frames")?;
    }
    #[cfg(unix)]
    {
        let executable = std::env::current_exe()?;
        #[cfg(target_os = "macos")]
        if let Some(bundle) = executable.ancestors().find(|path| {
            path.parent() == Some(root)
                && path.extension().is_some_and(|ext| ext == "app")
                && path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("apply-helper-"))
        }) {
            let _ = fs::remove_dir_all(bundle);
        }
        if executable.parent() == Some(root)
            && executable
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("apply-helper-"))
        {
            let _ = fs::remove_file(executable);
        }
    }
    Ok(())
}

fn replace_installation(target: &Path, replacement: &Path, backup: &Path) -> Result<()> {
    fs::rename(target, backup)?;
    if let Err(error) = fs::rename(replacement, target) {
        fs::rename(backup, target)
            .context("update failed; restore the .frames-backup file manually")?;
        return Err(error.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_updater_preserves_the_signed_bundle_and_launches() {
        let temp = tempfile::tempdir().unwrap();
        let app = temp.path().join("Frames.app");
        let binary = installed_binary(&app, true);
        fs::create_dir_all(binary.parent().unwrap()).unwrap();
        fs::write(
            app.join("Contents/Info.plist"),
            br#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>dev.frames.update-test</string>
<key>CFBundleExecutable</key><string>Frames</string>
<key>CFBundlePackageType</key><string>APPL</string>
</dict></plist>"#,
        )
        .unwrap();
        let source = temp.path().join("main.c");
        fs::write(&source, "int main(void) { return 0; }\n").unwrap();
        assert!(
            Command::new("/usr/bin/clang")
                .arg(&source)
                .arg("-o")
                .arg(&binary)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("/usr/bin/codesign")
                .args(["--force", "--sign", "-"])
                .arg(&app)
                .status()
                .unwrap()
                .success()
        );
        let cache = temp.path().join("cache");
        fs::create_dir(&cache).unwrap();

        let helper = prepare_update_helper(&cache, &app).unwrap();
        let bundle = helper
            .ancestors()
            .find(|path| path.extension().is_some_and(|ext| ext == "app"))
            .unwrap();
        assert_ne!(bundle, app);
        assert_eq!(
            fs::read(bundle.join("Contents/Info.plist")).unwrap(),
            fs::read(app.join("Contents/Info.plist")).unwrap()
        );
        assert!(
            Command::new(&helper).status().unwrap().success(),
            "macOS must accept the copied updater's signature"
        );
        // This is the old launch path: AMFI rejects a signed app executable
        // detached from the Info.plist sealed by its signature.
        let detached = cache.join("detached-updater");
        fs::copy(&binary, &detached).unwrap();
        assert!(
            !Command::new("/usr/bin/codesign")
                .args(["--verify", "--strict"])
                .arg(&detached)
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn mac_bundle_extracts_frames_and_rejects_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let archive = root.path().join("update.zip");
        let mut writer = zip::ZipWriter::new(File::create(&archive).unwrap());
        writer
            .start_file(
                "Frames.app/Contents/MacOS/Frames",
                zip::write::SimpleFileOptions::default().unix_permissions(0o755),
            )
            .unwrap();
        writer.write_all(b"Frames").unwrap();
        writer.finish().unwrap();
        let stage = root.path().join("stage");
        fs::create_dir(&stage).unwrap();
        let app = extract(&archive, &stage, true).unwrap();
        assert_eq!(fs::read(installed_binary(&app, true)).unwrap(), b"Frames");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(installed_binary(&app, true))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o755
            );
        }
        let mut writer = zip::ZipWriter::new(File::create(&archive).unwrap());
        writer
            .add_symlink(
                "Frames.app/Contents/Resources/link",
                "/tmp/outside",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        writer.finish().unwrap();
        assert!(
            extract(&archive, &root.path().join("other"), true)
                .unwrap_err()
                .to_string()
                .contains("symlinks")
        );
    }

    fn prepare_plan(cache: &Path, target: PathBuf, archive: PathBuf, bundle: bool) -> Pending {
        let key = SigningKey::from_bytes(&[42; 32]);
        let mut version = Version::parse(env!("CARGO_PKG_VERSION")).unwrap();
        version.patch += 1;
        let version = version.to_string();
        let platform = platform().unwrap();
        let manifest = Manifest {
            schema: 1,
            version: version.clone(),
            targets: BTreeMap::from([(
                platform.to_string(),
                Asset {
                    url: format!(
                        "https://github.com/{REPOSITORY}/releases/download/v{version}/Frames-{version}-{platform}.zip"
                    ),
                    sha256: digest(&archive).unwrap(),
                    size: fs::metadata(&archive).unwrap().len(),
                },
            )]),
        };
        let bytes = serde_json::to_vec(&manifest).unwrap();
        let plan = Pending {
            version,
            previous_sha256: digest(&installed_binary(&target, bundle)).unwrap(),
            target,
            bundle,
            archive,
            manifest: cache.join("update.json"),
            signature: cache.join("update.json.sig"),
        };
        fs::write(&plan.manifest, &bytes).unwrap();
        fs::write(&plan.signature, key.sign(&bytes).to_bytes()).unwrap();
        fs::write(
            cache.join("pending.json"),
            serde_json::to_vec(&plan).unwrap(),
        )
        .unwrap();
        plan
    }

    fn test_verifier(bytes: &[u8], signature: &[u8]) -> Result<Manifest> {
        verify_with_key(
            bytes,
            signature,
            &SigningKey::from_bytes(&[42; 32]).verifying_key().to_bytes(),
        )
    }

    #[cfg(windows)]
    #[test]
    fn signed_update_applies_and_tampered_archive_preserves_installation() {
        for tamper in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let root = root.path().canonicalize().unwrap();
            let cache = root.join("cache");
            fs::create_dir(&cache).unwrap();
            let target = root.join("Frames.exe");
            fs::write(&target, b"old").unwrap();
            let archive = cache.join("update.zip");
            let mut writer = zip::ZipWriter::new(File::create(&archive).unwrap());
            writer
                .start_file("Frames.exe", zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"updated").unwrap();
            writer.finish().unwrap();
            prepare_plan(&cache, target.clone(), archive.clone(), false);
            if tamper {
                fs::write(&archive, b"tampered").unwrap();
            }
            let result = apply_in(&cache, &cache.join("pending.json"), false, test_verifier);
            assert_eq!(result.is_err(), tamper);
            assert_eq!(
                fs::read(&target).unwrap(),
                if tamper {
                    b"old".as_slice()
                } else {
                    b"updated".as_slice()
                }
            );
            assert_eq!(cache.join("pending.json").exists(), tamper);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires FRAMES_UPDATE_TEST_APP pointing to a Developer ID signed Frames.app"]
    fn signed_mac_update_replaces_entire_bundle() {
        let fixture =
            PathBuf::from(std::env::var_os("FRAMES_UPDATE_TEST_APP").expect("signed app fixture"));
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let cache = root.join("cache");
        fs::create_dir(&cache).unwrap();
        let target = root.join("Frames.app");
        assert!(
            Command::new("ditto")
                .arg(&fixture)
                .arg(&target)
                .status()
                .unwrap()
                .success()
        );
        fs::write(
            target.join("Contents/Resources/obsolete-test-file"),
            b"old resource",
        )
        .unwrap();
        let archive = cache.join("update.zip");
        assert!(
            Command::new("ditto")
                .args(["-c", "-k", "--sequesterRsrc", "--keepParent"])
                .arg(&fixture)
                .arg(&archive)
                .status()
                .unwrap()
                .success()
        );
        prepare_plan(&cache, target.clone(), archive, true);
        apply_in(&cache, &cache.join("pending.json"), false, test_verifier).unwrap();
        assert!(
            !target
                .join("Contents/Resources/obsolete-test-file")
                .exists()
        );
        assert!(!cache.join("pending.json").exists());
        if fixture.join("Contents/CodeResources").is_file() {
            assert!(
                Command::new("xcrun")
                    .args(["stapler", "validate"])
                    .arg(&target)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        assert!(
            Command::new("codesign")
                .args(["--verify", "--deep", "--strict"])
                .arg(target)
                .status()
                .unwrap()
                .success()
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires FRAMES_UPDATE_TEST_APP pointing to a built Frames.app"]
    fn invalid_mac_signature_preserves_existing_bundle() {
        let fixture =
            PathBuf::from(std::env::var_os("FRAMES_UPDATE_TEST_APP").expect("app fixture"));
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let cache = root.join("cache");
        fs::create_dir(&cache).unwrap();
        let target = root.join("Frames.app");
        ensure_copy(&fixture, &target);
        fs::write(
            target.join("Contents/Resources/keep-existing-test-file"),
            b"existing",
        )
        .unwrap();
        let bad = root.join("bad/Frames.app");
        ensure_copy(&fixture, &bad);
        fs::write(bad.join("Contents/Info.plist"), b"broken signature").unwrap();
        let archive = cache.join("update.zip");
        assert!(
            Command::new("ditto")
                .args(["-c", "-k", "--sequesterRsrc", "--keepParent"])
                .arg(bad)
                .arg(&archive)
                .status()
                .unwrap()
                .success()
        );
        prepare_plan(&cache, target.clone(), archive, true);
        assert!(
            apply_in(&cache, &cache.join("pending.json"), false, test_verifier)
                .unwrap_err()
                .to_string()
                .contains("signature")
        );
        assert_eq!(
            fs::read(target.join("Contents/Resources/keep-existing-test-file")).unwrap(),
            b"existing"
        );
        assert!(cache.join("pending.json").exists());
    }

    #[cfg(target_os = "macos")]
    fn ensure_copy(source: &Path, destination: &Path) {
        assert!(
            Command::new("ditto")
                .arg(source)
                .arg(destination)
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn replacement_failure_restores_existing_installation() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("Frames.exe");
        let backup = root.path().join("backup");
        fs::write(&target, b"existing").unwrap();
        assert!(replace_installation(&target, &root.path().join("missing"), &backup).is_err());
        assert_eq!(fs::read(target).unwrap(), b"existing");
        assert!(!backup.exists());
    }

    #[test]
    fn active_process_lease_prevents_installation() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("run.lock");
        let running = lock_file(&path).unwrap();
        let installer = lock_file(&path).unwrap();
        fs2::FileExt::lock_shared(&running).unwrap();
        assert!(fs2::FileExt::try_lock_exclusive(&installer).is_err());
        drop(running);
        // A concurrently spawning Unix child can briefly inherit this lease
        // between fork and exec, when CLOEXEC closes its inherited descriptor.
        let deadline = Instant::now() + Duration::from_secs(2);
        while fs2::FileExt::try_lock_exclusive(&installer).is_err() {
            assert!(Instant::now() < deadline, "lease remained held after exit");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn signed_manifest_rejects_unknown_schema_and_prerelease() {
        let key = SigningKey::from_bytes(&[42; 32]);
        for bytes in [
            br#"{"schema":2,"version":"0.1.2","targets":{}}"#.as_slice(),
            br#"{"schema":1,"version":"0.1.2-beta","targets":{}}"#.as_slice(),
        ] {
            let signature = key.sign(bytes).to_bytes();
            assert!(verify_with_key(bytes, &signature, &key.verifying_key().to_bytes()).is_err());
        }
    }

    #[test]
    fn valid_windows_archive_extracts_only_the_executable() {
        let root = tempfile::tempdir().unwrap();
        let archive = root.path().join("update.zip");
        let mut writer = zip::ZipWriter::new(File::create(&archive).unwrap());
        for (name, bytes) in [
            ("Frames.exe", b"binary".as_slice()),
            ("README.md", b"docs".as_slice()),
        ] {
            writer
                .start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();
        let stage = root.path().join("stage");
        fs::create_dir(&stage).unwrap();
        let executable = extract(&archive, &stage, false).unwrap();
        assert_eq!(fs::read(executable).unwrap(), b"binary");
        assert!(!stage.join("README.md").exists());
    }
    #[test]
    fn forged_and_modified_manifests_are_rejected() {
        let key = SigningKey::from_bytes(&[42; 32]);
        let bytes = br#"{"schema":1,"version":"0.1.2","targets":{}}"#;
        let signature = key.sign(bytes).to_bytes();
        assert!(verify_with_key(bytes, &signature, &key.verifying_key().to_bytes()).is_ok());
        assert!(
            verify_with_key(
                br#"{"schema":1,"version":"9.0.0","targets":{}}"#,
                &signature,
                &key.verifying_key().to_bytes()
            )
            .is_err()
        );
        assert!(verify_with_key(bytes, &[0; 64], &key.verifying_key().to_bytes()).is_err());
    }
    #[test]
    fn zip_traversal_is_rejected_before_writing_outside_staging() {
        let root = tempfile::tempdir().unwrap();
        let archive = root.path().join("update.zip");
        let mut writer = zip::ZipWriter::new(File::create(&archive).unwrap());
        writer
            .start_file("../escape.exe", zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"invalid").unwrap();
        writer.finish().unwrap();
        let stage = root.path().join("stage");
        fs::create_dir(&stage).unwrap();
        assert!(extract(&archive, &stage, false).is_err());
        assert!(!root.path().join("escape.exe").exists());
    }
}
