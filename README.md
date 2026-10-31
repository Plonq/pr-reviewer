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
   minimap, file tree, inline review comments, local repo diffs,
  and the blob disk cache (now under `~/.cache/gander/`),
- dark-only Catppuccin Macchiato colors shared by the UI and syntax highlighting,
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

Apple Silicon, macOS 15 or later — `aarch64-apple-darwin` is the pinned default
target. `rust-toolchain.toml` pins Rust 1.98.1. Use `cargo build --release --locked`
for a reproducible release build and `cargo test --workspace --locked` for the
deterministic suite (authenticated/network checks are explicitly ignored).

The UI stack is GPUI Kit **0.8.1**, using its exactly matched **gpui-pre 0.3.10**
family. This is Longbridge's adapted Zed snapshot, not a newer official Zed GPUI
release. Update the family together, with `Cargo.lock`; do not mix snapshots.
Kit's platform enables the font backend and runtime Metal shader compilation.

## Features

- opens straight into the review queue, painted from a local cache and hot-swapped
  when the network answers (works offline, too)
- queue rows show when each PR was opened and when your review was last requested
  directly (team-only requests or unavailable history show an unknown request age)
- queue ordering is oldest review request first by default; `s` toggles to oldest
  PR creation first. Unknown request times sort last, with creation time breaking
  ties. Filter chips narrow by organization or repository, not author.
- enter (or click) opens a queue row straight into its changes view; `u` comes
  back to the queue with open PRs kept open behind the scenes
- one file at a time: the whole file fills the pane — no `@@` markers, hunk
  boundaries read from the highlighting — revealed at its first change on
  first visit, with each file's scroll position remembered while the review is open;
  `]`/`[` move between files and `n`/`p` between hunks (both wrap); returning
  to a PR resumes its last file and scroll position, even after restarting
- unified + split views
- **All changes / Changed since last review** in the Changes pane: compare the
  commit from your latest submitted GitHub review with the current PR head,
  including reviews submitted on GitHub. No manual commit selection. The scope
  stays selected while the PR is open; new opens default to All changes.
  Rebases/force-pushes that remove the reviewed commit from the head's ancestry
  disable the filter with an explanation. Missing review history or failed
  comparisons also disable it rather than guessing. Upstream/base-branch merges
  are excluded, including upstream-only edits inside files changed by the PR:
  the reviewed tree is brought forward to the upstream revision already merged
  into the current head before comparing. This uses Git 2.43+ in an isolated,
  temporary bare repository, with `gh`'s credential helper and lazy blob fetching;
  no local checkout is required or modified. Neither comparison path relies on
  GitHub's JSON file list (which is capped at 300 files).
  Reviewing the current head shows “No changes since your
  last review.” Existing full-PR left-side threads remain in Conversation, since
  their coordinates refer to a different revision; new incremental comments use
  GitHub's experimental revision-positioning API and may depend on its rollout.
  Left-side comments map back to the actual reviewed blob; upstream-only lines
  without a reviewed position must be commented on the new revision instead.
- `cmd-f` searches the current file's diff (literal, case-sensitive); Enter cycles
  highlighted matches, Shift-Enter goes back, and Esc restores the prior scroll
  position. Folded context is temporarily revealed; large-file safety banners
  still require explicit expansion.
- diff presentation controls: `W` hides whitespace changes, `K` folds unchanged
  regions, and `alt-z` toggles soft wrapping. All three start off for new PRs;
  each PR remembers its own choices. Horizontal scrolling works in both views.
  Fold boundaries expand on click. Files of 20,000+ lines open behind an
  explanatory, click-to-expand safety banner, without whole-file highlighting.
- tree-sitter highlighting (18 languages)
- word-level intra-line diffs
- whole-file phase-2 upgrade: hunks paint instantly, full-content re-diffs swap in
- multi-item sidebar with file tree; drag its right edge to resize (width is remembered)
- file-tree comment badges count comments and replies, including outdated comments and drafts
- per-viewer viewed checkboxes at the right edge of the file tree; click or `V`
  to mark/unmark on GitHub, with optimistic updates and visible sync failures
- quick-open (`cmd-t`) for known PR identifiers, PR URLs, and local repository paths
- PR conversation (title-bar Conversation / Changes button, or `⇧⌘←` / `⇧⌘→`):
  GFM description and chronological comments; reviews
  group their inline threads beneath the summary, with captured code context,
  expandable resolved stubs, and read-only reaction counts; cached offline
- comment from the conversation, or reply inside a review thread (including
  resolved/outdated threads); post immediately or start/add to a review.
  Main-thread drafts append to the pending review summary; replies stay in-thread.
  Posted comments appear immediately as placeholders and update in place when
  the refreshed discussion arrives.
- stock Markdown and images in conversations and diff comment threads,
  including cached private GitHub attachments using the secure HTTP bridge
- local repo diffs
- minimap
- inline GitHub review comments with GFM formatting (reading + posting: hover a line for `+`, or `C` on the selected line; replies post into their thread)
- edit comments in place and delete them from their own cards, in the diff or
  conversation; published comments follow GitHub's per-comment permissions
- review threads show resolved/unresolved status in both views, with
  permission-aware resolve/unresolve controls; resolved threads start collapsed
  (one line in the diff) and expand on click
- multi-line comments: click then shift-click a line, or drag a selection and
  press `C` (ranges stay on one side); comments outside a patch hunk, including
  cross-hunk ranges, use GitHub's experimental positioning API and may depend
  on GitHub's rollout for the repository
- pending reviews: start/add to a review without posting, resume drafts started
  on GitHub, and edit/delete them inline or in the pending-review tray before submitting;
   replies join the pending review too; **Add single comment** is available
   only without a pending review and posts immediately
- submitting reviews (approve / request changes / comment, `cmd-enter` or the Review button)
- dark-only theme with soft blue-grey surfaces and pastel accents

### Stock Markdown differences

Kit's TextView owns Markdown layout and media. GitHub mention/issue links and
emoji preprocessing remain. Reference-style images are omitted; unlinked images
do not open on click; SVG source toggles and custom image loading/error messages
are gone. Inline images can be smaller than the former media cards, and narrow
tables may clip. These are intentional stock-renderer tradeoffs, not custom
rendering features to restore.

## Where its state lives

The app keeps its own memory in one SQLite database (WAL mode) at
`~/Library/Application Support/Gander/store.db`:

- the **queue cache** — the last review queue, so the app opens instantly and
  still works with no network,
- **per-PR bookkeeping** — the head SHA and the review-thread timestamps the
  app last saw, for "what changed since I looked",
- **pending reviews** — a local mirror of server-side drafts, reconciled with
  GitHub on open, refresh, and before each review mutation,
- **review positions** — the last file and scroll anchor for each PR,
- **diff preferences** — each PR's whitespace, context-folding, and wrap choices,
- **settings** — the queue's chips and sort, so the app opens the queue you left.

GitHub data in the store is a cache, never a source of truth. Deleting the
directory forces a re-fetch and resets local settings and review positions.

Conversation images and avatars cache separately in `~/.cache/gander/images/`
for offline re-opens. This includes private attachments; cache files are readable
only by your account. Delete that directory to clear downloaded images.

## Keymap

| Key | Action |
|---|---|
| `cmd-t` / `cmd-w` / `cmd-b` | quick-open input / close item / toggle sidebar |
| `ctrl-tab` | cycle open items |
| `j` / `k` + `enter` | queue: move the selection, open the selected PR |
| `s` | queue: toggle review requested / created ordering (both ascending) |
| `u` | back to the queue (open PRs stay open) |
| `shift-cmd-left` / `shift-cmd-right` | conversation / changes |
| `cmd-+` / `cmd--` / `cmd-0` | diff font size: bigger / smaller / reset |
| `]` / `[` | next / previous file |
| `n` / `p` | next / previous hunk |
| `v` | unified ↔ split view |
| `V` | toggle current file viewed on GitHub |
| `W` / `K` | hide whitespace / collapse unchanged (sticky per PR) |
| `alt-z` | toggle soft wrap |
| `cmd-f` | search current diff; Enter / Shift-Enter cycle, Esc restores position |
| `/` | fuzzy file filter |
| `m` | toggle minimap |
| `c` | toggle inline comments |
| `C` | comment on the selected line (Esc cancels) |
| `r` | refresh active item |
| `cmd-enter` | submit review (approve / request changes / comment) |
| `home` / `end` | top / bottom |
| `cmd-c` | copy selection |
| `cmd-q` | quit |

The queue refreshes on its own: on open, when the window comes back to the
front, every few minutes while the window is frontmost, and on `cmd-r`.
