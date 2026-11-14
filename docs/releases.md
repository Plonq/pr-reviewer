# macOS releases

Pushes to `main` and pull requests check Rust formatting, run tests, and check the
updater-enabled build. New commits to a PR cancel its earlier CI run; checks on
`main` and release tags run independently. Publishing a DMG and automatic update
is an explicit release, triggered by a version tag such as `v0.2.0`.

## Create a release

After the changes you want to ship have merged to `main`, run:

```sh
./scripts/release.sh
```

The script suggests the next patch version; press Enter to accept it or type a
different version, such as `0.2.0` for a feature release. For the first release it
suggests the version in `crates/app/Cargo.toml`. You can also specify the version
in the same command: `./scripts/release.sh 0.2.0`.

That is the whole release step. The script fetches the remote `main` commit and
pushes a version tag pointing to it. Your current branch, local changes and local
tags are left alone. It uses the `origin` push repository and your existing Git
authentication, plus the GitHub CLI login to check versions. It refuses existing
tags and versions older than the highest release tag, including tags still building.

The **Release DMG** workflow tests that tagged commit, builds the Apple Silicon
app, signs it, and creates a GitHub release with `Gander.dmg`, its SHA-256 checksum,
and notes generated from merged PRs since the previous release (the last rolling
build for the first named release). Edit the release description on GitHub to
curate those notes. The tag supplies the app version automatically; there is no
separate Cargo version bump or commit to make. Tags use `vMAJOR.MINOR.PATCH`;
beta/prerelease suffixes are not supported by this workflow.

CI then updates the existing [`latest` downloads](https://github.com/Plonq/gander/releases/tag/latest)
and Sparkle feed. The download and update URLs stay the same for existing installs.
The versioned release and latest downloads appear only after CI succeeds; pushing
the tag alone does not mean the DMG is ready. Follow the Actions link printed by
the script. If a build fails, use **Actions → Release DMG → failed run → Re-run
failed jobs**. Re-running all jobs also works. Reuse that run rather than moving
the tag; once published, subsequent code changes need a new version.

Keep this workflow in `.github/workflows/release.yml`: its existing run-number
sequence is also the updater's build-number sequence. Release runs queue instead
of cancelling a publication already in progress. Re-running an older release
does not replace the latest downloads/feed when a newer release is already published.

No Apple Developer account or certificates are needed. Automatic updates use a
free Sparkle signing key (a GitHub Actions secret), unrelated to Apple signing.
The app has only a free ad-hoc signature for Apple Silicon compatibility, not a
Developer ID signature. The DMG is unsigned and the app is not notarised.
On first launch, macOS may block it: attempt to open the installed app, then choose
**System Settings → Privacy & Security → Open Anyway**.

## Automatic updates

Release bundles embed Sparkle 2.10.1. Gander checks quietly on every launch,
even if the previous check was recent, then Sparkle checks roughly hourly while
the app stays running. Updates download and verify in the background. A quiet
**Restart to update** button appears in the title bar when an update is staged.
There are no permission-to-check or update-available dialogs, and no profiling.
Errors appear as a small retry button with details in its tooltip.

**Gander → Check for Updates…** triggers a manual check without a dialog.
The header briefly shows checking/preparation status, then **Up to date** for
five seconds when no update is available, or **Restart to update** when ready.
A manual check never installs/restarts an update by itself, and repeated checks
join an existing background check/download instead of starting a second one.
The error button retries via the same manual-check path. Updater-free local
builds disable the menu item.

Clicking the button flushes the current session, then lets Sparkle install and
relaunch. In-flight writes/uploads and unresolved/unverified comment mutations
disable the button. Session persistence is normal app behaviour, independent
of updates: every quit saves the open reviews, active pane, file/source-line
position, view mode, local review comments, unsent comment editors and review
summary drafts. Every normal launch restores them. Review summaries reopen as
drafts, not as a modal; nothing is automatically submitted. Fresh data or a force
push can change the exact scroll position.

The snapshot is stored as the JSON setting `session_v1` in
`~/Library/Application Support/Gander/store.db`. Restoring it does not consume
it. Normal sessions checkpoint changed state every five seconds for crash
recovery, then flush again on quit. Existing `update_restart_v1` snapshots are
read as a fallback until the first general-session save.

Hold **Shift** while launching until the window appears to skip restoration
once and start at the queue. Explicit CLI PR/path arguments also skip restoration
and open those targets instead. These fresh sessions leave the prior snapshot
untouched (including during checkpoint intervals) until quit, when their state
becomes the new saved session. There is no persistent opt-out preference.

Sparkle also installs a staged background update on a normal quit. The next
launch restores the saved session just like any other launch. Installation permissions
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
(
  set -eu
  keydir=$(mktemp -d)
  trap 'rm -f "$keydir/private-key"; rmdir "$keydir"' EXIT
  keyfile="$keydir/private-key"
  umask 077
  target/sparkle/bin/generate_keys --account gander -x "$keyfile"
  test -s "$keyfile"
  gh secret set SPARKLE_PRIVATE_KEY --repo Plonq/gander < "$keyfile"
)
```

Sparkle requires the export filename to be absent; do not use an existing empty
file from `mktemp`. A secret can exist in GitHub with an empty value if an
empty file was uploaded. Re-run the export/upload above to repair that case,
then re-run the failed workflow.

Back up this key securely. With ad-hoc signatures and verification-before-
extraction enabled, losing it requires another manual installation to change
the embedded public key. Keep the same key across builds.

The workflow fails early if either setting is missing. It signs both the DMG
and feed, checks that the keypair matches the bundled public key, publishes the
versioned release and an immutable `build-<run>-<attempt>` updater archive, and
only then replaces `appcast.xml` on `latest`. These build prereleases are internal
download archives, created only when you explicitly release. Download URLs in the
feed never point at the overwritten DMG on `latest` or on a retried versioned release.
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
python3 scripts/test-updater.py --manual
python3 scripts/test-updater.py --join
python3 scripts/test-updater.py --manual --no-update
python3 scripts/test-updater.py --manual --invalid-feed
```

This uses a disposable native host in `target/`, an ephemeral Ed25519 key and
local HTTP feed. It exercises the production bridge, signed feed and archive,
tamper rejection, background download, ad-hoc-signed replacement, and relaunch.
Launch checks are tested with a recent last-check date; manual checks are tested
with automatic checking disabled, including no-update and invalid-feed results.
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
