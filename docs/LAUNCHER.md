# Frames launcher

The default entry point uses Slint on both desktop platforms. AppKit/Metal and Win32/D3D11 still present remote application windows. The launcher starts the existing `host` and `connect` commands, so pairing and transport rules are shared with the CLI.

- **Connections:** a split access/connect panel, audio/desktop options, a Sessions tab, and local network discovery. A running process may still be waiting for pairing; the launcher does not claim it is connected.
- **Devices:** the last 20 connection addresses, search, certificate fingerprints, incoming-control eligibility, and confirmed trust revocation. Saving an address never grants access. Revocation affects future connections; disconnect active sessions separately.
- **Permissions:** real macOS capture/control checks, individual setup actions, network-settings guidance, and attended/unattended sharing preferences. Local Network status cannot be preflighted by this helper, so it is explicitly left for review in Settings. Full Disk Access is not requested.

Network discovery, permission helpers, trust changes, and session supervision run outside the UI thread. Closing the launcher stops its supervised sessions; a separately registered login agent has its own lifecycle. Preferences and history live in `launcher.json` next to the existing trust store. Session errors are surfaced in the launcher; child stderr is retained in `sharing.log` and `connection-N.log` in the same directory.

## Design preview

Run the isolated fixture to inspect the UI without reading local trust/history, requesting OS permissions, or starting sessions:

```sh
cargo run -p frames --example launcher_preview -- connections
cargo run -p frames --example launcher_preview -- devices
cargo run -p frames --example launcher_preview -- permissions --compact
```

`--empty` displays empty device lists. `--active`, `--stress`, `--error`, and `--windows` cover running sessions, long lists/names, errors, and Windows labels. For reproducible PNG captures, select the software renderer and pass an output path as the second argument:

```sh
SLINT_BACKEND=winit-software cargo run -p frames --example launcher_preview -- connections /tmp/frames-connections.png
```

The fixture is titled **Frames — visual preview** and shows an explicit message for live-only actions. Use the main `frames` binary or a packaged Frames app for functional controls. The fixture shares the production `.slint` files. Native window chrome is excluded from snapshots. Buttons have keyboard focus states, input supports Enter to connect, and the revocation dialog disables the underlying content and supports Escape to cancel. The workspace scrolls vertically at smaller sizes, with a minimum window size of 820 × 540 logical pixels.

The layout uses one opaque, fixed header. Native window transparency and macOS title-bar transparency are explicitly disabled. Marketing copy and repeated status labels are omitted; certificate fingerprints are revealed through each trusted device's information button.

Local Network has no working URL anchor on macOS 27. Its setup action opens Privacy & Security, then uses the existing Accessibility grant to press only the localized Local Network navigation button. It never changes permission switches or requests Accessibility itself. If navigation is unavailable, the launcher shows the category to select manually. Native chrome explicitly uses the light theme to match the content.

The compact layout uses a 56 px indigo navigation rail, white content and header surfaces, 32 px buttons, and flat device/settings rows. Rail icons reveal their labels on hover or keyboard focus. Opening a client session selects the Sessions tab.

Permission checks refresh when the window regains focus. Setup prompts are limited to once per permission per launch; later clicks open Settings. `native/macos/sign.sh` signs both the helper and app with the same persistent certificate. Avoid ad-hoc signing for builds that need to retain privacy grants. Switching from an old ad-hoc build may require re-enabling that app once in macOS Settings.

When sharing at login is enabled, the launcher shows Manage sharing and leaves the background host under Service Management. It does not start a competing listener on port 47990. The footer reserves a fixed 32 px row and has no divider.

Explicit Screen Recording setup uses ScreenCaptureKit authorization; passive checks still use the non-prompting preflight API. If an older ad-hoc build left a stale code requirement in macOS, switching the old toggle may not replace it. Remove only the affected app/service record before approving the certificate-signed build; never reset permissions globally as part of routine startup.
