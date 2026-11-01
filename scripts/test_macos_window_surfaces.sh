#!/bin/bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
output="$(mktemp -d "${TMPDIR:-/tmp}/frames-surface-tests.XXXXXX")"
trap 'rm -rf "$output"' EXIT
sources=()
fixture="${1:-WindowSurfaceTests}"
for source in native/macos/Sources/*.swift; do
  if [[ "$source" != */main.swift ]]; then sources+=("$source"); fi
done
swiftc -warnings-as-errors -swift-version 5 -target arm64-apple-macos14.2 \
  -import-objc-header native/include/frames.h "${sources[@]}" \
  "native/macos/Tests/$fixture.swift" \
  "${FRAMES_CORE_LIBRARY:-target/aarch64-apple-darwin/release/libframes_core.a}" \
  -framework AppKit -framework ScreenCaptureKit -framework Metal -framework QuartzCore \
  -framework VideoToolbox -framework Security -framework CoreGraphics -framework ServiceManagement \
  -framework UniformTypeIdentifiers -framework AudioToolbox -framework CoreImage -framework CoreVideo \
  -framework CoreMedia -framework ImageIO -o "$output/surface-tests"
"$output/surface-tests"
