#!/bin/bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"
export MACOSX_DEPLOYMENT_TARGET=14.2
cargo build --locked --release --target aarch64-apple-darwin
mkdir -p dist/macos/Frames.app/Contents/{MacOS,Resources,Library/LaunchAgents}
swiftc -O -swift-version 5 -target arm64-apple-macos14.2 \
  -import-objc-header native/include/frames.h \
  native/macos/Sources/*.swift \
  target/aarch64-apple-darwin/release/libframes_core.a \
  -framework AppKit -framework ScreenCaptureKit -framework Metal -framework QuartzCore \
  -framework VideoToolbox -framework Security -framework CoreGraphics -framework ServiceManagement -framework UniformTypeIdentifiers -framework AudioToolbox \
  -o dist/macos/Frames.app/Contents/MacOS/FramesHelper
cp target/aarch64-apple-darwin/release/frames dist/macos/Frames.app/Contents/MacOS/frames
cp native/macos/Info.plist dist/macos/Frames.app/Contents/Info.plist
cp native/macos/app.frames.agent.plist dist/macos/Frames.app/Contents/Library/LaunchAgents/
if [[ -n "${FRAMES_SIGN_IDENTITY:-}" ]]; then
  codesign --force --sign "$FRAMES_SIGN_IDENTITY" --identifier app.frames.helper --options runtime --timestamp=none dist/macos/Frames.app/Contents/MacOS/FramesHelper
  codesign --force --sign "$FRAMES_SIGN_IDENTITY" --options runtime --timestamp=none dist/macos/Frames.app
  codesign --verify --deep --strict dist/macos/Frames.app
  echo 'Built signed dist/macos/Frames.app. FFmpeg must be on PATH or FRAMES_FFMPEG set.'
else
  echo 'Built dist/macos/Frames.app (unsigned). Set FRAMES_SIGN_IDENTITY for signing.'
fi
