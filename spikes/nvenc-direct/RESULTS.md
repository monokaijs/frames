# Spike 0-C: NVENC called directly (no FFmpeg) from D3D11 textures

The plan is in `~/.claude/plans/plan-on-a-project-lazy-candy.md`, Phase 0. This is a standalone C++ program and is not part of the cargo workspace.

## Setup

| Item | Detail |
|---|---|
| Test machine | Windows PC (`ssh windows-pc`): RTX 3080 Ti, driver 621.01, NVENC API 13.1 (`NvEncodeAPIGetMaxSupportedVersion`) |
| Header | `third_party/nvEncodeAPI.h` from FFmpeg nv-codec-headers `n13.1.15.0` (MIT). We load `nvEncodeAPI64.dll` at runtime, so the proprietary SDK is not needed. |
| Build | MSYS2 UCRT64: `g++ -O2 -std=c++17 -static nvenc_spike.cpp -o nvenc_spike.exe -ld3d11 -ldxgi -lwinmm`. Without `-static`, the exe fails with 0xC0000135 (missing MinGW runtime DLLs) whenever `C:\msys64\ucrt64\bin` is not on PATH. |
| Run | `nvenc_spike.exe [--hevc] [--size WxH] [--fps N] [--mbps N] [--slices N] [--no-subframe] [--ir off\|ondemand\|periodic] [--detail N] [--out prefix]`. `run_matrix.cmd`, `run_detail.cmd` and `loaded.cmd` contain the runs below. `--burn S` is a separate GPU-load process. |
| Content | A 2×-size high-frequency texture scrolled on the GPU (7, 3 px per frame) plus a moving 200 px box, copied into a BGRA texture that NVENC reads directly: `NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX`, `NV_ENC_BUFFER_FORMAT_ARGB`, with no colour conversion of our own. `--detail 1` is close to worst case (QP 44–51 at 30 Mbps); `--detail 8` has larger cells. |

## Encoder configuration (all runs)

- `nvEncGetEncodePresetConfigEx(P1, NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY)`, then:
  - `gopLength = idrPeriod = NVENC_INFINITE_GOPLENGTH`, `frameIntervalP = 1`
  - CBR with `averageBitRate = maxBitRate`, `vbvBufferSize = vbvInitialDelay = bitrate/fps` (a one-frame buffer)
  - `zeroReorderDelay = 1`, lookahead off
  - `maxNumRefFrames` (`maxNumRefFramesInDPB` for HEVC) = 4, so reference-frame invalidation (RFI) has older frames to fall back on
  - `sliceMode = 3`, `sliceModeData = 4`, `repeatSPSPPS = 1`
  - `enableIntraRefresh = 1` with `intraRefreshCnt = fps/6`. For on-demand mode `intraRefreshPeriod` is set huge; for periodic mode it is `fps`.
- `NV_ENC_INITIALIZE_PARAMS`: synchronous (`enableEncodeAsync = 0`), `enablePTD = 1`, `reportSliceOffsets = 1`, `enableSubFrameWrite = 1`.
- Sub-frame readback polls `nvEncLockBitstream` with `doNotWait = 1`:
  - `hwEncodeStatus` 0 means no data yet, 1 means encoding with partial slices visible, 2 means the picture is complete.
  - `numSlices` and `bitstreamSizeInBytes` grow one slice at a time.
  - Just before completion one poll returned 0/0/0. Treat that as "keep polling", not as an error.

## Capabilities reported (`nvEncGetEncodeCaps`)

| | H.264 | HEVC |
|---|---|---|
| Sub-frame readback, intra refresh, reference picture invalidation, custom VBV, dynamic bitrate and resolution, single-slice intra refresh, multiple reference frames | yes | yes |
| Max long-term reference frames | 8 | 7 |
| 10-bit | no | yes |

Both codecs support 4:4:4. The card has 1 NVENC engine.

## Latency (encode submit → data available; 600–1200 frames, first 30 excluded)

### Matrix A, `--detail 1`

The GPU was in a higher power state during this run for reasons we didn't capture; see "Power state" below.

| Run | First slice p50 / p95 | Full picture p50 / p95 / p99 | Size p50 (VBV) |
|---|---|---|---|
| H.264 1080p60, 30 Mbps, 4 slices, sub-frame | **1.6 / 2.0 ms** | 3.3 / 3.7 / 3.9 ms | 62.7 KB (62.5) |
| H.264 1080p60, 4 slices, blocking lock | — | 3.3 / 3.5 / 4.4 ms | same |
| H.264 1080p60, 1 slice, blocking lock | — | 3.3 / 3.7 / 3.8 ms | 62.5 KB |
| HEVC 1080p60, 30 Mbps | **1.3 / 2.0 ms** | **2.4 / 3.1 / 3.6 ms** | 60.9 KB |
| H.264 1080p120, 40 Mbps | 1.5 / 2.0 ms | 3.2 / 3.7 / 3.8 ms | **56.6 KB (41.7): QP pinned at 51, overshoots the VBV** |
| HEVC 1080p120, 40 Mbps | **1.1 / 1.7 ms** | **2.2 / 2.8 / 3.0 ms** | 40.0 KB (41.7) |
| H.264 1440p60, 50 Mbps | 2.5 / 2.9 ms | 5.4 / 5.8 / 6.2 ms | 106.8 KB (104.2) |
| HEVC 1440p120, 60 Mbps | 1.6 / 1.8 ms | 3.5 / 3.7 / 3.9 ms | 60.2 KB (62.5) |

Notes:
- Slicing costs nothing measurable in encode time: one slice versus four with a blocking lock was the same.
- Sub-frame readback delivers the first slice about **1.7 ms (50%) earlier** than the full picture, so packetizing and sending can overlap encoding.
- HEVC is about 25–30% faster than H.264 on this GPU and stays inside the one-frame VBV.

### Power state: the biggest finding

Measured with `nvidia-smi` in the middle of a run:

| GPU state during encode | H.264 1080p60 full p50 | HEVC 1080p60 (`--detail 8`) full p50 |
|---|---|---|
| **P8, video clock 555 MHz, 36 W.** The GPU is otherwise idle; NVENC at 40% utilisation does **not** raise the P-state. | **7.9 ms** (first slice 4.1) | **6.2 ms** |
| P3, video clock 990 MHz, 85 W, with the GPU loaded by `--burn` in a second process | 4.3 ms (first slice 2.2) | 3.05 ms |
| Matrix A (higher state, not captured) | 3.3 ms | 2.4 ms |

Encode latency scales with the video clock. **On a host whose GPU is otherwise idle, such as desktop streaming, NVENC latency is 2–2.5× worse than on a host that is running a game.** Frames v2 must hold the GPU in a higher P-state while a session streams. Candidate fix, still to be verified: an NVAPI driver-settings (DRS) application profile setting "Power management mode: Prefer maximum performance" for `Frames.exe`, applied while a session is active and reverted afterwards. A cheap GPU keep-alive load is the fallback. Include this in the Phase 3 latency gate.

## Keyframes and intra refresh with a one-frame VBV

- **No periodic IDR.** Over 900 frames the stream contained only frame 0 and the forced frame 450 as IDRs (ffprobe: 2 key frames, everything else P).
- **A forced IDR costs the same bytes as a P frame:** 63.2 KB against a 62.7 KB median. The one-frame VBV caps it, so it spends quality (QP), not bitrate. There is no wire burst and no queueing spike; quality dips briefly instead. `lowDelayKeyFrameScale` is the knob for this trade-off.
- **Periodic intra refresh** (H.264, period = fps) shows an "I" (non-key) frame at every wave start and decodes with exit 0. FFmpeg logs `reference count overflow` warnings with `maxNumRefFrames=4` plus intra refresh, which still needs investigating with hardware decoders.

## Recovery: verified bit-exact

The spike writes the full stream and a "lossy" stream with these frames removed: 299 (single loss), 357–359 (burst), 449 (before the forced IDR) and 599 (before the forced intra refresh). The Mac's FFmpeg decodes both, and per-frame MD5 hashes are compared, so every frame after a recovery point must match bit for bit.

| Recovery | H.264 | HEVC |
|---|---|---|
| **RFI**, `nvEncInvalidateRefFrames(ts)` for the lost frame(s), then encode the next frame | **Bit-exact** from frame 300 and from frame 360. The next frame is an ordinary P frame of normal size (62 KB, not an IDR). | **Bit-exact**, same result |
| Forced IDR | Bit-exact | Bit-exact |
| **On-demand intra refresh** (`forceIntraRefreshWithFrameCnt = 10`) after an unannounced loss | Frames 600–608 differ (the wave); **bit-exact from 609 on** | **Fails.** The lost frame stays in the HEVC reference picture set. FFmpeg reports "Error constructing the frame RPS" for every frame from 600 to 899; 300 frames are lost and the stream never recovers. |

So the recovery ladder from the plan holds, with one rule: **invalidate the lost frames (RFI) before, or together with, any intra refresh. For HEVC this is mandatory.** Repeat with D3D11VA and VideoToolbox decoders in Phase 3, because their handling of missing references may differ from FFmpeg's.

## Gotchas

1. **Adapter selection.** On the test PC the default DXGI adapter is the AMD iGPU, and NVENC fails to open (`nvEncOpenEncodeSessionEx` error 1) on that device. Frames must create its capture device on the NVIDIA adapter, or copy across adapters if the display is driven by the iGPU.
2. **Sub-frame polling burns a CPU core.** Each frame took 400–1000 `nvEncLockBitstream` polls about 6 µs apart, spinning for the whole encode. Async mode can't be combined with `reportSliceOffsets`/sub-frame readback. Phase 3 should poll with a short sleep (about 100–200 µs; a slice takes about 0.4–0.8 ms) on a dedicated MMCSS thread, and measure the cost.
3. **H.264 at high fps with hard content hits QP 51 and overshoots the VBV.** Prefer HEVC on NVENC, or let the rate controller step resolution or fps down.
4. **Encoder config cannot be changed per picture.** Bitrate and resolution changes go through `nvEncReconfigureEncoder` (supported per caps, not exercised here).

## Consequences for `frames-codec` (Phase 3) and `EncoderCaps`

| `EncFeatures` | NVENC mapping | Status |
|---|---|---|
| `ZERO_COPY` | Register the D3D11 BGRA texture with `NV_ENC_BUFFER_FORMAT_ARGB`; NVENC converts colour internally | Verified |
| `ONE_FRAME_VBV` | `NV_ENC_CAPS_SUPPORT_CUSTOM_VBV_BUF_SIZE`, `vbvBufferSize = bitrate/fps` | Verified; no keyframe spikes |
| Infinite GOP | `gopLength = idrPeriod = NVENC_INFINITE_GOPLENGTH` | Verified |
| `RFI` | `NV_ENC_CAPS_SUPPORT_REF_PIC_INVALIDATION`; `EncCtl::Invalidate{first,last}` maps to one `nvEncInvalidateRefFrames(inputTimeStamp)` per frame. The engine keeps a mapping from frame id to timestamp. Needs a DPB of 4 or more. | Verified bit-exact, H.264 and HEVC |
| `INTRA_REFRESH` | `forceIntraRefreshWithFrameCnt` on demand; `intraRefreshPeriod` for periodic | Verified for H.264; HEVC only after RFI |
| `SLICE_OUTPUT` | `NV_ENC_CAPS_SUPPORT_SUBFRAME_READBACK`, `enableSubFrameWrite + reportSliceOffsets`, synchronous mode, `doNotWait` polling. Emit a `Chunk` per new slice using `sliceOffsets`. | Verified; first slice about 50% earlier |
| `LTR` | 8 / 7 slots | Not exercised |
| `DYN_BITRATE` / `DYN_RES` | `nvEncReconfigureEncoder` | Caps yes, not exercised |
| `YUV444` / `HDR10` | Caps yes (10-bit HEVC only) | Not exercised |

The direct-NVENC path is viable and gives everything Phase 3 needs. The **Phase 3 encode budget** (first slice about 1 ms, full picture under 3 ms at 1080p) is met with HEVC **only if the GPU is held out of P8**. Add a gate: "encode p50 ≤ 3 ms at 1080p60 on an otherwise idle desktop host".
