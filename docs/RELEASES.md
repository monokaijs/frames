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

Variables: `MACOS_SIGN_IDENTITY` (certificate fingerprint) and `MACOS_TEAM_ID`. The app's pinned Developer ID team is also checked by the signing script and updater; a team migration requires changing those checks deliberately.

The workflow imports the certificate into a temporary keychain, signs with hardened runtime and a secure timestamp, submits to Apple, staples the ticket, and checks Gatekeeper acceptance. Signing material is removed in an always-run cleanup step. Windows builds are currently unsigned by Authenticode.

Packages upload directly to a draft GitHub Release. No Actions artifacts or dependency caches are created. Ordinary CI runs on pull requests or manual dispatch; release builds run the checks themselves, avoiding a duplicate build on each release push. Successful releases retain the install archives, checksums, and signed update manifest as release assets. Runner files disappear when GitHub destroys the hosted runner. Deleting artifacts does not recover execution minutes.

## Automatic updates

Official builds (`FRAMES_RELEASE_BUILD=1`) check on launcher startup and every six hours. Background downloads defer during remote sessions and cancel when a new session starts. The Ed25519 signature pins the manifest to the bundled public key; archive size and SHA-256 are checked before staging and again before installation. Mac replacements must also match the Developer ID team and bundle identifier.

Updates stage in the user's cache and apply after every Frames process using the installation exits. Replacement uses a temporary directory on the same filesystem and restores the previous install if moving the new one fails. No running session is forcibly stopped. Installation must be writable by the current user; the updater does not elevate or replace a privileged Windows service.

`frames update --check` checks manually. `frames update` downloads and schedules the update. Development builds do not check automatically. Keep the update private key backed up securely: losing it requires a manually distributed build with a new pinned public key.
