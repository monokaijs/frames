Frames uses FFmpeg shared libraries directly for video codecs, pixel conversion,
and audio resampling. It does not launch or distribute an FFmpeg executable.
Release builds disable GPL and nonfree components. Software video fallbacks use
libvpx (BSD) and libaom (BSD); AV1 decoding uses dav1d (BSD).

FFmpeg is licensed under LGPL 2.1 or later. Its license is distributed with the
application. Source revision and build settings are recorded in the media SDK's
SOURCE.txt. Windows builds use the pinned LGPL shared SDK from BtbN, with the
download URL and SHA256 in scripts/prepare_media.py. macOS builds use the same
FFmpeg revision and the configure command in scripts/prepare_media_macos.sh.

Sources and build recipes:

- https://github.com/FFmpeg/FFmpeg/tree/8dca1586f468bfcbb78ae3ed618af1361d9f140a
- https://github.com/BtbN/FFmpeg-Builds
- https://chromium.googlesource.com/webm/libvpx/
- https://aomedia.googlesource.com/aom/
- https://code.videolan.org/videolan/dav1d

An ABI-compatible replacement runtime can be selected explicitly with
FRAMES_MEDIA_LIBRARY_DIR. Windows normally extracts the unmodified embedded DLLs
to a directory identified by their SHA256 hashes; macOS loads signed dylibs from
Frames.app/Contents/Frameworks. macOS replacement libraries must satisfy the
operating system's code signing requirements.

Opus is linked into Frames through the opus Rust crate and its bundled
opusic-sys dependency. Opus uses the BSD 3-Clause license:
https://opus-codec.org/license/. Source: https://github.com/xiph/opus.
The opus Rust bindings use MIT/Apache-2.0; opusic-sys uses BSD-3-Clause.
Run `Frames licenses` to print the notices and media license texts embedded in
the executable. Mac bundles also contain these in Contents/Resources/MediaLicenses.

The architecture was informed by Sunshine and Moonlight's public implementations;
their source code has not been copied into Frames.

# Patched source dependency

`vendor/quinn-udp` contains quinn-udp 0.5.16 under MIT OR Apache-2.0, with its
license files included. `FRAMES-PATCH.md` describes the macOS IPv4 source-address
fix needed when Ethernet and Wi-Fi are active together.


# Device forwarding

SDL3 3.4.18 is statically bundled through sdl3 0.20.0 (MIT) and sdl3-sys
0.7.2. SDL uses the zlib license; bindings and source checksums are pinned in
Cargo.lock. Source: https://github.com/libsdl-org/SDL/tree/release-3.4.18 and
https://github.com/vhspace/sdl3-rs/tree/0ffa35e5f6d68b3bcc56bb724af0ad5e538af6f4.
The SDL source archive is distributed as the sdl3-src 3.4.18 crate.

Windows gamepad output uses vigem-client 0.1.4 (MIT), a pure Rust client:
https://github.com/CasualX/vigem-client. The optional, upstream-signed ViGEmBus
1.22.0 installer uses BSD-3-Clause. Upstream is retired. Source:
https://github.com/nefarius/ViGEmBus/tree/v1.22.0.

Optional USB setup downloads upstream installers without linking their code into
Frames: usbipd-win 5.3.0 (GPL-3.0-only) and usbip-win2 0.9.8.1 (BSD-2-Clause).
Installers, versions, SHA256 digests and matching source locations are recorded in
assets/device-packages.json and verified before execution. Sources:
https://github.com/dorssel/usbipd-win/tree/v5.3.0 and
https://github.com/vadimgrn/usbip-win2/tree/v.0.9.8.1.
The installers include their own notices. Device license texts are included in
`Frames licenses` and Mac bundle notices; GPL-3.0 is included in licenses/GPL-3.txt.
No NetJoy, Satellite, Moonlight or Sunshine application source is incorporated by
this device implementation.

# Forward error correction

reed-solomon-simd 3.1.0 is pinned in Cargo.lock and uses MIT AND BSD-3-Clause.
Its complete notice is included in licenses/Reed-Solomon-SIMD.txt and in
`Frames licenses`. Source: https://github.com/AndersTrier/reed-solomon-simd/tree/v3.1.0.

# Shared game input

The Windows x64 input adapter is a Frames-maintained headless adaptation of
SplitScreen-Me ProtoInput, pinned to commit
e18e06bb345b86540e7464f7355c649bfdd17f38 (MIT). Selected upstream sources and
archive provenance are retained in vendor/frames-protoinput/SOURCE.json.
The adapter statically builds the fork's user-space EasyHook implementation
(MIT) and udis86 disassembler (BSD); it does not install a kernel driver.

Sources and complete notices:

- https://github.com/SplitScreen-Me/splitscreenme-protoInput/tree/e18e06bb345b86540e7464f7355c649bfdd17f38
- licenses/ProtoInput-MIT.txt
- licenses/EasyHook-MIT.txt
- licenses/udis86-BSD.txt

The adapter and native helper are built from the pinned sources with MSVC and
embedded in the Windows executable. Runtime extraction verifies their SHA-256
before loading. No external input runtime download is required.
