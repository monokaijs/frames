# Spike 0-B: Swift platform services in-process, behind a C ABI, called from Rust `main`

This is Phase 0 of the plan in `~/.claude/plans/plan-on-a-project-lazy-candy.md`. It was tested on an Apple M2 Pro running macOS 27.2 (26B123), with Swift 6.4.2 (Xcode toolchain) and a deployment target of 14.2.

```sh
cargo build --release   # build.rs runs `swift build` for FramesMac/
./target/release/swift-static-spike --seconds 12 [--synthetic] [--fps 60] [--codec h264|hevc] [--ltr] [--slices]
```

## Layout

- `FramesMac/` is a SwiftPM package with two targets:
  - `CFramesMac`: the C target that holds `include/frames_mac.h`, the ABI.
  - `FramesMac`: the Swift target, using Swift 6 language mode and strict concurrency.
  - It produces a single product, `.library(type: .static)`, so `libFramesMac.a` contains both targets (about 180 KB).
- `src/main.rs` is the Rust host. Its bindings are handwritten copies of the header. In Phase 3 that flips round: the Rust side becomes the source of truth, cbindgen generates the header, and CI checks the Swift side compiles against it.

What the ABI exercises:
- `frames_mac_abi_version`
- `frames_mac_screen_capture_preflight`
- `frames_mac_probe` (a JSON report of which VideoToolbox properties are supported)
- `frames_mac_capture_start` / `frames_mac_capture_stop`
- `frames_mac_encoder_new` / `_encode` / `_acknowledge_ltr` / `_flush` / `_free`
- `frames_mac_run_app` / `frames_mac_stop_app`
- A `FramesMacCallbacks` table (`context` plus `on_frame`, `on_encoded`, `on_event`). It replaces the process-wide `Wire` stdout singleton, and nothing on it is JSON.

## Linking: the minimal flags that work

```
cargo:rustc-link-search=native=FramesMac/.build/<profile>
cargo:rustc-link-lib=static=FramesMac
cargo:rustc-link-search=native=$(xcrun --sdk macosx --show-sdk-path)/usr/lib/swift   # .tbd stubs
cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift
MACOSX_DEPLOYMENT_TARGET=14.2   (.cargo/config.toml; Swift objects target 14.0)
```

- Frameworks and `-lswift*` are pulled in automatically through `LC_LINKER_OPTION` autolink entries in the Swift objects. There's no need to list frameworks by hand.
- **No `swiftCompatibility*` libraries** are needed at a 14.x deployment target.
- `otool -L` shows every Swift library resolving to `/usr/lib/swift/*.dylib`, which is the OS copy. No Swift runtime has to be bundled.
- The final binary is 688 KB.

## Results

**Run 1:** synthetic 1080p60 NV12 IOSurface source, HEVC with LTR, 12 s, 724 frames.

| Check | Result |
|---|---|
| Rust `main` is the process main thread and `frames_mac_run_app(12)` runs `NSApplication` there | **Works.** The app ran for 12.1 s and the timer stopped it with `NSApp.stop` plus a posted event, after which control returned to Rust. |
| Callbacks arrive off the main thread | **0 of about 1,450** callbacks ran on main. Capture runs on its own queue and encoder output on VideoToolbox's thread. |
| Zero-copy handoff | 724 frames reached Rust through only **2 distinct IOSurfaces** (pool recycling). Rust receives a +1 `CVPixelBufferRef`, reads the IOSurface id and size, passes the same pointer to the Swift encoder, then `CFRelease`s it. No copies. |
| Capture → Rust callback latency | **p50 0.34 ms, p95 0.44 ms, p99 0.51 ms** (synthetic; this includes filling the buffer) |
| Infinite GOP (`MaxKeyFrameInterval=0`, `MaxKeyFrameIntervalDuration=0`) | **Works on H.264 and HEVC.** Over 724 frames the only keyframes were frame 0 and the forced IDR at frame 450. |
| LTR (`EnableLTR`, `AcknowledgedLTRTokens`, `ForceLTRRefresh`) | **Works on H.264 and HEVC.** Tokens arrive on 719 of 724 frames, and Rust acknowledges each one straight back. After a simulated loss at frame 300, `ForceLTRRefresh` produced a **P-frame** (keyframe=false, 308 B HEVC / 204 B H.264), compared with 1107 B for a forced IDR. That makes keyframe-free recovery possible on macOS. |
| Multiple slices | **Not available.** `MaxH264SliceBytes` returns -12900 (`kVTPropertyNotSupportedErr`) with `EnableLowLatencyRateControl`, and the supported-property dictionary doesn't list it. There is no sub-frame or slice output on VideoToolbox. |
| `MaxFrameDelayCount=0` | **Rejected (-12900) in low-latency mode.** Production `NativeVideo.swift:92` sets it and ignores the result; it does nothing there. |
| Swift 6 strict concurrency | **Builds cleanly** with one change: `@preconcurrency import ScreenCaptureKit`, because `SCStream` isn't `Sendable`. Thread-confined classes use `@unchecked Sendable`, and the callback table is `@unchecked Sendable` because its C function pointers are thread-agnostic by contract. AppKit is reached through `MainActor.assumeIsolated` inside `@_cdecl` entry points that assert they're on the main thread. |
| 120 fps | Works: 732 frames in 6 s (120.1 fps), 2 channel drops. |

**Encode latency** is measured from `VTCompressionSessionEncodeFrame` to the output callback. Source is 1080p synthetic at 20 Mbps, with low-latency rate control and real-time mode on.

| Codec / options | fps | p50 | p95 | p99 |
|---|---|---|---|---|
| H.264 | 30 | 7.13 ms | 7.62 | 11.9 |
| H.264 | 60 | 6.84 ms | 7.18 | 9.97 |
| H.264 | 120 | 6.67 ms | 8.73 | 30.8 |
| H.264 + LTR | 60 | 6.75 ms | 8.86 | 10.6 |
| HEVC | 60 | 8.16 ms | 10.11 | 11.9 |
| HEVC + LTR | 60 | 9.60 ms | 10.62 | 14.8 |

The latency doesn't change with frame rate, so this is the encoder itself rather than pacing. It's about 7 ms for H.264 and 8–10 ms for HEVC on the M2 Pro.

The synthetic content is simple (frames average about 360 B), so the frame sizes above say nothing about real-content efficiency. The LTR-versus-IDR size ratio needs repeating on real desktop content.

**Real ScreenCaptureKit capture was not exercised.** `CGPreflightScreenCaptureAccess()` returns `false` for this binary, because the responsible process here is the VS Code / Claude Code host, which has no Screen Recording permission. I deliberately did not call `SCShareableContent`, since that would show a system permission prompt. The spike fell back to the synthetic IOSurface pool automatically. The `SCStream` code path builds and is ready (NV12 output, `showsCursor=false`, `queueDepth=3`, `.complete`-only frames). The manual procedure below covers running it.

## Screen Recording (TCC) permission: what it is tied to, and whether it survives the change

These are read-only queries of `/Library/Application Support/com.apple.TCC/TCC.db` on this Mac; nothing was modified.

```
kTCCServiceScreenCapture | dev.creaton.frames | client_type 0 (bundle id) | auth_value 2 (allowed) | 2026-10-31
kTCCServiceAccessibility | dev.creaton.frames | client_type 0 | auth_value 2
csreq = identifier "dev.creaton.frames" and anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6]
        and certificate leaf[field.1.2.840.113635.100.6.1.13] and certificate leaf[subject.OU] = V59Z6S9A36
```

- **There is no entry for `dev.creaton.frames.helper`.** Today `FramesHelper` makes the ScreenCaptureKit and Accessibility calls, but TCC attributes them to the **responsible process**, the app bundle `dev.creaton.frames` whose main executable `Contents/MacOS/Frames` is the Rust binary. That's because `FramesHelper` is spawned by `Frames`, and `native/macos/sign.sh` gives it identifier `dev.creaton.frames.helper`.
- The stored code requirement is exactly the app bundle's designated requirement: identifier plus the Developer ID team `V59Z6S9A36`. It does not pin a code hash.
- **Conclusion:** moving capture and AX calls into the main `Frames` executable keeps the same TCC client (`dev.creaton.frames`) and the same requirement. **Existing grants should carry over without a prompt**, as long as:
  - the bundle identifier stays `dev.creaton.frames`;
  - the bundle is signed with the same Developer ID team, as `sign.sh` already enforces;
  - the app is launched as the bundle (Finder, `open`, or the launchd agent), not as a bare binary from a terminal. A terminal launch makes the terminal the responsible process.
- Two caveats:
  - Recent macOS versions periodically ask the user to re-confirm Screen Recording for apps that use the non-picker API. That happens with or without this change.
  - Ad-hoc builds (`FRAMES_SIGN_IDENTITY=-`) produce a different requirement and will be re-prompted. This is already documented.

### Manual verification procedure (needs your Mac and a 0.2.x install; about 10 minutes)

1. With the installed 0.2.x `/Applications/Frames.app`, confirm Screen Recording and Accessibility are on for **Frames** in System Settings > Privacy & Security, and that hosting works.
2. Record the baseline:
   `sudo sqlite3 -readonly "/Library/Application Support/com.apple.TCC/TCC.db" "select service,client,auth_value,last_modified from access where client like 'dev.creaton.frames%'"`
3. Build a v2 test bundle in which `Contents/MacOS/Frames` is the Rust binary linking `libFramesMac.a` (no `FramesHelper`). Use the same `Info.plist` bundle identifier and sign it with `native/macos/sign.sh` and the same Developer ID identity. Check:
   `codesign -dr - Frames.app` must print the same designated requirement as above.
4. Quit Frames. Replace `/Applications/Frames.app` with the test bundle, the same way the updater swaps the bundle.
5. Launch it **from Finder** (or `open -a Frames`). Run `Frames host` through the app or the launchd agent, not from Terminal.
6. **Expected result:** no Screen Recording or Accessibility prompt, `CGPreflightScreenCaptureAccess()` returns true, ScreenCaptureKit delivers `.complete` frames, and remote input works. Re-run the query from step 2: still the same single `dev.creaton.frames` rows, with `auth_value 2` and an unchanged `last_modified`.
7. **Negative control:** run the same binary directly from Terminal. Preflight should report the Terminal's grant, which shows that the responsible process decides.

## Consequences for Phase 3 (`frames-platform-macos`)

1. **In-process is viable.** Use a SwiftPM static library, `@_cdecl` entry points and one callback table per session, link with the four flags above, and ship no bundled Swift runtime. Rust owns `main`. Headless mode calls `frames_mac_run_app`. When the Slint launcher is running, winit owns NSApplication and Swift must not call it; that combination still needs testing when Phase 3 starts.
2. **The latency budget for a macOS host has to change.** VideoToolbox encode takes about 7 ms for H.264 and 8–10 ms for HEVC at 1080p on an M2 Pro, and offers no slice output. The plan's 1080p60 budget (1.0 ms to the first encoded slice, about 5.5 ms total) is only achievable with an NVENC host. For a Mac host, the target becomes about 12–14 ms capture→present at p50. Prefer H.264 for latency on Macs and HEVC when bandwidth is constrained. Re-measure on M3/M4 and on real content.
3. **Keyframe-free recovery on macOS uses LTR.** Acknowledge tokens once the client confirms the frame arrived (the Phase 1 wire format needs an `ltr_token` field in feedback), and call `ForceLTRRefresh` on loss. Infinite GOP is confirmed, so the hard-coded `MaxKeyFrameInterval=60` in `NativeVideo.swift:90` can go.
4. **Remove the `MaxFrameDelayCount` call.** It's a no-op under low-latency rate control.
5. **Zero copy needs Rust to own lifetimes.** Rust holds the `CVPixelBufferRef` (+1) through encode, and a `PixelBuffer` newtype with a `Drop` impl calling `CFRelease` does that cleanly. Use the same pattern for decoded surfaces going to the Metal presenter.
6. **Swift 6 strict concurrency is workable** with `@preconcurrency` on ScreenCaptureKit and explicit `@unchecked Sendable` on thread-confined objects. Adopt it from the start, per the client review.
7. **Still open:**
   - real ScreenCaptureKit capture latency, measured inside a signed bundle using the manual procedure;
   - coexistence with Slint/winit on the main thread;
   - the Metal presenter fed from Rust;
   - TCC persistence on an actual 0.2.x upgrade.
