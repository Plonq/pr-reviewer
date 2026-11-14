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
  <key>CFBundleVersion</key><string>${GITHUB_RUN_NUMBER:-1}.${GITHUB_RUN_ATTEMPT:-1}</string>
  <key>LSMinimumSystemVersion</key><string>15.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF
# Only updater-enabled binaries need the framework. Plain local bundles keep
# working with no downloaded SDK, signing key, or Apple Developer account.
if otool -L "$APP/Contents/MacOS/gander" | grep -q 'Sparkle.framework'; then
  : "${SPARKLE_PUBLIC_KEY:?Set SPARKLE_PUBLIC_KEY to the Sparkle public key}"
  mkdir -p "$APP/Contents/Frameworks"
  ditto "${SPARKLE_DIR:-target/sparkle}/Sparkle.framework" "$APP/Contents/Frameworks/Sparkle.framework"
  cp "${SPARKLE_DIR:-target/sparkle}/LICENSE" "$APP/Contents/Resources/Sparkle-LICENSE"
  python3 - "$APP/Contents/Info.plist" <<'PY'
import base64, os, plistlib, sys
key = os.environ['SPARKLE_PUBLIC_KEY'].strip()
assert len(base64.b64decode(key, validate=True)) == 32, 'Invalid Sparkle public key'
with open(sys.argv[1], 'rb') as f:
    info = plistlib.load(f)
info.update({
    'SUPublicEDKey': key,
    'SUFeedURL': f"https://github.com/{os.environ.get('GITHUB_REPOSITORY', 'Plonq/gander')}/releases/download/latest/appcast.xml",
    'SUEnableAutomaticChecks': True,
    'SUAutomaticallyUpdate': True,
    'SUScheduledCheckInterval': 3600,
    'SUEnableSystemProfiling': False,
    'SUVerifyUpdateBeforeExtraction': True,
    'SURequireSignedFeed': True,
})
with open(sys.argv[1], 'wb') as f:
    plistlib.dump(info, f)
PY
fi
plutil -lint "$APP/Contents/Info.plist"
ln -sfn /Applications dist/Applications
