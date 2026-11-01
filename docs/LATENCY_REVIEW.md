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
The sender subtracts measured overhead bytes per second from the encoder
allowance, including partial-block rounding. Tiny idle pictures do not multiply
the entire bitrate budget by their header-to-payload ratio.

Headers carry window identity, stream generation, frame number, keyframe status,
capture/encode timestamps, original length, FEC scheme/profile, block counts,
and shard indices. Validation precedes allocation. Reassembly reserves output,
source/parity storage, reconstruction scratch, and metadata under a 24 MiB
session limit and 20 MiB per-window quota; encoded pending/decoder submissions
are included in admission accounting. The frame limit is 8 MiB. Retired frame
identities reject duplicates and late resurrection. Reordered originals arriving
after a successful repair still update pre-FEC loss counters until the deadline.

Ordinary pictures expire 25 ms after first arrival. Keyframes get a size-aware
50–200 ms ceiling, allowing a detailed desktop refresh to arrive on the 30 Mbps
reference path. Completion runs immediately; this ceiling adds no playout delay.
A deadline timer replaces coarse polling. Encoded references
are submitted in order. A missing reference discards its dependent chain and
requests a fresh independently decodable picture within the current generation.
Requests coalesce and retry after `clamp(2 * RTT, 50 ms, 250 ms)`, while suppressing
redundant requests during active keyframe assembly. The encoder wakes
on recovery even without a changed desktop. Sender status every 50 ms reveals a
completely lost final picture. Configuration/backend changes create generations;
ordinary packet loss does not repeat the readiness handshake.

## Scheduling and adaptation

One sender serves video, audio, and parity for each connection. Audio preempts
video; video uses deadlines with a small focused-window preference. Admission is
bounded and non-evicting. The Quinn send buffer is 16 KiB, with an application
ceiling targeting 5 ms of queued media, estimated from the congestion window and
RTT. A token bucket retains at most 1 MiB of wire credit for refresh bursts while
bounding the average connection budget. Quinn congestion control paces the path;
dispatch yields after four datagrams so audio and control can run. Queue pressure
measures actual transport admission waits, not time spent enforcing the user's
average bandwidth setting. Successful admission is
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
and loss statistics. A focused 1080p stream begins with a 12 Mbps encoder target.
Settings supports an automatic budget or a custom 1-200 Mbps media limit per
connection (`--bandwidth-mbps` in the CLI). A custom limit raises the encoder
ceiling; weighted sharing gives the focused window four shares and each
background window one, reclaiming unused requested allocations. Pacing reserves
5% for transport/control plus a 256 kbps audio allowance before video allocation.
FEC and measured padding/header costs reduce the encoder allocation within that
limit; congestion adaptation remains active. Changes apply to new connections.
In automatic mode, larger physical captures scale the clean-link budget by source pixel count, up
to 64 Mbps (4K starts at 48 Mbps). Source resizing preserves existing congestion
reductions. Isolated late pictures and short encoder spikes do not reduce quality;
pressure must persist for 300 ms before bitrate drops. RTT variation alone and
application budget waits do not imply network congestion. Recoverable packet
loss may increase FEC without permanently preventing quality restoration.
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
Encoder timestamps use a stable media clock across focus/frame-rate changes.
A successful VideoToolbox callback without a sample is a picture drop, not a
backend failure; native retries are paced and bounded by 500 ms. Only actual
encoded pictures advance frame identity. A separate bounded source acknowledgement
lane holds the next encode until Rust admits or retires the current access unit.
Raw captures coalesce while a large refresh sends; encoded references stay in
order. Obsolete pictures captured before decoder readiness are retired with a
fresh keyframe request. A meaningful bitrate increase forces
a fresh independent picture even on a stationary desktop.
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
1440p, and 4K. Dense 1920×1080 text fixtures check decoded text edges and source
acknowledgement backpressure. Set `FRAMES_SCREEN_TEXT_OUTPUT` to an existing
directory when running the native test to export synthetic PNGs and access units.
`FRAMES_SCREEN_TEXT_PACKET` enables the real Mac access-unit branch of
`media_sender::tests::fast_quic_link_delivers_a_large_refresh_without_self_congestion`.
The `text_quality_probe` example connects through the production session and
checks full source resolution and text edges for 30 seconds; point
`FRAMES_SCREEN_TEXT_SOURCE` at the generated source PNG and display it with
`FRAMES_QA_TEXT_SOURCE` in `QAWindowFixture.app`. It saves only synthetic fixture
pixels and performs no remote input.
`cross_transfer_smoke.py --native-ui --motion --hold-ui 600` runs a
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
| Mac hardware dense 1080p text | H.264 / HEVC refresh 547,940 / 550,380 bytes; text-edge mean error 4.55 / 4.02 on a 0–255 scale | Synthetic source, native decode; functional readability |
| Mac dense-text access unit through FEC, loopback QUIC, Windows software decode | Exact compressed bytes; 1920×1080; text-edge mean error 4.51; debug send admission about 80.5 ms | Real hardware-generated access unit; no capture or display |
| Mac to Windows production text session, 30 seconds | 129 decoded frames, all 1920×1080; 24 sharpness samples passed (edge error 9.54–28.21) | Sparse synthetic capture updates, Auto bandwidth, development builds, no competing builds; not a 60 fps or latency gate |
| Mac to Windows generated 1080p60 fixture | 20 click-to-decoded-pixel samples: p50 54 ms, p95 81 ms; hide/restore 273 ms | Single client clock; excludes native presentation and panel |
| Windows to Mac ten-minute live motion run | Connected/rendering throughout; first long stream capture-to-presentation p50 71 ms, p95 86 ms; 105 intervals over 50 ms among 23,129 intervals | Development builds, startup included, other window activity; source pacing subsequently corrected |

The live run exposed and fixed a CAMetalDisplayLink presentation exception. The
4K fixture exposed transient VideoToolbox picture drops; retries are now bounded
and paced. These fixes are covered by the native fixtures and live smoke checks.
The text session exposed refresh admission at the average encoder bitrate and
RTT-only congestion decisions collapsing the stream to 50% resolution near
1 Mbps. Refresh wire credit, size-aware expiry, source admission acknowledgements,
and corrected congestion signals address that loop. An earlier text run during
concurrent builds failed one sharpness sample; the isolated 30-second run passed.
The results above do **not** establish the clean-LAN <=65 ms input-to-presentation
gate, the Wi-Fi <=85 ms gate, the complete hardware matrix, or a two-hour soak.
No display-level latency claim is made. Physical validation and remaining release
gates are tracked in [VALIDATION.md](VALIDATION.md).
