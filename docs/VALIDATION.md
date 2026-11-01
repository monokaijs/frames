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

## Remaining coverage

- Controlled RustDesk/Moonlight comparison and input-to-photon measurements.
- Long sessions, sustained packet loss, different GPUs, multi-display selection, and placement restoration across view switches.
- Windows login/UAC secure-desktop acceptance, permission revocation during sessions, and privileged service updates.
- Mac locked/prelogin/FileVault behavior beyond the reported locked-session state.
- App-specific file drops, unusual clipboard formats, IMEs, and custom application menus.

Release CI signs, notarizes, staples, and assesses the Mac bundle. Windows packages currently have no Authenticode signature. The signed update manifest authenticates both platform archives.

## Shared game mode

[Shared-game validation](SHARED_GAME_VALIDATION.md) records native input, process-only audio, production QUIC policy, lifetime recovery, and sustained-input measurements. These fixtures do not grant game verification; physical simultaneous use and the Windows/macOS game matrix remain release acceptance requirements. See [compatibility evidence](GAME_COMPATIBILITY.md).
