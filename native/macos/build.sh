#!/bin/bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"
cargo build --release --target aarch64-apple-darwin
mkdir -p dist/macos/Frames.app/Contents/{MacOS,Resources,Library/LaunchAgents}
swiftc -O -swift-version 5 -target arm64-apple-macos14.2 \
  -import-objc-header native/include/frames.h \
  native/macos/Sources/*.swift \
  target/aarch64-apple-darwin/release/libframes_core.a \
  -framework AppKit -framework ScreenCaptureKit -framework Metal -framework QuartzCore \
  -framework VideoToolbox -framework Security -framework CoreGraphics -framework ServiceManagement -framework UniformTypeIdentifiers -framework AudioToolbox \
  -o dist/macos/Frames.app/Contents/Resources/FramesHelper
cp target/aarch64-apple-darwin/release/frames dist/macos/Frames.app/Contents/MacOS/frames
cp native/macos/Info.plist dist/macos/Frames.app/Contents/Info.plist
cp native/macos/app.frames.agent.plist dist/macos/Frames.app/Contents/Library/LaunchAgents/
echo 'Built dist/macos/Frames.app (unsigned). FFmpeg must be on PATH or FRAMES_FFMPEG set.'
