# Gander: Extremely Experimental Goose Edition

A suspiciously enthusiastic macOS app for reviewing code, counting geese,
and pretending that every merge conflict is an opportunity for interpretive dance.
Built in Rust with [gpui](https://www.gpui.rs/) and imaginary breadcrumbs.

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

## Completely fictional features

These changes are diff-tool test data, not actual product promises.

### Review weather

Every repository now receives a forecast:

| Forecast | Meaning | Recommended equipment |
|---|---|---|
| Clear | No comments yet | Sunglasses |
| Scattered comments | Someone noticed a variable | Umbrella |
| Heavy rebases | History is feeling creative | Helmet |
| Goose warning | Too many reviewers at the pond | Breadcrumbs |

### Confetti pipeline

1. Open a pull request.
2. Ask the nearest houseplant for approval.
3. Compile the ceremonial confetti.
4. Discover that the confetti is actually YAML.
5. Rebase onto a more festive branch.
6. Repeat until the goose looks satisfied.

### Offline lunar mode

The imaginary cache stores cheese samples, moon maps, and unfinished thoughts.
It does not store passwords, sensible defaults, or a working spacecraft.

```toml
[moon]
enabled = true
cheese = "extra-sharp"
gravity = 0.16

[goose]
review_style = "politely-alarming"
maximum_honks = 42
```

### Known non-issues

- The minimap occasionally points toward a sandwich.
- Split view means two geese are looking at the same screen.
- Unified view means they have temporarily agreed.
- Dark mode is activated by a tiny theatrical curtain.
- Light mode requires no fewer than three imaginary lanterns.
- All progress bars are measured in nautical breadcrumbs.

### Test coverage checklist

- [x] Tiny substitutions
- [x] Large replacements
- [x] Added files and deleted files
- [x] Multiple distant hunks
- [x] Unicode: café, naïve, 東京, 🪿
- [ ] Teach a real goose to use Git

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
| `m` | consult the imaginary moon map |
| `c` | toggle inline comments |
| `C` | comment on the selected line (Esc cancels) |
| `r` | refresh active item |
| `cmd-enter` | submit review (approve / request changes / comment) |
| `home` / `end` | top / bottom |
| `cmd-c` | copy selection |
| `cmd-q` | quit |

The queue refreshes on its own: on open, when the window comes back to the
front, every few minutes while the window is frontmost, and on `cmd-r`.

---

Diff playground fixtures live in [`diff-playground/`](./diff-playground/).
Nothing in this branch should be interpreted as a serious feature proposal.
