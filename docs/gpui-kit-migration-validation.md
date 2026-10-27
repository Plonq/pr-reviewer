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
  AVIF is **not** enabled by the new GPUI defaults; this needs an explicit
  capability decision rather than assuming old defaults carry forward.
- GPUI Fast may appear in the lockfile as upstream optional dependencies; it
  must not appear in the active target's compiled dependency graph.

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
