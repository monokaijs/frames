# Release validation

This file separates implemented paths from measured results. No latency, signing, or cross-machine guarantee is inferred from unit tests.

Development runs on 2026-10-29: Windows 11 x64 GNU, Ryzen 7800X3D, RTX 3080 Ti plus AMD integrated graphics and a virtual-display adapter; Mac mini Apple M2 Pro, macOS 27.0.1 (26A434). Twenty-two core tests and one mutual-TLS test pass on both platforms. Five Windows interactive native tests plus two geometry tests, the installed ADB client against a synthetic device through QUIC, Clippy with warnings denied, and Windows native UI menu checks passed. Codec/audio roundtrips, CLI transfers, and the cross-machine checks below were exercised. This pair has not been validated against the latency targets.

## Mac build and cross-machine run

- Windows development app installed at `C:/Users/monok/AppData/Local/Programs/Frames`, with a Start menu shortcut. Its installed executable passes the five loopback CLI transfer scenarios.
- Source pushed to `monokaijs/frames`, cloned at `~/WebstormProjects/frames` on `macmini`. Rust and Swift build on the physical Mac; the macOS CI compile gate also passes.
- Development app installed at `~/Applications/Frames.app`, Developer ID signed and verified after the user approved Keychain access. Screen Recording and Accessibility both verify **true** and persist through subsequent signed updates. macOS 27 labels the latter permission **Device Control and Data Access**. Notarization and a distribution installer remain outstanding.
- Repaired a broken external FFmpeg installation by installing Homebrew's separate keg-only `ffmpeg-full`. Frames discovers it without changing the shell PATH or replacing the existing regular FFmpeg link.
- Live Mac raw-frame encode/decode succeeds for H.264/HEVC through VideoToolbox and AV1/VP9 in software. Generated Opus PCM roundtrip passes. These are codec checks, not captured Mac app streams.
- Mac GUI Keychain identity creation succeeds. Launching directly through SSH cannot access that GUI Keychain identity; cross-machine tests launch the installed app with `open` in Aqua.
- Bidirectional LAN pairing and transfers now pass: Windows client/Mac host uploads and downloads, plus Mac client/Windows host uploads, using matching TLS-exporter codes, explicit approvals, Unicode filenames, generated 512 KiB payloads, SHA-256 verification, and trusted reconnection. Local Network approval resolved the reverse connection failure. Runs use Ethernet `192.168.0.169`; the earlier Wi-Fi `192.168.0.131` handshake failure has not been retested after approval.
- The production-session Mac-host probe passes twelve functional checkpoints: title/icon snapshot; standard title-bar removal with content dimensions matching scale metadata; direct capture of the red window under an opaque blue cover; suppression of decoded frames while unsubscribed; fresh frame after restore; disabled menu projection; AX menu command invocation; resize acknowledgement with actual 520×360 content geometry; Vietnamese/Unicode input; live explicit desktop capture; return to seamless mode with reconciled generations; and close/removal without a stale identity. The new content-only stream is 440×320 instead of 440×352. Latest full-probe evidence: `dist/cross-platform/20261029T112012Z/mac-host-control.json`.
- Windows WGC → H.264 AMF → QUIC → Mac decoding → AppKit/Metal presentation passes with a real 464×301 client area, excluding the original 466×353 frame/menu surface. Native diagnostics confirm GPU command-buffer completion and dimensions matching host content metadata. Source Windows menus appear in the Mac system menu bar. The user confirmed physical clicks and typing work. Command/Control, Option/Alt, terminal interrupts, and focus-switch shortcut tests were deferred at the user's request.
- Content-only capture: Windows copies only the client region from the independent WGC texture before readback/encoding. Its occluded-window fixture includes a native menu and asserts client dimensions and red content at all four corners. Mac ScreenCaptureKit remains an independent-window stream, with verified standard chrome removed before encoding; unverified/custom headers are retained. Host input, sizing acknowledgements, and drops now use content bounds. Pure Mac geometry checks exercise 2× scale; the latest physical Mac fixture capture used 1× scale. Full desktop capture remains complete and passes both platform checks.
- Modifier regressions: side-specific Mac Control/Shift/Option/Command state, Command-shortcut key release, local Option composition versus physical shortcuts/pointer gestures, and release on app deactivation/profile change are implemented. Standalone Swift modifier checks pass locally and in CI. Windows suppresses duplicate local menu handling for forwarded Alt combinations. These checks do not replace the deferred physical keyboard/IME matrix.
- The Aqua client fixture also passes Rust/Swift bridging, native window/menu creation with regular activation policy, and Metal GPU completion. Latest logs: `/var/folders/mr/28z60fqx1r7dj14g796fmvvh0000gn/T/frames-native-smoke.ywb5vc9n`.
- Runtime regressions found and fixed: stale pre-resize SCK geometry in acknowledgements; odd-size decoder output rounding that desynchronized raw-frame boundaries; accessory activation hiding the Mac menu bar; and a test-harness FIFO startup race that rejected fast console pairing. Patterned 128×96 and 129×97 frames remain aligned for twelve decoded frames through every advertised codec on both machines. The odd-size regression fails with the old filter and passes with exact padding removal.
- Restore timing is **not release-ready**: background-stream probes measured 385–1204 ms to the first fresh decoded frame across runs; the latest 440×320 content-only run measured 1204 ms. Some runs meet 500 ms, several do not. This measures decoder delivery, not native display presentation. Local decoder suppression does not independently prove capture/encoding/transmission stop within 500 ms. 1080p60/1440p60 input-to-photon percentiles remain unmeasured.
- Cross-machine clipboard, device audio, file dragging, physical keyboard/IME coverage, popups/sheets, secure desktop, reboot/login, and the complete application matrix still need acceptance runs. No full-release pass is inferred from these fixture checks.

Use `scripts/cross_transfer_smoke.py`, the `control_probe`/`window_fixture` examples, and `scripts/mac_native_smoke.py` to repeat these checks. Tests use generated files/windows and do not persist captured pixels. The user granted permissions through macOS; only Frames' obsolete approvals were reset with the supported `tccutil` tool before Developer ID signing was established.

## Exercised locally

- Rust workspace type checking, Clippy with warnings denied, and core regression tests on Windows x64 GNU.
- Real mutual-TLS QUIC connection: both certificate fingerprints and matching TLS-exporter pairing codes.
- Native Windows Graphics Capture of a red fixture while a blue window fully covers it: correct source pixels received.
- D3D11 presentation captured back through WGC: correct rendered pixels received. Repeated capture/render teardown checks pass after pinning the system capture DLL, which prevents a locally reproduced late callback into an unloaded module. The renderer uses a compatibility blt swap chain.
- Runtime codec probes plus live raw-frame encode/decode for hardware H.264 (AMF), software AV1 (libaom), and VP9 (libvpx) on this machine.
- Small-frame hardware padding: decoder crops only padding introduced by the encoder, never a desktop stream.
- Generated PCM → Opus → PCM roundtrip, without recording or playing device audio.
- Clipboard convergence for simultaneous copies, separate incoming-control permissions, file offset validation, checksum publication, collision prevention, and cancel/resume prefix handling.
- Real CLI pairing with matching codes, Unicode filename upload, trusted reconnect, collision-safe publication, host-to-client download, and interrupted upload resume. Isolated temporary identities and generated files on loopback; no desktop input or clipboard changes.
- Concurrent first-run host/client processes retain a single protected identity per device.
- Physical keyboard mapping distinguishes main/keypad Enter, navigation keys, and extended modifiers. Concurrent sessions cannot write the same transfer partial file.
- Focused-window scheduling preempts background streams at the stream limit and retains other running streams to avoid churn.
- Primary-display WGC capture is independent of seamless window capture. No display pixels are persisted or sent outside the local test.
- Standard Win32 menu projection, disabled commands, command-ID and position-based invocation, and rejecting a command after the host menu changes.
- Native Windows menu-bar placement, fixture command routing, and switching fixture app windows to a desktop surface. A GDI paint-order issue discovered in this UI run was fixed; the initial GPU frame stays visible.
- ADB smart-socket traffic through a real mutual-TLS QUIC connection, detach/listener cleanup, and an installed ADB executable listing a synthetic device through the bridge. No physical phone was accessed.

## Implemented, requires runtime checks

- Taskbar identity/icon grouping, lifecycle reconciliation, mixed-DPI coordinates, parent/popover placement, sizing preferences, and visibility suspension. The Windows launcher and discovery empty state have been inspected locally; discovered-host selection still needs a second host.
- Clipboard text/PNG/DIB, Vietnamese/IME input, modifier swapping and focus-loss release.
- Native file drops into host apps, downloaded-file drag shelf, macOS file promises, and progress/cancel UX.
- Optional Opus system audio independent of window visibility.
- Windows service installation, session supervision, DPAPI/ACL setup, login/UAC Desktop Duplication, reconnect through desktop transitions.
- Broader macOS AX matching, Dock grouping, system audio, and per-user login-item recovery beyond the generated fixtures above.
- Broader desktop-mode input-coordinate coverage, native menu clicks on the Mac, tray recovery after Explorer restarts, and physical Android phone/IDE compatibility.

## Outstanding implementation / release gates

- **macOS prelogin/unattended helper:** the login item is per-user Aqua only. LoginWindow/System Keychain/TCC handoff is not implemented or proven. FileVault preboot unlock is outside Frames.
- **Privileged/user service handoff:** Windows login agent disables file operations; transferring control to an ordinary user agent without reconnecting remains outstanding.
- **Full file UX:** folder trees, generic drag extraction directly from captured pixels, and arbitrary custom app drag formats remain outstanding. The current shelf shows transfer progress and cancels active transfers together.
- **Zero-copy media:** native capture → hardware codec → native decode textures need an in-process GPU resource path. The current FFmpeg adapter reports CPU readback.
- **Adaptive media:** focus-based frame rates and stream limits exist; measured codec selection, adaptive bitrate/resolution, session-wide GPU budgeting, native hardware decoding, and a live diagnostics panel remain outstanding. Current encoding targets 12 Mb/s per active video stream.
- **Platform edge cases:** secure attention sequence, HDR, app-specific protected surfaces, macOS ambiguous AX mappings, exact popup composition, and custom transparent-window occlusion need hardware/app verification.
- **Window event coverage:** discovery currently reconciles snapshots every 100 ms. Native destroy/create event hooks and macOS AX notifications are still needed to guarantee identity during handle reuse entirely between scans, and to capture very short-lived popups.
- **macOS Dock identity:** separate proxy processes set source icons, but distinct per-application bundle identities and Dock grouping need implementation/verification on macOS.
- **Distribution:** Windows code signing, Mac notarization, installer/update lifecycle, and codec/dependency notices. Mac Developer ID helper/application signing and permission retention are exercised locally.
- **Desktop mode:** primary display only; selecting additional displays and restoring per-window client placement across switches remain outstanding.
- **USB:** only the ADB server bridge exists. Raw USB export/import drivers, macOS controller-simulation entitlement, per-device selection, fastboot/DFU, and iOS forwarding remain outstanding. ADB reverse/forward commands that expect callbacks to a local IDE require separate workflow validation.

## Acceptance matrix to record on reference hardware

| Scenario | Target |
|---|---|
| 1080p60, wired LAN RTT ≤5 ms, hardware codec | p95 input-to-photon ≤80 ms |
| 1440p60, same network | p95 input-to-photon ≤100 ms |
| Client minimize/hide/inactive desktop | no video within 500 ms |
| Restore hidden window | fresh frame within 500 ms |
| Open/close/multiple same-title windows | correct identity; no stale proxy |
| Fixed-size window | remote resize disabled; local stretching available |
| Dialog, nested menus, popovers | correct owner, location, focus and taskbar behavior |
| Ten open windows, several visible | bounded memory and encoder sessions |
| Loss, constrained bandwidth, simultaneous files | recoverable video; input/control remain responsive |
| Lock/logout/UAC/reboot/revoke/crash | clear stale surfaces; no orphan input or workers |

Test both Windows→Mac and Mac→Windows on physical machines before calling the first release ready.
