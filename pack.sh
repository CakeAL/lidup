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

# Sign the bundle.
#  - If $DEVELOPER_ID is set, sign with that Developer ID identity (Hardened Runtime)
#    and, if notarization creds are present, notarize + staple.
#  - Otherwise fall back to an ad-hoc signature (local dev only).
if [ -n "${DEVELOPER_ID:-}" ]; then
    echo "==> Signing with Developer ID: ${DEVELOPER_ID}"
    codesign --force --deep --options runtime --sign "$DEVELOPER_ID" "$APP" || {
        echo "!! codesign failed with '$DEVELOPER_ID'" >&2; exit 1
    }
    # Notarize if credentials are provided (used by CI).
    if [ -n "${APPLE_ID:-}" ] && [ -n "${APPLE_TEAM_ID:-}" ] && [ -n "${APPLE_APP_PASSWORD:-}" ]; then
        echo "==> Notarizing..."
        ditto -c -k --keepParent "$APP" "$APP.zip"
        xcrun notarytool submit "$APP.zip" --apple-id "$APPLE_ID" \
            --team-id "$APPLE_TEAM_ID" --password "$APPLE_APP_PASSWORD" --wait || {
            echo "!! notarization failed" >&2; exit 1
        }
        xcrun stapler staple "$APP" || true
        rm -f "$APP.zip"
    fi
else
    echo "==> Ad-hoc signing (local dev). Set DEVELOPER_ID for a real signature."
    codesign --force --deep --sign - "$APP" >/dev/null 2>&1 || echo "!! ad-hoc codesign failed (optional)"
fi

echo
echo "Built: $APP"
echo "Run:   open \"$APP\""
echo "Copy:  cp -R \"$APP\" /Applications/"
