# Frames v2 parity checklist

The Phase 6 gate (docs/V2_PLAN.md) requires every v1 feature in the README
to work on the v2 stack before the cutover. Status as of 2026-11-05.
**Done** means implemented on v2 and checked on real machines; **Partial**
says what is missing; **Not started** has no v2 code yet.

v2 today is the `frames-next` binary (host and viewer, command line), also
built into the regular app as `frames next ...` (same commands, so a
release carries v2 without changing what users see). The
launcher, installer, updater, and service still run v1.

## Connecting and trust

| v1 feature | v2 status |
|---|---|
| Direct LAN/VPN connections | **Done**: iroh, direct paths found automatically (PC ↔ Mac on the LAN). |
| Internet connections (new in v2) | **Partial**: n0 relays and self-hosted relays work; NAT matrix not run. |
| Nearby discovery | **Partial**: hosts advertise `_frames2._udp` (device id and name); `frames-next discover` lists nearby hosts; `connect` dials a nearby host's LAN addresses directly (works without internet; 129 ms to connect PC ← Mac) and accepts an unambiguous id prefix. No launcher cards yet. |
| Pairing code confirmation | **Done**: the host shows the viewer's name and the code in a launcher prompt (`--ask-launcher`: a `Frames pairing:` line, answered on the host's stdin), or on its terminal from the command line; the viewer's Sessions row shows the code while it waits. |
| Accept and trust / this session only / decline | **Done**: Accept and trust (saved with the viewer's name), Accept this time (for the rest of that sharing session, so each of its windows is not asked again), Decline. Checked end to end on the Mac with the launcher's own command lines: once, trusted, then no prompt. Trusted devices are re-checked on every connection, so removing one takes effect at once. |
| Trusted devices reconnect without a prompt | **Done** (`trusted-v2.json`). |
| v1 trust carries over | **Done** on Windows (`--migrate-v1`); macOS Keychain path untested. |
| Unattended access (sharing at login, Windows service) | **Partial**: `frames host` runs the v2 host, so the macOS login agent (`Frames host --unattended`) shares on v2 for trusted devices; the Windows service agent runs the v2 host with this account's device key and trust list, copied at install. Neither has been run on hardware yet. The Unattended access setting is gone: trusted devices always reconnect without a prompt. |
| Remove / revoke a trusted device | **Done**: launcher Devices > Remove, `frames revoke <id>`, `frames peers`. |
| Device cards with screenshots, recent devices | **Partial**: cards with names, online state (v2 local discovery) and recent devices (dialled by id); no preview screenshots yet. |
| Sessions tab with live progress | **Done**: connecting, waiting (with the pairing code), connected, with the host's name; failures explained (refused, not found, no answer). |

## Streaming

| v1 feature | v2 status |
|---|---|
| Full desktop view | **Done** on Windows (WGC monitor capture); macOS uses ScreenCaptureKit but was only run with the generated source (Screen Recording grant needed). |
| Individual remote application windows | **Partial**: the host lists its application windows on a catalog stream as they open, close and are renamed (at most Settings > Shared window limit); the viewer (`--apps`, the launcher's Application windows view) opens a session and local window per remote window, titled like it, and closes it with the remote one. Windows hosts capture by HWND (WGC), Mac hosts by CGWindowID (ScreenCaptureKit, ABI 4), with input following the window as it moves. A resized remote window is followed live: the host waits 150 ms for the size to settle, reconfigures the encoder with a keyframe and tells the viewer, which recreates its decoder and reshapes its window at the same scale; local windows keep the picture's shape while resized, so no bars appear (checked: Notepad on the laptop, resized twice while viewed from the Mac). Not yet: host menus, closing a host window from its local window, child windows/z-order. |
| Selected windows / One window sharing scopes | **Partial**: on a Windows host, `frames-next host --window A --window B ...` shares those windows (title match; `frames-next windows` lists them), one session per window, real input landing on wherever each window is; a viewer opens them with `--windows N`, one local window each. Two test windows PC → Mac at once: p50 3.6 and 4.1 ms. No macOS window capture, no picker UI, no window catalog (windows opening and closing). |
| Isolated input for games (One window) | **Not started**. |
| Codec choice and automatic selection | **Done**: NVENC (H.264/HEVC), AMF/QSV/MF and software AV1 through FFmpeg, VideoToolbox; host picks the best shared codec. VP9 dropped. Without `--encoder`, a Windows host tries NVENC, AMF, QSV (for the GPUs present), then MF and software AV1, keeping the first whose test encode of the first frame works, so an NVIDIA driver too old for NVENC falls back instead of failing. NVENC runs on API 13.1 (driver 570+), 12.2 (551+), 12.1 (531+) or 12.0 (522+), whichever is the newest the driver has: the same encoder code is compiled against each version's header. 13.1 ran on the RTX 3080 Ti (driver 621); 12.2, 12.1 and 12.0 on the laptop's RTX 4050 (driver 555, with `FRAMES_NVENC_API=12.1` or `12.0` forcing the older ones, which a newer driver accepts): each passes the doctor's test encode, and 30 s live on 12.0 gave capture to present p50 8.4 ms, p95 10.7 ms, encode 2.5 ms, nothing lost. On the laptop (RTX 4050, driver 555) NVENC now runs on API 12.2: 60 s to the Mac at 60 fps, HEVC encode p50 2.4 ms (QSV took 11.5 ms), capture to present p50 9.0 ms, p95 14.7 ms (QSV: 21.6 and 28.8 ms); a Wi-Fi stall at 18 s (rtt 12 ms) was recovered by invalidation (5) with 7 keyframes in all. `frames next doctor` test-encodes each one with a generated frame (no screen capture needed). Checked on a laptop with driver 555: NVENC (API 12.2), QSV HEVC/H.264 on the Iris Xe, MF H.264 and software AV1 all working; before the 12.x bindings NVENC was refused there and QSV chosen. The test encode also changes the bitrate once, since QSV on that laptop failed only then (`MFXVideoENCODE_Reset`: incompatible video parameters, with HRD conformance, low power and external BRC each tried off). Encoders that cannot change bitrate live (QSV, MF) restart with a keyframe instead: soon (at most once a second) to fall, every 5 s and only by 25% or more to climb. The replacement opens on another thread while the old one keeps encoding, because a restart took QSV HEVC 0.25 s, QSV H.264 1.1 s and MF 0.1 s, and frame numbers carry on across it. A frame QSV skips ("Invalid FrameType:0") is dropped rather than ending the session. Live from that laptop's desktop to the Mac: QSV HEVC at 60 fps, capture to present p50 21 ms; a 60 s run through six bitrate changes (60 to 13.7 Mbps and back) finished without a stall or error. With the desktop unlocked: 60 s at 58 fps (3436 frames, 5 lost to a Wi-Fi burst and recovered), capture to present p50 21.6 ms, p95 28.8 ms, while the target fell 60 to 24 Mbps; QSV HEVC encode is 11.5 ms of that. |
| Host audio | **Partial**: 10 ms Opus datagrams with the previous packet repeated, v1's jitter buffer and drift-corrected playback (`frames-audio`, cpal on both platforms), `frames-next --audio on|qa` on both ends. QA run PC host → Mac viewer (generated 440 Hz tone, silent sink): 800 of 800 packets in 8 s, none concealed, the tone's 880 zero crossings per second recovered. Windows loopback capture and real playback are ported but not exercised automatically (they would capture and play the user's audio); macOS host capture (ScreenCaptureKit audio) is not wired yet. |
| Image quality settings, live change | **Partial**: bitrate ceiling from the command line; adaptive rate, probing. |
| Debug overlay | **Partial**: per-second statistics printed, and the viewer window's title shows frame rate, capture → present p50/p95, path (direct or relayed) with RTT, and frames lost, refreshed each second (not read back automatically). No graphs or panel. |
| Low latency | **Done**: PC capture → present p50 3-5 ms; PC ↔ Mac 8-12 ms; photodiode check pending. |
| Host display stays awake while viewed (new in v2) | **Done**: the host holds a display and system wake request for the session (`SetThreadExecutionState` on Windows, a `PreventUserIdleDisplaySleep` assertion on macOS); a viewer is not local input, and a sleeping display gave Windows capture a few frames a second. Both checked with `powercfg /requests` and `pmset -g assertions`. |

## Input

| v1 feature | v2 status |
|---|---|
| Mouse, wheel, keyboard | **Done** on both viewers and both hosts (macOS host needs Accessibility; posting not exercised automatically). |
| Release input on focus loss / exit | **Done**. |
| Ctrl/Command swap, physical-key profile | **Partial**: the host states its OS in the stream description and a viewer on the other OS swaps Control and Command (both sides), as v1 does by default; Mac viewer of a PC host swaps, PC of PC does not. No physical-key profile switch yet. |
| Ctrl+Alt+F12 release shortcut, menus | **Partial**: Ctrl+Alt+F12 (Control+Option+F12 on a Mac) releases remote input and minimizes the viewer, never reaching the host (Swift test on the Mac; Windows compiled, not exercised: it reads the real keyboard state). No Frames menu yet. |
| Host cursor shown locally | **Done** on both viewers. |
| Relative mouse (games) | **Partial**: Control+Option+F11 on the Mac viewer holds the pointer (hidden, not following the mouse) and sends motion as running-total datagrams (a lost or reordered one costs no motion; the newest is repeated once 4 ms later); the host presses buttons where its pointer is. Swift test for the toggle and deltas with a stand-in lock; loopback QA run: 750 motion datagrams, host applied exactly (2250, -750). Hosts inject it (macOS: Quartz delta fields; Windows: relative `SendInput`), neither exercised (they move the real cursor). The Windows viewer has the same Ctrl+Alt+F11 toggle: motion from raw input (`WM_INPUT` deltas), the pointer confined to the viewer and hidden, given back on Ctrl+Alt+F12, focus loss and exit (QA run, laptop viewer to Mac host over LAN: 40 s of generated raw motion, host summed exactly (14997, -4999), the 3:1 steps intact; 60 fps, capture to present p50 13.1 ms, input to wire p50 15 µs; the real cursor was not confined in QA mode). |

## Data and devices

| v1 feature | v2 status |
|---|---|
| Clipboard (text and PNG) | **Partial**: `frames-clipboard` sync (no echo, the pre-session clipboard stays local), Windows and macOS clipboards, `frames-next --clipboard on|qa` (off by default). QA run PC host ↔ Mac viewer: 4 of 4 copies arrived. The real clipboards were not exercised automatically (tests must not overwrite the user's clipboard). |
| File drop, fetch, resume, checksums | **Partial**: viewer → host sending (`frames-next connect --send FILE`, host `--receive-dir`, default ~/Downloads/Frames, only from a device trusted for control): SHA-256 checked, resumed from the partial file after an interruption, never overwrites (`name (2).ext`). Mac → PC: 50 MB in 1.1 s; a 300 MB transfer killed at 4 s resumed at 197 MB and verified. Fetching from the host (`--get PATH`, relative to the host user's home; `..`, absolute paths, folders and symlinks out of home are refused with the reason): 20 MB PC → Mac in 0.3 s, verified. Not yet: drag and drop. |
| Android ADB bridge | **Partial**: viewer `--adb` forwards its ADB server (`--adb-server`, default 127.0.0.1:5037) to `127.0.0.1:15037` on a host started with `--adb` (loopback only, control-trusted viewers only), one stream per connection. Checked PC host → Mac viewer with an echo server standing in for ADB. Not checked against a real ADB server and device. |
| Gamepad forwarding | **Not started**. |
| USB/IP passthrough | **Not started**. |

## App and platform

| v1 feature | v2 status |
|---|---|
| Launcher (Slint) | **Done** for sessions: Start sharing, Connect, the pairing prompt, devices and trust run on v2; settings map to v2 options (audio, clipboard, codec, encoder, quality and bandwidth as the viewer's cap, shared window limit). Sessions end with the launcher (stdin closes), and hold the update lease. The v1 engine is removed from the app. |
| Installer, signed updates, update channel | **Not started** ("Frames Next" channel). |
| Windows service, secure desktop | **Not started**. |
| macOS menu bar item, Dock behaviour | **Not started**. |
| `frames doctor`, `discover`, `peers`, `revoke` | **Partial**: `frames next discover`, `peers`, `revoke` and `windows` work from the app binary (Mac and Windows); `frames next doctor` reports identity and trust, macOS permissions (Screen Recording, Accessibility), whether VideoToolbox opens each codec, relay reachability, direct addresses and nearby hosts (checked on the Mac; the Windows section only lists the encoder and shareable windows and has not been run). |
| Single process on macOS (no helper) | **Done** for media; the launcher still uses the v1 helper. |
