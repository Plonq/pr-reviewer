# macOS releases

Every push to `main` builds the Apple Silicon app and publishes `Gander.dmg` on
the rolling [`latest` release](https://github.com/Plonq/pr-reviewer/releases/tag/latest).
The download URL stays the same. A SHA-256 checksum is published alongside it.
The **Release DMG** workflow also has a **Run workflow** button for retries on `main`.

No Apple Developer account or certificates are needed. Automatic updates use a
free Sparkle signing key (a GitHub Actions secret), unrelated to Apple signing.
The app has only a free ad-hoc signature for Apple Silicon compatibility, not a
Developer ID signature. The DMG is unsigned and the app is not notarised.
On first launch, macOS may block it: attempt to open the installed app, then choose
**System Settings → Privacy & Security → Open Anyway**.

## Automatic updates

Release bundles embed Sparkle 2.10.1. While Gander is running, Sparkle checks
roughly hourly and downloads/verifies updates in the background. A quiet
**Restart to update** button appears in the title bar when an update is staged.
There are no permission-to-check or update-available dialogs, and no profiling.
Errors appear as a small retry button with details in its tooltip.

Clicking the button saves the open reviews, active pane, file/source-line
position, view mode, local review comments, unsent comment editors and review
summary drafts, then lets Sparkle install and relaunch. In-flight writes/uploads
and unresolved/unverified comment mutations disable the button. The restart
snapshot is stored in the existing local SQLite database and consumed once;
ordinary launches still open the queue. Review summaries reopen as drafts,
not as a modal. Fresh data or a force push can change the exact scroll position.

Sparkle also installs a staged background update on a normal quit. Gander saves
the same restart snapshot then, for the next launch. Installation permissions
can still require a macOS authorisation prompt. Run the app from Applications
(or another writable folder), not from the mounted DMG. Ad-hoc signing and lack
of notarisation are unchanged; the first manual installation may still need
**Open Anyway**. Existing pre-updater installs need one last manual DMG upgrade.

### One-time signing setup

Do this on your own Mac. **Never commit or paste the private key into chat.**

```sh
bash scripts/fetch-sparkle.sh
target/sparkle/bin/generate_keys --account gander
target/sparkle/bin/generate_keys --account gander -p
```

Add the printed public key as the repository **Actions variable**
`SPARKLE_PUBLIC_KEY`. Export the private seed into a private temporary file,
upload it directly with `gh`, then remove only that temporary file:

```sh
keyfile=$(mktemp)
chmod 600 "$keyfile"
target/sparkle/bin/generate_keys --account gander -x "$keyfile"
gh secret set SPARKLE_PRIVATE_KEY < "$keyfile"
rm "$keyfile"
```

Back up this key securely. With ad-hoc signatures and verification-before-
extraction enabled, losing it requires another manual installation to change
the embedded public key. Keep the same key across builds.

The workflow fails early if either setting is missing. It signs both the DMG
and feed, checks that the keypair matches the bundled public key, publishes an
immutable `build-<run>-<attempt>` release, and only then replaces `appcast.xml`
on `latest`. Download URLs in the feed never point at the overwritten DMG.
`CFBundleVersion` is `<run>.<attempt>` so workflow retries are distinct updates.
Do not delete build releases while clients may still be downloading them.

### Local updater-enabled builds

Plain `cargo run`, tests and local bundles remain updater-free. To include it:

```sh
bash scripts/fetch-sparkle.sh
cargo build --release --locked --features auto-updates
SPARKLE_PUBLIC_KEY="$(target/sparkle/bin/generate_keys --account gander -p)" \
  bash scripts/bundle-macos.sh
codesign --force --sign - dist/Gander.app
```

The upstream framework's symlinks, permissions and nested helper signatures
are preserved. The host stays ad-hoc signed, without hardened runtime/library
validation. Sparkle's private seed is needed only in CI for publishing, never
in the app or DMG.

### Testing the installer locally

```sh
bash scripts/fetch-sparkle.sh
python3 scripts/test-updater.py
python3 scripts/test-updater.py --quit
```

This uses a disposable native host in `target/`, an ephemeral Ed25519 key and
local HTTP feed. It exercises the production bridge, signed feed and archive,
tamper rejection, background download, ad-hoc-signed replacement, and relaunch.
It never accesses your Keychain, Gander database, or installed Gander app.
Requires a logged-in macOS GUI session. Production feeds remain HTTPS-only.
The publishing/test scripts use OpenSSL 3 to check/generate Ed25519 keys (already
available on the CI runner; locally, `brew install openssl@3` if needed).

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
