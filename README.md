# Frames

Native remote control for Windows 11 x64 and macOS 14.2+ on Apple silicon. Open individual remote applications in local windows, or switch to a full desktop view.

[Download the latest release](https://github.com/monokaijs/frames/releases/latest). Media codecs are bundled and run inside Frames; no FFmpeg executable is required. Both computers need Frames 0.5 or newer: sessions run on the Frames Next engine, which earlier versions cannot connect to.

On Windows, run **Frames-VERSION-windows-x86_64-setup.exe**. It installs Frames for your account, adds a Start menu shortcut and an Installed Apps entry, and includes an uninstaller. The default installation directory supports automatic updates without administrator access. The ZIP is also available for portable use.

## Connect

Launch Frames and choose **Start sharing** under **Share this device** on the host. On the other computer, pick it under **Devices** (computers sharing on the same network appear there), or enter the **device ID** the host shows under **Connect to a device**; an IP address of a computer sharing on this network also works. Rename this device in Settings. **Remove** stops trusting a device. The Sessions tab shows each connection's progress (connecting, waiting for approval with the pairing code, connected), and a failed connection explains what to check next.

The first time a device connects, the host shows its name and a pairing code: allow it only if the other device shows the same code. **Accept and trust** lets it connect again without asking, **Accept this time** lets it in for this sharing session, and **Decline** refuses it. Devices trusted with Frames 0.4 or earlier are trusted again automatically on their first connection. Open the chevron on Connect to choose the view (application windows or full desktop) and host audio; Frames remembers these per device, along with the image quality.

Connections are end-to-end encrypted. Frames finds a direct path across networks and NATs where it can and otherwise relays through public relay servers (n0's, at about 8-10 Mbps); a relayed connection is marked as such. No ports need opening.

On macOS, open the Frames DMG, drag Frames.app to Applications, and open it in the logged-in desktop session. Allow Local Network, Screen Recording, and Accessibility in System Settings > Privacy & Security. Accessibility is named Device Control and Data Access on newer macOS versions. A locked Mac must be unlocked locally before application control can continue.

## Remote windows

**Application windows** (the default view) opens each of the host's application windows in its own local window, titled like the remote one; windows appear and close as they do on the host. When a remote window is resized, the local one follows in the same shape, and resizing a local window keeps the picture's shape, so no bars appear around it. **Full desktop** shows the whole remote screen. Closing a local window stops showing that remote window; the session ends when all are closed.

**Control+Option+F11** (Ctrl+Alt+F11 on Windows) holds the pointer for games (relative mouse); **Ctrl+Alt+F12** gives it back. Cross-platform sessions swap Control and Command. Text and PNG clipboard sync is on by default.

Not yet on the current engine (they were in Frames 0.4): the host's application menus and the Frames menu in remote windows, closing a host window from its local window, the debug overlay, dropping files into remote windows (`frames next connect ID --send FILE` sends one), device preview images on device cards, gamepad and USB forwarding, and isolated input for One window sharing. See [docs/V2_PARITY.md](docs/V2_PARITY.md).

## Sharing scopes

Use **Start sharing** with **Everything** (default), **Selected windows**, or **One window**. The window modes open a searchable preview grid; select windows and press **Share**. Viewers then see only those windows.

## Command line

```sh
frames host                      # share until stopped; new devices are approved here
frames connect DEVICE_ID         # application windows (--view desktop for the screen)
frames connect DEVICE_ID --audio --view desktop --codec hevc
frames discover                  # computers sharing on this network
frames peers                     # this device's ID and its trusted devices
frames revoke DEVICE_ID
frames doctor
frames update --check
frames update
```

`--no-clipboard` turns clipboard sharing off. On a Windows host, `--encoder nvenc|amf|qsv|mf|software` prefers an encoder. `frames next ...` reaches every Frames Next option (`frames next connect ID --send FILE`, `--get PATH`, `--relay-only`, ...). Run `frames --help` for all options.

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

An optional Windows service installs with `frames service install` from an Administrator terminal. It shares the signed-in desktop with this device's ID and trusted devices (copied when it is installed; new devices cannot be approved there). Uninstall with `frames service uninstall`. The service, secure desktop and Mac prelogin/FileVault access need further validation; see [validation](docs/VALIDATION.md).

[Transport and latency](docs/LATENCY_REVIEW.md) ? [Launcher development](docs/LAUNCHER.md) ? [Validation](docs/VALIDATION.md)

Choose **Settings > Image quality** for saved connection defaults: it caps what the host sends, which rate control adapts to the network below that.

| Preset | Most the viewer receives |
| --- | --- |
| High (default) | 50 Mbps |
| Medium | 12 Mbps |
| Low | 4 Mbps |

A custom **Settings > Bandwidth limit** of **1-200 Mbps** lowers that cap. Saved defaults apply to new connections. CLI: `frames connect DEVICE_ID --quality medium --bandwidth-mbps 20` (`0` means Automatic).

On Windows, **Settings > Video encoder** chooses the encoder used while sharing: Automatic (the first that works: the NVIDIA, AMD or Intel encoder of the GPUs present, then Windows Media Foundation, then software AV1), or one of them. A chosen encoder that does not work on the PC is skipped and the next one in that order is tried, so sharing still starts. It applies the next time sharing starts. NVIDIA encoding needs driver 522 or newer. CLI: `frames host --encoder nvenc|amf|qsv|mf|software`.
