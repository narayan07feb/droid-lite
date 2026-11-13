#!/bin/sh
# Builds "target/release/Droid Lite.app" with the app icon.
# Copy it to /Applications to start it from Launchpad or keep it in the Dock.
set -e
cd "$(dirname "$0")/.."
cargo build --release
APP="target/release/Droid Lite.app"
rm -rf "target/release/Droid Lite.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/droid-lite "$APP/Contents/MacOS/droid-lite"
cp assets/DroidLite.icns "$APP/Contents/Resources/DroidLite.icns"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Droid Lite</string>
  <key>CFBundleDisplayName</key><string>Droid Lite</string>
  <key>CFBundleIdentifier</key><string>dev.droidlite.ide</string>
  <key>CFBundleExecutable</key><string>droid-lite</string>
  <key>CFBundleIconFile</key><string>DroidLite</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
# seal the whole bundle (Info.plist, icon), not only the binary: a half-signed app shows as "damaged"
codesign --force --sign - "$APP"
echo "Built $APP"
