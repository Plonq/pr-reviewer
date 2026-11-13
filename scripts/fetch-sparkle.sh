#!/bin/bash
# Pinned, checksum-verified upstream binary and signing tools. No Apple account.
set -euo pipefail
cd "$(dirname "$0")/.."
VERSION=2.10.1
SHA256=1a856d2a9edb8e628de88121d7be04cf0e2167f8641650b78fca00b14244cf27
DEST="$PWD/target/sparkle"
if [[ -f "$DEST/.version" && "$(cat "$DEST/.version")" == "$VERSION" ]]; then
  exit 0
fi
mkdir -p "$DEST"
curl --fail --location --retry 3 "https://github.com/sparkle-project/Sparkle/releases/download/$VERSION/Sparkle-$VERSION.tar.xz" -o "$DEST/Sparkle.tar.xz"
echo "$SHA256  $DEST/Sparkle.tar.xz" | shasum -a 256 -c -
tar -xJf "$DEST/Sparkle.tar.xz" -C "$DEST"
echo "$VERSION" > "$DEST/.version"
