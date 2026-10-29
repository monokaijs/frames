# Frames

Native per-window remote control for Windows 11 x64 and macOS 14.2+ on Apple silicon. Frames is an independent Rust application with a Slint connection workspace and Win32/D3D11 and AppKit/Metal remote-window frontends.

**Development build, not a production release.** Live Mac capture/control, Windows-to-Mac AppKit/Metal video, native menu projection, desktop/window switching, codec roundtrips, and bidirectional LAN transfers have passed development checks. Resume latency and the full acceptance matrix still need work. See [release gates](docs/VALIDATION.md) for measurements and remaining checks.

## Build and run on Windows

Install Rust and either Visual Studio C++ Build Tools or an existing MSYS2 UCRT64 toolchain. Install FFmpeg with H.264/HEVC hardware encoders, libvpx, libaom, and libopus support; Frames probes actual working encoders and decoders rather than assuming GPU capabilities.

```powershell
.\scripts\build.ps1
.\dist\windows\Frames.exe
```

Launching without arguments opens the Slint workspace. **Connections** contains connect/share actions, session controls, and nearby discovery; **Devices** contains recent addresses and trusted certificates; **Permissions** guides macOS setup and sharing preferences. Enter a numeric IP address (port **47990** is used when omitted), or use **Scan network** to discover hosts. Both devices must explicitly approve the same pairing code. Closing the launcher stops the sessions it started. See [launcher design and preview](docs/LAUNCHER.md).

CLI equivalents:

```powershell
# Host, with attended pairing enabled
Frames.exe host --pair
# Allow devices approved during this run to reconnect without further approval
Frames.exe host --pair --unattended
# Client
Frames.exe connect 192.168.1.10:47990 --audio
# Optional manual codec: auto, h264, hevc, av1, vp9
Frames.exe connect 192.168.1.10:47990 --codec vp9
Frames.exe doctor
Frames.exe discover
Frames.exe peers
Frames.exe revoke CERTIFICATE_FINGERPRINT
```

The default UDP port is **47990**. Allow Frames through the firewall on the intended private network. LAN/VPN connectivity is direct; this build has no cloud account, relay, or NAT traversal service.

Use the native **Frames** menu to choose sizing, input profiles, file fetching, and view mode. On macOS it lives in the system menu bar; on Windows it lives below the local title bar. The source application's exposed menus appear alongside it: Accessibility menus for macOS hosts and standard Win32 menus for Windows hosts. Focus a window to refresh its menus, or choose **Refresh application menus**. Disabled items and check marks are preserved. Custom-drawn menus remain usable in the captured image. Windows title-bar system-menu commands are also retained. **Ctrl+Alt+F12** releases remote input and minimizes the client window.

Seamless streams show application content without duplicated system chrome. Windows uses the native client rectangle to exclude standard borders, title bars, and menu bars. macOS removes standard title bars when Accessibility title-button geometry verifies them; the global menu stays in the client's native menu bar. Capture still comes directly from each window. Only the window's content region reaches the encoder; no desktop stream is cropped into app windows. Input, resize acknowledgements, and drops use the same content bounds. Custom-drawn or unverifiable headers can remain visible to avoid removing app controls. Full desktop mode shows the complete desktop.

**Frames → Full desktop view / Per-window view** switches an active session. Desktop view captures the host's primary display, including its menu bar, taskbar/Dock, and any excluded applications visible on that display. Per-window mode continues to use direct window capture. The switch releases held input and tears down old video streams while preserving pairing, audio settings, and transfers; host applications keep running. Closing a desktop-view client window disconnects the session. The Windows notification-area Frames icon keeps view controls available even when no application windows are open.

Select **Open full desktop view** in the launcher, or start directly with `Frames.exe connect IP:47990 --view desktop`. Use matching builds on both devices: this update uses protocol **2**. Display selection and restoring client placements across mode switches remain pending.

Choose **Toggle physical keys (terminal)** for applications that need raw key positions and literal Control shortcuts. This profile bypasses Control/Command swapping and local Unicode composition; the host keyboard layout interprets the keys. The default profile composes text locally for Unicode/IME input. Preferences are remembered per host/application. Switching profiles releases held remote input.

Cross-platform sessions enable **Control / Command swap** by default: Mac Command becomes Windows Control, Mac Control becomes Windows key, Windows Control becomes Mac Command, and Windows key becomes Mac Control when delivered to Frames. Shift and Alt/Option are not swapped. Disable swapping for literal modifiers, or use the physical-key terminal profile. In the default Mac profile, Option dead keys compose text locally; Option with navigation/function keys or pointer gestures is forwarded as Alt. System app switching stays local. Focus loss, app deactivation, profile changes, and disconnect release remote input. Mac menu check marks show the selected profile and mapping.

## Build on macOS

```bash
rustup target add aarch64-apple-darwin
bash native/macos/build.sh
open dist/macos/Frames.app
```

Xcode command-line tools and FFmpeg are required. The build reuses its previous certificate, or selects a single available Developer ID / Apple Development identity. Set `FRAMES_SIGN_IDENTITY` to choose one explicitly; `-` selects ad-hoc signing, whose privacy grants may reset after every rebuild. Signing/notarization must be completed before distribution. Development signatures, executable paths, and responsible-process identity affect macOS permissions.

Open the installed app in the logged-in desktop session and allow **Local Network**, **Screen Recording**, and **Accessibility** in System Settings → Privacy & Security. On macOS 27, Accessibility is named **Device Control and Data Access**. Local Network is needed for outbound LAN connections on newer macOS versions. A connection timeout or `No route to host` can indicate this permission is missing; check the address and routing too. Apple documents the network permission model in [TN3179](https://developer.apple.com/documentation/technotes/tn3179-understanding-local-network-privacy).

`Frames.app/Contents/MacOS/frames permissions` reports capture/input permissions; add `--request` to show the normal OS prompts. The login Keychain may be unavailable to a process launched directly over SSH, so run host/client through the app's GUI session. Permissions are not granted by the test scripts.

`FRAMES_MAC_HELPER` can select a development helper. `FRAMES_FFMPEG` can select an FFmpeg executable when it is not on the process's PATH. GUI builds also discover Homebrew's regular and keg-only `ffmpeg-full` binaries. The launcher offers a per-user sharing login item. **Prelogin/FileVault access is not validated or enabled by this login item.**

## Phones and developer devices

This build adds an optional **Android ADB server bridge**, disabled on each new session. Plug the Android phone into the **client** computer, start its existing ADB server (`adb start-server`), and approve that computer on the phone. In Frames choose **Devices → Android ADB bridge**. On the **host**, run:

```sh
adb -H 127.0.0.1 -P 15037 devices
adb -H 127.0.0.1 -P 15037 -s PHONE_SERIAL shell
```

The host's loopback port **15037** forwards to the client's loopback ADB server on **5037** over the pinned QUIC session. It shares every device and command exposed by that ADB server. Use the same Android platform-tools version on both computers; IDEs may need a custom ADB server configuration and restart. Frames does not start/replace an ADB server, take over port 5037, or expose a LAN listener. Disable the menu item or disconnect to close the listener and active streams. The privileged Windows login agent disallows this feature.

This is development-tool forwarding, **not generic USB passthrough**. Fastboot, DFU, iOS/usbmux, USB storage, license dongles, cameras, and isochronous USB devices are not implemented. Generic support needs device claiming/export on the physical-device computer and a virtual USB controller/device stack on the consuming computer, with device-specific validation. Windows provides a [UDE driver architecture](https://learn.microsoft.com/en-us/windows-hardware/drivers/usbcon/usb-emulated-device--ude--architecture); macOS controller simulation requires an [Apple-managed entitlement](https://developer.apple.com/forums/thread/802495). No driver installation or entitlement request is performed by this build. ADB's custom server arguments are documented in the [AOSP manual](https://android.googlesource.com/platform/packages/modules/adb/+/refs/heads/main/docs/user/adb.1.md).

## Files and clipboard

Text and PNG clipboard synchronization is enabled by default; use `--no-clipboard` to turn it off. Images up to 2 MiB compressed are supported. Standard Windows DIB images are converted to PNG. Recognized concealed macOS clipboard images are excluded.

Drag regular files from the local desktop into a remote proxy to upload them and request a native copy-only drop into the host app. App-specific drop behavior requires validation; a rejected drop leaves the verified file in the receive folder. **Fetch remote file** downloads a path inside the host user's home folder. Received files appear in a native shelf and can be dragged into local apps; macOS uses file promises. Arbitrary dragging directly out of pixels in a captured Finder/Explorer window is not implemented. Folder trees/custom drag formats are not supported yet.

Standalone transfers:

```powershell
Frames.exe send 192.168.1.10:47990 .\report.pdf
Frames.exe get 192.168.1.10:47990 Downloads/report.pdf
```

The default receive directory is `~/Downloads/Frames`. Host `--receive-dir PATH` changes it. Interrupted uploads preserve partial data and resume when the same unchanged file is offered again. Checksums are verified before publishing completed files. Existing files are never overwritten.

## Optional Windows service

First pair a device with `host --pair --unattended`, then stop the ordinary host. In an Administrator terminal:

```powershell
Frames.exe service install
Frames.exe service uninstall
```

The installer creates protected Program Files/ProgramData directories, copies the executable and your local FFmpeg, and migrates identity/trust to a machine-scoped DPAPI store. It refuses existing installation directories. The broker supervises one interactive-console agent. Ordinary windows use WGC; privileged login/elevation views use Desktop Duplication. Normal application surfaces are removed during secure-desktop transitions. Installation and secure-desktop behavior are **not locally runtime-verified**.

The privileged login agent intentionally disables file operations; use a normal user-session host for transfers. This keeps file access outside the SYSTEM agent. Uninstallation retains binaries and trust records. Service trust is separate from per-user trust; use `FRAMES_CONFIG_DIR=C:\ProgramData\Frames` in an Administrator terminal when inspecting or revoking service peers. macOS prelogin service integration remains a release gate.

## Architecture

- `frames-core`: protocol, ordered window graph, input state, codec probing/process adapters, QUIC, certificate trust, protected identity storage, media fragmentation/recovery, transfer integrity, C ABI.
- `frames-native`: direct WGC capture, Win32 proxies and D3D11 presentation, native clipboard/input/drag APIs, WASAPI loopback/CoreAudio playback, macOS helper bridge.
- `frames`: host/client orchestration, separate low-priority QUIC file streams, discovery, Slint launcher and session supervision, optional Windows service.
- `native/macos`: ScreenCaptureKit/Accessibility host, AppKit/Metal client, Dock helpers, file promises, system audio, login item.

Seamless windows are **never desktop crops**. Explicit desktop mode uses WGC monitor capture or ScreenCaptureKit display capture. The privileged Windows secure-screen path separately uses Desktop Duplication. Host and client positions are independent. Hiding a client unsubscribes its video stream, drops its capture/encoder, and preserves metadata; audio remains independently enabled. Capture and codec I/O currently use CPU readback/FFmpeg pipes. GPU presentation is implemented; end-to-end zero-copy is not.

## Verification

```powershell
cargo test --workspace
cargo test -p frames-core --test media_roundtrip -- --ignored --nocapture
cargo test -p frames-native -- --ignored --nocapture
python scripts/smoke_transfer.py target/debug/frames.exe
```

The native tests open short-lived fixture windows on an interactive Windows desktop. They verify independent WGC capture, native menus, and D3D11 presentation. The Python command tests pairing/transfers using generated files. GNU toolchains require `--target x86_64-pc-windows-gnu` and GCC on PATH. CI builds both platforms.

For the real Windows/Mac LAN transfer check, install matching builds and run on Windows:

```powershell
python scripts/cross_transfer_smoke.py dist/windows/Frames.exe --mac-address MAC_IP --windows-address WINDOWS_IP
```

It uses SSH host `macmini` by default, the Mac app at `~/Applications/Frames.app`, fresh protected test identities, generated files, and dedicated ports 47991/47992. It compares both pairing codes before approving and records checksums/results under `dist/cross-platform`. It leaves generated test files for inspection.

For live capture/control, run `bash native/macos/build_fixture.sh` on the Mac and open `dist/QAWindowFixture.app`. Build the production-session probe on Windows with `cargo build -p frames --example control_probe`, then add `--control-probe PATH_TO_CONTROL_PROBE` to the LAN command above. The probe only controls the generated **Frames QA source** window. It checks occluded capture, restore timing, native menus, resizing, Unicode text, desktop/window switching, and close reconciliation.

For the native Mac client, build `cargo build -p frames-native --example window_fixture` on Windows and pass `--native-ui --window-fixture PATH_TO_WINDOW_FIXTURE`. Add `--hold-ui 600` for direct interaction on the Mac. The optional `--desktop-bridge` shows the Mac desktop back on Windows; using that nested view to control the Windows fixture can feed injected input back into the same desktop, so use direct Mac input for this check.

On the Mac, build `cargo build -p frames-native --example menu_preview --target aarch64-apple-darwin`, then run `python3 scripts/mac_native_smoke.py` to check the Rust/Swift bridge, native menu installation, AppKit window, and Metal GPU completion with generated pixels. `FRAMES_NATIVE_DIAGNOSTICS=1` emits native client metadata to stderr without recording titles, pixels, clipboard contents, or text input. These checks do not measure display latency.

RustDesk and TwinDesk were studied as behavior/architecture references. Their implementations are not vendored. FFmpeg is an external runtime dependency and is not included in build artifacts. Review dependency and codec distribution obligations before packaging a public release.
