# Frames v2 parity checklist

The Phase 6 gate (docs/V2_PLAN.md) requires every v1 feature in the README
to work on the v2 stack before the cutover. Status as of 2026-11-06.

0.5.0 shipped claiming more than it had: per-window viewing lacked v1's
connection menu, title-bar dropdown and window options. Those were ported
on 2026-11-06 (see "Viewer windows and connection menu" below); the
checks say what ran on hardware and what only passed tests.
**Done** means implemented on v2 and checked on real machines; **Partial**
says what is missing; **Not started** has no v2 code yet.

v2 today is the `frames-next` binary (host and viewer, command line), also
built into the regular app as `frames next ...` (same commands, so a
release carries v2 without changing what users see). The
launcher, installer, updater, and service still run v1.

## Connecting and trust

| v1 feature | v2 status |
|---|---|
| Direct LAN/VPN connections | **Done**, with a caveat: iroh, direct paths found automatically. With a Mac host and a Windows viewer the selected direct path can go silent one way (iroh multipath); hosts send a heartbeat and viewers drop a silent direct path within 0.5 s and carry on over the relay (checked: losses 448 → 33 frames in 60 s; short hitches remain while iroh retries direct). |
| Internet connections (new in v2) | **Partial**: n0 relays and self-hosted relays work; NAT matrix not run. |
| Nearby discovery | **Done**: hosts advertise `_frames2._udp` (device id and name); the launcher's Devices list shows them (`frames discover` too), and connecting dials a nearby host's LAN addresses directly (works without internet) or an unambiguous id prefix. |
| Pairing code confirmation | **Done**: the host shows the viewer's name and the code in a launcher prompt (`--ask-launcher`: a `Frames pairing:` line, answered on the host's stdin), or on its terminal from the command line; the viewer's Sessions row shows the code while it waits. |
| Accept and trust / this session only / decline | **Done**: Accept and trust (saved with the viewer's name), Accept this time (for the rest of that sharing session, so each of its windows is not asked again), Decline. Checked end to end on the Mac with the launcher's own command lines: once, trusted, then no prompt. Trusted devices are re-checked on every connection, so removing one takes effect at once. |
| Trusted devices reconnect without a prompt | **Done** (`trusted-v2.json`). |
| v1 trust carries over | **Done** on Windows (`--migrate-v1`); macOS Keychain path untested. |
| Unattended access (sharing at login, Windows service) | **Partial**: `frames host` runs the v2 host, so the macOS login agent (`Frames host --unattended`) shares on v2 for trusted devices; the Windows service agent runs the v2 host with this account's device key and trust list, copied at install. Neither has been run on hardware yet. The Unattended access setting is gone: trusted devices always reconnect without a prompt. |
| Access password (new) | **Done**: Settings > Sharing > Access password; a viewer that gives it connects without approval at the host (PBKDF2 key salted with the host id, HMAC of the session's TLS exporter; 5 wrong in a minute pause attempts). Checked on the Mac: right password admitted with nobody at the host, wrong one refused as wrong, none refused. |
| Remove / revoke a trusted device | **Done**: launcher Devices > Remove, `frames revoke <id>`, `frames peers`. |
| Device cards with screenshots, recent devices | **Done**: cards with names, online state and recent devices; trusted nearby hosts send a 480 px PNG preview of their screen at most once a minute (`PREVIEW` stream; refused to untrusted devices without asking anyone). Checked: laptop → Mac 480×300 PNG; an untrusted device refused. |
| Sessions tab with live progress | **Done**: connecting, waiting (with the pairing code), connected, with the host's name; failures explained (refused, not found, no answer). |

## Viewer windows and connection menu (ported 2026-11-06)

The viewer's menus come from one controller (`crates/frames-next/src/control.rs`):
a snapshot the platforms build menus from, and commands they send back.
`--qa-command <s>=<json>` runs commands on a timer (unattended tests).

| v1 feature | v2 status |
|---|---|
| Title-bar dropdown ("Connection menu") | **Done**: a chevron beside minimize on Windows, a title-bar accessory on Mac, with v1's labels and order; not clicked through by a person yet. |
| Windows list, show / hide, Show all, Hide all, Ignore / hide this window | **Done**: hiding stops that window's stream; checked with scripted commands (hide closed the session, Show all reopened it). Windows shows "Frames connection" when every window is hidden. |
| Per-window view / Full desktop view during a session | **Done**: checked windows → desktop → windows with scripted commands; the launcher remembers the last view per device. |
| Image quality, Show remote cursor (remembered per device) | **Done**: checked with scripted commands (host capped at 4 Mbps, cursor drawn into the picture). |
| Show remote window header (cropped by default) | **Done**: Windows hosts crop to the client area (checked: Notepad client 1797x1256, picture 1796x1256) and re-crop at once on a change; Mac hosts crop the standard title bar (Accessibility). |
| Resize remote app / Stretch locally / Fill or fit | **Done** in code on both platforms; remote resize not exercised (it would resize someone's window). |
| Swap Control / Command, Physical keys, typed text (IME, dead keys, AltGr) | **Done** in code on both platforms; typing through an IME not exercised. |
| Refresh application menus, Fetch remote file…, received-files list | **Done**: Fetch prompt and list on both platforms; drag out of the list, Cancel transfers. |
| Remote menus, tooltips, popups | **Done** in code: listed by the host with a parent and shown as frameless child windows; about 0.5 s late (catalog poll). |
| Raise the host window before input, host input errors in the title | **Done**: notices on a `NOTICE` stream (Windows: focus refused; Mac: Accessibility missing). |
| Window icons, taskbar grouping (Windows), Dock menu per remote app (Mac) | **Done**: Mac has one Dock tile with a menu per remote app (one process has one tile). |
| Remote modal dialogs (Mac) | **Done** in code: input to a blocked window goes to its dialog. |
| Debug overlay with graphs | **Done**: v1's panel and graphs on both viewers (checked rendering on both). |
| Hidden or unfocused windows | **Done**: paused while hidden, 24 fps unfocused (`Event::Visibility`). |
| Devices menu (gamepads, USB, ADB during a session) | **Done** in code; gamepads only while a viewer window has focus. |
| One window (games): Relative mouse, Window compatibility, physical keys | **Done**: compatibility from v1's bundled catalog by the executable's SHA-256. |

## Fixes from cross-machine QA (2026-11-06, after 0.6.0)

| Issue | Status |
|---|---|
| Windows viewer: picture shrunk to the top left with black right/bottom bars | **Fixed, reference-probe verified**. The presenter now sets the video processor's source rect, and each decoded frame carries its own visible size (`frames_decoder_receive_surface_sized`) instead of the size the decoder was opened with. A Mac VideoToolbox decode and a PC2 render of the same captured frame match within 0.92/255 mean RGB at 440x320. Live from a Mac host (Codex, 2026-11-06): a Windows viewer of a Mac host's 440x320 and 360x260 windows showed both full pictures with no black bars (two streams at 57 fps, nothing lost); resizes to 640x400 and to an odd 441x321 (sent as 440x320) were followed live; dragging the Windows viewer's own resize grip in Resize remote mode resized the Mac window 440x320 → 636x434, and the settled picture reached every edge with no padding. Live as well: a Mac sheet already open before connecting shows over its parent's picture, frameless, with no extra menu row; a modeless dialog opens as its own captioned owned window, its parent stays usable while it is open, and it closes from the viewer. A full-desktop session and per-window sessions ran side by side. |
| "Static surface pool size exceeded" after stalls | **Fixed in code, tested**: viewers keep only the newest frame of a burst (`ClientSession::poll_latest`), and the decoder pool has 8 spare surfaces. Test: crates/frames-testkit/tests/decoded_surfaces.rs. |
| Hiding and showing a window again hung the host; a second viewer could not connect | **Fixed, verified live** (3a89e15). Hosts took one connection at a time, and a session ending dropped a handshake that was under way, so after a window was hidden no new session was accepted. Each connection is now accepted and admitted on its own task (20 s handshake limit), and devices already let in never wait for a new device's prompt. Viewers connect each window on its own task and retry a failed one, where one failed connection had ended the whole view. Live, Windows viewers of a Mac host (Codex, 2026-11-06): the main window hidden and shown twice, each time a new stream while the second window kept streaming; a second viewer on the laptop connected meanwhile; both at 57-58 fps, nothing lost. Loopback test: a stalled handshake does not hold up the next connection. |
| Mac viewer: the whole picture black (frames decoded and presented) | **Fixed, A/B proven** (Codex, 2026-11-06): the Metal layer's first Core Animation transaction was never committed before AppKit took the layer over. The presenter now flushes it on its render thread first. Original build black, fixed build correct, original rebuilt black again. The presenter change is in Codex's working tree, not yet committed; final Mac app build pending. |
| Mac host: one missed VideoToolbox callback ended the session | **Fixed in code, tested**: the frame is dropped and recovery uses a keyframe, or a new session after three in a row; late outputs are matched to their call. Unit tests only. |
| Quick Apps (new) | **Backend done, end-to-end on Windows (logs)**: allowlisted host apps by install location (path/bundle and process ancestry), `--app ID [--only-app]`. Checked laptop viewer → laptop host with a test app (main window, owned dialog, context menu) and a decoy window from another program: a view-only viewer does not start it; with control it starts and only its window, dialog and menu are shared (the decoy never), with its own audio; the desktop view is refused with only-app; an already-running app is not started again. Launcher UI by the UI pass, not yet visually verified. Mac host, live (Codex, 2026-11-06, Windows viewer): a Quick App configured under /tmp (macOS reports it as /private/tmp) was started and only its own windows were listed, not an identical copy's or any other app's (the host had no window filter); with only-app the desktop view was refused; a view-only viewer did not start it and was told control is needed. Process ids reused by Windows no longer count as the app's (start times are checked). |
| Typed text and popups, Windows hosts | **Checked end to end (logs)**: Mac and Windows viewers' generated input reaches a Windows host's test window as characters ("aé…", no raw keys); a real Win32 context menu reaches the viewer as its own popup while open. Not clicked through in the real UI. |
| Frames' own windows in a Windows host's list | **Fixed**: excluded by window class as well as executable name, so a renamed Frames copy is never shared (a viewer showing itself multiplies streams). |

## Streaming

| v1 feature | v2 status |
|---|---|
| Full desktop view | **Done** on Windows (WGC monitor capture); macOS uses ScreenCaptureKit but was only run with the generated source (Screen Recording grant needed). |
| Individual remote application windows | **Done**: the host lists its application windows on a catalog stream as they open, close and are renamed (at most Settings > Shared window limit); the viewer (`--apps`, the launcher's Application windows view) opens a session and local window per remote window, titled like it. Windows hosts capture by HWND (WGC), Mac hosts by CGWindowID (ScreenCaptureKit, ABI 4). A resized remote window is followed live (encoder reconfigured with a keyframe, viewer window reshaped at the same scale); local windows keep the picture's shape, so no bars appear (checked with Notepad on the laptop resized twice). Closing a local window asks the host window to close (WM_CLOSE; its close button through Accessibility on macOS); a second close closes it locally. The application's menus are shown in the viewer window (Win32 menu bars read on the host; macOS menu bars through Accessibility) and run on the host when chosen (checked: System Information's File/Edit/View/Help, 22 items, reached the Mac viewer; choosing items not exercised automatically). Not yet: child-window placement and z-order. |
| Selected windows / One window sharing scopes | **Done**: Selected windows lists only those windows to the viewer (`--window` filters on the host); One window shares one window and, on a Windows host, uses isolated input (below). |
| Isolated input for games (One window) | **Done** on Windows hosts: `--isolated` (the launcher's One window) attaches the `FramesInput.dll` adapter (built and embedded by frames-platform-windows) to the window's process; viewer input goes there only, heartbeats keep the 500 ms lease, the next click or key resumes after a pause, and the game's cursor is sent to the viewer. If it cannot attach, the viewer gets no input rather than the host's mouse and keyboard. Checked with the shared-input fixture, Mac viewer → laptop: input delivered raw and legacy, 0 errors, the host's real cursor unmoved. On 2026-11-06 the fixture (DPI-unaware, on a 150% display) showed clicks landing 1.5x too far: positions were computed in this process's DPI context; they now use the game window's own. macOS hosts use normal input, as in v1. |
| Codec choice and automatic selection | **Done**: NVENC (H.264/HEVC), AMF/QSV/MF and software AV1 through FFmpeg, VideoToolbox; host picks the best shared codec. VP9 dropped. Without `--encoder`, a Windows host tries NVENC, AMF, QSV (for the GPUs present), then MF and software AV1, keeping the first whose test encode of the first frame works, so an NVIDIA driver too old for NVENC falls back instead of failing. NVENC runs on API 13.1 (driver 570+), 12.2 (551+), 12.1 (531+) or 12.0 (522+), whichever is the newest the driver has: the same encoder code is compiled against each version's header. 13.1 ran on the RTX 3080 Ti (driver 621); 12.2, 12.1 and 12.0 on the laptop's RTX 4050 (driver 555, with `FRAMES_NVENC_API=12.1` or `12.0` forcing the older ones, which a newer driver accepts): each passes the doctor's test encode, and 30 s live on 12.0 gave capture to present p50 8.4 ms, p95 10.7 ms, encode 2.5 ms, nothing lost. On the laptop (RTX 4050, driver 555) NVENC now runs on API 12.2: 60 s to the Mac at 60 fps, HEVC encode p50 2.4 ms (QSV took 11.5 ms), capture to present p50 9.0 ms, p95 14.7 ms (QSV: 21.6 and 28.8 ms); a Wi-Fi stall at 18 s (rtt 12 ms) was recovered by invalidation (5) with 7 keyframes in all. `frames next doctor` test-encodes each one with a generated frame (no screen capture needed). Checked on a laptop with driver 555: NVENC (API 12.2), QSV HEVC/H.264 on the Iris Xe, MF H.264 and software AV1 all working; before the 12.x bindings NVENC was refused there and QSV chosen. The test encode also changes the bitrate once, since QSV on that laptop failed only then (`MFXVideoENCODE_Reset`: incompatible video parameters, with HRD conformance, low power and external BRC each tried off). Encoders that cannot change bitrate live (QSV, MF) restart with a keyframe instead: soon (at most once a second) to fall, every 5 s and only by 25% or more to climb. The replacement opens on another thread while the old one keeps encoding, because a restart took QSV HEVC 0.25 s, QSV H.264 1.1 s and MF 0.1 s, and frame numbers carry on across it. A frame QSV skips ("Invalid FrameType:0") is dropped rather than ending the session. Live from that laptop's desktop to the Mac: QSV HEVC at 60 fps, capture to present p50 21 ms; a 60 s run through six bitrate changes (60 to 13.7 Mbps and back) finished without a stall or error. With the desktop unlocked: 60 s at 58 fps (3436 frames, 5 lost to a Wi-Fi burst and recovered), capture to present p50 21.6 ms, p95 28.8 ms, while the target fell 60 to 24 Mbps; QSV HEVC encode is 11.5 ms of that. |
| Host audio | **Partial**: real capture never reached the encoder before 2026-11-06 (only the QA tone did); fixed. A Windows host sends system audio, or only the shared app's (process loopback) when sharing selected windows of one app (checked: 3982 packets laptop → Mac, none lost). Mac hosts capture through ScreenCaptureKit (whole Mac or one app), not yet run with the Screen Recording grant. 10 ms Opus datagrams with the previous packet repeated, v1's jitter buffer and drift-corrected playback (`frames-audio`, cpal on both platforms), `frames-next --audio on|qa` on both ends. QA run PC host → Mac viewer (generated 440 Hz tone, silent sink): 800 of 800 packets in 8 s, none concealed, the tone's 880 zero crossings per second recovered. Windows loopback capture and real playback are ported but not exercised automatically (they would capture and play the user's audio); macOS host capture is wired (FramesMac ABI 7, `frames_mac_audio_start`, whole Mac or one process) but not yet run. |
| Image quality settings, live change | **Done**: Settings > Image quality and Bandwidth limit cap what the viewer receives (sent in the hello); the viewer's Frames menu changes it during the session (`Event::MaxBitrate`, applied by the host's rate control at once). |
| Debug overlay | **Done**: see the connection menu section. |
| Low latency | **Done**: PC capture → present p50 3-5 ms; PC ↔ Mac 8-12 ms; photodiode check pending. |
| Host display stays awake while viewed (new in v2) | **Done**: the host holds a display and system wake request for the session (`SetThreadExecutionState` on Windows, a `PreventUserIdleDisplaySleep` assertion on macOS); a viewer is not local input, and a sleeping display gave Windows capture a few frames a second. Both checked with `powercfg /requests` and `pmset -g assertions`. |

## Input

| v1 feature | v2 status |
|---|---|
| Mouse, wheel, keyboard | **Done** on both viewers and both hosts (macOS host needs Accessibility; posting not exercised automatically). |
| Release input on focus loss / exit | **Done**. |
| Ctrl/Command swap, physical-key profile | **Done**: across platforms Control and Command trade places by default; Settings > Keyboard > Physical keys (`--physical-keys`) keeps them in place. |
| Ctrl+Alt+F12 release shortcut, menus | **Done**: Ctrl+Alt+F12 (Control+Option+F12) releases input and minimizes. The Frames menu (the window menu on Windows, the menu bar on Mac; viewers are regular Dock apps there) has image quality, Send file…, Show statistics, Release input and Disconnect. |
| Host cursor shown locally | **Done** on both viewers. |
| Relative mouse (games) | **Partial**: Control+Option+F11 on the Mac viewer holds the pointer (hidden, not following the mouse) and sends motion as running-total datagrams (a lost or reordered one costs no motion; the newest is repeated once 4 ms later); the host presses buttons where its pointer is. Swift test for the toggle and deltas with a stand-in lock; loopback QA run: 750 motion datagrams, host applied exactly (2250, -750). Hosts inject it (macOS: Quartz delta fields; Windows: relative `SendInput`), neither exercised (they move the real cursor). The Windows viewer has the same Ctrl+Alt+F11 toggle: motion from raw input (`WM_INPUT` deltas), the pointer confined to the viewer and hidden, given back on Ctrl+Alt+F12, focus loss and exit (QA run, laptop viewer to Mac host over LAN: 40 s of generated raw motion, host summed exactly (14997, -4999), the 3:1 steps intact; 60 fps, capture to present p50 13.1 ms, input to wire p50 15 µs; the real cursor was not confined in QA mode). |

## Data and devices

| v1 feature | v2 status |
|---|---|
| Clipboard (text and PNG) | **Partial**: `frames-clipboard` sync (no echo, the pre-session clipboard stays local), Windows and macOS clipboards, `frames-next --clipboard on|qa` (off by default). QA run PC host ↔ Mac viewer: 4 of 4 copies arrived. The real clipboards were not exercised automatically (tests must not overwrite the user's clipboard). |
| File drop, fetch, resume, checksums | **Done**: files dropped on a viewer window, or chosen with Frames menu > Send file…, go to the host's receive folder (default ~/Downloads/Frames; control-trusted devices only): SHA-256 checked, resumed after an interruption, never overwritten. `--get PATH` fetches from the host's home folder. Mac → PC: 50 MB in 1.1 s; a 300 MB transfer killed at 4 s resumed and verified. |
| Android ADB bridge | **Done**: Settings > Android ADB bridge (`--adb`) forwards this device's ADB server to hosts it controls (`adb -H 127.0.0.1 -P 15037` there; loopback only). Checked with an echo server standing in for ADB, not a real adb. |
| Gamepad forwarding | **Done, not checked with a controller**: Settings > Forward gamepads (`--gamepads`); every controller on the viewer (up to four, SDL3 in the `gamepad-worker` helper, `frames-devices`) becomes a virtual Xbox 360 controller on a Windows host (ViGEmBus), with rumble back and stale reports dropped, as in v1, on a `DEVICES` stream (control-trusted devices only). The helper starts on the Mac; no controller was attached to test. |
| USB/IP passthrough | **Done, not checked on hardware**: Settings > USB passthrough (Windows) lists this PC's USB devices; chosen ones (`--usb BUS-ID`) are exported with an elevated lease and attached on the Windows host through USB/IP, bridged on `USB` streams, as v1 did (`frames-devices` usb/usbip). Needs usbipd and the USB/IP client on two Windows PCs; only one is available to test. |

## App and platform

| v1 feature | v2 status |
|---|---|
| Launcher (Slint) | **Done** for sessions: Start sharing, Connect, the pairing prompt, devices and trust run on v2; settings map to v2 options (audio, clipboard, codec, encoder, quality and bandwidth as the viewer's cap, shared window limit). Sessions end with the launcher (stdin closes), and hold the update lease. The v1 engine is removed from the app. |
| Installer, signed updates, update channel | **Done**: the app is Frames Next now, so the existing installer, DMG and signed updates ship it; no separate channel. |
| Windows service, secure desktop | **Done, not run on hardware**: the service agent runs the v2 host with this account's device key, trust list and access password (copied at install); while a secure desktop (UAC, sign-in) is the input desktop it is shown through DXGI Desktop Duplication and driven from a thread on that desktop (`frames-platform-windows` secure). Installing the service was not tried on the test laptop. |
| macOS menu bar item, Dock behaviour | **Done**: the launcher's menu bar item is unchanged; viewer windows belong to a regular app with a Dock icon and the Frames menu. |
| `frames doctor`, `discover`, `peers`, `revoke` | **Done**: `frames doctor|discover|peers|revoke|identity` run on Frames Next (`frames next preview ID FILE` and `password set|clear|status` too). |
| Single process on macOS (no helper) | **Done** for media; the launcher still uses the v1 helper. |
