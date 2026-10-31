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

Codec/audio integration tests use the bundled shared libraries and run with the normal test suite. Prepare the media SDK before invoking Cargo directly. The release workflow also compiles Swift fixtures for geometry, modifier routing, binary media, blocked-media control delivery, session state, and remote header behavior. Update tests reject forged manifests, modified signatures, unsafe archives, and incompatible packages.

## Physical-machine checks

Windows and Apple-silicon Mac development builds have passed bidirectional LAN file transfers, pairing, native window capture/control, AppKit/Metal presentation, menu projection, content geometry, desktop/window switching, clipboard text/images, and interrupted transfer resumption. Mac privacy permissions persisted across rebuilds using the same Developer ID identity.

The latency fixture and its limitations are recorded in [media and latency](LATENCY_REVIEW.md). Generated codec roundtrips and real QUIC tests cover software codecs and ordered input delivery independently of a large blocked clipboard message. These checks do not establish performance on other GPUs or networks.

## Remaining coverage

- Controlled RustDesk/Moonlight comparison and input-to-photon measurements.
- Long sessions, sustained packet loss, different GPUs, multi-display selection, and placement restoration across view switches.
- Windows login/UAC secure-desktop acceptance, permission revocation during sessions, and privileged service updates.
- Mac locked/prelogin/FileVault behavior beyond the reported locked-session state.
- App-specific file drops, unusual clipboard formats, IMEs, and custom application menus.

Release CI signs, notarizes, staples, and assesses the Mac bundle. Windows packages currently have no Authenticode signature. The signed update manifest authenticates both platform archives.
