#!/bin/bash
# Build lidup and package it as a menu-bar-only .app bundle (no Dock icon).
set -euo pipefail
cd "$(dirname "$0")"

echo "==> cargo build --release"
cargo build --release

BIN="target/release/lidup"
APP="target/release/lidup.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/lidup"

cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>lidup</string>
    <key>CFBundleDisplayName</key><string>lidup</string>
    <key>CFBundleIdentifier</key><string>com.lidup.app</string>
    <key>CFBundleVersion</key><string>0.1.0</string>
    <key>CFBundleShortVersionString</key><string>0.1.0</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleExecutable</key><string>lidup</string>
    <key>LSMinimumSystemVersion</key><string>12.0</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>LSUIElement</key><true/>
</dict>
</plist>
PLIST

# Simple ad-hoc signature (personal use / no App Store / no Developer ID needed).
# This is enough for the app to run locally and be launched at login.
codesign --force --deep --sign - "$APP" >/dev/null 2>&1 || echo "!! ad-hoc codesign failed (optional)"

# Package as a .zip so it can be published as a release asset / shared directly.
# `ditto -c -k --keepParent` preserves the .app bundle structure on unzip.
ZIP="target/release/lidup.zip"
rm -f "$ZIP"
ditto -c -k --keepParent "$APP" "$ZIP" 2>/dev/null || zip -r "$ZIP" "$APP"

echo
echo "Built: $APP"
echo "Zip:   $ZIP"
echo "Run:   open \"$APP\""
echo "Copy:  cp -R \"$APP\" /Applications/"
