# Frames

Native remote control for Windows 11 x64 and macOS 14.2+ on Apple silicon. Open individual remote applications in local windows, or switch to a full desktop view.

[Download the latest release](https://github.com/monokaijs/frames/releases/latest). Media codecs are bundled and run inside Frames; no FFmpeg executable is required. Both computers need matching Frames builds; the current protocol is **12**.

On Windows, run **Frames-VERSION-windows-x86_64-setup.exe**. It installs Frames for your account, adds a Start menu shortcut and an Installed Apps entry, and includes an uninstaller. The default installation directory supports automatic updates without administrator access. The ZIP is also available for portable use.

## Connect

Launch Frames, enable sharing on the host, and enter its address on the client. Nearby discovery works on the local network. Rename this device in Settings. Device cards show names, online/offline status, and screenshots shared privately with trusted devices; recent devices without trust show a blurred image from their last session. Both computers must approve the same pairing code on first connection. Incoming requests offer Decline connection, Accept and trust, or Accept for this session only. You can also trust a previously accepted device from Recent. Unattended reconnection requires the host's explicit approval. Open the chevron on Connect to choose connection mode and host audio; Frames remembers these per device, along with live view and image-quality choices.

Frames connects directly over LAN or VPN on UDP **47990**. Allow that port through the host firewall. There is no relay or NAT traversal service.

On macOS, open the Frames DMG, drag Frames.app to Applications, and open it in the logged-in desktop session. Allow Local Network, Screen Recording, and Accessibility in System Settings > Privacy & Security. Accessibility is named Device Control and Data Access on newer macOS versions. A locked Mac must be unlocked locally before application control can continue.

## Remote windows

The Frames menu controls desktop/window mode, sizing, keyboard mapping, cursor visibility, and file transfers. The host application's exposed menus appear alongside it. Custom application menus remain accessible in the video. **Ctrl+Alt+F12** releases remote input and minimizes the client window.

Per-window capture excludes standard system chrome when its geometry can be verified. Custom headers remain intact. **Show remote window header** changes the host capture while keeping the local title bar on both Mac and Windows clients. Full desktop view includes every visible application, including applications excluded from per-window sharing.

Closing a remote application window requests closing the actual host window. Closing a desktop-view window disconnects. Quitting the launcher stops the sessions it started. On Windows, closing its window asks whether to hide to the tray or quit. Remember your choice, or change it in Settings. A separately configured sharing login item continues independently.

Cross-platform sessions swap Control and Command by default. Shift and Alt/Option keep their meaning. Choose the physical-key profile for terminals or literal key positions. Focus loss, profile changes, and disconnect release held remote input.

Text and PNG clipboard synchronization is enabled by default. Regular files can be dropped into remote windows or fetched from a path within the host user's home folder. Completed transfers are checksum verified and never overwrite existing files. Receive files default to `~/Downloads/Frames`; interrupted uploads can resume. Folder trees and dragging arbitrary files out of captured Finder/Explorer pixels are not supported.

The optional Android ADB bridge forwards the client's existing ADB server over the paired connection. Start `adb start-server` on the client, enable the bridge in Frames, then use `adb -H 127.0.0.1 -P 15037 devices` on the host. It exposes all devices available to that ADB server. Standard USB/Bluetooth gamepads can be forwarded to a Windows host through Devices > Gamepads. Wired USB passthrough is available between Windows computers with optional USB/IP drivers. USB audio/video and Mac receiving are excluded from this release; device compatibility needs physical validation. See [device forwarding and setup](docs/DEVICE_FORWARDING.md).

## Sharing scopes

Use **Start sharing** with **Everything** (default), **Selected windows**, or **One window**. The window modes open a searchable preview grid; select one or several windows and press **Share**. On Windows, One window uses experimental isolated input for eligible x64 applications and games, leaving your mouse and keyboard available. Its viewer scales locally and closing it disconnects. Selected windows retains normal remote control. Scoped sharing disables desktop access, clipboard/files, and device forwarding.

Any technically eligible game can be tried as **Untested**. The initial catalog has no verified games; compatibility requires exact build and adapter/profile matching. See [sharing scopes and isolated input](docs/SHARED_GAME.md), [compatibility records](docs/GAME_COMPATIBILITY.md), and [validation limits](docs/SHARED_GAME_VALIDATION.md).

## Command line

```sh
frames host --pair
frames host --pair --unattended
frames connect 192.168.1.10:47990 --audio
frames connect 192.168.1.10:47990 --view desktop --codec h264
frames send 192.168.1.10:47990 report.pdf
frames get 192.168.1.10:47990 Downloads/report.pdf
frames discover
frames doctor
frames peers
frames revoke CERTIFICATE_FINGERPRINT
frames update --check
frames update
```

Use `--no-clipboard` on host or client to disable clipboard sharing. Automatic codec selection probes working implementations; `--codec` accepts h264, hevc, av1, or vp9. Run `frames --help` for all options.

Official builds check for signed updates in the background every six hours. Use **Settings > Check for updates** to download manually, including during a session. **Restart to update** asks you to confirm disconnecting active sessions and file transfers before restarting Frames. Automatic downloads defer while a remote session is active and never interrupt sessions. Verified packages install after all Frames processes using that installation close. Updates require write access to the installation directory. See [release setup](docs/RELEASES.md), including migration from the previous bundle identifier.

## Build

Rust, CMake, Python, and the platform toolchain are required: Visual Studio C++ Build Tools on Windows, or Xcode command-line tools on Mac. An existing MSYS2 UCRT64 toolchain is also supported locally.

```powershell
.\scriptsuild.ps1
.\dist\windows\Frames.exe
```

```sh
rustup target add aarch64-apple-darwin
bash native/macos/build.sh
open dist/macos/Frames.app
```

Mac builds reuse the previous signing identity or select a unique Developer ID/Apple Development certificate. Set `FRAMES_SIGN_IDENTITY` to choose one explicitly. Ad-hoc signing (`-`) can reset privacy grants after rebuilds. `FRAMES_MAC_HELPER` selects a development helper; Mac builds prepare a minimal LGPL codec runtime with Homebrew build dependencies. Windows builds fetch a checksum-pinned LGPL SDK. Run `python scripts/prepare_media.py` before direct Cargo commands on Windows, or `bash scripts/prepare_media_macos.sh` on Mac.

An optional Windows service installs with `frames service install` from an Administrator terminal after unattended pairing. It uses machine-scoped protected credentials and an interactive-console agent. Uninstall with `frames service uninstall`. Secure-desktop support and Mac prelogin/FileVault access need further validation; see [validation](docs/VALIDATION.md).

[Transport and latency](docs/LATENCY_REVIEW.md) ? [Launcher development](docs/LAUNCHER.md) ? [Validation](docs/VALIDATION.md)

Choose **Settings > Image quality** for saved connection defaults, or change **Image quality** live from the dropdown in a remote window's title bar.

| Preset | Resolution / frame rate | 1080p encoder budget before protection |
| --- | --- | --- |
| High (default) | Native resolution, up to 60 fps; congestion keeps native resolution | 50 Mbps |
| Medium | Native resolution, up to 60 fps; may reduce resolution under congestion | 12 Mbps |
| Low | Starts at 75% resolution and up to 30 fps | 4 Mbps |

Larger source displays scale the budget by pixel count, up to 200/64/24 Mbps respectively. A custom **Settings > Bandwidth limit** of **1-200 Mbps** caps the preset's total connection traffic, shared across video, protection, and audio. Automatic uses the preset budget. Saved defaults apply to new connections; title-bar preset changes apply immediately to that connection. CLI: `frames connect ADDRESS --quality high --bandwidth-mbps 50` (`0` means Automatic). Both peers need the protocol-12 build.

The title-bar dropdown replaces each connection's Windows tray icon and the old eye-slash header button. It retains window options, view, devices, remote cursor, and disconnect controls. Choose **Ignore / hide this window** to hide a local proxy without closing the remote application. Restore individual windows under **Windows**, or choose **Show all windows**. When all windows are hidden on Windows, a small connection window provides restore and menu controls. On Mac, the same controls remain in the Frames menu and existing Dock entries can restore hidden windows. The launcher's single tray icon is unchanged.
