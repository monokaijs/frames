# Media and latency

Frames uses QUIC with separate reliable streams for control, ordered interactive input, and bulk transfers. Video uses datagrams; incomplete or stale pictures are discarded and trigger recovery. Clipboard operations run independently of input handling.

Mac capture and playback use dedicated binary Unix sockets between Rust and Swift. Raw video and audio are not serialized as base64/JSON. Native delivery keeps the newest decoded picture per window and coalesces wakeups; Windows wakes its UI with a posted message instead of a polling interval.

Capture drives encoding only when a fresh image arrives. Encoder output queues are bounded. H.264/HEVC packet boundaries are preserved through local AVI framing so delivery does not wait for the next picture. Codec options disable frame reordering and unnecessary buffering where supported. Hidden windows retain at most two drained decoders to reduce resume startup costs. Opus uses 10 ms packets with bounded playback buffering.

## Measurements

A 1080p60 physical Mac-to-Windows fixture measured **74 ms median / 87 ms p95** from a sent click to changed decoded pixels over 20 clicks. One hide/restore sample took **460 ms**. These measurements exclude display presentation and do not establish input-to-photon latency or RustDesk/Moonlight parity.

A synthetic Windows AMF H.264 test measured **18.7 ms median / 19.6 ms p95** for encoding plus decoding after startup. D3D11VA copyback was slower on the tested machine and was not enabled by default.

Frames still copies captured surfaces into CPU memory and invokes external FFmpeg processes on several paths. Moonlight's GPU decode/presentation pipeline is a useful comparison target; a direct GPU path and controlled input-to-photon comparison remain follow-up work. No claim of equal or lower latency than RustDesk is established by these tests.

## Reproduce

Run `cargo run -p frames-core --example media_latency -- libx264 1920 1080` for synthetic codec timing. `control_probe` exercises a live remote fixture. Tests in `media_roundtrip`, `audio_roundtrip`, and `quic` cover packet delivery, codec output, and input responsiveness while a bulk control transfer is blocked. See [validation](VALIDATION.md) for commands and limits.
