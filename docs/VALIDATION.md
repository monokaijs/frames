# Validation

## Automated checks

```sh
cargo fmt --all --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test -p frames-core --test media_roundtrip --locked -- --nocapture
cargo test -p frames-core --test audio_roundtrip --locked -- --nocapture
python native/macos/Tests/SigningTests.py
```

Codec/audio integration tests use the bundled shared libraries and run with the normal test suite. Prepare the media SDK before invoking Cargo directly. The release workflow also compiles Swift fixtures for geometry, modifier routing, binary media, blocked-media control delivery, session state, remote header behavior, native compressed text quality, and source admission backpressure. Update tests reject forged manifests, modified signatures, unsafe archives, and incompatible packages.

## Physical-machine checks

Windows and Apple-silicon Mac development builds have passed bidirectional LAN file transfers, pairing, native window capture/control, AppKit/Metal presentation, menu projection, content geometry, desktop/window switching, clipboard text/images, and interrupted transfer resumption. Mac privacy permissions persisted across rebuilds using the same Developer ID identity.

The latency fixture and its limitations are recorded in [media and latency](LATENCY_REVIEW.md). Generated codec roundtrips and real QUIC tests cover software codecs and ordered input delivery independently of a large blocked clipboard message. These checks do not establish performance on other GPUs or networks.

The quality regression check uses a generated dense-text source rather than flat
colors. A 30-second Mac-to-Windows production session kept all 129 decoded frames
at 1920×1080, with 24 passing edge-sharpness samples and a visually inspected
decoded PNG. Bidirectional transfer checks also passed. This fixture has sparse
capture updates and does not establish continuous 60 fps, display latency, or the
full network envelope. Windows and Mac workspace tests and Clippy passed; local
Windows tests used MinGW. One earlier sharpness sample failed during competing
builds, so that workload is not covered by the passing isolated run.

## Cross-platform end-to-end matrix

`scripts/e2e_matrix.py` runs on the Mac and drives the Windows test PC over
SSH. It covers four pairings: Mac client to Windows host, Windows client to
Mac host, and a loopback pairing on each machine.

For each pairing it starts a generated `Frames QA source` fixture and an
isolated host, with a fresh identity, configuration, receive folder and port.
It then checks the following:

- mDNS discovery.
- Matching-code pairing with a Unicode upload, verified by SHA-256.
- A trusted reconnect without approval, with a download verified the same
  way.
- `e2e_probe`, a production session client without native UI, covering:
  - the window list;
  - 10 s of sustained video, with capture-to-decoded p50 and p95;
  - the debug overlay;
  - audio;
  - a live High, Low, High quality change;
  - clipboard in both directions, on cross-machine pairings only;
  - a clean disconnect.
- `control_probe`, covering input, Unicode text, menus, resize and close.
- The real native client, measuring presented frames per second and
  capture-to-present latency.
- Host shutdown, checking that the UDP port is released and no stray
  processes remain.

What the runs leave alone:

- Hosts share only the fixture: every other app with a window is excluded.
- The full-desktop view is never requested.
- Pixels are never stored.
- Both clipboards are restored afterwards.

How processes start on each machine:

- **Windows:** every process runs in the logged-on console session through
  one-shot `/IT` scheduled tasks (`scripts/e2e/win_launch.ps1`). This is
  where capture, the user's clipboard, DPAPI and GUI windows work.
- **Mac:** processes are unbundled builds started from the invoking session.
  - `--features test-identity-file` keeps each throwaway identity key in its
    config folder. It is for automation contexts that cannot use the login
    Keychain and is never enabled in releases.
  - Capture needs a Screen Recording grant. If the invoking session has none,
    pass `--mac-host-app` to run the Mac host from a signed bundle that has
    the grant, launched through LaunchServices. That bundle must speak the
    same protocol version.

```sh
# Windows (PC): build into the target folder the driver reads.
powershell -File scripts\e2e\win_build.ps1 -Source F:\frames-e2e -Target F:\frames-ab\target
# Mac: helper, fixture, and test-identity binaries.
bash native/macos/build.sh            # the helper is used unbundled; signing may fail in automation
bash native/macos/build_fixture.sh
cargo build --release --locked --target aarch64-apple-darwin -p frames \
  --features test-identity-file --bin frames --example e2e_probe --example control_probe
python3 scripts/e2e_matrix.py --win-address 192.168.0.100 --artifacts /tmp/frames-e2e \
  [--pairings mac-win,win-mac,mac-mac,win-win] [--mac-host-app /Applications/Frames.app]
```

Each run writes `results.json` and the logs to the artifacts folder. A
signed Mac host keeps its test identities in the login Keychain; the driver
prints a `security delete-generic-password` command for each one.

## FEC/media release gates

The F6 media pipeline has deterministic tests for loss/reordering/duplicates,
MTU changes, malformed headers, shared receive budgets, parity loss, whole-frame
loss, stale generations, lost recovery keyframes, blocked UI delivery, and decoder
pressure. Scalar/x64/ARM golden FEC vectors match. Native tests verify actual
Windows D3D11 encode/decode/presentation and Mac VideoToolbox/Metal surfaces.
See [media and latency](LATENCY_REVIEW.md) for measurements and reproduction.

The proposed targets remain release gates, not measured guarantees:

| Scenario | Gate | Current status |
| --- | --- | --- |
| Clean LAN, continuous 1080p60, ten minutes | Input-to-presentation p95 <=65 ms; no interval >50 ms | Not established; live diagnostic run had gaps and capture-to-presentation p95 above 65 ms |
| Wi-Fi: RTT <=20 ms, 0-2% loss, <=10 ms added jitter, four-packet bursts, >=30 Mbps | p95 <=85 ms; <0.1% intervals >50 ms | Controlled impairment proxy supplied; complete gate pending |
| Loss within parity capacity | Exact reconstruction without recovery | Deterministic fixtures pass |
| 250 ms outage | Motion within 150 ms after delivery returns | Same-generation retry/watchdog tests pass; physical timed gate pending |
| Bulk transfer plus multiple windows | Input p95 inflation <=10 ms; no growing media backlog | Isolated-lane tests and bidirectional transfer smoke pass; measured contention gate pending |
| 1080p60 FEC | Encode/reconstruction p95 <=1 ms after warmup | Processing fixture passes at 25/90 KB; 250 KB reconstruction exceeded 1 ms |

Windows-to-Windows, Windows-to-Mac, and Mac-to-Mac need an isolated final-build hardware
matrix covering hide/restore, resize, desktop/window switching, virtual displays,
device failure, and software fallback. Mac native fixtures cover functional
1440p/4K scaling; sustained-rate scalability and the two-hour mixed workload soak
remain pending. Measure input-to-photon separately before display-level claims.

The preset-cycle production fixture also passed all four applied preset stages
(High/Low/Medium/High), including Low at 1440x810 and restoration to 1920x1080.
The synthetic Windows connection-menu fixture verified the title-bar dropdown,
hiding all windows, and restoring them from the connection window. Mac native
header tests cover the dropdown, preset check state, and hide/restore without
closing the remote application.

## Remaining coverage

### macOS dialogs and context menus

Run `bash scripts/test_macos_window_surfaces.sh` after the Mac release build.
The native regression fixture covers menu/help window levels, untitled AX
dialogs, ambiguous and cyclic ownership, nested popup creation, placement beyond
the parent edges, Retina and negative-origin geometry, local scaling/movement,
drag routing into submenus, Escape dismissal, tooltip hit testing, document,
application and nested modality, independent windows, hide/restore, reparenting,
and removal.
Build and release CI run this fixture. Header, shared-game, and content geometry
regressions also passed against the changed Swift sources on an Apple-silicon Mac.

Live capture/control acceptance is still pending: the available Mac reported
Screen Recording and Accessibility unavailable to the test helper. To reproduce
the remaining checks on an authorized Mac:

```sh
swiftc native/macos/Tests/WindowFixture.swift -o /tmp/frames-window-fixture
FRAMES_QA_SURFACES=1 FRAMES_QA_NO_COVER=1 /tmp/frames-window-fixture
```

Connect in per-window mode and check both Mac and Windows viewers:

- Right-click each source window, select normal/checked items, verify disabled
  items, hover into the submenu, drag and release onto an item, and dismiss with
  Escape or an outside click. Repeat near a screen edge.
- Open the alert sheet and Open file panel. Verify keyboard navigation and text
  entry, Cancel/Done, parent blocking, independent-window input, and focus return
  after dismissal. Check that App modal alert blocks both source windows. Open
  the untitled modeless dialog and interactive popover.
- Move and scale the local parent, hide/restore it, minimize/restore it, switch
  applications, and enter/leave fullscreen. Verify child placement, stacking,
  tooltip pass-through, held-key release, and no leftover popup after closing.

This work concerns per-window view. The separate Selected windows sharing
allowlist still admits only explicitly selected window IDs; admitting transient
descendants in that scope requires a separate authorization-policy change.

### Other release checks

- Controlled RustDesk/Moonlight comparison and input-to-photon measurements.
- Long sessions, sustained packet loss, different GPUs, multi-display selection, and placement restoration across view switches.
- Windows login/UAC secure-desktop acceptance, permission revocation during sessions, and privileged service updates.
- Mac locked/prelogin/FileVault behavior beyond the reported locked-session state.
- App-specific file drops, unusual clipboard formats, IMEs, and custom application menus.

Release CI signs, notarizes, staples, and assesses the Mac bundle. Windows packages currently have no Authenticode signature. The signed update manifest authenticates both platform archives.

## Shared game mode

[Shared-game validation](SHARED_GAME_VALIDATION.md) records native input, process-only audio, production QUIC policy, lifetime recovery, and sustained-input measurements. These fixtures do not grant game verification; physical simultaneous use and the Windows/macOS game matrix remain release acceptance requirements. See [compatibility evidence](GAME_COMPATIBILITY.md).
