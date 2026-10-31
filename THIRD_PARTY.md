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
