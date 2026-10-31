#!/bin/bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"
source native/macos/sign.sh
# Resolve before replacing the executable so an existing certificate is retained.
FRAMES_SIGN_IDENTITY="$(frames_signing_identity dist/macos/Frames.app)"
export FRAMES_SIGN_IDENTITY
export MACOSX_DEPLOYMENT_TARGET=14.2
# macOS 27 rejects stripped proc-macro dylibs from older Rust toolchains.
export CARGO_PROFILE_RELEASE_BUILD_OVERRIDE_STRIP="${CARGO_PROFILE_RELEASE_BUILD_OVERRIDE_STRIP:-none}"
cargo build --locked --release --target aarch64-apple-darwin
mkdir -p dist/macos/Frames.app/Contents/{MacOS,Resources,Library/LaunchAgents}
swiftc -O -swift-version 5 -target arm64-apple-macos14.2 \
  -import-objc-header native/include/frames.h \
  native/macos/Sources/*.swift \
  target/aarch64-apple-darwin/release/libframes_core.a \
  -framework AppKit -framework ScreenCaptureKit -framework Metal -framework QuartzCore \
  -framework VideoToolbox -framework Security -framework CoreGraphics -framework ServiceManagement -framework UniformTypeIdentifiers -framework AudioToolbox \
  -o dist/macos/Frames.app/Contents/MacOS/FramesHelper
# Retire the previous bundle's executable and login item when rebuilding in place.
# Do this before copying Frames: macOS volumes are usually case-insensitive.
rm -f dist/macos/Frames.app/Contents/MacOS/frames dist/macos/Frames.app/Contents/Library/LaunchAgents/app.frames.agent.plist
cp target/aarch64-apple-darwin/release/frames dist/macos/Frames.app/Contents/MacOS/Frames
cp assets/icons/Frames.icns dist/macos/Frames.app/Contents/Resources/Frames.icns
cp native/macos/Info.plist dist/macos/Frames.app/Contents/Info.plist
# Cargo is the version source of truth, including local builds after a version bump.
version="$(sed -n 's/^version = "\([^"]*\)".*/\1/p' Cargo.toml | head -n 1)"
test -n "$version"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $version" dist/macos/Frames.app/Contents/Info.plist
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $version" dist/macos/Frames.app/Contents/Info.plist
cp native/macos/dev.creaton.frames.agent.plist dist/macos/Frames.app/Contents/Library/LaunchAgents/
frames_sign_app dist/macos/Frames.app
printf 'Built dist/macos/Frames.app. FFmpeg must be on PATH or FRAMES_FFMPEG set.\n'
