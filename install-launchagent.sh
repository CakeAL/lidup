#!/bin/bash
# Manual fallback to install a LaunchAgent so lidup starts at login.
# The primary way to toggle this is the "Start at Login" item in the tray menu,
# which points the agent at the currently running binary.
# Requires the app to be at /Applications/lidup.app (or edit the path below).
set -euo pipefail

BIN="$(pwd)/target/release/lidup.app/Contents/MacOS/lidup"
APP_PATH="$(pwd)/target/release/lidup.app"
DESTSRC="$HOME/Library/LaunchAgents"

echo "==> Installing app bundle to /Applications"
if [ -d "$APP_PATH" ]; then
    rm -rf /Applications/lidup.app
    cp -R "$APP_PATH" /Applications/lidup.app
else
    echo "!! lidup.app not found. Run ./pack.sh first."; exit 1
fi

echo "==> Writing LaunchAgent plist"
mkdir -p "$DESTSRC"
cat > "$DESTSRC/com.lidup.app.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>com.lidup.app</string>
    <key>ProgramArguments</key>
    <array>
        <string>/Applications/lidup.app/Contents/MacOS/lidup</string>
    </array>
    <key>RunAtLoad</key><true/>
</dict>
</plist>
PLIST

echo "==> Loading LaunchAgent"
launchctl unload "$DESTSRC/com.lidup.app.plist" 2>/dev/null || true
launchctl load "$DESTSRC/com.lidup.app.plist"

echo "Done. lidup will start at login."
echo "Remove with: launchctl unload ~/Library/LaunchAgents/com.lidup.app.plist"
