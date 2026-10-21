mod http;
mod local_store;
mod theme;

use anyhow::anyhow;
use diff_core::{diff_texts, DiffRow, FileDiff, FileStatus, Hunk, PrDiff};
use fuzzy_matcher::{skim::SkimMatcherV2, FuzzyMatcher};
use gpui::{
    actions, canvas, div, fill, font, point, prelude::*, px, relative, size, uniform_list, App,
    Application, AssetSource, Bounds, ClipboardItem, Context, FocusHandle, HighlightStyle, Hsla,
    KeyBinding, Keystroke, ListHorizontalSizingBehavior, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PathPromptOptions, Pixels, Point, Rgba, ScrollStrategy,
    SharedString, StyledText, Subscription, Task, TitlebarOptions, UniformListScrollHandle, Window,
    WindowBounds, WindowOptions,
};
use gpui_component::{
    avatar::Avatar,
    button::{Button, ButtonVariants as _},
    input::{CompletionProvider, Escape as InputEscape, Input, InputEvent, InputState},
    kbd::Kbd,
    scroll::Scrollbar,
    tag::Tag,
    Disableable as _, Icon, IconName, Root, Rope, RopeExt as _, Sizable as _, TitleBar,
};
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

const MONO: &str = "Menlo";

/// Diff pane font size in px, adjustable at runtime (cmd-+ / cmd-- / cmd-0).
/// A process-wide cell rather than a plumbed parameter: it feeds free
/// functions (row building, minimap, hit tests) as well as render, and the app
/// is a single window, so threading it through every call site would be pure
/// churn. Only the main thread writes it.
static FONT_PX: AtomicU32 = AtomicU32::new(DEFAULT_TEXT_SIZE as u32);
const DEFAULT_TEXT_SIZE: f32 = 13.0;
/// Zoom bounds; below ~7px glyphs stop being legible, above ~28px a diff row
/// wastes most of the window.
const MIN_TEXT_SIZE: f32 = 7.0;
const MAX_TEXT_SIZE: f32 = 28.0;
/// Row height as a multiple of the font size. 13 * 1.7 ≈ 22, the height this
/// pane used before zoom existed, so the default look is unchanged.
const LINE_HEIGHT_RATIO: f32 = 1.7;

fn text_size() -> f32 {
    FONT_PX.load(Ordering::Relaxed) as f32
}

/// Row height for a given font size. Kept pure so it's testable without
/// touching the process-wide size.
fn row_height_for(size: f32) -> f32 {
    (size * LINE_HEIGHT_RATIO).round()
}

/// Height of one diff row. Derived from the font so text never outgrows its
/// row; every scroll/hit-test/minimap calculation keys off this.
fn row_height() -> f32 {
    row_height_for(text_size())
}

/// Gutter widths in px, matching render_row's fixed-width children: unified is
/// two 44px line-number columns + a 28px marker; each split cell is one of
/// each. Mouse→column math depends on these.
const UNIFIED_GUTTER: f32 = 44. + 44. + 28.;
const SPLIT_GUTTER: f32 = 44. + 28.;
const SPLIT_DIVIDER: f32 = 6.0;

/// How often the queue refreshes itself while the window is frontmost.
///
/// GitHub's search costs a couple of GraphQL points, so a poll every few
/// minutes is cheap; the reviewer never has to think about it. The poll
/// stops when the window is backgrounded and the focus refresh covers the
/// moment the user comes back.
const BACKGROUND_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(180);

/// The settings key the queue's search (its chips and its sort) is
/// remembered under.
const QUEUE_SEARCH_SETTING: &str = "queue_search";

/// The queue's search as it was last remembered: the chips and the sort,
/// rebuilt into a search.
///
/// A setting this build doesn't understand (or a corrupt one) reads as
/// the default queue — the app must never fail to start over a
/// preference.
fn restore_queue_search(value: Option<serde_json::Value>) -> github::QueueSearch {
    let Some(value) = value else {
        return github::QueueSearch::review_requested();
    };
    let filters = value
        .get("filters")
        .and_then(|filters| filters.as_array())
        .map(|filters| {
            filters
                .iter()
                .filter_map(|filter| {
                    let kind = filter.get("kind")?.as_str()?;
                    let value = filter.get("value")?.as_str()?;
                    Some(match kind {
                        "org" => github::QueueFilter::org(value),
                        "repo" => github::QueueFilter::repo(value),
                        "author" => github::QueueFilter::author(value),
                        _ => return None,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let sort = match value.get("sort").and_then(|sort| sort.as_str()) {
        Some("oldest") => github::QueueSort::OldestUpdated,
        _ => github::QueueSort::RecentlyUpdated,
    };
    github::QueueSearch::from_parts(filters, sort)
}

actions!(
    gander,
    [
        NextFile,
        PrevFile,
        NextHunk,
        PrevHunk,
        GoToTop,
        GoToBottom,
        ToggleView,
        Quit,
        ToggleSidebar,
        OpenInput,
        CloseItem,
        NextItem,
        PrevItem,
        Refresh,
        QueueUp,
        QueueDown,
        QueueOpen,
        BackToQueue,
        QueueSortToggle,
        QueueClearFilters,
        OpenPalette,
        PaletteUp,
        PaletteDown,
        PaletteBack,
        ClearSelection,
        CopySelection,
        FocusTreeFilter,
        ToggleMinimap,
        ToggleComments,
        SubmitReview,
        ZoomIn,
        ZoomOut,
        ZoomReset
    ]
);

/// Environment that makes every `git` we (or anything below us) run use the
/// token from `gh auth login` and never block on a prompt.
///
/// Per-command `-c` flags only reach the processes we spawn ourselves, but
/// the blob cache's git fetches — and any tool that walks a checkout of this
/// repo — can trigger git's own credential lookups, and those run their own
/// `git`. Those grandchildren are invisible to us, so the config has to
/// travel in the environment, which they inherit.
///
/// `GIT_CONFIG_COUNT`/`_KEY_n`/`_VALUE_n` is git's env spelling of `-c`. The
/// empty helper first clears any inherited one (macOS ships `osxkeychain` in
/// Xcode's system gitconfig) so it can't win or raise its own dialog.
///
/// `GIT_TERMINAL_PROMPT=0` is the load-bearing part: git asks for credentials on
/// `/dev/tty` directly, not stdin, so piping a child's stdio does *not* stop the
/// prompt — it just leaves it hanging on the launching terminal.
fn git_env() -> [(&'static str, &'static str); 6] {
    [
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_CONFIG_COUNT", "2"),
        ("GIT_CONFIG_KEY_0", "credential.helper"),
        ("GIT_CONFIG_VALUE_0", ""),
        ("GIT_CONFIG_KEY_1", "credential.helper"),
        ("GIT_CONFIG_VALUE_1", "!gh auth git-credential"),
    ]
}

/// The app's own assets, layered over gpui-component's icon bundle. Today
/// that's the speech bubble the queue's comment count rides on — an
/// Octicon (comment-16, © GitHub, MIT: https://github.com/primer/octicons),
/// which gpui renders as a monochrome mask so it tints with the palette.
/// The app's icons live under `icons/gander/` so they can never shadow an
/// upstream name.
struct AppAssets;

/// (asset path, embedded bytes) — served exactly as an `AssetSource`
/// expects, so the icon renders like any bundled one.
const OWNED_ASSETS: &[(&str, &[u8])] = &[(
    "icons/gander/comment-16.svg",
    include_bytes!("assets/icons/gander/comment-16.svg"),
)];

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        if let Some((_, bytes)) = OWNED_ASSETS.iter().find(|(known, _)| *known == path) {
            return Ok(Some(Cow::Borrowed(bytes)));
        }
        gpui_component_assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        let mut all = gpui_component_assets::Assets.list(path)?;
        all.extend(
            OWNED_ASSETS
                .iter()
                .filter(|(known, _)| known.starts_with(path))
                .map(|(known, _)| SharedString::from(*known)),
        );
        Ok(all)
    }
}

fn main() {
    // Before any thread or child exists: set_var is process-global and not
    // thread-safe, and every subprocess needs to inherit this.
    for (key, value) in git_env() {
        std::env::set_var(key, value);
    }
    // Keep an SSH remote from stalling on a passphrase prompt for the same
    // reason — but never clobber a wrapper the user configured themselves
    // (custom identity, proxy command), which would break their setup.
    if std::env::var_os("GIT_SSH_COMMAND").is_none() {
        std::env::set_var("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut sources = Vec::new();
    let mut errors = Vec::new();
    // Bare launch lands on the review queue — the home screen. The repo we
    // stand in stays one explicit act away (`.` as an argument, or the
    // palette's "open local folder"), so local review keeps working
    // without hijacking the launch.
    for arg in &args {
        let parsed = if Path::new(arg).is_dir() {
            git::resolve_local(Path::new(arg)).map(Source::Local)
        } else {
            gh::resolve_pr_arg(arg).map(Source::Pr)
        };
        match parsed {
            Ok(source) => sources.push(source),
            Err(err) => {
                eprintln!("error: {arg}: {err:#}");
                errors.push(format!("{arg}: {err:#}"));
            }
        }
    }
    Application::new()
        .with_assets(AppAssets)
        // gpui ships a NullHttpClient: without a real one, every remote
        // `img()` (the account avatar, later PR media) silently fails.
        .with_http_client(http::http_client())
        .run(move |cx: &mut App| {
            gpui_component::init(cx);
            theme::apply_ui_theme(None, cx);
            cx.bind_keys([
                KeyBinding::new("]", NextFile, Some("ReviewApp")),
                KeyBinding::new("[", PrevFile, Some("ReviewApp")),
                KeyBinding::new("n", NextHunk, Some("ReviewApp")),
                KeyBinding::new("p", PrevHunk, Some("ReviewApp")),
                KeyBinding::new("home", GoToTop, Some("ReviewApp")),
                KeyBinding::new("end", GoToBottom, Some("ReviewApp")),
                KeyBinding::new("v", ToggleView, Some("ReviewApp")),
                KeyBinding::new("m", ToggleMinimap, Some("ReviewApp")),
                KeyBinding::new("c", ToggleComments, Some("ReviewApp")),
                KeyBinding::new("r", Refresh, Some("ReviewApp")),
                // The queue screen's keys, in its own context: j/k drive the
                // selection, r refreshes, s flips the sort, escape lifts
                // every chip — and the diff's single-key bindings stay quiet
                // while the queue is up.
                KeyBinding::new("j", QueueDown, Some("Queue")),
                KeyBinding::new("k", QueueUp, Some("Queue")),
                // Enter completes j/k: the selected row opens into the
                // changes view.
                KeyBinding::new("enter", QueueOpen, Some("Queue")),
                KeyBinding::new("r", Refresh, Some("Queue")),
                KeyBinding::new("s", QueueSortToggle, Some("Queue")),
                KeyBinding::new("escape", QueueClearFilters, Some("Queue")),
                // `u` leaves the changes view for the queue again; the open
                // item stays open behind the scenes (⌘K / ctrl-tab reach
                // it). Bound in the diff's context so typing in the queue
                // or any input never triggers it.
                KeyBinding::new("u", BackToQueue, Some("ReviewApp")),
                // ⌘R refreshes from anywhere, whatever has focus.
                KeyBinding::new("cmd-r", Refresh, None),
                // Finish the review: approve / request changes / comment.
                KeyBinding::new("cmd-enter", SubmitReview, Some("ReviewApp")),
                // Only while the diff pane has focus; typing `/` in any input
                // stays a plain character.
                KeyBinding::new("/", FocusTreeFilter, Some("ReviewApp")),
                // Selection: escape/cmd-c only fire while the diff pane has
                // focus; with the palette open its input has focus, so the
                // palette's own escape routing wins by construction.
                KeyBinding::new("escape", ClearSelection, Some("ReviewApp")),
                KeyBinding::new("cmd-c", CopySelection, Some("ReviewApp")),
                KeyBinding::new("ctrl-tab", NextItem, Some("ReviewApp")),
                KeyBinding::new("ctrl-shift-tab", PrevItem, Some("ReviewApp")),
                // Zoom. Bind both "cmd-=" and "cmd-+" so it fires with or
                // without shift, the way browsers and editors behave.
                KeyBinding::new("cmd-=", ZoomIn, None),
                KeyBinding::new("cmd-+", ZoomIn, None),
                KeyBinding::new("cmd--", ZoomOut, None),
                KeyBinding::new("cmd-0", ZoomReset, None),
                // Global (None context): must work while the open input is focused.
                KeyBinding::new("cmd-b", ToggleSidebar, None),
                KeyBinding::new("cmd-t", OpenInput, None),
                KeyBinding::new("cmd-w", CloseItem, None),
                KeyBinding::new("cmd-k", OpenPalette, None),
                KeyBinding::new("cmd-q", Quit, None),
                // Palette navigation. The `Palette > Input` variants are bound
                // after gpui_component::init, so at the input's dispatch depth
                // they take precedence over the Input's own up/down (which a
                // single-line input consumes without propagating).
                KeyBinding::new("up", PaletteUp, Some("Palette")),
                KeyBinding::new("down", PaletteDown, Some("Palette")),
                KeyBinding::new("escape", PaletteBack, Some("Palette")),
                KeyBinding::new("up", PaletteUp, Some("Palette > Input")),
                KeyBinding::new("down", PaletteDown, Some("Palette > Input")),
            ]);
            cx.on_action(|_: &Quit, cx| cx.quit());
            // One window is the whole app: closing it quits the process.
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            let bounds = Bounds::centered(None, size(px(1280.), px(860.)), cx);
            // The local store opens before the window: the queue paints
            // from it before anything else happens, so the store's own
            // cost (a few milliseconds for a SQLite open) is on the cold
            // start's critical path.
            let (store, store_error) = local_store::LocalStore::open_default();
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("Gander".into()),
                        ..TitleBar::title_bar_options()
                    }),
                    ..Default::default()
                },
                |window, cx| {
                    let view = cx.new(|cx| {
                        ReviewApp::new(
                            sources,
                            errors,
                            github_client(),
                            store,
                            store_error,
                            window,
                            cx,
                        )
                    });
                    window.focus(&view.read(cx).focus_handle);
                    cx.new(|cx| Root::new(view, window, cx))
                },
            )
            .unwrap();
            cx.activate(true);
        });
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LineKind {
    Context,
    Added,
    Removed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Unified,
    Split,
}

/// One side of a split row: line number, kind, text, word-level highlights,
/// and tree-sitter token spans.
struct Cell {
    no: u32,
    kind: LineKind,
    text: SharedString,
    intra: Vec<Range<usize>>,
    syntax: Vec<(Range<usize>, syntax::Token)>,
}

enum Row {
    Spacer,
    FileHeader {
        path: SharedString,
        old_path: Option<SharedString>,
        status: FileStatus,
        additions: u32,
        deletions: u32,
        /// Review comments anchored in this file / outdated (unanchorable)
        /// ones. Shown as a dim count when nonzero; always populated for PR
        /// items even while comment rows are toggled off.
        comments: usize,
        outdated: usize,
    },
    HunkHeader {
        label: SharedString,
        /// True once the file's hunks come from the Phase-2 full-content
        /// re-diff (renders the label in a subtly different color).
        upgraded: bool,
    },
    Binary,
    /// Hidden shared lines in an upgraded file: before the first hunk, between
    /// hunks, or after the last one. Clicking expands the whole gap.
    /// Selectable-through like headers (`row_side_text` returns None).
    Gap {
        file_ix: usize,
        gap_ix: usize,
        hidden: u32,
    },
    Line {
        old_no: Option<u32>,
        new_no: Option<u32>,
        kind: LineKind,
        text: SharedString,
        intra: Vec<Range<usize>>,
        syntax: Vec<(Range<usize>, syntax::Token)>,
    },
    SplitLine {
        left: Option<Cell>,
        right: Option<Cell>,
    },
    /// First row of one review comment: author + age. Selectable-through
    /// like headers. In split mode `half` places the card in the pane the
    /// comment was left on (LEFT/RIGHT, like GitHub); None spans full width.
    CommentHeader {
        author: SharedString,
        when: SharedString,
        is_reply: bool,
        half: Option<CommentSide>,
        /// First row of the whole thread, so it draws the card's top edge.
        top: bool,
    },
    /// One soft-wrapped line of a comment body (wrapped at
    /// [`COMMENT_WRAP_CHARS`] when the rows are built).
    CommentBody {
        line: SharedString,
        half: Option<CommentSide>,
    },
    /// The "↳ reply" affordance closing a thread; clicking opens the
    /// composer targeting `post_reply` on the thread's root comment.
    CommentActions {
        root_id: u64,
        path: SharedString,
        side: CommentSide,
        line: u64,
        half: Option<CommentSide>,
    },
}

fn is_comment_row(row: &Row) -> bool {
    matches!(
        row,
        Row::CommentHeader { .. } | Row::CommentBody { .. } | Row::CommentActions { .. }
    )
}

/// Index of the `n`-th non-comment row (0-based), clamped to the last row.
/// Comment rows are pure insertions relative to the diff rows, so this maps
/// a position across rebuilds that only add or remove comment rows.
fn nth_noncomment_row(rows: &[Row], n: usize) -> usize {
    let mut seen = 0;
    for (ix, row) in rows.iter().enumerate() {
        if !is_comment_row(row) {
            if seen == n {
                return ix;
            }
            seen += 1;
        }
    }
    rows.len().saturating_sub(1)
}

// --- Review comments -------------------------------------------------------

/// Which diff side a review comment anchors to, GitHub's LEFT/RIGHT.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum CommentSide {
    Left,
    Right,
}

impl CommentSide {
    fn api_str(self) -> &'static str {
        match self {
            CommentSide::Left => "LEFT",
            CommentSide::Right => "RIGHT",
        }
    }
}

/// One review thread: the top-level comment plus its replies, in
/// created_at order.
#[derive(Debug)]
struct CommentThread {
    root: gh::ReviewComment,
    replies: Vec<gh::ReviewComment>,
}

/// One file's threads, keyed by anchor.
type FileAnchors = HashMap<(CommentSide, u64), Vec<CommentThread>>;

/// Review comments grouped for row building.
#[derive(Debug, Default)]
struct CommentIndex {
    /// path → (side, line) → threads in root-created order.
    threads: HashMap<String, FileAnchors>,
    /// path → (anchored comment count, outdated comment count).
    counts: HashMap<String, (usize, usize)>,
}

/// Group flat REST comments into anchored threads: replies attach to their
/// thread via `in_reply_to_id` (orphans are dropped), threads whose root has
/// no current line are only counted as outdated.
fn group_comments(mut comments: Vec<gh::ReviewComment>) -> CommentIndex {
    // ISO-8601 UTC strings sort lexicographically = chronologically, so this
    // orders roots before their replies and threads by root creation.
    comments.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
    let mut threads: Vec<CommentThread> = Vec::new();
    // Comment id (root or reply) → thread index, so replies-to-replies still
    // land in the right thread.
    let mut thread_of: HashMap<u64, usize> = HashMap::new();
    for comment in comments {
        match comment.in_reply_to_id {
            None => {
                thread_of.insert(comment.id, threads.len());
                threads.push(CommentThread {
                    root: comment,
                    replies: Vec::new(),
                });
            }
            Some(parent) => {
                if let Some(&ix) = thread_of.get(&parent) {
                    thread_of.insert(comment.id, ix);
                    threads[ix].replies.push(comment);
                }
                // Orphaned reply (parent not fetched): skip.
            }
        }
    }
    let mut index = CommentIndex::default();
    for thread in threads {
        let size = 1 + thread.replies.len();
        let counts = index.counts.entry(thread.root.path.clone()).or_default();
        let Some(line) = thread.root.line else {
            counts.1 += size;
            continue;
        };
        counts.0 += size;
        let side = if thread.root.side.as_deref() == Some("LEFT") {
            CommentSide::Left
        } else {
            CommentSide::Right
        };
        index
            .threads
            .entry(thread.root.path.clone())
            .or_default()
            .entry((side, line))
            .or_default()
            .push(thread);
    }
    index
}

#[derive(Debug, Default)]
struct LocalReview {
    comments: Vec<gh::ReviewComment>,
    next_id: u64,
}

impl LocalReview {
    fn index(&self) -> CommentIndex {
        group_comments(self.comments.clone())
    }

    fn add_comment(
        &mut self,
        reply_to: Option<u64>,
        path: String,
        side: CommentSide,
        line: u64,
        body: String,
    ) {
        self.next_id += 1;
        self.comments.push(gh::ReviewComment {
            id: self.next_id,
            path,
            line: Some(line),
            side: Some(side.api_str().to_string()),
            start_line: None,
            body,
            user: gh::Author {
                login: "you".to_string(),
            },
            created_at: "draft".to_string(),
            in_reply_to_id: reply_to,
        });
    }
}

/// Widest a comment body ever wraps, however roomy the pane: past ~80 columns
/// prose gets hard to track. Also the fallback before the pane is measured.
const COMMENT_WRAP_CHARS: usize = 80;
/// Never wrap narrower than this, however cramped the pane — below it every
/// word lands on its own line, which is worse than clipping.
const MIN_COMMENT_WRAP_CHARS: usize = 24;

/// Chrome around a comment body inside its card: the gutter indent, the accent
/// border, and the horizontal padding (see `comment_row`).
const COMMENT_CARD_CHROME: f32 = 72. + 2. + 24.;

/// Columns a comment body can use given the measured list width — the whole
/// width in unified, one half in split. Wrapping is then only as narrow as the
/// pane forces, capped at [`COMMENT_WRAP_CHARS`] for readability.
fn comment_wrap_cols(list_width: f32, mode: ViewMode, char_width: f32) -> usize {
    if char_width <= 0. {
        return COMMENT_WRAP_CHARS;
    }
    let card = match mode {
        ViewMode::Unified => list_width,
        ViewMode::Split => (list_width - SPLIT_DIVIDER) / 2.,
    };
    let cols = ((card - COMMENT_CARD_CHROME) / char_width).floor();
    (cols.max(0.) as usize).clamp(MIN_COMMENT_WRAP_CHARS, COMMENT_WRAP_CHARS)
}

/// Soft-wrap `text` at `width` chars: explicit newlines are preserved, wraps
/// prefer the last space in range (the space is consumed), and a word longer
/// than the width hard-breaks on a char boundary.
fn wrap_body(text: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.split('\n') {
        let mut line = line.strip_suffix('\r').unwrap_or(line);
        loop {
            // Byte offset of the (width+1)-th char; absent = the rest fits.
            let Some((cut, _)) = line.char_indices().nth(width) else {
                out.push(line.to_string());
                break;
            };
            match line[..cut].rfind(' ') {
                Some(space) if space > 0 => {
                    out.push(line[..space].to_string());
                    line = &line[space + 1..];
                }
                _ => {
                    out.push(line[..cut].to_string());
                    line = &line[cut..];
                }
            }
        }
    }
    out
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Unix seconds for an ISO-8601 UTC timestamp ("2026-07-01T12:34:56Z").
fn parse_iso_utc(s: &str) -> Option<i64> {
    if s.len() < 20 {
        return None;
    }
    let num = |range: Range<usize>| s.get(range)?.parse::<i64>().ok();
    Some(
        days_from_civil(num(0..4)?, num(5..7)?, num(8..10)?) * 86400
            + num(11..13)? * 3600
            + num(14..16)? * 60
            + num(17..19)?,
    )
}

/// Compact "3d ago"-style age of an ISO timestamp relative to `now` (unix
/// seconds). Unparseable input renders as-is.
fn short_age(iso: &str, now: i64) -> String {
    let Some(t) = parse_iso_utc(iso) else {
        return iso.to_string();
    };
    let d = now - t;
    if d < 60 {
        "just now".to_string()
    } else if d < 3600 {
        format!("{}m ago", d / 60)
    } else if d < 86400 {
        format!("{}h ago", d / 3600)
    } else if d < 365 * 86400 {
        format!("{}d ago", d / 86400)
    } else {
        format!("{}y ago", d / (365 * 86400))
    }
}

/// Append the display rows of every thread anchored at (side, no) — comment
/// headers, wrapped body lines, and one reply-affordance row per thread.
/// No-op when comments are hidden/absent (`anchors` None) or the row has no
/// number on that side.
fn push_thread_rows(
    rows: &mut Vec<Row>,
    anchors: Option<&FileAnchors>,
    path: &str,
    side: CommentSide,
    no: Option<u32>,
    now: i64,
    mode: ViewMode,
    wrap: usize,
) {
    // Split mode renders the thread inside the half it was left on, like the
    // GitHub UI; unified has one column, so the card spans it.
    let half = match mode {
        ViewMode::Split => Some(side),
        ViewMode::Unified => None,
    };
    let (Some(anchors), Some(no)) = (anchors, no) else {
        return;
    };
    let Some(threads) = anchors.get(&(side, no as u64)) else {
        return;
    };
    for thread in threads {
        for (ix, comment) in std::iter::once(&thread.root)
            .chain(&thread.replies)
            .enumerate()
        {
            rows.push(Row::CommentHeader {
                author: comment.user.login.clone().into(),
                when: short_age(&comment.created_at, now).into(),
                is_reply: ix > 0,
                half,
                // A thread always opens with its root's header and closes with
                // the actions row, so those two carry the card's edges.
                top: ix == 0,
            });
            for line in wrap_body(&comment.body, wrap) {
                rows.push(Row::CommentBody {
                    line: line.into(),
                    half,
                });
            }
        }
        rows.push(Row::CommentActions {
            root_id: thread.root.id,
            path: path.to_string().into(),
            side,
            line: no as u64,
            half,
        });
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Which text stream a selection runs through. Split selections are locked to
/// the side where the drag started, like GitHub; the other side paints nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SelSide {
    Unified,
    Left,
    Right,
}

/// A point in text space: display row index + char index into that row's text
/// (char, not byte — convert to byte offsets only when slicing). Ordered by
/// (row, col), which is exactly document order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct RowCol {
    row: usize,
    col: usize,
}

/// Anchor stays where the drag started; head follows the mouse. The ordered
/// pair is derived on use, so dragging upward needs no special casing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Selection {
    side: SelSide,
    anchor: RowCol,
    head: RowCol,
}

impl Selection {
    fn ordered(&self) -> (RowCol, RowCol) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
}

/// The text a selection on `side` runs through for this row, if any. Rows
/// that aren't text (headers, spacers, binary) and absent split cells yield
/// None: they're selectable-through but contribute nothing.
fn row_side_text(row: &Row, side: SelSide) -> Option<&str> {
    match (row, side) {
        (Row::Line { text, .. }, SelSide::Unified) => Some(text.as_ref()),
        (Row::SplitLine { left, .. }, SelSide::Left) => left.as_ref().map(|c| c.text.as_ref()),
        (Row::SplitLine { right, .. }, SelSide::Right) => right.as_ref().map(|c| c.text.as_ref()),
        _ => None,
    }
}

/// Byte offset of char index `col`, clamped to the end of the text.
fn char_to_byte(text: &str, col: usize) -> usize {
    text.char_indices()
        .nth(col)
        .map(|(byte, _)| byte)
        .unwrap_or(text.len())
}

/// The selected byte range within display row `row_ix`, or None if the row
/// contributes nothing (outside the selection, not a text row, or the selected
/// side is absent). The range can be empty (e.g. a selected empty line): copy
/// keeps it as an empty line, painting skips it.
fn row_selection_range(sel: &Selection, row_ix: usize, row: &Row) -> Option<Range<usize>> {
    let (start, end) = sel.ordered();
    if row_ix < start.row || row_ix > end.row {
        return None;
    }
    let text = row_side_text(row, sel.side)?;
    let chars = text.chars().count();
    let start_col = if row_ix == start.row {
        start.col.min(chars)
    } else {
        0
    };
    let end_col = if row_ix == end.row {
        end.col.min(chars)
    } else {
        chars
    };
    if start_col > end_col {
        return None;
    }
    Some(char_to_byte(text, start_col)..char_to_byte(text, end_col))
}

/// The selected text: each contributing row's selected substring, joined with
/// newlines. Header/spacer rows and absent split cells are skipped entirely
/// (no blank line for them).
fn selection_text(sel: &Selection, rows: &[Row]) -> String {
    let (start, end) = sel.ordered();
    let mut parts = Vec::new();
    for ix in start.row..=end.row.min(rows.len().saturating_sub(1)) {
        if let Some(range) = row_selection_range(sel, ix, &rows[ix]) {
            let text = row_side_text(&rows[ix], sel.side).unwrap_or_default();
            parts.push(&text[range]);
        }
    }
    parts.join("\n")
}

/// Guardrails: hunk sides bigger than this render without syntax highlighting.
const MAX_HUNK_SOURCE_BYTES: usize = 100 * 1024;
/// Individual lines longer than this stay plain even in a highlighted hunk.
const MAX_SYNTAX_LINE_BYTES: usize = 4096;

/// Patch-only highlighting: we have no full files, so highlight each hunk's
/// text standalone, per side — old_source is context+removed lines, new_source
/// is context+added — and hand each row its side's line spans (Context and
/// Added from new, Removed from old). Degraded (no cross-hunk parser state)
/// but visually fine. Returns one span list per hunk row, index-aligned.
fn hunk_syntax(
    lang: Option<&'static syntax::Language>,
    rows: &[DiffRow],
) -> Vec<Vec<(Range<usize>, syntax::Token)>> {
    let Some(lang) = lang else {
        return vec![Vec::new(); rows.len()];
    };
    let mut old_source = String::new();
    let mut new_source = String::new();
    // Per row: (takes spans from the old side, line index within that side).
    let mut side_lines = Vec::with_capacity(rows.len());
    let (mut old_line, mut new_line) = (0usize, 0usize);
    for row in rows {
        match row {
            DiffRow::Context { text, .. } => {
                old_source.push_str(text);
                old_source.push('\n');
                old_line += 1;
                new_source.push_str(text);
                new_source.push('\n');
                side_lines.push((false, new_line));
                new_line += 1;
            }
            DiffRow::Added { text, .. } => {
                new_source.push_str(text);
                new_source.push('\n');
                side_lines.push((false, new_line));
                new_line += 1;
            }
            DiffRow::Removed { text, .. } => {
                old_source.push_str(text);
                old_source.push('\n');
                side_lines.push((true, old_line));
                old_line += 1;
            }
        }
    }
    let highlight = |source: &str| {
        if source.is_empty() || source.len() > MAX_HUNK_SOURCE_BYTES {
            Vec::new()
        } else {
            syntax::highlight_lines(lang, source)
        }
    };
    let old_spans = highlight(&old_source);
    let new_spans = highlight(&new_source);
    rows.iter()
        .zip(side_lines)
        .map(|(row, (from_old, line))| {
            let text = match row {
                DiffRow::Context { text, .. }
                | DiffRow::Added { text, .. }
                | DiffRow::Removed { text, .. } => text,
            };
            if text.len() > MAX_SYNTAX_LINE_BYTES {
                return Vec::new();
            }
            let side = if from_old { &old_spans } else { &new_spans };
            side.get(line).cloned().unwrap_or_default()
        })
        .collect()
}

/// Phase-2 result for one file: full new-side lines and whole-file syntax
/// span tables (per line, both sides), plus which gaps the user has expanded.
/// The authoritative re-diffed hunks live in `diff.files[ix].hunks` — this is
/// the extra state needed to render spans and expand context. Lives in
/// `ItemData.upgrades`, so a view-mode toggle preserves expansion and a
/// refresh (which rebuilds ItemData contents) resets it.
struct FileUpgrade {
    /// The complete new-side file, line-ending-normalized, split into lines.
    new_lines: Vec<SharedString>,
    old_spans: Vec<Vec<(Range<usize>, syntax::Token)>>,
    new_spans: Vec<Vec<(Range<usize>, syntax::Token)>>,
    /// Expanded gap indices (0 = before the first hunk, i+1 = after hunk i).
    expanded: HashSet<usize>,
}

impl FileUpgrade {
    /// Whole-file syntax spans for a hunk row, by its side's line number.
    fn row_spans(&self, row: &DiffRow) -> Vec<(Range<usize>, syntax::Token)> {
        let (table, no, text) = match row {
            DiffRow::Context { new_no, text, .. } | DiffRow::Added { new_no, text, .. } => {
                (&self.new_spans, *new_no, text)
            }
            DiffRow::Removed { old_no, text, .. } => (&self.old_spans, *old_no, text),
        };
        if text.len() > MAX_SYNTAX_LINE_BYTES {
            return Vec::new();
        }
        table.get(no as usize - 1).cloned().unwrap_or_default()
    }
}

/// The hidden shared lines in gap `gap_ix` (0..=hunks.len()) of an upgraded
/// file: (first old line, first new line, line count), 1-based. Context lines
/// are shared, so the count is the same on both sides and old numbers stay a
/// constant offset from new ones across the gap. Handles git's zero-count
/// convention (a zero-count side's start is the line *before* the hunk).
fn gap_span(hunks: &[Hunk], gap_ix: usize, total_new: u32) -> (u32, u32, u32) {
    let (old_lo, new_lo) = if gap_ix == 0 {
        (1, 1)
    } else {
        let h = &hunks[gap_ix - 1];
        let pre_old = if h.old_count == 0 {
            h.old_start
        } else {
            h.old_start - 1
        };
        let pre_new = if h.new_count == 0 {
            h.new_start
        } else {
            h.new_start - 1
        };
        (pre_old + h.old_count + 1, pre_new + h.new_count + 1)
    };
    let new_hi = if gap_ix == hunks.len() {
        total_new
    } else {
        let h = &hunks[gap_ix];
        if h.new_count == 0 {
            h.new_start
        } else {
            h.new_start - 1
        }
    };
    (old_lo, new_lo, (new_hi + 1).saturating_sub(new_lo))
}

/// Emit gap `gap_ix` of an upgraded file into `rows`: nothing when no lines
/// are hidden there, synthesized full-context rows when expanded, otherwise
/// one clickable Gap row.
fn push_gap_rows(
    rows: &mut Vec<Row>,
    upgrade: &FileUpgrade,
    hunks: &[Hunk],
    file_ix: usize,
    gap_ix: usize,
    mode: ViewMode,
    path: &str,
    anchors: Option<&FileAnchors>,
    now: i64,
    wrap: usize,
) {
    let (old_lo, new_lo, count) = gap_span(hunks, gap_ix, upgrade.new_lines.len() as u32);
    if count == 0 {
        return;
    }
    if !upgrade.expanded.contains(&gap_ix) {
        rows.push(Row::Gap {
            file_ix,
            gap_ix,
            hidden: count,
        });
        return;
    }
    for j in 0..count {
        let (old_no, new_no) = (old_lo + j, new_lo + j);
        let text = upgrade.new_lines[(new_no - 1) as usize].clone();
        let syntax = if text.len() > MAX_SYNTAX_LINE_BYTES {
            Vec::new()
        } else {
            upgrade
                .new_spans
                .get((new_no - 1) as usize)
                .cloned()
                .unwrap_or_default()
        };
        rows.push(match mode {
            ViewMode::Unified => Row::Line {
                old_no: Some(old_no),
                new_no: Some(new_no),
                kind: LineKind::Context,
                text,
                intra: Vec::new(),
                syntax,
            },
            ViewMode::Split => Row::SplitLine {
                left: Some(Cell {
                    no: old_no,
                    kind: LineKind::Context,
                    text: text.clone(),
                    intra: Vec::new(),
                    syntax: syntax.clone(),
                }),
                right: Some(Cell {
                    no: new_no,
                    kind: LineKind::Context,
                    text,
                    intra: Vec::new(),
                    syntax,
                }),
            },
        });
        push_thread_rows(
            rows,
            anchors,
            path,
            CommentSide::Left,
            Some(old_no),
            now,
            ViewMode::Split,
            wrap,
        );
        push_thread_rows(
            rows,
            anchors,
            path,
            CommentSide::Right,
            Some(new_no),
            now,
            ViewMode::Split,
            wrap,
        );
    }
}

/// Flatten the diff into display rows plus the row indices of file headers and
/// hunk headers. Split mode pairs removed/added runs positionally into
/// two-cell rows; unequal runs leave one-sided rows. Files present in
/// `upgrades` use whole-file syntax spans and get gap rows between hunks.
/// Review threads render beneath the line they anchor to when
/// `show_comments` is set; file headers always carry the comment counts.
fn build_rows(
    diff: &PrDiff,
    mode: ViewMode,
    upgrades: &HashMap<usize, FileUpgrade>,
    comments: Option<&CommentIndex>,
    show_comments: bool,
    wrap: usize,
) -> (Vec<Row>, Vec<usize>, Vec<usize>) {
    let mut rows = Vec::new();
    let mut file_rows = Vec::new();
    let mut hunk_rows = Vec::new();
    let now = now_unix();

    for (file_ix, file) in diff.files.iter().enumerate() {
        let upgrade = upgrades.get(&file_ix);
        let path = file.display_path();
        let (n_comments, n_outdated) = comments
            .and_then(|index| index.counts.get(path))
            .copied()
            .unwrap_or((0, 0));
        let anchors = if show_comments {
            comments.and_then(|index| index.threads.get(path))
        } else {
            None
        };
        if !rows.is_empty() {
            rows.push(Row::Spacer);
        }
        file_rows.push(rows.len());
        rows.push(Row::FileHeader {
            path: path.to_string().into(),
            old_path: match file.status {
                FileStatus::Renamed => file.old_path.clone().map(Into::into),
                _ => None,
            },
            status: file.status,
            additions: file.additions,
            deletions: file.deletions,
            comments: n_comments,
            outdated: n_outdated,
        });
        if file.status == FileStatus::Binary {
            rows.push(Row::Binary);
            continue;
        }
        let lang = syntax::language_for_path(path);
        for (hunk_ix, hunk) in file.hunks.iter().enumerate() {
            if let Some(upgrade) = upgrade {
                push_gap_rows(
                    &mut rows,
                    upgrade,
                    &file.hunks,
                    file_ix,
                    hunk_ix,
                    mode,
                    path,
                    anchors,
                    now,
                    wrap,
                );
            }
            let syntax_spans = match upgrade {
                Some(upgrade) => hunk.rows.iter().map(|row| upgrade.row_spans(row)).collect(),
                None => hunk_syntax(lang, &hunk.rows),
            };
            hunk_rows.push(rows.len());
            let mut label = format!(
                "@@ -{},{} +{},{} @@",
                hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
            );
            if !hunk.section.is_empty() {
                label.push(' ');
                label.push_str(&hunk.section);
            }
            rows.push(Row::HunkHeader {
                label: label.into(),
                upgraded: upgrade.is_some(),
            });
            match mode {
                ViewMode::Unified => {
                    for (ix, row) in hunk.rows.iter().enumerate() {
                        let syntax = syntax_spans[ix].clone();
                        let (old_no, new_no) = match row {
                            DiffRow::Context { old_no, new_no, .. } => {
                                (Some(*old_no), Some(*new_no))
                            }
                            DiffRow::Added { new_no, .. } => (None, Some(*new_no)),
                            DiffRow::Removed { old_no, .. } => (Some(*old_no), None),
                        };
                        rows.push(match row {
                            DiffRow::Context {
                                old_no,
                                new_no,
                                text,
                            } => Row::Line {
                                old_no: Some(*old_no),
                                new_no: Some(*new_no),
                                kind: LineKind::Context,
                                text: text.clone().into(),
                                intra: Vec::new(),
                                syntax,
                            },
                            DiffRow::Added {
                                new_no,
                                text,
                                intra,
                            } => Row::Line {
                                old_no: None,
                                new_no: Some(*new_no),
                                kind: LineKind::Added,
                                text: text.clone().into(),
                                intra: intra.clone(),
                                syntax,
                            },
                            DiffRow::Removed {
                                old_no,
                                text,
                                intra,
                            } => Row::Line {
                                old_no: Some(*old_no),
                                new_no: None,
                                kind: LineKind::Removed,
                                text: text.clone().into(),
                                intra: intra.clone(),
                                syntax,
                            },
                        });
                        push_thread_rows(
                            &mut rows,
                            anchors,
                            path,
                            CommentSide::Left,
                            old_no,
                            now,
                            ViewMode::Unified,
                            wrap,
                        );
                        push_thread_rows(
                            &mut rows,
                            anchors,
                            path,
                            CommentSide::Right,
                            new_no,
                            now,
                            ViewMode::Unified,
                            wrap,
                        );
                    }
                }
                ViewMode::Split => {
                    // Same run-scan shape as diff-core's compute_intra_line:
                    // a run of Removed immediately followed by a run of Added
                    // pairs positionally; the excess (and lone runs) render
                    // one-sided.
                    let hrows = &hunk.rows;
                    let mut i = 0;
                    while i < hrows.len() {
                        match &hrows[i] {
                            DiffRow::Context {
                                old_no,
                                new_no,
                                text,
                            } => {
                                let text: SharedString = text.clone().into();
                                let syntax = syntax_spans[i].clone();
                                rows.push(Row::SplitLine {
                                    left: Some(Cell {
                                        no: *old_no,
                                        kind: LineKind::Context,
                                        text: text.clone(),
                                        intra: Vec::new(),
                                        syntax: syntax.clone(),
                                    }),
                                    right: Some(Cell {
                                        no: *new_no,
                                        kind: LineKind::Context,
                                        text,
                                        intra: Vec::new(),
                                        syntax,
                                    }),
                                });
                                push_thread_rows(
                                    &mut rows,
                                    anchors,
                                    path,
                                    CommentSide::Left,
                                    Some(*old_no),
                                    now,
                                    ViewMode::Split,
                                    wrap,
                                );
                                push_thread_rows(
                                    &mut rows,
                                    anchors,
                                    path,
                                    CommentSide::Right,
                                    Some(*new_no),
                                    now,
                                    ViewMode::Split,
                                    wrap,
                                );
                                i += 1;
                            }
                            DiffRow::Added {
                                new_no,
                                text,
                                intra,
                            } => {
                                // Added run with no preceding Removed run.
                                rows.push(Row::SplitLine {
                                    left: None,
                                    right: Some(Cell {
                                        no: *new_no,
                                        kind: LineKind::Added,
                                        text: text.clone().into(),
                                        intra: intra.clone(),
                                        syntax: syntax_spans[i].clone(),
                                    }),
                                });
                                push_thread_rows(
                                    &mut rows,
                                    anchors,
                                    path,
                                    CommentSide::Right,
                                    Some(*new_no),
                                    now,
                                    ViewMode::Split,
                                    wrap,
                                );
                                i += 1;
                            }
                            DiffRow::Removed { .. } => {
                                let start = i;
                                while i < hrows.len() && matches!(hrows[i], DiffRow::Removed { .. })
                                {
                                    i += 1;
                                }
                                let mid = i;
                                while i < hrows.len() && matches!(hrows[i], DiffRow::Added { .. }) {
                                    i += 1;
                                }
                                let (removed, added) = (mid - start, i - mid);
                                for pair in 0..removed.max(added) {
                                    let left =
                                        (pair < removed).then(|| match &hrows[start + pair] {
                                            DiffRow::Removed {
                                                old_no,
                                                text,
                                                intra,
                                            } => Cell {
                                                no: *old_no,
                                                kind: LineKind::Removed,
                                                text: text.clone().into(),
                                                intra: intra.clone(),
                                                syntax: syntax_spans[start + pair].clone(),
                                            },
                                            _ => unreachable!(),
                                        });
                                    let right = (pair < added).then(|| match &hrows[mid + pair] {
                                        DiffRow::Added {
                                            new_no,
                                            text,
                                            intra,
                                        } => Cell {
                                            no: *new_no,
                                            kind: LineKind::Added,
                                            text: text.clone().into(),
                                            intra: intra.clone(),
                                            syntax: syntax_spans[mid + pair].clone(),
                                        },
                                        _ => unreachable!(),
                                    });
                                    let left_no = left.as_ref().map(|cell| cell.no);
                                    let right_no = right.as_ref().map(|cell| cell.no);
                                    rows.push(Row::SplitLine { left, right });
                                    push_thread_rows(
                                        &mut rows,
                                        anchors,
                                        path,
                                        CommentSide::Left,
                                        left_no,
                                        now,
                                        ViewMode::Split,
                                        wrap,
                                    );
                                    push_thread_rows(
                                        &mut rows,
                                        anchors,
                                        path,
                                        CommentSide::Right,
                                        right_no,
                                        now,
                                        ViewMode::Split,
                                        wrap,
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
        if let Some(upgrade) = upgrade {
            push_gap_rows(
                &mut rows,
                upgrade,
                &file.hunks,
                file_ix,
                file.hunks.len(),
                mode,
                path,
                anchors,
                now,
                wrap,
            );
        }
    }

    (rows, file_rows, hunk_rows)
}

/// Per-kind row tint, word-highlight tint, gutter marker, and marker color.
fn kind_style(
    kind: LineKind,
) -> (
    Option<gpui::Rgba>,
    Option<gpui::Rgba>,
    &'static str,
    gpui::Rgba,
) {
    match kind {
        LineKind::Context => (None, None, "", theme::overlay0()),
        LineKind::Added => (
            Some(theme::added_row_bg()),
            Some(theme::added_word_bg()),
            "+",
            theme::green(),
        ),
        LineKind::Removed => (
            Some(theme::removed_row_bg()),
            Some(theme::removed_word_bg()),
            "−",
            theme::red(),
        ),
    }
}

/// Overlay syntax color spans, intra word-diff background ranges, and the
/// selection background into one sorted, non-overlapping highlight list:
/// ranges are split at every boundary of any input, so an overlap gets the
/// combined style (token foreground + a background). Where the selection
/// overlaps an intra range, the selection background wins; the syntax
/// foreground is kept either way. All inputs are sorted and non-overlapping
/// with char-boundary offsets, so char-boundary safety is preserved.
fn merge_highlights(
    syntax: &[(Range<usize>, syntax::Token)],
    intra: &[Range<usize>],
    word_bg: Option<gpui::Rgba>,
    selection: Option<Range<usize>>,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let mut bounds = Vec::with_capacity(2 * (syntax.len() + intra.len() + 1));
    for (range, _) in syntax {
        bounds.push(range.start);
        bounds.push(range.end);
    }
    for range in intra {
        bounds.push(range.start);
        bounds.push(range.end);
    }
    if let Some(sel) = &selection {
        bounds.push(sel.start);
        bounds.push(sel.end);
    }
    bounds.sort_unstable();
    bounds.dedup();

    let mut out: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
    let (mut si, mut ii) = (0, 0);
    for seg in bounds.windows(2) {
        let (start, end) = (seg[0], seg[1]);
        while si < syntax.len() && syntax[si].0.end <= start {
            si += 1;
        }
        while ii < intra.len() && intra[ii].end <= start {
            ii += 1;
        }
        let token = (si < syntax.len() && syntax[si].0.start <= start).then(|| syntax[si].1);
        let in_intra = ii < intra.len() && intra[ii].start <= start;
        let in_sel = selection
            .as_ref()
            .is_some_and(|sel| sel.start <= start && start < sel.end);
        if token.is_none() && !in_intra && !in_sel {
            continue;
        }
        let mut style = token.map(theme::token_style).unwrap_or_default();
        if in_intra {
            style.background_color = word_bg.map(Into::into);
        }
        if in_sel {
            style.background_color = Some(theme::selection_bg().into());
        }
        match out.last_mut() {
            // Coalesce adjacent identically-styled segments.
            Some((prev, prev_style)) if prev.end == start && *prev_style == style => prev.end = end,
            _ => out.push((start..end, style)),
        }
    }
    out
}

/// Line text with syntax colors overlaid with word-level highlight ranges and
/// the selection background, shared by unified rows and split cells.
fn line_content(
    text: &SharedString,
    syntax: &[(Range<usize>, syntax::Token)],
    intra: &[Range<usize>],
    word_bg: Option<gpui::Rgba>,
    selection: Option<Range<usize>>,
) -> gpui::AnyElement {
    let highlights = merge_highlights(syntax, intra, word_bg, selection);
    if highlights.is_empty() {
        div().child(text.clone()).into_any_element()
    } else {
        StyledText::new(text.clone())
            .with_highlights(highlights)
            .into_any_element()
    }
}

/// Shared shape of every comment row: indented card with a mantle background
/// and a blue left accent, spanning full width in both view modes.
/// Where a row sits in its thread's card, so the rows together draw one
/// continuous outline instead of a box per row.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CardEdge {
    Top,
    Middle,
    Bottom,
}

fn comment_row(
    inner: gpui::AnyElement,
    half: Option<CommentSide>,
    header: bool,
    edge: CardEdge,
) -> gpui::AnyElement {
    // The card itself: indented past the gutter, blue-tinted and accented so a
    // thread reads as one block distinct from the diff behind it. The author
    // line gets a stronger wash so each comment's start is obvious.
    let card = |inner: gpui::AnyElement| {
        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .child(div().w(px(72.)).flex_shrink_0())
            .child({
                // Sides on every row, caps only on the thread's first and last,
                // so the rows stack into one unbroken outline.
                let body = div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .bg(if header {
                        theme::comment_header_bg()
                    } else {
                        theme::comment_bg()
                    })
                    .border_color(theme::comment_outline())
                    .border_l_2()
                    .border_r_1();
                let body = match edge {
                    CardEdge::Top => body.border_t_1().rounded_t_md(),
                    CardEdge::Bottom => body.border_b_1().rounded_b_md(),
                    CardEdge::Middle => body,
                };
                body.px_3()
                    .flex()
                    .items_center()
                    .overflow_hidden()
                    .child(inner)
            })
    };
    let row = div().h(px(row_height())).w_full().flex();
    match half {
        // Unified: one column, so the card spans it.
        None => row.child(card(inner)).into_any_element(),
        // Split: sit in the half the comment was left on, mirroring the line
        // rows' [cell | divider | cell] geometry so the divider stays aligned.
        Some(side) => {
            let empty = || div().flex_1().min_w_0().h_full();
            let divider = div()
                .w(px(SPLIT_DIVIDER))
                .flex_shrink_0()
                .h_full()
                .bg(theme::crust())
                .border_l_1()
                .border_r_1()
                .border_color(theme::surface0());
            match side {
                CommentSide::Left => row
                    .child(card(inner))
                    .child(divider)
                    .child(empty())
                    .into_any_element(),
                CommentSide::Right => row
                    .child(empty())
                    .child(divider)
                    .child(card(inner))
                    .into_any_element(),
            }
        }
    }
}

/// `selection` is this row's selected byte range (side + non-empty range),
/// computed by the caller via `row_selection_range`; its side always matches
/// the row shape (Unified for Line rows, Left/Right for SplitLine rows).
/// `entity` is used by Gap rows (click expands) and CommentActions rows
/// (click opens the reply composer); `ix` is the display-row index.
fn render_row(
    ix: usize,
    row: &Row,
    selection: Option<(SelSide, Range<usize>)>,
    entity: &gpui::Entity<ReviewApp>,
) -> gpui::AnyElement {
    let row_height = px(row_height());
    match row {
        Row::CommentHeader {
            author,
            when,
            is_reply,
            half,
            top,
        } => {
            let mut inner = div().flex().items_center().gap_2().min_w_0();
            if *is_reply {
                inner = inner.child(
                    div()
                        .text_color(theme::overlay0())
                        .child(SharedString::from("↳")),
                );
            }
            comment_row(
                inner
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(theme::text())
                            .child(author.clone()),
                    )
                    .child(
                        div()
                            .text_color(theme::overlay0())
                            .text_size(px(11.))
                            .child(when.clone()),
                    )
                    .into_any_element(),
                *half,
                true,
                if *top {
                    CardEdge::Top
                } else {
                    CardEdge::Middle
                },
            )
        }
        Row::CommentBody { line, half } => comment_row(
            div()
                .whitespace_nowrap()
                .text_color(theme::subtext())
                .child(line.clone())
                .into_any_element(),
            *half,
            false,
            CardEdge::Middle,
        ),
        Row::CommentActions {
            root_id,
            path,
            side,
            line,
            half,
        } => {
            let (root_id, side, line) = (*root_id, *side, *line);
            let path = path.clone();
            let entity = entity.clone();
            comment_row(
                div()
                    .id(("reply", root_id as usize))
                    .cursor_pointer()
                    .text_color(theme::blue())
                    .hover(|style| style.opacity(0.8))
                    .child(SharedString::from("↳ reply"))
                    .on_click(move |_, window, cx| {
                        entity.update(cx, |this, cx| {
                            this.open_composer(
                                Some(root_id),
                                path.to_string(),
                                side,
                                line,
                                ix,
                                window,
                                cx,
                            );
                        });
                    })
                    .into_any_element(),
                *half,
                false,
                CardEdge::Bottom,
            )
        }
        Row::Spacer => div().h(row_height).into_any_element(),
        Row::Gap {
            file_ix,
            gap_ix,
            hidden,
        } => {
            let (file_ix, gap_ix) = (*file_ix, *gap_ix);
            let entity = entity.clone();
            let noun = if *hidden == 1 { "line" } else { "lines" };
            div()
                .id(("gap", (file_ix << 20) | gap_ix))
                .h(row_height)
                .w_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(theme::crust())
                .hover(|style| style.bg(theme::surface0()))
                .cursor_pointer()
                .text_color(theme::overlay0())
                .child(SharedString::from(format!("⋯ {hidden} hidden {noun}")))
                .on_click(move |_, _, cx| {
                    entity.update(cx, |this, cx| this.expand_gap(file_ix, gap_ix, cx));
                })
                .into_any_element()
        }
        Row::FileHeader {
            path,
            old_path,
            status,
            additions,
            deletions,
            comments,
            outdated,
        } => {
            let (status_label, status_color) = status_style(*status);
            let status: Hsla = status_color.into();
            let mut header = div()
                .h(row_height)
                .w_full()
                .flex()
                .items_center()
                .gap_3()
                .px_3()
                .bg(theme::mantle())
                .child(
                    Tag::custom(status.opacity(0.15), status, status.opacity(0.4))
                        .small()
                        .child(SharedString::from(status_label)),
                )
                .child(
                    div()
                        .text_color(theme::text())
                        .font_weight(gpui::FontWeight::BOLD)
                        .child(path.clone()),
                );
            if let Some(old_path) = old_path {
                header = header.child(
                    div()
                        .text_color(theme::overlay0())
                        .child(SharedString::from(format!("← {old_path}"))),
                );
            }
            if *comments > 0 || *outdated > 0 {
                let mut note = String::new();
                if *comments > 0 {
                    note = format!(
                        "{comments} comment{}",
                        if *comments == 1 { "" } else { "s" }
                    );
                }
                if *outdated > 0 {
                    if !note.is_empty() {
                        note.push_str(" · ");
                    }
                    note.push_str(&format!("{outdated} outdated"));
                }
                header = header.child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme::overlay0())
                        .child(SharedString::from(note)),
                );
            }
            header
                .child(div().flex_1())
                .child(
                    div()
                        .text_color(theme::green())
                        .child(SharedString::from(format!("+{additions}"))),
                )
                .child(
                    div()
                        .text_color(theme::red())
                        .child(SharedString::from(format!("−{deletions}"))),
                )
                .into_any_element()
        }
        Row::HunkHeader { label, upgraded } => div()
            .h(row_height)
            .w_full()
            .flex()
            .items_center()
            .px_3()
            .bg(theme::crust())
            // Subtle upgrade indicator: full-content re-diffed hunks get a
            // faint blue label instead of the plain overlay color.
            .text_color(if *upgraded {
                Hsla::from(theme::blue()).opacity(0.55)
            } else {
                theme::overlay0().into()
            })
            .child(label.clone())
            .into_any_element(),
        Row::Binary => div()
            .h(row_height)
            .flex()
            .items_center()
            .px_3()
            .text_color(theme::overlay0())
            .child(SharedString::from("binary file changed"))
            .into_any_element(),
        Row::Line {
            old_no,
            new_no,
            kind,
            text,
            intra,
            syntax,
        } => {
            let (row_bg, word_bg, marker, marker_color) = kind_style(*kind);
            let number = |no: Option<u32>| {
                div()
                    .w(px(44.))
                    .flex_shrink_0()
                    .text_color(theme::overlay0())
                    .flex()
                    .justify_end()
                    .child(SharedString::from(
                        no.map(|no| no.to_string()).unwrap_or_default(),
                    ))
            };
            let mut line = div().h(row_height).flex().items_center();
            if let Some(bg) = row_bg {
                line = line.bg(bg);
            }
            line.child(number(*old_no))
                .child(number(*new_no))
                .child(
                    div()
                        .w(px(28.))
                        .flex_shrink_0()
                        .flex()
                        .justify_center()
                        .text_color(marker_color)
                        .child(SharedString::from(marker)),
                )
                .child(div().whitespace_nowrap().child(line_content(
                    text,
                    syntax,
                    intra,
                    word_bg,
                    selection.map(|(_, range)| range),
                )))
                .into_any_element()
        }
        Row::SplitLine { left, right } => {
            let (left_sel, right_sel) = match selection {
                Some((SelSide::Left, range)) => (Some(range), None),
                Some((SelSide::Right, range)) => (None, Some(range)),
                _ => (None, None),
            };
            let cell = |cell: &Option<Cell>, sel: Option<Range<usize>>| {
                let base = div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .h_full()
                    .flex()
                    .items_center();
                let Some(cell) = cell else {
                    return base.bg(theme::void_cell_bg());
                };
                let (row_bg, word_bg, marker, marker_color) = kind_style(cell.kind);
                let mut side = base;
                if let Some(bg) = row_bg {
                    side = side.bg(bg);
                }
                side.child(
                    div()
                        .w(px(44.))
                        .flex_shrink_0()
                        .text_color(theme::overlay0())
                        .flex()
                        .justify_end()
                        .child(SharedString::from(cell.no.to_string())),
                )
                .child(
                    div()
                        .w(px(28.))
                        .flex_shrink_0()
                        .flex()
                        .justify_center()
                        .text_color(marker_color)
                        .child(SharedString::from(marker)),
                )
                .child(div().whitespace_nowrap().child(line_content(
                    &cell.text,
                    &cell.syntax,
                    &cell.intra,
                    word_bg,
                    sel,
                )))
            };
            // w_full is load-bearing: without a definite row width the row
            // sizes to fit-content and the flex_1 halves collapse to their
            // text width, putting the divider at a different x every row.
            div()
                .h(row_height)
                .w_full()
                .flex()
                .child(cell(left, left_sel))
                .child(
                    div()
                        .w(px(6.))
                        .flex_shrink_0()
                        .h_full()
                        .bg(theme::crust())
                        .border_l_1()
                        .border_r_1()
                        .border_color(theme::surface0()),
                )
                .child(cell(right, right_sel))
                .into_any_element()
        }
    }
}

// --- Minimap ---------------------------------------------------------------

/// Width of the minimap column on the right edge of the diff pane.
const MINIMAP_WIDTH: f32 = 100.0;
/// Horizontal inset of the bars inside the column.
const MINIMAP_PAD: f32 = 4.0;
/// Gap between the two half-columns mirroring split view.
const MINIMAP_GAP: f32 = 2.0;
/// A line this long (or longer) draws a full-width bar.
const MAX_MINIMAP_CHARS: usize = 160;

/// One display row reduced to what the minimap paints: a kind (color) and a
/// width fraction. Index-aligned with `ItemData::rows`, recomputed wherever
/// the rows are rebuilt.
#[derive(Clone, Copy, PartialEq, Debug)]
struct MinimapRow {
    kind: MinimapKind,
    /// Line length / MAX_MINIMAP_CHARS, capped at 1. For SplitPair rows this
    /// is the max of the two halves (used by downsample grouping).
    len_frac: f32,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum MinimapKind {
    Context,
    Added,
    Removed,
    /// A split-view row: per-half width fractions, and whether each half
    /// holds a change (left = removed present, right = added present) as
    /// opposed to context. Absent halves have a zero fraction.
    SplitPair {
        left_frac: f32,
        right_frac: f32,
        left: bool,
        right: bool,
    },
    Header,
    Gap,
    Blank,
}

/// Fraction of a full-width minimap bar for one line of text. Counting stops
/// at the cap, so pathological lines cost nothing extra.
fn line_frac(text: &str) -> f32 {
    text.chars().take(MAX_MINIMAP_CHARS).count() as f32 / MAX_MINIMAP_CHARS as f32
}

/// Reduce display rows to minimap rows, one per row, index-aligned.
fn minimap_rows(rows: &[Row]) -> Vec<MinimapRow> {
    rows.iter()
        .map(|row| match row {
            // Comment rows stay blank in the minimap: threads are short and
            // already flagged by the file-header counts.
            Row::Spacer
            | Row::Binary
            | Row::CommentHeader { .. }
            | Row::CommentBody { .. }
            | Row::CommentActions { .. } => MinimapRow {
                kind: MinimapKind::Blank,
                len_frac: 0.,
            },
            Row::FileHeader { .. } | Row::HunkHeader { .. } => MinimapRow {
                kind: MinimapKind::Header,
                len_frac: 1.,
            },
            Row::Gap { .. } => MinimapRow {
                kind: MinimapKind::Gap,
                len_frac: 1.,
            },
            Row::Line { kind, text, .. } => MinimapRow {
                kind: match kind {
                    LineKind::Context => MinimapKind::Context,
                    LineKind::Added => MinimapKind::Added,
                    LineKind::Removed => MinimapKind::Removed,
                },
                len_frac: line_frac(text),
            },
            Row::SplitLine { left, right } => {
                let frac =
                    |cell: &Option<Cell>| cell.as_ref().map(|c| line_frac(&c.text)).unwrap_or(0.);
                let (left_frac, right_frac) = (frac(left), frac(right));
                MinimapRow {
                    kind: MinimapKind::SplitPair {
                        left_frac,
                        right_frac,
                        left: matches!(left, Some(c) if c.kind == LineKind::Removed),
                        right: matches!(right, Some(c) if c.kind == LineKind::Added),
                    },
                    len_frac: left_frac.max(right_frac),
                }
            }
        })
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MinimapLane {
    /// Bar from the left edge across the full usable width (unified rows and
    /// header/gap ticks).
    Full,
    /// Left half-column (split view's left cell).
    Left,
    /// Right half-column (split view's right cell).
    Right,
}

/// Bar colors as a plain enum so the precompute stays theme-free and testable;
/// painting maps them to theme colors.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MinimapColor {
    Added,
    Removed,
    Context,
    Header,
    Gap,
}

/// Downsample priority: when several rows share one pixel-row, the highest
/// wins (Blank rows contribute nothing and lose to everything).
fn minimap_priority(color: MinimapColor) -> u8 {
    match color {
        MinimapColor::Removed => 5,
        MinimapColor::Added => 4,
        MinimapColor::Header => 3,
        MinimapColor::Gap => 2,
        MinimapColor::Context => 1,
    }
}

/// A coalesced vertical run of identical bars, in slot space: a slot is one
/// painted row of the minimap, `slot_h` px tall, covering `group` display
/// rows. `tick` runs paint 1px tall at the slot top instead of filling it.
#[derive(Clone, Copy, PartialEq, Debug)]
struct MinimapRun {
    start: u32,
    /// Exclusive.
    end: u32,
    lane: MinimapLane,
    color: MinimapColor,
    frac: f32,
    tick: bool,
}

struct MinimapLayout {
    /// Painted height of one slot: clamp(pane_height / total_rows, 1, 3).
    slot_h: f32,
    /// Display rows per slot; 1 unless the diff is taller than the pane at
    /// 1px per row, then ceil(total / pane_px).
    group: usize,
    runs: Vec<MinimapRun>,
}

fn minimap_scale(total: usize, pane_px: f32) -> (f32, usize) {
    if total == 0 || pane_px <= 0. {
        return (1., 1);
    }
    if total as f32 > pane_px {
        (1., (total as f32 / pane_px).ceil() as usize)
    } else {
        ((pane_px / total as f32).clamp(1., 3.), 1)
    }
}

/// The full minimap precompute: scale, downsample, and coalesce into quad
/// runs. Per group and lane the highest-priority color wins and the width is
/// the group's max fraction; vertically adjacent slots with the same lane,
/// color, and width merge into one run. Recomputed only when the rows are
/// rebuilt or the pane height changes — painting just iterates the runs.
fn minimap_runs(rows: &[MinimapRow], pane_px: f32) -> MinimapLayout {
    let (slot_h, group) = minimap_scale(rows.len(), pane_px);
    let mut runs: Vec<MinimapRun> = Vec::new();
    // Per lane: index into `runs` of the run still open for extension.
    let mut open: [Option<usize>; 3] = [None; 3];
    for (slot, chunk) in rows.chunks(group).enumerate() {
        let mut winner: [Option<MinimapColor>; 3] = [None; 3];
        let mut frac: [f32; 3] = [0.; 3];
        for row in chunk {
            let mut fold = |lane: MinimapLane, color: MinimapColor, f: f32| {
                let ix = lane as usize;
                if winner[ix].map_or(true, |best| {
                    minimap_priority(color) > minimap_priority(best)
                }) {
                    winner[ix] = Some(color);
                }
                frac[ix] = frac[ix].max(f);
            };
            match row.kind {
                MinimapKind::Blank => {}
                MinimapKind::Header => fold(MinimapLane::Full, MinimapColor::Header, 1.),
                MinimapKind::Gap => fold(MinimapLane::Full, MinimapColor::Gap, 1.),
                MinimapKind::Context => {
                    fold(MinimapLane::Full, MinimapColor::Context, row.len_frac)
                }
                MinimapKind::Added => fold(MinimapLane::Full, MinimapColor::Added, row.len_frac),
                MinimapKind::Removed => {
                    fold(MinimapLane::Full, MinimapColor::Removed, row.len_frac)
                }
                MinimapKind::SplitPair {
                    left_frac,
                    right_frac,
                    left,
                    right,
                } => {
                    if left_frac > 0. || left {
                        let color = if left {
                            MinimapColor::Removed
                        } else {
                            MinimapColor::Context
                        };
                        fold(MinimapLane::Left, color, left_frac);
                    }
                    if right_frac > 0. || right {
                        let color = if right {
                            MinimapColor::Added
                        } else {
                            MinimapColor::Context
                        };
                        fold(MinimapLane::Right, color, right_frac);
                    }
                }
            }
        }
        for ix in 0..3 {
            let (Some(color), f) = (winner[ix], frac[ix]) else {
                open[ix] = None;
                continue;
            };
            // Zero-width bars (empty lines) paint nothing; they also break
            // the run so bars on either side don't fuse across them.
            if f <= 0. {
                open[ix] = None;
                continue;
            }
            // Header/gap ticks are 1px marks; when a slot is already 1px
            // they're plain bars and coalesce like everything else.
            let tick = matches!(color, MinimapColor::Header | MinimapColor::Gap) && slot_h > 1.;
            if let Some(run_ix) = open[ix] {
                let run = &mut runs[run_ix];
                if !tick && !run.tick && run.color == color && run.frac == f {
                    run.end += 1;
                    continue;
                }
            }
            open[ix] = (!tick).then_some(runs.len());
            runs.push(MinimapRun {
                start: slot as u32,
                end: slot as u32 + 1,
                lane: [MinimapLane::Full, MinimapLane::Left, MinimapLane::Right][ix],
                color,
                frac: f,
                tick,
            });
        }
    }
    MinimapLayout {
        slot_h,
        group,
        runs,
    }
}

// --- Sidebar file tree ---------------------------------------------------

/// Row height of sidebar file-tree entries.
const TREE_ROW_HEIGHT: f32 = 24.0;

/// One row of the file tree, flattened depth-first for a uniform_list.
/// Directories precede files at each level; both are name-sorted.
#[derive(Debug, PartialEq)]
struct TreeEntry {
    depth: usize,
    /// Display name: a file name, or a compressed single-child directory
    /// chain ("src/core/flags").
    name: SharedString,
    kind: TreeEntryKind,
}

#[derive(Debug, PartialEq)]
enum TreeEntryKind {
    /// Full path of the chain's deepest directory — the collapse-state key.
    Dir { path: String },
    /// Index into `PrDiff::files` (and thus `ItemData::file_rows`).
    File { file_ix: usize },
}

/// Group file paths (diff order) into a depth-first tree. Directory chains
/// with a single child directory and no files of their own compress into one
/// entry, GitHub-style.
fn build_tree(paths: &[&str]) -> Vec<TreeEntry> {
    #[derive(Default)]
    struct DirNode {
        dirs: std::collections::BTreeMap<String, DirNode>,
        files: Vec<(String, usize)>,
    }
    let mut root = DirNode::default();
    for (file_ix, path) in paths.iter().enumerate() {
        let (dirs, name) = match path.rsplit_once('/') {
            Some((dirs, name)) => (Some(dirs), name),
            None => (None, *path),
        };
        let mut node = &mut root;
        for part in dirs.into_iter().flat_map(|dirs| dirs.split('/')) {
            node = node.dirs.entry(part.to_string()).or_default();
        }
        node.files.push((name.to_string(), file_ix));
    }
    fn flatten(node: DirNode, prefix: &str, depth: usize, out: &mut Vec<TreeEntry>) {
        for (name, mut child) in node.dirs {
            let mut label = name;
            let mut path = if prefix.is_empty() {
                label.clone()
            } else {
                format!("{prefix}/{label}")
            };
            while child.files.is_empty() && child.dirs.len() == 1 {
                let (next_name, next) = child.dirs.into_iter().next().unwrap();
                label.push('/');
                label.push_str(&next_name);
                path.push('/');
                path.push_str(&next_name);
                child = next;
            }
            out.push(TreeEntry {
                depth,
                name: label.into(),
                kind: TreeEntryKind::Dir { path: path.clone() },
            });
            flatten(child, &path, depth + 1, out);
        }
        let mut files = node.files;
        files.sort();
        for (name, file_ix) in files {
            out.push(TreeEntry {
                depth,
                name: name.into(),
                kind: TreeEntryKind::File { file_ix },
            });
        }
    }
    let mut out = Vec::new();
    flatten(root, "", 0, &mut out);
    out
}

/// Indices of the entries visible given the collapsed directories: everything
/// deeper than a collapsed dir (until the next entry at its depth) is hidden.
fn visible_entries(entries: &[TreeEntry], collapsed: &HashSet<String>) -> Vec<usize> {
    let mut out = Vec::with_capacity(entries.len());
    let mut hide_deeper_than: Option<usize> = None;
    for (ix, entry) in entries.iter().enumerate() {
        if let Some(depth) = hide_deeper_than {
            if entry.depth > depth {
                continue;
            }
            hide_deeper_than = None;
        }
        out.push(ix);
        if let TreeEntryKind::Dir { path } = &entry.kind {
            if collapsed.contains(path) {
                hide_deeper_than = Some(entry.depth);
            }
        }
    }
    out
}

/// File indices whose full path fuzzy-matches `query`, best score first.
/// An empty query keeps every file in diff order.
fn fuzzy_file_matches(paths: &[&str], query: &str) -> Vec<usize> {
    let query = query.trim();
    if query.is_empty() {
        return (0..paths.len()).collect();
    }
    let matcher = SkimMatcherV2::default();
    let mut scored: Vec<(i64, usize)> = paths
        .iter()
        .enumerate()
        .filter_map(|(ix, path)| matcher.fuzzy_match(path, query).map(|score| (score, ix)))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, ix)| ix).collect()
}

/// Status label and color, shared by file headers and tree entries.
fn status_style(status: FileStatus) -> (&'static str, gpui::Rgba) {
    match status {
        FileStatus::Added => ("added", theme::green()),
        FileStatus::Deleted => ("deleted", theme::red()),
        FileStatus::Modified => ("modified", theme::blue()),
        FileStatus::Renamed => ("renamed", theme::mauve()),
        FileStatus::Binary => ("binary", theme::peach()),
    }
}

/// One row of the sidebar's tree list: an index into `ItemData::tree`, or —
/// while the fuzzy filter is active — a matching file shown as its full path.
#[derive(Clone, Copy)]
enum TreeListRow {
    Entry(usize),
    FilteredFile(usize),
}

/// `current` marks the file the diff viewport is showing (surface0, like the
/// active sidebar item). Clicking a dir toggles collapse; clicking a file
/// jumps the diff to its header.
fn render_tree_row(
    row: TreeListRow,
    pos: usize,
    current: bool,
    data: &ItemData,
    entity: &gpui::Entity<ReviewApp>,
) -> gpui::AnyElement {
    let stats = |file: &FileDiff| {
        div()
            .flex()
            .items_center()
            .gap_1()
            .flex_shrink_0()
            .text_size(px(10.))
            .child(
                div()
                    .text_color(Hsla::from(theme::green()).opacity(0.7))
                    .child(SharedString::from(format!("+{}", file.additions))),
            )
            .child(
                div()
                    .text_color(Hsla::from(theme::red()).opacity(0.7))
                    .child(SharedString::from(format!("−{}", file.deletions))),
            )
    };
    let entity = entity.clone();
    let base = div()
        .id(("tree-row", pos))
        .h(px(TREE_ROW_HEIGHT))
        .w_full()
        .flex()
        .items_center()
        .gap_1()
        .pr_2()
        .cursor_pointer()
        .when(current, |row| row.bg(theme::surface0()))
        .when(!current, |row| {
            row.hover(|style| style.bg(Hsla::from(theme::surface0()).opacity(0.5)))
        });
    match row {
        TreeListRow::Entry(entry_ix) => {
            let entry = &data.tree[entry_ix];
            let indent = px(8. + entry.depth as f32 * 12.);
            let base = base.pl(indent).on_click(move |_, window, cx| {
                entity.update(cx, |this, cx| this.tree_entry_clicked(entry_ix, window, cx));
            });
            match &entry.kind {
                TreeEntryKind::Dir { path } => {
                    let chevron = if data.collapsed.contains(path) {
                        "▸"
                    } else {
                        "▾"
                    };
                    base.child(
                        div()
                            .w(px(12.))
                            .flex_shrink_0()
                            .text_color(theme::overlay0())
                            .child(SharedString::from(chevron)),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(theme::overlay0())
                            .child(entry.name.clone()),
                    )
                    .into_any_element()
                }
                TreeEntryKind::File { file_ix } => {
                    let file = &data.diff.files[*file_ix];
                    let (status_label, status_color) = status_style(file.status);
                    let status: Hsla = status_color.into();
                    base.child(div().w(px(12.)).flex_shrink_0()) // aligns with dir chevrons
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(status_color)
                                .child(entry.name.clone()),
                        )
                        // The status tag mirrors the file header's — the
                        // tree answers "what happened to this file" at a
                        // glance, without reading colors alone.
                        .child(
                            Tag::custom(status.opacity(0.15), status, status.opacity(0.4))
                                .xsmall()
                                .child(SharedString::from(status_label)),
                        )
                        .child(stats(file))
                        .into_any_element()
                }
            }
        }
        TreeListRow::FilteredFile(file_ix) => {
            let file = &data.diff.files[file_ix];
            let (status_label, status_color) = status_style(file.status);
            let status: Hsla = status_color.into();
            base.pl_2()
                .on_click(move |_, window, cx| {
                    entity.update(cx, |this, cx| this.jump_to_file(file_ix, window, cx));
                })
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(status_color)
                        .child(SharedString::from(file.display_path().to_string())),
                )
                .child(
                    Tag::custom(status.opacity(0.15), status, status.opacity(0.4))
                        .xsmall()
                        .child(SharedString::from(status_label)),
                )
                .child(stats(file))
                .into_any_element()
        }
    }
}

/// Where a review item's diff comes from.
#[derive(Clone)]
enum Source {
    Pr(gh::PrLocator),
    Local(git::LocalSource),
}

/// Whether two sources describe the same thing to review: a PR by
/// owner/repo/number, a local diff by its working tree and base. The
/// session opens one item per source — [`ReviewApp::open_item`] brings an
/// existing item forward instead of duplicating it.
fn sources_match(a: &Source, b: &Source) -> bool {
    match (a, b) {
        (Source::Pr(x), Source::Pr(y)) => {
            x.owner == y.owner && x.repo == y.repo && x.number == y.number
        }
        (Source::Local(x), Source::Local(y)) => {
            x.repo_root == y.repo_root && x.base_ref == y.base_ref
        }
        _ => false,
    }
}

enum ItemState {
    Loading,
    Ready(Box<ItemData>),
    Failed(String),
}

/// Everything the diff pane needs for one loaded item. Each item owns its
/// scroll handle so per-item scroll position survives switching.
struct ItemData {
    pr_meta: Option<gh::PrMeta>,
    diff: PrDiff,
    mode: ViewMode,
    rows: Vec<Row>,
    file_rows: Vec<usize>,
    hunk_rows: Vec<usize>,
    /// Minimap model, index-aligned with `rows`; rebuilt with them.
    minimap: Vec<MinimapRow>,
    /// Coalesced minimap quad runs for one pane height, computed lazily on
    /// paint and reused until the height changes or the rows are rebuilt.
    minimap_cache: RefCell<Option<(f32, Rc<MinimapLayout>)>>,
    cursor: usize,
    scroll: UniformListScrollHandle,
    additions: u32,
    deletions: u32,
    /// Mouse text selection, in display-row space. Per item (survives item
    /// switching); cleared on view-mode toggle and refresh, where row indices
    /// change meaning.
    selection: Option<Selection>,
    /// Phase-2 upgrades by file index into `diff.files`: whole-file span
    /// tables, full new-side lines, and expanded gaps. The re-diffed hunks
    /// themselves replace `diff.files[ix].hunks`. Reset on refresh.
    upgrades: HashMap<usize, FileUpgrade>,
    /// Review threads grouped by anchor; Some for PR items (possibly empty),
    /// and for local items with in-memory draft comments.
    comments: Option<CommentIndex>,
    local_review: Option<LocalReview>,
    /// `c` toggles the comment rows; the file-header counts always show.
    comments_visible: bool,
    /// Columns comment bodies are currently wrapped at, derived from the
    /// measured pane width (see `comment_wrap_cols`). Changing it re-wraps the
    /// bodies, which changes the row count, so render rebuilds the rows when
    /// the pane resizes past a column boundary.
    comment_wrap: usize,
    /// Users offered by the composer's `@`-mention autocomplete. Shared live
    /// with any open `MentionProvider`; seeded from PR participants, then
    /// filled from the repo's mentionable set (see `mentions_fetched`).
    mentions: Rc<RefCell<Vec<gh::Mention>>>,
    /// The one-time background mentionable-user fetch has been kicked off.
    mentions_fetched: bool,
    /// Sidebar file tree, rebuilt whenever the diff itself changes (load,
    /// refresh, blob upgrade) — but not on view-mode toggles: entries map to
    /// file indices, not row indices, so they survive row rebuilds.
    tree: Vec<TreeEntry>,
    /// Collapsed directory paths, preserved across rebuilds where the
    /// directory still exists.
    collapsed: HashSet<String>,
    tree_scroll: UniformListScrollHandle,
    /// The file last auto-centered in the tree, so follow-the-diff only
    /// scrolls the tree when the viewport's file changes — never while the
    /// user scrolls the tree themselves.
    tree_last_file: Option<usize>,
}

impl ItemData {
    /// Install freshly built display rows, keeping the minimap model in sync
    /// (every row rebuild goes through here).
    fn set_rows(&mut self, (rows, file_rows, hunk_rows): (Vec<Row>, Vec<usize>, Vec<usize>)) {
        self.minimap = minimap_rows(&rows);
        self.minimap_cache.replace(None);
        self.rows = rows;
        self.file_rows = file_rows;
        self.hunk_rows = hunk_rows;
    }

    /// Rebuild the display rows after only the comment rows changed
    /// (visibility toggle, comment refetch), keeping the viewport anchored:
    /// the first visible non-comment row stays put even though comment rows
    /// above it appeared or disappeared.
    fn rebuild_rows_anchored(&mut self) {
        let offset = self.scroll.0.borrow().base_handle.offset();
        let top_px = f32::from(-offset.y).max(0.);
        let top_row =
            ((top_px / row_height()).floor() as usize).min(self.rows.len().saturating_sub(1));
        let frac = top_px - top_row as f32 * row_height();
        let count_noncomment =
            |rows: &[Row]| rows.iter().filter(|row| !is_comment_row(row)).count();
        let top_base = count_noncomment(&self.rows[..top_row]);
        let cursor_base = count_noncomment(&self.rows[..self.cursor.min(self.rows.len())]);
        self.set_rows(build_rows(
            &self.diff,
            self.mode,
            &self.upgrades,
            self.comments.as_ref(),
            self.comments_visible,
            self.comment_wrap,
        ));
        self.selection = None;
        self.cursor = nth_noncomment_row(&self.rows, cursor_base);
        let new_top = nth_noncomment_row(&self.rows, top_base);
        self.scroll
            .0
            .borrow()
            .base_handle
            .set_offset(point(offset.x, px(-(new_top as f32 * row_height() + frac))));
    }

    /// The minimap quad runs for this pane height, from the cache when the
    /// height hasn't changed since the last paint.
    fn minimap_layout(&self, pane_px: f32) -> Rc<MinimapLayout> {
        let mut cache = self.minimap_cache.borrow_mut();
        if let Some((h, layout)) = &*cache {
            if *h == pane_px {
                return layout.clone();
            }
        }
        let layout = Rc::new(minimap_runs(&self.minimap, pane_px));
        *cache = Some((pane_px, layout.clone()));
        layout
    }

    /// Rebuild the sidebar file tree from the current diff, keeping collapse
    /// state for directories that still exist.
    fn rebuild_tree(&mut self) {
        let paths: Vec<&str> = self.diff.files.iter().map(|f| f.display_path()).collect();
        let tree = build_tree(&paths);
        self.collapsed.retain(|path| {
            tree.iter()
                .any(|e| matches!(&e.kind, TreeEntryKind::Dir { path: p } if p == path))
        });
        self.tree = tree;
        self.tree_last_file = None;
    }
}

struct ReviewItem {
    id: u64,
    source: Source,
    state: ItemState,
    /// A refresh is in flight while the old data stays visible.
    reloading: bool,
    refresh_error: Option<SharedString>,
    /// Bumped whenever fresh data is installed; an in-flight Phase-2 upgrade
    /// only lands if the generation it captured is still current.
    upgrade_gen: u64,
}

impl ReviewItem {
    fn primary(&self) -> SharedString {
        match &self.source {
            Source::Pr(loc) => format!("{}#{}", loc.repo_slug(), loc.number).into(),
            Source::Local(src) => src.branch.clone().into(),
        }
    }

    fn secondary(&self) -> SharedString {
        match &self.source {
            Source::Pr(_) => match &self.state {
                ItemState::Ready(data) => data
                    .pr_meta
                    .as_ref()
                    .map(|meta| meta.title.clone())
                    .unwrap_or_default()
                    .into(),
                _ => SharedString::default(),
            },
            Source::Local(src) => {
                format!("{} ← {}", dir_name(&src.repo_root), src.base_label).into()
            }
        }
    }

    /// Open a PR from the local store, for the case the network isn't
    /// there: the app has a snapshot of the PR from the last time it
    /// fetched it, and shows that instead of an error.
    ///
    /// The diff itself needs the network (the patch comes from GitHub),
    /// so what's shown is the PR's metadata — its title, its body, its
    /// author — and an empty diff pane. It is a readable PR, not a
    /// complete one: the reviewer can read the conversation offline and
    /// see the code again when the network is back.
    ///
    /// Returns false when the store has no snapshot of this PR: there's
    /// nothing to show, and the caller falls back to the error.
    fn install_offline_snapshot(&mut self, store: &local_store::LocalStore) -> bool {
        let Source::Pr(loc) = &self.source else {
            return false;
        };
        let Some(snapshot) = store.snapshot(&loc.repo_slug(), loc.number) else {
            return false;
        };
        let meta = gh::PrMeta {
            number: snapshot.number,
            title: snapshot.title.clone(),
            author: gh::Author {
                login: snapshot.author.clone(),
            },
            state: "OPEN".to_string(),
            url: format!(
                "https://github.com/{}/pull/{}",
                snapshot.repository, snapshot.number
            ),
            body: snapshot.body.clone(),
            base_ref_name: snapshot.base_ref.clone(),
            head_ref_name: snapshot.head_ref.clone(),
            base_ref_oid: String::new(),
            head_ref_oid: snapshot.head_sha.clone(),
            additions: 0,
            deletions: 0,
            changed_files: 0,
            review_decision: String::new(),
        };
        let diff = diff_core::PrDiff::default();
        let (rows, file_rows, hunk_rows) = build_rows(
            &diff,
            ViewMode::Split,
            &HashMap::new(),
            None,
            true,
            COMMENT_WRAP_CHARS,
        );
        let minimap = minimap_rows(&rows);
        let mut data = Box::new(ItemData {
            pr_meta: Some(meta),
            diff,
            mode: ViewMode::Split,
            rows,
            file_rows,
            hunk_rows,
            minimap,
            minimap_cache: RefCell::new(None),
            cursor: 0,
            scroll: UniformListScrollHandle::new(),
            additions: 0,
            deletions: 0,
            selection: None,
            upgrades: HashMap::new(),
            comments: None,
            local_review: None,
            comments_visible: true,
            comment_wrap: COMMENT_WRAP_CHARS,
            mentions: Rc::new(RefCell::new(Vec::new())),
            mentions_fetched: false,
            tree: Vec::new(),
            collapsed: HashSet::new(),
            tree_scroll: UniformListScrollHandle::new(),
            tree_last_file: None,
        });
        data.rebuild_tree();
        self.state = ItemState::Ready(data);
        true
    }

    fn dot_color(&self) -> gpui::Rgba {
        match &self.source {
            Source::Local(_) => theme::blue(),
            Source::Pr(_) => match &self.state {
                ItemState::Ready(data) => match data.pr_meta.as_ref().map(|m| m.state.as_str()) {
                    Some("OPEN") => theme::green(),
                    Some("MERGED") => theme::mauve(),
                    Some("CLOSED") => theme::red(),
                    _ => theme::overlay0(),
                },
                ItemState::Failed(_) => theme::red(),
                ItemState::Loading => theme::overlay0(),
            },
        }
    }

    /// Swap fetched data in. On refresh (already Ready) scroll position,
    /// cursor, and view mode are preserved.
    ///
    /// Returns the PR's snapshot, when the fetch was a PR's: the caller
    /// keeps it in the local store, so the next launch knows what the app
    /// last saw of this PR.
    fn install(&mut self, loaded: Loaded) -> Option<local_store::StoredPrSnapshot> {
        let Loaded {
            meta,
            diff,
            mut rows,
            mut file_rows,
            mut hunk_rows,
            mode,
            comments,
            snapshot,
        } = loaded;
        let (additions, deletions) = diff
            .files
            .iter()
            .fold((0, 0), |(a, d), f| (a + f.additions, d + f.deletions));
        let pr_meta = match meta {
            LoadedMeta::Pr(meta) => Some(meta),
            LoadedMeta::Local(src) => {
                // Branch/base may have moved since open; keep the label fresh.
                self.source = Source::Local(src);
                None
            }
        };
        self.refresh_error = None;
        match &mut self.state {
            ItemState::Ready(data) => {
                // Fresh patch-derived data: any previous upgrade (and its
                // expanded gaps) is stale — the upgrade re-runs from scratch.
                data.upgrades.clear();
                data.comments = match &data.local_review {
                    Some(local) => Some(local.index()),
                    None => comments,
                };
                if data.local_review.is_some() || data.mode != mode || !data.comments_visible {
                    // Background rows assumed the fetched comments. Local
                    // drafts are reattached here, and view/comment toggles may
                    // have changed while the refresh ran.
                    (rows, file_rows, hunk_rows) = build_rows(
                        &diff,
                        data.mode,
                        &data.upgrades,
                        data.comments.as_ref(),
                        data.comments_visible,
                        data.comment_wrap,
                    );
                }
                if pr_meta.is_some() {
                    data.pr_meta = pr_meta;
                }
                data.diff = diff;
                data.set_rows((rows, file_rows, hunk_rows));
                data.cursor = data.cursor.min(data.rows.len().saturating_sub(1));
                data.additions = additions;
                data.deletions = deletions;
                data.selection = None;
                data.rebuild_tree();
            }
            _ => {
                let minimap = minimap_rows(&rows);
                let local_review =
                    matches!(&self.source, Source::Local(_)).then(LocalReview::default);
                let comments = match &local_review {
                    Some(local) => Some(local.index()),
                    None => comments,
                };
                let mut data = Box::new(ItemData {
                    pr_meta,
                    diff,
                    mode,
                    rows,
                    file_rows,
                    hunk_rows,
                    minimap,
                    minimap_cache: RefCell::new(None),
                    cursor: 0,
                    scroll: UniformListScrollHandle::new(),
                    additions,
                    deletions,
                    selection: None,
                    upgrades: HashMap::new(),
                    comments,
                    local_review,
                    comments_visible: true,
                    comment_wrap: COMMENT_WRAP_CHARS,
                    mentions: Rc::new(RefCell::new(Vec::new())),
                    mentions_fetched: false,
                    tree: Vec::new(),
                    collapsed: HashSet::new(),
                    tree_scroll: UniformListScrollHandle::new(),
                    tree_last_file: None,
                });
                data.rebuild_tree();
                self.state = ItemState::Ready(data);
            }
        }
        snapshot
    }
}

fn dir_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

enum LoadedMeta {
    Pr(gh::PrMeta),
    Local(git::LocalSource),
}

struct Loaded {
    meta: LoadedMeta,
    diff: PrDiff,
    rows: Vec<Row>,
    file_rows: Vec<usize>,
    hunk_rows: Vec<usize>,
    mode: ViewMode,
    /// Some (possibly empty) for PR items, None for local ones.
    comments: Option<CommentIndex>,
    /// The snapshot of a PR's fetch, for the local store: what the app
    /// saw of the PR, and when. None for local items.
    snapshot: Option<local_store::StoredPrSnapshot>,
}

/// Blocking fetch + parse + row building for one item; runs on the background
/// executor, so subprocess waits and tree-sitter work stay off the main thread.
/// PR items fetch meta, patch, and review comments concurrently.
///
/// A PR's fetch also produces the snapshot the local store keeps: what the
/// app saw of the PR (its head SHA, its threads), so the next launch knows
/// what "since I last looked" means, and an offline launch can still read
/// the PR.
fn fetch_item(source: &Source, mode: ViewMode) -> anyhow::Result<Loaded> {
    let (meta, patch, comments, snapshot) = match source {
        Source::Pr(loc) => {
            let meta_loc = loc.clone();
            let meta_thread = std::thread::spawn(move || gh::fetch_meta(&meta_loc));
            let comments_loc = loc.clone();
            let comments_thread =
                std::thread::spawn(move || gh::fetch_review_comments(&comments_loc));
            let patch = gh::fetch_patch(loc)?;
            let meta = meta_thread
                .join()
                .map_err(|_| anyhow!("gh metadata fetch panicked"))??;
            let comments = comments_thread
                .join()
                .map_err(|_| anyhow!("gh comments fetch panicked"))??;
            let snapshot = pr_snapshot(loc, &meta, &comments);
            (
                LoadedMeta::Pr(meta),
                patch,
                Some(group_comments(comments)),
                Some(snapshot),
            )
        }
        Source::Local(src) => {
            let src = git::resolve_local_with_base(&src.repo_root, src.base_ref.as_deref())?;
            let patch = git::diff_patch(&src)?;
            (LoadedMeta::Local(src), patch, None, None)
        }
    };
    let diff = diff_core::parse_patch(&patch);
    let (rows, file_rows, hunk_rows) = build_rows(
        &diff,
        mode,
        &HashMap::new(),
        comments.as_ref(),
        true,
        COMMENT_WRAP_CHARS,
    );
    Ok(Loaded {
        meta,
        diff,
        rows,
        file_rows,
        hunk_rows,
        mode,
        comments,
        snapshot,
    })
}

/// The snapshot of a PR, as the local store keeps it: what the app saw
/// (its head SHA, its review threads) and when it saw it.
fn pr_snapshot(
    loc: &gh::PrLocator,
    meta: &gh::PrMeta,
    comments: &[gh::ReviewComment],
) -> local_store::StoredPrSnapshot {
    // A thread is a root comment plus its replies: the root's id is the
    // thread's id, and the thread's timestamp is its last comment's.
    let mut threads: Vec<local_store::StoredThreadSeen> = Vec::new();
    let mut replies: HashMap<u64, u64> = HashMap::new();
    for comment in comments {
        if let Some(parent) = comment.in_reply_to_id {
            replies.insert(comment.id, parent);
        }
    }
    for comment in comments {
        if comment.in_reply_to_id.is_some() {
            continue; // a reply, not a thread root
        }
        // The thread's last comment: the newest reply under this root, or
        // the root itself.
        let mut updated_at = comment.created_at.clone();
        for other in comments {
            let mut root = other.in_reply_to_id;
            let mut guard = 0;
            while let Some(parent) = root {
                if parent == comment.id {
                    if other.created_at > updated_at {
                        updated_at = other.created_at.clone();
                    }
                    break;
                }
                root = replies.get(&parent).copied();
                guard += 1;
                if guard > 32 {
                    break; // a reply cycle can't happen, but don't spin
                }
            }
        }
        threads.push(local_store::StoredThreadSeen {
            id: comment.id.to_string(),
            updated_at,
            resolved: false,
        });
    }
    local_store::StoredPrSnapshot {
        repository: loc.repo_slug(),
        number: meta.number,
        title: meta.title.clone(),
        body: meta.body.clone(),
        author: meta.author.login.clone(),
        head_sha: meta.head_ref_oid.clone(),
        base_ref: meta.base_ref_name.clone(),
        head_ref: meta.head_ref_name.clone(),
        threads,
        is_draft: false,
        fetched_at_ms: local_store::now_ms(),
    }
}

// --- Phase-2 full-content upgrade ---------------------------------------

/// Never upgrade more than this many files per item; the rest stay on the
/// perfectly serviceable patch-derived view.
const MAX_UPGRADE_FILES: usize = 400;
/// Per-side blob cap; gh::fetch_file_at enforces the same limit for PRs.
const MAX_UPGRADE_BLOB_BYTES: usize = 1024 * 1024;
/// gh/git are one subprocess per call, so a small worker pool is plenty.
const UPGRADE_WORKERS: usize = 4;

/// Where to fetch full file contents from, snapshotted when an item loads.
enum UpgradeSource {
    Pr {
        loc: gh::PrLocator,
        base_oid: String,
        head_oid: String,
    },
    Local(git::LocalSource),
}

/// One file's fetch inputs, snapshotted from the parsed patch (paths and
/// statuses are already known — no extra API call needed).
struct UpgradeJob {
    file_ix: usize,
    old_path: Option<String>,
    new_path: Option<String>,
    status: FileStatus,
}

/// One file's completed upgrade: authoritative hunks plus the render state.
struct UpgradedFile {
    file_ix: usize,
    hunks: Vec<Hunk>,
    additions: u32,
    deletions: u32,
    upgrade: FileUpgrade,
}

/// Blocking fetch + re-diff + whole-file highlight for every eligible file;
/// runs on the background executor. Files that fail on either side are simply
/// missing from the result and keep their patch-derived view.
fn run_upgrade(source: &UpgradeSource, mut jobs: Vec<UpgradeJob>) -> Vec<UpgradedFile> {
    if jobs.len() > MAX_UPGRADE_FILES {
        eprintln!(
            "gander: {} changed files; upgrading the first {MAX_UPGRADE_FILES} to full \
             contents, the rest stay patch-derived",
            jobs.len()
        );
        jobs.truncate(MAX_UPGRADE_FILES);
    }
    let queue = std::sync::Mutex::new(jobs.into_iter());
    let done = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..UPGRADE_WORKERS {
            scope.spawn(|| loop {
                let Some(job) = queue.lock().unwrap().next() else {
                    break;
                };
                if let Some(result) = upgrade_file(source, &job) {
                    done.lock().unwrap().push(result);
                }
            });
        }
    });
    let mut done = done.into_inner().unwrap();
    done.sort_by_key(|file| file.file_ix);
    done
}

/// One side's full contents, or None to leave the file un-upgraded: absent,
/// binary/non-UTF-8, over the size cap, or a fetch failure.
fn fetch_side(source: &UpgradeSource, path: &str, old: bool) -> Option<String> {
    let text = match source {
        UpgradeSource::Pr {
            loc,
            base_oid,
            head_oid,
        } => {
            let oid = if old { base_oid } else { head_oid };
            match gh::fetch_file_at(loc, oid, path) {
                Ok(text) => text?,
                Err(err) => {
                    eprintln!("gander: {path}: {err:#}");
                    return None;
                }
            }
        }
        UpgradeSource::Local(src) => {
            if old {
                git::file_at_base(src, path)?
            } else {
                String::from_utf8(std::fs::read(src.repo_root.join(path)).ok()?).ok()?
            }
        }
    };
    (text.len() <= MAX_UPGRADE_BLOB_BYTES).then_some(text)
}

fn upgrade_file(source: &UpgradeSource, job: &UpgradeJob) -> Option<UpgradedFile> {
    // Presence must match the patch's story: Added has no old side, Deleted no
    // new side, everything else needs both. A wanted-but-missing side (404,
    // binary, huge) keeps the whole file patch-derived.
    let old_text = match (job.status, &job.old_path) {
        (FileStatus::Added, _) => String::new(),
        (_, Some(path)) => fetch_side(source, path, true)?,
        (_, None) => return None,
    };
    let new_text = match (job.status, &job.new_path) {
        (FileStatus::Deleted, _) => String::new(),
        (_, Some(path)) => fetch_side(source, path, false)?,
        (_, None) => return None,
    };
    // Normalize CRLF→LF up front: hunks, span tables, and gap rows must all
    // index the same line/byte space (diff_texts itself is ending-agnostic).
    let normalize = |text: String| {
        if text.contains('\r') {
            text.replace("\r\n", "\n")
        } else {
            text
        }
    };
    let old_text = normalize(old_text);
    let new_text = normalize(new_text);

    let hunks = diff_texts(&old_text, &new_text, 3);
    let (additions, deletions) =
        hunks
            .iter()
            .flat_map(|hunk| &hunk.rows)
            .fold((0, 0), |(a, d), row| match row {
                DiffRow::Added { .. } => (a + 1, d),
                DiffRow::Removed { .. } => (a, d + 1),
                DiffRow::Context { .. } => (a, d),
            });

    let path = job.new_path.as_deref().or(job.old_path.as_deref())?;
    let lang = syntax::language_for_path(path);
    let spans = |text: &str| match lang {
        Some(lang) if !text.is_empty() => syntax::highlight_lines(lang, text),
        _ => Vec::new(),
    };
    Some(UpgradedFile {
        file_ix: job.file_ix,
        hunks,
        additions,
        deletions,
        upgrade: FileUpgrade {
            old_spans: spans(&old_text),
            new_spans: spans(&new_text),
            new_lines: new_text
                .lines()
                .map(|line| SharedString::from(line.to_string()))
                .collect(),
            expanded: HashSet::new(),
        },
    })
}

fn centered_message(text: SharedString, color: gpui::Rgba) -> gpui::AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .text_color(color)
        .child(text)
        .into_any_element()
}

fn app_title(detail: Option<String>) -> gpui::AnyElement {
    let mut title = div()
        .flex()
        .items_center()
        .gap_2()
        .flex_1()
        .min_w_0()
        .child(
            div()
                .font_weight(gpui::FontWeight::BOLD)
                .child(SharedString::from("Gander")),
        );
    if let Some(detail) = detail {
        title = title.child(
            div()
                .text_color(theme::subtext())
                .truncate()
                .child(SharedString::from(detail)),
        );
    }
    title.into_any_element()
}

fn pr_titlebar_content(meta: &gh::PrMeta, cx: &mut Context<ReviewApp>) -> gpui::AnyElement {
    let (state_color, state_label) = match meta.state.as_str() {
        "OPEN" => (theme::green(), "open"),
        "MERGED" => (theme::mauve(), "merged"),
        "CLOSED" => (theme::red(), "closed"),
        other => (theme::overlay0(), other),
    };
    let state: Hsla = state_color.into();
    // The PR's overall review decision, when it has one.
    let decision = match meta.review_decision.as_str() {
        "APPROVED" => Some((theme::green(), "approved")),
        "CHANGES_REQUESTED" => Some((theme::red(), "changes requested")),
        _ => None,
    };
    let url = meta.url.clone();
    div()
        .flex()
        .items_center()
        .flex_1()
        .min_w_0()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .min_w_0()
                .flex_1()
                .child(
                    Tag::custom(state.opacity(0.15), state, state.opacity(0.4))
                        .small()
                        .child(SharedString::from(state_label.to_string())),
                )
                .child(
                    div()
                        .font_weight(gpui::FontWeight::BOLD)
                        .truncate()
                        .child(SharedString::from(meta.title.clone())),
                )
                .child(
                    div()
                        .text_color(theme::subtext())
                        .child(SharedString::from(format!("#{}", meta.number))),
                )
                .child(
                    div()
                        .text_color(theme::subtext())
                        .whitespace_nowrap()
                        .child(SharedString::from(format!("by {}", meta.author.login))),
                )
                .when_some(decision, |row, (color, label)| {
                    let tint: Hsla = color.into();
                    row.child(
                        Tag::custom(tint.opacity(0.15), tint, tint.opacity(0.4))
                            .small()
                            .child(SharedString::from(label.to_string())),
                    )
                }),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .flex_shrink_0()
                .pr_3()
                .child(
                    div()
                        .text_color(theme::overlay0())
                        .child(SharedString::from(format!(
                            "{} ← {}",
                            meta.base_ref_name, meta.head_ref_name
                        ))),
                )
                .child(
                    div()
                        .text_color(theme::green())
                        .child(SharedString::from(format!("+{}", meta.additions))),
                )
                .child(
                    div()
                        .text_color(theme::red())
                        .child(SharedString::from(format!("−{}", meta.deletions))),
                )
                .child(
                    Button::new("open-in-browser")
                        .icon(IconName::ExternalLink)
                        .ghost()
                        .xsmall()
                        .on_click(move |_, _, cx| cx.open_url(&url)),
                )
                .when(meta.state == "OPEN", |row| {
                    row.child(
                        Button::new("submit-review")
                            .label("Review")
                            .primary()
                            .xsmall()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_review(window, cx);
                            })),
                    )
                }),
        )
        .into_any_element()
}

fn local_titlebar_content(
    item_id: u64,
    src: &git::LocalSource,
    data: &ItemData,
    cx: &mut Context<ReviewApp>,
) -> gpui::AnyElement {
    let blue: Hsla = theme::blue().into();
    let repo_root = src.repo_root.clone();
    div()
        .flex()
        .items_center()
        .flex_1()
        .min_w_0()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .min_w_0()
                .flex_1()
                .child(
                    Tag::custom(blue.opacity(0.15), blue, blue.opacity(0.4))
                        .small()
                        .child(SharedString::from("local")),
                )
                .child(
                    div()
                        .font_weight(gpui::FontWeight::BOLD)
                        .truncate()
                        .child(SharedString::from(dir_name(&src.repo_root))),
                ),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .flex_shrink_0()
                .pr_3()
                .child(
                    div()
                        .id("local-base-picker")
                        .px_1()
                        .rounded_sm()
                        .cursor_pointer()
                        .text_color(theme::overlay0())
                        .hover(|style| style.bg(Hsla::from(theme::surface0()).opacity(0.6)))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_local_base_palette(item_id, repo_root.clone(), window, cx);
                        }))
                        .child(SharedString::from(format!(
                            "{} ← {}",
                            src.base_label, src.branch
                        ))),
                )
                .child(
                    div()
                        .text_color(theme::green())
                        .child(SharedString::from(format!("+{}", data.additions))),
                )
                .child(
                    div()
                        .text_color(theme::red())
                        .child(SharedString::from(format!("−{}", data.deletions))),
                )
                .child(
                    Button::new("copy-local-review-prompt")
                        .label("Copy prompt")
                        .primary()
                        .xsmall()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.copy_local_review_prompt(cx);
                        })),
                ),
        )
        .into_any_element()
}

/// The cmd-k command palette: a staged flow for opening things.
enum PaletteStep {
    /// Step 1: pick what to open.
    Sources { selected: usize },
    /// Step 2 (GitHub path): type `owner/repo`.
    RepoInput { error: Option<SharedString> },
    /// Step 3 (GitHub path): pick a PR from the repo's open list.
    PrList { repo: String, prs: PrListState },
    /// Local path: pick the base ref for an already-open local item.
    LocalBaseList {
        item_id: u64,
        repo_root: PathBuf,
        current: String,
        bases: BaseListState,
    },
}

enum PrListState {
    Loading,
    Loaded {
        all: Vec<gh::PrSummary>,
        /// Indices into `all`, fuzzy-filtered by the query, best match first.
        filtered: Vec<usize>,
        /// Index into `filtered`.
        selected: usize,
    },
    Failed(String),
}

enum BaseListState {
    Loading,
    Loaded {
        all: Vec<String>,
        /// Indices into `all`, fuzzy-filtered by the query, best match first.
        filtered: Vec<usize>,
        /// Index into `filtered`.
        selected: usize,
    },
    Failed(String),
}

const PALETTE_SOURCES: [&str; 2] = ["Open GitHub pull request", "Open local folder"];
const SOURCE_PR: usize = 0;
const SOURCE_FOLDER: usize = 1;
const PALETTE_ROW_HEIGHT: f32 = 30.0;
/// Queue rows stay one line but a dense one — avatar, title, badges, size,
/// counts, chips — so they sit a little taller than the palette's, with
/// the uniform list keeping scroll math simple and rows virtualized.
const QUEUE_ROW_HEIGHT: f32 = 36.0;

/// Step-1 options matching the query (case-insensitive substring), as indices
/// into PALETTE_SOURCES. Empty query keeps both.
fn filtered_sources(query: &str) -> Vec<usize> {
    let q = query.trim().to_lowercase();
    PALETTE_SOURCES
        .iter()
        .enumerate()
        .filter(|(_, label)| q.is_empty() || label.to_lowercase().contains(&q))
        .map(|(ix, _)| ix)
        .collect()
}

/// Fuzzy-filter PRs against `#number title author branch`, best score first;
/// an empty query keeps gh's original (most recently updated) order.
fn filter_prs(all: &[gh::PrSummary], query: &str) -> Vec<usize> {
    let query = query.trim();
    if query.is_empty() {
        return (0..all.len()).collect();
    }
    let matcher = SkimMatcherV2::default();
    let mut scored: Vec<(i64, usize)> = all
        .iter()
        .enumerate()
        .filter_map(|(ix, pr)| {
            let haystack = format!(
                "#{} {} {} {}",
                pr.number, pr.title, pr.author.login, pr.head_ref_name
            );
            matcher
                .fuzzy_match(&haystack, query)
                .map(|score| (score, ix))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, ix)| ix).collect()
}

fn filter_base_refs(all: &[String], query: &str) -> Vec<usize> {
    let query = query.trim();
    if query.is_empty() {
        return (0..all.len()).collect();
    }
    let matcher = SkimMatcherV2::default();
    let mut scored: Vec<(i64, usize)> = all
        .iter()
        .enumerate()
        .filter_map(|(ix, base)| matcher.fuzzy_match(base, query).map(|score| (score, ix)))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, ix)| ix).collect()
}

/// One row of the palette's PR list: state dot, #number, title, author, head
/// branch. Clicking opens the PR just like enter does.
fn palette_pr_row(
    pr: &gh::PrSummary,
    pos: usize,
    selected: bool,
    entity: gpui::Entity<ReviewApp>,
) -> gpui::AnyElement {
    let dot = if pr.is_draft {
        theme::overlay0()
    } else {
        theme::green()
    };
    div()
        .id(("palette-pr", pos))
        .mx_1()
        .px_2()
        .h(px(PALETTE_ROW_HEIGHT))
        .rounded_md()
        .flex()
        .items_center()
        .gap_2()
        .cursor_pointer()
        .when(selected, |row| row.bg(theme::surface0()))
        .when(!selected, |row| {
            row.hover(|style| style.bg(Hsla::from(theme::surface0()).opacity(0.5)))
        })
        .on_click(move |_, window, cx| {
            entity.update(cx, |this, cx| this.palette_open_pr_row(pos, window, cx));
        })
        .child(
            div()
                .w(px(8.))
                .h(px(8.))
                .flex_shrink_0()
                .rounded_full()
                .bg(dot),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_color(theme::subtext())
                .child(SharedString::from(format!("#{}", pr.number))),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(theme::text())
                .child(SharedString::from(pr.title.clone())),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_color(theme::subtext())
                .child(SharedString::from(pr.author.login.clone())),
        )
        .child(
            div()
                .flex_shrink_0()
                .max_w(px(140.))
                .truncate()
                .text_color(theme::overlay0())
                .child(SharedString::from(pr.head_ref_name.clone())),
        )
        .into_any_element()
}

/// The CI dot's paint, per verdict: green opens without fear, red means
/// expect breakage, yellow means checks still running, gray means no
/// signal — no checks configured, or the enrichment couldn't see them.
fn ci_dot_color(ci: github::CiState) -> Rgba {
    match ci {
        github::CiState::Pass => theme::green(),
        github::CiState::Fail => theme::red(),
        github::CiState::Pending => theme::yellow(),
        github::CiState::NoChecks => theme::overlay0(),
    }
}

/// The review-state bit of a queue row: a green ✓ or red ✕ for the decided
/// states, plain muted text for the queue's default "awaiting" posture —
/// so the row never keeps quiet about where a PR stands.
fn review_state_view(state: github::ReviewState) -> gpui::AnyElement {
    let (icon, label, color) = match state {
        github::ReviewState::Approved => (Some(IconName::CircleCheck), "approved", theme::green()),
        github::ReviewState::ChangesRequested => {
            (Some(IconName::CircleX), "changes requested", theme::red())
        }
        github::ReviewState::Awaiting => (None, "awaiting review", theme::subtext()),
    };
    div()
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap_1()
        .when_some(icon, |state, icon| {
            state.child(Icon::new(icon).xsmall().text_color(color))
        })
        .child(div().text_size(px(12.)).text_color(color).child(label))
        .into_any_element()
}

/// The diff size at a glance — `+151 −30`, additions in green, deletions
/// in red, the "big vs small" triage signal. `None` until the enrichment
/// has seen the diff, and rendered not at all rather than as a fake zero.
fn size_view(additions: Option<u64>, deletions: Option<u64>) -> Option<gpui::AnyElement> {
    let (additions, deletions) = (additions?, deletions?);
    Some(
        div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_1()
            .text_size(px(12.))
            .child(
                div()
                    .text_color(theme::green())
                    .child(SharedString::from(format!("+{additions}"))),
            )
            .child(
                div()
                    .text_color(theme::red())
                    .child(SharedString::from(format!("−{deletions}"))),
            )
            .into_any_element(),
    )
}

/// The conversation count: a muted speech bubble beside the number, so
/// "has discussion happened here?" reads at a glance.
fn comment_count_view(comments: u64) -> impl IntoElement {
    div()
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap_1()
        .child(
            Icon::default()
                .path("icons/gander/comment-16.svg")
                .xsmall()
                .text_color(theme::overlay0()),
        )
        .child(
            div()
                .text_size(px(12.))
                .text_color(theme::subtext())
                .child(SharedString::from(comments.to_string())),
        )
}

/// GitHub's label colors are six hex digits without `#` (`"1D76DB"`).
/// Anything else degrades to `None` and the chip falls back to neutral
/// colors — a malformed color never breaks a row.
fn label_color(color: &str) -> Option<Hsla> {
    let channel = |text: &str| u8::from_str_radix(text, 16).ok();
    let bytes = color.as_bytes();
    if bytes.len() != 6 {
        return None;
    }
    let (r, g, b) = (
        channel(std::str::from_utf8(&bytes[0..2]).ok()?)?,
        channel(std::str::from_utf8(&bytes[2..4]).ok()?)?,
        channel(std::str::from_utf8(&bytes[4..6]).ok()?)?,
    );
    Some(Hsla::from(Rgba {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a: 1.0,
    }))
}

/// Mix `a` toward `b` by `t` (0 = `a`, 1 = `b`). Hue takes the short way
/// around the wheel; alpha lerps like the rest.
fn mix_hsla(a: Hsla, b: Hsla, t: f32) -> Hsla {
    let mut hue = b.h - a.h;
    if hue > 0.5 {
        hue -= 1.0;
    } else if hue < -0.5 {
        hue += 1.0;
    }
    Hsla {
        h: (a.h + hue * t).rem_euclid(1.0),
        s: a.s + (b.s - a.s) * t,
        l: a.l + (b.l - a.l) * t,
        a: a.a + (b.a - a.a) * t,
    }
}

/// A label chip's `(background, text, border)` from GitHub's label color,
/// tuned for the palette: a tinted fill with the color itself as text —
/// lifted toward white in dark mode, deepened toward black in light, so
/// GitHub's darker label colors stay readable on both. A missing color
/// degrades to neutral grays, never breaks the row.
fn label_chip_colors(color: Option<Hsla>, light: bool) -> (Hsla, Hsla, Hsla) {
    let Some(color) = color else {
        let bg = Hsla::from(theme::surface0()).opacity(0.4);
        let text = Hsla::from(theme::subtext());
        return (bg, text, Hsla::from(theme::surface0()));
    };
    let (text, toward) = if light {
        (
            mix_hsla(
                color,
                Hsla {
                    h: 0.0,
                    s: 0.0,
                    l: 0.0,
                    a: 1.0,
                },
                0.2,
            ),
            0.18,
        )
    } else {
        (
            mix_hsla(
                color,
                Hsla {
                    h: 0.0,
                    s: 0.0,
                    l: 1.0,
                    a: 1.0,
                },
                0.4,
            ),
            0.12,
        )
    };
    (
        color.opacity(toward),
        text,
        color.opacity(if light { 0.6 } else { 0.5 }),
    )
}

/// How many label chips a row shows before the rest collapse into a
/// muted `+n` — enough to recognize, not enough to crowd the row.
const QUEUE_LABELS_SHOWN: usize = 3;

/// The chips to paint plus how many more labels exist: the budget the row
/// gives labels before size and state start competing for space.
fn label_budget(labels: &[github::QueueLabel]) -> (&[github::QueueLabel], usize) {
    let shown = labels.len().min(QUEUE_LABELS_SHOWN);
    (&labels[..shown], labels.len() - shown)
}

/// How many chips one filter dimension shows before its `+n` overflow: the
/// chip bar stays a bounded strip over the queue however wide the day's
/// contexts spread, the way a row's label budget keeps room for size and
/// state. The active chip is pinned to the front of its group, so it can
/// never fall off the capped end.
const QUEUE_CHIPS_SHOWN: usize = 8;

/// One quick-filter chip over the queue: the filter clicking it applies,
/// how many rows it covers, and whether it's its dimension's active value.
/// The chip paints the qualifier it compiles to — the bar doubles as a
/// readout of the search the next refresh runs.
struct QueueChip {
    filter: github::QueueFilter,
    count: usize,
    active: bool,
}

/// One filter dimension's chips: the values present in the queue, ordered
/// for triage — the active value first, then the heaviest, then
/// alphabetically — capped with a count of the rest. A dimension with a
/// single value and nothing active stays off the bar entirely: there's
/// nothing to narrow, and an inert chip would only look broken.
struct QueueChipGroup {
    kind: github::QueueFilterKind,
    chips: Vec<QueueChip>,
    hidden: usize,
}

/// The chip groups for the queue as it stands: one per dimension — org,
/// repo, author — its values derived from the rows the queue is currently
/// showing, each carrying the rows it covers and whether it's active.
/// Deriving from the current rows is what keeps the bar honest: a filtered
/// queue offers the values it still contains — the active chip included,
/// so it stays visible and clearable — and an unfiltered one offers
/// everything. Two rules keep every chip meaningful: a dimension appears
/// only once it can narrow (two or more values, or one while it's the
/// active filter — its chip is the one-glance readout), and a malformed
/// repository row degrades honestly, its whole string standing in as the
/// org rather than panicking or vanishing.
fn queue_chips(prs: &[github::QueueItem], search: &github::QueueSearch) -> Vec<QueueChipGroup> {
    use github::QueueFilterKind::{Author, Org, Repo};
    [Org, Repo, Author]
        .into_iter()
        .filter_map(|kind| {
            let mut counts: std::collections::BTreeMap<&str, usize> =
                std::collections::BTreeMap::new();
            for pr in prs {
                let value = match kind {
                    Org => pr
                        .repository
                        .split_once('/')
                        .map(|(owner, _)| owner)
                        .unwrap_or(pr.repository.as_str()),
                    Repo => pr.repository.as_str(),
                    Author => pr.author.as_str(),
                };
                *counts.entry(value).or_insert(0) += 1;
            }
            let active = search.filters().iter().find(|f| f.kind() == kind);
            // One value and nothing active: the chip couldn't narrow
            // anything, so it stays off the bar.
            if counts.len() < 2 && active.is_none() {
                return None;
            }
            let mut values: Vec<(&str, usize)> = counts.into_iter().collect();
            values.sort_by(|(a_value, a_count), (b_value, b_count)| {
                // The active value always leads its group — the chip must
                // stay visible, whatever its weight.
                let a_active = active.is_some_and(|f| f.value() == *a_value);
                let b_active = active.is_some_and(|f| f.value() == *b_value);
                b_active
                    .cmp(&a_active)
                    .then(b_count.cmp(a_count))
                    .then(a_value.cmp(b_value))
            });
            let (shown, hidden) = values.split_at(values.len().min(QUEUE_CHIPS_SHOWN));
            Some(QueueChipGroup {
                kind,
                chips: shown
                    .iter()
                    .map(|(value, count)| QueueChip {
                        filter: match kind {
                            Org => github::QueueFilter::org(*value),
                            Repo => github::QueueFilter::repo(*value),
                            Author => github::QueueFilter::author(*value),
                        },
                        count: *count,
                        active: active.is_some_and(|f| f.value() == *value),
                    })
                    .collect(),
                hidden: hidden.len(),
            })
        })
        .collect()
}

/// One review-queue row, carrying everything needed to triage without
/// opening anything: CI verdict as a colored dot, author avatar, `#number`
/// and title (drafts badged and muted), review state, diff size,
/// conversation count, label chips, repository, and age. Clicking selects
/// the row (opening from the queue arrives with the PR screen).
fn queue_row(
    pr: &github::QueueItem,
    pos: usize,
    selected: bool,
    now: i64,
    entity: gpui::Entity<ReviewApp>,
) -> gpui::AnyElement {
    let (labels, overflow) = label_budget(&pr.labels);
    div()
        .id(("queue-row", pos))
        .mx_1()
        .px_3()
        .h(px(QUEUE_ROW_HEIGHT))
        .rounded_md()
        .flex()
        .items_center()
        .gap_2()
        .cursor_pointer()
        .when(selected, |row| row.bg(theme::surface0()))
        .when(!selected, |row| {
            row.hover(|style| style.bg(Hsla::from(theme::surface0()).opacity(0.5)))
        })
        .on_click(move |_, window, cx| {
            entity.update(cx, |this, cx| {
                // Focus lands on the pane so the keyboard keeps working —
                // j/k from a click still move the (new) selection.
                window.focus(&this.focus_handle);
                this.queue_open(pos, cx);
            });
        })
        // The CI verdict sits at the very edge: green rows open without
        // fear, red ones expect breakage — the first thing to catch the eye.
        .child(
            div()
                .flex_shrink_0()
                .size(px(8.))
                .rounded_full()
                .bg(ci_dot_color(pr.ci))
                .when(pr.ci == github::CiState::Pending, |dot| {
                    dot.border_1()
                        .border_color(theme::yellow())
                        .bg(Hsla::from(theme::yellow()).opacity(0.35))
                }),
        )
        .child(
            // `name` covers the slot the enrichment couldn't fill: the
            // author's initials on a tinted circle instead of a face.
            match &pr.author_avatar_url {
                Some(url) => Avatar::new().src(url.clone()),
                None => Avatar::new(),
            }
            .name(pr.author.clone())
            .xsmall(),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_color(theme::subtext())
                .child(SharedString::from(format!("#{}", pr.number))),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                // Drafts read muted with a badge beside them — visibly
                // not-ready while still fully triageable.
                .text_color(if pr.is_draft {
                    theme::subtext()
                } else {
                    theme::text()
                })
                .child(SharedString::from(pr.title.clone())),
        )
        .when(pr.is_draft, |row| {
            row.child(
                div()
                    .flex_shrink_0()
                    .px_1p5()
                    .text_size(px(10.))
                    .rounded_sm()
                    .bg(Hsla::from(theme::overlay0()).opacity(0.2))
                    .border_1()
                    .border_color(Hsla::from(theme::overlay0()).opacity(0.5))
                    .text_color(theme::subtext())
                    .child("Draft"),
            )
        })
        .child(review_state_view(pr.review_state))
        .when_some(size_view(pr.additions, pr.deletions), |row, size| {
            row.child(size)
        })
        .child(comment_count_view(pr.comments))
        .when(!labels.is_empty(), |row| {
            row.child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_1()
                    .max_w(px(360.))
                    .overflow_hidden()
                    .children(labels.iter().map(|label| {
                        let (bg, text, border) =
                            label_chip_colors(label_color(&label.color), theme::is_light());
                        Tag::custom(bg, text, border)
                            .xsmall()
                            .max_w(px(120.))
                            .truncate()
                            .child(SharedString::from(label.name.clone()))
                    }))
                    .when(overflow > 0, |chips| {
                        chips.child(
                            div()
                                .text_size(px(12.))
                                .text_color(theme::overlay0())
                                .child(SharedString::from(format!("+{overflow}"))),
                        )
                    }),
            )
        })
        .child(
            div()
                .flex_shrink_0()
                .max_w(px(220.))
                .truncate()
                .text_color(theme::blue())
                .child(SharedString::from(pr.repository.clone())),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_color(theme::overlay0())
                .child(SharedString::from(short_age(&pr.updated_at, now))),
        )
        .into_any_element()
}

fn parse_repo_slug(value: &str) -> Result<(String, String), &'static str> {
    let (owner, repo) = value.split_once('/').ok_or("expected owner/repo")?;
    if owner.is_empty() || repo.is_empty() || repo.contains('/') {
        return Err("expected owner/repo");
    }
    Ok((owner.to_string(), repo.to_string()))
}

// --- @-mention autocomplete ------------------------------------------------

/// Most completion items to offer at once.
const MENTION_LIMIT: usize = 50;

/// `@`-mention autocomplete for the comment composer, backed by the item's
/// shared, live-updating pool of mentionable users (seeded with PR
/// participants, then filled from the repo's mentionable set in the
/// background). Reads the pool fresh on every keystroke.
struct MentionProvider {
    users: Rc<RefCell<Vec<gh::Mention>>>,
}

/// If the cursor sits inside an `@mention` token, return the byte offset of the
/// `@` and the (possibly empty) login text typed after it. The `@` must begin a
/// word — preceded by whitespace or the start of the text — matching GitHub's
/// own mention rules, so `foo@bar` never triggers.
fn mention_prefix(text: &Rope, offset: usize) -> Option<(usize, String)> {
    let s = text.to_string();
    let offset = offset.min(s.len());
    let before = &s[..offset];
    // GitHub logins are alphanumeric plus hyphen; walk back over that run.
    let start = before
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_ascii_alphanumeric() || *c == '-')
        .last()
        .map(|(i, _)| i)
        .unwrap_or(offset);
    if start == 0 || before.as_bytes()[start - 1] != b'@' {
        return None;
    }
    let at = start - 1;
    if at > 0 && !before[..at].chars().next_back().unwrap().is_whitespace() {
        return None;
    }
    Some((at, before[start..offset].to_string()))
}

/// Rank of `user` against `query` (matched case-insensitively), lower = better;
/// None = no match. Login prefix beats name prefix beats substring matches.
fn mention_rank(user: &gh::Mention, query: &str) -> Option<u8> {
    if query.is_empty() {
        return Some(0);
    }
    let query = &query.to_ascii_lowercase();
    let login = user.login.to_ascii_lowercase();
    let name = user.name.as_ref().map(|n| n.to_ascii_lowercase());
    if login.starts_with(query) {
        Some(0)
    } else if name
        .as_deref()
        .is_some_and(|n| n.split_whitespace().any(|w| w.starts_with(query)))
    {
        Some(1)
    } else if login.contains(query) {
        Some(2)
    } else if name.as_deref().is_some_and(|n| n.contains(query)) {
        Some(3)
    } else {
        None
    }
}

impl CompletionProvider for MentionProvider {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        _trigger: lsp_types::CompletionContext,
        _window: &mut Window,
        _cx: &mut Context<InputState>,
    ) -> Task<anyhow::Result<lsp_types::CompletionResponse>> {
        let empty = Task::ready(Ok(lsp_types::CompletionResponse::Array(vec![])));
        let Some((at, prefix)) = mention_prefix(text, offset) else {
            return empty;
        };
        // The edit replaces `@prefix` (the token so far) with `@login `.
        let range = lsp_types::Range {
            start: text.offset_to_position(at),
            end: text.offset_to_position(offset),
        };
        let users = self.users.borrow();
        let mut ranked: Vec<(u8, &gh::Mention)> = users
            .iter()
            .filter_map(|u| mention_rank(u, &prefix).map(|r| (r, u)))
            .collect();
        // Stable sort keeps GitHub's alphabetical order within each rank.
        ranked.sort_by_key(|(rank, _)| *rank);
        let items = ranked
            .into_iter()
            .take(MENTION_LIMIT)
            .map(|(_, u)| lsp_types::CompletionItem {
                label: u.login.clone(),
                filter_text: Some(u.login.clone()),
                detail: u.name.clone(),
                text_edit: Some(lsp_types::CompletionTextEdit::Edit(lsp_types::TextEdit {
                    range,
                    new_text: format!("@{} ", u.login),
                })),
                ..Default::default()
            })
            .collect::<Vec<_>>();
        Task::ready(Ok(lsp_types::CompletionResponse::Array(items)))
    }

    fn is_completion_trigger(
        &self,
        _offset: usize,
        _new_text: &str,
        _cx: &mut Context<InputState>,
    ) -> bool {
        // Cheap to always run; `completions` returns nothing outside a mention.
        true
    }
}

/// Seed `cell` with everyone already visible on the PR — the author and every
/// comment author — so autocomplete has relevant names before the full
/// mentionable-user fetch returns. Additive: never drops fetched entries.
fn seed_mentions(cell: &Rc<RefCell<Vec<gh::Mention>>>, data: &ItemData) {
    let mut pool = cell.borrow_mut();
    let mut add = |login: &str| {
        if !login.is_empty() && !pool.iter().any(|m| m.login.eq_ignore_ascii_case(login)) {
            pool.push(gh::Mention {
                login: login.to_string(),
                name: None,
            });
        }
    };
    if let Some(meta) = &data.pr_meta {
        add(&meta.author.login);
    }
    if let Some(index) = &data.comments {
        for anchors in index.threads.values() {
            for threads in anchors.values() {
                for thread in threads {
                    add(&thread.root.user.login);
                    for reply in &thread.replies {
                        add(&reply.user.login);
                    }
                }
            }
        }
    }
}

/// The floating comment/reply composer. One at a time, targeting a specific
/// anchor of a specific item (it survives item switches but only renders —
/// and can only post — for the item it was opened on).
struct Composer {
    item_id: u64,
    /// Some(root comment id) = reply to that thread; None = new top-level
    /// comment at (path, side, line) against `commit_id`.
    reply_to: Option<u64>,
    commit_id: String,
    path: String,
    side: CommentSide,
    line: u64,
    /// Display row the composer is anchored beneath (best effort; goes stale
    /// harmlessly if rows rebuild while it is open).
    row_ix: usize,
    input: gpui::Entity<InputState>,
    error: Option<SharedString>,
    in_flight: bool,
    _subscription: Subscription,
}

/// The "submit review" modal: a verdict (approve / request changes /
/// comment) plus an optional body, targeting the item it was opened on.
struct ReviewDialog {
    item_id: u64,
    verdict: gh::ReviewVerdict,
    input: gpui::Entity<InputState>,
    error: Option<SharedString>,
    in_flight: bool,
    _subscription: Subscription,
}

fn local_review_prompt(src: &git::LocalSource, review: &LocalReview) -> String {
    let threads = local_review_threads(review)
        .iter()
        .map(format_local_thread_prompt)
        .collect::<Vec<_>>()
        .join("\n=====\n");
    format!(
        "Diff: {} ← {}. Locations are GitHub diff-style: path:Rline for right/new, path:Lline for left/old.\n\n{}",
        src.base_label, src.branch, threads
    )
}

fn local_review_threads(review: &LocalReview) -> Vec<CommentThread> {
    let mut comments = review.comments.clone();
    comments.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
    let mut threads = Vec::new();
    let mut thread_of: HashMap<u64, usize> = HashMap::new();
    for comment in comments {
        match comment.in_reply_to_id {
            None => {
                thread_of.insert(comment.id, threads.len());
                threads.push(CommentThread {
                    root: comment,
                    replies: Vec::new(),
                });
            }
            Some(parent) => {
                if let Some(&ix) = thread_of.get(&parent) {
                    thread_of.insert(comment.id, ix);
                    threads[ix].replies.push(comment);
                }
            }
        }
    }
    threads
}

fn format_local_thread_prompt(thread: &CommentThread) -> String {
    let file = if thread.root.path.is_empty() {
        "<unknown file>"
    } else {
        &thread.root.path
    };
    let mut sections = vec![
        local_comment_location(file, thread.root.side.as_deref(), thread.root.line),
        thread.root.body.clone(),
    ];
    for (ix, reply) in thread.replies.iter().enumerate() {
        let author = if reply.user.login.trim().is_empty() {
            "Unknown"
        } else {
            reply.user.login.as_str()
        };
        sections.push(format!("Reply {} ({author})", ix + 1));
        sections.push(reply.body.clone());
    }
    sections.join("\n")
}

fn local_comment_location(file: &str, side: Option<&str>, line: Option<u64>) -> String {
    let line = line.unwrap_or(0);
    match side {
        Some("LEFT") => format!("{file}:L{line}"),
        _ => format!("{file}:R{line}"),
    }
}

/// The GitHub backend the app runs on. v0.1 shells out to the `gh` CLI;
/// a v0.2 REST/GraphQL backend replaces this one constructor and nothing
/// else in the app — every caller goes through the facade trait.
fn github_client() -> Arc<dyn github::GitHubClient> {
    Arc::new(github::gh_cli::GhCliClient::new())
}

/// Where the account check stands. The signed-out case keeps the facade's
/// [`github::ClientError`]: its `Display` is the user-facing message, and it
/// already names the `gh auth login` fix.
enum AccountStatus {
    /// Check still running on a background thread; renders nothing yet.
    Checking,
    SignedIn(github::Viewer),
    SignedOut(github::ClientError),
}

/// The review queue screen: every open PR where the viewer's review is
/// requested (directly or via a team — the search qualifier covers both),
/// most recently updated first.
///
/// The queue is **cache-first**: the rows it paints come from the local
/// store when the app opens, and the network's answer hot-swaps in when
/// it lands. The rows on screen therefore carry their origin — cached or
/// fresh — which is what the status line says.
struct QueueState {
    /// The compiled search the queue runs. v1 always the review-requested
    /// preset, recently-updated first.
    search: github::QueueSearch,
    /// Queue rows in the search's order. Kept (stale) across refresh
    /// failures, so the screen stays useful offline.
    prs: Vec<github::QueueItem>,
    /// Where the rows on screen came from: the local cache (painted the
    /// instant the app opened) or the network (hot-swapped in when the
    /// fetch landed).
    origin: QueueOrigin,
    /// A fetch is in flight — the initial load or a refresh.
    loading: bool,
    /// The last fetch failure; kept while stale rows remain visible,
    /// cleared by the next success.
    error: Option<String>,
    /// The row the keyboard selection sits on, kept by position across
    /// refreshes.
    selected: usize,
    scroll: UniformListScrollHandle,
}

/// Where the rows on screen came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueueOrigin {
    /// Nothing has been painted yet: the app is opening, or the cache was
    /// empty.
    None,
    /// The rows came from the local cache — the app's first paint, and
    /// what stays on screen when the network is unreachable.
    Cached,
    /// The rows came from the network.
    Fresh,
}

impl QueueState {
    /// The queue as it stands before any fetch: the default search, empty,
    /// waiting on the app-open refresh.
    fn new() -> Self {
        Self {
            search: github::QueueSearch::review_requested(),
            prs: Vec::new(),
            origin: QueueOrigin::None,
            loading: false,
            error: None,
            selected: 0,
            scroll: UniformListScrollHandle::new(),
        }
    }
}

/// The selection after moving `delta` rows in a list of `len`: clamped at
/// both ends — j/k can never fall off the list — and an empty list has
/// nothing to select. `delta` 0 re-clamps in place (how a refresh keeps the
/// selection position when the queue shrinks).
fn move_selection(selected: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    (selected as isize + delta).clamp(0, len as isize - 1) as usize
}

struct ReviewApp {
    items: Vec<ReviewItem>,
    active: usize,
    sidebar_visible: bool,
    open_input: gpui::Entity<InputState>,
    open_error: Option<SharedString>,
    /// Fuzzy filter over the active item's file tree (`/` focuses it).
    tree_filter_input: gpui::Entity<InputState>,
    focus_handle: FocusHandle,
    next_id: u64,
    palette: Option<PaletteStep>,
    palette_input: gpui::Entity<InputState>,
    /// Bumped on every palette transition; an in-flight PR-list fetch only
    /// lands if the generation it captured is still current.
    palette_gen: u64,
    palette_scroll: UniformListScrollHandle,
    /// Where the current selection drag started (side locked at mouse-down);
    /// None when no drag is in progress.
    drag_anchor: Option<(SelSide, RowCol)>,
    /// `m` toggles the minimap column for every item.
    minimap_visible: bool,
    /// A minimap scrub drag is in progress (mouse went down on the minimap).
    minimap_scrub: bool,
    /// Advance width of one monospace cell at (MONO, text_size()), measured once.
    char_width: Option<Pixels>,
    /// Diff row + split half under the pointer where a hover "+" (new
    /// comment) affordance shows; None when the pointer isn't on a
    /// commentable line.
    hover_plus: Option<(usize, SelSide)>,
    composer: Option<Composer>,
    /// Bumped on every composer open/close; an in-flight post only reports
    /// back into the composer generation it was submitted from.
    composer_gen: u64,
    review: Option<ReviewDialog>,
    /// Bumped on every review-dialog open/close, same protocol as
    /// `composer_gen`.
    review_gen: u64,
    /// The GitHub facade every GitHub operation goes through.
    github: Arc<dyn github::GitHubClient>,
    /// The account check surfaced in the title bar (see `render_account`).
    account: AccountStatus,
    /// Bumped on every account re-check; an in-flight result only lands if
    /// the generation it captured is still current.
    account_gen: u64,
    /// The review queue — the screen the app opens on.
    queue: QueueState,
    /// Bumped on every queue fetch; an in-flight result only lands if the
    /// generation it captured is still current.
    queue_gen: u64,
    /// Whether the main pane shows the review queue (the home screen) or
    /// the open items. Launching bare shows the queue; launching with PRs
    /// or paths on the CLI shows those instead.
    show_queue: bool,
    /// The local store: the queue cache the app paints from on open, and
    /// the place fetched queues are written to.
    store: local_store::LocalStore,
    /// Why the store couldn't be opened, when it couldn't — the app says
    /// so rather than pretending the cache is there.
    store_error: Option<String>,
    /// Whether the window is frontmost. The background poll only runs
    /// while it is; a backgrounded app doesn't spend the user's data.
    frontmost: bool,
    /// Bumped on every background poll; an older poll stops when it sees
    /// a newer generation, so there is never more than one running.
    poll_gen: u64,
    _subscriptions: Vec<Subscription>,
}

/// The comment anchor of a display row: which (side, line) a new comment on
/// it targets. Unified rows anchor by kind (Removed → LEFT, else RIGHT);
/// split rows by the half under the pointer. Rows without line numbers and
/// absent cells yield None.
fn comment_anchor(rows: &[Row], row_ix: usize, side: SelSide) -> Option<(CommentSide, u64)> {
    match (rows.get(row_ix)?, side) {
        (
            Row::Line {
                kind: LineKind::Removed,
                old_no,
                ..
            },
            _,
        ) => Some((CommentSide::Left, (*old_no)? as u64)),
        (Row::Line { new_no, .. }, _) => Some((CommentSide::Right, (*new_no)? as u64)),
        (Row::SplitLine { left, .. }, SelSide::Left) => {
            Some((CommentSide::Left, left.as_ref()?.no as u64))
        }
        (Row::SplitLine { right, .. }, SelSide::Right) => {
            Some((CommentSide::Right, right.as_ref()?.no as u64))
        }
        _ => None,
    }
}

impl ReviewApp {
    fn new(
        sources: Vec<Source>,
        errors: Vec<String>,
        github: Arc<dyn github::GitHubClient>,
        store: local_store::LocalStore,
        store_error: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let open_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("owner/repo#123, PR URL, or path"));
        let palette_input = cx.new(|cx| InputState::new(window, cx));
        let tree_filter_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("filter files…"));
        let _subscriptions = vec![
            // The system appearance can flip while we run; re-apply the
            // palette (Mocha ↔ Latte) and repaint when it does.
            cx.observe_window_appearance(window, |_, window, cx| {
                theme::apply_ui_theme(Some(window), cx);
                cx.notify();
            }),
            // The window's key status: the app refreshes when it comes
            // back to the front, and the background poll only runs while
            // it is frontmost.
            cx.observe_window_activation(window, |this, window, cx| {
                this.set_frontmost(window.is_window_active(), cx);
            }),
            cx.subscribe_in(
                &open_input,
                window,
                |this, _, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        this.submit_open(window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &palette_input,
                window,
                |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::PressEnter { .. } => this.palette_confirm(window, cx),
                    InputEvent::Change => this.palette_query_changed(cx),
                    _ => {}
                },
            ),
            cx.subscribe_in(
                &tree_filter_input,
                window,
                |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::PressEnter { .. } => this.tree_filter_confirm(window, cx),
                    InputEvent::Change => {
                        // The match list changes shape; start it at the top.
                        if let Some(data) = this.active_data() {
                            data.tree_scroll.scroll_to_item(0, ScrollStrategy::Top);
                        }
                        cx.notify();
                    }
                    _ => {}
                },
            ),
        ];
        let mut this = Self {
            items: Vec::new(),
            active: 0,
            // The queue is the home screen now: with nothing opened the
            // sidebar starts hidden (⌘T/⌘K carry the open affordances, and
            // both open it); several sources or CLI errors still start it
            // open, the way they always did.
            sidebar_visible: !errors.is_empty() || sources.len() > 1,
            open_input,
            open_error: errors.first().cloned().map(SharedString::from),
            tree_filter_input,
            focus_handle: cx.focus_handle(),
            next_id: 0,
            palette: None,
            palette_input,
            palette_gen: 0,
            palette_scroll: UniformListScrollHandle::new(),
            drag_anchor: None,
            minimap_visible: true,
            minimap_scrub: false,
            char_width: None,
            hover_plus: None,
            composer: None,
            composer_gen: 0,
            review: None,
            review_gen: 0,
            github,
            account: AccountStatus::Checking,
            account_gen: 0,
            queue: QueueState::new(),
            queue_gen: 0,
            show_queue: sources.is_empty(),
            store,
            store_error,
            frontmost: true,
            poll_gen: 0,
            _subscriptions,
        };
        for source in sources {
            this.open_item(source, cx);
        }
        this.active = 0;
        // The queue's search comes back from the last run before the
        // cache is read: the cached rows belong to a search, and the app
        // must know which one before it paints them.
        this.queue.search = this.restore_queue_search();
        this.spawn_account_check(cx);
        // The queue paints from the cache before the network is asked at
        // all: the first frame already shows the last queue, and the
        // fetch that follows hot-swaps the fresh one in.
        this.paint_queue_from_cache();
        this.spawn_queue_refresh(cx);
        this.spawn_background_poll(cx);
        this
    }

    /// The window's key status changed: the app is frontmost, or it isn't
    /// any more.
    ///
    /// Coming back to the front is the moment the queue is most likely to
    /// have moved on, so the queue refreshes and the background poll
    /// starts again. Going away stops the poll: a backgrounded app
    /// doesn't spend the user's data.
    fn set_frontmost(&mut self, frontmost: bool, cx: &mut Context<Self>) {
        if frontmost == self.frontmost {
            return;
        }
        self.frontmost = frontmost;
        if frontmost {
            // Coming back to the front is the moment the queue is most
            // likely to have moved on: refresh it, and let the poll run
            // again (the new poll ends the old one).
            self.spawn_queue_refresh(cx);
            self.spawn_background_poll(cx);
        }
        cx.notify();
    }

    /// The gentle background poll.
    ///
    /// While the window is frontmost, the queue refreshes every
    /// [`BACKGROUND_POLL_INTERVAL`] — GitHub's search costs a couple of
    /// points, so a poll every few minutes is cheap and keeps the queue
    /// fresh without the reviewer thinking about it. The poll stops the
    /// moment the window loses the key: a backgrounded app doesn't spend
    /// the user's data, and the focus refresh covers the moment they come
    /// back.
    ///
    /// The generation guard ends the previous poll when a new one starts,
    /// so a window that comes back to the front has exactly one poll
    /// running — never two, never none.
    fn spawn_background_poll(&mut self, cx: &mut Context<Self>) {
        self.poll_gen += 1;
        let gen = self.poll_gen;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(BACKGROUND_POLL_INTERVAL)
                    .await;
                // A newer poll took over, or the window went to the
                // background: this poll is done.
                let keep_going = this
                    .read_with(cx, |app, _| app.poll_gen == gen && app.frontmost)
                    .unwrap_or(false);
                if !keep_going {
                    return;
                }
                this.update(cx, |app, cx| app.spawn_queue_refresh(cx)).ok();
            }
        })
        .detach();
    }

    /// Paint the queue from the local cache.
    ///
    /// This is the app's first paint: the rows on screen are the ones the
    /// last run saw, so the queue is there before the network is asked.
    /// The fetch that runs alongside it hot-swaps the fresh rows in when
    /// it lands.
    ///
    /// A cache from a *different* search isn't painted as this search's
    /// queue: those rows are another queue, and showing them under this
    /// search's chips would be a lie. The app paints what it has and
    /// says so.
    fn paint_queue_from_cache(&mut self) {
        let cached = self.store.cached_queue();
        if cached.prs.is_empty() {
            return;
        }
        let (sort, order) = self.queue.search.sort();
        let matches = cached.matches(&self.queue.search.query(), sort, order);
        self.queue.prs = self.store.cached_queue_items();
        // The origin stays `None` for a cache that belongs to another
        // search: the status line must not claim "cached" for a queue it
        // doesn't match.
        self.queue.origin = if matches {
            QueueOrigin::Cached
        } else {
            QueueOrigin::None
        };
        self.queue.selected = move_selection(self.queue.selected, self.queue.prs.len(), 0);
    }

    /// Ask the facade who we're signed in as — a `gh` subprocess in v0.1, so
    /// it runs on a background thread — and surface the result in the title
    /// bar. The generation guard keeps a stale check from overwriting a newer
    /// one after a re-check (`r`).
    fn spawn_account_check(&mut self, cx: &mut Context<Self>) {
        self.account_gen += 1;
        let gen = self.account_gen;
        let client = self.github.clone();
        cx.spawn(async move |this, cx| {
            let viewer = cx.background_spawn(async move { client.viewer() }).await;
            this.update(cx, |app, cx| {
                if app.account_gen != gen {
                    return;
                }
                app.account = match viewer {
                    Ok(viewer) => AccountStatus::SignedIn(viewer),
                    Err(err) => AccountStatus::SignedOut(err),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Run the queue search in the background and hot-swap the rows in.
    /// Failures are non-destructive: stale rows stay visible with the error
    /// surfaced beside them (see `render_queue`), and the next success
    /// clears it. The generation guard keeps a stale fetch from landing
    /// after a newer refresh started.
    ///
    /// A successful fetch also lands in the local store, so the *next*
    /// launch paints this queue before the network is asked at all.
    fn spawn_queue_refresh(&mut self, cx: &mut Context<Self>) {
        self.queue.loading = true;
        self.queue_gen += 1;
        let gen = self.queue_gen;
        let (client, search) = (self.github.clone(), self.queue.search.clone());
        cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_spawn(async move { client.search_prs(&search) })
                .await;
            this.update(cx, |app, cx| {
                if app.queue_gen != gen {
                    return;
                }
                app.queue.loading = false;
                match fetched {
                    Ok(prs) => {
                        // Keep the selection where it was, clamped into the
                        // fresh rows.
                        app.queue.selected = move_selection(app.queue.selected, prs.len(), 0);
                        app.queue.prs = prs;
                        app.queue.origin = QueueOrigin::Fresh;
                        app.queue.error = None;
                        // The fresh queue becomes the cache: the next
                        // launch paints it before the network answers. The
                        // write goes to the background executor — a disk
                        // write must never sit on the UI thread.
                        let store = app.store.clone();
                        let search = app.queue.search.clone();
                        let prs = app.queue.prs.clone();
                        cx.background_spawn(async move {
                            store.cache_queue(&search, &prs);
                        })
                        .detach();
                    }
                    Err(err) => app.queue.error = Some(err.to_string()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// `j`/`k`: move the queue selection by `delta` rows and keep the
    /// selected row visible. No-op outside the queue screen.
    fn queue_move(&mut self, delta: isize, cx: &mut Context<Self>) {
        if !self.show_queue || self.queue.prs.is_empty() {
            return;
        }
        self.queue.selected = move_selection(self.queue.selected, self.queue.prs.len(), delta);
        self.queue
            .scroll
            .scroll_to_item(self.queue.selected, ScrollStrategy::Top);
        cx.notify();
    }

    /// The locator a queue row opens: its repository is `owner/name`, so
    /// the row carries everything the PR-open path needs. A row without a
    /// well-formed repository degrades to nothing — Enter on it is a no-op,
    /// the same honesty the chip derivation shows a malformed slug.
    fn queue_item_locator(pr: &github::QueueItem) -> Option<gh::PrLocator> {
        let (owner, repo) = pr.repository.split_once('/')?;
        Some(gh::PrLocator {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number: pr.number,
        })
    }

    /// Enter (or a click) on a queue row: that PR opens into the changes
    /// view — its own tab, fresh (fetch + phase-2 upgrade) or straight
    /// from the snapshot when offline — with the sidebar's file tree
    /// showing. A PR that's already open behind the scenes just comes
    /// forward ([`ReviewApp::open_item`]'s one-item-per-source rule), so
    /// re-opening never loses your place in the first copy.
    fn queue_open(&mut self, pos: usize, cx: &mut Context<Self>) {
        if !self.show_queue {
            return;
        }
        let Some(pr) = self.queue.prs.get(pos) else {
            return;
        };
        let Some(loc) = Self::queue_item_locator(pr) else {
            return;
        };
        self.queue.selected = pos;
        self.open_item(Source::Pr(loc), cx);
        // The changes view's navigation lives in the sidebar: the file
        // tree (and the open items above it) must be there when the PR
        // lands, even though the queue starts with the sidebar hidden.
        self.sidebar_visible = true;
        cx.notify();
    }

    /// `u`: back to the queue. The open item stays open — ⌘K and
    /// ctrl-tab come right back to it — and the queue keeps the
    /// selection and scroll exactly where they were left.
    fn back_to_queue(&mut self, cx: &mut Context<Self>) {
        if self.show_queue {
            return;
        }
        self.show_queue = true;
        cx.notify();
    }

    /// `s` and the sort toggle: flip the queue between recently-updated
    /// and oldest-first. The order runs server-side — the compiled search
    /// carries it — so the rows hot-swap on the next fetch, and the
    /// selection starts over: a position in one order means nothing in the
    /// other.
    fn queue_toggle_sort(&mut self, cx: &mut Context<Self>) {
        if !self.show_queue {
            return;
        }
        let flipped = match self.queue.search.ordering() {
            github::QueueSort::RecentlyUpdated => github::QueueSort::OldestUpdated,
            github::QueueSort::OldestUpdated => github::QueueSort::RecentlyUpdated,
        };
        self.queue.search = self.queue.search.clone().sorted_by(flipped);
        self.persist_queue_search();
        self.queue_run_search(cx);
    }

    /// A chip click: selecting a value narrows the queue to it; selecting
    /// the active chip lifts that dimension again — the one-glance rule
    /// runs both ways, and clearing is never more than one click.
    fn queue_toggle_filter(&mut self, filter: github::QueueFilter, cx: &mut Context<Self>) {
        if !self.show_queue {
            return;
        }
        self.queue.search = if self.queue.search.filters().contains(&filter) {
            self.queue.search.clone().without_filter(filter.kind())
        } else {
            self.queue.search.clone().filtered_by(filter)
        };
        self.persist_queue_search();
        self.queue_run_search(cx);
    }

    /// `escape` and the clear chip: lift every chip at once — the preset
    /// queue again, in whatever order it was in.
    fn queue_clear_filters(&mut self, cx: &mut Context<Self>) {
        if !self.show_queue || self.queue.search.filters().is_empty() {
            return;
        }
        self.queue.search = self.queue.search.clone().unfiltered();
        self.persist_queue_search();
        self.queue_run_search(cx);
    }

    /// Remember the queue's search — the chips and the sort — so the next
    /// launch opens the same queue the last one did.
    ///
    /// The parts are stored, not the compiled string: a rebuilt search is
    /// the same search, and the parts survive a change in how the query
    /// compiles.
    fn persist_queue_search(&self) {
        let filters: Vec<serde_json::Value> = self
            .queue
            .search
            .filters()
            .iter()
            .map(|filter| {
                serde_json::json!({
                    "kind": filter.kind().prefix(),
                    "value": filter.value(),
                })
            })
            .collect();
        let sort = match self.queue.search.ordering() {
            github::QueueSort::RecentlyUpdated => "recent",
            github::QueueSort::OldestUpdated => "oldest",
        };
        self.store.set_setting(
            QUEUE_SEARCH_SETTING,
            serde_json::json!({ "filters": filters, "sort": sort }),
        );
    }

    /// The queue's search as it was last remembered: the chips and the
    /// sort, rebuilt into a search. A setting this build doesn't
    /// understand (or a corrupt one) reads as the default queue.
    fn restore_queue_search(&self) -> github::QueueSearch {
        restore_queue_search(self.store.setting(QUEUE_SEARCH_SETTING))
    }

    /// Run the queue's (already-changed) search from the top: the rows
    /// that come back belong to a different result set, so the selection
    /// restarts at the first row and the fetch hot-swaps in as ever — the
    /// stale rows stay visible until it lands.
    fn queue_run_search(&mut self, cx: &mut Context<Self>) {
        self.queue.selected = 0;
        self.queue.scroll.scroll_to_item(0, ScrollStrategy::Top);
        self.spawn_queue_refresh(cx);
        cx.notify();
    }

    /// Re-wrap comment bodies to the pane's current width. Called each render:
    /// the bounds come from the last paint, so a resize (or toggling the
    /// sidebar or view mode) settles on the following frame. Rebuilding
    /// only when the column count actually changes keeps this off the hot path
    /// — a few pixels of drag usually map to the same width in columns.
    fn resync_comment_wrap(&mut self, window: &Window) {
        let char_width = f32::from(self.char_width(window));
        let Some(data) = self.active_data_mut() else {
            return;
        };
        // Nothing to re-wrap without visible threads.
        let has_threads = data.comments_visible
            && data
                .comments
                .as_ref()
                .is_some_and(|index| !index.threads.is_empty());
        let list_width = f32::from(data.scroll.0.borrow().base_handle.bounds().size.width);
        if list_width <= 0. {
            return; // Not painted yet; keep the default until it is.
        }
        let wrap = comment_wrap_cols(list_width, data.mode, char_width);
        if wrap == data.comment_wrap {
            return;
        }
        data.comment_wrap = wrap;
        if has_threads {
            data.rebuild_rows_anchored();
        }
    }

    /// Step the diff font size (cmd-+ / cmd-- / cmd-0). Row height follows the
    /// font, so every item's scroll offset is rescaled to keep the same line at
    /// the top — otherwise the pixel offset would silently mean a different row.
    fn zoom(&mut self, delta: f32, reset: bool, cx: &mut Context<Self>) {
        let old_rh = row_height();
        let next = if reset {
            DEFAULT_TEXT_SIZE
        } else {
            (text_size() + delta).clamp(MIN_TEXT_SIZE, MAX_TEXT_SIZE)
        };
        if next == text_size() {
            return; // Already at the bound; nothing to redraw.
        }
        FONT_PX.store(next as u32, Ordering::Relaxed);
        let new_rh = row_height();
        // The cached advance width was measured at the old size.
        self.char_width = None;
        for item in &mut self.items {
            let ItemState::Ready(data) = &mut item.state else {
                continue;
            };
            let offset = data.scroll.0.borrow().base_handle.offset();
            let top_row = (-f32::from(offset.y) / old_rh).max(0.);
            data.scroll
                .0
                .borrow()
                .base_handle
                .set_offset(point(offset.x, px(-(top_row * new_rh))));
            // Minimap geometry is height-derived; drop the memoized layout.
            data.minimap_cache.replace(None);
        }
        cx.notify();
    }

    fn active_item(&self) -> Option<&ReviewItem> {
        self.items.get(self.active)
    }

    fn active_data(&self) -> Option<&ItemData> {
        match &self.items.get(self.active)?.state {
            ItemState::Ready(data) => Some(data),
            _ => None,
        }
    }

    fn active_data_mut(&mut self) -> Option<&mut ItemData> {
        match &mut self.items.get_mut(self.active)?.state {
            ItemState::Ready(data) => Some(data),
            _ => None,
        }
    }

    /// Advance width of one monospace cell, measured once via the text system
    /// (Menlo is monospace, so 'm' stands in for every glyph).
    fn char_width(&mut self, window: &Window) -> Pixels {
        *self.char_width.get_or_insert_with(|| {
            let text_system = window.text_system();
            let font_id = text_system.resolve_font(&font(MONO));
            text_system
                .em_advance(font_id, px(text_size()))
                .unwrap_or(px(text_size() * 0.6))
        })
    }

    /// Window position → (side, row/col) in the active diff. Row from the
    /// uniform_list's scroll offset and painted bounds (both kept fresh each
    /// frame on the tracked scroll handle); col from monospace arithmetic.
    /// `locked` pins a split drag to the side where it started. Pure mouse
    /// math — verified manually, not unit-tested.
    fn pane_hit(
        &self,
        position: Point<Pixels>,
        char_width: Pixels,
        locked: Option<SelSide>,
    ) -> Option<(SelSide, RowCol)> {
        let (side, row, text_x) = self.pane_text_hit(position, locked)?;
        let col = (f32::from(text_x) / f32::from(char_width)).round().max(0.) as usize;
        Some((side, RowCol { row, col }))
    }

    fn pane_text_hit(
        &self,
        position: Point<Pixels>,
        locked: Option<SelSide>,
    ) -> Option<(SelSide, usize, Pixels)> {
        let data = self.active_data()?;
        if data.rows.is_empty() {
            return None;
        }
        let (bounds, offset) = {
            let state = data.scroll.0.borrow();
            (state.base_handle.bounds(), state.base_handle.offset())
        };
        // offset.y is negative when scrolled down.
        let y = f32::from(position.y - bounds.top() - offset.y);
        let row = ((y / row_height()).floor().max(0.) as usize).min(data.rows.len() - 1);
        let rel_x = f32::from(position.x - bounds.left());
        let (side, text_x) = match data.mode {
            // offset.x is negative when scrolled right (unified mode only;
            // split uses FitList and never scrolls horizontally).
            ViewMode::Unified => (
                SelSide::Unified,
                rel_x - f32::from(offset.x) - UNIFIED_GUTTER,
            ),
            ViewMode::Split => {
                let half = (f32::from(bounds.size.width) - SPLIT_DIVIDER) / 2.;
                let side = locked.unwrap_or(if rel_x < half + SPLIT_DIVIDER / 2. {
                    SelSide::Left
                } else {
                    SelSide::Right
                });
                let cell_x = match side {
                    SelSide::Right => rel_x - half - SPLIT_DIVIDER,
                    _ => rel_x,
                };
                (side, cell_x - SPLIT_GUTTER)
            }
        };
        Some((side, row, px(text_x)))
    }

    fn open_item(&mut self, source: Source, cx: &mut Context<Self>) {
        // The session keeps one item per source: opening something it
        // already has brings that item forward — scroll, cursor, view mode
        // and comments exactly where they were left — instead of a second
        // copy that would be a way to lose your place in the first. Every
        // open path (queue Enter, palette, ⌘T, the CLI) shares this.
        if let Some(ix) = self
            .items
            .iter()
            .position(|item| sources_match(&item.source, &source))
        {
            self.active = ix;
            // Opening something leaves the home screen for the item's pane.
            self.show_queue = false;
            cx.notify();
            return;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.items.push(ReviewItem {
            id,
            source: source.clone(),
            state: ItemState::Loading,
            reloading: false,
            refresh_error: None,
            upgrade_gen: 0,
        });
        self.active = self.items.len() - 1;
        // Opening something leaves the home screen for the item's pane.
        self.show_queue = false;
        Self::spawn_fetch(id, source, ViewMode::Split, cx);
        cx.notify();
    }

    fn spawn_fetch(id: u64, source: Source, mode: ViewMode, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_spawn(async move { fetch_item(&source, mode) })
                .await;
            this.update(cx, |app, cx| {
                let Some(item) = app.items.iter_mut().find(|item| item.id == id) else {
                    return;
                };
                item.reloading = false;
                match fetched {
                    Ok(loaded) => {
                        let snapshot = item.install(loaded);
                        // The PR's snapshot lands in the local store: the
                        // next launch knows what the app last saw of it
                        // (its head SHA, its threads), and an offline
                        // launch can still read it.
                        if let Some(snapshot) = snapshot {
                            app.store.save_snapshot(&snapshot);
                        }
                        // Phase 2: after the instant patch-derived paint,
                        // upgrade every eligible file to full contents in the
                        // background. The bumped generation cancels any
                        // still-running upgrade from before a refresh.
                        item.upgrade_gen += 1;
                        let gen = item.upgrade_gen;
                        if let ItemState::Ready(data) = &item.state {
                            let jobs: Vec<UpgradeJob> = data
                                .diff
                                .files
                                .iter()
                                .enumerate()
                                .filter(|(_, f)| {
                                    f.status != FileStatus::Binary && !f.hunks.is_empty()
                                })
                                .map(|(ix, f)| UpgradeJob {
                                    file_ix: ix,
                                    old_path: f.old_path.clone(),
                                    new_path: f.new_path.clone(),
                                    status: f.status,
                                })
                                .collect();
                            let source = match &item.source {
                                Source::Pr(loc) => data
                                    .pr_meta
                                    .as_ref()
                                    .filter(|meta| {
                                        !meta.base_ref_oid.is_empty()
                                            && !meta.head_ref_oid.is_empty()
                                    })
                                    .map(|meta| UpgradeSource::Pr {
                                        loc: loc.clone(),
                                        base_oid: meta.base_ref_oid.clone(),
                                        head_oid: meta.head_ref_oid.clone(),
                                    }),
                                Source::Local(src) => Some(UpgradeSource::Local(src.clone())),
                            };
                            if let (Some(source), false) = (source, jobs.is_empty()) {
                                Self::spawn_upgrade(id, gen, source, jobs, cx);
                            }
                        }
                    }
                    Err(err) => {
                        let msg = format!("{err:#}");
                        match &item.state {
                            // Refresh failure: keep the stale-but-useful data.
                            ItemState::Ready(_) => item.refresh_error = Some(msg.into()),
                            _ => {
                                // A PR that can't be fetched may still be
                                // readable from the local store: the app
                                // opens it from the snapshot it kept, and
                                // says the data is a cached one.
                                let offline = item.install_offline_snapshot(&app.store);
                                if offline {
                                    item.refresh_error =
                                        Some("offline — showing the last saved copy".into());
                                } else {
                                    item.state = ItemState::Failed(msg);
                                }
                            }
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Phase 2, per item: fetch every file's full contents, re-diff, and
    /// re-highlight in the background, then rebuild rows once and swap
    /// atomically. Scroll keeps its pixel offset (the re-diff normally
    /// reproduces the patch's hunks, so drift is small); the selection is
    /// cleared because row indices shift.
    fn spawn_upgrade(
        id: u64,
        gen: u64,
        source: UpgradeSource,
        jobs: Vec<UpgradeJob>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let upgraded = cx
                .background_spawn(async move { run_upgrade(&source, jobs) })
                .await;
            if upgraded.is_empty() {
                return;
            }
            this.update(cx, |app, cx| {
                let Some(item) = app.items.iter_mut().find(|item| item.id == id) else {
                    return;
                };
                if item.upgrade_gen != gen {
                    return;
                }
                let ItemState::Ready(data) = &mut item.state else {
                    return;
                };
                for file in upgraded {
                    let Some(target) = data.diff.files.get_mut(file.file_ix) else {
                        continue;
                    };
                    target.hunks = file.hunks;
                    target.additions = file.additions;
                    target.deletions = file.deletions;
                    data.upgrades.insert(file.file_ix, file.upgrade);
                }
                (data.additions, data.deletions) = data
                    .diff
                    .files
                    .iter()
                    .fold((0, 0), |(a, d), f| (a + f.additions, d + f.deletions));
                // Comment anchors live in the same absolute line-number space
                // the re-diff produces, so threads re-insert at the re-diffed
                // rows without translation.
                data.set_rows(build_rows(
                    &data.diff,
                    data.mode,
                    &data.upgrades,
                    data.comments.as_ref(),
                    data.comments_visible,
                    data.comment_wrap,
                ));
                data.cursor = data.cursor.min(data.rows.len().saturating_sub(1));
                data.selection = None;
                // Paths can't change in an upgrade, but stats did; rebuilding
                // keeps the tree in lockstep with every row rebuild.
                data.rebuild_tree();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Reveal all hidden lines of one gap in an upgraded file, keeping the
    /// viewport stable: when the expansion happens above the visible top row,
    /// the scroll offset shifts down by exactly the inserted height.
    fn expand_gap(&mut self, file_ix: usize, gap_ix: usize, cx: &mut Context<Self>) {
        let Some(data) = self.active_data_mut() else {
            return;
        };
        let Some(upgrade) = data.upgrades.get_mut(&file_ix) else {
            return;
        };
        if !upgrade.expanded.insert(gap_ix) {
            return;
        }
        let gap_row = data.rows.iter().position(|row| {
            matches!(row, Row::Gap { file_ix: f, gap_ix: g, .. } if *f == file_ix && *g == gap_ix)
        });
        let old_len = data.rows.len();
        data.set_rows(build_rows(
            &data.diff,
            data.mode,
            &data.upgrades,
            data.comments.as_ref(),
            data.comments_visible,
            data.comment_wrap,
        ));
        // The gap row is replaced by its hidden context rows (plus any
        // comment threads anchored inside them): the row-count delta is
        // exactly what got inserted.
        let inserted = data.rows.len().saturating_sub(old_len);
        data.selection = None;
        if let Some(gap_row) = gap_row {
            if data.cursor > gap_row {
                data.cursor += inserted;
            }
            let scroll = data.scroll.0.borrow();
            let offset = scroll.base_handle.offset();
            // offset.y is negative when scrolled down.
            let top_row = (f32::from(-offset.y) / row_height()).floor() as usize;
            if gap_row < top_row {
                scroll.base_handle.set_offset(point(
                    offset.x,
                    offset.y - px(inserted as f32 * row_height()),
                ));
            }
        }
        cx.notify();
    }

    fn submit_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.open_input.read(cx).value().trim().to_string();
        if value.is_empty() {
            return;
        }
        let parsed = if Path::new(&value).is_dir() {
            git::resolve_local(Path::new(&value)).map(Source::Local)
        } else {
            gh::resolve_pr_arg(&value).map(Source::Pr)
        };
        match parsed {
            Ok(source) => {
                self.open_error = None;
                self.open_input
                    .update(cx, |state, cx| state.set_value("", window, cx));
                self.open_item(source, cx);
                window.focus(&self.focus_handle);
            }
            Err(err) => self.open_error = Some(format!("{err:#}").into()),
        }
        cx.notify();
    }

    /// Reset the palette's shared input for the step being entered and keep
    /// keyboard focus in it (the palette traps focus while open).
    fn set_palette_input(
        &mut self,
        value: &str,
        placeholder: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.palette_input.update(cx, |state, cx| {
            state.set_value(value.to_string(), window, cx);
            state.set_placeholder(placeholder, window, cx);
            state.focus(window, cx);
        });
    }

    fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette = Some(PaletteStep::Sources { selected: 0 });
        self.palette_gen += 1;
        self.set_palette_input("", "type to filter…", window, cx);
        cx.notify();
    }

    fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette = None;
        self.palette_gen += 1;
        window.focus(&self.focus_handle);
        cx.notify();
    }

    /// Esc: one step back, or close from step 1.
    fn palette_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.palette {
            None | Some(PaletteStep::Sources { .. }) => self.close_palette(window, cx),
            Some(PaletteStep::RepoInput { .. }) => {
                self.palette = Some(PaletteStep::Sources { selected: 0 });
                self.palette_gen += 1;
                self.set_palette_input("", "type to filter…", window, cx);
                cx.notify();
            }
            Some(PaletteStep::PrList { repo, .. }) => {
                let repo = repo.clone();
                self.palette = Some(PaletteStep::RepoInput { error: None });
                self.palette_gen += 1;
                self.set_palette_input(&repo, "owner/repo", window, cx);
                cx.notify();
            }
            Some(PaletteStep::LocalBaseList { .. }) => self.close_palette(window, cx),
        }
    }

    fn palette_move(&mut self, delta: isize, cx: &mut Context<Self>) {
        let query = self.palette_input.read(cx).value().to_string();
        match &mut self.palette {
            Some(PaletteStep::Sources { selected }) => {
                let len = filtered_sources(&query).len();
                if len > 0 {
                    *selected = (*selected as isize + delta).clamp(0, len as isize - 1) as usize;
                }
            }
            Some(PaletteStep::PrList {
                prs:
                    PrListState::Loaded {
                        filtered, selected, ..
                    },
                ..
            }) => {
                if !filtered.is_empty() {
                    *selected =
                        (*selected as isize + delta).clamp(0, filtered.len() as isize - 1) as usize;
                    self.palette_scroll
                        .scroll_to_item(*selected, ScrollStrategy::Top);
                }
            }
            Some(PaletteStep::LocalBaseList {
                bases:
                    BaseListState::Loaded {
                        filtered, selected, ..
                    },
                ..
            }) => {
                if !filtered.is_empty() {
                    *selected =
                        (*selected as isize + delta).clamp(0, filtered.len() as isize - 1) as usize;
                    self.palette_scroll
                        .scroll_to_item(*selected, ScrollStrategy::Top);
                }
            }
            _ => return,
        }
        cx.notify();
    }

    fn palette_query_changed(&mut self, cx: &mut Context<Self>) {
        let query = self.palette_input.read(cx).value().to_string();
        match &mut self.palette {
            Some(PaletteStep::Sources { selected }) => {
                let len = filtered_sources(&query).len();
                *selected = (*selected).min(len.saturating_sub(1));
            }
            Some(PaletteStep::RepoInput { error }) => *error = None,
            Some(PaletteStep::PrList {
                prs:
                    PrListState::Loaded {
                        all,
                        filtered,
                        selected,
                    },
                ..
            }) => {
                *filtered = filter_prs(all, &query);
                *selected = 0;
                self.palette_scroll.scroll_to_item(0, ScrollStrategy::Top);
            }
            Some(PaletteStep::LocalBaseList {
                bases:
                    BaseListState::Loaded {
                        all,
                        filtered,
                        selected,
                    },
                ..
            }) => {
                *filtered = filter_base_refs(all, &query);
                *selected = 0;
                self.palette_scroll.scroll_to_item(0, ScrollStrategy::Top);
            }
            _ => return,
        }
        cx.notify();
    }

    /// Enter, on whichever step is showing.
    fn palette_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.palette_input.read(cx).value().trim().to_string();
        match &self.palette {
            Some(PaletteStep::Sources { selected }) => {
                if let Some(&opt) = filtered_sources(&query).get(*selected) {
                    self.palette_activate_source(opt, window, cx);
                }
            }
            Some(PaletteStep::RepoInput { .. }) => match parse_repo_slug(&query) {
                Ok((owner, repo)) => self.palette_fetch_prs(owner, repo, window, cx),
                Err(msg) => {
                    if let Some(PaletteStep::RepoInput { error }) = &mut self.palette {
                        *error = Some(msg.into());
                        cx.notify();
                    }
                }
            },
            Some(PaletteStep::PrList {
                prs: PrListState::Loaded { selected, .. },
                ..
            }) => {
                let selected = *selected;
                self.palette_open_pr_row(selected, window, cx);
            }
            Some(PaletteStep::LocalBaseList {
                bases: BaseListState::Loaded { selected, .. },
                ..
            }) => {
                let selected = *selected;
                self.palette_open_base_row(selected, window, cx);
            }
            _ => {}
        }
    }

    fn palette_activate_source(&mut self, opt: usize, window: &mut Window, cx: &mut Context<Self>) {
        match opt {
            SOURCE_PR => {
                self.palette = Some(PaletteStep::RepoInput { error: None });
                self.palette_gen += 1;
                self.set_palette_input("", "owner/repo", window, cx);
                cx.notify();
            }
            SOURCE_FOLDER => {
                // The palette closes when the native dialog opens.
                self.close_palette(window, cx);
                self.prompt_open_folder(cx);
            }
            _ => {}
        }
    }

    /// Step 2 → step 3: show "loading…" and fetch the open-PR list on the
    /// background executor. The generation guard drops the result if the user
    /// closed the palette or navigated away before it landed.
    fn palette_fetch_prs(
        &mut self,
        owner: String,
        repo: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.palette_gen += 1;
        let gen = self.palette_gen;
        self.palette = Some(PaletteStep::PrList {
            repo: format!("{owner}/{repo}"),
            prs: PrListState::Loading,
        });
        self.set_palette_input("", "search pull requests…", window, cx);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_spawn(async move { gh::list_prs(&owner, &repo) })
                .await;
            this.update(cx, |app, cx| {
                if app.palette_gen != gen {
                    return;
                }
                let query = app.palette_input.read(cx).value().to_string();
                let Some(PaletteStep::PrList { prs, .. }) = &mut app.palette else {
                    return;
                };
                *prs = match fetched {
                    Ok(all) => {
                        let filtered = filter_prs(&all, &query);
                        PrListState::Loaded {
                            all,
                            filtered,
                            selected: 0,
                        }
                    }
                    Err(err) => PrListState::Failed(format!("{err:#}")),
                };
                app.palette_scroll.scroll_to_item(0, ScrollStrategy::Top);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn open_local_base_palette(
        &mut self,
        item_id: u64,
        repo_root: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self
            .items
            .iter()
            .find(|item| item.id == item_id)
            .and_then(|item| match &item.source {
                Source::Local(src) => Some(src.base_label.clone()),
                Source::Pr(_) => None,
            })
            .unwrap_or_else(|| "HEAD".to_string());
        self.palette_gen += 1;
        let gen = self.palette_gen;
        self.palette = Some(PaletteStep::LocalBaseList {
            item_id,
            repo_root: repo_root.clone(),
            current,
            bases: BaseListState::Loading,
        });
        self.set_palette_input("", "search base refs…", window, cx);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_spawn(async move { git::list_base_refs(&repo_root) })
                .await;
            this.update(cx, |app, cx| {
                if app.palette_gen != gen {
                    return;
                }
                let query = app.palette_input.read(cx).value().to_string();
                let Some(PaletteStep::LocalBaseList { bases, .. }) = &mut app.palette else {
                    return;
                };
                *bases = match fetched {
                    Ok(all) => {
                        let filtered = filter_base_refs(&all, &query);
                        BaseListState::Loaded {
                            all,
                            filtered,
                            selected: 0,
                        }
                    }
                    Err(err) => BaseListState::Failed(format!("{err:#}")),
                };
                app.palette_scroll.scroll_to_item(0, ScrollStrategy::Top);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Open the PR at `pos` in the filtered list as a new review item.
    fn palette_open_pr_row(&mut self, pos: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(PaletteStep::PrList {
            repo,
            prs: PrListState::Loaded { all, filtered, .. },
        }) = &self.palette
        else {
            return;
        };
        let Some(&ix) = filtered.get(pos) else {
            return;
        };
        let Some((owner, repo)) = repo.split_once('/') else {
            return;
        };
        let source = Source::Pr(gh::PrLocator {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number: all[ix].number,
        });
        self.close_palette(window, cx);
        self.open_item(source, cx);
    }

    fn palette_open_base_row(&mut self, pos: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(PaletteStep::LocalBaseList {
            item_id,
            repo_root,
            bases: BaseListState::Loaded { all, filtered, .. },
            ..
        }) = &self.palette
        else {
            return;
        };
        let Some(&ix) = filtered.get(pos) else {
            return;
        };
        let (item_id, repo_root, base_ref) = (*item_id, repo_root.clone(), all[ix].clone());
        self.close_palette(window, cx);
        let Some(item) = self.items.iter_mut().find(|item| item.id == item_id) else {
            return;
        };
        let mode = match &item.state {
            ItemState::Ready(data) => data.mode,
            _ => ViewMode::Split,
        };
        let branch = match &item.source {
            Source::Local(src) => src.branch.clone(),
            Source::Pr(_) => dir_name(&repo_root),
        };
        let source = Source::Local(git::LocalSource {
            repo_root,
            branch,
            base_ref: Some(base_ref.clone()),
            base_label: base_ref,
            base_oid: None,
        });
        item.source = source.clone();
        item.refresh_error = None;
        match item.state {
            ItemState::Failed(_) => item.state = ItemState::Loading,
            ItemState::Loading => {}
            ItemState::Ready(_) => item.reloading = true,
        }
        Self::spawn_fetch(item_id, source, mode, cx);
        cx.notify();
    }

    /// Native directory picker. The chosen path becomes a Local item through
    /// the normal add-item path; fetch_item re-resolves it on the background
    /// executor, so a non-repo directory surfaces as a Failed item.
    fn prompt_open_folder(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            this.update(cx, |app, cx| {
                let source = Source::Local(git::LocalSource {
                    branch: dir_name(&path),
                    base_ref: None,
                    base_label: "…".to_string(),
                    base_oid: None,
                    repo_root: path,
                });
                app.open_item(source, cx);
            })
            .ok();
        })
        .detach();
    }

    fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix < self.items.len() {
            self.active = ix;
            self.show_queue = false;
            window.focus(&self.focus_handle);
            cx.notify();
        }
    }

    fn close_item(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.items.len() {
            return;
        }
        self.items.remove(ix);
        if self.active > ix || self.active >= self.items.len() {
            self.active = self.active.saturating_sub(1);
        }
        // Closing the last item comes home to the queue.
        if self.items.is_empty() {
            self.active = 0;
            self.show_queue = true;
        }
        cx.notify();
    }

    fn cycle_items(&mut self, delta: isize, cx: &mut Context<Self>) {
        let len = self.items.len() as isize;
        if len > 0 {
            self.active = (self.active as isize + delta).rem_euclid(len) as usize;
            self.show_queue = false;
            cx.notify();
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        // `r`/⌘R re-check the account and re-run the queue search (the home
        // screen), alongside the visible item: run `gh auth login` in a
        // terminal, press `r` here, and both pick the sign-in up without a
        // relaunch.
        self.spawn_account_check(cx);
        self.spawn_queue_refresh(cx);
        if self.show_queue {
            cx.notify();
            return;
        }
        let Some(item) = self.items.get_mut(self.active) else {
            return;
        };
        if matches!(item.state, ItemState::Loading) || item.reloading {
            return;
        }
        let mode = match &item.state {
            ItemState::Ready(data) => data.mode,
            _ => ViewMode::Split,
        };
        match item.state {
            ItemState::Failed(_) => item.state = ItemState::Loading,
            _ => item.reloading = true,
        }
        item.refresh_error = None;
        let (id, source) = (item.id, item.source.clone());
        Self::spawn_fetch(id, source, mode, cx);
        cx.notify();
    }

    fn toggle_view(&mut self, cx: &mut Context<Self>) {
        let Some(data) = self.active_data_mut() else {
            return;
        };
        // Best-effort position preservation: stay on the same file.
        let file_pos = data.file_rows.iter().rposition(|&ix| ix <= data.cursor);
        data.selection = None;
        data.mode = match data.mode {
            ViewMode::Unified => ViewMode::Split,
            ViewMode::Split => ViewMode::Unified,
        };
        data.set_rows(build_rows(
            &data.diff,
            data.mode,
            &data.upgrades,
            data.comments.as_ref(),
            data.comments_visible,
            data.comment_wrap,
        ));
        let target = file_pos
            .and_then(|pos| data.file_rows.get(pos).copied())
            .unwrap_or(0);
        self.jump(target, cx);
    }

    fn jump(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(data) = self.active_data_mut() {
            data.cursor = ix;
            data.scroll.scroll_to_item_strict(ix, ScrollStrategy::Top);
        }
        cx.notify();
    }

    fn jump_next(&mut self, targets: &[usize], cx: &mut Context<Self>) {
        let Some(cursor) = self.active_data().map(|data| data.cursor) else {
            return;
        };
        if let Some(&ix) = targets.iter().find(|&&ix| ix > cursor) {
            self.jump(ix, cx);
        }
    }

    /// Jump the diff to `file_ix`'s header row and hand focus to the diff
    /// pane (like clicking a sidebar item does).
    fn jump_to_file(&mut self, file_ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(&row) = self
            .active_data()
            .and_then(|data| data.file_rows.get(file_ix))
        else {
            return;
        };
        window.focus(&self.focus_handle);
        self.jump(row, cx);
    }

    /// Click on an unfiltered tree row: directories toggle collapse, files
    /// jump the diff.
    fn tree_entry_clicked(&mut self, entry_ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(data) = self.active_data_mut() else {
            return;
        };
        match data.tree.get(entry_ix).map(|entry| &entry.kind) {
            Some(TreeEntryKind::Dir { path }) => {
                let path = path.clone();
                if !data.collapsed.remove(&path) {
                    data.collapsed.insert(path);
                }
                cx.notify();
            }
            Some(&TreeEntryKind::File { file_ix }) => self.jump_to_file(file_ix, window, cx),
            None => {}
        }
    }

    /// Enter in the tree filter: jump to the best match and return focus to
    /// the diff (the filter stays, like GitHub's tree).
    fn tree_filter_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.tree_filter_input.read(cx).value().trim().to_string();
        let Some(data) = self.active_data() else {
            return;
        };
        if query.is_empty() {
            return;
        }
        let paths: Vec<&str> = data.diff.files.iter().map(|f| f.display_path()).collect();
        if let Some(file_ix) = fuzzy_file_matches(&paths, &query).into_iter().next() {
            self.jump_to_file(file_ix, window, cx);
        }
    }

    fn jump_prev(&mut self, targets: &[usize], cx: &mut Context<Self>) {
        let Some(cursor) = self.active_data().map(|data| data.cursor) else {
            return;
        };
        if let Some(&ix) = targets.iter().rev().find(|&&ix| ix < cursor) {
            self.jump(ix, cx);
        }
    }

    /// Scrub the diff to the row under a minimap mouse position: invert the
    /// minimap scale (downsample-aware) to a fractional row, then center it
    /// by setting the scroll offset directly. Pure mouse math — verified
    /// manually, like the selection hit test.
    fn minimap_scrub_to(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(data) = self.active_data() else {
            return;
        };
        let total = data.rows.len();
        if total == 0 {
            return;
        }
        let (bounds, offset) = {
            let state = data.scroll.0.borrow();
            (state.base_handle.bounds(), state.base_handle.offset())
        };
        let pane_h = f32::from(bounds.size.height);
        if pane_h <= 0. {
            return;
        }
        // The minimap column is the same height as the list, so its y space
        // starts at the list's top.
        let (slot_h, group) = minimap_scale(total, pane_h);
        let px_per_row = slot_h / group as f32;
        let y = f32::from(position.y - bounds.top());
        let row = (y / px_per_row).clamp(0., (total - 1) as f32);
        let target = row * row_height() - (pane_h - row_height()) / 2.;
        let max_scroll = (total as f32 * row_height() - pane_h).max(0.);
        data.scroll
            .0
            .borrow()
            .base_handle
            .set_offset(point(offset.x, px(-target.clamp(0., max_scroll))));
        cx.notify();
    }

    /// `c`: show/hide the comment rows of the active item, keeping the
    /// viewport anchored at the first visible diff row.
    fn toggle_comments(&mut self, cx: &mut Context<Self>) {
        let Some(data) = self.active_data_mut() else {
            return;
        };
        if data.comments.is_none() {
            return;
        }
        data.comments_visible = !data.comments_visible;
        data.rebuild_rows_anchored();
        cx.notify();
    }

    /// The row + split half under the pointer if it can take a new comment:
    /// the pointer is inside the diff list, and the row has a line number on
    /// that side. PR items also need a known head oid for GitHub anchoring.
    fn hover_target(
        &mut self,
        position: Point<Pixels>,
        window: &Window,
    ) -> Option<(usize, SelSide)> {
        if self.palette.is_some() {
            return None;
        }
        let char_width = self.char_width(window);
        let item = self.active_item()?;
        let ItemState::Ready(data) = &item.state else {
            return None;
        };
        // New comments post against the head oid; without one (older gh
        // missing headRefOid) the affordance stays off entirely.
        if matches!(&item.source, Source::Pr(_)) {
            if data
                .pr_meta
                .as_ref()
                .is_none_or(|meta| meta.head_ref_oid.is_empty())
            {
                return None;
            }
        }
        let bounds = data.scroll.0.borrow().base_handle.bounds();
        if !bounds.contains(&position) {
            return None;
        }
        let (side, hit) = self.pane_hit(position, char_width, None)?;
        let data = self.active_data()?;
        comment_anchor(&data.rows, hit.row, side).map(|_| (hit.row, side))
    }

    fn open_composer(
        &mut self,
        reply_to: Option<u64>,
        path: String,
        side: CommentSide,
        line: u64,
        row_ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = self.items.get(self.active) else {
            return;
        };
        let item_id = item.id;
        let pr_loc = match &item.source {
            Source::Pr(loc) => Some(loc.clone()),
            Source::Local(_) => None,
        };
        let commit_id = self
            .active_data()
            .and_then(|data| data.pr_meta.as_ref())
            .map(|meta| meta.head_ref_oid.clone())
            .unwrap_or_default();
        if reply_to.is_none() {
            match &item.source {
                Source::Pr(_) if commit_id.is_empty() => return,
                Source::Pr(_) | Source::Local(_) => {}
            }
        }
        self.composer_gen += 1;
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .auto_grow(3, 8)
                .placeholder("leave a comment…")
        });
        // cmd-enter: the input's own `secondary-enter` binding emits
        // PressEnter { secondary: true } (after inserting a newline, which
        // submit trims away).
        let _subscription =
            cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { secondary: true }) {
                    this.submit_composer(window, cx);
                }
            });
        input.update(cx, |state, cx| state.focus(window, cx));
        // Wire up @-mention autocomplete for PR items: seed the pool with known
        // participants now, attach the provider, then fetch the full list once.
        if let Some(loc) = pr_loc {
            if let Some(data) = self.active_data() {
                let mentions = data.mentions.clone();
                seed_mentions(&mentions, data);
                input.update(cx, |state, _| {
                    state.lsp.completion_provider =
                        Some(Rc::new(MentionProvider { users: mentions }));
                });
            }
            self.ensure_mentions(item_id, loc, cx);
        }
        self.composer = Some(Composer {
            item_id,
            reply_to,
            commit_id,
            path,
            side,
            line,
            row_ix,
            input,
            error: None,
            in_flight: false,
            _subscription,
        });
        cx.notify();
    }

    /// Fetch the repo's mentionable users once per item, merging them into the
    /// item's shared mention pool (which an open composer's completion provider
    /// reads live). Best-effort: on failure the seeded participants remain.
    fn ensure_mentions(&mut self, item_id: u64, loc: gh::PrLocator, cx: &mut Context<Self>) {
        let Some(data) = self.active_data_mut() else {
            return;
        };
        if data.mentions_fetched {
            return;
        }
        data.mentions_fetched = true;
        cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_spawn(async move { gh::fetch_mentionable_users(&loc) })
                .await;
            let Ok(users) = fetched else {
                return;
            };
            this.update(cx, |app, cx| {
                let Some(item) = app.items.iter().find(|item| item.id == item_id) else {
                    return;
                };
                let ItemState::Ready(data) = &item.state else {
                    return;
                };
                let mut pool = data.mentions.borrow_mut();
                for user in users {
                    match pool
                        .iter_mut()
                        .find(|m| m.login.eq_ignore_ascii_case(&user.login))
                    {
                        // Upgrade a seeded (name-less) participant in place.
                        Some(existing) if existing.name.is_none() => existing.name = user.name,
                        Some(_) => {}
                        None => pool.push(user),
                    }
                }
                drop(pool);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn close_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.composer.take().is_some() {
            self.composer_gen += 1;
            window.focus(&self.focus_handle);
            cx.notify();
        }
    }

    /// Submit the composer's comment (or reply): local items update in-memory
    /// drafts immediately; PR items post via gh on the background executor.
    fn submit_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(composer) = &self.composer else {
            return;
        };
        if composer.in_flight {
            return;
        }
        let body = composer.input.read(cx).value().trim().to_string();
        if body.is_empty() {
            return;
        }
        let Some(item) = self.items.iter().find(|item| item.id == composer.item_id) else {
            return;
        };
        let item_id = item.id;
        let source = item.source.clone();
        let gen = self.composer_gen;
        let (reply_to, commit_id, path, side, line) = {
            let composer = self.composer.as_mut().unwrap();
            composer.in_flight = true;
            composer.error = None;
            (
                composer.reply_to,
                composer.commit_id.clone(),
                composer.path.clone(),
                composer.side,
                composer.line,
            )
        };
        if let Source::Local(_) = source {
            self.add_local_comment(item_id, reply_to, path, side, line, body, cx);
            if self.composer_gen == gen {
                self.close_composer(window, cx);
            }
            return;
        }
        let Source::Pr(loc) = source else {
            return;
        };
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let post_loc = loc.clone();
            let result = cx
                .background_spawn(async move {
                    match reply_to {
                        Some(root_id) => gh::post_reply(&post_loc, root_id, &body),
                        None => gh::post_review_comment(
                            &post_loc,
                            &commit_id,
                            &path,
                            side.api_str(),
                            line,
                            &body,
                        ),
                    }
                })
                .await;
            this.update_in(cx, |app, window, cx| {
                if app.composer_gen != gen {
                    return; // The composer was closed or retargeted meanwhile.
                }
                match result {
                    Ok(()) => {
                        app.close_composer(window, cx);
                        app.refetch_comments(item_id, loc, cx);
                    }
                    Err(err) => {
                        if let Some(composer) = &mut app.composer {
                            composer.in_flight = false;
                            composer.error = Some(format!("{err:#}").into());
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn add_local_comment(
        &mut self,
        item_id: u64,
        reply_to: Option<u64>,
        path: String,
        side: CommentSide,
        line: u64,
        body: String,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = self.items.iter_mut().find(|item| item.id == item_id) else {
            return;
        };
        let ItemState::Ready(data) = &mut item.state else {
            return;
        };
        let Some(local) = &mut data.local_review else {
            return;
        };
        local.add_comment(reply_to, path, side, line, body);
        data.comments = Some(local.index());
        data.rebuild_rows_anchored();
        cx.notify();
    }

    fn copy_local_review_prompt(&self, cx: &mut Context<Self>) {
        let Some(item) = self.active_item() else {
            return;
        };
        let Source::Local(src) = &item.source else {
            return;
        };
        let ItemState::Ready(data) = &item.state else {
            return;
        };
        let Some(local) = &data.local_review else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(local_review_prompt(src, local)));
    }

    /// Refetch only the review comments (not meta/patch), regroup, and
    /// rebuild the rows with the viewport anchored — the counterpart of a
    /// full refresh for the post-comment path.
    fn refetch_comments(&mut self, item_id: u64, loc: gh::PrLocator, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_spawn(async move { gh::fetch_review_comments(&loc) })
                .await;
            this.update(cx, |app, cx| {
                let Some(item) = app.items.iter_mut().find(|item| item.id == item_id) else {
                    return;
                };
                let ItemState::Ready(data) = &mut item.state else {
                    return;
                };
                match fetched {
                    Ok(comments) => {
                        data.comments = Some(group_comments(comments));
                        data.rebuild_rows_anchored();
                    }
                    Err(err) => {
                        item.refresh_error =
                            Some(format!("comment refresh failed: {err:#}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Open the "submit review" modal for the active item (PR items only).
    fn open_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.active_item() else {
            return;
        };
        let Source::Pr(_) = item.source else {
            return;
        };
        let ItemState::Ready(_) = item.state else {
            return;
        };
        let item_id = item.id;
        self.review_gen += 1;
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .auto_grow(3, 8)
                .placeholder("leave a review comment… (optional when approving)")
        });
        let _subscription =
            cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { secondary: true }) {
                    this.submit_review(window, cx);
                }
            });
        input.update(cx, |state, cx| state.focus(window, cx));
        self.review = Some(ReviewDialog {
            item_id,
            verdict: gh::ReviewVerdict::Approve,
            input,
            error: None,
            in_flight: false,
            _subscription,
        });
        cx.notify();
    }

    fn close_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.review.take().is_some() {
            self.review_gen += 1;
            window.focus(&self.focus_handle);
            cx.notify();
        }
    }

    /// Submit the review via gh on the background executor. Success closes
    /// the dialog and refetches the PR meta (so the titlebar's decision tag
    /// updates); failure surfaces gh's stderr inline in the dialog.
    fn submit_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(review) = &self.review else {
            return;
        };
        if review.in_flight {
            return;
        }
        let body = review.input.read(cx).value().trim().to_string();
        let verdict = review.verdict;
        // GitHub rejects bodyless request-changes/comment reviews; fail
        // locally with a clearer message.
        if body.is_empty() && verdict != gh::ReviewVerdict::Approve {
            if let Some(review) = &mut self.review {
                review.error = Some("this review type needs a comment".into());
            }
            cx.notify();
            return;
        }
        let Some(item) = self.items.iter().find(|item| item.id == review.item_id) else {
            return;
        };
        let Source::Pr(loc) = &item.source else {
            return;
        };
        let loc = loc.clone();
        let item_id = item.id;
        let gen = self.review_gen;
        if let Some(review) = &mut self.review {
            review.in_flight = true;
            review.error = None;
        }
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let submit_loc = loc.clone();
            let result = cx
                .background_spawn(async move { gh::submit_review(&submit_loc, verdict, &body) })
                .await;
            this.update_in(cx, |app, window, cx| {
                if app.review_gen != gen {
                    return; // The dialog was closed and reopened meanwhile.
                }
                match result {
                    Ok(()) => {
                        app.close_review(window, cx);
                        app.refetch_meta(item_id, loc, cx);
                    }
                    Err(err) => {
                        if let Some(review) = &mut app.review {
                            review.in_flight = false;
                            review.error = Some(format!("{err:#}").into());
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Refetch only the PR meta — the post-review counterpart of
    /// `refetch_comments`, so the titlebar reflects the new review decision
    /// without reloading the whole diff.
    fn refetch_meta(&mut self, item_id: u64, loc: gh::PrLocator, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_spawn(async move { gh::fetch_meta(&loc) })
                .await;
            this.update(cx, |app, cx| {
                let Some(item) = app.items.iter_mut().find(|item| item.id == item_id) else {
                    return;
                };
                let ItemState::Ready(data) = &mut item.state else {
                    return;
                };
                match fetched {
                    Ok(meta) => data.pr_meta = Some(meta),
                    Err(err) => {
                        item.refresh_error = Some(format!("meta refresh failed: {err:#}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The hover "+" affordance: a small blue box at the far left of the
    /// hovered line (its half, in split mode), absolutely positioned over the
    /// list like the minimap viewport. Clicking opens the composer for that
    /// row's anchor.
    fn render_plus(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let (row_ix, side) = self.hover_plus?;
        if self.palette.is_some() {
            return None;
        }
        let item = self.active_item()?;
        let ItemState::Ready(data) = &item.state else {
            return None;
        };
        if matches!(&item.source, Source::Pr(_)) {
            if data
                .pr_meta
                .as_ref()
                .is_none_or(|meta| meta.head_ref_oid.is_empty())
            {
                return None;
            }
        }
        let (anchor_side, line) = comment_anchor(&data.rows, row_ix, side)?;
        let file_ix = data
            .file_rows
            .iter()
            .rposition(|&header| header <= row_ix)?;
        let path = data.diff.files[file_ix].display_path().to_string();
        let (bounds, offset) = {
            let state = data.scroll.0.borrow();
            (state.base_handle.bounds(), state.base_handle.offset())
        };
        // Pane-relative y of the hovered row; skip when scrolled out of view.
        let y = row_ix as f32 * row_height() + f32::from(offset.y);
        if y < 0. || y + row_height() > f32::from(bounds.size.height) {
            return None;
        }
        let x = match (data.mode, side) {
            (ViewMode::Split, SelSide::Right) => {
                (f32::from(bounds.size.width) - SPLIT_DIVIDER) / 2. + SPLIT_DIVIDER + 2.
            }
            _ => 2.,
        };
        let entity = cx.entity();
        Some(
            div()
                .absolute()
                .left(px(x))
                .top(px(y + (row_height() - 16.) / 2.))
                .w(px(16.))
                .h(px(16.))
                .rounded_sm()
                .bg(theme::blue())
                .flex()
                .items_center()
                .justify_center()
                .text_color(gpui::white())
                .text_size(px(13.))
                .cursor_pointer()
                .child(SharedString::from("+"))
                .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                    cx.stop_propagation();
                    let path = path.clone();
                    entity.update(cx, |this, cx| {
                        this.open_composer(None, path, anchor_side, line, row_ix, window, cx);
                    });
                })
                .into_any_element(),
        )
    }

    /// The floating composer card: absolutely positioned at root level (so
    /// its input sits outside the "ReviewApp" key context and plain letters
    /// stay text), anchored near the target line's y, clamped into the pane.
    fn render_composer(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let empty = || div().into_any_element();
        let Some(composer) = &self.composer else {
            return empty();
        };
        // Only render for the item it was opened on, and only while active.
        if self.items.get(self.active).map(|item| item.id) != Some(composer.item_id) {
            return empty();
        }
        let Some(data) = self.active_data() else {
            return empty();
        };
        let (bounds, offset) = {
            let state = data.scroll.0.borrow();
            (state.base_handle.bounds(), state.base_handle.offset())
        };
        let pane_h = f32::from(bounds.size.height);
        let row_y = composer.row_ix as f32 * row_height() + f32::from(offset.y);
        let y = f32::from(bounds.top()) + (row_y + row_height()).clamp(8., (pane_h - 250.).max(8.));
        let x = (f32::from(bounds.left()) + 72.).min((f32::from(bounds.right()) - 528.).max(8.));
        let action = if composer.reply_to.is_some() {
            "Reply"
        } else {
            "Comment"
        };
        let target = format!(
            "{}{}:{} ({})",
            if composer.reply_to.is_some() {
                "reply · "
            } else {
                ""
            },
            composer.path,
            composer.line,
            composer.side.api_str()
        );
        div()
            .absolute()
            .left(px(x))
            .top(px(y))
            .w(px(520.))
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            // The input propagates Escape when it has nothing of its own to
            // dismiss; catch it here (before the root's handler) to cancel.
            .on_action(cx.listener(|this, _: &InputEscape, window, cx| {
                this.close_composer(window, cx);
            }))
            .rounded_lg()
            .border_1()
            .border_color(theme::surface0())
            .bg(theme::mantle())
            .shadow_lg()
            .p_2()
            .flex()
            .flex_col()
            .gap_2()
            .text_size(px(12.))
            .child(
                div()
                    .text_color(theme::overlay0())
                    .truncate()
                    .child(SharedString::from(target)),
            )
            .child(Input::new(&composer.input))
            .when_some(composer.error.clone(), |card, err| {
                card.child(div().text_color(theme::red()).child(err))
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(11.))
                            .text_color(theme::overlay0())
                            .child(SharedString::from("⌘⏎ to submit")),
                    )
                    .child(
                        Button::new("composer-cancel")
                            .label("Cancel")
                            .ghost()
                            .small()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.close_composer(window, cx);
                            })),
                    )
                    .child(
                        Button::new("composer-submit")
                            .label(action)
                            .primary()
                            .small()
                            .disabled(composer.in_flight)
                            .loading(composer.in_flight)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.submit_composer(window, cx);
                            })),
                    ),
            )
            .into_any_element()
    }

    /// The "submit review" modal: a dimming backdrop (click closes) over a
    /// centered card with the verdict picker, body input, and submit row.
    /// Root-level for the same reason as the composer: its input must sit
    /// outside the "ReviewApp" key context.
    fn render_review(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let empty = || div().into_any_element();
        let Some(review) = &self.review else {
            return empty();
        };
        // Only render for the item it was opened on, and only while active.
        if self.items.get(self.active).map(|item| item.id) != Some(review.item_id) {
            return empty();
        }
        let selected = review.verdict;
        let verdict_option =
            |label: &'static str, verdict: gh::ReviewVerdict, color: gpui::Rgba| {
                let tint: Hsla = color.into();
                div()
                    .id(label)
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .cursor_pointer()
                    .when(verdict == selected, |opt| {
                        opt.bg(tint.opacity(0.15))
                            .border_color(tint.opacity(0.6))
                            .text_color(color)
                    })
                    .when(verdict != selected, |opt| {
                        opt.border_color(theme::surface0())
                            .text_color(theme::subtext())
                            .hover(|style| style.bg(Hsla::from(theme::surface0()).opacity(0.5)))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(review) = &mut this.review {
                            review.verdict = verdict;
                            review.error = None;
                            cx.notify();
                        }
                    }))
                    .child(SharedString::from(label))
            };
        let submit_label = match selected {
            gh::ReviewVerdict::Approve => "Approve",
            gh::ReviewVerdict::RequestChanges => "Request changes",
            gh::ReviewVerdict::Comment => "Comment",
        };
        div()
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .flex_col()
            .items_center()
            .pt(px(120.))
            .bg(theme::palette_backdrop())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.close_review(window, cx);
                }),
            )
            .child(
                div()
                    .w(px(560.))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    // The input propagates Escape when it has nothing of its
                    // own to dismiss; catch it here to cancel the dialog.
                    .on_action(cx.listener(|this, _: &InputEscape, window, cx| {
                        this.close_review(window, cx);
                    }))
                    .rounded_lg()
                    .border_1()
                    .border_color(theme::surface0())
                    .bg(theme::mantle())
                    .shadow_lg()
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .text_size(px(12.))
                    .child(
                        div()
                            .text_color(theme::overlay0())
                            .child(SharedString::from("Finish your review")),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(verdict_option(
                                "approve",
                                gh::ReviewVerdict::Approve,
                                theme::green(),
                            ))
                            .child(verdict_option(
                                "request changes",
                                gh::ReviewVerdict::RequestChanges,
                                theme::red(),
                            ))
                            .child(verdict_option(
                                "comment",
                                gh::ReviewVerdict::Comment,
                                theme::blue(),
                            )),
                    )
                    .child(Input::new(&review.input))
                    .when_some(review.error.clone(), |card, err| {
                        card.child(div().text_color(theme::red()).child(err))
                    })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(px(11.))
                                    .text_color(theme::overlay0())
                                    .child(SharedString::from("⌘⏎ to submit")),
                            )
                            .child(
                                Button::new("review-cancel")
                                    .label("Cancel")
                                    .ghost()
                                    .small()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.close_review(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("review-submit")
                                    .label(submit_label)
                                    .primary()
                                    .small()
                                    .disabled(review.in_flight)
                                    .loading(review.in_flight)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.submit_review(window, cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// The minimap column: precomputed, coalesced quad runs plus one
    /// per-frame viewport rectangle, painted straight into a canvas (no text,
    /// no per-row elements). Mouse-downs stop propagation here so the pane's
    /// selection listeners never see them.
    fn render_minimap(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let entity = cx.entity();
        div()
            .w(px(MINIMAP_WIDTH))
            .h_full()
            .flex_shrink_0()
            .bg(Hsla::from(theme::crust()).opacity(0.5))
            .border_l_1()
            .border_color(theme::surface0())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    window.focus(&this.focus_handle);
                    this.minimap_scrub = true;
                    this.minimap_scrub_to(event.position, cx);
                }),
            )
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, cx| {
                        let this = entity.read(cx);
                        let Some(data) = this.active_data() else {
                            return;
                        };
                        let total = data.rows.len();
                        let pane_h = f32::from(bounds.size.height);
                        if total == 0 || pane_h <= 0. {
                            return;
                        }
                        let layout = data.minimap_layout(pane_h);
                        let (x0, y0) = (f32::from(bounds.left()), f32::from(bounds.top()));
                        let usable = f32::from(bounds.size.width) - 2. * MINIMAP_PAD;
                        let half = (usable - MINIMAP_GAP) / 2.;
                        for run in &layout.runs {
                            let (x, w) = match run.lane {
                                MinimapLane::Full => (0., usable * run.frac),
                                MinimapLane::Left => (0., half * run.frac),
                                MinimapLane::Right => (half + MINIMAP_GAP, half * run.frac),
                            };
                            let y = run.start as f32 * layout.slot_h;
                            let h = if run.tick {
                                1.
                            } else {
                                (run.end - run.start) as f32 * layout.slot_h
                            };
                            // Full alpha-ish tints: these are 1-3px bars and
                            // need punch, unlike the row backgrounds.
                            let color: Hsla = match run.color {
                                MinimapColor::Added => Hsla::from(theme::green()).opacity(0.8),
                                MinimapColor::Removed => Hsla::from(theme::red()).opacity(0.8),
                                MinimapColor::Context => {
                                    Hsla::from(theme::overlay0()).opacity(0.35)
                                }
                                MinimapColor::Header => Hsla::from(theme::blue()).opacity(0.5),
                                MinimapColor::Gap => Hsla::from(theme::overlay0()).opacity(0.2),
                            };
                            window.paint_quad(fill(
                                Bounds::new(
                                    point(px(x0 + MINIMAP_PAD + x), px(y0 + y)),
                                    size(px(w.max(1.)), px(h)),
                                ),
                                color,
                            ));
                        }
                        // Viewport indicator — the only per-frame math.
                        let offset_y = f32::from(-data.scroll.0.borrow().base_handle.offset().y);
                        let px_per_row = layout.slot_h / layout.group as f32;
                        let top_row = offset_y / row_height();
                        let visible = (pane_h / row_height()).min(total as f32 - top_row);
                        let vy = top_row * px_per_row;
                        let vh = (visible * px_per_row).max(3.);
                        window.paint_quad(
                            fill(
                                Bounds::new(
                                    point(bounds.left(), px(y0 + vy)),
                                    size(bounds.size.width, px(vh)),
                                ),
                                Hsla::from(theme::text()).opacity(0.08),
                            )
                            .border_widths(1.)
                            .border_color(Hsla::from(theme::overlay0()).opacity(0.4)),
                        );
                    },
                )
                .size_full(),
            )
            .into_any_element()
    }

    fn render_titlebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let content: gpui::AnyElement = if self.show_queue {
            app_title(None)
        } else {
            match self.active_item() {
                None => app_title(None),
                Some(item) => match &item.state {
                    ItemState::Ready(data) => match &item.source {
                        Source::Pr(_) => match &data.pr_meta {
                            Some(meta) => pr_titlebar_content(meta, cx),
                            None => app_title(None),
                        },
                        Source::Local(src) => local_titlebar_content(item.id, src, data, cx),
                    },
                    ItemState::Loading => app_title(Some(format!("loading {}…", item.primary()))),
                    ItemState::Failed(_) => app_title(Some(format!("{} — failed", item.primary()))),
                },
            }
        };
        // On the queue screen the pane itself spells out the empty and
        // first-failure cases; with rows up, the title bar carries the
        // refresh status beside the account chip, like an item's.
        let note: Option<SharedString> = if self.show_queue {
            if !self.queue.prs.is_empty() {
                if self.queue.loading {
                    Some("refreshing queue…".into())
                } else {
                    self.queue
                        .error
                        .as_ref()
                        .map(|err| SharedString::from(format!("queue refresh failed: {err}")))
                }
            } else {
                None
            }
        } else {
            self.active_item().and_then(|item| {
                if item.reloading {
                    Some("reloading…".into())
                } else {
                    item.refresh_error
                        .as_ref()
                        .map(|err| SharedString::from(format!("refresh failed: {err}")))
                }
            })
        };
        // The title bar spreads its children edge to edge; one right group
        // keeps the item note beside the account chip instead of stranded
        // mid-bar.
        let mut right = div().flex().items_center().flex_shrink_0().h_full().gap_2();
        if let Some(note) = note {
            right = right.child(
                div()
                    .max_w(px(280.))
                    .truncate()
                    .text_color(theme::overlay0())
                    .child(note),
            );
        }
        right = right.child(self.render_account());
        TitleBar::new()
            .text_size(px(13.))
            .child(content)
            .child(right)
    }

    /// The account chip at the title bar's right edge: avatar + login when
    /// GitHub knows who we are; the fix in one line when it doesn't — the
    /// empty pane spells the failure out in full. While the check is still
    /// running, nothing: the chip appears rather than flickers.
    fn render_account(&self) -> gpui::AnyElement {
        match &self.account {
            AccountStatus::Checking => div().into_any_element(),
            AccountStatus::SignedIn(viewer) => div()
                .flex()
                .items_center()
                .gap_1()
                .pr_3()
                .child(Avatar::new().src(viewer.avatar_url.clone()).xsmall())
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme::subtext())
                        .child(SharedString::from(viewer.login.clone())),
                )
                .into_any_element(),
            AccountStatus::SignedOut(err) => div()
                .pr_3()
                .max_w(px(360.))
                .truncate()
                .text_size(px(12.))
                .text_color(theme::peach())
                .child(SharedString::from(err.to_string()))
                .into_any_element(),
        }
    }

    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        // Item counts are small: a plain scrollable div capped at ~40% of the
        // sidebar leaves the rest for the active item's file tree.
        let mut list = div()
            .id("sidebar-items")
            .max_h(relative(0.4))
            .flex_shrink_0()
            .overflow_y_scroll()
            .py_1();
        for (ix, item) in self.items.iter().enumerate() {
            let active = ix == self.active;
            let dot: Hsla = item.dot_color().into();
            let status: gpui::AnyElement = match &item.state {
                ItemState::Ready(data) => div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .flex_shrink_0()
                    .text_size(px(11.))
                    .child(
                        div()
                            .text_color(theme::green())
                            .child(SharedString::from(format!("+{}", data.additions))),
                    )
                    .child(
                        div()
                            .text_color(theme::red())
                            .child(SharedString::from(format!("−{}", data.deletions))),
                    )
                    .into_any_element(),
                ItemState::Loading => div()
                    .flex_shrink_0()
                    .text_size(px(11.))
                    .text_color(theme::overlay0())
                    .child(SharedString::from("loading…"))
                    .into_any_element(),
                ItemState::Failed(_) => div()
                    .flex_shrink_0()
                    .text_size(px(11.))
                    .text_color(theme::red())
                    .child(SharedString::from("failed"))
                    .into_any_element(),
            };
            let secondary = item.secondary();
            let entry = div()
                .id(("item", ix))
                .group("sidebar-item")
                .mx_1()
                .px_2()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .when(active, |entry| entry.bg(theme::surface0()))
                .when(!active, |entry| {
                    entry.hover(|style| style.bg(Hsla::from(theme::surface0()).opacity(0.5)))
                })
                .on_click(cx.listener(move |this, _, window, cx| this.activate(ix, window, cx)))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .w(px(8.))
                                .h(px(8.))
                                .flex_shrink_0()
                                .rounded_full()
                                .bg(dot),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(theme::text())
                                .child(item.primary()),
                        )
                        .child(status)
                        .child(
                            div()
                                .flex_shrink_0()
                                .opacity(0.)
                                .group_hover("sidebar-item", |style| style.opacity(1.))
                                .child(
                                    Button::new(("close-item", ix))
                                        .icon(IconName::Close)
                                        .ghost()
                                        .xsmall()
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.close_item(ix, cx)
                                        })),
                                ),
                        ),
                )
                .when(!secondary.is_empty(), |entry| {
                    entry.child(
                        div()
                            .pl(px(16.))
                            .truncate()
                            .text_size(px(11.))
                            .text_color(theme::subtext())
                            .child(secondary),
                    )
                });
            list = list.child(entry);
        }

        // --- file tree for the active item ---
        let query = self.tree_filter_input.read(cx).value().trim().to_string();
        let mut tree_rows: Vec<TreeListRow> = Vec::new();
        let mut current_file = None;
        let mut current_row = None;
        if let Some(data) = self.active_data() {
            if query.is_empty() {
                tree_rows = visible_entries(&data.tree, &data.collapsed)
                    .into_iter()
                    .map(TreeListRow::Entry)
                    .collect();
            } else {
                let paths: Vec<&str> = data.diff.files.iter().map(|f| f.display_path()).collect();
                tree_rows = fuzzy_file_matches(&paths, &query)
                    .into_iter()
                    .map(TreeListRow::FilteredFile)
                    .collect();
            }
            // Follow the diff: highlight the file whose header row is at (or
            // scrolled past) the top of the viewport. A pending scroll_to_item
            // (from ]/[ or a tree click) hasn't reached the offset yet, so it
            // takes precedence; otherwise the same offset/row_height() math the
            // selection hit test uses.
            let scroll = data.scroll.0.borrow();
            let top_row = match &scroll.deferred_scroll_to_item {
                Some(deferred) => deferred.item_index,
                None => (f32::from(-scroll.base_handle.offset().y) / row_height()).max(0.) as usize,
            };
            drop(scroll);
            current_file = data.file_rows.iter().rposition(|&ix| ix <= top_row);
            current_row = current_file.and_then(|file| {
                tree_rows.iter().position(|row| match row {
                    TreeListRow::Entry(ix) => matches!(
                        data.tree[*ix].kind,
                        TreeEntryKind::File { file_ix } if file_ix == file
                    ),
                    TreeListRow::FilteredFile(file_ix) => *file_ix == file,
                })
            });
        }
        // Keep the highlighted file in view — but only when it changes, so
        // the user's own tree scrolling is never fought.
        if let Some(file) = current_file {
            if let Some(data) = self.active_data_mut() {
                if data.tree_last_file != Some(file) {
                    data.tree_last_file = Some(file);
                    if let Some(pos) = current_row {
                        data.tree_scroll.scroll_to_item(pos, ScrollStrategy::Center);
                    }
                }
            }
        }
        let tree_scroll = self.active_data().map(|data| data.tree_scroll.clone());
        let entity = cx.entity();
        let tree_list: gpui::AnyElement = match tree_scroll {
            Some(scroll) if !tree_rows.is_empty() => {
                uniform_list("file-tree", tree_rows.len(), move |range, _window, cx| {
                    let this = entity.read(cx);
                    let Some(data) = this.active_data() else {
                        return Vec::new();
                    };
                    range
                        .filter_map(|pos| tree_rows.get(pos).map(|row| (pos, *row)))
                        .map(|(pos, row)| {
                            render_tree_row(row, pos, current_row == Some(pos), data, &entity)
                        })
                        .collect()
                })
                .track_scroll(scroll)
                .h_full()
                .into_any_element()
            }
            Some(_) if !query.is_empty() => div()
                .px_3()
                .py_2()
                .text_color(theme::overlay0())
                .child(SharedString::from("no matching files"))
                .into_any_element(),
            _ => div().into_any_element(),
        };

        div()
            .w(px(260.))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(theme::mantle())
            .border_r_1()
            .border_color(theme::surface0())
            .text_size(px(12.))
            .child(
                div()
                    .p_2()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(Input::new(&self.open_input).small())
                    .when_some(self.open_error.clone(), |area, err| {
                        area.child(div().text_size(px(11.)).text_color(theme::red()).child(err))
                    }),
            )
            .child(list)
            .child(div().h(px(1.)).flex_shrink_0().bg(theme::surface0()))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    // The filter input propagates Escape when it has nothing
                    // of its own to dismiss; catch it here (before the root's
                    // handler) to clear the query and return to the diff.
                    .on_action(cx.listener(|this, _: &InputEscape, window, cx| {
                        this.tree_filter_input
                            .update(cx, |state, cx| state.set_value("", window, cx));
                        window.focus(&this.focus_handle);
                        cx.notify();
                    }))
                    .child(
                        div()
                            .p_2()
                            .child(Input::new(&self.tree_filter_input).small()),
                    )
                    .child(div().flex_1().min_h_0().child(tree_list)),
            )
    }

    fn render_palette(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(step) = &self.palette else {
            return div().into_any_element();
        };
        let query = self.palette_input.read(cx).value().to_string();

        let header: Option<SharedString> = match step {
            PaletteStep::Sources { .. } => None,
            PaletteStep::RepoInput { .. } => Some("Open GitHub pull request".into()),
            PaletteStep::PrList { repo, .. } => Some(repo.clone().into()),
            PaletteStep::LocalBaseList { .. } => Some("Choose local diff base".into()),
        };

        let body: gpui::AnyElement = match step {
            PaletteStep::Sources { selected } => {
                let filtered = filtered_sources(&query);
                let selected = *selected;
                let mut list = div().py_1().flex().flex_col();
                if filtered.is_empty() {
                    list = list.child(
                        div()
                            .px_3()
                            .py_2()
                            .text_color(theme::overlay0())
                            .child(SharedString::from("no matches")),
                    );
                }
                for (pos, &opt) in filtered.iter().enumerate() {
                    list = list.child(
                        div()
                            .id(("palette-source", opt))
                            .mx_1()
                            .px_2()
                            .h(px(PALETTE_ROW_HEIGHT))
                            .rounded_md()
                            .flex()
                            .items_center()
                            .cursor_pointer()
                            .when(pos == selected, |row| row.bg(theme::surface0()))
                            .when(pos != selected, |row| {
                                row.hover(|style| {
                                    style.bg(Hsla::from(theme::surface0()).opacity(0.5))
                                })
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.palette_activate_source(opt, window, cx)
                            }))
                            .child(
                                div()
                                    .text_color(theme::text())
                                    .child(SharedString::from(PALETTE_SOURCES[opt])),
                            ),
                    );
                }
                list.into_any_element()
            }
            PaletteStep::RepoInput { error } => {
                let (text, color): (SharedString, gpui::Rgba) = match error {
                    Some(err) => (err.clone(), theme::red()),
                    None => (
                        "enter to list open pull requests · esc to go back".into(),
                        theme::overlay0(),
                    ),
                };
                div()
                    .px_3()
                    .py_2()
                    .text_color(color)
                    .child(text)
                    .into_any_element()
            }
            PaletteStep::PrList { prs, .. } => match prs {
                PrListState::Loading => div()
                    .px_3()
                    .py_2()
                    .text_color(theme::overlay0())
                    .child(SharedString::from("loading pull requests…"))
                    .into_any_element(),
                PrListState::Failed(msg) => div()
                    .px_3()
                    .py_2()
                    .text_color(theme::red())
                    .child(SharedString::from(msg.clone()))
                    .into_any_element(),
                PrListState::Loaded { filtered, .. } if filtered.is_empty() => div()
                    .px_3()
                    .py_2()
                    .text_color(theme::overlay0())
                    .child(SharedString::from("no matching pull requests"))
                    .into_any_element(),
                PrListState::Loaded { filtered, .. } => {
                    let entity = cx.entity();
                    let count = filtered.len();
                    div()
                        .py_1()
                        .h(px(count.min(10) as f32 * PALETTE_ROW_HEIGHT + 8.))
                        .child(
                            uniform_list("palette-prs", count, move |range, _window, cx| {
                                let this = entity.read(cx);
                                let Some(PaletteStep::PrList {
                                    prs:
                                        PrListState::Loaded {
                                            all,
                                            filtered,
                                            selected,
                                        },
                                    ..
                                }) = &this.palette
                                else {
                                    return Vec::new();
                                };
                                range
                                    .filter_map(|pos| Some((pos, &all[*filtered.get(pos)?])))
                                    .map(|(pos, pr)| {
                                        palette_pr_row(pr, pos, pos == *selected, entity.clone())
                                    })
                                    .collect()
                            })
                            .track_scroll(self.palette_scroll.clone())
                            .h_full(),
                        )
                        .into_any_element()
                }
            },
            PaletteStep::LocalBaseList { bases, .. } => match bases {
                BaseListState::Loading => div()
                    .px_3()
                    .py_2()
                    .text_color(theme::overlay0())
                    .child(SharedString::from("loading base refs…"))
                    .into_any_element(),
                BaseListState::Failed(msg) => div()
                    .px_3()
                    .py_2()
                    .text_color(theme::red())
                    .child(SharedString::from(msg.clone()))
                    .into_any_element(),
                BaseListState::Loaded { filtered, .. } if filtered.is_empty() => div()
                    .px_3()
                    .py_2()
                    .text_color(theme::overlay0())
                    .child(SharedString::from("no matching base refs"))
                    .into_any_element(),
                BaseListState::Loaded { filtered, .. } => {
                    let entity = cx.entity();
                    let count = filtered.len();
                    div()
                        .py_1()
                        .h(px(count.min(10) as f32 * PALETTE_ROW_HEIGHT + 8.))
                        .child(
                            uniform_list("palette-bases", count, move |range, _window, cx| {
                                let this = entity.read(cx);
                                let Some(PaletteStep::LocalBaseList {
                                    current,
                                    bases:
                                        BaseListState::Loaded {
                                            all,
                                            filtered,
                                            selected,
                                        },
                                    ..
                                }) = &this.palette
                                else {
                                    return Vec::new();
                                };
                                range
                                    .filter_map(|pos| {
                                        let base = all.get(*filtered.get(pos)?)?;
                                        Some((pos, base.clone(), base == current))
                                    })
                                    .map(|(pos, base, is_current)| {
                                        div()
                                            .id(("palette-base", pos))
                                            .mx_1()
                                            .px_2()
                                            .h(px(PALETTE_ROW_HEIGHT))
                                            .rounded_md()
                                            .flex()
                                            .items_center()
                                            .gap_2()
                                            .cursor_pointer()
                                            .when(pos == *selected, |row| row.bg(theme::surface0()))
                                            .when(pos != *selected, |row| {
                                                row.hover(|style| {
                                                    style
                                                        .bg(Hsla::from(theme::surface0())
                                                            .opacity(0.5))
                                                })
                                            })
                                            .on_click({
                                                let entity = entity.clone();
                                                move |_, window, cx| {
                                                    entity.update(cx, |this, cx| {
                                                        this.palette_open_base_row(pos, window, cx)
                                                    });
                                                }
                                            })
                                            .child(
                                                div()
                                                    .w(px(8.))
                                                    .h(px(8.))
                                                    .flex_shrink_0()
                                                    .rounded_full()
                                                    .bg(if is_current {
                                                        theme::green()
                                                    } else {
                                                        theme::overlay0()
                                                    }),
                                            )
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .truncate()
                                                    .text_color(theme::text())
                                                    .child(SharedString::from(base)),
                                            )
                                            .into_any_element()
                                    })
                                    .collect()
                            })
                            .track_scroll(self.palette_scroll.clone())
                            .h_full(),
                        )
                        .into_any_element()
                }
            },
        };

        // Backdrop: dims and occludes everything (sidebar included); click
        // closes. The panel swallows its own mouse-downs so clicks inside
        // don't bubble to the backdrop.
        div()
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .flex_col()
            .items_center()
            .pt(px(120.))
            .bg(theme::palette_backdrop())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.close_palette(window, cx);
                }),
            )
            .child(
                div()
                    .key_context("Palette")
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_action(cx.listener(|this, _: &PaletteUp, _, cx| this.palette_move(-1, cx)))
                    .on_action(cx.listener(|this, _: &PaletteDown, _, cx| this.palette_move(1, cx)))
                    .on_action(cx.listener(|this, _: &PaletteBack, window, cx| {
                        this.palette_back(window, cx)
                    }))
                    // The palette input propagates Escape when it has nothing
                    // of its own to dismiss; intercept it here before the
                    // root's handler steals focus back to the diff.
                    .on_action(cx.listener(|this, _: &InputEscape, window, cx| {
                        this.palette_back(window, cx)
                    }))
                    .w(px(560.))
                    .rounded_lg()
                    .border_1()
                    .border_color(theme::surface0())
                    .bg(theme::mantle())
                    .shadow_lg()
                    .text_size(px(13.))
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .when_some(header, |panel, header| {
                        panel.child(
                            div()
                                .px_3()
                                .pt_2()
                                .text_size(px(11.))
                                .text_color(theme::overlay0())
                                .child(header),
                        )
                    })
                    .child(
                        div()
                            .p_2()
                            .border_b_1()
                            .border_color(theme::surface0())
                            .child(Input::new(&self.palette_input).small()),
                    )
                    .child(body),
            )
            .into_any_element()
    }

    /// The review queue screen — what a bare launch opens on. Rows are the
    /// facade's queue items in the search's order (recently updated first,
    /// oldest-first at a toggle); `j`/`k` walk them with a visible
    /// selection, the control row sorts and filters (see
    /// [`Self::queue_controls`]). Failures are non-destructive: the last
    /// rows stay up with the error beside them, and a first-fetch failure
    /// explains itself where the rows would be, naming the fix. An empty
    /// queue says so instead of a blank pane — an empty *filtered* one
    /// offers the way back out.
    fn render_queue(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let queue = &self.queue;
        if queue.prs.is_empty() && queue.loading {
            return centered_message("loading review queue…".into(), theme::overlay0());
        }
        if queue.prs.is_empty() {
            if let Some(err) = &queue.error {
                return div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .p_8()
                    .child(
                        div()
                            .max_w(px(560.))
                            .text_center()
                            .text_size(px(14.))
                            .text_color(theme::peach())
                            .child(SharedString::from(err.clone())),
                    )
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(theme::overlay0())
                            .child(SharedString::from("⌘R to retry")),
                    )
                    .into_any_element();
            } // A filter that matched everything away — the rows it was
              // derived from went with them, so the way back out is the
              // message itself: one click (or escape) clears and re-runs.
            if !queue.search.filters().is_empty() {
                let entity = cx.entity();
                return div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .p_8()
                    .child(
                        div()
                            .text_size(px(14.))
                            .text_color(theme::overlay0())
                            .child("no PRs match the current filters"),
                    )
                    .child(
                        div()
                            .id("queue-clear-empty")
                            .cursor_pointer()
                            .flex()
                            .items_center()
                            .px_2()
                            .h(px(24.))
                            .rounded_md()
                            .border_1()
                            .border_color(theme::surface0())
                            .text_size(px(13.))
                            .text_color(theme::subtext())
                            .hover(|style| style.bg(theme::surface0()))
                            .child("clear filters (escape)")
                            .on_click(move |_, window, cx| {
                                entity.update(cx, |this, cx| {
                                    window.focus(&this.focus_handle);
                                    this.queue_clear_filters(cx);
                                })
                            }),
                    )
                    .into_any_element();
            }
            return centered_message("no PRs awaiting your review".into(), theme::overlay0());
        }
        let entity = cx.entity();
        let now = now_unix();
        let count = queue.prs.len();
        let selected = queue.selected;
        div()
            .size_full()
            .flex()
            .flex_col()
            .text_size(px(13.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_4()
                    .flex_shrink_0()
                    .h(px(34.))
                    .border_b_1()
                    .border_color(theme::surface0())
                    .child(div().text_color(theme::text()).child("Review queue"))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_right()
                            .text_size(px(12.))
                            .text_color(theme::subtext())
                            .child(SharedString::from(format!(
                                "{count} {}",
                                if count == 1 { "PR" } else { "PRs" }
                            ))),
                    ),
            )
            .child(self.queue_controls(entity.clone()))
            .child(
                uniform_list("queue", count, move |range, _window, cx| {
                    let this = entity.read(cx);
                    range
                        .filter_map(|pos| this.queue.prs.get(pos).map(|pr| (pos, pr)))
                        .map(|(pos, pr)| queue_row(pr, pos, pos == selected, now, entity.clone()))
                        .collect()
                })
                .track_scroll(queue.scroll.clone())
                .flex_1()
                .min_h_0(),
            )
            .into_any_element()
    }

    /// The queue's control row, under the title: the sort toggle, then the
    /// filter chips derived from the rows below — each showing the exact
    /// qualifier it compiles to, the rows it covers, and its active state —
    /// and, while any chip is active, the one-click clear-all. The row's
    /// right edge carries the fetch status: the refresh in flight, or the
    /// failure that kept the rows stale — beside the chips that caused
    /// it, where the eye already is. The chips are click-driven for now;
    /// the keyboard drives the sort (`s`) and the clear (`escape`).
    fn queue_controls(&self, entity: gpui::Entity<Self>) -> gpui::AnyElement {
        let search = &self.queue.search;
        let oldest = search.ordering() == github::QueueSort::OldestUpdated;
        // The sort toggle: its label is the order the next refresh runs in
        // — newest first by default, oldest first for backlog clearing.
        let mut row = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1()
            .px_3()
            .py_1()
            .flex_shrink_0()
            .border_b_1()
            .border_color(theme::surface0())
            .child(
                div()
                    .id("queue-sort-toggle")
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_1p5()
                    .h(px(22.))
                    .rounded_md()
                    .border_1()
                    .border_color(theme::surface0())
                    .text_size(px(12.))
                    .text_color(theme::subtext())
                    .hover(|style| style.bg(theme::surface0()))
                    .on_click({
                        let entity = entity.clone();
                        move |_, window, cx| {
                            entity.update(cx, |this, cx| {
                                window.focus(&this.focus_handle);
                                this.queue_toggle_sort(cx);
                            })
                        }
                    })
                    .child(
                        div()
                            .text_color(theme::overlay0())
                            .child(SharedString::from("sort:")),
                    )
                    .child(SharedString::from(if oldest {
                        "oldest first ↑"
                    } else {
                        "recently updated ↓"
                    })),
            );
        // Chip ids run across the whole row: groups re-enumerated would
        // collide ("org"'s first chip and "repo"'s are both
        // ("queue-chip", 0)).
        let mut chip_ix = 0usize;
        for group in queue_chips(&self.queue.prs, search) {
            for chip in group.chips {
                let ix = chip_ix;
                chip_ix += 1;
                // The active chip reads like a selected filter — blue-tinted
                // like GitHub's own label chips — and the inactive ones sit
                // quiet until hovered.
                let (bg, fg, border) = if chip.active {
                    label_chip_colors(Some(Hsla::from(theme::blue())), theme::is_light())
                } else {
                    label_chip_colors(None, theme::is_light())
                };
                row = row.child(
                    div()
                        .id(("queue-chip", ix))
                        .cursor_pointer()
                        .flex()
                        .items_center()
                        .gap_0p5()
                        .px_1p5()
                        .h(px(22.))
                        .rounded_md()
                        .border_1()
                        .border_color(border)
                        .bg(bg)
                        .text_size(px(12.))
                        .when(chip.active, |chip| {
                            chip.hover(|style| style.bg(Hsla::from(theme::blue()).opacity(0.2)))
                        })
                        .when(!chip.active, |chip| {
                            chip.hover(|style| style.bg(theme::surface0()))
                        })
                        .on_click({
                            let entity = entity.clone();
                            let filter = chip.filter.clone();
                            move |_, window, cx| {
                                entity.update(cx, |this, cx| {
                                    window.focus(&this.focus_handle);
                                    this.queue_toggle_filter(filter.clone(), cx);
                                })
                            }
                        })
                        .child(
                            div()
                                .text_color(theme::overlay0())
                                .child(SharedString::from(format!("{}:", group.kind.prefix()))),
                        )
                        .child(
                            div()
                                .max_w(px(220.))
                                .truncate()
                                .text_color(fg)
                                .child(SharedString::from(chip.filter.value().to_string())),
                        )
                        .child(
                            div()
                                .text_color(if chip.active {
                                    fg
                                } else {
                                    Hsla::from(theme::overlay0())
                                })
                                .child(SharedString::from(format!("· {}", chip.count))),
                        ),
                );
            }
            if group.hidden > 0 {
                row = row.child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme::overlay0())
                        .child(SharedString::from(format!("+{} more", group.hidden))),
                );
            }
        }
        if !search.filters().is_empty() {
            row = row.child(
                div()
                    .id("queue-clear-filters")
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .px_1p5()
                    .h(px(22.))
                    .rounded_md()
                    .border_1()
                    .border_color(theme::surface0())
                    .text_size(px(12.))
                    .text_color(theme::overlay0())
                    .hover(|style| style.text_color(theme::red()).border_color(theme::red()))
                    .on_click({
                        let entity = entity.clone();
                        move |_, window, cx| {
                            entity.update(cx, |this, cx| {
                                window.focus(&this.focus_handle);
                                this.queue_clear_filters(cx);
                            })
                        }
                    })
                    .child(SharedString::from("✕ clear")),
            );
        }
        // The fetch status, right-aligned where the chips are: the refresh
        // a click just kicked off, or the failure that left the stale
        // rows up — surfaced in the pane, not only in the title bar.
        //
        // A queue painted from the local cache says so, and says how old
        // it is: the rows on screen are the last ones the app saw, not
        // live data, and the reviewer deserves to know which they're
        // looking at.
        let mut status: Vec<(String, gpui::Hsla)> = Vec::new();
        if self.queue.loading {
            status.push((
                match self.queue.origin {
                    QueueOrigin::Cached => "refreshing queue… (showing cached rows)".to_string(),
                    _ => "refreshing queue…".to_string(),
                },
                gpui::Hsla::from(theme::overlay0()),
            ));
        } else if let Some(err) = self.queue.error.as_deref() {
            status.push((err.to_string(), gpui::Hsla::from(theme::peach())));
        } else if self.queue.origin == QueueOrigin::Cached {
            let age = self.store.cached_queue().age_seconds(local_store::now_ms());
            status.push((
                format!("cached · {age}s ago"),
                gpui::Hsla::from(theme::overlay0()),
            ));
        }
        // A store that couldn't be opened is worth saying out loud: the
        // queue still works, but nothing is cached, so the next launch
        // starts from the network again. The path it *would* have used
        // is part of the message — it's what the user might have to fix.
        if let Some(err) = &self.store_error {
            let where_ = match self.store.path() {
                Some(path) => format!(" ({})", path.display()),
                None => String::new(),
            };
            status.push((
                format!("cache unavailable{where_}: {err}"),
                gpui::Hsla::from(theme::peach()),
            ));
        }
        row = row
            .child(div().flex_1().min_w_0())
            .when(!status.is_empty(), |row| {
                row.children(status.into_iter().map(|(text, color)| {
                    div()
                        .min_w_0()
                        .max_w(px(480.))
                        .truncate()
                        .text_size(px(12.))
                        .text_color(color)
                        .child(SharedString::from(text))
                }))
            });
        row.into_any_element()
    }

    /// The pane with nothing open: the ⌘T hint — plus, when GitHub isn't
    /// usable, the facade's sign-in message above it, so a fresh launch
    /// without `gh auth login` explains itself instead of showing a near-
    /// blank screen. With something open, local diffs included, only the
    /// title-bar chip carries the message.
    fn render_empty_pane(&self) -> gpui::AnyElement {
        match &self.account {
            AccountStatus::SignedOut(err) => div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_4()
                .p_8()
                .child(
                    div()
                        .max_w(px(560.))
                        .text_center()
                        .text_size(px(14.))
                        .text_color(theme::peach())
                        .child(SharedString::from(err.to_string())),
                )
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(theme::overlay0())
                        .child(SharedString::from("⌘T to open a PR or path")),
                )
                .into_any_element(),
            _ => centered_message("⌘T to open a PR or path".into(), theme::overlay0()),
        }
    }

    fn render_footer(&self) -> impl IntoElement {
        let hint = |keys: &[&str], label: &'static str| {
            let mut hint = div().flex().items_center().gap_1();
            for key in keys {
                hint = hint.child(Kbd::new(Keystroke::parse(key).unwrap()));
            }
            hint.child(
                div()
                    .text_color(theme::overlay0())
                    .child(SharedString::from(label)),
            )
        };
        let mut footer = div()
            .h(px(28.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_4()
            .px_3()
            .bg(theme::mantle())
            .border_t_1()
            .border_color(theme::surface0())
            .text_size(px(12.));
        // The queue screen's hints differ from the diff's: j/k drive the
        // rows, and nothing about hunks or files applies yet.
        if self.show_queue {
            footer = footer
                .child(hint(&["j", "k"], "rows"))
                .child(hint(&["enter"], "open PR"))
                .child(hint(&["s"], "sort"))
                .child(hint(&["r"], "refresh"))
                // Escape only clears once there's something to clear, so
                // its hint appears with it.
                .when(!self.queue.search.filters().is_empty(), |footer| {
                    footer.child(hint(&["escape"], "clear filters"))
                })
                .child(hint(&["cmd-t"], "open"))
                .child(hint(&["cmd-k"], "palette"))
                .child(hint(&["cmd-b"], "sidebar"));
        } else {
            footer = footer
                .child(hint(&["]", "["], "files"))
                .child(hint(&["n", "p"], "hunks"))
                .child(hint(&["u"], "queue"))
                .child(hint(&["v"], "unified/split"))
                .child(hint(&["m"], "minimap"))
                .child(hint(&["c"], "comments"))
                .child(hint(&["/"], "filter files"))
                .child(hint(&["home", "end"], "top/bottom"))
                .child(hint(&["cmd-k"], "palette"))
                .child(hint(&["cmd-t"], "open"))
                .child(hint(&["cmd-b"], "sidebar"))
                .child(hint(&["r"], "refresh"))
                .child(hint(&["cmd-enter"], "review"));
        }
        footer
    }
    /// The pane for an open item — loading, failure message, or the
    /// diff itself — everything the main pane shows when the queue
    /// screen isn't up.
    fn render_item_pane(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let entity = cx.entity();
        match self.active_item() {
            None => self.render_empty_pane(),
            Some(item) => match &item.state {
                ItemState::Loading => centered_message("loading…".into(), theme::overlay0()),
                ItemState::Failed(msg) => div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .p_8()
                    .child(
                        div()
                            .max_w(px(720.))
                            .text_color(theme::red())
                            .child(SharedString::from(msg.clone())),
                    )
                    .into_any_element(),
                ItemState::Ready(data) => div()
                    .size_full()
                    .relative()
                    .flex()
                    .font_family(MONO)
                    .text_size(px(text_size()))
                    .line_height(px(row_height()))
                    // Selection mouse listeners live on the diff pane only.
                    // While the palette is open its occluding backdrop keeps
                    // this hitbox from being hovered, so none of these fire;
                    // the palette.is_none() guard documents (and backstops)
                    // that. The Scrollbar stops propagation of its own
                    // mouse-downs and thumb drags before they reach us.
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, window, cx| {
                            if this.palette.is_some() {
                                return;
                            }
                            window.focus(&this.focus_handle);
                            let char_width = this.char_width(window);
                            this.drag_anchor = this.pane_hit(event.position, char_width, None);
                            // A plain click clears; a selection only appears
                            // once the drag covers ≥ 1 char.
                            if let Some(data) = this.active_data_mut() {
                                if data.selection.take().is_some() {
                                    cx.notify();
                                }
                            }
                        }),
                    )
                    .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                        if !event.dragging() {
                            return;
                        }
                        // A scrub drag that started on the minimap keeps
                        // scrubbing wherever the pointer goes; it never
                        // becomes a text selection (drag_anchor stays None).
                        if this.minimap_scrub {
                            this.minimap_scrub_to(event.position, cx);
                            return;
                        }
                        let Some((side, anchor)) = this.drag_anchor else {
                            return;
                        };
                        let char_width = this.char_width(window);
                        let Some((_, head)) = this.pane_hit(event.position, char_width, Some(side))
                        else {
                            return;
                        };
                        let selection =
                            (head != anchor).then_some(Selection { side, anchor, head });
                        if let Some(data) = this.active_data_mut() {
                            if data.selection != selection {
                                data.selection = selection;
                                cx.notify();
                            }
                        }
                    }))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseUpEvent, _, _| {
                            this.drag_anchor = None;
                            this.minimap_scrub = false;
                        }),
                    )
                    // Releases outside the pane (drag ended over the sidebar,
                    // footer, …) must still end the drag.
                    .on_mouse_up_out(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseUpEvent, _, _| {
                            this.drag_anchor = None;
                            this.minimap_scrub = false;
                        }),
                    )
                    .child(
                        uniform_list("diff", data.rows.len(), move |range, _window, cx| {
                            let this = entity.read(cx);
                            match this.active_data() {
                                Some(data) => {
                                    let sel = data.selection;
                                    range
                                        .filter_map(|ix| data.rows.get(ix).map(|row| (ix, row)))
                                        .map(|(ix, row)| {
                                            let row_sel = sel.and_then(|sel| {
                                                row_selection_range(&sel, ix, row)
                                                    .filter(|range| !range.is_empty())
                                                    .map(|range| (sel.side, range))
                                            });
                                            render_row(ix, row, row_sel, &entity)
                                        })
                                        .collect()
                                }
                                None => Vec::new(),
                            }
                        })
                        .track_scroll(data.scroll.clone())
                        .with_horizontal_sizing_behavior(match data.mode {
                            ViewMode::Unified => ListHorizontalSizingBehavior::Unconstrained,
                            ViewMode::Split => ListHorizontalSizingBehavior::FitList,
                        })
                        .h_full()
                        .flex_1()
                        .min_w_0(),
                    )
                    // Between the list and the Scrollbar, which paints over
                    // the column's right edge. Nothing is mounted when hidden
                    // — zero cost.
                    .when(self.minimap_visible, |pane| {
                        pane.child(self.render_minimap(cx))
                    })
                    .child(Scrollbar::new(&data.scroll))
                    // Hover "+" (add comment) overlay, absolutely positioned
                    // at the hovered row's y like the minimap viewport.
                    .children(self.render_plus(cx))
                    .into_any_element(),
            },
        }
    }
}

impl Render for ReviewApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.resync_comment_wrap(window);
        let pane: gpui::AnyElement = if self.show_queue {
            self.render_queue(cx)
        } else {
            self.render_item_pane(cx)
        };
        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(theme::base())
            .text_color(theme::text())
            .on_action(cx.listener(|this, _: &OpenPalette, window, cx| {
                if this.palette.is_some() {
                    this.close_palette(window, cx);
                } else {
                    this.open_palette(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &NextFile, _, cx| {
                let targets = this
                    .active_data()
                    .map(|d| d.file_rows.clone())
                    .unwrap_or_default();
                this.jump_next(&targets, cx)
            }))
            .on_action(cx.listener(|this, _: &PrevFile, _, cx| {
                let targets = this
                    .active_data()
                    .map(|d| d.file_rows.clone())
                    .unwrap_or_default();
                this.jump_prev(&targets, cx)
            }))
            .on_action(cx.listener(|this, _: &NextHunk, _, cx| {
                let targets = this
                    .active_data()
                    .map(|d| d.hunk_rows.clone())
                    .unwrap_or_default();
                this.jump_next(&targets, cx)
            }))
            .on_action(cx.listener(|this, _: &PrevHunk, _, cx| {
                let targets = this
                    .active_data()
                    .map(|d| d.hunk_rows.clone())
                    .unwrap_or_default();
                this.jump_prev(&targets, cx)
            }))
            .on_action(cx.listener(|this, _: &GoToTop, _, cx| this.jump(0, cx)))
            .on_action(cx.listener(|this, _: &GoToBottom, _, cx| {
                if let Some(last) = this.active_data().map(|d| d.rows.len().saturating_sub(1)) {
                    this.jump(last, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleView, _, cx| this.toggle_view(cx)))
            .on_action(cx.listener(|this, _: &ToggleMinimap, _, cx| {
                this.minimap_visible = !this.minimap_visible;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleComments, _, cx| this.toggle_comments(cx)))
            .on_action(cx.listener(|this, _: &SubmitReview, window, cx| {
                // Backstop: with the dialog open, focus sits in its input,
                // whose own secondary-enter handles submission.
                if this.review.is_some() {
                    this.submit_review(window, cx);
                } else if matches!(
                    this.active_item().map(|item| &item.source),
                    Some(Source::Local(_))
                ) {
                    this.copy_local_review_prompt(cx);
                } else {
                    this.open_review(window, cx);
                }
            }))
            // Hover tracking for the "+" affordance lives on the root so the
            // affordance clears when the pointer leaves the diff list.
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                if event.dragging() {
                    return;
                }
                let hover = this.hover_target(event.position, window);
                if this.hover_plus != hover {
                    this.hover_plus = hover;
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &Refresh, _, cx| this.refresh(cx)))
            .on_action(cx.listener(|this, _: &QueueUp, _, cx| this.queue_move(-1, cx)))
            .on_action(cx.listener(|this, _: &QueueDown, _, cx| this.queue_move(1, cx)))
            .on_action(
                cx.listener(|this, _: &QueueOpen, _, cx| this.queue_open(this.queue.selected, cx)),
            )
            .on_action(cx.listener(|this, _: &BackToQueue, _, cx| this.back_to_queue(cx)))
            .on_action(cx.listener(|this, _: &QueueSortToggle, _, cx| this.queue_toggle_sort(cx)))
            .on_action(
                cx.listener(|this, _: &QueueClearFilters, _, cx| this.queue_clear_filters(cx)),
            )
            // Bound in the "ReviewApp" context: these only fire while the
            // diff pane has focus. With the palette open, focus sits in the
            // palette input, so its escape routing (PaletteBack) wins.
            .on_action(cx.listener(|this, _: &ClearSelection, window, cx| {
                // With the composer open but focus back on the diff pane,
                // escape closes the composer before touching the selection.
                if this.composer.is_some() {
                    this.close_composer(window, cx);
                    return;
                }
                if let Some(data) = this.active_data_mut() {
                    if data.selection.take().is_some() {
                        cx.notify();
                    }
                }
            }))
            .on_action(cx.listener(|this, _: &CopySelection, _, cx| {
                let Some(data) = this.active_data() else {
                    return;
                };
                let Some(sel) = data.selection else {
                    return;
                };
                let text = selection_text(&sel, &data.rows);
                if !text.is_empty() {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| {
                this.sidebar_visible = !this.sidebar_visible;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ZoomIn, _, cx| this.zoom(1., false, cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| this.zoom(-1., false, cx)))
            .on_action(cx.listener(|this, _: &ZoomReset, _, cx| this.zoom(0., true, cx)))
            .on_action(cx.listener(|this, _: &FocusTreeFilter, window, cx| {
                this.sidebar_visible = true;
                this.tree_filter_input
                    .update(cx, |state, cx| state.focus(window, cx));
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &OpenInput, window, cx| {
                // The sidebar input can't take focus under the palette.
                this.palette = None;
                this.palette_gen += 1;
                this.sidebar_visible = true;
                this.open_input
                    .update(cx, |state, cx| state.focus(window, cx));
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &CloseItem, _, cx| {
                let active = this.active;
                this.close_item(active, cx)
            }))
            .on_action(cx.listener(|this, _: &NextItem, _, cx| this.cycle_items(1, cx)))
            .on_action(cx.listener(|this, _: &PrevItem, _, cx| this.cycle_items(-1, cx)))
            // The open input propagates Escape when it has nothing of its own
            // to dismiss: hand focus back to the diff.
            .on_action(cx.listener(|this, _: &InputEscape, window, cx| {
                window.focus(&this.focus_handle);
                cx.notify();
            }))
            .child(self.render_titlebar(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .when(self.sidebar_visible, |main| {
                        main.child(self.render_sidebar(cx))
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            // The queue screen is its own key context: while
                            // it's up, the diff's single-key bindings stay
                            // quiet and the queue's (j/k, r) fire instead.
                            .key_context(if self.show_queue {
                                "Queue"
                            } else {
                                "ReviewApp"
                            })
                            .track_focus(&self.focus_handle)
                            .child(pane),
                    ),
            )
            .child(self.render_footer())
            // Root-level so the composer's input escapes the "ReviewApp" key
            // context (plain letters must stay text, like the palette input).
            .when(self.composer.is_some(), |root| {
                root.child(self.render_composer(cx))
            })
            .when(self.review.is_some(), |root| {
                root.child(self.render_review(cx))
            })
            .when(self.palette.is_some(), |root| {
                root.child(self.render_palette(cx))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diff_core::{FileDiff, Hunk};

    fn mention(login: &str, name: Option<&str>) -> gh::Mention {
        gh::Mention {
            login: login.to_string(),
            name: name.map(str::to_string),
        }
    }

    #[test]
    fn mention_prefix_detects_at_tokens_at_word_boundaries() {
        let at = |s: &str, off: usize| mention_prefix(&Rope::from(s), off);
        // Bare `@` with the cursor right after it: empty prefix.
        assert_eq!(at("hi @", 4), Some((3, String::new())));
        // Mid-token cursor returns only what's typed so far.
        assert_eq!(at("hi @oct", 7), Some((3, "oct".to_string())));
        assert_eq!(at("hi @oct", 5), Some((3, "o".to_string())));
        // Start of text counts as a boundary.
        assert_eq!(at("@oct", 4), Some((0, "oct".to_string())));
        // Hyphens are valid login characters.
        assert_eq!(at("@foo-bar", 8), Some((0, "foo-bar".to_string())));
        // Not a boundary (looks like an email) — no completion.
        assert_eq!(at("foo@bar", 7), None);
        // No `@` at all.
        assert_eq!(at("hello", 5), None);
        // Cursor before the `@`.
        assert_eq!(at("hi @oct", 3), None);
    }

    #[test]
    fn mention_rank_orders_login_prefix_first() {
        let octocat = mention("octocat", Some("The Octocat"));
        // Login prefix is the strongest match.
        assert_eq!(mention_rank(&octocat, "oct"), Some(0));
        // Empty query matches everything at the top rank.
        assert_eq!(mention_rank(&octocat, ""), Some(0));
        // Name-word prefix beats a login substring.
        assert_eq!(
            mention_rank(&mention("xyz", Some("Bob Jones")), "bob"),
            Some(1)
        );
        assert_eq!(mention_rank(&mention("abobc", None), "bob"), Some(2));
        // Case-insensitive, and no match returns None.
        assert_eq!(mention_rank(&octocat, "OCT"), Some(0));
        assert_eq!(mention_rank(&octocat, "zzz"), None);
    }

    fn ctx(old_no: u32, new_no: u32, text: &str) -> DiffRow {
        DiffRow::Context {
            old_no,
            new_no,
            text: text.to_string(),
        }
    }

    fn add(new_no: u32, text: &str, intra: Vec<Range<usize>>) -> DiffRow {
        DiffRow::Added {
            new_no,
            text: text.to_string(),
            intra,
        }
    }

    fn rem(old_no: u32, text: &str, intra: Vec<Range<usize>>) -> DiffRow {
        DiffRow::Removed {
            old_no,
            text: text.to_string(),
            intra,
        }
    }

    fn hunk(old_start: u32, new_start: u32, rows: Vec<DiffRow>) -> Hunk {
        Hunk {
            old_start,
            old_count: 0,
            new_start,
            new_count: 0,
            section: String::new(),
            rows,
        }
    }

    fn sample_diff() -> PrDiff {
        PrDiff {
            files: vec![
                FileDiff {
                    old_path: Some("a.rs".into()),
                    new_path: Some("a.rs".into()),
                    status: FileStatus::Modified,
                    hunks: vec![
                        // Equal-count modified run, flanked by context.
                        hunk(
                            1,
                            1,
                            vec![
                                ctx(1, 1, "ctx"),
                                rem(2, "old1", vec![0..3]),
                                rem(3, "old2", Vec::new()),
                                add(2, "new1", vec![0..3]),
                                add(3, "new2", Vec::new()),
                                ctx(4, 4, "tail"),
                            ],
                        ),
                        // Unequal run (2 removed, 1 added) + a lone added run.
                        hunk(
                            10,
                            10,
                            vec![
                                rem(10, "r1", Vec::new()),
                                rem(11, "r2", Vec::new()),
                                add(10, "a1", Vec::new()),
                                ctx(12, 11, "c"),
                                add(12, "lone", Vec::new()),
                            ],
                        ),
                    ],
                    additions: 4,
                    deletions: 4,
                },
                FileDiff {
                    old_path: Some("b.png".into()),
                    new_path: Some("b.png".into()),
                    status: FileStatus::Binary,
                    hunks: Vec::new(),
                    additions: 0,
                    deletions: 0,
                },
            ],
        }
    }

    fn cell(cell: &Option<Cell>) -> (u32, LineKind, &str, &[Range<usize>]) {
        let cell = cell.as_ref().expect("expected a cell");
        (cell.no, cell.kind, cell.text.as_ref(), &cell.intra)
    }

    #[test]
    fn split_context_fills_both_cells() {
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Split,
            &HashMap::new(),
            None,
            true,
            COMMENT_WRAP_CHARS,
        );
        // rows[0] = FileHeader, rows[1] = HunkHeader, rows[2] = first context.
        match &rows[2] {
            Row::SplitLine { left, right } => {
                assert_eq!(cell(left), (1, LineKind::Context, "ctx", &[][..]));
                assert_eq!(cell(right), (1, LineKind::Context, "ctx", &[][..]));
            }
            _ => panic!("expected split line"),
        }
    }

    #[test]
    fn split_pairs_equal_runs_positionally() {
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Split,
            &HashMap::new(),
            None,
            true,
            COMMENT_WRAP_CHARS,
        );
        match &rows[3] {
            Row::SplitLine { left, right } => {
                assert_eq!(cell(left), (2, LineKind::Removed, "old1", &[0..3][..]));
                assert_eq!(cell(right), (2, LineKind::Added, "new1", &[0..3][..]));
            }
            _ => panic!("expected split line"),
        }
        match &rows[4] {
            Row::SplitLine { left, right } => {
                assert_eq!(cell(left), (3, LineKind::Removed, "old2", &[][..]));
                assert_eq!(cell(right), (3, LineKind::Added, "new2", &[][..]));
            }
            _ => panic!("expected split line"),
        }
        // Equal run + 2 context rows: 4 split lines for a 6-row hunk.
        match &rows[5] {
            Row::SplitLine { left, right } => {
                assert_eq!(cell(left).2, "tail");
                assert_eq!(cell(right).2, "tail");
            }
            _ => panic!("expected split line"),
        }
    }

    #[test]
    fn split_unequal_and_lone_runs_are_one_sided() {
        let (rows, _, hunk_rows) = build_rows(
            &sample_diff(),
            ViewMode::Split,
            &HashMap::new(),
            None,
            true,
            COMMENT_WRAP_CHARS,
        );
        let h2 = hunk_rows[1];
        // 2 removed / 1 added: first row paired, second left-only.
        match &rows[h2 + 1] {
            Row::SplitLine { left, right } => {
                assert_eq!(cell(left), (10, LineKind::Removed, "r1", &[][..]));
                assert_eq!(cell(right), (10, LineKind::Added, "a1", &[][..]));
            }
            _ => panic!("expected split line"),
        }
        match &rows[h2 + 2] {
            Row::SplitLine { left, right } => {
                assert_eq!(cell(left), (11, LineKind::Removed, "r2", &[][..]));
                assert!(right.is_none());
            }
            _ => panic!("expected split line"),
        }
        // Lone added run after context: right-only.
        match &rows[h2 + 4] {
            Row::SplitLine { left, right } => {
                assert!(left.is_none());
                assert_eq!(cell(right), (12, LineKind::Added, "lone", &[][..]));
            }
            _ => panic!("expected split line"),
        }
    }

    /// A 20-line file with line 10 changed, re-diffed with context 3: one
    /// hunk covering lines 7..=13, hidden gaps of 6 lines above and 7 below.
    fn upgraded_diff() -> (PrDiff, HashMap<usize, FileUpgrade>) {
        let old: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        let new = old.replace("line 10\n", "line ten\n");
        let hunks = diff_texts(&old, &new, 3);
        let new_lines: Vec<SharedString> = new.lines().map(|l| l.to_string().into()).collect();
        let n = new_lines.len();
        let file = FileDiff {
            old_path: Some("a.txt".into()),
            new_path: Some("a.txt".into()),
            status: FileStatus::Modified,
            hunks,
            additions: 1,
            deletions: 1,
        };
        let upgrade = FileUpgrade {
            new_lines,
            old_spans: vec![Vec::new(); 20],
            new_spans: vec![Vec::new(); n],
            expanded: HashSet::new(),
        };
        (PrDiff { files: vec![file] }, HashMap::from([(0, upgrade)]))
    }

    #[test]
    fn gap_span_math() {
        let (diff, upgrades) = upgraded_diff();
        let hunks = &diff.files[0].hunks;
        let total = upgrades[&0].new_lines.len() as u32;
        assert_eq!(gap_span(hunks, 0, total), (1, 1, 6)); // lines 1..=6 hidden
        assert_eq!(gap_span(hunks, 1, total), (14, 14, 7)); // lines 14..=20

        // Zero hunks (e.g. a pure CRLF flip): the whole file is one gap.
        assert_eq!(gap_span(&[], 0, total), (1, 1, 20));
        // Added file: single hunk covers everything, both gaps empty.
        let added = diff_texts("", "a\nb\n", 3);
        assert_eq!(gap_span(&added, 0, 2).2, 0);
        assert_eq!(gap_span(&added, 1, 2).2, 0);
        // Deleted file: new side empty, no gaps and no underflow.
        let deleted = diff_texts("a\nb\n", "", 3);
        assert_eq!(gap_span(&deleted, 0, 0).2, 0);
        assert_eq!(gap_span(&deleted, 1, 0).2, 0);
    }

    fn row_name(row: &Row) -> &'static str {
        match row {
            Row::Spacer => "Spacer",
            Row::FileHeader { .. } => "FileHeader",
            Row::HunkHeader { .. } => "HunkHeader",
            Row::Binary => "Binary",
            Row::Gap { .. } => "Gap",
            Row::Line { .. } => "Line",
            Row::SplitLine { .. } => "SplitLine",
            Row::CommentHeader { .. } => "CommentHeader",
            Row::CommentBody { .. } => "CommentBody",
            Row::CommentActions { .. } => "CommentActions",
        }
    }

    #[test]
    fn upgraded_file_gets_gap_rows_and_marked_headers() {
        let (diff, upgrades) = upgraded_diff();
        for mode in [ViewMode::Unified, ViewMode::Split] {
            let (rows, _, hunk_rows) =
                build_rows(&diff, mode, &upgrades, None, true, COMMENT_WRAP_CHARS);
            // FileHeader, Gap(6), HunkHeader, 7 hunk rows, Gap(7).
            match &rows[1] {
                Row::Gap {
                    file_ix,
                    gap_ix,
                    hidden,
                } => {
                    assert_eq!((*file_ix, *gap_ix, *hidden), (0, 0, 6));
                }
                other => panic!("expected leading gap, got {}", row_name(other)),
            }
            assert_eq!(hunk_rows, vec![2]);
            assert!(matches!(rows[2], Row::HunkHeader { upgraded: true, .. }));
            match rows.last().unwrap() {
                Row::Gap { gap_ix, hidden, .. } => assert_eq!((*gap_ix, *hidden), (1, 7)),
                other => panic!("expected trailing gap, got {}", row_name(other)),
            }
            // Gap rows are selectable-through, like headers.
            assert!(row_side_text(&rows[1], SelSide::Unified).is_none());
            assert!(row_side_text(&rows[1], SelSide::Left).is_none());
        }
        // Un-upgraded build of the same diff has no gap rows.
        let (rows, _, _) = build_rows(
            &diff,
            ViewMode::Unified,
            &HashMap::new(),
            None,
            true,
            COMMENT_WRAP_CHARS,
        );
        assert!(!rows.iter().any(|row| matches!(row, Row::Gap { .. })));
        assert!(matches!(
            rows[1],
            Row::HunkHeader {
                upgraded: false,
                ..
            }
        ));
    }

    #[test]
    fn expanded_gap_synthesizes_context_rows_with_correct_numbers() {
        let (diff, mut upgrades) = upgraded_diff();
        upgrades.get_mut(&0).unwrap().expanded.insert(0);

        let (rows, _, hunk_rows) = build_rows(
            &diff,
            ViewMode::Unified,
            &upgrades,
            None,
            true,
            COMMENT_WRAP_CHARS,
        );
        // Leading gap expanded into 6 context rows before the hunk header.
        assert_eq!(hunk_rows, vec![7]); // FileHeader + 6 context rows
        for (j, row) in rows[1..7].iter().enumerate() {
            match row {
                Row::Line {
                    old_no,
                    new_no,
                    kind,
                    text,
                    ..
                } => {
                    assert_eq!(*kind, LineKind::Context);
                    assert_eq!(*old_no, Some(j as u32 + 1));
                    assert_eq!(*new_no, Some(j as u32 + 1));
                    assert_eq!(text.as_ref(), format!("line {}", j + 1));
                }
                other => panic!("expected context line, got {}", row_name(other)),
            }
        }
        // Trailing gap still collapsed.
        assert!(matches!(rows.last(), Some(Row::Gap { gap_ix: 1, .. })));

        // Split mode: same expansion as two-cell context rows.
        let (rows, _, _) = build_rows(
            &diff,
            ViewMode::Split,
            &upgrades,
            None,
            true,
            COMMENT_WRAP_CHARS,
        );
        match &rows[1] {
            Row::SplitLine { left, right } => {
                let (l, r) = (left.as_ref().unwrap(), right.as_ref().unwrap());
                assert_eq!((l.no, r.no), (1, 1));
                assert_eq!(l.kind, LineKind::Context);
                assert_eq!(l.text.as_ref(), "line 1");
                assert_eq!(r.text.as_ref(), "line 1");
            }
            other => panic!("expected split context, got {}", row_name(other)),
        }

        // Expanding the trailing gap too: numbering continues past the hunk.
        upgrades.get_mut(&0).unwrap().expanded.insert(1);
        let (rows, _, _) = build_rows(
            &diff,
            ViewMode::Unified,
            &upgrades,
            None,
            true,
            COMMENT_WRAP_CHARS,
        );
        assert!(!rows.iter().any(|row| matches!(row, Row::Gap { .. })));
        match rows.last().unwrap() {
            Row::Line {
                old_no,
                new_no,
                text,
                ..
            } => {
                assert_eq!((*old_no, *new_no), (Some(20), Some(20)));
                assert_eq!(text.as_ref(), "line 20");
            }
            other => panic!("expected context line, got {}", row_name(other)),
        }
    }

    use syntax::Token;

    fn style(token: Token) -> HighlightStyle {
        theme::token_style(token)
    }

    fn style_bg(token: Option<Token>) -> HighlightStyle {
        let mut style = token.map(style).unwrap_or_default();
        style.background_color = Some(theme::added_word_bg().into());
        style
    }

    #[test]
    fn merge_syntax_only() {
        let syntax = [(0..2, Token::Keyword), (3..7, Token::Function)];
        assert_eq!(
            merge_highlights(&syntax, &[], None, None),
            vec![
                (0..2, style(Token::Keyword)),
                (3..7, style(Token::Function))
            ]
        );
    }

    #[test]
    fn merge_intra_only() {
        let bg = Some(theme::added_word_bg());
        assert_eq!(
            merge_highlights(&[], &[2..5], bg, None),
            vec![(2..5, style_bg(None))]
        );
    }

    #[test]
    fn merge_partial_overlap() {
        let bg = Some(theme::added_word_bg());
        let syntax = [(0..6, Token::String)];
        assert_eq!(
            merge_highlights(&syntax, &[4..8], bg, None),
            vec![
                (0..4, style(Token::String)),
                (4..6, style_bg(Some(Token::String))),
                (6..8, style_bg(None)),
            ]
        );
    }

    #[test]
    fn merge_intra_spanning_multiple_tokens() {
        let bg = Some(theme::added_word_bg());
        let syntax = [(0..3, Token::Keyword), (5..8, Token::Number)];
        assert_eq!(
            merge_highlights(&syntax, &[1..7], bg, None),
            vec![
                (0..1, style(Token::Keyword)),
                (1..3, style_bg(Some(Token::Keyword))),
                (3..5, style_bg(None)),
                (5..7, style_bg(Some(Token::Number))),
                (7..8, style(Token::Number)),
            ]
        );
    }

    #[test]
    fn merge_adjacent_ranges() {
        // Same style across a shared boundary coalesces; different styles
        // stay split exactly at the boundary.
        let syntax = [(0..2, Token::Keyword), (2..4, Token::Keyword)];
        assert_eq!(
            merge_highlights(&syntax, &[], None, None),
            vec![(0..4, style(Token::Keyword))]
        );
        let syntax = [(0..2, Token::Keyword), (2..4, Token::Type)];
        assert_eq!(
            merge_highlights(&syntax, &[], None, None),
            vec![(0..2, style(Token::Keyword)), (2..4, style(Token::Type))]
        );
        let bg = Some(theme::added_word_bg());
        assert_eq!(
            merge_highlights(&[], &[0..2, 2..4], bg, None),
            vec![(0..4, style_bg(None))]
        );
    }

    #[test]
    fn hunk_syntax_takes_spans_from_the_right_side() {
        let lang = syntax::language_for_path("x.rs");
        let rows = vec![
            ctx(1, 1, "fn f() {"),
            rem(2, "// gone", Vec::new()),
            add(2, "    let b = 2;", Vec::new()),
            ctx(3, 3, "}"),
        ];
        let spans = hunk_syntax(lang, &rows);
        assert_eq!(spans.len(), 4);
        // Context: from the new side.
        assert!(spans[0].contains(&(0..2, Token::Keyword)));
        // Removed: highlighted as part of old_source — a comment.
        assert_eq!(spans[1], vec![(0..7, Token::Comment)]);
        // Added: highlighted as part of new_source — `let` keyword.
        assert!(spans[2].contains(&(4..7, Token::Keyword)));
    }

    #[test]
    fn hunk_syntax_guardrails() {
        let rows = vec![ctx(1, 1, "fn f() {}")];
        // No language → no spans.
        assert_eq!(hunk_syntax(None, &rows), vec![Vec::new()]);
        // Over-long line stays plain even when the hunk is highlighted.
        let lang = syntax::language_for_path("x.rs");
        let long = format!("// {}", "x".repeat(5000));
        let rows = vec![ctx(1, 1, "fn f() {}"), ctx(2, 2, &long)];
        let spans = hunk_syntax(lang, &rows);
        assert!(!spans[0].is_empty());
        assert!(spans[1].is_empty());
    }

    fn pr(number: u64, title: &str, author: &str, branch: &str) -> gh::PrSummary {
        gh::PrSummary {
            number,
            title: title.to_string(),
            author: gh::Author {
                login: author.to_string(),
            },
            state: "OPEN".to_string(),
            is_draft: false,
            head_ref_name: branch.to_string(),
            updated_at: "2026-07-01T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn pr_filter_empty_query_keeps_original_order() {
        let all = vec![
            pr(3, "fix crash", "alice", "fix-crash"),
            pr(1, "add feature", "bob", "feat"),
            pr(2, "docs", "carol", "docs"),
        ];
        assert_eq!(filter_prs(&all, ""), vec![0, 1, 2]);
        assert_eq!(filter_prs(&all, "   "), vec![0, 1, 2]);
    }

    #[test]
    fn pr_filter_matches_number_title_author_and_branch() {
        let all = vec![
            pr(3, "fix crash", "alice", "fix-crash"),
            pr(1, "add feature", "bob", "feat"),
        ];
        assert_eq!(filter_prs(&all, "#3"), vec![0]);
        assert_eq!(filter_prs(&all, "bob"), vec![1]);
        assert_eq!(filter_prs(&all, "crash"), vec![0]);
        assert!(filter_prs(&all, "zzzqqq").is_empty());
    }

    #[test]
    fn queue_selection_moves_and_clamps() {
        // Movement in bounds, both directions, from a middle position.
        assert_eq!(move_selection(1, 3, 1), 2);
        assert_eq!(move_selection(1, 3, -1), 0);
        // Clamped at both ends — j/k can never fall off the list.
        assert_eq!(move_selection(0, 3, -1), 0);
        assert_eq!(move_selection(2, 3, 1), 2);
        // An empty list has nothing to select, however far you move.
        assert_eq!(move_selection(0, 0, 1), 0);
        assert_eq!(move_selection(2, 0, -1), 0);
        // Delta 0 re-clamps in place: how a refresh keeps the selection
        // position when the queue shrinks.
        assert_eq!(move_selection(5, 3, 0), 2);
    }

    /// A queue row's label colors: GitHub's six-hex form parses, and only
    /// that form — anything else degrades the chip to neutral, never
    /// breaks the row.
    #[test]
    fn queue_label_colors_parse_githubs_hex() {
        assert_eq!(
            label_color("1D76DB"),
            Some(Hsla::from(Rgba {
                r: 0x1d as f32 / 255.0,
                g: 0x76 as f32 / 255.0,
                b: 0xdb as f32 / 255.0,
                a: 1.0,
            }))
        );
        for malformed in ["", "1D76D", "1D76DBB", "#1D76DB", "GHIJKL", "1D 6DB"] {
            assert_eq!(
                label_color(malformed),
                None,
                "{malformed:?} isn't a label color"
            );
        }
    }

    #[test]
    fn queue_label_chips_stay_readable_in_both_palettes() {
        // A GitHub label color, mid-tone blue.
        let blue = label_color("1D76DB").unwrap();
        // Dark mode: text lifts toward white, fill stays tinted.
        let (bg, text, border) = label_chip_colors(Some(blue), false);
        assert!(
            text.l > blue.l,
            "dark-mode text must be lighter than the base"
        );
        assert_eq!(bg.a, 0.12);
        assert!(border.a > 0.0);
        // Light mode: text deepens toward black instead.
        let (_, text, _) = label_chip_colors(Some(blue), true);
        assert!(
            text.l < blue.l,
            "light-mode text must be darker than the base"
        );
        // No color at all: neutral grays, so the chip still reads.
        let (bg, text, _) = label_chip_colors(None, false);
        assert_eq!(bg, Hsla::from(theme::surface0()).opacity(0.4));
        assert_eq!(text, Hsla::from(theme::subtext()));
    }

    #[test]
    fn queue_label_mixing_takes_the_short_way_round() {
        let red = Hsla {
            h: 0.95,
            s: 1.0,
            l: 0.5,
            a: 1.0,
        };
        let yellow = Hsla {
            h: 0.05,
            s: 1.0,
            l: 0.5,
            a: 1.0,
        };
        // Near-equality: hues wrap through rem_euclid, so endpoints land a
        // float's whisker off the originals — visible to ==, not to eyes.
        let close = |a: Hsla, b: Hsla| {
            (a.h - b.h).abs() < 1e-6
                && (a.s - b.s).abs() < 1e-6
                && (a.l - b.l).abs() < 1e-6
                && (a.a - b.a).abs() < 1e-6
        };
        // Endpoints are (near enough to) exact.
        assert!(close(mix_hsla(red, yellow, 0.0), red));
        assert!(close(mix_hsla(red, yellow, 1.0), yellow));
        // Halfway wraps through 1.0 → 0.0 (red → yellow the short way),
        // not the long way through green.
        assert_eq!(mix_hsla(red, yellow, 0.5).h, 0.0);
        // Alpha lerps with the rest.
        assert_eq!(mix_hsla(red, Hsla { a: 0.0, ..yellow }, 0.5).a, 0.5);
    }

    /// The label budget: three chips, then a count — the row keeps its
    /// room for size and state no matter how many labels a PR carries.
    #[test]
    fn queue_rows_show_capped_labels_with_an_overflow_count() {
        let label = |name: &str| github::QueueLabel {
            name: name.to_string(),
            color: String::new(),
        };
        let few = vec![label("api"), label("frontend")];
        assert_eq!(label_budget(&few).0.len(), 2);
        assert_eq!(label_budget(&few).1, 0);
        let many: Vec<_> = (0..5).map(|_| label("x")).collect();
        let (shown, overflow) = label_budget(&many);
        assert_eq!(shown.len(), QUEUE_LABELS_SHOWN);
        assert_eq!(overflow, 2);
    }

    /// The CI dot's palette wiring: each verdict paints its own color —
    /// the acceptance "green vs red" rides on exactly these mappings.
    #[test]
    fn queue_ci_dots_paint_their_verdicts() {
        assert_eq!(ci_dot_color(github::CiState::Pass), theme::green());
        assert_eq!(ci_dot_color(github::CiState::Fail), theme::red());
        assert_eq!(ci_dot_color(github::CiState::Pending), theme::yellow());
        assert_eq!(ci_dot_color(github::CiState::NoChecks), theme::overlay0());
    }

    /// Size stays absent until the enrichment has seen both halves — a
    /// half-known PR shows nothing rather than a misleading number.
    #[test]
    fn queue_size_waits_for_the_whole_diff() {
        assert!(size_view(None, None).is_none());
        assert!(size_view(Some(151), None).is_none());
        assert!(size_view(None, Some(30)).is_none());
        assert!(size_view(Some(151), Some(30)).is_some());
    }

    /// A queue row for the chip tests: a fully-formed [`QueueItem`] with
    /// only the fields the chips derive from filled in.
    fn queued(number: u64, repository: &str, author: &str) -> github::QueueItem {
        github::QueueItem {
            number,
            title: format!("pr {number}"),
            author: author.to_string(),
            author_avatar_url: None,
            repository: repository.to_string(),
            is_draft: false,
            review_state: github::ReviewState::Awaiting,
            ci: github::CiState::NoChecks,
            additions: None,
            deletions: None,
            comments: 0,
            labels: Vec::new(),
            updated_at: String::new(),
        }
    }

    /// The `kind` group from a derivation — the bar skips dimensions with
    /// nothing to offer, so positions in the vec aren't stable.
    fn group(groups: &[QueueChipGroup], kind: github::QueueFilterKind) -> &QueueChipGroup {
        groups
            .iter()
            .find(|group| group.kind == kind)
            .expect("the kind must be on the bar")
    }

    /// Chips derive from the rows the queue is showing: one group per
    /// dimension that can narrow, each value with the rows it covers,
    /// heaviest first with alphabetical ties.
    #[test]
    fn queue_chips_derive_from_the_current_rows() {
        let prs = vec![
            queued(1, "acme/widgets", "alice"),
            queued(2, "acme/widgets", "bob"),
            queued(3, "acme/gadgets", "alice"),
            queued(4, "globex/thing", "bob"),
        ];
        let groups = queue_chips(&prs, &github::QueueSearch::review_requested());
        // Orgs in kind order first, then repos, then authors — every
        // dimension here has two or more values.
        assert_eq!(
            groups.iter().map(|group| group.kind).collect::<Vec<_>>(),
            vec![
                github::QueueFilterKind::Org,
                github::QueueFilterKind::Repo,
                github::QueueFilterKind::Author,
            ]
        );
        // Heaviest first, whatever the alphabetical order would say.
        let org = group(&groups, github::QueueFilterKind::Org);
        assert_eq!(org.chips[0].filter, github::QueueFilter::org("acme"));
        assert_eq!(org.chips[0].count, 3);
        assert!(!org.chips[0].active);
        assert_eq!(org.chips[1].filter, github::QueueFilter::org("globex"));
        let repo = group(&groups, github::QueueFilterKind::Repo);
        assert_eq!(
            repo.chips[0].filter,
            github::QueueFilter::repo("acme/widgets")
        );
        assert_eq!(repo.chips[0].count, 2);
        assert_eq!(
            repo.chips[1].filter,
            github::QueueFilter::repo("acme/gadgets")
        );
        assert_eq!(
            repo.chips[2].filter,
            github::QueueFilter::repo("globex/thing")
        );
        // alice and bob both cover 2 rows: the tie breaks alphabetically.
        let authors = group(&groups, github::QueueFilterKind::Author);
        assert_eq!(
            authors.chips[0].filter,
            github::QueueFilter::author("alice")
        );
        assert_eq!(authors.chips[1].filter, github::QueueFilter::author("bob"));
        assert_eq!(groups.iter().map(|group| group.hidden).sum::<usize>(), 0);
    }

    /// A dimension with a single value has nothing to narrow: it stays
    /// off the bar instead of offering a chip that couldn't visibly change
    /// anything — but while it's the active filter its chip returns, as
    /// the one-glance readout of what's filtering the queue.
    #[test]
    fn queue_chips_stay_off_the_bar_until_they_can_narrow() {
        let prs = vec![
            queued(1, "acme/widgets", "alice"),
            queued(2, "acme/widgets", "bob"),
        ];
        // One org, one repo, two authors: only the authors offer a choice.
        let groups = queue_chips(&prs, &github::QueueSearch::review_requested());
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].kind, github::QueueFilterKind::Author);
        assert_eq!(groups[0].chips.len(), 2);
        // Active brings a single-value dimension back — marked, and the
        // only chip in its group.
        let search =
            github::QueueSearch::review_requested().filtered_by(github::QueueFilter::org("acme"));
        let groups = queue_chips(&prs, &search);
        let org = group(&groups, github::QueueFilterKind::Org);
        assert_eq!(org.chips.len(), 1);
        assert!(org.chips[0].active);
        assert_eq!(org.chips[0].filter, github::QueueFilter::org("acme"));
        assert_eq!(org.hidden, 0);
    }

    /// The active chip pins to the front of its group — even when it's the
    /// lightest value there — and reads back marked, so the one-glance
    /// rule holds under the cap too.
    #[test]
    fn queue_chips_pin_the_active_value_first() {
        let prs = vec![
            queued(1, "acme/widgets", "alice"),
            queued(2, "acme/widgets", "alice"),
            queued(3, "acme/gadgets", "bob"),
        ];
        let search =
            github::QueueSearch::review_requested().filtered_by(github::QueueFilter::author("bob"));
        let groups = queue_chips(&prs, &search);
        let authors = group(&groups, github::QueueFilterKind::Author);
        assert_eq!(authors.chips[0].filter, github::QueueFilter::author("bob"));
        assert_eq!(authors.chips[0].count, 1);
        assert!(authors.chips[0].active);
        assert_eq!(
            authors.chips[1].filter,
            github::QueueFilter::author("alice")
        );
        assert!(!authors.chips[1].active);
    }

    /// What the queue holds after narrowing to one repo: the bar offers
    /// only that repo — the active chip, visible and marked. The org and
    /// author dimensions now hold a single value each with nothing
    /// active, so they leave the bar: nothing left to narrow.
    #[test]
    fn queue_chips_derive_from_the_filtered_queue() {
        let prs = vec![queued(3, "acme/gadgets", "alice")];
        let search = github::QueueSearch::review_requested()
            .filtered_by(github::QueueFilter::repo("acme/gadgets"));
        let groups = queue_chips(&prs, &search);
        assert_eq!(groups.len(), 1, "only the active repo's dimension remains");
        let repo = group(&groups, github::QueueFilterKind::Repo);
        assert_eq!(
            repo.chips[0].filter,
            github::QueueFilter::repo("acme/gadgets")
        );
        assert!(repo.chips[0].active);
        // With the repo chip lifted too, a single row can't offer
        // anything to narrow: the bar empties.
        let groups = queue_chips(
            &prs,
            &search.clone().without_filter(github::QueueFilterKind::Repo),
        );
        assert!(
            groups.is_empty(),
            "a single row can't offer anything to narrow"
        );
    }

    /// Ten authors: the bar shows the cap and reports the rest — bounded
    /// strip, honest count. All counts equal here, so alphabetical order
    /// decides what shows.
    #[test]
    fn queue_chips_cap_each_group_with_an_overflow_count() {
        let prs: Vec<_> = (0..10)
            .map(|n| queued(n, "acme/widgets", &format!("user{n:02}")))
            .collect();
        let groups = queue_chips(&prs, &github::QueueSearch::review_requested());
        let authors = group(&groups, github::QueueFilterKind::Author);
        assert_eq!(authors.chips.len(), QUEUE_CHIPS_SHOWN);
        assert_eq!(authors.hidden, 10 - QUEUE_CHIPS_SHOWN);
        assert_eq!(
            authors.chips[0].filter,
            github::QueueFilter::author("user00")
        );
        assert_eq!(
            authors.chips[QUEUE_CHIPS_SHOWN - 1].filter,
            github::QueueFilter::author("user07")
        );
    }

    /// A malformed repository row degrades honestly: the whole string
    /// stands in as the org rather than panicking or vanishing — and it
    /// counts as its own value, so a queue mixing malformed and normal
    /// rows still offers both.
    #[test]
    fn queue_chips_degrade_a_malformed_repository() {
        let prs = vec![
            queued(1, "not-a-slug", "alice"),
            queued(2, "acme/widgets", "alice"),
        ];
        let groups = queue_chips(&prs, &github::QueueSearch::review_requested());
        let org = group(&groups, github::QueueFilterKind::Org);
        assert_eq!(org.chips.len(), 2);
        assert_eq!(org.chips[0].filter, github::QueueFilter::org("acme"));
        assert_eq!(org.chips[1].filter, github::QueueFilter::org("not-a-slug"));
        let repo = group(&groups, github::QueueFilterKind::Repo);
        assert_eq!(
            repo.chips[0].filter,
            github::QueueFilter::repo("acme/widgets")
        );
        assert_eq!(
            repo.chips[1].filter,
            github::QueueFilter::repo("not-a-slug")
        );
    }

    /// A queue-shaped facade for the app-level tests: scripted search
    /// results keyed by what the app asks for, recording every query it
    /// compiled — the observer for the exact chain a chip click runs.
    /// The scripted rows are deliberately one-org/one-repo — the shape a
    /// real queue often has — so the tests also pin that narrowing by
    /// author works even when org and repo can't narrow anything.
    struct FakeQueue {
        asked: std::sync::Mutex<Vec<(String, (&'static str, &'static str))>>,
    }

    impl FakeQueue {
        fn rows(&self, query: &str) -> Vec<github::QueueItem> {
            for author in ["alice", "bob"] {
                if query.contains(&format!("author:{author}")) {
                    return vec![queued(
                        if author == "alice" { 1 } else { 2 },
                        "acme/widgets",
                        author,
                    )];
                }
            }
            vec![
                queued(1, "acme/widgets", "alice"),
                queued(2, "acme/widgets", "bob"),
            ]
        }
    }

    impl github::GitHubClient for FakeQueue {
        fn viewer(&self) -> Result<github::Viewer, github::ClientError> {
            Ok(github::Viewer {
                login: "tester".into(),
                avatar_url: String::new(),
            })
        }

        fn search_prs(
            &self,
            search: &github::QueueSearch,
        ) -> Result<Vec<github::QueueItem>, github::ClientError> {
            self.asked
                .lock()
                .unwrap()
                .push((search.query(), search.sort()));
            Ok(self.rows(&search.query()))
        }
    }

    /// A window running the real app over the scripted queue, parked
    /// after the app-open fetch: `(window, app)` ready to drive.
    fn queue_window(
        cx: &mut gpui::TestAppContext,
        client: Arc<dyn github::GitHubClient>,
    ) -> (gpui::AnyWindowHandle, gpui::Entity<ReviewApp>) {
        queue_window_with_store(cx, client, local_store::LocalStore::in_memory())
    }

    /// The same window, over a store the test controls — so a test can
    /// seed a cache before the app opens and watch it paint from it.
    fn queue_window_with_store(
        cx: &mut gpui::TestAppContext,
        client: Arc<dyn github::GitHubClient>,
        store: local_store::LocalStore,
    ) -> (gpui::AnyWindowHandle, gpui::Entity<ReviewApp>) {
        cx.update(|cx| {
            gpui_component::init(cx);
            theme::apply_ui_theme(None, cx);
        });
        let stash = std::rc::Rc::new(std::cell::RefCell::new(None::<gpui::Entity<ReviewApp>>));
        let stash2 = stash.clone();
        let window = cx.add_window(move |window, cx| {
            let view = cx.new(|cx| {
                ReviewApp::new(
                    Vec::new(),
                    Vec::new(),
                    client.clone(),
                    store,
                    None,
                    window,
                    cx,
                )
            });
            window.focus(&view.read(cx).focus_handle);
            *stash2.borrow_mut() = Some(view.clone());
            Root::new(view, window, cx)
        });
        let app = stash.borrow().clone().unwrap();
        cx.run_until_parked();
        (window.into(), app)
    }

    /// The whole chain a chip click runs — handler → search update →
    /// background fetch → landing → row swap — through the real app in a
    /// test window. A broken link anywhere shows up as rows that never
    /// narrow, exactly what the queue must never do. (The mouse and the
    /// real `gh` subprocess have their own tests.)
    #[gpui::test]
    fn a_chip_click_narrows_the_queue(cx: &mut gpui::TestAppContext) {
        let fake = Arc::new(FakeQueue {
            asked: std::sync::Mutex::new(Vec::new()),
        });
        let (_window, app) = queue_window(cx, fake.clone());

        // The app-open fetch landed: the default queue, both authors, in
        // the default order.
        cx.update(|cx| assert_eq!(app.read(cx).queue.prs.len(), 2));
        assert_eq!(
            fake.asked.lock().unwrap().as_slice(),
            &[(
                "is:pr is:open review-requested:@me".to_string(),
                ("updated", "desc")
            )]
        );

        // Click the bob chip: the search narrows, the fetch that lands is
        // the one the chip asked for, and the rows swap to just bob's.
        cx.update(|cx| {
            app.update(cx, |this, cx| {
                this.queue_toggle_filter(github::QueueFilter::author("bob"), cx)
            })
        });
        cx.run_until_parked();
        cx.update(|cx| {
            assert_eq!(
                app.read(cx).queue.search.filters(),
                &[github::QueueFilter::author("bob")]
            );
            assert_eq!(app.read(cx).queue.prs.len(), 1, "the queue must narrow");
            assert_eq!(app.read(cx).queue.prs[0].author, "bob");
        });
        assert_eq!(
            fake.asked.lock().unwrap().last().unwrap(),
            &(
                "is:pr is:open review-requested:@me author:bob".to_string(),
                ("updated", "desc")
            )
        );

        // Click the active chip again: the dimension lifts, and the rows
        // come back.
        cx.update(|cx| {
            app.update(cx, |this, cx| {
                this.queue_toggle_filter(github::QueueFilter::author("bob"), cx)
            })
        });
        cx.run_until_parked();
        cx.update(|cx| {
            assert!(app.read(cx).queue.search.filters().is_empty());
            assert_eq!(app.read(cx).queue.prs.len(), 2);
        });

        // `s` flips the sort: the next fetch carries the flipped spec —
        // the same re-run-a-different-search machinery, order side.
        cx.update(|cx| app.update(cx, |this, cx| this.queue_toggle_sort(cx)));
        cx.run_until_parked();
        cx.update(|cx| {
            assert_eq!(
                app.read(cx).queue.search.ordering(),
                github::QueueSort::OldestUpdated
            )
        });
        assert_eq!(
            fake.asked.lock().unwrap().last().unwrap(),
            &(
                "is:pr is:open review-requested:@me".to_string(),
                ("updated", "asc")
            )
        );
    }

    /// A queue that never answers — the offline case: the facade fails
    /// every search, and the app must not fall over, must not clear the
    /// rows it has, and must say what went wrong.
    struct OfflineQueue {
        asked: std::sync::Mutex<usize>,
    }

    impl github::GitHubClient for OfflineQueue {
        fn viewer(&self) -> Result<github::Viewer, github::ClientError> {
            Err(github::ClientError::Failed("offline".into()))
        }

        fn search_prs(
            &self,
            _search: &github::QueueSearch,
        ) -> Result<Vec<github::QueueItem>, github::ClientError> {
            *self.asked.lock().unwrap() += 1;
            Err(github::ClientError::Failed(
                "error connecting to api.github.com".into(),
            ))
        }
    }

    /// The cache-first paint: with a warm cache, the app paints the
    /// cached rows the instant it opens — before the network has answered
    /// — and the fresh rows hot-swap in when the fetch lands.
    ///
    /// The two halves are checked separately: the cache paint is what
    /// makes the app open instantly, and the hot swap is what keeps it
    /// honest.
    #[gpui::test]
    fn a_warm_cache_paints_the_queue_before_the_network_answers(cx: &mut gpui::TestAppContext) {
        let store = local_store::LocalStore::in_memory();
        // Seed the cache with the rows the last run saw.
        store.cache_queue(
            &github::QueueSearch::review_requested(),
            &[
                queued(1, "acme/widgets", "alice"),
                queued(2, "acme/widgets", "bob"),
            ],
        );
        // A facade that never answers: the cache paint is the only thing
        // that can put rows on screen.
        let fake = Arc::new(OfflineQueue {
            asked: std::sync::Mutex::new(0),
        });
        let (_window, app) = queue_window_with_store(cx, fake.clone(), store);

        cx.update(|cx| {
            let app = app.read(cx);
            assert_eq!(app.queue.prs.len(), 2, "the cached rows must paint");
            assert_eq!(app.queue.origin, QueueOrigin::Cached);
            // The network was asked, and failed: the cached rows stay.
            assert!(app.queue.error.is_some(), "the failure must be surfaced");
        });
        assert!(
            *fake.asked.lock().unwrap() >= 1,
            "the app must still ask the network"
        );
    }

    /// The hot swap: the fresh rows replace the cached ones when the
    /// fetch lands, and the queue says it is fresh.
    #[gpui::test]
    fn the_fresh_queue_hot_swaps_in_over_the_cached_one(cx: &mut gpui::TestAppContext) {
        let store = local_store::LocalStore::in_memory();
        store.cache_queue(
            &github::QueueSearch::review_requested(),
            &[queued(1, "acme/widgets", "alice")],
        );
        let fake = Arc::new(FakeQueue {
            asked: std::sync::Mutex::new(Vec::new()),
        });
        let (_window, app) = queue_window_with_store(cx, fake.clone(), store);

        cx.update(|cx| {
            let app = app.read(cx);
            assert_eq!(
                app.queue.prs.len(),
                2,
                "the fresh queue replaced the cached one"
            );
            assert_eq!(app.queue.origin, QueueOrigin::Fresh);
            assert!(app.queue.error.is_none());
        });
    }

    /// The fetched queue lands in the cache: the next launch paints it
    /// before the network is asked. Without this the cache would stay
    /// empty forever and the app would never be instant.
    #[gpui::test]
    fn the_fetched_queue_lands_in_the_cache(cx: &mut gpui::TestAppContext) {
        let store = local_store::LocalStore::in_memory();
        let fake = Arc::new(FakeQueue {
            asked: std::sync::Mutex::new(Vec::new()),
        });
        let (_window, _app) = queue_window_with_store(cx, fake.clone(), store.clone());

        let cached = store.cached_queue();
        assert_eq!(cached.prs.len(), 2, "the fetched queue must be cached");
        assert_eq!(cached.search, "is:pr is:open review-requested:@me");
        assert_eq!(cached.sort, "updated");
        assert_eq!(cached.order, "desc");
        assert!(
            cached.fetched_at_ms > 0,
            "the cache must know when it was written"
        );
    }

    /// The offline launch: a warm cache, a network that never answers.
    /// The app opens, shows the cached queue, and reports the failure
    /// without losing the rows.
    #[gpui::test]
    fn an_offline_launch_shows_the_cached_queue(cx: &mut gpui::TestAppContext) {
        let store = local_store::LocalStore::in_memory();
        store.cache_queue(
            &github::QueueSearch::review_requested(),
            &[
                queued(1, "acme/widgets", "alice"),
                queued(2, "acme/widgets", "bob"),
            ],
        );
        let fake = Arc::new(OfflineQueue {
            asked: std::sync::Mutex::new(0),
        });
        let (_window, app) = queue_window_with_store(cx, fake.clone(), store);

        cx.update(|cx| {
            let app = app.read(cx);
            assert_eq!(
                app.queue.prs.len(),
                2,
                "the cached queue must survive an offline launch"
            );
            assert!(app.queue.error.is_some());
            // The rows are usable: the selection can still move.
            assert_eq!(app.queue.selected, 0);
        });
    }

    /// One item per source: a PR is its owner/repo/number, a local diff
    /// is its working tree and base — anything else is a different thing
    /// to review and opens its own item.
    #[test]
    fn sources_match_on_identity_not_labels() {
        let pr = |owner: &str, number: u64| {
            Source::Pr(gh::PrLocator {
                owner: owner.into(),
                repo: "widgets".into(),
                number,
            })
        };
        let local = |base: Option<&str>| {
            Source::Local(git::LocalSource {
                branch: "main".into(),
                base_ref: base.map(str::to_string),
                base_label: base.unwrap_or("…").into(),
                base_oid: None,
                repo_root: "/tmp/repo".into(),
            })
        };
        // Same PR, same local tree+base: the same thing.
        assert!(sources_match(&pr("acme", 1), &pr("acme", 1)));
        assert!(sources_match(&local(Some("main")), &local(Some("main"))));
        // Different number, different owner, different base: not the same.
        assert!(!sources_match(&pr("acme", 1), &pr("acme", 2)));
        assert!(!sources_match(&pr("acme", 1), &pr("beta", 1)));
        assert!(!sources_match(&local(Some("main")), &local(Some("dev"))));
        assert!(!sources_match(&local(Some("main")), &local(None)));
        // And never across kinds.
        assert!(!sources_match(&pr("acme", 1), &local(None)));
    }

    /// The locator a row opens: the repository's `owner/name` plus the
    /// number — everything ⌘T's open input parses into, derived from
    /// what the row already carries. A malformed repository degrades
    /// honestly: no locator, so Enter on it does nothing.
    #[test]
    fn queue_rows_carry_the_locator_they_open() {
        let pr = queued(7, "acme/widgets", "alice");
        let loc = ReviewApp::queue_item_locator(&pr).expect("owner/name parses");
        assert_eq!(
            (loc.owner.as_str(), loc.repo.as_str(), loc.number),
            ("acme", "widgets", 7)
        );
        assert!(ReviewApp::queue_item_locator(&queued(7, "widgets", "alice")).is_none());
        assert!(ReviewApp::queue_item_locator(&queued(7, "", "alice")).is_none());
    }

    /// The tracer bullet's first half: Enter on a queue row opens that
    /// PR into the changes view — its own item, the pane leaves the
    /// queue screen, and the sidebar (whose file tree is the item's
    /// navigation) is showing. `u` then comes home without closing
    /// anything, and the queue remembers exactly where it was left.
    #[gpui::test]
    fn enter_opens_the_selected_pr_and_u_comes_home(cx: &mut gpui::TestAppContext) {
        let fake = Arc::new(FakeQueue {
            asked: std::sync::Mutex::new(Vec::new()),
        });
        let (_window, app) = queue_window(cx, fake);

        cx.update(|cx| {
            app.update(cx, |this, cx| {
                // j to the second row (bob, acme/widgets#2), Enter to open.
                this.queue_move(1, cx);
                this.queue_open(this.queue.selected, cx);
                // The PR opened as its own item, and the pane is showing it.
                assert!(!this.show_queue, "the changes view must be up");
                assert_eq!(this.items.len(), 1);
                assert_eq!(this.active, 0);
                assert!(matches!(this.items[0].state, ItemState::Loading));
                let Source::Pr(loc) = &this.items[0].source else {
                    panic!("a queue row must open a PR");
                };
                assert_eq!(
                    (loc.owner.as_str(), loc.repo.as_str(), loc.number),
                    ("acme", "widgets", 2)
                );
                // The sidebar carries the file tree: it must be showing.
                assert!(this.sidebar_visible);
                // `u`: back to the queue — the item stays open behind the
                // scenes (⌘K / ctrl-tab reach it) and the queue keeps the
                // selection where it was left.
                this.back_to_queue(cx);
                assert!(this.show_queue, "u must come home");
                assert_eq!(this.items.len(), 1, "the PR must stay open");
                assert_eq!(this.queue.selected, 1, "the queue keeps its place");
            });
        });
    }

    /// The other side of the same glue: opening a PR the session already
    /// has brings its tab forward — a duplicate would be a way to lose
    /// your place in the first copy — and a row without a well-formed
    /// repository opens nothing at all.
    #[gpui::test]
    fn an_already_open_pr_comes_forward_instead_of_duplicating(cx: &mut gpui::TestAppContext) {
        let fake = Arc::new(FakeQueue {
            asked: std::sync::Mutex::new(Vec::new()),
        });
        let (_window, app) = queue_window(cx, fake);

        // Two PRs already open (as the session would have them after two
        // Enters), the second one in front.
        cx.update(|cx| {
            app.update(cx, |this, _cx| {
                this.items = vec![
                    ReviewItem {
                        id: 0,
                        source: Source::Pr(gh::PrLocator {
                            owner: "acme".into(),
                            repo: "widgets".into(),
                            number: 1,
                        }),
                        state: ItemState::Failed("stale".into()),
                        reloading: false,
                        refresh_error: None,
                        upgrade_gen: 0,
                    },
                    ReviewItem {
                        id: 1,
                        source: Source::Pr(gh::PrLocator {
                            owner: "acme".into(),
                            repo: "widgets".into(),
                            number: 2,
                        }),
                        state: ItemState::Failed("stale".into()),
                        reloading: false,
                        refresh_error: None,
                        upgrade_gen: 0,
                    },
                ];
                this.active = 1;
                this.next_id = 2;
            });
        });

        // Enter on the first row (alice, acme/widgets#1): its tab comes
        // forward — no fetch, no third item.
        cx.update(|cx| {
            app.update(cx, |this, cx| {
                this.queue_open(0, cx);
                assert_eq!(this.items.len(), 2, "no duplicate may open");
                assert_eq!(this.active, 0, "the open tab comes forward");
                assert!(!this.show_queue);
                assert!(this.sidebar_visible);

                // A row whose repository isn't owner/name opens nothing.
                this.back_to_queue(cx);
                this.queue.prs[0].repository = "not-a-slug".into();
                this.queue_open(0, cx);
                assert!(this.show_queue, "a malformed row is a no-op");
                assert_eq!(this.items.len(), 2);
            });
        });
    }

    /// The queue's search comes back from the last run: the chips and
    /// the sort, rebuilt into the same search — so the app opens the
    /// queue the reviewer left, and the cache it paints belongs to that
    /// search.
    #[test]
    fn the_queue_search_survives_a_restart() {
        let store = local_store::LocalStore::in_memory();
        let search = github::QueueSearch::review_requested()
            .filtered_by(github::QueueFilter::author("bob"))
            .sorted_by(github::QueueSort::OldestUpdated);
        store.set_setting(
            "queue_search",
            serde_json::json!({
                "filters": [{"kind": "author", "value": "bob"}],
                "sort": "oldest",
            }),
        );
        let app_search = restore_queue_search(store.setting("queue_search"));
        assert_eq!(app_search.query(), search.query());
        assert_eq!(app_search.ordering(), search.ordering());
        assert_eq!(app_search.filters(), search.filters());
    }

    /// A setting the app doesn't understand reads as the default queue:
    /// a corrupt preference must never keep the app from starting.
    #[test]
    fn an_unreadable_queue_search_setting_reads_as_the_default_queue() {
        let default = restore_queue_search(None);
        assert_eq!(default.query(), "is:pr is:open review-requested:@me");
        assert_eq!(default.ordering(), github::QueueSort::RecentlyUpdated);
        // A blob of the wrong shape.
        let drifted = restore_queue_search(Some(serde_json::json!("nonsense")));
        assert_eq!(drifted.query(), "is:pr is:open review-requested:@me");
        // A filter kind this build doesn't know is dropped, the rest kept.
        let mixed = restore_queue_search(Some(serde_json::json!({
            "filters": [
                {"kind": "author", "value": "bob"},
                {"kind": "colour", "value": "blue"},
            ],
            "sort": "oldest",
        })));
        assert_eq!(
            mixed.query(),
            "is:pr is:open review-requested:@me author:bob"
        );
        assert_eq!(mixed.ordering(), github::QueueSort::OldestUpdated);
    }

    /// The freshness bookkeeping: a fetched PR's snapshot lands in the
    /// store with its head SHA and its threads, so the next fetch can be
    /// compared against it — "what changed since I last looked".
    #[test]
    fn a_fetched_pr_leaves_its_freshness_bookkeeping() {
        let store = local_store::LocalStore::in_memory();
        let snapshot = local_store::StoredPrSnapshot {
            repository: "acme/widgets".into(),
            number: 7,
            title: "Add deactivation filters".into(),
            body: String::new(),
            author: "alice".into(),
            head_sha: "abc123".into(),
            base_ref: "main".into(),
            head_ref: "feature/deactivation".into(),
            threads: vec![local_store::StoredThreadSeen {
                id: "T_1".into(),
                updated_at: "2026-10-20T08:07:08Z".into(),
                resolved: false,
            }],
            is_draft: false,
            fetched_at_ms: 1_700_000_000_000,
        };
        store.save_snapshot(&snapshot);

        // The head SHA is its own column, so "has the head moved?" is a
        // cheap comparison.
        assert_eq!(store.head_sha("acme/widgets", 7).as_deref(), Some("abc123"));
        // The snapshot knows when it was taken.
        assert_eq!(
            store.snapshot_fetched_at_ms("acme/widgets", 7),
            Some(1_700_000_000_000)
        );
        // The threads the app saw are remembered, one by one.
        let threads = store.threads_seen("acme/widgets", 7);
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].id, "T_1");
        assert_eq!(threads[0].updated_at, "2026-10-20T08:07:08Z");
        // Another PR's bookkeeping is another PR's.
        assert!(store.threads_seen("acme/widgets", 8).is_empty());
    }

    /// A PR the app has a snapshot of can be opened without the network:
    /// the local store's copy stands in for the fetch that failed, so an
    /// offline reviewer can still read the PR.
    #[test]
    fn a_pr_opens_from_the_store_when_the_fetch_fails() {
        let store = local_store::LocalStore::in_memory();
        store.save_snapshot(&local_store::StoredPrSnapshot {
            repository: "acme/widgets".into(),
            number: 7,
            title: "Add deactivation filters".into(),
            body: "Some **markdown**.".into(),
            author: "alice".into(),
            head_sha: "abc123".into(),
            base_ref: "main".into(),
            head_ref: "feature/deactivation".into(),
            threads: Vec::new(),
            is_draft: false,
            fetched_at_ms: 1_700_000_000_000,
        });

        let mut item = ReviewItem {
            id: 1,
            source: Source::Pr(gh::PrLocator {
                owner: "acme".into(),
                repo: "widgets".into(),
                number: 7,
            }),
            state: ItemState::Loading,
            reloading: false,
            refresh_error: None,
            upgrade_gen: 0,
        };
        assert!(
            item.install_offline_snapshot(&store),
            "the stored snapshot must stand in for the failed fetch"
        );
        let ItemState::Ready(data) = &item.state else {
            panic!("the PR must be readable from the store");
        };
        let meta = data
            .pr_meta
            .as_ref()
            .expect("the PR's metadata must be there");
        assert_eq!(meta.title, "Add deactivation filters");
        assert_eq!(meta.body, "Some **markdown**.");
        assert_eq!(meta.author.login, "alice");
        assert_eq!(meta.head_ref_oid, "abc123");

        // A PR the store has never seen has nothing to stand in: the
        // caller falls back to the error.
        let mut unknown = ReviewItem {
            id: 2,
            source: Source::Pr(gh::PrLocator {
                owner: "acme".into(),
                repo: "widgets".into(),
                number: 8,
            }),
            state: ItemState::Loading,
            reloading: false,
            refresh_error: None,
            upgrade_gen: 0,
        };
        assert!(!unknown.install_offline_snapshot(&store));
        assert!(matches!(unknown.state, ItemState::Loading));
    }

    /// The focus refresh: the window coming back to the front re-runs the
    /// queue search. The reviewer comes back from the mail app and the
    /// queue is current again — without them thinking about it.
    #[gpui::test]
    fn the_window_focusing_refreshes_the_queue(cx: &mut gpui::TestAppContext) {
        let fake = Arc::new(FakeQueue {
            asked: std::sync::Mutex::new(Vec::new()),
        });
        let (window, app) = queue_window(cx, fake.clone());
        let asked_after_open = fake.asked.lock().unwrap().len();
        assert_eq!(asked_after_open, 1, "the app-open fetch runs once");

        // The window leaves the foreground and comes back: the app
        // refreshes on the way back. The activation observer is what
        // fires, so the test drives the window's active state directly.
        cx.update(|cx| app.update(cx, |this, cx| this.set_frontmost(false, cx)));
        cx.run_until_parked();
        cx.update(|cx| app.update(cx, |this, cx| this.set_frontmost(true, cx)));
        cx.run_until_parked();
        assert!(
            fake.asked.lock().unwrap().len() > asked_after_open,
            "the focus must trigger a refresh"
        );
        // The rows are still there — a refresh never clears the queue.
        cx.update(|cx| assert_eq!(app.read(cx).queue.prs.len(), 2));
        let _ = window;
    }

    /// The background poll: while the window is frontmost, the queue
    /// refreshes on its own every few minutes, and stops the moment the
    /// window goes to the background.
    #[gpui::test]
    fn the_background_poll_refreshes_while_frontmost(cx: &mut gpui::TestAppContext) {
        let fake = Arc::new(FakeQueue {
            asked: std::sync::Mutex::new(Vec::new()),
        });
        let (_window, app) = queue_window(cx, fake.clone());
        let asked_after_open = fake.asked.lock().unwrap().len();
        assert_eq!(asked_after_open, 1, "the app-open fetch runs once");

        // The window goes to the background: the poll must not run.
        cx.update(|cx| app.update(cx, |this, cx| this.set_frontmost(false, cx)));
        cx.run_until_parked();
        cx.executor().advance_clock(BACKGROUND_POLL_INTERVAL * 2);
        cx.run_until_parked();
        assert_eq!(
            fake.asked.lock().unwrap().len(),
            asked_after_open,
            "a backgrounded app must not poll"
        );

        // The window comes back: the poll runs again, and the queue
        // refreshes on its own.
        cx.update(|cx| app.update(cx, |this, cx| this.set_frontmost(true, cx)));
        cx.run_until_parked();
        let asked_after_focus = fake.asked.lock().unwrap().len();
        assert!(
            asked_after_focus > asked_after_open,
            "the focus must trigger a refresh"
        );
        cx.executor().advance_clock(BACKGROUND_POLL_INTERVAL);
        cx.run_until_parked();
        assert!(
            fake.asked.lock().unwrap().len() > asked_after_focus,
            "the poll must refresh the queue while frontmost"
        );
    }

    /// The mouse path itself: a real down-up click on a chip must narrow
    /// the queue — hitbox, element state, pending mouse-down, click
    /// listeners — everything between the platform mouse event and the
    /// handler the previous test drives directly. Headless tests can't
    /// read element bounds, so this one probes: first down the window's
    /// left edge until the sort chip answers (it's the control row's
    /// first element), then rightward along that row until an author chip
    /// does.
    #[gpui::test]
    fn the_mouse_click_path_narrows_the_queue(cx: &mut gpui::TestAppContext) {
        let fake = Arc::new(FakeQueue {
            asked: std::sync::Mutex::new(Vec::new()),
        });
        let (window, app) = queue_window(cx, fake.clone());
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        // Paint the element tree the mouse events hit-test against.
        vcx.update(|window, cx| {
            window.draw(cx);
        });

        let click = |vcx: &mut gpui::VisualTestContext, x: f32, y: f32| {
            let at = gpui::point(gpui::px(x), gpui::px(y));
            vcx.simulate_mouse_move(at, None, gpui::Modifiers::default());
            vcx.simulate_mouse_down(at, gpui::MouseButton::Left, gpui::Modifiers::default());
            vcx.simulate_mouse_up(at, gpui::MouseButton::Left, gpui::Modifiers::default());
            vcx.run_until_parked();
        };
        let ordering = |vcx: &gpui::VisualTestContext| {
            vcx.cx.update(|cx| app.read(cx).queue.search.ordering())
        };

        // Find the control row: the sort chip is its first element, and a
        // click on it flips the order — probe down until one does.
        let mut band = None;
        for y in (40..170).step_by(2) {
            click(&mut vcx, 20., y as f32);
            if ordering(&vcx) == github::QueueSort::OldestUpdated {
                band = Some(y as f32);
                break;
            }
        }
        let band = band.expect("no click ever reached the sort chip");
        // Restore the default order for the sweep.
        click(&mut vcx, 20., band);
        assert_eq!(
            ordering(&vcx),
            github::QueueSort::RecentlyUpdated,
            "the sort chip must flip back on a second click"
        );

        // Sweep rightward until an author chip answers. The sweep may
        // cross the sort chip again — restore it when it does, so the
        // state stays known.
        let mut clicked: Option<github::QueueFilter> = None;
        let mut x = 24.;
        while x < 640. {
            click(&mut vcx, x, band);
            if ordering(&vcx) == github::QueueSort::OldestUpdated {
                click(&mut vcx, 20., band);
            }
            let filters = vcx
                .cx
                .update(|cx| app.read(cx).queue.search.filters().to_vec());
            if let Some(filter) = filters
                .iter()
                .find(|filter| filter.kind() == github::QueueFilterKind::Author)
                .cloned()
            {
                clicked = Some(filter);
                break;
            }
            x += 3.;
        }
        let filter = clicked.expect("no click ever reached an author chip");

        // The click narrowed the queue — the mouse path works end to end.
        vcx.cx.update(|cx| {
            let app = app.read(cx);
            assert_eq!(app.queue.prs.len(), 1, "the click must narrow the queue");
            assert_eq!(app.queue.prs[0].author, filter.value());
            assert_eq!(app.queue.search.filters(), std::slice::from_ref(&filter));
        });
        assert!(fake
            .asked
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .0
            .contains("author:"));
    }

    /// (depth, display name, Some(file_ix) for files / None for dirs).
    fn flat(entries: &[TreeEntry]) -> Vec<(usize, &str, Option<usize>)> {
        entries
            .iter()
            .map(|e| {
                let file_ix = match &e.kind {
                    TreeEntryKind::Dir { .. } => None,
                    TreeEntryKind::File { file_ix } => Some(*file_ix),
                };
                (e.depth, e.name.as_ref(), file_ix)
            })
            .collect()
    }

    fn dir_paths(entries: &[TreeEntry]) -> Vec<&str> {
        entries
            .iter()
            .filter_map(|e| match &e.kind {
                TreeEntryKind::Dir { path } => Some(path.as_str()),
                TreeEntryKind::File { .. } => None,
            })
            .collect()
    }

    #[test]
    fn tree_nests_dirs_first_and_sorts_names() {
        let tree = build_tree(&["src/main.rs", "README.md", "src/lib.rs"]);
        assert_eq!(
            flat(&tree),
            vec![
                (0, "src", None),
                (1, "lib.rs", Some(2)),
                (1, "main.rs", Some(0)),
                (0, "README.md", Some(1)),
            ]
        );
        // No directories at all: a flat, sorted file list.
        let tree = build_tree(&["b.txt", "a.txt"]);
        assert_eq!(
            flat(&tree),
            vec![(0, "a.txt", Some(1)), (0, "b.txt", Some(0))]
        );
    }

    #[test]
    fn tree_compresses_single_child_dir_chains() {
        // core→flags has one child dir and no files: compressed. src has a
        // file of its own, so it is not folded into the chain.
        let tree = build_tree(&[
            "src/core/flags/defs.rs",
            "src/core/flags/parse.rs",
            "src/main.rs",
        ]);
        assert_eq!(
            flat(&tree),
            vec![
                (0, "src", None),
                (1, "core/flags", None),
                (2, "defs.rs", Some(0)),
                (2, "parse.rs", Some(1)),
                (1, "main.rs", Some(2)),
            ]
        );
        // The collapse key is the chain's full path.
        assert_eq!(dir_paths(&tree), vec!["src", "src/core/flags"]);

        // A chain starting at the root compresses too.
        let tree = build_tree(&["a/b/c.txt"]);
        assert_eq!(flat(&tree), vec![(0, "a/b", None), (1, "c.txt", Some(0))]);
        assert_eq!(dir_paths(&tree), vec!["a/b"]);

        // A dir with its own files stops the chain even with one child dir.
        let tree = build_tree(&["a/f.txt", "a/b/g.txt"]);
        assert_eq!(
            flat(&tree),
            vec![
                (0, "a", None),
                (1, "b", None),
                (2, "g.txt", Some(1)),
                (1, "f.txt", Some(0)),
            ]
        );
    }

    #[test]
    fn collapse_hides_subtrees() {
        let tree = build_tree(&[
            "src/core/flags/defs.rs",
            "src/core/flags/parse.rs",
            "src/main.rs",
            "README.md",
        ]);
        // flat: [src, core/flags, defs, parse, main.rs, README.md]
        let none = HashSet::new();
        assert_eq!(visible_entries(&tree, &none), vec![0, 1, 2, 3, 4, 5]);

        // Collapsing the inner chain hides its files but keeps the sibling.
        let inner = HashSet::from(["src/core/flags".to_string()]);
        assert_eq!(visible_entries(&tree, &inner), vec![0, 1, 4, 5]);

        // Collapsing src hides everything under it, collapsed child included.
        let outer = HashSet::from(["src".to_string(), "src/core/flags".to_string()]);
        assert_eq!(visible_entries(&tree, &outer), vec![0, 5]);
    }

    #[test]
    fn fuzzy_filter_flattens_and_ranks() {
        let paths = ["src/core/flags/defs.rs", "src/main.rs", "docs/guide.md"];
        assert_eq!(fuzzy_file_matches(&paths, "defs"), vec![0]);
        assert_eq!(fuzzy_file_matches(&paths, "guide"), vec![2]);
        assert!(fuzzy_file_matches(&paths, "zzzqqq").is_empty());
        // Empty / whitespace query: everything, diff order.
        assert_eq!(fuzzy_file_matches(&paths, ""), vec![0, 1, 2]);
        assert_eq!(fuzzy_file_matches(&paths, "  "), vec![0, 1, 2]);
        // Best score first: the contiguous match beats the spread-out one.
        let paths = ["main_test.rs", "main.rs"];
        assert_eq!(fuzzy_file_matches(&paths, "main.rs"), vec![1, 0]);
    }

    #[test]
    fn status_style_matches_file_header_tags() {
        assert_eq!(status_style(FileStatus::Added), ("added", theme::green()));
        assert_eq!(status_style(FileStatus::Deleted), ("deleted", theme::red()));
        assert_eq!(
            status_style(FileStatus::Modified),
            ("modified", theme::blue())
        );
        assert_eq!(
            status_style(FileStatus::Renamed),
            ("renamed", theme::mauve())
        );
        assert_eq!(status_style(FileStatus::Binary), ("binary", theme::peach()));
    }

    #[test]
    fn repo_slug_parsing() {
        assert_eq!(
            parse_repo_slug("BurntSushi/ripgrep"),
            Ok(("BurntSushi".to_string(), "ripgrep".to_string()))
        );
        assert!(parse_repo_slug("ripgrep").is_err());
        assert!(parse_repo_slug("/ripgrep").is_err());
        assert!(parse_repo_slug("BurntSushi/").is_err());
        assert!(parse_repo_slug("a/b/c").is_err());
    }

    #[test]
    fn source_options_filter_by_substring() {
        assert_eq!(filtered_sources(""), vec![0, 1]);
        assert_eq!(filtered_sources("pull"), vec![0]);
        assert_eq!(filtered_sources("FOLDER"), vec![1]);
        assert_eq!(filtered_sources("open"), vec![0, 1]);
        assert!(filtered_sources("nope").is_empty());
    }

    #[test]
    fn header_indices_are_correct_in_both_modes() {
        let diff = sample_diff();
        for mode in [ViewMode::Unified, ViewMode::Split] {
            let (rows, file_rows, hunk_rows) =
                build_rows(&diff, mode, &HashMap::new(), None, true, COMMENT_WRAP_CHARS);
            assert_eq!(file_rows.len(), 2);
            assert_eq!(hunk_rows.len(), 2);
            for &ix in &file_rows {
                assert!(matches!(rows[ix], Row::FileHeader { .. }));
            }
            for &ix in &hunk_rows {
                assert!(matches!(rows[ix], Row::HunkHeader { .. }));
            }
            // Binary file: header immediately followed by the binary row.
            assert!(matches!(rows[file_rows[1] + 1], Row::Binary));
        }
        // Unified emits one row per diff row; split collapses the equal run.
        let (unified, _, _) = build_rows(
            &diff,
            ViewMode::Unified,
            &HashMap::new(),
            None,
            true,
            COMMENT_WRAP_CHARS,
        );
        let (split, _, _) = build_rows(
            &diff,
            ViewMode::Split,
            &HashMap::new(),
            None,
            true,
            COMMENT_WRAP_CHARS,
        );
        let unified_lines = unified
            .iter()
            .filter(|r| matches!(r, Row::Line { .. }))
            .count();
        let split_lines = split
            .iter()
            .filter(|r| matches!(r, Row::SplitLine { .. }))
            .count();
        assert_eq!(unified_lines, 11);
        assert_eq!(split_lines, 8); // 4 (hunk 1) + 4 (hunk 2)
    }

    fn line(text: &str) -> Row {
        Row::Line {
            old_no: Some(1),
            new_no: Some(1),
            kind: LineKind::Context,
            text: text.to_string().into(),
            intra: Vec::new(),
            syntax: Vec::new(),
        }
    }

    fn split(left: Option<&str>, right: Option<&str>) -> Row {
        let cell = |text: &str| Cell {
            no: 1,
            kind: LineKind::Context,
            text: text.to_string().into(),
            intra: Vec::new(),
            syntax: Vec::new(),
        };
        Row::SplitLine {
            left: left.map(cell),
            right: right.map(cell),
        }
    }

    fn sel(side: SelSide, anchor: (usize, usize), head: (usize, usize)) -> Selection {
        Selection {
            side,
            anchor: RowCol {
                row: anchor.0,
                col: anchor.1,
            },
            head: RowCol {
                row: head.0,
                col: head.1,
            },
        }
    }

    #[test]
    fn selection_ordered_swaps_backward_drags() {
        let forward = sel(SelSide::Unified, (1, 3), (4, 2));
        let backward = sel(SelSide::Unified, (4, 2), (1, 3));
        assert_eq!(forward.ordered(), backward.ordered());
        // Same row, backward drag: ordered by column.
        let same_row = sel(SelSide::Unified, (2, 7), (2, 1));
        let (start, end) = same_row.ordered();
        assert_eq!((start.col, end.col), (1, 7));
    }

    #[test]
    fn char_to_byte_multibyte() {
        let text = "let s = \"héllo\";";
        assert_eq!(char_to_byte(text, 0), 0);
        assert_eq!(char_to_byte(text, 10), 10); // 'é'
        assert_eq!(char_to_byte(text, 11), 12); // first 'l': 'é' is 2 bytes
        assert_eq!(char_to_byte(text, 99), text.len()); // clamped
        assert_eq!(
            &text[char_to_byte(text, 8)..char_to_byte(text, 14)],
            "\"héllo"
        );
    }

    #[test]
    fn row_range_unified_multi_row() {
        let rows = vec![line("first line"), line("middle"), line("last line")];
        let sel = sel(SelSide::Unified, (0, 6), (2, 4));
        // First row: from col 6 to end of text.
        assert_eq!(row_selection_range(&sel, 0, &rows[0]), Some(6..10));
        // Middle row: fully selected, col 0 to end.
        assert_eq!(row_selection_range(&sel, 1, &rows[1]), Some(0..6));
        // Last row: col 0 to col 4.
        assert_eq!(row_selection_range(&sel, 2, &rows[2]), Some(0..4));
        // Outside the selection.
        assert_eq!(row_selection_range(&sel, 3, &rows[0]), None);
    }

    #[test]
    fn row_range_skips_non_text_rows() {
        let header = Row::HunkHeader {
            label: "@@".into(),
            upgraded: false,
        };
        let sel = sel(SelSide::Unified, (0, 0), (2, 3));
        // Headers/spacers inside the span contribute nothing…
        assert_eq!(row_selection_range(&sel, 1, &header), None);
        assert_eq!(row_selection_range(&sel, 1, &Row::Spacer), None);
        // …and split rows never match a Unified-side selection.
        assert_eq!(
            row_selection_range(&sel, 1, &split(Some("x"), Some("y"))),
            None
        );
    }

    #[test]
    fn row_range_split_sides_and_absent_cells() {
        let rows = vec![
            split(Some("left one"), Some("right one")),
            split(None, Some("right only")),
            split(Some("left only"), None),
        ];
        let right = sel(SelSide::Right, (0, 6), (2, 4));
        assert_eq!(row_selection_range(&right, 0, &rows[0]), Some(6..9));
        assert_eq!(row_selection_range(&right, 1, &rows[1]), Some(0..10));
        // Selected side absent: contributes nothing.
        assert_eq!(row_selection_range(&right, 2, &rows[2]), None);
        let left = sel(SelSide::Left, (0, 5), (1, 3));
        assert_eq!(row_selection_range(&left, 0, &rows[0]), Some(5..8));
        assert_eq!(row_selection_range(&left, 1, &rows[1]), None);
    }

    #[test]
    fn row_range_clamps_columns_to_text() {
        let rows = vec![line("ab"), line("cdef")];
        // Anchor col way past the end of a short line.
        let sel = sel(SelSide::Unified, (0, 99), (1, 2));
        assert_eq!(row_selection_range(&sel, 0, &rows[0]), Some(2..2)); // empty
        assert_eq!(row_selection_range(&sel, 1, &rows[1]), Some(0..2));
    }

    #[test]
    fn copy_assembles_contributing_rows() {
        let rows = vec![
            line("fn main() {"),
            Row::HunkHeader {
                label: "@@".into(),
                upgraded: false,
            },
            line(""),
            line("    body();"),
            line("}"),
        ];
        // From col 3 of row 0 through col 1 of row 4: the header is skipped
        // (no blank line for it), the empty line survives as an empty line.
        let forward = sel(SelSide::Unified, (0, 3), (4, 1));
        assert_eq!(
            selection_text(&forward, &rows),
            "main() {\n\n    body();\n}"
        );
        // Backward drag over one row.
        let backward = sel(SelSide::Unified, (0, 7), (0, 3));
        assert_eq!(selection_text(&backward, &rows), "main");
    }

    #[test]
    fn copy_split_takes_locked_side_only() {
        let rows = vec![
            split(Some("old a"), Some("new a")),
            split(None, Some("new b")),
            split(Some("old c"), None),
        ];
        let right = sel(SelSide::Right, (0, 0), (2, 5));
        assert_eq!(selection_text(&right, &rows), "new a\nnew b");
        let left = sel(SelSide::Left, (0, 0), (2, 5));
        assert_eq!(selection_text(&left, &rows), "old a\nold c");
    }

    #[test]
    fn copy_multibyte_slice() {
        let rows = vec![line("let s = \"héllo\";")];
        let quoted = sel(SelSide::Unified, (0, 8), (0, 14));
        assert_eq!(selection_text(&quoted, &rows), "\"héllo");
    }

    // --- Minimap -----------------------------------------------------------

    fn mrow(kind: MinimapKind, len_frac: f32) -> MinimapRow {
        MinimapRow { kind, len_frac }
    }

    #[test]
    fn minimap_rows_unified_kinds_and_fracs() {
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Unified,
            &HashMap::new(),
            None,
            true,
            COMMENT_WRAP_CHARS,
        );
        let mm = minimap_rows(&rows);
        assert_eq!(mm.len(), rows.len());
        // FileHeader and HunkHeader both map to Header ticks.
        assert_eq!(mm[0], mrow(MinimapKind::Header, 1.));
        assert_eq!(mm[1], mrow(MinimapKind::Header, 1.));
        // Line rows: kind from the row, frac = chars / MAX_MINIMAP_CHARS.
        assert_eq!(mm[2], mrow(MinimapKind::Context, 3. / 160.)); // "ctx"
        assert_eq!(mm[3], mrow(MinimapKind::Removed, 4. / 160.)); // "old1"
        assert_eq!(mm[5], mrow(MinimapKind::Added, 4. / 160.)); // "new1"
                                                                // Spacer and Binary rows are blank.
        assert_eq!(mm[14], mrow(MinimapKind::Blank, 0.));
        assert_eq!(mm[16], mrow(MinimapKind::Blank, 0.));
    }

    #[test]
    fn minimap_rows_split_pairs_and_gaps() {
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Split,
            &HashMap::new(),
            None,
            true,
            COMMENT_WRAP_CHARS,
        );
        let mm = minimap_rows(&rows);
        assert_eq!(mm.len(), rows.len());
        // Context pair: both halves, no change flags.
        assert_eq!(
            mm[2].kind,
            MinimapKind::SplitPair {
                left_frac: 3. / 160.,
                right_frac: 3. / 160.,
                left: false,
                right: false,
            }
        );
        // Paired removed/added ("old1" / "new1").
        assert_eq!(
            mm[3].kind,
            MinimapKind::SplitPair {
                left_frac: 4. / 160.,
                right_frac: 4. / 160.,
                left: true,
                right: true,
            }
        );
        assert_eq!(mm[3].len_frac, 4. / 160.);
        // One-sided rows: the absent half has a zero fraction and no flag.
        let h2 = 6; // second HunkHeader (see split tests above)
        assert_eq!(
            mm[h2 + 2].kind,
            MinimapKind::SplitPair {
                left_frac: 2. / 160., // "r2"
                right_frac: 0.,
                left: true,
                right: false,
            }
        );
        assert_eq!(
            mm[h2 + 4].kind,
            MinimapKind::SplitPair {
                left_frac: 0.,
                right_frac: 4. / 160., // "lone"
                left: false,
                right: true,
            }
        );

        // Gap rows map to Gap in both modes.
        let (diff, upgrades) = upgraded_diff();
        for mode in [ViewMode::Unified, ViewMode::Split] {
            let (rows, _, _) = build_rows(&diff, mode, &upgrades, None, true, COMMENT_WRAP_CHARS);
            let mm = minimap_rows(&rows);
            assert_eq!(mm[1], mrow(MinimapKind::Gap, 1.));
        }
    }

    #[test]
    fn minimap_line_frac_caps() {
        assert_eq!(line_frac(""), 0.);
        assert_eq!(line_frac("abcd"), 4. / 160.);
        assert_eq!(line_frac(&"x".repeat(1000)), 1.);
    }

    #[test]
    fn minimap_scale_clamps_and_downsamples() {
        // Tiny diff: 3px max per row, no grouping.
        assert_eq!(minimap_scale(10, 300.), (3., 1));
        // In between: pane / total, no grouping.
        assert_eq!(minimap_scale(200, 300.), (1.5, 1));
        assert_eq!(minimap_scale(300, 300.), (1., 1));
        // Taller than the pane at 1px/row: group N = ceil(total / pane).
        assert_eq!(minimap_scale(1000, 300.), (1., 4));
        assert_eq!(minimap_scale(0, 300.), (1., 1));
    }

    #[test]
    fn minimap_runs_coalesce_same_bars() {
        let rows = vec![
            mrow(MinimapKind::Added, 0.5),
            mrow(MinimapKind::Added, 0.5),
            mrow(MinimapKind::Added, 0.25), // different width: new run
            mrow(MinimapKind::Context, 0.), // empty line: no bar, breaks the run
            mrow(MinimapKind::Added, 0.5),
        ];
        let layout = minimap_runs(&rows, 100.);
        assert_eq!((layout.slot_h, layout.group), (3., 1));
        let expect = |start: u32, end: u32, color: MinimapColor, frac: f32| MinimapRun {
            start,
            end,
            lane: MinimapLane::Full,
            color,
            frac,
            tick: false,
        };
        assert_eq!(
            layout.runs,
            vec![
                expect(0, 2, MinimapColor::Added, 0.5),
                expect(2, 3, MinimapColor::Added, 0.25),
                expect(4, 5, MinimapColor::Added, 0.5),
            ]
        );
    }

    #[test]
    fn minimap_header_ticks_do_not_merge_unless_1px() {
        let rows = vec![
            mrow(MinimapKind::Header, 1.),
            mrow(MinimapKind::Header, 1.),
            mrow(MinimapKind::Context, 0.5),
        ];
        // 3px slots: adjacent headers stay separate 1px ticks.
        let layout = minimap_runs(&rows, 100.);
        assert_eq!(layout.slot_h, 3.);
        assert_eq!(layout.runs.len(), 3);
        assert!(layout.runs[0].tick && layout.runs[1].tick);
        assert_eq!((layout.runs[0].start, layout.runs[0].end), (0, 1));
        assert_eq!((layout.runs[1].start, layout.runs[1].end), (1, 2));
        // 1px slots: ticks are plain bars and coalesce.
        let layout = minimap_runs(&rows, 3.);
        assert_eq!(layout.slot_h, 1.);
        assert_eq!(layout.runs.len(), 2);
        assert!(!layout.runs[0].tick);
        assert_eq!((layout.runs[0].start, layout.runs[0].end), (0, 2));
    }

    #[test]
    fn minimap_downsample_priority_and_max_frac() {
        // 100 rows into a 10px pane: 10 rows per 1px slot.
        let mut rows = vec![mrow(MinimapKind::Blank, 0.); 100];
        rows[0] = mrow(MinimapKind::Removed, 0.1);
        rows[1] = mrow(MinimapKind::Added, 1.);
        rows[2] = mrow(MinimapKind::Context, 0.2);
        for slot in rows[10..20].iter_mut() {
            *slot = mrow(MinimapKind::Context, 0.2);
        }
        let layout = minimap_runs(&rows, 10.);
        assert_eq!((layout.slot_h, layout.group), (1., 10));
        // Slot 0: Removed outranks Added/Context; width is the group max.
        // Slot 1: all-context. Slots 2..: blank, nothing painted.
        assert_eq!(
            layout.runs,
            vec![
                MinimapRun {
                    start: 0,
                    end: 1,
                    lane: MinimapLane::Full,
                    color: MinimapColor::Removed,
                    frac: 1.,
                    tick: false,
                },
                MinimapRun {
                    start: 1,
                    end: 2,
                    lane: MinimapLane::Full,
                    color: MinimapColor::Context,
                    frac: 0.2,
                    tick: false,
                },
            ]
        );
    }

    #[test]
    fn minimap_split_rows_use_half_lanes() {
        let pair = |left_frac: f32, right_frac: f32, left: bool, right: bool| {
            mrow(
                MinimapKind::SplitPair {
                    left_frac,
                    right_frac,
                    left,
                    right,
                },
                left_frac.max(right_frac),
            )
        };
        let rows = vec![
            pair(0.5, 0.25, true, true),
            pair(0.5, 0.25, true, true),
            pair(0.5, 0., false, false), // context left, absent right
        ];
        let layout = minimap_runs(&rows, 90.);
        assert_eq!(layout.slot_h, 3.);
        assert_eq!(
            layout.runs,
            vec![
                MinimapRun {
                    start: 0,
                    end: 2,
                    lane: MinimapLane::Left,
                    color: MinimapColor::Removed,
                    frac: 0.5,
                    tick: false,
                },
                MinimapRun {
                    start: 0,
                    end: 2,
                    lane: MinimapLane::Right,
                    color: MinimapColor::Added,
                    frac: 0.25,
                    tick: false,
                },
                MinimapRun {
                    start: 2,
                    end: 3,
                    lane: MinimapLane::Left,
                    color: MinimapColor::Context,
                    frac: 0.5,
                    tick: false,
                },
            ]
        );
    }

    // --- Review comments ----------------------------------------------------

    fn rc(
        id: u64,
        path: &str,
        side: Option<&str>,
        line: Option<u64>,
        body: &str,
        author: &str,
        created_at: &str,
        reply_to: Option<u64>,
    ) -> gh::ReviewComment {
        gh::ReviewComment {
            id,
            path: path.to_string(),
            line,
            side: side.map(str::to_string),
            start_line: None,
            body: body.to_string(),
            user: gh::Author {
                login: author.to_string(),
            },
            created_at: created_at.to_string(),
            in_reply_to_id: reply_to,
        }
    }

    #[test]
    fn wrap_preserves_newlines_and_breaks_at_spaces() {
        // Explicit newlines (and CRLF) survive; empty lines stay empty.
        assert_eq!(wrap_body("a\n\nb\r\nc", 10), vec!["a", "", "b", "c"]);
        // Fits exactly: no wrap.
        assert_eq!(wrap_body("abcde", 5), vec!["abcde"]);
        // Breaks at the last space in range; the space is consumed.
        assert_eq!(
            wrap_body("one two three four", 9),
            vec!["one two", "three", "four"]
        );
        // A word longer than the width hard-breaks; no space is invented.
        assert_eq!(wrap_body("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        // Never splits inside a word when a space exists in range.
        assert_eq!(wrap_body("aa bbbb", 5), vec!["aa", "bbbb"]);
        // Multibyte chars: wraps on char boundaries, not bytes.
        assert_eq!(wrap_body("ééééé", 3), vec!["ééé", "éé"]);
        assert_eq!(wrap_body("éé éé", 3), vec!["éé", "éé"]);
    }

    #[test]
    fn short_age_buckets() {
        let now = parse_iso_utc("2026-07-09T12:00:00Z").unwrap();
        assert_eq!(short_age("2026-07-09T11:59:30Z", now), "just now");
        assert_eq!(short_age("2026-07-09T11:15:00Z", now), "45m ago");
        assert_eq!(short_age("2026-07-09T05:00:00Z", now), "7h ago");
        assert_eq!(short_age("2026-07-06T12:00:00Z", now), "3d ago");
        assert_eq!(short_age("2024-01-01T00:00:00Z", now), "2y ago");
        // Clock skew (future timestamp) degrades to "just now".
        assert_eq!(short_age("2026-07-09T12:05:00Z", now), "just now");
        // Unparseable input renders as-is.
        assert_eq!(short_age("garbage", now), "garbage");
    }

    #[test]
    fn parse_iso_utc_matches_known_epoch() {
        assert_eq!(parse_iso_utc("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso_utc("2001-09-09T01:46:40Z"), Some(1_000_000_000));
        assert_eq!(parse_iso_utc("2026-07-09"), None);
    }

    #[test]
    fn group_comments_threads_replies_orphans_and_outdated() {
        let index = group_comments(vec![
            // Deliberately out of order: grouping sorts by created_at.
            rc(
                2,
                "a.rs",
                Some("RIGHT"),
                Some(2),
                "reply",
                "bob",
                "2026-01-02T00:00:00Z",
                Some(1),
            ),
            rc(
                1,
                "a.rs",
                Some("RIGHT"),
                Some(2),
                "root",
                "alice",
                "2026-01-01T00:00:00Z",
                None,
            ),
            // Second thread at the same anchor, created later.
            rc(
                3,
                "a.rs",
                Some("RIGHT"),
                Some(2),
                "later",
                "carol",
                "2026-01-03T00:00:00Z",
                None,
            ),
            // LEFT-side thread on the same line number: distinct anchor.
            rc(
                4,
                "a.rs",
                Some("LEFT"),
                Some(2),
                "old side",
                "dave",
                "2026-01-04T00:00:00Z",
                None,
            ),
            // Outdated: no current line. Counts, never anchors.
            rc(
                5,
                "a.rs",
                None,
                None,
                "stale",
                "erin",
                "2026-01-05T00:00:00Z",
                None,
            ),
            // Orphaned reply: parent never fetched — dropped entirely.
            rc(
                6,
                "a.rs",
                Some("RIGHT"),
                Some(2),
                "orphan",
                "mallory",
                "2026-01-06T00:00:00Z",
                Some(999),
            ),
            // Reply-to-a-reply lands in the root's thread.
            rc(
                7,
                "a.rs",
                Some("RIGHT"),
                Some(2),
                "nested",
                "alice",
                "2026-01-07T00:00:00Z",
                Some(2),
            ),
            rc(
                8,
                "b.rs",
                Some("RIGHT"),
                Some(9),
                "other file",
                "bob",
                "2026-01-08T00:00:00Z",
                None,
            ),
        ]);
        let a = &index.threads["a.rs"];
        let right = &a[&(CommentSide::Right, 2)];
        assert_eq!(right.len(), 2);
        assert_eq!(right[0].root.id, 1);
        assert_eq!(
            right[0].replies.iter().map(|c| c.id).collect::<Vec<_>>(),
            vec![2, 7]
        );
        assert_eq!(right[1].root.id, 3);
        assert!(right[1].replies.is_empty());
        assert_eq!(a[&(CommentSide::Left, 2)][0].root.id, 4);
        // Counts: anchored = 3 (root+reply+nested) + 1 + 1 = 5 (the orphan
        // is dropped), outdated = 1.
        assert_eq!(index.counts["a.rs"], (5, 1));
        assert_eq!(index.counts["b.rs"], (1, 0));
        assert_eq!(index.threads["b.rs"][&(CommentSide::Right, 9)].len(), 1);
    }

    /// Comments for sample_diff's a.rs: a RIGHT thread (with one reply) on
    /// added line 2 ("new1"), a LEFT thread on removed line 2 ("old1"), and
    /// one outdated comment.
    fn sample_comments() -> CommentIndex {
        group_comments(vec![
            rc(
                1,
                "a.rs",
                Some("RIGHT"),
                Some(2),
                "on new1",
                "alice",
                "2026-01-01T00:00:00Z",
                None,
            ),
            rc(
                2,
                "a.rs",
                Some("RIGHT"),
                Some(2),
                "reply",
                "bob",
                "2026-01-02T00:00:00Z",
                Some(1),
            ),
            rc(
                3,
                "a.rs",
                Some("LEFT"),
                Some(2),
                "on old1",
                "carol",
                "2026-01-03T00:00:00Z",
                None,
            ),
            rc(
                4,
                "a.rs",
                None,
                None,
                "outdated",
                "dave",
                "2026-01-04T00:00:00Z",
                None,
            ),
        ])
    }

    #[test]
    fn unified_rows_insert_threads_beneath_their_anchor() {
        let index = sample_comments();
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Unified,
            &HashMap::new(),
            Some(&index),
            true,
            COMMENT_WRAP_CHARS,
        );
        // rows: FileHeader, HunkHeader, ctx, rem old1 (old_no 2) + LEFT
        // thread, rem old2, add new1 (new_no 2) + RIGHT thread, …
        let names: Vec<&str> = rows.iter().map(row_name).collect();
        assert_eq!(
            &names[..12],
            &[
                "FileHeader",
                "HunkHeader",
                "Line",          // ctx 1/1
                "Line",          // rem old1 (old 2)
                "CommentHeader", // carol on old1
                "CommentBody",
                "CommentActions",
                "Line",          // rem old2
                "Line",          // add new1 (new 2)
                "CommentHeader", // alice
                "CommentBody",
                "CommentHeader", // bob's reply
            ]
        );
        assert_eq!(names[12], "CommentBody");
        assert_eq!(names[13], "CommentActions");
        // The LEFT thread is carol's; the reply flag follows position.
        match &rows[4] {
            Row::CommentHeader {
                author, is_reply, ..
            } => {
                assert_eq!(author.as_ref(), "carol");
                assert!(!is_reply);
            }
            other => panic!("expected comment header, got {}", row_name(other)),
        }
        match &rows[11] {
            Row::CommentHeader {
                author, is_reply, ..
            } => {
                assert_eq!(author.as_ref(), "bob");
                assert!(is_reply);
            }
            other => panic!("expected reply header, got {}", row_name(other)),
        }
        // Actions row targets the thread root and carries the anchor.
        match &rows[13] {
            Row::CommentActions {
                root_id,
                path,
                side,
                line,
                ..
            } => {
                assert_eq!(*root_id, 1);
                assert_eq!(path.as_ref(), "a.rs");
                assert_eq!((*side, *line), (CommentSide::Right, 2));
            }
            other => panic!("expected actions row, got {}", row_name(other)),
        }
        // File header carries the counts (3 anchored, 1 outdated).
        match &rows[0] {
            Row::FileHeader {
                comments, outdated, ..
            } => {
                assert_eq!((*comments, *outdated), (3, 1));
            }
            other => panic!("expected file header, got {}", row_name(other)),
        }
        // Comment rows are selectable-through and blank in the minimap.
        assert!(row_side_text(&rows[4], SelSide::Unified).is_none());
        assert_eq!(minimap_rows(&rows)[4], mrow(MinimapKind::Blank, 0.));
    }

    #[test]
    fn split_rows_anchor_threads_by_cell() {
        let index = sample_comments();
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Split,
            &HashMap::new(),
            Some(&index),
            true,
            COMMENT_WRAP_CHARS,
        );
        // rows[3] pairs old1/new1 (both line 2): LEFT thread then RIGHT
        // thread directly beneath it.
        let names: Vec<&str> = rows.iter().map(row_name).collect();
        assert_eq!(
            &names[2..11],
            &[
                "SplitLine",     // ctx
                "SplitLine",     // old1 | new1
                "CommentHeader", // carol (LEFT)
                "CommentBody",
                "CommentActions",
                "CommentHeader", // alice (RIGHT)
                "CommentBody",
                "CommentHeader", // bob reply
                "CommentBody",
            ]
        );
        assert_eq!(names[11], "CommentActions");
        assert_eq!(names[12], "SplitLine"); // old2 | new2
    }

    /// The half a comment row renders in, or None for a full-width card.
    fn row_half(row: &Row) -> Option<Option<CommentSide>> {
        match row {
            Row::CommentHeader { half, .. }
            | Row::CommentBody { half, .. }
            | Row::CommentActions { half, .. } => Some(*half),
            _ => None,
        }
    }

    #[test]
    fn split_comments_render_in_the_half_they_were_left_on() {
        let index = sample_comments();
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Split,
            &HashMap::new(),
            Some(&index),
            true,
            COMMENT_WRAP_CHARS,
        );
        // rows[4..7] are carol's LEFT thread, rows[7..12] the RIGHT one; each
        // row of a thread carries the side it was anchored to.
        let halves: Vec<Option<Option<CommentSide>>> = rows[4..12].iter().map(row_half).collect();
        assert_eq!(
            halves,
            vec![
                Some(Some(CommentSide::Left)),  // carol header
                Some(Some(CommentSide::Left)),  // body
                Some(Some(CommentSide::Left)),  // actions
                Some(Some(CommentSide::Right)), // alice header
                Some(Some(CommentSide::Right)), // body
                Some(Some(CommentSide::Right)), // bob reply header
                Some(Some(CommentSide::Right)), // body
                Some(Some(CommentSide::Right)), // actions
            ]
        );
    }

    #[test]
    fn only_a_threads_first_row_draws_the_card_top() {
        let index = sample_comments();
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Unified,
            &HashMap::new(),
            Some(&index),
            true,
            COMMENT_WRAP_CHARS,
        );
        // Exactly one top edge per thread, and it's the root's header — a
        // reply's header sits mid-card and must not cap it.
        let mut tops = 0;
        let mut threads = 0;
        for row in &rows {
            match row {
                Row::CommentHeader { top, is_reply, .. } => {
                    if *top {
                        tops += 1;
                        assert!(!is_reply, "a reply header must not open the card");
                    }
                }
                // The actions row always closes a thread, so it counts them.
                Row::CommentActions { .. } => threads += 1,
                _ => {}
            }
        }
        assert!(threads > 0, "fixture should have threads");
        assert_eq!(tops, threads, "one top edge per thread");
    }

    #[test]
    fn git_env_routes_credentials_through_gh_and_never_prompts() {
        let env: HashMap<&str, &str> = git_env().into_iter().collect();
        // The prompt is the whole bug: git asks on /dev/tty, so piping a
        // child's stdio doesn't suppress it.
        assert_eq!(env.get("GIT_TERMINAL_PROMPT"), Some(&"0"));
        // GIT_CONFIG_COUNT must match the number of KEY/VALUE pairs or git
        // ignores the trailing ones (or errors).
        let count: usize = env.get("GIT_CONFIG_COUNT").unwrap().parse().unwrap();
        assert_eq!(count, 2);
        for ix in 0..count {
            assert!(
                env.contains_key(format!("GIT_CONFIG_KEY_{ix}").as_str())
                    && env.contains_key(format!("GIT_CONFIG_VALUE_{ix}").as_str()),
                "pair {ix} is incomplete"
            );
        }
        // Pair 0 clears any inherited helper (macOS ships osxkeychain via
        // Xcode's system gitconfig); pair 1 then installs gh's. Order matters:
        // reversed, the inherited helper would win.
        assert_eq!(env.get("GIT_CONFIG_KEY_0"), Some(&"credential.helper"));
        assert_eq!(env.get("GIT_CONFIG_VALUE_0"), Some(&""));
        assert_eq!(env.get("GIT_CONFIG_KEY_1"), Some(&"credential.helper"));
        assert_eq!(
            env.get("GIT_CONFIG_VALUE_1"),
            Some(&"!gh auth git-credential")
        );
    }

    #[test]
    fn zoom_keybindings_parse() {
        // These strings are only parsed when the app boots, so a typo would be
        // a runtime panic rather than a compile error.
        for keys in ["cmd-=", "cmd-+", "cmd--", "cmd-0", "cmd-r", "j", "k"] {
            assert!(
                Keystroke::parse(keys).is_ok(),
                "{keys:?} should be a valid keystroke"
            );
        }
    }

    #[test]
    fn row_height_follows_the_font_size() {
        // The default must reproduce the pre-zoom geometry exactly.
        assert_eq!(DEFAULT_TEXT_SIZE, 13.0);
        assert_eq!(row_height_for(DEFAULT_TEXT_SIZE), 22.0);
        // Rows grow and shrink with the text, always leaving headroom so
        // glyphs can't outgrow their row at either bound.
        assert_eq!(row_height_for(20.), 34.0);
        assert_eq!(row_height_for(8.), 14.0);
        for size in [MIN_TEXT_SIZE, DEFAULT_TEXT_SIZE, MAX_TEXT_SIZE] {
            assert!(
                row_height_for(size) > size,
                "row must be taller than {size}px text"
            );
        }
    }

    #[test]
    fn comment_wrap_cols_follows_the_pane_width() {
        let cw = 8.0; // 8px per column keeps the arithmetic obvious.
        let chrome = COMMENT_CARD_CHROME;
        // Unified uses the whole width; split only its half, so the same pane
        // yields roughly half the columns.
        assert_eq!(
            comment_wrap_cols(chrome + 40. * cw, ViewMode::Unified, cw),
            40
        );
        let split_pane = 2. * (chrome + 40. * cw) + SPLIT_DIVIDER;
        assert_eq!(comment_wrap_cols(split_pane, ViewMode::Split, cw), 40);
        // A roomy pane stops widening at the readability cap...
        assert_eq!(
            comment_wrap_cols(10_000., ViewMode::Unified, cw),
            COMMENT_WRAP_CHARS
        );
        // ...and a cramped one bottoms out rather than wrapping every word.
        assert_eq!(
            comment_wrap_cols(0., ViewMode::Split, cw),
            MIN_COMMENT_WRAP_CHARS
        );
        // Degenerate char width can't divide by zero.
        assert_eq!(
            comment_wrap_cols(800., ViewMode::Unified, 0.),
            COMMENT_WRAP_CHARS
        );
    }

    #[test]
    fn comment_bodies_wrap_to_the_requested_column_width() {
        // One long prose line plus an unbreakable token (a URL), the two ways a
        // body runs past the card.
        let body = format!(
            "{} https://example.com/{}",
            "word ".repeat(60),
            "x".repeat(200)
        );
        let index = group_comments(vec![rc(
            1,
            "a.rs",
            Some("RIGHT"),
            Some(2),
            &body,
            "alice",
            "2026-01-01T00:00:00Z",
            None,
        )]);
        for (name, mode, cap) in [
            ("unified", ViewMode::Unified, COMMENT_WRAP_CHARS),
            ("split", ViewMode::Split, 40),
        ] {
            let (rows, _, _) = build_rows(
                &sample_diff(),
                mode,
                &HashMap::new(),
                Some(&index),
                true,
                cap,
            );
            let bodies: Vec<&SharedString> = rows
                .iter()
                .filter_map(|row| match row {
                    Row::CommentBody { line, .. } => Some(line),
                    _ => None,
                })
                .collect();
            assert!(!bodies.is_empty(), "{name} should have body rows");
            for line in bodies {
                assert!(
                    line.chars().count() <= cap,
                    "{name}: {:?} exceeds {cap} cols",
                    line.as_ref()
                );
            }
        }
    }

    #[test]
    fn unified_comments_span_the_full_width() {
        let index = sample_comments();
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Unified,
            &HashMap::new(),
            Some(&index),
            true,
            COMMENT_WRAP_CHARS,
        );
        // Unified has one column, so no comment row is confined to a half.
        assert!(rows.iter().filter_map(row_half).all(|half| half.is_none()));
        // ...and the diff really does contain comment rows to check.
        assert!(rows.iter().any(is_comment_row));
    }

    #[test]
    fn hidden_comments_keep_header_counts() {
        let index = sample_comments();
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Unified,
            &HashMap::new(),
            Some(&index),
            false,
            COMMENT_WRAP_CHARS,
        );
        assert!(!rows.iter().any(is_comment_row));
        match &rows[0] {
            Row::FileHeader {
                comments, outdated, ..
            } => {
                assert_eq!((*comments, *outdated), (3, 1));
            }
            other => panic!("expected file header, got {}", row_name(other)),
        }
        // And with no index at all (local items): zero counts, no rows.
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Unified,
            &HashMap::new(),
            None,
            true,
            COMMENT_WRAP_CHARS,
        );
        assert!(!rows.iter().any(is_comment_row));
        match &rows[0] {
            Row::FileHeader {
                comments, outdated, ..
            } => {
                assert_eq!((*comments, *outdated), (0, 0));
            }
            other => panic!("expected file header, got {}", row_name(other)),
        }
    }

    #[test]
    fn expanded_gap_context_rows_host_threads() {
        let (diff, mut upgrades) = upgraded_diff();
        // a.txt line 3 lives in the leading gap (hunk covers 7..=13).
        let index = group_comments(vec![rc(
            9,
            "a.txt",
            Some("RIGHT"),
            Some(3),
            "gap comment",
            "alice",
            "2026-01-01T00:00:00Z",
            None,
        )]);
        let (rows, _, _) = build_rows(
            &diff,
            ViewMode::Unified,
            &upgrades,
            Some(&index),
            true,
            COMMENT_WRAP_CHARS,
        );
        // Collapsed gap: the thread has no anchor row and stays hidden.
        assert!(!rows.iter().any(is_comment_row));
        upgrades.get_mut(&0).unwrap().expanded.insert(0);
        let (rows, _, _) = build_rows(
            &diff,
            ViewMode::Unified,
            &upgrades,
            Some(&index),
            true,
            COMMENT_WRAP_CHARS,
        );
        // FileHeader, ctx 1, ctx 2, ctx 3, then the thread.
        assert_eq!(row_name(&rows[3]), "Line");
        assert_eq!(row_name(&rows[4]), "CommentHeader");
        assert_eq!(row_name(&rows[5]), "CommentBody");
        assert_eq!(row_name(&rows[6]), "CommentActions");
    }

    #[test]
    fn comment_anchor_resolves_sides_and_skips_non_lines() {
        let index = sample_comments();
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Unified,
            &HashMap::new(),
            Some(&index),
            true,
            COMMENT_WRAP_CHARS,
        );
        // Headers and comment rows anchor nothing.
        assert_eq!(comment_anchor(&rows, 0, SelSide::Unified), None);
        assert_eq!(comment_anchor(&rows, 4, SelSide::Unified), None);
        // Context row (1,1) → RIGHT 1; removed old1 → LEFT 2; added new1 → RIGHT 2.
        assert_eq!(
            comment_anchor(&rows, 2, SelSide::Unified),
            Some((CommentSide::Right, 1))
        );
        assert_eq!(
            comment_anchor(&rows, 3, SelSide::Unified),
            Some((CommentSide::Left, 2))
        );
        assert_eq!(
            comment_anchor(&rows, 8, SelSide::Unified),
            Some((CommentSide::Right, 2))
        );
        // Split: the half under the pointer decides; absent cells refuse.
        let (rows, _, hunk_rows) = build_rows(
            &sample_diff(),
            ViewMode::Split,
            &HashMap::new(),
            None,
            true,
            COMMENT_WRAP_CHARS,
        );
        assert_eq!(
            comment_anchor(&rows, 3, SelSide::Left),
            Some((CommentSide::Left, 2))
        );
        assert_eq!(
            comment_anchor(&rows, 3, SelSide::Right),
            Some((CommentSide::Right, 2))
        );
        // Second hunk: r2 has no right cell (see split tests above).
        let h2 = hunk_rows[1];
        assert_eq!(comment_anchor(&rows, h2 + 2, SelSide::Right), None);
        assert_eq!(
            comment_anchor(&rows, h2 + 2, SelSide::Left),
            Some((CommentSide::Left, 11))
        );
    }

    #[test]
    fn nth_noncomment_row_maps_positions_across_comment_insertions() {
        let index = sample_comments();
        let (plain, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Unified,
            &HashMap::new(),
            None,
            true,
            COMMENT_WRAP_CHARS,
        );
        let (with, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Unified,
            &HashMap::new(),
            Some(&index),
            true,
            COMMENT_WRAP_CHARS,
        );
        // Every plain row maps to the same row content with comments shown.
        for (n, row) in plain.iter().enumerate() {
            let ix = nth_noncomment_row(&with, n);
            assert_eq!(row_name(&with[ix]), row_name(row), "row {n}");
        }
        // n beyond the end clamps to the last row.
        assert_eq!(nth_noncomment_row(&plain, 999), plain.len() - 1);
    }

    #[test]
    fn local_review_comments_index_like_review_threads() {
        let mut review = LocalReview::default();
        review.add_comment(
            None,
            "src/lib.rs".to_string(),
            CommentSide::Right,
            12,
            "please simplify this".to_string(),
        );
        review.add_comment(
            Some(1),
            "src/lib.rs".to_string(),
            CommentSide::Right,
            12,
            "also add a test".to_string(),
        );

        let index = review.index();
        assert_eq!(index.counts["src/lib.rs"], (2, 0));
        let threads = &index.threads["src/lib.rs"][&(CommentSide::Right, 12)];
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].root.body, "please simplify this");
        assert_eq!(threads[0].replies[0].body, "also add a test");
    }

    #[test]
    fn local_review_prompt_matches_difit_comment_format() {
        let src = git::LocalSource {
            repo_root: "/tmp/myrepo".into(),
            branch: "feature".into(),
            base_ref: None,
            base_label: "upstream/main".into(),
            base_oid: None,
        };
        let mut review = LocalReview::default();
        review.add_comment(
            None,
            "src/lib.rs".to_string(),
            CommentSide::Right,
            7,
            "why remove this?\nplease explain".to_string(),
        );
        review.add_comment(
            Some(1),
            "src/lib.rs".to_string(),
            CommentSide::Right,
            7,
            "because this path handles nil".to_string(),
        );
        review.add_comment(
            None,
            "src/main.rs".to_string(),
            CommentSide::Left,
            12,
            "second thread".to_string(),
        );

        assert_eq!(
            local_review_prompt(&src, &review),
            "Diff: upstream/main ← feature. Locations are GitHub diff-style: path:Rline for right/new, path:Lline for left/old.\n\nsrc/lib.rs:R7\nwhy remove this?\nplease explain\nReply 1 (you)\nbecause this path handles nil\n=====\nsrc/main.rs:L12\nsecond thread"
        );
    }

    #[test]
    fn merge_selection_wins_over_intra() {
        let bg = Some(theme::added_word_bg());
        let sel_style = |token: Option<Token>| {
            let mut style = token.map(style).unwrap_or_default();
            style.background_color = Some(theme::selection_bg().into());
            style
        };
        // Intra 2..6, selection 4..8: the overlap 4..6 paints selection bg.
        assert_eq!(
            merge_highlights(&[], &[2..6], bg, Some(4..8)),
            vec![(2..4, style_bg(None)), (4..8, sel_style(None))]
        );
        // Selection over a syntax token keeps the token foreground.
        let syntax = [(0..4, Token::Keyword)];
        assert_eq!(
            merge_highlights(&syntax, &[], None, Some(2..6)),
            vec![
                (0..2, style(Token::Keyword)),
                (2..4, sel_style(Some(Token::Keyword))),
                (4..6, sel_style(None)),
            ]
        );
    }
}
