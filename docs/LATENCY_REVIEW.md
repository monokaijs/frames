# Media and latency

Frames uses QUIC with separate reliable streams for control, ordered interactive input, and bulk transfers. Video uses datagrams; incomplete or stale pictures trigger recovery. Clipboard work runs independently of input handling.

Video encoders and decoders now run inside Frames on dedicated blocking workers. Windows embeds LGPL codec DLLs in its single executable and extracts them to a hash-identified cache; Mac bundles signed dylibs. The updater starts without loading codecs. There is no FFmpeg executable, pipe, AVI/IVF demuxer, or Ogg page on the live media path.

Windows selects a successfully probed NVENC, Quick Sync, AMF, or Media Foundation encoder. Mac uses VideoToolbox directly, with hardware required, realtime encoding, no reordering, and a completion callback for each picture. Software fallbacks are VP9 and AV1. Probes require four consecutive pictures to encode and decode without additional input or EOF. A path that buffers pictures is rejected.

Encoding reads the newest capture at the requested rate; its output queue holds one packet. Decoding accepts two queued packets, uses slice threading instead of frame threading, and publishes only the newest completed picture. Actual encoder keyframe flags cover H.264, HEVC, AV1, and VP9. Mac decoding uses VideoToolbox with CPU copyback when its reference probe succeeds. Windows currently uses software decoding; D3D11VA remains a future GPU presentation path.

Opus is linked into Frames and emits raw 10 ms packets paced at the playback rate, including when a native capture callback contains several packets. Device rate/channel conversion uses in-process libswresample. Audio capture and decoder queues are bounded; playback retains at most one device callback plus 20 ms. Late audio packets are discarded and small gaps use Opus loss concealment instead of restarting a container stream.

## Matched synthetic measurements

Forty alternating generated 1920?1080 pictures, one startup sample excluded, same physical machines. Each picture is checked for the expected changed pixels. These numbers exclude capture, networking, presentation, and the display panel. The before build is f214f42; the after build uses this in-process rewrite.

| Machine and encoder | Before median / p95 | After median / p95 |
| --- | --- | --- |
| Windows, AMD AMF H.264 | 20.807 / 21.698 ms | 11.247 / 11.641 ms |
| Apple M2 Pro, VideoToolbox H.264 | 32.410 / 39.377 ms | 21.879 / 24.747 ms |

The Windows machine also has an NVIDIA RTX 3080 Ti; AMF was selected explicitly to compare the same encoder before and after. The complete summary is in [codec-latency.json](codec-latency.json). These are local measurements, not performance guarantees for other hardware or networks.

An earlier physical Mac-to-Windows 1080p60 fixture measured 74 ms median / 87 ms p95 from an injected fixture click to changed decoded pixels, and 460 ms for one hide/restore. Those measurements predate this rewrite. Neither set establishes input-to-photon latency or Moonlight parity. Capture and presentation still use CPU BGRA buffers; direct GPU surface delivery remains future work.

## Reproduce

Prepare the media SDK as described in the README, then run:

```sh
cargo run --release -p frames-core --example media_latency -- h264_amf 1920 1080
# On Mac:
cargo run --release -p frames-core --example media_latency -- h264_videotoolbox 1920 1080
# With no encoder argument, select the negotiated default.
Frames media-check
```

`media-check` verifies the packaged libraries, all four decoders, working encoder paths, and a single 10 ms Opus packet without opening capture or playback devices. `media_roundtrip` checks changing/odd-sized pictures and single-picture delivery. `audio_roundtrip` covers one-packet delivery, shutdown with open inputs, and 44.1/96/192 kHz device formats. `control_probe` exercises a generated remote fixture over the real session; `quic` checks input responsiveness while bulk control traffic is blocked.

The architecture follows the same broad approach as [Sunshine's video pipeline](https://github.com/LizardByte/Sunshine/blob/master/src/video.cpp) and [Moonlight's decoder](https://github.com/moonlight-stream/moonlight-qt/blob/master/app/streaming/video/ffmpeg.cpp): codec APIs, explicit complete pictures, bounded work, and dedicated decoding workers. Frames retains its own transport and window-control implementation.
