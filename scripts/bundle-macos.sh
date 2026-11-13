#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/.."

APP=dist/Gander.app
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/aarch64-apple-darwin/release/gander "$APP/Contents/MacOS/gander"
cp assets/Gander.icns "$APP/Contents/Resources/Gander.icns"
# Numeric, monotonically increasing CI build number (not a commit SHA).
VERSION=$(sed -n 's/^version = "\([^"]*\)"/\1/p' crates/app/Cargo.toml | head -1)
cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Gander</string>
  <key>CFBundleDisplayName</key><string>Gander</string>
  <key>CFBundleIdentifier</key><string>org.plonq.gander</string>
  <key>CFBundleExecutable</key><string>gander</string>
  <key>CFBundleIconFile</key><string>Gander.icns</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>${GITHUB_RUN_NUMBER:-1}</string>
  <key>LSMinimumSystemVersion</key><string>15.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF
plutil -lint "$APP/Contents/Info.plist"
ln -sfn /Applications dist/Applications
