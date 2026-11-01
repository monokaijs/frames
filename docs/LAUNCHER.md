# Launcher

The Slint launcher supervises the same host/connect commands exposed by the CLI. Remote windows use AppKit/Metal on macOS and Win32/D3D11 on Windows.

- **Connections:** sharing, connection options, nearby discovery, and running sessions. A running process may still be waiting for pairing.
- **Devices:** recent addresses, approved certificate fingerprints, and trust revocation. Revocation affects future connections; disconnect existing sessions separately.
- **Settings:** saved defaults for connection view, host audio, video codec, clipboard sync, unattended access, and the shared-window limit. Changes save automatically. Each Connect button has a chevron on the same surface for application-window/full-desktop mode and host audio. Frames remembers these choices per device, plus image quality and successful view changes made during a session. New devices use Settings defaults. Sharing at login is managed separately on macOS.
- **Permissions dialog:** open the shield button in the rail to review macOS capture/input checks and system settings shortcuts. Missing sharing permissions open this dialog automatically. Local Network permission must be checked in system settings. Close with Done, the close button, or Escape.
- **Updates:** manual downloads work during active sessions. Restart to update asks for confirmation before disconnecting sessions, sharing and file transfers. Cancel or Escape leaves sessions running. Preview the dialog with `launcher_preview settings --update-ready --update-confirm --active`, optionally adding `--update-confirm-error`.

Discovery, permission helpers, trust changes, and session supervision run outside the UI thread. Preferences live in `launcher.json` next to the trust store. Child output is retained in `sharing.log` and `connection-N.log`. Closing the launcher stops its supervised sessions.

Per-device client choices live in `connection-preferences.json`, keyed by certificate fingerprint so they survive address changes. Remote-cursor visibility also survives reconnects; existing native per-application preferences retain sizing and keyboard choices. Trust and connection approval are separate: the host must enable Unattended access and accept a device with **Accept and trust** while it is enabled to allow reconnecting without a prompt. Trusting an outgoing host does not grant it incoming control.

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

PNG previews also accept `--click=x,y` (logical coordinates) and `--key=Escape`, `Tab`, `Down`, or `Return`. Repeating these exercises menu selection, keyboard access, and dismissal before saving the snapshot.

For text-rendering checks, run `launcher_preview close --windows --compact`
without a PNG output argument. Set `SLINT_BACKEND=winit-femtovg` explicitly and
repeat with `SLINT_SCALE_FACTOR=1`, `1.25`, `1.5`, and `2`, including resizing to
odd window dimensions. A PNG output argument selects the headless software
renderer and does not validate GPU text. `SLINT_DEBUG_PERFORMANCE=refresh_full_speed,console`
forces continuous redraws and prints throughput for performance comparisons;
unset it when checking normal idle CPU usage.
