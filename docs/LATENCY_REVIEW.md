# Media delivery and latency

Frames uses in-process codecs, complete encoded access units, frame identities,
and bounded workers. Matching builds are required. The media wire format is F6;
subsequent device/shared-session work in the same workspace advances the overall
control protocol and ALPN separately (see `frames_core::PROTOCOL_VERSION`).

## FEC, deadlines, and recovery

`reed-solomon-simd = 3.1.0` supplies systematic Reed-Solomon protection with runtime
SIMD selection. Both encoder and decoder tables are warmed before live media.
A picture is striped across independent blocks of at most 32 source shards.
Profiles use 2, 4, or 8 parity shards per 32 source shards, rounded upward for a
partial block. Keyframes use eight. Even shard payloads fit the smaller of the
peer's datagram limit and 1200 bytes, after the 60-byte header. Small pictures
use small shards; padding, headers, and parity are charged as actual wire bytes.
The sender adjusts the encoder allowance for measured overhead, including
partial-block rounding.

Headers carry window identity, stream generation, frame number, keyframe status,
capture/encode timestamps, original length, FEC scheme/profile, block counts,
and shard indices. Validation precedes allocation. Reassembly reserves output,
source/parity storage, reconstruction scratch, and metadata under a 24 MiB
session limit and 20 MiB per-window quota; encoded pending/decoder submissions
are included in admission accounting. The frame limit is 8 MiB. Retired frame
identities reject duplicates and late resurrection. Reordered originals arriving
after a successful repair still update pre-FEC loss counters until the deadline.

Ordinary pictures expire 25 ms after first arrival; keyframes get 50 ms. Completion
runs immediately, and a deadline timer replaces coarse polling. Encoded references
are submitted in order. A missing reference discards its dependent chain and
requests a fresh independently decodable picture within the current generation.
Requests coalesce and retry after `clamp(2 * RTT, 50 ms, 250 ms)`. The encoder wakes
on recovery even without a changed desktop. Sender status every 50 ms reveals a
completely lost final picture. Configuration/backend changes create generations;
ordinary packet loss does not repeat the readiness handshake.

## Scheduling and adaptation

One sender serves video, audio, and parity for each connection. Audio preempts
video; video uses deadlines with a small focused-window preference. Admission is
bounded and non-evicting. The Quinn send buffer is 16 KiB, with an application
ceiling targeting 5 ms of queued media. A bounded pacing credit lets a new picture
start promptly without accumulating an unbounded burst. Successful admission is
not a delivery acknowledgement. MTU changes retire the affected picture and
request a fresh keyframe at the new size.

QUIC congestion control stays enabled. Bulk transfers receive a small shared
allowance while video is active. Input and essential media control have separate
reliable streams, so clipboard/file payloads cannot hold up recovery messages.
Media reception, codec initialization, file IO/checksums, native rendering, and
blocking UI delivery have independent bounded workers. Renderer device/shader
initialization runs on presentation workers, created lazily after a first frame. Replaceable snapshots,
raw captures, and decoded presentation frames may coalesce; encoded references
and essential control acknowledgements cannot be silently evicted.

Feedback runs every 100 ms and includes bytes/shards, pre-FEC loss, repairs,
failures, late frames, decoder backlog, presentation gaps, and audio underruns.
The sender combines it with Quinn RTT, minimum RTT, congestion events, window,
and loss statistics. A focused stream begins with a 12 Mbps encoder target.
Persistent overload reduces bitrate by 20%; stable links recover 5% per second
after three seconds. Protection moves from 12.5% toward 6.25% after five clean
seconds, or toward 25% for loss above 2% or failed reconstruction without
congestion. Extra parity consumes the encoder's existing allowance.

After a sustained overloaded second, dimensions step through 100%, 75%, and 50%.
Frame rate then steps through 60, 45, and 30. Restoration requires ten stable
seconds per quality step. Stream dimensions do not resize the remote application
or alter normalized input coordinates.

## Native video and audio

Windows WGC uses frame-arrival notifications and retains the newest available
surface in a bounded pool. Crop, scale, and BGRA/NV12 conversion use D3D11 before
encoding where adapter interoperability succeeds. A diagnostic identifies a
bounded CPU readback fallback. D3D11 decoding returns pinned FFmpeg surfaces to
a dedicated renderer worker. Flip-model swap chains use a frame-latency waitable
object and maximum latency one; unsupported adapters retain the compatibility
presentation path. Driver waits stay off the window thread.

macOS keeps ScreenCaptureKit, Core Image/Metal scaling, hardware VideoToolbox,
CVPixelBuffer lifetimes, and Metal presentation in the native helper. Compressed
access units cross IPC for hardware codecs; the CPU path remains for software
codecs. Encoded video and audio have separate bounded sockets from control.
Each native decoder has an ordered bounded worker, and decode/presentation
acknowledgements carry generation and frame number. CAMetalDisplayLink selects
the newest decoded surface at display opportunities, with one outstanding GPU
submission and hidden-window suspension. The helper uses the display link's
presentation scheduling, without an explicit `atTime` drawable presentation.

Opus remains 10 ms. A sequence buffer reorders within a 10 ms wait and emits at
most 20 ms of PLC after a gap. Under measured loss, an access unit may carry the
previous compressed packet when it fits the datagram. Playback uses bounded
storage and gradual clock-drift correction. Device callbacks do not allocate or
wait for video IPC. Hardware failures produce a reason and one controlled
fallback/recovery instead of repeated backend switching.

## Diagnostics and reproducible checks

Independent switches are `FRAMES_FEC=0`, `FRAMES_ADAPTATION=0`, `FRAMES_GPU=0`,
`FRAMES_HW_DECODE=0`, and Windows `FRAMES_PRESENTATION_COMPAT=1`.
`FRAMES_MEDIA_DIAGNOSTICS=1` enables bounded stage histograms and counters;
`FRAMES_NATIVE_DIAGNOSTICS=1` adds native GPU completion/presentation details.
Histograms use 250 microsecond buckets through 127 ms and report the observed maximum in
the overflow bucket, so long stalls remain visible. Clock probes calibrate
cross-machine monotonic offsets and report uncertainty. Acknowledged native
presentation and GPU completion do not establish input-to-photon latency.

```sh
cargo test --workspace --locked
cargo test --release -p frames-core --test fec_delivery --locked
cargo run --release -p frames-core --example fec_fixture -- 1000
cargo run --release -p frames-native --example gpu_pipeline_probe
python scripts/media_report.py CLIENT.log HOST.log --output report.json
```

The release FEC fixture executes 36,000 virtual frames with bursts, reordering,
duplicates, parity loss, and changing MTUs; the debug suite uses a shorter 600-frame
run. Other tests cover malformed metadata, deadlines/budgets, stale generations,
lost recovery replies, completely lost idle frames, blocked UI delivery, decoder
overload, and sequence-aware audio recovery. Fixed golden FEC vectors run against
scalar, SSSE3, AVX2, and NEON engines on x64 and Apple silicon, including
eight-erasure reconstruction.

`NativeVideoTests.swift` exercises actual H.264/HEVC hardware encode/decode,
changed-pixel verification, idle keyframes, scaling, odd dimensions, 1080p,
1440p, and 4K. `cross_transfer_smoke.py --native-ui --motion --hold-ui 600` runs a
real 1080p motion fixture; `--hold-ui 7200` supports a two-hour run.
`media_network_fixture.py` supplies a bounded seeded UDP proxy with loss, bursts,
jitter, capacity, and outage controls. Use a Mac loopback proxy with
`--windows-client-address 127.0.0.1 --windows-client-port PORT` when Windows
firewall policy prevents a Python listener. The proxy reports actual dispatch
lateness; requested settings alone do not establish a measured network envelope.

## Measurements from this implementation

These are development-machine observations, not release acceptance results.
Earlier software/CPU measurements remain in [codec-latency.json](codec-latency.json).

| Check | Observed result | Scope |
| --- | --- | --- |
| Windows x64 FEC, 25 KB pictures, 1000 warm samples | Packetization p95 0.0092 ms; reassembly/reconstruction p95 0.2691 ms | One source erasure per block; no network or display |
| Windows x64 FEC, 90 KB pictures | Packetization p95 0.0989 ms; reconstruction p95 0.8478 ms | Same fixture |
| Windows x64 FEC, 250 KB pictures | Reconstruction p95 1.6127 ms | Exceeds the proposed 1 ms gate at this larger size |
| RTX 3080 Ti, generated 1080p | 41/41 GPU-encoded and GPU-decoded surfaces; warm roundtrip p95 13.151 ms | Includes verification readback; excludes capture/network/presentation |
| RTX 3080 Ti, generated 1440p / 4K | 41/41 GPU-encoded and GPU-decoded surfaces each; warm roundtrip p95 18.365 / 41.125 ms | Same synthetic scope; functional scalability, no sustained-rate claim |
| Mac hardware H.264/HEVC | Changed pixels, idle keyframes, 75% scale, odd sizes, 1080p/1440p/4K passed | Functional; 1440p/4K fixture uses 30 fps |
| Mac to Windows generated 1080p60 fixture | 20 click-to-decoded-pixel samples: p50 54 ms, p95 81 ms; hide/restore 273 ms | Single client clock; excludes native presentation and panel |
| Windows to Mac ten-minute live motion run | Connected/rendering throughout; first long stream capture-to-presentation p50 71 ms, p95 86 ms; 105 intervals over 50 ms among 23,129 intervals | Development builds, startup included, other window activity; source pacing subsequently corrected |

The live run exposed and fixed a CAMetalDisplayLink presentation exception. The
4K fixture exposed transient VideoToolbox picture drops; retries are now bounded
and paced. These fixes are covered by the native fixtures and live smoke checks.
The results above do **not** establish the clean-LAN <=65 ms input-to-presentation
gate, the Wi-Fi <=85 ms gate, the complete hardware matrix, or a two-hour soak.
No display-level latency claim is made. Physical validation and remaining release
gates are tracked in [VALIDATION.md](VALIDATION.md).
