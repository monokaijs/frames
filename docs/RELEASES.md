# Releases

Run the **Release** workflow manually and choose `patch`, `minor`, or `major`. It increments the workspace version, builds both platforms, verifies packages, commits the version, pushes an atomic branch/tag update, and publishes the release. A failed build removes its incomplete draft.

```sh
gh workflow run release.yml --ref main -f bump=patch
gh run list --workflow release.yml
```

## Repository configuration

Secrets:

- `MACOS_CERTIFICATE_P12`: base64 Developer ID Application certificate and private key.
- `MACOS_CERTIFICATE_PASSWORD`: exported certificate password.
- `APPLE_API_KEY_P8`, `APPLE_API_KEY_ID`, `APPLE_API_ISSUER_ID`: App Store Connect notarization credentials.
- `UPDATE_PRIVATE_KEY`: Ed25519 PEM signing key matching `scripts/update-public-key.hex`.

Variable: `MACOS_SIGN_IDENTITY` (certificate fingerprint). The pinned Developer ID team is `V59Z6S9A36`, checked by both the signing script and updater; a team migration requires changing those checks deliberately.

The product and macOS executable are named **Frames**. The bundle identifier is **dev.creaton.frames**; its helper and login item use `dev.creaton.frames.helper` and `dev.creaton.frames.agent`. Native icons in `assets/icons` are generated from the approved white-frame artwork with `python scripts/generate_icons.py` (Pillow is needed only to regenerate them).

Windows releases include **Frames-VERSION-windows-x86_64-setup.exe**, built with Inno Setup 6. It installs per user to `%LOCALAPPDATA%\Programs\Frames`, with stable application identity `dev.creaton.frames`, a Start menu shortcut, an optional desktop shortcut, and an Installed Apps/uninstaller entry. Setup and uninstall refuse to replace an executable in use and do not close active sessions. The signed ZIP remains the updater payload; replacing an installed executable also refreshes its Installed Apps version. Portable copies do not modify installation metadata. Build locally with `python scripts/release.py windows-installer VERSION`; run `scripts/test_windows_installer.ps1` for isolated install, upgrade, locked-file, and uninstall checks. CI and release jobs run these checks before uploading the installer.

The workflow imports the certificate into a temporary keychain, signs with hardened runtime and a secure timestamp, submits to Apple, staples the ticket, and checks Gatekeeper acceptance. It then builds, signs, notarizes, and staples a **Frames-VERSION-macos-aarch64.dmg** containing Frames.app and an Applications shortcut, laid out in a designed drag-to-install window (`assets/installer`, regenerated with `python scripts/generate_dmg_background.py`, which needs Pillow; the layout constants live in `scripts/release.py`). The image is built with a pinned [dmgbuild](https://pypi.org/project/dmgbuild/), which writes the Finder layout without scripting Finder. The installer check mounts the DMG read-only, copies the app to a temporary Applications directory, verifies its signature and identity, and runs the copied executable. The signed ZIP remains the updater payload. Signing material is removed in an always-run cleanup step. Windows builds are currently unsigned by Authenticode.

Apple ticket stapling and validation retry CloudKit lookup failures (exit 68) up to five attempts, waiting 5, 10, 20, and 40 seconds between attempts. Other errors fail immediately; exhausted retries still block publication. This applies to both the app and DMG. After a failed release has removed its draft, start a new Release workflow run from `main` so the draft is recreated.

Packages upload directly to a draft GitHub Release; no Actions artifacts are created. A release publishes the two installers (DMG, setup EXE), the two signed ZIPs that every installed copy's updater downloads (the Windows ZIP also runs as a portable app), and the signed update manifest. SHA-256 checksums are listed in the release notes rather than as separate assets.

CI (`build.yml`) runs every regression check on each push to `main` and on pull requests. A release looks up the CI run for the commit it releases. If CI failed, the release stops at once. If CI passed or is still running, the release skips its copies of those checks, builds immediately, and `ci-gate` holds publishing until CI has passed. If CI never ran for the commit (for example, a documentation-only change), the release runs the checks itself. Release-only checks always run: installer install/upgrade, the signed macOS updater test, notarization, Gatekeeper and the DMG check.

Builds reuse dependency caches: one Rust cache per platform (`Swatinem/rust-cache`, saved only from `main`, without debug info) and the built macOS codec SDK, keyed on its pinned sources. Each CI run on `main` deletes superseded Rust caches (`scripts/prune_caches.sh`), keeping usage well inside the repository's 10 GB cache allowance; release jobs only restore. Runner files disappear when GitHub destroys the hosted runner.

## Automatic updates

Official builds (`FRAMES_RELEASE_BUILD=1`) check on launcher startup and every six hours. Background downloads defer during remote sessions and cancel when a new session starts. The Ed25519 signature pins the manifest to the bundled public key; archive size and SHA-256 are checked before staging and again before installation. Mac replacements must also match the Developer ID team and bundle identifier.

Updates stage in the user's cache and apply after every Frames process using the installation exits. Replacement uses a temporary directory on the same filesystem and restores the previous install if moving the new one fails. Automatic updates never stop sessions. A confirmed **Disconnect and update** closes sessions and other Frames processes from the exact installation being updated; other installed copies are excluded. The helper verifies and extracts the replacement before shutting down those remaining processes and still acquires the exclusive installation lease before replacing files. Installation must be writable by the current user; the updater does not elevate or replace a privileged Windows service.

`frames update --check` checks manually. `frames update` downloads and schedules the update. Development builds do not check automatically. Keep the update private key backed up securely: losing it requires a manually distributed build with a new pinned public key.

In **Settings**, Check for updates downloads during active sessions and shows progress and errors. After verification, Restart to update opens a confirmation explaining that sessions, sharing and file transfers will disconnect. Cancel or Escape leaves them running. Confirming prepares the helper before closing launcher-owned sessions, then reopens the installed replacement. On Mac, enabled sharing at login is temporarily unregistered to prevent launchd from respawning the old host, then registered again after installation (or on failure). An app running from a read-only DMG must first be copied to Applications. The updater does not request administrator access.

On macOS, the updater runs from a temporary copy of the complete signed app bundle. Its executable must stay with the signed Info.plist and resources so macOS can validate it before launch.

The pinned `quinn-udp` source in `vendor/quinn-udp` preserves IPv4 reply addresses on Macs with multiple active interfaces. To check both LAN addresses without opening a remote session or approving pairing, run `cargo run -p frames-core --example quic_probe -- IP1:47990 IP2:47990`. Both TLS probes must succeed and report the same host identity. The Mac regression test is `FRAMES_TEST_SOURCE_IPV4=IP1 FRAMES_TEST_RECEIVER_IPV4=IP2 cargo test --manifest-path vendor/quinn-udp/Cargo.toml wildcard_socket_preserves_secondary_ipv4_source -- --ignored` (both addresses must belong to that Mac).

Launcher-supervised sessions pass `FRAMES_LAUNCHER_PID` so Windows pairing and error dialogs remain owned by their launcher; current session windows take priority. On Windows, run the ignored `session_dialog_resolves_launcher_owner` native test with that variable set to a visible Frames launcher's process ID. macOS helper alerts use the modal panel level because they run outside the launcher's process.

Older builds used `app.frames.desktop`. Install this DMG manually once to migrate to `dev.creaton.frames`: the old updater intentionally rejects a different bundle identifier. Disable the old sharing login item before replacing the app and grant the new Frames identity its macOS privacy permissions. Pairing data is retained in its existing location.

For a local installer check on a Mac, run `bash native/macos/build.sh`, `python3 -m pip install dmgbuild==1.6.7` (in a virtual environment), `python3 scripts/release.py dmg VERSION`, and `python3 scripts/release.py verify-dmg dist/release/Frames-VERSION-macos-aarch64.dmg VERSION` (use the workspace version). A local Developer ID signed build can exercise the complete updater with `FRAMES_UPDATE_TEST_APP="$PWD/dist/macos/Frames.app" cargo test -p frames signed_mac_update_replaces_entire_bundle -- --ignored --nocapture`; the test uses temporary installation/cache directories and an isolated test signing key.

The F6 media format requires matching builds. Keep FEC/adaptation/GPU diagnostic switches available during acceptance. Run the media gates in [VALIDATION.md](VALIDATION.md) before claiming 1080p60 latency or uninterrupted frame delivery; smoke checks and codec timings do not certify those gates.
