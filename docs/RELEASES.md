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

The workflow imports the certificate into a temporary keychain, signs with hardened runtime and a secure timestamp, submits to Apple, staples the ticket, and checks Gatekeeper acceptance. It then builds, signs, notarizes, and staples a **Frames-VERSION-macos-aarch64.dmg** containing Frames.app and an Applications shortcut. The installer check mounts the DMG read-only, copies the app to a temporary Applications directory, verifies its signature and identity, and runs the copied executable. The signed ZIP remains the updater payload. Signing material is removed in an always-run cleanup step. Windows builds are currently unsigned by Authenticode.

Apple ticket stapling and validation retry CloudKit lookup failures (exit 68) up to five attempts, waiting 5, 10, 20, and 40 seconds between attempts. Other errors fail immediately; exhausted retries still block publication. This applies to both the app and DMG. After a failed release has removed its draft, start a new Release workflow run from `main` so the draft is recreated.

Packages upload directly to a draft GitHub Release. No Actions artifacts or dependency caches are created. Ordinary CI runs on pull requests or manual dispatch; release builds run the checks themselves, avoiding a duplicate build on each release push. Successful releases retain the install archives, checksums, and signed update manifest as release assets. Runner files disappear when GitHub destroys the hosted runner. Deleting artifacts does not recover execution minutes.

## Automatic updates

Official builds (`FRAMES_RELEASE_BUILD=1`) check on launcher startup and every six hours. Background downloads defer during remote sessions and cancel when a new session starts. The Ed25519 signature pins the manifest to the bundled public key; archive size and SHA-256 are checked before staging and again before installation. Mac replacements must also match the Developer ID team and bundle identifier.

Updates stage in the user's cache and apply after every Frames process using the installation exits. Replacement uses a temporary directory on the same filesystem and restores the previous install if moving the new one fails. No running session is forcibly stopped. Installation must be writable by the current user; the updater does not elevate or replace a privileged Windows service.

`frames update --check` checks manually. `frames update` downloads and schedules the update. Development builds do not check automatically. Keep the update private key backed up securely: losing it requires a manually distributed build with a new pinned public key.

In **Settings**, Check for updates shows progress and errors. After verification, Restart to update closes the idle launcher and reopens the installed replacement. Stop sessions and disable sharing at login first; another Frames process can defer installation. An app running from a read-only DMG must first be copied to Applications. The updater does not request administrator access.

Older builds used `app.frames.desktop`. Install this DMG manually once to migrate to `dev.creaton.frames`: the old updater intentionally rejects a different bundle identifier. Disable the old sharing login item before replacing the app and grant the new Frames identity its macOS privacy permissions. Pairing data is retained in its existing location.

For a local installer check on a Mac, run `bash native/macos/build.sh`, `python3 scripts/release.py dmg VERSION`, and `python3 scripts/release.py verify-dmg dist/release/Frames-VERSION-macos-aarch64.dmg VERSION` (use the workspace version). A local Developer ID signed build can exercise the complete updater with `FRAMES_UPDATE_TEST_APP="$PWD/dist/macos/Frames.app" cargo test -p frames signed_mac_update_replaces_entire_bundle -- --ignored --nocapture`; the test uses temporary installation/cache directories and an isolated test signing key.
