# Launcher

The Slint launcher supervises the same host/connect commands exposed by the CLI. Remote windows use AppKit/Metal on macOS and Win32/D3D11 on Windows.

- **Connections:** sharing, connection options, nearby discovery, and running sessions. A running process may still be waiting for pairing.
- **Devices:** recent addresses, approved certificate fingerprints, and trust revocation. Revocation affects future connections; disconnect existing sessions separately.
- **Settings:** saved defaults for connection view, host audio, video codec, clipboard sync, unattended access, and the shared-window limit. Save changes to apply them to new sessions; audio and desktop checkboxes on Connections override the defaults for that connection. Sharing at login is managed separately on macOS.
- **Permissions dialog:** open the shield button in the rail to review macOS capture/input checks and system settings shortcuts. Missing sharing permissions open this dialog automatically. Local Network permission must be checked in system settings. Close with Done, the close button, or Escape.

Discovery, permission helpers, trust changes, and session supervision run outside the UI thread. Preferences live in `launcher.json` next to the trust store. Child output is retained in `sharing.log` and `connection-N.log`. Closing the launcher stops its supervised sessions.

## Preview

The launcher uses GPU-accelerated FemtoVG with Slint 1.16.1's Swash glyph
rasterization for small text. The close dialog's origin is rounded to physical
pixels so centering does not add a half-pixel offset at odd window sizes or
fractional display scaling. Remote video presentation uses its separate native
renderer.

The preview uses fixture data and the production Slint views without starting sessions or reading local trust/history:

```sh
cargo run -p frames --example launcher_preview -- connections
cargo run -p frames --example launcher_preview -- devices
cargo run -p frames --example launcher_preview -- settings --compact
cargo run -p frames --example launcher_preview -- permissions --compact
SLINT_BACKEND=winit-software cargo run -p frames --example launcher_preview -- connections /tmp/frames-preview.png
```

Options include `--empty`, `--active`, `--stress`, `--error`, and `--windows`. The minimum window size is 820 by 540 logical pixels. Permission checks refresh on focus; repeated setup actions open Settings after the first prompt. Keep the signing identity stable to preserve macOS privacy grants.

For text-rendering checks, run `launcher_preview close --windows --compact`
without a PNG output argument. Set `SLINT_BACKEND=winit-femtovg` explicitly and
repeat with `SLINT_SCALE_FACTOR=1`, `1.25`, `1.5`, and `2`, including resizing to
odd window dimensions. A PNG output argument selects the headless software
renderer and does not validate GPU text. `SLINT_DEBUG_PERFORMANCE=refresh_full_speed,console`
forces continuous redraws and prints throughput for performance comparisons;
unset it when checking normal idle CPU usage.
