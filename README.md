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
PR reviews; local repo diffs work without it. Finder/Dock launches recover
PATH from your interactive login shell (`$SHELL`, or `/bin/zsh`) and also
search standard Homebrew/MacPorts locations. The inherited PATH takes precedence;
only PATH is imported, and shell startup is limited to two seconds so a broken
startup script cannot prevent the app from opening.

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
- **Past reviews** toggles the queue to your 10 most recently reviewed PRs,
  newest submitted review first, including closed and merged PRs. Toggle it off
  to return to the current queue; new launches always start in the current view.
- enter (or click) opens a new PR in its conversation view; `⇧⌘→` switches to
  Changes and `⇧⌘←` back to Conversation. Reopening an already-open PR keeps
  its selected view; `u` returns to the queue with open PRs kept open behind
  the scenes. The description and discussion appear as soon as they arrive,
  while the patch and file details continue loading in the background; diff
  loading or failure does not block reading the conversation. Local repository
  diffs open in Changes.
- one file at a time: the whole file fills the pane — no `@@` markers, hunk
  boundaries read from the highlighting — revealed at its first change on
  first visit, with each file's scroll position remembered while the review is open;
  `Shift+N`/`Shift+P` (or `]`/`[`) move between files and `n`/`p` between hunks
(both wrap), following
  the sidebar's directory-first, name-sorted order. New reviews start at the top
  file; returning to a PR resumes its last file and scroll position, even after restarting
- file-selection history per open review: `cmd-[` / `cmd-]`, title-bar arrows,
  or mouse back/forward buttons revisit files at their remembered scroll positions.
  History survives refreshes and switching open reviews, but resets on close or
  comparison-scope changes; selecting another file after going back clears forward history
- image diffs show before and after side by side, including SVGs; SVGs have a
  Rendered / Code diff toggle, with unified/split source views. Additions and
  deletions leave the missing side empty. Video and other non-image media
  are links only, with no inline playback in the diff
- unified + split views; `↑`/`↓` move the selected source line, and
  `Shift-↑`/`Shift-↓` extend or shrink a whole-line range, staying on the same
  side in split view. `Esc` clears the range. Double-click a word to select it,
  then drag to extend by whole words (`cmd-c` copies the selection).
  Selecting text outlines literal, case-sensitive matches on both sides of the
  displayed file and marks them in the minimap, without opening Find or unfolding
  hidden context. Clearing the selection clears the marks; whitespace-only and
  whole-line navigation selections do not trigger matching.
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
- `cmd-f` searches the current file; `cmd-shift-f` searches across the entire
  diff (literal, case-sensitive). Selected diff text autofills the query and
  searches immediately. Choose **Current file**, **All files** (full content of
  changed files, including context and removals), or **Changed lines** (additions
  and removals only). Enter / Shift-Enter cycle highlighted results
  across files; Esc restores the original file and scroll position. Folded and
  whitespace-hidden context is temporarily revealed. Large-file safety banners
  still require explicit expansion; search reports excluded large files and
  files where only patch content is available.
- diff presentation controls: `w` hides whitespace changes, `k` folds unchanged
  regions, and `alt-z` toggles soft wrapping. All three start off for new PRs;
  each PR remembers its own choices. Horizontal scrolling works in both views.
  Fold boundaries expand on click. Files of 20,000+ lines open behind an
  explanatory, click-to-expand safety banner, without whole-file highlighting.
- tree-sitter highlighting, including CSS and SCSS
- word-level intra-line diffs
- whole-file phase-2 upgrade: hunks paint instantly, full-content re-diffs swap in
- standalone review queue without a sidebar; reviews have a permanent multi-item
  sidebar with file tree beside Changes / Conversation; drag its right edge to
  resize (width is remembered). Title-bar Review queue / Back to review buttons
  switch screens without losing the active item's pane or position
- `/` focuses the fuzzy file filter; while filtering, `↑` / `↓` move the existing
  tree highlight between visible files, skipping directories. `Enter` opens the
  highlighted file and focuses the diff (keeping the filter); `Esc` clears the
  filter and returns to the diff without opening a file. Each query starts at
  the first matching file in tree order; arrows stop at either end.
- file-tree comment badges count comments and replies anchored in the current diff,
  including drafts but excluding outdated threads (still available in Conversation)
- per-viewer viewed checkboxes at the right edge of the file tree; click or `V`
  to mark/unmark on GitHub, with optimistic updates and visible sync failures
- quick-open modal (`cmd-o`) for known PR identifiers, PR URLs, and local repository paths
- PR conversation (title-bar Conversation / Changes button, or `⇧⌘←` / `⇧⌘→`):
  GFM description and chronological comments; reviews
  group their inline threads beneath the summary, with captured code context,
  expandable resolved stubs, and emoji reactions; cached offline
- see, add, or remove GitHub's eight standard emoji reactions on published
  comments and reviews, in both the conversation and diff views
- comment from the conversation, or reply inside a review thread (including
  resolved/outdated threads); post immediately or start/add to a review.
  Main-thread drafts append to the pending review summary; replies stay in-thread.
  Posted comments appear immediately as placeholders and update in place when
  the refreshed discussion arrives.
- paste an image or copied media file, or drop images/videos onto any PR comment
  editor (including replies, edits, and review summaries) to upload to GitHub and
  insert its attachment Markdown at the cursor. Uploads show progress and errors;
  submission waits for completion. Uses your `gh` login and requires repository
  write access. Supports PNG/JPEG/GIF/WebP/SVG up to 10 MB and MP4/MOV/WebM up to
  100 MB (GitHub may enforce a lower video limit for your plan). Local-repository
  comments do not upload files. See [attachment testing](docs/comment-media-testing.md).
- stock Markdown and images in conversations and diff comment threads,
  including cached private GitHub attachments using the secure HTTP bridge;
  click Markdown images or GitHub's pasted/dropped images to inspect them in a
  full-window image viewer
- embedded video cards with native macOS playback, audio, seeking and a
  full-window viewer shared with images; private videos use protected local files
- automatic Jira links for `PYAPI-123`, `BOT-123`, `CS-123`, and `HELP-123`
  in PR descriptions and comments (excluding code and existing links)
- local repo diffs
- minimap
- inline GitHub review comments with GFM formatting (reading + posting: hover a line for `+`, or `C` on the selected line; replies post into their thread)
- comments use the proportional UI font, keeping inline code and code blocks
  monospace; replies are indented beneath their thread root in Conversation and
  Changes, including pending replies and both unified and split diffs
- edit comments in place and delete them from their own cards, in the diff or
  conversation; published comments follow GitHub's per-comment permissions
- review threads show resolved/unresolved status in both views, with
  permission-aware resolve/unresolve controls; resolved threads start collapsed
  (one line in the diff) and expand on click
- multi-line comments: select a range with `Shift-↑`/`Shift-↓`, click then
  Shift-click a line, or drag a text selection; `C` starts a comment for the
  selected range (ranges stay on one side). Selecting does not open the composer.
  Comments outside a patch hunk, including
  cross-hunk ranges, use GitHub's experimental positioning API and may depend
  on GitHub's rollout for the repository
- pending reviews: start/add to a review without posting, resume drafts started
  on GitHub, and edit/delete them inline or in the pending-review tray before submitting.
  The tray starts collapsed, keeping the comment count, Refresh drafts, and
  Finish review controls visible; Show comments expands the draft list.
  Replies join the pending review too; **Add single comment** is available
  only without a pending review and posts immediately. In the comment composer,
  `cmd-enter` submits immediately without a review, or adds a draft to an existing
  review; `shift-cmd-enter` adds a draft and starts a review if needed
- submitting reviews (approve / request changes / comment, `cmd-enter` or the Review button)
- dark-only theme with soft blue-grey surfaces and pastel accents

### Image viewer

Click an image written as Markdown (`![alt](https://…)`) or GitHub's generated
`<img width="…" height="…" alt="…" src="https://github.com/…">` in a PR description,
conversation comment or inline review thread to inspect it without leaving
Gander. GitHub commonly uses the HTML form when you paste or drop a screenshot.
This includes extensionless GitHub attachment URLs, SVGs and animated images, in
both unified and split Changes views. The dark overlay fills the area below the
app header, keeping macOS window controls clear, without entering fullscreen or
changing Spaces. While either media viewer is open, app header content is replaced
by a blank, non-interactive strip; it returns when the viewer closes.

- Images start fitted to the window (small images are not enlarged).
- Zoom with **+ / −**, the mouse wheel/trackpad, or the toolbar buttons; drag the
  image to pan. Press **0** or **F**, or click **Fit**, to reset.
- Close with **Esc**, **Close**, or a click on the backdrop. Review shortcuts and
  scrolling are isolated while the viewer is open; closing restores focus and
  leaves the review's scroll position, navigation and diff selection intact.
- For linked images, the image click opens the viewer, not the browser.
  **Follow link** opens the original link; **Open image in browser** is an
  explicit fallback. Failed image loads offer **Retry**.

GitHub image tags (including its user-image hosts) use the viewer and retain
their specified inline dimensions, including tags followed by a plain caption.
General HTML containers, non-GitHub HTML images and reference-style images retain
stock behaviour; reference-style images remain omitted.

### Video playback

Direct MP4, M4V, MOV and WebM links, GitHub attachment links, and standalone HTML
`<video src="https://…">` embeds (including `<source src="…">`) become inline
video cards in descriptions, conversation comments and unified/split review
threads. GitHub attachment links without an extension are checked with a
one-byte ranged GET request, inspecting headers without reading or caching the
response body; only a `video/*` response becomes a video card. This works with
GitHub's GET-signed attachment redirects, which can reject HEAD requests.
Ordinary links, images and code examples are not downloaded as videos. Rich HTML
containers, iframes, YouTube and Vimeo players are not supported.

- Videos preload a paused first-frame preview and initial media ranges. There
  is **no autoplay**, even when an HTML embed requests it. Click the picture or
  play icon to start; playback can begin before the whole file is downloaded.
- Play immediately starts an eager full download, independent of the decoder's
  buffering. It continues while paused so playback no longer depends on expiring
  attachment authorization once cached. Startup still depends on network speed
  and the container's metadata; servers without range support may need more data.
- Inline pictures use a larger, full-width 420px-high area. Icon controls overlay
  the picture, appearing on hover while playing and remaining visible while
  paused. They provide play/pause, seek/download progress, mute, volume, browser
  fallback and expand, with tooltips. Both seek and volume support click-and-drag,
  including outside the slider bounds. Clicking the picture toggles play/pause.
- Expand moves the same session into the window-filling media viewer; **Esc**,
  the collapse icon, or the backdrop returns it inline without resetting playback.
  Expanded controls hide after two seconds without mouse movement or immediately
  on window exit; moving the pointer reveals them. Active slider drags keep them
  visible. In the viewer, **Space** toggles playback, **← / →** seek five seconds and **M**
  toggles mute.
- Only one video plays at a time. Leaving its review pane pauses it; returning
  does not resume automatically. Closing a review releases its previews/caches.
- Download progress, **Cancel**, **Retry**, unsupported-format and playback-error
  states stay in the card. **Browser** is an explicit fallback, never automatic.

Playback uses macOS AVFoundation and system codecs, not a webview or bundled
codec library. Common H.264/AAC MP4 and MOV files are the primary targets. WebM
links are recognized but decoding depends on macOS support for their container
and codecs; **WebM playback is not guaranteed**. An unsupported file shows an
error and offers the browser fallback.

Videos use a private temporary range cache (0700 directory, 0600 sparse file),
with a **128 MiB per-video limit**. At most four previews/caches are retained;
additional videos prepare on hover or Play, evicting an inactive cache. Automatic
preloading is bounded to 8 MiB per video; short clips may fit entirely. Offscreen
row measurement never starts preloading. Only complete cached ranges are exposed
to AVFoundation through an unguessable loopback-only URL; missing ranges are
fetched on demand. Play also starts a full download immediately, even if the
player already has enough buffered data. A canceled transfer can take its
current network timeout to release its file. Files are removed on eviction,
review closure or normal app exit, and are not a persistent/offline library.
Abnormal termination can leave temporary files for the OS to clean up. GitHub
tokens are used only for trusted HTTPS attachment endpoints, never sent to CDN
redirects or AVFoundation, and never stored with the video. Signed redirect URLs
also stay out of AVFoundation and the on-disk cache. The
image cache and its existing download limits are unchanged. See the
[native playback testing guide](docs/video-playback-testing.md) for fixture and
manual verification instructions.

### Stock Markdown differences

Kit's TextView still owns prose, tables, code and selection; small media plugins
add image activation and video cards without replacing the stock renderer.
GitHub mention/issue links and emoji preprocessing remain. SVG source toggles in
Markdown are not provided. Mixed-paragraph images can be smaller than the former media cards,
and narrow tables may clip.

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
| `cmd-o` / `cmd-w` | quick-open modal / close item (from the queue, return to the open review) |
| `ctrl-tab` | cycle open items |
| `j` / `k` + `enter` | queue: move the selection, open the selected PR |
| `s` | queue: toggle review requested / created ordering (both ascending) |
| `u` | back to the queue (open PRs stay open) |
| `shift-cmd-left` / `shift-cmd-right` | conversation / changes |
| `cmd-+` / `cmd--` / `cmd-0` | diff font size: bigger / smaller / reset |
| `]` / `[` | next / previous file |
| `cmd-[` / `cmd-]` | back / forward through file selections (Changes pane) |
| `n` / `p` | next / previous hunk |
| `v` | unified ↔ split view |
| `V` | toggle current file viewed on GitHub |
| `w` / `k` | hide whitespace / collapse unchanged (sticky per PR) |
| `alt-z` | toggle soft wrap |
| `cmd-f` | search current diff; Enter / Shift-Enter cycle, Esc restores position |
| `/` | fuzzy file filter |
| `m` | toggle minimap |
| `c` | toggle inline comments |
| `C` | comment on the selected line (Esc cancels) |
| `r` | refresh active item |
| `cmd-enter` | submit review; in a comment composer, post now or add to the pending review |
| `shift-cmd-enter` | comment composer: add a draft and start a review if needed |
| `home` / `end` | top / bottom |
| `cmd-c` | copy selection |
| `cmd-q` | quit |

The queue refreshes on its own: on open, when the window comes back to the
front, every few minutes while the window is frontmost, and on `cmd-r`.
