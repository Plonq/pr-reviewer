# GPUI Kit migration validation

## Baseline

- Rust 1.98.1, Apple Silicon; `cargo test --workspace --locked`: **427 passed,
  6 ignored**, no failures. Ignored checks require authenticated `gh`/network.
- After palette removal: **420 passed, 6 ignored**, no failures. Seven tests
  removed with their deleted workflows (source picker, repo slug/list/filter,
  manual base list). Shared automatic-base and explicit-base domain tests remain.
- Baseline warning: `LocalStore::mark_thread_seen` is unused.
- No baseline native performance or IME validation was performed. Do not infer
  native input correctness or performance parity from synthetic tests.

## Dependency audit

- Kit/Base/Component/Assets 0.8.1; GPUI/platform/macOS/HTTP family 0.3.10.
- Tree-sitter and its highlighter must both be 0.26.13: their native `links`
  contract prevents retaining the application's old 0.25 runtime alongside Kit.
- `cargo tree -e features -i image` shows stock raster decoding (PNG/JPEG/GIF,
  WebP, TIFF, BMP, ICO, DDS, EXR, farbfeld, HDR, PNM, QOI, TGA) on image 0.25.10.
  AVIF is **not** enabled by the new GPUI defaults. The old graph contained
  `ravif`/`rav1e` (encoding), but no `dav1d`/`mp4parse` decoder. No working AVIF
  decoding capability is being claimed or lost; no system decoder dependency
  is added just to preserve an unused encoder feature.
- GPUI Fast may appear in the lockfile as upstream optional dependencies; it
  does not appear in `cargo tree --locked --target aarch64-apple-darwin`.
  That graph contains a single GPUI family (0.3.10), with Kit/Base/Component
  0.8.1 and tree-sitter/highlight 0.26.13.

## Migration checks (2026-10-28)

- Host: Apple Silicon, macOS 27.0.1 (26A434), Rust 1.98.1. The declared minimum
  remains macOS 15; this host does not establish runtime compatibility on 15.
- `cargo test --workspace --locked`: **420 passed, 6 ignored**, no failures.
  The ignored checks still require network/authentication; none were treated
  as deterministic passes.
- `cargo build --release --locked --target aarch64-apple-darwin`: **passed**.
- `cargo fmt --all` and `git diff --check`: clean.
- Added adapter coverage for wrapped Editor growth/shrink, width/font changes,
  complete three-to-eight text rows, mention acceptance as editable text,
  clipboard/undo, and Cmd-Enter while a suggestion is available.
- Dialog tests cover initial focus, pending dismissal guards, failed draft and
  verdict retention, retry/success, restoration of the actual prior focus,
  imperative cleanup, and late success after a close/reopen generation change.
- Tree tests cover stable path selection through filtering and file reordering,
  directory collapse, visible-only current-file following, retained diff focus,
  and viewed-checkbox isolation. Existing sidebar drag/clamp/persistence,
  diff geometry/scrolling, session, queue, and HTTP/cache wire tests pass.
- Fixed two migration regressions without weakening assertions: active and
  inactive search matches now use distinct stock warning overlay strengths;
  prose Editor explicitly disables code-editor scroll-past-end space so an
  empty field does not consume the diff's wheel events.

### Native attempt and remaining gate

The source-built release executable was launched against a temporary local Git
fixture with an isolated `HOME` and `GH_CONFIG_DIR`, and empty token environment
variables. It stayed running without log errors; Orca listed its process and a
Gander window. It was then terminated. No real review state or GitHub mutations
were used.

**Native acceptance is incomplete.** `orca computer get-app-state` returned
`permission_denied`: the app had a visible window but no readable accessibility
window after retries. `orca computer permissions` reported both Accessibility
and Screen Recording granted. No permissions or user settings were changed.
Window enumeration is not evidence that assets, fonts, or interaction behavior
render correctly. Native dead-key/Japanese/Chinese IME composition, appearance
changes, scrolling/media responsiveness, and persistence remain unverified.
Synthetic Editor tests do not establish native IME correctness or jitter-free
sizing. No native performance comparison against baseline was completed.

To resume manually, use a disposable Git repository with modified files and run
the built executable with an isolated home (create the home directory first):

```sh
env HOME=/absolute/path/to/disposable-home \
    GH_CONFIG_DIR=/absolute/path/to/disposable-home/gh \
    GH_TOKEN= GITHUB_TOKEN= \
    target/aarch64-apple-darwin/release/gander /absolute/path/to/fixture-repo
```

Complete the checklist below in a usable graphical session before declaring the
migration accepted. Authenticated PR/media checks need a deliberate test account
and fixture; the local-only launch does not cover them.

## Native acceptance checklist (source-built, not DMG)

- [ ] Startup/assets/fonts, queue and direct PR/path opening.
- [ ] System light/dark changes; code zoom independent of UI typography.
- [ ] Editor wrapping/resize growth (3–8 rows), mentions as editable text,
  ordinary Enter vs Cmd-Enter, undo, selection, clipboard.
- [ ] Native dead keys and Japanese/Chinese IME composition.
- [ ] Dialog focus once/open, dismissal/restoration, pending/failure/retry,
  success and stale results after close/reopen.
- [ ] Tree selection vs current-file marker, collapsed directories, filtering,
  viewed-checkbox isolation; no tree keyboard-focus route.
- [ ] Sidebar dragging outside handle, interruption, width persistence.
- [ ] Unified/split scrolling, wrapping, folds, search reveal/cancel, selection,
  multiline comment anchors, minimap, large-file banner and phase-2 upgrade.
- [ ] Conversation/inline Markdown and asynchronously loaded images; cached
  private and extensionless SVG attachments; no overlap or anchor drift.
- [ ] Restart restores review position/preferences and existing cache/state.
- [ ] Compare large/wrapped/comment-heavy responsiveness with baseline.

No mount/install/launch/signing/notarization checks of packaged DMGs are required.
