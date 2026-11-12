#!/bin/sh
# Rebuilds assets/DroidLite.icns (Mac app) and assets/icon-256.png (window icon) from assets/icon.svg.
# Needs rsvg-convert (brew install librsvg) and macOS iconutil.
set -e
cd "$(dirname "$0")/.."
rm -rf target/DroidLite.iconset
mkdir -p target/DroidLite.iconset
for s in 16 32 128 256 512; do
  rsvg-convert -w "$s" -h "$s" assets/icon.svg -o "target/DroidLite.iconset/icon_${s}x${s}.png"
  rsvg-convert -w "$((s * 2))" -h "$((s * 2))" assets/icon.svg -o "target/DroidLite.iconset/icon_${s}x${s}@2x.png"
done
iconutil -c icns target/DroidLite.iconset -o assets/DroidLite.icns
rsvg-convert -w 256 -h 256 assets/icon.svg -o assets/icon-256.png
echo "Icons rebuilt from assets/icon.svg"
