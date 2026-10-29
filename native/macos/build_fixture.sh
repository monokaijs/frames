#!/bin/bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"
APP=dist/QAWindowFixture.app
mkdir -p "$APP/Contents/MacOS"
swiftc -target arm64-apple-macos14.2 native/macos/Tests/WindowFixture.swift \
  -framework AppKit -o "$APP/Contents/MacOS/QAWindowFixture"
cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>dev.frames.qa.fixture</string>
<key>CFBundleName</key><string>Frames QA Fixture</string>
<key>CFBundleExecutable</key><string>QAWindowFixture</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>LSMinimumSystemVersion</key><string>14.2</string>
</dict></plist>
PLIST
codesign --force --sign - "$APP"
echo "Built $APP. Opens a red source under a blue cover; closes after five minutes."
