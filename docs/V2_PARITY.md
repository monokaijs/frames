# Frames v2 parity checklist

The Phase 6 gate (docs/V2_PLAN.md) requires every v1 feature in the README
to work on the v2 stack before the cutover. Status as of 2026-11-05.
**Done** means implemented on v2 and checked on real machines; **Partial**
says what is missing; **Not started** has no v2 code yet.

v2 today is the `frames-next` binary (host and viewer, command line). The
launcher, installer, updater, and service still run v1.

## Connecting and trust

| v1 feature | v2 status |
|---|---|
| Direct LAN/VPN connections | **Done**: iroh, direct paths found automatically (PC ↔ Mac on the LAN). |
| Internet connections (new in v2) | **Partial**: n0 relays and self-hosted relays work; NAT matrix not run. |
| Nearby discovery | **Not started** in the launcher; iroh dials by device id. |
| Pairing code confirmation | **Partial**: both ends show the code; the host approves on its terminal (no dialog yet). |
| Accept and trust / this session only / decline | **Partial**: view only or view and control, always remembered; no "this session only" yet. |
| Trusted devices reconnect without a prompt | **Done** (`trusted-v2.json`). |
| v1 trust carries over | **Done** on Windows (`--migrate-v1`); macOS Keychain path untested. |
| Unattended access (sharing at login, Windows service) | **Not started**. |
| Remove / revoke a trusted device | **Partial**: `frames-next peers` lists trusted devices, `frames-next revoke <id or prefix>` removes one (ambiguous prefixes refused); no launcher UI yet. |
| Device cards with screenshots, recent devices | **Not started**. |
| Sessions tab with live progress | **Not started**. |

## Streaming

| v1 feature | v2 status |
|---|---|
| Full desktop view | **Done** on Windows (WGC monitor capture); macOS uses ScreenCaptureKit but was only run with the generated source (Screen Recording grant needed). |
| Individual remote application windows | **Not started** (window catalog, per-window capture, local windows). |
| Selected windows / One window sharing scopes | **Not started**. |
| Isolated input for games (One window) | **Not started**. |
| Codec choice and automatic selection | **Done**: NVENC (H.264/HEVC), AMF/QSV/MF and software AV1 through FFmpeg, VideoToolbox; host picks the best shared codec. VP9 dropped. |
| Host audio | **Partial**: 10 ms Opus datagrams with the previous packet repeated, v1's jitter buffer and drift-corrected playback (`frames-audio`, cpal on both platforms), `frames-next --audio on|qa` on both ends. QA run PC host → Mac viewer (generated 440 Hz tone, silent sink): 800 of 800 packets in 8 s, none concealed, the tone's 880 zero crossings per second recovered. Windows loopback capture and real playback are ported but not exercised automatically (they would capture and play the user's audio); macOS host capture (ScreenCaptureKit audio) is not wired yet. |
| Image quality settings, live change | **Partial**: bitrate ceiling from the command line; adaptive rate, probing. |
| Debug overlay | **Partial**: per-second statistics printed; no overlay. |
| Low latency | **Done**: PC capture → present p50 3-5 ms; PC ↔ Mac 8-12 ms; photodiode check pending. |

## Input

| v1 feature | v2 status |
|---|---|
| Mouse, wheel, keyboard | **Done** on both viewers and both hosts (macOS host needs Accessibility; posting not exercised automatically). |
| Release input on focus loss / exit | **Done**. |
| Ctrl/Command swap, physical-key profile | **Partial**: the host states its OS in the stream description and a viewer on the other OS swaps Control and Command (both sides), as v1 does by default; Mac viewer of a PC host swaps, PC of PC does not. No physical-key profile switch yet. |
| Ctrl+Alt+F12 release shortcut, menus | **Not started**. |
| Host cursor shown locally | **Done** on both viewers. |
| Relative mouse (games) | **Not started**. |

## Data and devices

| v1 feature | v2 status |
|---|---|
| Clipboard (text and PNG) | **Partial**: `frames-clipboard` sync (no echo, the pre-session clipboard stays local), Windows and macOS clipboards, `frames-next --clipboard on|qa` (off by default). QA run PC host ↔ Mac viewer: 4 of 4 copies arrived. The real clipboards were not exercised automatically (tests must not overwrite the user's clipboard). |
| File drop, fetch, resume, checksums | **Partial**: viewer → host sending (`frames-next connect --send FILE`, host `--receive-dir`, default ~/Downloads/Frames, only from a device trusted for control): SHA-256 checked, resumed from the partial file after an interruption, never overwrites (`name (2).ext`). Mac → PC: 50 MB in 1.1 s; a 300 MB transfer killed at 4 s resumed at 197 MB and verified. Not yet: host → viewer fetch, drag and drop. |
| Android ADB bridge | **Not started**. |
| Gamepad forwarding | **Not started**. |
| USB/IP passthrough | **Not started**. |

## App and platform

| v1 feature | v2 status |
|---|---|
| Launcher (Slint) | **Not started** for v2 sessions. |
| Installer, signed updates, update channel | **Not started** ("Frames Next" channel). |
| Windows service, secure desktop | **Not started**. |
| macOS menu bar item, Dock behaviour | **Not started**. |
| `frames doctor`, `discover`, `peers`, `revoke` | **Not started**. |
| Single process on macOS (no helper) | **Done** for media; the launcher still uses the v1 helper. |
