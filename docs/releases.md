# macOS releases

Every push to `main` builds the Apple Silicon app and publishes `Gander.dmg` on
the rolling [`latest` release](https://github.com/Plonq/pr-reviewer/releases/tag/latest).
The download URL stays the same. A SHA-256 checksum is published alongside it.
The **Release DMG** workflow also has a **Run workflow** button for retries on `main`.

No Apple Developer account, certificates, or repository secrets are needed.
The app has only a free ad-hoc signature for Apple Silicon compatibility, not a
Developer ID signature. The DMG is unsigned and the app is not notarised.
On first launch, macOS may block it: attempt to open the installed app, then choose
**System Settings → Privacy & Security → Open Anyway**.

## Icons and local bundles

`assets/Gander.png` is the 1024×1024 original. `assets/Gander.iconset` includes all
ten standard macOS 1×/2× representations (16–1024 pixels); `assets/Gander.icns`
is the compiled Finder/Dock/App Switcher icon embedded in the bundle.
All are committed, so release builds don't need an image tool or generator.

To change the artwork, edit the vector drawing in `scripts/generate-icons.swift`
and regenerate on macOS with the Xcode command-line tools installed:

```sh
swift scripts/generate-icons.swift
cargo build --release --locked
bash scripts/bundle-macos.sh
codesign --force --sign - dist/Gander.app
open dist/Gander.app
```

To verify a downloaded DMG's checksum, download `Gander.dmg.sha256` alongside it
and run `shasum -a 256 -c Gander.dmg.sha256`.
