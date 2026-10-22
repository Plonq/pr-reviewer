# Gander

A fast, native macOS app for reviewing code — GitHub pull requests and local
repository diffs — built in Rust with [gpui](https://www.gpui.rs/).

Gander is an **absorb-and-diverge fork** of
[ellie/lgtm](https://github.com/ellie/lgtm) (MIT). The fork point is preserved
in git history (the `Import ellie/lgtm source tree` commit); from there the
project diverges with no upstream tracking. Everything in this repo is MIT
licensed — see [LICENSE](./LICENSE), which carries the upstream copyright
notice. Fork modifications are © 2026 Huon Imberger, also under MIT.

Changes at the fork point relative to upstream lgtm:

- renamed the app to **Gander** (binary, window title, bundle id `org.plonq.gander`),
- stripped the Claude Code chat integration (`cmd-j` panel) and the LSP client
  (Bifrost / rust-analyzer hover + go-to-definition),
- the diff/review core that lgtm is kept for is intact: whole-file two-phase
  diff rendering, tree-sitter highlighting, word-level intra-line diffs,
  minimap, ⌘K palette, file tree, inline review comments, local repo diffs,
  and the blob disk cache (now under `~/.cache/gander/`),
- the palette follows the system appearance: Catppuccin Mocha in dark mode,
  Latte in light mode,
- builds target Apple Silicon only.

## Setup

Requires the [GitHub CLI](https://cli.github.com/) (`gh auth login` first) for
PR reviews; local repo diffs work without it.

Or build from source:

```sh
cargo run --release
```

Note that you will need Xcode to build it from source because the metal dev
tools are bundled with it (and `xcodebuild -downloadComponent MetalToolchain`
if `cargo` complains about a missing Metal toolchain).

Apple Silicon only — `aarch64-apple-darwin` is the pinned default target.

## Features

- opens straight into the review queue, painted from a local cache and hot-swapped
  when the network answers (works offline, too)
- enter (or click) opens a queue row straight into its changes view; `u` comes
  back to the queue with open PRs kept open behind the scenes
- one file at a time: the whole file fills the pane — no `@@` markers, hunk
  boundaries read from the highlighting — revealed at its first change;
  `]`/`[` move between files and `n`/`p` between hunks (both wrap)
- unified + split views
- tree-sitter highlighting (18 languages)
- word-level intra-line diffs
- whole-file phase-2 upgrade: hunks paint instantly, full-content re-diffs swap in
- multi-item sidebar with file tree
- cmd-k palette with fuzzy PR picker
- local repo diffs
- minimap
- inline GitHub review comments (reading + posting, hover a line for `+`)
- submitting reviews (approve / request changes / comment, `cmd-enter` or the Review button)
- follows the system light/dark appearance

## Where its state lives

The app keeps its own memory in one SQLite database (WAL mode) at
`~/Library/Application Support/Gander/store.db`:

- the **queue cache** — the last review queue, so the app opens instantly and
  still works with no network,
- **per-PR bookkeeping** — the head SHA and the review-thread timestamps the
  app last saw, for "what changed since I looked",
- **settings** — the queue's chips and sort, so the app opens the queue you left.

The store is a cache, never a source of truth: deleting the directory costs a
re-fetch, nothing more.

## Keymap

| Key | Action |
|---|---|
| `cmd-k` | open palette (GitHub PR picker / folder) |
| `cmd-t` / `cmd-w` / `cmd-b` | quick-open input / close item / toggle sidebar |
| `ctrl-tab` | cycle open items |
| `j` / `k` + `enter` | queue: move the selection, open the selected PR |
| `u` | back to the queue (open PRs stay open) |
| `cmd-+` / `cmd--` / `cmd-0` | diff font size: bigger / smaller / reset |
| `]` / `[` | next / previous file |
| `n` / `p` | next / previous hunk |
| `v` | unified ↔ split view |
| `/` | fuzzy file filter |
| `m` | toggle minimap |
| `c` | toggle inline comments |
| `r` | refresh active item |
| `cmd-enter` | submit review (approve / request changes / comment) |
| `home` / `end` | top / bottom |
| `cmd-c` | copy selection |
| `cmd-q` | quit |

The queue refreshes on its own: on open, when the window comes back to the
front, every few minutes while the window is frontmost, and on `cmd-r`.
