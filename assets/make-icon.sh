#!/bin/bash
# Regenerate the committed macOS icon from the editable SVG source.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
TEMP_DIR=$(mktemp -d)
trap 'rm -rf "$TEMP_DIR"' EXIT

mkdir -p "$TEMP_DIR/lidup.iconset"
rsvg-convert --width 1024 --height 1024 \
    --output "$TEMP_DIR/source.png" "$HERE/app-icon.svg"
magick "$TEMP_DIR/source.png" -alpha on "PNG32:$TEMP_DIR/source-rgba.png"

for size in 16 32 128 256 512; do
    sips -z "$size" "$size" "$TEMP_DIR/source-rgba.png" \
        --out "$TEMP_DIR/lidup.iconset/icon_${size}x${size}.png" >/dev/null
    double=$((size * 2))
    sips -z "$double" "$double" "$TEMP_DIR/source-rgba.png" \
        --out "$TEMP_DIR/lidup.iconset/icon_${size}x${size}@2x.png" >/dev/null
done

iconutil -c icns "$TEMP_DIR/lidup.iconset" -o "$HERE/app-icon.icns"
test -s "$HERE/app-icon.icns"
