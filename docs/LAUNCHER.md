# Launcher

The Slint launcher supervises the same host/connect commands exposed by the CLI. Remote windows use AppKit/Metal on macOS and Win32/D3D11 on Windows.

- **Connections:** sharing, connection options, nearby discovery, and running sessions. A running process may still be waiting for pairing.
- **Devices:** recent addresses, approved certificate fingerprints, and trust revocation. Revocation affects future connections; disconnect existing sessions separately.
- **Permissions:** macOS capture/input checks, Settings shortcuts, and sharing preferences. Local Network permission must be checked in Settings.

Discovery, permission helpers, trust changes, and session supervision run outside the UI thread. Preferences live in `launcher.json` next to the trust store. Child output is retained in `sharing.log` and `connection-N.log`. Closing the launcher stops its supervised sessions.

## Preview

The preview uses fixture data and the production Slint views without starting sessions or reading local trust/history:

```sh
cargo run -p frames --example launcher_preview -- connections
cargo run -p frames --example launcher_preview -- devices
cargo run -p frames --example launcher_preview -- permissions --compact
SLINT_BACKEND=winit-software cargo run -p frames --example launcher_preview -- connections /tmp/frames-preview.png
```

Options include `--empty`, `--active`, `--stress`, `--error`, and `--windows`. The minimum window size is 820 by 540 logical pixels. Permission checks refresh on focus; repeated setup actions open Settings after the first prompt. Keep the signing identity stable to preserve macOS privacy grants.
