<!-- Approved 2026-11-04. Phase 0 spike write-ups: spikes/iroh-transport, spikes/swift-static, spikes/nvenc-direct (each RESULTS.md). -->

# Frames v2: re-architecture, Parsec-class latency, internet connectivity

## Context

Frames works, but three things block it from competing with Parsec:

1. **Architecture**
   - The real-time pipeline is split across the app crate (`crates/frames/src/streaming.rs`, `media_sender.rs`, `session.rs`) and `frames-core`.
   - `protocol.rs` depends on almost every domain module.
   - `frames-native::Desktop` is a 33-method god-trait; only 6 of its methods are media.
   - macOS media runs in a separate Swift helper process, with JSON over stdio and FRM1/FRV2 frames over file descriptors.
   - There are three VideoToolbox paths.
   - Every new streaming feature would have to be built two or three times.
2. **Latency**
   - Measured capture→present is 54–71 ms at p50. Parsec-class on wired LAN is around 5–10 ms capture→present.
   - Missing pieces:
     - no infinite GOP (GOP is hard-coded at 60);
     - no intra refresh, reference-frame invalidation (RFI) or long-term reference frames (LTR);
     - no slice output;
     - no NACK; recovery is keyframes only;
     - congestion control is loss-based Cubic;
     - fps is capped at 60;
     - present always waits for vsync;
     - the cursor is baked into the video;
     - no thread/GPU priority and no DSCP.
3. **Internet**
   - Connections are IPv4-only and direct to `ip:47990`.
   - There is no NAT traversal and no relay.

**Decisions made:**
- Ground-up rewrite as v2.
- iroh for transport: n0 public relays at first, then self-hosted `iroh-relay`.
- macOS media in-process: Swift becomes a static library with a C ABI, called from Rust.
- NVENC SDK called directly; AMF, QSV and software encoding stay on FFmpeg.

**Goal:** lower p50 and p95 than Parsec on the same rig. Reliable connections across NAT, with relay fallback. An architecture where each feature is implemented once behind a stable interface.

---

## 1. Target architecture

Dependencies point down only. CI enforces this with a `cargo metadata` check script.

```
frames-app        CLI, Slint launcher, updater, installer, Windows service (bin)
  └ frames-session  handshake, capability negotiation, service registry, BandwidthArbiter
      ├ services/*    frames-svc-{transfer,clipboard,devices(usbip,gamepad,adb),shared-game}
      ├ frames-net    iroh endpoint, identity/trust/pairing, channels, custom congestion controller, relay/path info
      ├ frames-media  engine: host/client pipelines, packet format, FEC+NACK, reassembly,
      │               rate control, clock, metrics; `api` module holds the traits
      │   ├ frames-codec            implements the media traits: FFmpeg runtime (moved from frames-core/src/codec), NVENC direct (cfg windows)
      │   ├ frames-platform-windows WGC/Desktop Duplication capture, D3D11 convert, D3D11VA decode, flip/tearing presenter, input, windows
      │   └ frames-platform-macos   Rust side of the C ABI to native/macos (SwiftPM static lib)
      └ frames-proto  envelope, handshake, feature negotiation ONLY (no domain messages)
frames-testkit    synthetic capture/encoder/decoder (tracks reference chains), simulated transport, virtual clock, netsim
```

Rules:
- **frames-proto stays small.** Each service owns its message types, version and channel ID; nothing grows back into one `Message` enum. Peers negotiate capabilities instead of requiring an exact `PROTOCOL_VERSION` match (`crates/frames-core/src/lib.rs:21`).
- **Media wire format** is hand-rolled binary in `frames-media::wire` (not serde) and fuzz-tested.
- **frames-media and frames-testkit build with no C toolchain.** That breaks today's media↔codec cycle (`GpuConverter`/`HardwareLease`/`LatencyHistogram`) and lets netsim run on any CI runner.
- **The `Desktop` trait is split** into focused traits per platform: `WindowCatalog`, `InputSink`, `Clipboard`, `MediaPlatform` (capture sources, encoders, decoders, presenter, cursor source), `Health`.
- **`BandwidthArbiter`** in frames-session replaces `MediaSender::throttle_bulk`, which is used today at `session.rs:409` and `device_session.rs:635`. Media publishes its spare bandwidth; bulk services take from it, together with QUIC stream priorities.
- **macOS process model:**
  - The Rust binary owns main. Slint's winit owns NSApplication when the launcher runs.
  - Headless host mode calls an exported `frames_mac_run_app()`.
  - Swift never calls `NSApp.run` while Slint is active. AppKit work goes through `DispatchQueue.main`.
  - ScreenCaptureKit and VideoToolbox use their own queues; the Metal presenter has its own render thread.
  - The `FramesHelper` process, the FRM1/FRV2 framing (`crates/frames-native/src/native_media.rs`, `native/macos/Sources/Wire.swift`) and the direct VideoToolbox path in `native.c` (`vt_open`) are all deleted.

### Engine boundary (in `frames-media::api`; all synchronous, one thread per stage)

- **Capture:**
  - `CaptureSource { caps, configure, next(deadline) -> Frame|Timeout|Resized|Lost, cursor() -> Option<CursorUpdate> }`
  - `Surface` is `Cpu` | `D3D11(tex+fence)` | `IoSurface`.
- **Encode:**
  - `VideoEncoder { caps -> EncoderCaps{features: INTRA_REFRESH|RFI|LTR|SLICE_OUTPUT|DYN_BITRATE|DYN_RES|ONE_FRAME_VBV|ZERO_COPY|HDR10|YUV444, max_slices, ltr_slots}, control(Bitrate|ForceIdr|Invalidate{first,last}|MarkLtr|Reconfigure), encode(frame, &mut FnMut(Chunk)) }`
  - Slices are emitted as soon as each one is ready.
- **Decode:** `VideoDecoder { decode(AccessUnit) -> Frame|Pending|BrokenRef{last_good} }`
- **Present:** `Presenter { caps(tearing,vrr,hz), wait_ready, present(frame, cursor_overlay, mode) -> PresentStamp }`
- **Transport:** `MediaTransport { max_datagram, try_send (never queues beyond the CC window), recv, path() -> rtt/Direct|Relay/mtu, set_target_rate }`
- **Clock:** `MediaClock` (a virtual clock in testkit).

What to reuse rather than rewrite:
- FEC and the reassembler: `crates/frames-core/src/media.rs` (`fragment_fec`, `Reassembler`, `ClockEstimator`).
- The adaptive controller's decisions about resolution, fps and FEC: `crates/frames-core/src/adaptive.rs`.
- The EDF scheduler and audio-first logic: `crates/frames/src/media_sender.rs` (`select_work`, `WirePacer`).
- Opus code: `crates/frames-core/src/audio.rs`.
- The WGC capture and D3D11 video-processor conversion: `crates/frames-native/src/windows/capture.rs`, `native.c:537-566`.
- The swapchain setup: `crates/frames-native/src/windows/render.rs`.
- The macOS Swift media code: `NativeVideo.swift`, ScreenCaptureKit capture in `Host.swift`, the Metal presenter in `Client.swift`. Only its I/O changes; it gets re-homed behind the C ABI.
- Trust and the Keychain/DPAPI storage: `crates/frames-core/src/trust.rs`.
- The shared-game virtual cursor views: `virtual_cursor.rs`, `VirtualCursorView`. These generalize to all modes.

### Client and UI architecture (review of 2026-11-04)

Problems verified in code:

- **Input waits behind media feedback.**
  - Input shares one `select!` loop with per-frame `Decoded`/`Presented` feedback (`media.send().await`) and an inline `probe_local_adb().await` (`session.rs:1235-1278`).
  - On macOS every input event is also JSON-encoded, piped to another process and re-parsed (`Client.swift:557`, `macos.rs:706-759`). The latency timestamp is taken only after those hops (`macos.rs:757`).
- **Every video frame passes through the UI thread:**
  - Windows: `InvalidateRect` → `WM_PAINT` (`windows/client.rs:1515`).
  - macOS: `main.async` → `pendingVideo` (`main.swift:104`).
- **A modal dialog on the UI thread stalls video and window updates, and can end the session.**
  - Windows: `MessageBoxW` and a nested `GetMessage` loop.
  - macOS: `NSAlert` inside `update()`.
  - Mechanism: the UI stops draining `ClientSender`, so its sender blocks on a condition variable (`client_updates.rs:95`), and the delivery queue fails the session after 64 pending updates (`ui_delivery.rs:66`).
- **State is duplicated three to four times with manual fan-out:**
  - Rust session, `WindowGraph`, per-HWND `Proxy`, and the Swift `[String:Any]` dictionaries.
  - The macOS helper's `frames_session_apply` result is freed without being read (`Client.swift:261`).
  - Preferences are split across five stores, including Swift `UserDefaults`.
- **Monoliths and unsafe code:**
  - Windows has a 645-line WndProc and a 566-line `run_client`.
  - Per-window state is reached by dereferencing `GWLP_USERDATA` with no null check (`client.rs:1158`).
  - The present worker is detached and re-created on every show.
  - A presenter failure is reported as a decoder failure, which forces software decode.
- **Untyped, unversioned bridge.** About 35 stringly-typed JSON message names; bad messages are silently dropped.
- **The launcher can't see session state.**
  - It runs one process per session; liveness is `try_wait` and status is the last line of a log file.
  - Stop means `kill()` (`launcher.rs:441`).
  - It polls (100 ms UI timer, 250 ms worker).
  - `handle()` is 289 lines and does blocking work inline.
- **Dead or wasteful code:**
  - `Launcher.swift` is dead (`frames_native::launcher()` has no callers).
  - One `FramesHelper --dock` process runs per remote app.
  - The tray is rebuilt on every wake.
  - A `CoCreateInstance` call and a z-order walk run every 100 ms.

Target design (folded into Phases 3 and 6):

1. **`ClientModel` in Rust is the only source of truth** for session state, the window catalog (diffing, parenting, modality, ordering) and preferences, with one preferences store. Platforms receive typed `SurfaceCommand::{Create, Update, Destroy}` and keep only native handles. Reconciliation logic becomes pure and unit-tested.
2. **Separate input lane.**
   - `InputSink`: the platform pushes typed events with native timestamps (`NSEvent.timestamp`, `GetMessageTime`) into a lock-free queue read by the input writer task.
   - Pointer motion travels as datagrams; nothing else shares that loop.
   - macOS: `@_cdecl frames_input(...)` takes a C struct.
3. **Presenter fed by the engine.** Decoded surfaces go straight to each window's long-lived presenter. The UI thread only publishes geometry and visibility through atomics. Presented and decoded counts are lock-free counters, not messages. `PresenterError` and `DecoderError` are separate.
4. **No modal dialogs on the UI loop.** Use in-window banners and toasts. The UI is woken through a message-only HWND or `PostMessage`, not thread messages. If the UI stalls, the engine pauses the UI feed instead of failing the session.
5. **Typed C ABI**: `#[repr(C)]` structs and enums generated with cbindgen, a version check at session start, and an injected callback table that replaces the `Wire` singleton.
6. **Launcher runs sessions in-process.** `SessionSupervisor` returns `SessionHandle { watch::Receiver<SessionStatus>, CancellationToken }` and pushes updates to Slint via `invoke_from_event_loop`. `handle()` is split into SessionSupervisor, DeviceDirectory, SettingsStore and PermissionService behind a view-model. The expected device id is passed as a typed parameter instead of `FRAMES_EXPECTED_DEVICE_ID`.
7. **Windows window code** is split into per-window handler structs (input, sizing, commands) and a `WindowFactory`. Proxy state moves to `HashMap<HWND, Box<Proxy>>`, removed on `WM_NCDESTROY`, with a typed command enum. The tray and occlusion update on events (`DwmGetWindowAttribute`), not on every wake.
8. **Swift**: SwiftPM, XCTest, Swift 6 strict concurrency, swift-format. Split into SurfaceManager, MenuController, a pure InputTranslator, and Presenter. Delete `Launcher.swift` and the dock-proxy processes once one process owns the Dock icon.

Gate additions:
- Phase 3: input→wire p95 ≤ 1 ms on the client, measured from the native event timestamp.
- Phase 3: no video stall while a modal or menu is open, and a 5-minute open dialog does not end the session.
- Phase 6: the launcher shows live session status and stops sessions gracefully.

---

### Phase 0 spike results (2026-11-04; details in `spikes/*/RESULTS.md`)

- **0-A iroh 1.4.1 (noq 1.4.0):**
  - What works:
    - The custom congestion controller drives pacing (`ControllerMetrics.pacing_rate`).
    - Datagrams work (1162 B before MTU discovery, 1200 B after).
    - `export_keying_material` works.
    - Per-path relay/direct info is available.
    - `clear_ip_transports()` gives a relay-only test mode.
    - Over LAN, Windows→Mac direct ran at 60 Mbps with queueing p95 ≤ 4.4 ms.
  - Problems:
    - The macOS source-address bug exists in noq-udp. It is patched in `spikes/iroh-transport/vendor/noq-udp`; the patch lets the handshake on a secondary address complete and lets LAN peers upgrade from relay to direct.
    - Host→client data then stops on a dual-interface, same-subnet Mac: the IPv4-mapped path on the v6 socket is never validated. **Unresolved, and it blocks the Phase 4 gate.**
    - n0's public relay caps out at about 8–10 Mbps, with queueing reaching 1.2 s above that. Self-hosted relays are needed by the start of the beta.
    - QUIC RTT did not reflect relay queueing, so rate control must use app-level one-way delay.
- **0-B Swift static library (M2 Pro, macOS 27.2):**
  - What works:
    - SwiftPM static library, built in Swift 6 mode, links into the cargo binary.
    - Rust main runs NSApp.
    - Zero-copy IOSurface handoff, p50 0.34 ms.
    - No periodic keyframes.
    - **VideoToolbox LTR works**: recovery as a P-frame instead of a keyframe.
    - 120 fps works.
  - Problems and open checks:
    - Mac encode is about 7–10 ms and VideoToolbox has no slice output, so a realistic Mac-host capture→present is about 12–14 ms.
    - `MaxFrameDelayCount=0` is rejected.
    - The TCC grant is held by the bundle `dev.creaton.frames` with team V59Z6S9A36, so it should survive the move to a single process. Needs the manual check in RESULTS.md.
    - Real ScreenCaptureKit capture is still to be run inside the bundle.
- **0-C NVENC direct (RTX 3080 Ti, driver 621.01, API 13.1):**
  - Timings:

    | Codec | First slice | Full frame |
    |---|---|---|
    | H.264 1080p60 | 1.6 ms | 3.3 ms |
    | HEVC 1080p120 | 1.1 ms | 2.2 ms |

  - Behaviour:
    - Infinite GOP plus 1-frame VBV: no spikes, and a forced IDR is about the size of a P-frame.
    - RFI recovery is bit-exact on H.264 and HEVC.
    - Intra refresh is bit-exact on H.264.
    - HEVC intra refresh without invalidating the lost frames first corrupts permanently, so always invalidate first.
  - Requirements:
    - Hold the GPU in a high power state during a session (encode is 2–2.5× slower at P8).
    - Select the NVIDIA adapter explicitly.
    - Use a sleeping poll thread for sub-frame output.

---

## 2. Phases and exit gates

### Phase 0: Foundations and spikes (prerequisite for everything)

**Process:**
- Cut `release/0.2` from main; hotfixes land there and are cherry-picked forward.
- `.github/workflows/build.yml`: run on `push` to main as well as PRs, and protect main.
- Add swift-format and convert `native/macos` to a SwiftPM package so tests run with `swift test`. That replaces the ad hoc `swiftc` steps in CI.
- Rule: no logic inside `tokio::select!` arms; each arm calls a named function.

**Measurement:**
- Per-stage tracing on 0.2.x to explain the 54–71 ms. It must account for at least 80% of it.
- Set up the photodiode click-to-photon rig: a microcontroller acting as a USB mouse plus a photodiode, or NVIDIA LDAT.
- Record a Parsec baseline on the same machines.

**Spikes** (throwaway code, each with a written result):
- **0-A iroh,** on Windows and on a multihomed Mac:
  - a custom quinn-fork congestion controller driven by an atomic target rate;
  - 1200-byte datagrams over direct and relay paths, measuring RTT and jitter;
  - `export_keying_material` availability (pairing codes depend on it);
  - raw-socket access for DSCP/qWAVE;
  - correct reply source address. That is the reason for the `vendor/quinn-udp` patch, which will not apply to iroh's fork.
- **0-B** SwiftPM static library linked into the cargo binary on macOS. An installed 0.2.x with Screen Recording already granted must keep its TCC grant after upgrading.
- **0-C** NVENC direct with D3D11 ARGB/NV12 input: infinite GOP, intra refresh, RFI, slice output (`enableSubFrameWrite` / slice mode).

**Gate:** all three spikes pass, the stage breakdown is documented, and the Parsec baseline is recorded.

### Phase 1: Skeleton, protocol and frozen wire format

**Status (2026-11-04): done.**
- `crates/frames-proto`: envelope, handshake, feature negotiation.
- `crates/frames-media`: `wire` (41-byte media header, feedback, probes; strict canonical parsing; 500k-datagram fuzz smoke), `seq`, and the `api` traits.
- `crates/frames-testkit`: virtual clock, seeded link and transport, synthetic capture, and an encoder/decoder pair that tracks reference chains.
- `scripts/check_layers.py`, plus a Linux `v2-core` CI job that builds and tests with no C toolchain.

- Create the crates above, with frames-testkit and the dependency-direction CI check.
- **frames-proto:** envelope, handshake, feature negotiation. ALPN `frames/2`.
- **Media wire v2** (`frames-media::wire`):
  - per-packet sequence number and send timestamp in microseconds;
  - frame id, slice index/count, reference info (last referenced frame / LTR slot), FEC block info;
  - transport-wide feedback carried as datagrams: arrival time per sequence number, NACK ranges, loss and decode state.
  - Fuzz tests for all of it.
- **Gate:** frames-proto, frames-media and frames-testkit compile and pass tests on Linux with no FFI, and the wire format is frozen.

### Phase 2: Media engine on testkit

**Status (2026-11-04): engine and gates done, in virtual time.** The engine is in `frames-media` (`host`, `client`, `fec`, `rate`); scenarios are in `frames-testkit::netsim`, with gates in `crates/frames-testkit/tests/netsim.rs`.

Measured results (60 fps, 1080p synthetic):

| Scenario | Result |
|---|---|
| Clean LAN | 0 lost, 1 keyframe, p95 3.8 ms |
| 1% random loss, 20 ms RTT | 0 lost (NACK), p99 24 ms |
| Burst loss | 0 lost, p99 55 ms |
| 80 ms outage | recovered by invalidation, no keyframe |
| 600 ms outage, no invalidation | recovered from a long-term reference 50 ms after detection, no keyframe |
| 80→10 Mbps step | ≤10 Mbps within 2 s, settles at 9.2–9.5 Mbps with no standing queue (delay-trend plus 30 ms queue bound) |

- Seeded runs replay exactly.
- Intra refresh is supported by the API and the synthetic codec but not yet used by the host's recovery ladder. Spike 0-C showed HEVC needs invalidation first.
- The comparison with 0.2.x netsim waits until the v2 engine drives real codecs (Phase 3).

Host pipeline: capture → encode (slices) → packetize/FEC → pacer. Client pipeline: recv → reassemble → NACK/FEC → decode → present → feedback.

- **NACK:** retransmit a missing shard only while `now + rtt/2 < frame deadline`. FEC covers what retransmission can't reach in time; FEC overhead adapts to measured residual loss.
- **Loss recovery ladder:**
  1. NACK;
  2. FEC;
  3. `Invalidate` (RFI) or reference an acknowledged LTR;
  4. intra refresh wave;
  5. IDR only as a last resort.
  Keyframe-request coalescing and retry come from `streaming.rs:1338/1581`.
- **Rate control:** a GCC-style trendline estimator on one-way-delay gradient, plus loss-based bounds. It outputs the target bitrate to the encoder and the pacer, and the window to the custom congestion controller via `set_target_rate`. Keep the resolution/fps step-down logic from `adaptive.rs`, driven by the new estimator.
- **Pacing:** spread each frame's packets over about 1/4 frame interval; audio preempts.
- **Jitter buffer:** zero on LAN. Adaptive on internet paths, sized from measured jitter and capped.
- **Netsim:** moves into frames-testkit with a virtual clock and runs on every push. Scenarios: lan, wifi, lossy (1%), jitter (20 ms), step-down (50→10 Mbps), internet (50 ms RTT, 1% loss), relay (TCP-like head-of-line delays).
- **Gate:**
  - recovery from loss within 1 RTT + 1 frame with no IDR when RFI or intra refresh is available;
  - bitrate converges within 2 s after a step-down;
  - deterministic results under the virtual clock;
  - no regression in frames delivered compared with 0.2.x netsim.

### Phase 3: Platform layers

**Status (2026-11-05): Windows media path working on hardware, not yet in the app.** `crates/frames-codec` (NVENC direct: HEVC/H.264, infinite GOP, 1-frame VBV, 4 slices with sub-frame output, invalidation, intra refresh; FFmpeg D3D11VA decode) and `crates/frames-platform-windows` (WGC capture with the cursor kept separate, flip-model presenter with tearing, MMCSS, GPU priority, WLAN streaming mode, qWAVE DSCP). `examples/hw_loop` runs capture → NVENC → v2 host engine → simulated LAN link in real time → v2 client engine → D3D11VA → present on the RTX 3080 Ti:

| Run | capture → present p50 | p95 |
|---|---|---|
| 1080p60 HEVC, window capture | 3.55 ms | 4.33 ms |
| 1080p120 HEVC, window capture | 4.43 ms | 5.36 ms |
| 1080p60, GPU clocks not held | 6.99 ms | 8.91 ms |

Over real QUIC (`crates/frames-next`, host and viewer as two processes on the PC, iroh direct path, 1080p60 HEVC QA window, 20 s): capture → present p50 4.90 ms, p95 6.96 ms, p99 52 ms (one 130 ms stall at t=3 s, under investigation), 0 lost of 1,186, 1 keyframe.

Input lane (`crates/frames-input`, `frames-platform-windows::input`, viewer and host in `frames-next`): pointer positions as datagrams (newest wins), buttons, wheel and keys (USB HID usages) on a reliable stream, release-all on focus loss or exit. With generated mouse and key messages: input to wire p50 8 µs, p95 20 µs from the viewer's window procedure (gate: p95 ≤ 1 ms), every click, wheel step and key delivered to the host's QA window, video unaffected (p50 3.93 ms, p95 5.86 ms).

FFmpeg encoders (`frames-codec::ffmpeg::FfmpegEncoder`) for GPUs NVENC direct does not cover: AMF, QSV, Media Foundation, and software AV1 (the bundled LGPL FFmpeg has no software H.264/HEVC). They run with an infinite GOP and a one-frame VBV through a per-thread encoder shape in the v1 runtime; v1 keeps its 60-frame GOP. On the PC's AMD iGPU, `h264_amf` over frames-next: capture → present p50 10.8 ms, p95 13.6 ms, one keyframe in 12 s, nothing lost (encode 7.7 ms on the iGPU). Software AV1 is not real time at 1080p (42 ms per frame; the engine then drops frames as stale and asks for keyframes).

**macOS half** (`crates/frames-platform-macos`, SwiftPM package `native/macos/FramesMac` linked into the Rust binary, versioned C ABI with an injected callback table, no helper process): ScreenCaptureKit capture with the cursor separate, VideoToolbox encode with acknowledged long-term references and decode, Metal presenter with on-glass timestamps. `examples/mac_pipeline` on an M2 Pro (generated 1080p60 → VideoToolbox → v2 engine over a simulated LAN → VideoToolbox → Metal window): H.264 capture → present p50 10.96 ms, p95 11.63 ms; HEVC p50 12.54 ms, p95 18.53 ms, recovering from a 300 ms outage through a long-term reference with no keyframe. Encode (8-9 ms) dominates; VideoToolbox has no slice output.

Across machines (`frames-next`, PC host with NVENC H.264 on a QA window → MacBook viewer with VideoToolbox and Metal, iroh direct path on the LAN, RTT about 1 ms): capture → present p50 11.66 ms, p95 18.30 ms, p99 24.15 ms over 20 s, one frame lost, one keyframe. Viewers drop media that queued while their window and decoder started (497 datagrams on the Mac), which had made the first seconds 0.4-0.8 s late. The Mac viewer sends input too (macOS ABI 2: the presenter window's view reports mouse, wheel and key events, with the event's own timestamp, through a callback that is removed under a lock before Rust frees it; key codes map through `frames_input::keys`, all 83 v1 entries). Sending from the window's callback rather than the viewer loop (which can wait a frame for a drawable) took Mac input to wire from p95 1.2 ms to p50 10 µs, p95 56 µs; every generated click, wheel step and key reached the PC's QA window. The Mac viewer does not show the host's cursor shape yet. Mac host → PC viewer (ScreenCaptureKit generated source, VideoToolbox H.264, iroh direct over the Mac's Ethernet address): p50 12.4 ms, p95 24.5 ms, nothing lost; input reaches the Mac host but is not applied yet (macOS injection is still to do).

"Presented" is when `Present` returns, not photons. The clock hold was a dummy GPU load (about 140 W extra). `frames-platform-windows::nvprofile` replaces it: an NVAPI driver profile sets "Power management mode: Prefer maximum performance" for the running executable before its D3D11 device exists, and restores the previous state (value, no setting, or no profile) when the session ends; a test applies and restores it on the PC. `frames-next` uses it by default and no longer runs the dummy load. With the user's own applications keeping the GPU busy, the profile measured p50 3.05 ms against 3.86 ms without; the idle-GPU comparison with the dummy load (3.55 ms) is still to repeat on a quiet machine. The NVAPI setting struct must use the original 12,320-byte layout (without the header's newer 64-bit union member); driver 621.01 rejects the 12,328-byte one. Still open: the macOS half (`frames-platform-macos`, on a branch), AMF/QSV through FFmpeg, Desktop Duplication, the input lane and client model, and running this path inside the app over real QUIC.

**Windows** (`frames-platform-windows`, `frames-codec`):
- Capture:
  - WGC with `MinUpdateInterval` set to the target fps;
  - DXGI Desktop Duplication for full-desktop capture (from `secure.rs`), including pointer shape/position via `GetFramePointerShape` for the local cursor.
- Encoding:
  - NVENC direct with D3D11 input, ultra-low-latency P1, infinite GOP, 1-frame VBV (`bitrate/fps`), intra refresh, RFI, slice output.
  - FFmpeg AMF/QSV/MF with infinite GOP, `intra_refresh` where FFmpeg exposes it, and a 1-frame VBV.
- Decode with D3D11VA.
- Presenter: flip-discard plus `DXGI_PRESENT_ALLOW_TEARING` / sync 0 ("Lowest latency" mode) or vsync ("Smooth"), and VRR when available.
- Priorities and network:
  - MMCSS (`AvSetMmThreadCharacteristics("Games")`) for the capture, encode, send, receive, decode and present threads;
  - `D3DKMTSetProcessSchedulingPriorityClass` and `IDXGIDevice::SetGPUThreadPriority`;
  - qWAVE DSCP (`QOSAddSocketToFlow`);
  - `WlanSetInterface` media-streaming mode while a session is active.

**macOS** (`frames-platform-macos` plus a SwiftPM package):
- A C ABI with `@_cdecl`, and a header generated or checked against the Rust bindings.
- ScreenCaptureKit capture: `minimumFrameInterval` from the display's refresh rate, `showsCursor=false`, cursor shape/position sent on a separate channel.
- VideoToolbox:
  - encode with `MaxKeyFrameInterval` = 0 (infinite), `EnableLTR` plus `AcknowledgedLTRTokens`, multiple slices for loss resilience;
  - decode.
- Metal presenter with `displaySyncEnabled=false` in low-latency mode.
- Real-time threads using `THREAD_TIME_CONSTRAINT_POLICY`, and `SO_NET_SERVICE_TYPE=NET_SERVICE_TYPE_VI` on the socket.

**Both platforms:**
- The client renders the cursor locally in every mode, using cursor datagrams.
- Pointer motion goes over datagrams (newest wins); buttons and keys stay on a reliable stream.
- The 60 fps caps are removed (`adaptive.rs:130`, `model.rs:235`, `streaming.rs:781`, `Host.swift:567`, `NativeVideo.swift:41/64`, `native.c:530`, `Client.swift:687`). The cap becomes `min(host display, client display, encoder caps)`.

**Gate:**
- Wired LAN, NVENC host, hardware decode, tearing present, measured capture→present:
  - 1080p60: p50 ≤ 6 ms, p95 ≤ 10 ms;
  - 1080p120: p50 ≤ 5 ms, p95 ≤ 8 ms.
- Click-to-photon p50 is lower than the Parsec baseline at both refresh rates.
- macOS works with no `FramesHelper` process.

**Budget at 1080p60, p50** (sums to about 5.5 ms):

| Stage | Target |
|---|---|
| Capture | 1.0 ms |
| Convert | 0.3 ms |
| Encode, first slice | 1.0 ms |
| Wire | 0.5 ms |
| Reassemble | 0.1 ms |
| Decode | 2.0 ms |
| Present | 0.5 ms |

### Phase 4: Internet transport and identity (frames-net)

**Status (2026-11-05): crate on main, gate not run.** `crates/frames-net`: iroh endpoint with n0 relays and discovery, device key derived from the v1 identity, v1 trust migration proof, media datagram channel, and a congestion controller that follows the media target. `iroh` and `iroh-relay` are vendored without their cdylibs (Windows debug builds exceeded the DLL export limit). A node holds 1 ms timer resolution on Windows; at the default 15.6 ms, QUIC pacing fell below the media rate and a loopback stream lost 56% of frames. Admission (`frames-net::trust`, `frames-next`): each device keeps a stable v2 key (`device-v2.key`, user-only permissions) and a trust list (`trusted-v2.json`, written atomically; a corrupted file is an error, never an empty list). A host lets a listed device in, with input only if it is listed for control; an unknown device needs the person at the host to compare the pairing code shown on both screens and choose view only or view and control. A host without a terminal refuses unknown devices unless started with `--approve` (test runs), and the viewer reports the refusal. Checked on the Mac (refused, approved and saved, recognized after restart) and on the PC (a view-only device's input is dropped on the host; a control device's is applied). Until the v2 cutover the key is stored, not derived from the v1 key (`DeviceKey::from_legacy_pkcs8` is ready): reading the v1 Keychain item from a test binary prompts the user.

Self-hosted relays (`docs/RELAY.md`): `frames-next --relay URL` uses our own `iroh-relay` (vendored) instead of n0's. Relay-only through a relay on the PC: 1080p60 at 36 Mbps, capture → present p50 4.32 ms, p95 5.73 ms, nothing lost. Only n0's public relays get the 6 Mbps cap. Deploying relays in 2-3 regions needs servers and hostnames (to decide).

v1 trust migration (`frames-next connect --migrate-v1`): the viewer's first stream carries a proof made with its v1 key, bound to this connection's TLS exporter and its v2 device id; the host verifies it, looks the v1 certificate fingerprint up in v1's `trusted.json`, and trusts the device under its v2 id with v1's name and control permission, without asking. Checked on the PC with scratch v1 identities (DPAPI): an untrusted v1 identity was verified and then asked about; after adding it to v1 trust, the next connection was migrated as "Old laptop" with control, its fingerprint recorded, and its input applied. Viewers now wait up to 90 s for a host that is asking its user.

Still open: the NAT matrix (needs other networks), production relays, a graphical pairing flow, migration against a real 0.2.x install (and the Keychain path on macOS), and deriving the v2 key from the v1 key at cutover. The dual-interface Mac case did not reproduce: Mac host → PC viewer selected the Mac's Ethernet address and streamed normally (spike 0-A's failure was host → client on the same subnet before the noq-udp patch).

- **iroh `Endpoint`:**
  - One endpoint per process, used for both serving and connecting, with IPv4 and IPv6.
  - Hole punching, with relay fallback.
  - n0 public relays during beta. Before general availability, self-hosted `iroh-relay` in 2–3 regions, with the relay URLs in config.
- **Discovery:**
  - mDNS locally, replacing `mdns-sd` usage in `crates/frames-core/src/discovery.rs`;
  - DNS/pkarr discovery for internet peers.
  - Peers are addressed by `EndpointId`, not by IP.
- **Internet pairing:**
  - The host shows a ticket (QR code or short code) containing `EndpointId` plus a relay hint.
  - The existing confirm-the-same-code UX stays, with the code derived from the TLS exporter.
- **Identity migration** (keeps existing trusted pairs):
  1. Derive the ed25519 key as `HKDF-SHA256(legacy P-256 secret, "frames-iroh-v1")`. Nothing new is stored, and the Windows service's machine identity derives the same key.
  2. After the handshake, each side sends `LegacyProof{cert_der, sig_p256("frames-migrate-v1" ‖ endpoint_id ‖ exporter)}`. A match in `trusted.json` re-keys that entry under the `EndpointId`. The old fingerprint is kept as an alias, and the `unattended`/`allow_control` flags are copied.
  3. `connection_history` and the mDNS TXT records carry both IDs during the transition.
  4. The last 0.2.x release maps an ALPN mismatch to a "Update your peer" message.
- **Relay paths are a degraded mode:** a bitrate cap, a deeper jitter buffer, FEC preferred over NACK, and a UI indicator showing "Relayed".
- **Connection manager:** automatic reconnect by `EndpointId`, path migration when the network changes, and a probe to upgrade from relay to direct.
- **Gate:**
  - direct connections across at least 5 NAT types, including a CGNAT/phone hotspot;
  - relay fallback works when UDP is blocked;
  - migration verified against a real 0.2.x `trusted.json` and the service identity.

### Phase 5: Internet quality hardening
- Bandwidth probing at session start and after a step-down: padding or FEC probes, so the stream doesn't open at a guess.
- Codec preference for constrained paths: HEVC or AV1 over H.264 when both ends support it.
- Network-change handling covering Wi-Fi to Ethernet and sleep/wake.
- Per-path stats in the debug overlay (`crates/frames/src/media_debug.rs` concept).
- **Gate:**
  - on an internet path with 50 ms RTT and 1% loss: p95 frame latency ≤ RTT/2 + 20 ms, freezes under 0.5% of frames, and no IDR storms;
  - on relay: a stable stream at the relay's cap.

### Phase 6: Services, parity and cutover
- Port these as `services/*` on the new session: transfer, clipboard, devices (USB/IP, gamepad, ADB bridge), shared game, the window graph and menus, updater, installer, the Windows service, and the secure desktop through a Desktop Duplication SYSTEM helper.
- **Parity checklist** against the README features. Ship as a "Frames Next" updater channel. It reads the shared config directory, and writes migrated trust entries alongside the old ones, never in place of them.
- Two-week beta.
- **Gate:**
  - parity checklist complete;
  - no regression in crash rate;
  - cut over at 0.3.0;
  - then delete `crates/frames-core`, `crates/frames-native` and the old `crates/frames` modules.

**Later** (not in this plan's gates):
- an IddCx virtual display for headless hosts and refresh rates above the physical display;
- 4:4:4 and HDR;
- an AMF/oneVPL direct SDK.

---

## 3. Keeping 0.2.x alive during the rewrite
- The new crates live in the same workspace next to the old ones; old crates are untouched apart from hotfixes.
- The v2 binary is `frames-next` until cutover.
- Only critical fixes go to `release/0.2`.

## 4. Verification
- **Every push** (CI):
  - `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test --workspace`;
  - the dependency-direction check;
  - fuzz smoke on `frames-media::wire`;
  - testkit netsim with all scenarios, failing on regression beyond tolerance compared with the stored baseline CSV.
- **Nightly** (self-hosted Windows PC plus a Mac):
  - real-hardware netsim: NVENC/VideoToolbox, real QUIC through the impairment proxy (`scripts/media_network_fixture.py`);
  - per-stage latency histograms;
  - p50/p95 trend stored as an artifact.
- **Per phase gate:**
  - photodiode click-to-photon, at least 300 samples per configuration, reporting p50/p95/p99, Frames vs Parsec on identical machines, cable, resolution, fps and bitrate, with both vsync and tearing settings;
  - a camera-based display-to-display cross-check once per phase.
- **Internet:**
  - a NAT matrix: home router, double NAT, phone hotspot/CGNAT, corporate UDP-blocked (forces relay), IPv6-only;
  - a forced-relay mode switch for tests;
  - 30-minute soak on each.
- **Migration:** automated test using fixtures copied from real 0.2.x `trusted.json`, `connections.json` and the DPAPI/Keychain identity, plus one manual upgrade on each OS where Screen Recording/Accessibility grants must persist.

## 5. Main risks
- **iroh fork and API churn:** pin the version, keep a thin wrapper in frames-net, and let spike 0-A decide.
- **Relay over TCP adds head-of-line latency:** treated as degraded mode; self-hosted relays sit close to users.
- **Losing TCC grants on the macOS process change:** spike 0-B. Fallback: keep a minimal helper only for capture permission, still without media IPC.
- **NVENC driver/SDK version skew:** load at runtime with a version check, and fall back to FFmpeg NVENC.
- **Rewrite scope creep:** the phase gates are hard. No Phase-5 work starts before the Phase-3 latency gate passes.
