mod chrome;
mod conversation;
mod http;
mod local_store;
mod minimap;
mod queue;
mod review_target;
mod rows;
mod selection;
mod theme;
mod threads;
mod tree;
mod upgrade;

#[cfg(test)]
mod test_util;

use chrome::{
    app_title, centered_message, filtered_sources, palette_pr_row, BaseListState, PaletteStep,
    PrListState, PALETTE_ROW_HEIGHT, PALETTE_SOURCES, SOURCE_FOLDER, SOURCE_PR,
};
use diff_core::{FileStatus, PrDiff};
use gpui::{
    actions, canvas, div, fill, font, point, prelude::*, px, relative, rems, size, uniform_list,
    App, Application, AssetSource, Bounds, ClipboardItem, Context, FocusHandle, Hsla, KeyBinding,
    Keystroke, ListHorizontalSizingBehavior, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, PathPromptOptions, Pixels, Point, ScrollHandle, ScrollStrategy, SharedString,
    Subscription, Task, TitlebarOptions, UniformListScrollHandle, Window, WindowBounds,
    WindowOptions,
};
use gpui_component::{
    avatar::Avatar,
    button::{Button, ButtonVariants as _},
    input::{CompletionProvider, Escape as InputEscape, Input, InputEvent, InputState},
    kbd::Kbd,
    scroll::Scrollbar,
    tag::Tag,
    text::{TextView, TextViewStyle},
    ActiveTheme, Disableable as _, IconName, Root, Rope, RopeExt as _, Sizable as _, TitleBar,
};
use minimap::{
    minimap_rows, minimap_runs, minimap_scale, MinimapColor, MinimapLane, MinimapLayout,
    MinimapRow, MINIMAP_GAP, MINIMAP_PAD, MINIMAP_WIDTH,
};
use queue::{filter_base_refs, filter_prs, label_chip_colors, queue_chips, queue_row};
use review_target::{dir_name, fetch_item, sources_match, Loaded, LoadedMeta, Source};
use rows::{
    build_rows, is_comment_row, now_unix, nth_noncomment_row, render_row, widest_row_ix,
    FileUpgrade, LineKind, Row, ViewMode,
};
use selection::{row_selection_range, selection_text, RowCol, SelSide, Selection};
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use threads::{
    comment_wrap_cols, group_comments, CommentIndex, CommentSide, CommentThread, COMMENT_WRAP_CHARS,
};
use tree::{
    build_tree, fuzzy_file_matches, render_tree_row, visible_entries, TreeEntry, TreeEntryKind,
    TreeListRow,
};
use upgrade::{run_upgrade, UpgradeJob, UpgradeSource};

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
        ScrollLeft,
        ScrollRight,
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
        NewComment,
        SubmitReview,
        ZoomIn,
        ZoomOut,
        ZoomReset,
        ShowConversation,
        ShowChanges
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
    // One backend for the whole process: CLI arguments resolve through the
    // same facade the app runs on, so launch errors speak the app's one
    // GitHub vocabulary.
    let github = github_client();
    // Bare launch lands on the review queue — the home screen. The repo we
    // stand in stays one explicit act away (`.` as an argument, or the
    // palette's "open local folder"), so local review keeps working
    // without hijacking the launch.
    for arg in &args {
        let parsed = if Path::new(arg).is_dir() {
            git::resolve_local(Path::new(arg))
                .map(Source::Local)
                .map_err(|err| format!("{err:#}"))
        } else {
            github
                .resolve_pr(arg)
                .map(Source::Pr)
                .map_err(|err| err.to_string())
        };
        match parsed {
            Ok(source) => sources.push(source),
            Err(message) => {
                eprintln!("error: {arg}: {message}");
                errors.push(format!("{arg}: {message}"));
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
                // Sideways through long lines in the unified diff. Inputs
                // deeper in the tree (a focused palette/filter input) keep
                // their own left/right for caret movement.
                KeyBinding::new("left", ScrollLeft, Some("ReviewApp")),
                KeyBinding::new("right", ScrollRight, Some("ReviewApp")),
                KeyBinding::new("v", ToggleView, Some("ReviewApp")),
                KeyBinding::new("m", ToggleMinimap, Some("ReviewApp")),
                KeyBinding::new("c", ToggleComments, Some("ReviewApp")),
                // `C` (shift-c) opens the inline composer on the selected
                // line — the keyboard twin of the hover `+`.
                KeyBinding::new("shift-c", NewComment, Some("ReviewApp")),
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
                // ⇧⌘←/→ flips the open item between the conversation and
                // the changes, conversation on the left of that axis (the
                // GitHub tab strip's order). PRs open onto changes; this is
                // the one-keystroke hop. Inputs deeper in the tree bind
                // shift-cmd-left/right themselves for text selection, so
                // typing in the filter or the composer is unaffected.
                KeyBinding::new("shift-cmd-left", ShowConversation, Some("ReviewApp")),
                KeyBinding::new("shift-cmd-right", ShowChanges, Some("ReviewApp")),
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
                        ReviewApp::new(sources, errors, github, store, store_error, window, cx)
                    });
                    window.focus(&view.read(cx).focus_handle);
                    cx.new(|cx| Root::new(view, window, cx))
                },
            )
            .unwrap();
            cx.activate(true);
        });
}

// --- Review comments -------------------------------------------------------

#[derive(Debug, Default)]
struct LocalReview {
    comments: Vec<github::ReviewComment>,
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
        self.comments.push(github::ReviewComment {
            id: self.next_id,
            path,
            line: Some(line),
            side: Some(side.api_str().to_string()),
            start_line: None,
            body,
            user: github::Author {
                login: "you".to_string(),
            },
            created_at: "draft".to_string(),
            in_reply_to_id: reply_to,
        });
    }
}

/// One hunk-navigation step's target: a row to jump to within the current
/// rows, or — when it wraps past the end of the displayed file's hunks — a
/// file to switch the pane to, landing on one of its hunks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HunkNav {
    Row(usize),
    File { file_ix: usize, hunk_ix: usize },
}

enum ItemState {
    Loading,
    Ready(Box<ItemData>),
    Failed(String),
}

/// Everything the diff pane needs for one loaded item. Each item owns its
/// scroll handle so per-item scroll position survives switching.
pub(crate) struct ItemData {
    pr_meta: Option<github::PrMeta>,
    pub(crate) diff: PrDiff,
    mode: ViewMode,
    /// The one file the pane shows (file mode is the only presentation).
    file_ix: usize,
    rows: Vec<Row>,
    file_rows: Vec<usize>,
    hunk_rows: Vec<usize>,
    /// The row `uniform_list` measures for its horizontal scroll range
    /// (gpui measures a single item; row 0 is always the file header,
    /// which would leave that range at zero). Recomputed in `set_rows`.
    wide_row: usize,
    /// Minimap model, index-aligned with `rows`; rebuilt with them.
    minimap: Vec<MinimapRow>,
    /// Coalesced minimap quad runs for one pane height, computed lazily on
    /// paint and reused until the height changes or the rows are rebuilt.
    minimap_cache: RefCell<Option<(f32, Rc<MinimapLayout>)>>,
    cursor: usize,
    scroll: UniformListScrollHandle,
    /// The conversation pane's scroll position. A pane-level container (not
    /// the TextView's own), so the wheel scrolls from anywhere over the pane
    /// and the scrollbar paints at the pane's edge — like the diff pane's.
    conv_scroll: ScrollHandle,
    additions: u32,
    deletions: u32,
    /// Mouse text selection, in display-row space. Per item (survives item
    /// switching); cleared on view-mode toggle and refresh, where row indices
    /// change meaning.
    selection: Option<Selection>,
    /// Phase-2 upgrades by file index into `diff.files`: whole-file span
    /// tables and full new-side lines. The re-diffed hunks themselves
    /// replace `diff.files[ix].hunks`. Reset on refresh.
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
    mentions: Rc<RefCell<Vec<github::Mention>>>,
    /// The one-time background mentionable-user fetch has been kicked off.
    mentions_fetched: bool,
    /// Sidebar file tree, rebuilt whenever the diff itself changes (load,
    /// refresh, blob upgrade) — but not on view-mode toggles: entries map to
    /// file indices, not row indices, so they survive row rebuilds.
    pub(crate) tree: Vec<TreeEntry>,
    /// Collapsed directory paths, preserved across rebuilds where the
    /// directory still exists.
    pub(crate) collapsed: HashSet<String>,
    tree_scroll: UniformListScrollHandle,
    /// The file last auto-centered in the tree, so follow-the-diff only
    /// scrolls the tree when the viewport's file changes — never while the
    /// user scrolls the tree themselves.
    tree_last_file: Option<usize>,
}

impl ItemData {
    /// A fresh item's data, ready to show: `file_ix` on screen, revealed
    /// at its first change.
    fn new(
        pr_meta: Option<github::PrMeta>,
        diff: PrDiff,
        mode: ViewMode,
        file_ix: usize,
        rows: (Vec<Row>, Vec<usize>, Vec<usize>),
        comments: Option<CommentIndex>,
        local_review: Option<LocalReview>,
    ) -> Self {
        let mut data = Self {
            pr_meta,
            diff,
            mode,
            file_ix,
            rows: Vec::new(),
            file_rows: Vec::new(),
            hunk_rows: Vec::new(),
            wide_row: 0,
            minimap: Vec::new(),
            minimap_cache: RefCell::new(None),
            cursor: 0,
            scroll: UniformListScrollHandle::new(),
            conv_scroll: ScrollHandle::new(),
            additions: 0,
            deletions: 0,
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
        };
        data.additions = data.diff.files.iter().fold(0, |a, f| a + f.additions);
        data.deletions = data.diff.files.iter().fold(0, |d, f| d + f.deletions);
        data.set_rows(rows);
        data.rebuild_tree();
        data.reveal_hunk(None);
        data
    }

    /// Install freshly built display rows, keeping the minimap model in sync
    /// (every row rebuild goes through here).
    fn set_rows(&mut self, (rows, file_rows, hunk_rows): (Vec<Row>, Vec<usize>, Vec<usize>)) {
        self.minimap = minimap_rows(&rows);
        self.minimap_cache.replace(None);
        self.wide_row = widest_row_ix(&rows);
        self.rows = rows;
        self.file_rows = file_rows;
        self.hunk_rows = hunk_rows;
    }

    /// Rebuild the display rows for the displayed file and mode.
    fn row_build(&self) -> (Vec<Row>, Vec<usize>, Vec<usize>) {
        build_rows(
            &self.diff,
            self.mode,
            &self.upgrades,
            self.comments.as_ref(),
            self.comments_visible,
            self.comment_wrap,
            self.file_ix,
        )
    }

    /// Put `hunk_ix`-th hunk (the first when None) on screen: cursor + scroll
    /// to its first row, or the file header when the file has no hunks.
    /// This is the "first change on screen" reveal of file mode.
    fn reveal_hunk(&mut self, hunk_ix: Option<usize>) {
        let target = hunk_ix
            .and_then(|ix| self.hunk_rows.get(ix).copied())
            .or_else(|| self.hunk_rows.first().copied())
            .unwrap_or(0);
        self.cursor = target;
        self.scroll
            .scroll_to_item_strict(target, ScrollStrategy::Top);
    }

    /// Show `file_ix` alone in the pane, revealed at `hunk_ix` (its first
    /// hunk when None).
    fn open_file(&mut self, file_ix: usize, hunk_ix: Option<usize>) {
        self.file_ix = file_ix;
        self.selection = None;
        self.set_rows(self.row_build());
        self.reveal_hunk(hunk_ix);
    }

    /// The next (delta = +1) or previous (delta = -1) file, wrapping over
    /// the diff's file list; None when there is nothing to move to.
    fn step_file(&self, delta: i32) -> Option<usize> {
        let n = self.diff.files.len();
        if n < 2 {
            return None;
        }
        Some(((self.file_ix as i32 + delta).rem_euclid(n as i32)) as usize)
    }

    /// The next (delta = +1) or previous (delta = -1) file *with hunks*,
    /// wrapping: which one to open for hunk navigation that runs off the
    /// end of the current file, and which of its hunks to land on (the
    /// first when advancing, the last when retreating).
    fn step_hunk_file(&self, delta: i32) -> Option<(usize, usize)> {
        let n = self.diff.files.len();
        if n == 0 {
            return None;
        }
        let start = self.file_ix as i32;
        for step in 1..=n {
            let file_ix = (start + delta * step as i32).rem_euclid(n as i32) as usize;
            let count = self.diff.files.get(file_ix)?.hunks.len();
            if count > 0 {
                return Some((file_ix, if delta > 0 { 0 } else { count - 1 }));
            }
        }
        None
    }

    /// The target of `n` (delta = +1) or `p` (delta = -1): a row to jump to,
    /// or — having run off the end of the displayed file's hunks — the file
    /// to wrap into, which switches the display.
    fn step_hunk(&self, delta: i32) -> Option<HunkNav> {
        let within = if delta > 0 {
            self.hunk_rows.iter().find(|&&ix| ix > self.cursor)
        } else {
            self.hunk_rows.iter().rev().find(|&&ix| ix < self.cursor)
        };
        if let Some(&ix) = within {
            return Some(HunkNav::Row(ix));
        }
        self.step_hunk_file(delta)
            .map(|(file_ix, hunk_ix)| HunkNav::File { file_ix, hunk_ix })
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
        self.set_rows(self.row_build());
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

/// Which pane an open item shows: the diff (changes), or the conversation
/// — the PR description, rendered as GFM. Items open onto changes; ⇧⌘←/→
/// flips between them. Per item, so ctrl-tab's other item keeps its own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ItemPane {
    #[default]
    Changes,
    Conversation,
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
    /// Which pane the item shows; lives here — not in `ItemData` — so a
    /// refresh (which swaps the data in) never flips the reviewer's view.
    pane: ItemPane,
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
        let meta = github::PrMeta {
            number: snapshot.number,
            title: snapshot.title.clone(),
            author: github::Author {
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
            0,
        );
        let data = Box::new(ItemData::new(
            Some(meta),
            diff,
            ViewMode::Split,
            0,
            (rows, file_rows, hunk_rows),
            None,
            None,
        ));
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
            file_ix,
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
                // Fresh patch-derived data: any previous upgrade is stale —
                // the upgrade re-runs from scratch.
                data.upgrades.clear();
                data.comments = match &data.local_review {
                    Some(local) => Some(local.index()),
                    None => comments,
                };
                // Background rows assumed the fetched comments and the
                // displayed file. Local drafts are reattached here, and
                // view/comment toggles — or a file switch while the fetch
                // ran — may have moved the target, so anything that drifted
                // rebuilds.
                if data.local_review.is_some()
                    || data.mode != mode
                    || !data.comments_visible
                    || file_ix != data.file_ix
                {
                    (rows, file_rows, hunk_rows) = build_rows(
                        &diff,
                        data.mode,
                        &data.upgrades,
                        data.comments.as_ref(),
                        data.comments_visible,
                        data.comment_wrap,
                        data.file_ix,
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
                let local_review =
                    matches!(&self.source, Source::Local(_)).then(LocalReview::default);
                let comments = match &local_review {
                    Some(local) => Some(local.index()),
                    None => comments,
                };
                let data = Box::new(ItemData::new(
                    pr_meta,
                    diff,
                    mode,
                    file_ix,
                    (rows, file_rows, hunk_rows),
                    comments,
                    local_review,
                ));
                self.state = ItemState::Ready(data);
            }
        }
        snapshot
    }
}

fn pr_titlebar_content(meta: &github::PrMeta, cx: &mut Context<ReviewApp>) -> gpui::AnyElement {
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
    users: Rc<RefCell<Vec<github::Mention>>>,
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
fn mention_rank(user: &github::Mention, query: &str) -> Option<u8> {
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
        let mut ranked: Vec<(u8, &github::Mention)> = users
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
fn seed_mentions(cell: &Rc<RefCell<Vec<github::Mention>>>, data: &ItemData) {
    let mut pool = cell.borrow_mut();
    let mut add = |login: &str| {
        if !login.is_empty() && !pool.iter().any(|m| m.login.eq_ignore_ascii_case(login)) {
            pool.push(github::Mention {
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
    verdict: github::ReviewVerdict,
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

pub(crate) struct ReviewApp {
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

/// The anchor `C` resolves when the *keyboard* picks the line (there is no
/// pointer half to take): like the hover `+`, but with the split side chosen
/// by row shape — the right cell when the row has one (a context pair or an
/// added line), the left when the row is a removal with no right cell. The
/// side is irrelevant for unified rows; `comment_anchor` derives theirs from
/// the row kind.
fn cursor_anchor(rows: &[Row], row_ix: usize) -> Option<(CommentSide, u64)> {
    let side = match rows.get(row_ix) {
        Some(Row::SplitLine { right, .. }) if right.is_none() => SelSide::Left,
        _ => SelSide::Right,
    };
    comment_anchor(rows, row_ix, side)
}

/// The comment composer card's x, window-absolute: in split view the card
/// sits under the half its anchor is on (a right-side card starts at the
/// right half's edge), otherwise near the pane's left — and always clamped
/// so the 520px card fits inside the pane.
fn composer_x(pane_left: f32, pane_width: f32, mode: ViewMode, side: CommentSide) -> f32 {
    let half = (pane_width - SPLIT_DIVIDER) / 2.;
    let x = match (mode, side) {
        (ViewMode::Split, CommentSide::Right) => pane_left + half + SPLIT_DIVIDER,
        _ => pane_left + 72.,
    };
    let pane_right = pane_left + pane_width;
    x.min((pane_right - 528.).max(8.))
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
    fn queue_item_locator(pr: &github::QueueItem) -> Option<github::PrLocator> {
        let (owner, repo) = pr.repository.split_once('/')?;
        Some(github::PrLocator {
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
            // PRs still open onto changes — the tracer decision; whether
            // the default flips to conversation-first is a one-line change
            // revisited at dogfood (task 28).
            pane: ItemPane::default(),
        });
        self.active = self.items.len() - 1;
        // Opening something leaves the home screen for the item's pane.
        self.show_queue = false;
        self.spawn_fetch(id, source, ViewMode::Split, cx);
        cx.notify();
    }

    fn spawn_fetch(&self, id: u64, source: Source, mode: ViewMode, cx: &mut Context<Self>) {
        let github = self.github.clone();
        // Build rows for what the item will show when they land: the ready
        // item's displayed file, or the first file for a first load.
        let file_ix = match self
            .items
            .iter()
            .find(|item| item.id == id)
            .map(|item| &item.state)
        {
            Some(ItemState::Ready(data)) => data.file_ix,
            _ => 0,
        };
        cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_spawn(async move { fetch_item(github, &source, mode, file_ix) })
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
                                app.spawn_upgrade(id, gen, source, jobs, cx);
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
        &self,
        id: u64,
        gen: u64,
        source: UpgradeSource,
        jobs: Vec<UpgradeJob>,
        cx: &mut Context<Self>,
    ) {
        let github = self.github.clone();
        cx.spawn(async move |this, cx| {
            let upgraded = cx
                .background_spawn(async move { run_upgrade(github.as_ref(), &source, jobs) })
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
                // Keep the cursor on the same hunk (the re-diff preserves
                // the hunks, so the first change stays on screen); None means
                // the cursor sits at/before the first hunk — the reveal
                // itself, re-anchored below.
                let hunk_ix = data.hunk_rows.iter().rposition(|&ix| ix <= data.cursor);
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
                    data.file_ix,
                ));
                data.selection = None;
                match hunk_ix {
                    Some(hunk_ix) => {
                        let target = data
                            .hunk_rows
                            .get(hunk_ix)
                            .or_else(|| data.hunk_rows.first())
                            .copied()
                            .unwrap_or(0);
                        data.cursor = target;
                        data.scroll
                            .scroll_to_item_strict(target, ScrollStrategy::Top);
                    }
                    None => data.cursor = data.cursor.min(data.rows.len().saturating_sub(1)),
                }
                // Paths can't change in an upgrade, but stats did; rebuilding
                // keeps the tree in lockstep with every row rebuild.
                data.rebuild_tree();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn submit_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.open_input.read(cx).value().trim().to_string();
        if value.is_empty() {
            return;
        }
        let parsed = if Path::new(&value).is_dir() {
            git::resolve_local(Path::new(&value))
                .map(Source::Local)
                .map_err(|err| format!("{err:#}"))
        } else {
            self.github
                .resolve_pr(&value)
                .map(Source::Pr)
                .map_err(|err| err.to_string())
        };
        match parsed {
            Ok(source) => {
                self.open_error = None;
                self.open_input
                    .update(cx, |state, cx| state.set_value("", window, cx));
                self.open_item(source, cx);
                window.focus(&self.focus_handle);
            }
            Err(message) => self.open_error = Some(message.into()),
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
        let github = self.github.clone();
        cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_spawn(async move { github.list_prs(&owner, &repo) })
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
                    Err(err) => PrListState::Failed(err.to_string()),
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
        let source = Source::Pr(github::PrLocator {
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
        self.spawn_fetch(item_id, source, mode, cx);
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

    /// Flip the open item to `pane` (⇧⌘←/→): the PR's conversation — its
    /// description, rendered as GFM — sits one keystroke off the changes,
    /// and ⇧⌘→ is the way back. Already-on-target flips are no-ops, so the
    /// keys stay useful as "go there" rather than "toggle".
    fn flip_pane(&mut self, pane: ItemPane, cx: &mut Context<Self>) {
        if let Some(item) = self.items.get_mut(self.active) {
            if item.pane != pane {
                item.pane = pane;
                cx.notify();
            }
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
        self.spawn_fetch(id, source, mode, cx);
        cx.notify();
    }

    fn toggle_view(&mut self, cx: &mut Context<Self>) {
        let Some(data) = self.active_data_mut() else {
            return;
        };
        // Position preservation: stay on the same hunk (hunk indices are
        // identical across modes).
        let hunk = data.hunk_rows.iter().rposition(|&ix| ix <= data.cursor);
        data.selection = None;
        data.mode = match data.mode {
            ViewMode::Unified => ViewMode::Split,
            ViewMode::Split => ViewMode::Unified,
        };
        data.set_rows(data.row_build());
        let target = hunk
            .and_then(|ix| data.hunk_rows.get(ix).copied())
            .or_else(|| data.hunk_rows.first().copied())
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

    /// `left`/`right`: nudge the diff's horizontal scroll by ten columns.
    /// Split mode never scrolls horizontally (FitList sizes the list to the
    /// viewport and each cell clips its own overflow), so the nudge only
    /// applies to the unified list; the paint-time clamp pins x to
    /// [-max_offset, 0], so overshooting the ends is harmless.
    fn scroll_horizontal(&mut self, direction: i32, window: &mut Window, cx: &mut Context<Self>) {
        let step = f32::from(self.char_width(window)) * 10.;
        let Some(data) = self.active_data_mut() else {
            return;
        };
        if data.mode != ViewMode::Unified {
            return;
        }
        let (offset, max_x) = {
            let state = data.scroll.0.borrow();
            (
                state.base_handle.offset(),
                state.base_handle.max_offset().width,
            )
        };
        let x = (f32::from(offset.x) + direction as f32 * step).clamp(-f32::from(max_x), 0.);
        data.scroll
            .0
            .borrow()
            .base_handle
            .set_offset(point(px(x), offset.y));
        cx.notify();
    }

    /// `n`/`p`: step to the next/previous hunk, wrapping past the ends —
    /// into the next/previous file that has hunks, switching the display.
    fn step_hunk(&mut self, delta: i32, cx: &mut Context<Self>) {
        let nav = self.active_data().and_then(|data| data.step_hunk(delta));
        match nav {
            Some(HunkNav::Row(ix)) => self.jump(ix, cx),
            Some(HunkNav::File { file_ix, hunk_ix }) => {
                if let Some(data) = self.active_data_mut() {
                    data.open_file(file_ix, Some(hunk_ix));
                }
                cx.notify();
            }
            None => {}
        }
    }

    /// `]`/`[`: step to the next/previous file in tree order, wrapping —
    /// the pane switches to it, revealed at its first change.
    fn step_file(&mut self, delta: i32, cx: &mut Context<Self>) {
        let target = self.active_data().and_then(|data| data.step_file(delta));
        let Some(file_ix) = target else {
            return;
        };
        if let Some(data) = self.active_data_mut() {
            data.open_file(file_ix, None);
        }
        cx.notify();
    }

    /// Show `file_ix` in the pane (revealed at its first change) and hand
    /// focus to the diff pane — what clicking a sidebar item does.
    pub(crate) fn jump_to_file(
        &mut self,
        file_ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle);
        if let Some(data) = self.active_data_mut() {
            data.open_file(file_ix, None);
        }
        cx.notify();
    }

    /// Click on an unfiltered tree row: directories toggle collapse, files
    /// jump the diff.
    pub(crate) fn tree_entry_clicked(
        &mut self,
        entry_ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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

    /// `C`: open the composer on the selected line. The selected line is
    /// the text selection's top row when a drag left one (the side it runs
    /// through decides the anchor half), else the keyboard cursor's row —
    /// which every way of moving (n/p, jump, a plain click) keeps on the
    /// line the reviewer is looking at. Only a real line comments: the
    /// anchor goes through `comment_anchor`/`cursor_anchor`, so headers,
    /// comment rows, and binary rows are no-ops.
    fn open_composer_on_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.show_queue || self.composer.is_some() || self.review.is_some() {
            return;
        }
        let Some(item) = self.active_item() else {
            return;
        };
        let ItemState::Ready(data) = &item.state else {
            return;
        };
        let (row_ix, anchor) = match data.selection {
            // A mouse selection names its line and side: anchor under it.
            Some(sel) => {
                let row = sel.ordered().0.row;
                (row, comment_anchor(&data.rows, row, sel.side))
            }
            // No selection: the keyboard cursor names the line; a split
            // row's side falls out of its shape.
            None => (data.cursor, cursor_anchor(&data.rows, data.cursor)),
        };
        let Some((anchor_side, line)) = anchor else {
            return;
        };
        let path = data.diff.files[data.file_ix].display_path().to_string();
        self.open_composer(None, path, anchor_side, line, row_ix, window, cx);
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
    fn ensure_mentions(&mut self, item_id: u64, loc: github::PrLocator, cx: &mut Context<Self>) {
        let Some(data) = self.active_data_mut() else {
            return;
        };
        if data.mentions_fetched {
            return;
        }
        data.mentions_fetched = true;
        let github = self.github.clone();
        cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_spawn(async move { github.mentionable_users(&loc) })
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
        let github = self.github.clone();
        cx.spawn_in(window, async move |this, cx| {
            let post_loc = loc.clone();
            let result = cx
                .background_spawn(async move {
                    match reply_to {
                        Some(root_id) => github.post_reply(&post_loc, root_id, &body),
                        None => github.post_review_comment(
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
                            composer.error = Some(err.to_string().into());
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
    fn refetch_comments(&mut self, item_id: u64, loc: github::PrLocator, cx: &mut Context<Self>) {
        let github = self.github.clone();
        cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_spawn(async move { github.review_comments(&loc) })
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
                        item.refresh_error = Some(format!("comment refresh failed: {err}").into());
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
            verdict: github::ReviewVerdict::Approve,
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
        if body.is_empty() && verdict != github::ReviewVerdict::Approve {
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
        let github = self.github.clone();
        cx.spawn_in(window, async move |this, cx| {
            let submit_loc = loc.clone();
            let result = cx
                .background_spawn(async move { github.submit_review(&submit_loc, verdict, &body) })
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
                            review.error = Some(err.to_string().into());
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
    fn refetch_meta(&mut self, item_id: u64, loc: github::PrLocator, cx: &mut Context<Self>) {
        let github = self.github.clone();
        cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_spawn(async move { github.pr_meta(&loc) })
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
                        item.refresh_error = Some(format!("meta refresh failed: {err}").into());
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
        let path = data.diff.files[data.file_ix].display_path().to_string();
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
        let x = composer_x(
            f32::from(bounds.left()),
            f32::from(bounds.size.width),
            data.mode,
            composer.side,
        );
        let action = if composer.reply_to.is_some() {
            "Reply"
        } else {
            // GitHub's own label for the posts-now mode — the counterpart
            // ("start a review", batched) arrives with pending reviews.
            "Add single comment"
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
            |label: &'static str, verdict: github::ReviewVerdict, color: gpui::Rgba| {
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
            github::ReviewVerdict::Approve => "Approve",
            github::ReviewVerdict::RequestChanges => "Request changes",
            github::ReviewVerdict::Comment => "Comment",
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
                                github::ReviewVerdict::Approve,
                                theme::green(),
                            ))
                            .child(verdict_option(
                                "request changes",
                                github::ReviewVerdict::RequestChanges,
                                theme::red(),
                            ))
                            .child(verdict_option(
                                "comment",
                                github::ReviewVerdict::Comment,
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
            // Follow the diff: highlight the file the pane is showing.
            current_file = Some(data.file_ix);
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
        } else if self
            .active_item()
            .is_some_and(|item| item.pane == ItemPane::Conversation)
        {
            // The conversation pane's hints: the way back to the changes
            // comes first, and the diff's file/hunk hints don't apply there.
            footer = footer
                .child(hint(&["shift-cmd-right"], "changes"))
                .child(hint(&["u"], "queue"))
                .child(hint(&["r"], "refresh"))
                .child(hint(&["cmd-k"], "palette"))
                .child(hint(&["cmd-t"], "open"))
                .child(hint(&["cmd-b"], "sidebar"));
        } else {
            footer = footer
                .child(hint(&["]", "["], "files"))
                .child(hint(&["n", "p"], "hunks"))
                // The conversation pane is one keystroke off the changes.
                .child(hint(&["shift-cmd-left"], "conversation"))
                .child(hint(&["u"], "queue"))
                .child(hint(&["v"], "unified/split"))
                .child(hint(&["m"], "minimap"))
                .child(hint(&["c"], "comments"))
                .child(hint(&["C"], "comment"))
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
    /// The pane for an open item — loading, failure message, the
    /// conversation, or the diff itself — everything the main pane shows
    /// when the queue screen isn't up.
    fn render_item_pane(&self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
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
                ItemState::Ready(data) if item.pane == ItemPane::Conversation => {
                    self.render_conversation(item, data, window, cx)
                }
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
                            // A plain click also moves the keyboard cursor:
                            // `C` and `n`/`p` follow the line the reviewer
                            // last touched, mouse or keyboard alike.
                            if let Some((_, hit)) = this.drag_anchor {
                                if let Some(data) = this.active_data_mut() {
                                    data.cursor = hit.row;
                                }
                            }
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
                        // Unconstrained sizing turns on horizontal overflow,
                        // but gpui sizes the scroll range from one measured
                        // item — row 0, the file header, which never exceeds
                        // the viewport, so `offset.x` was clamped to zero and
                        // long lines were unreachable. Measure the widest
                        // row instead. Split mode sizes to the list and clips
                        // per cell, so it keeps the default measurement.
                        .with_width_from_item(match data.mode {
                            ViewMode::Unified => Some(data.wide_row),
                            ViewMode::Split => None,
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

    /// The conversation pane: the PR's description, rendered as full GFM
    /// — tables, task lists, highlighted code blocks, links — through the
    /// Kit TextView's markdown setup, after the conversation pipe expands
    /// GitHub's `:shortcodes:`, `@mentions`, and `#refs` into characters
    /// and links. The description arrived with the detail fetch (or from
    /// the store snapshot, offline), so it paints instantly, no fetching.
    fn render_conversation(
        &self,
        item: &ReviewItem,
        data: &ItemData,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        // Only a PR has a conversation to read; the titlebar keeps the PR's
        // identity, so the pane can be just the description.
        let (Some(meta), Source::Pr(loc)) = (&data.pr_meta, &item.source) else {
            return centered_message("A local diff has no conversation".into(), theme::overlay0());
        };
        // The pipe runs on every paint; bodies are small, the output is
        // what the TextView's keyed state diffs against (same text → no
        // re-parse), and keeping it out of ItemData means a refresh swaps
        // the description without any cache to invalidate.
        let body = conversation::preprocess(&meta.body, &loc.repo_slug());
        let content = if body.trim().is_empty() {
            // GitHub's own empty state, verbatim.
            div()
                .text_color(theme::overlay0())
                .child(SharedString::from("No description provided."))
                .into_any_element()
        } else {
            TextView::markdown(
                ("conversation", item.id),
                SharedString::from(body),
                window,
                cx,
            )
            // Plain, not scrollable: the pane is the scroll container (see
            // below). Selectable, for copying the description's text.
            .selectable(true)
            .style(TextViewStyle {
                // The pane's font size follows the app's zoom (⌘±/⌘0);
                // headings scale from it, and code blocks pick up the
                // theme's highlight palette.
                heading_base_font_size: px(text_size()),
                paragraph_gap: rems(0.75),
                highlight_theme: cx.theme().highlight_theme.clone(),
                is_dark: !theme::is_light(),
                ..Default::default()
            })
            .into_any_element()
        };
        // The pane itself is the scroll container — like the diff pane, not
        // like the TextView's own scrollable mode — so the wheel scrolls
        // from anywhere over the pane (the centered column's gutters
        // included). The tradeoff: the TextView renders the whole
        // description rather than virtualizing, which descriptions (unlike
        // diffs) are short enough for.
        //
        // Two layers, like the diff pane's: the scroller below, and the
        // Scrollbar overlay outside it, in a non-scrolling relative parent —
        // an absolutely-positioned child *of* the scroller would position
        // against the scrolled content and ride below it, not the viewport.
        div()
            .size_full()
            .relative()
            .text_size(px(text_size()))
            .child(
                div()
                    .id("conversation")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&data.conv_scroll)
                    .child(
                        // A reading column, centered like GitHub's: prose at
                        // line length, not stretched across a diff-width
                        // window. mx_auto (not justify-center) so a
                        // taller-than-pane column keeps its top reachable.
                        div()
                            .max_w(px(760.))
                            .min_w_0()
                            .w_full()
                            .mx_auto()
                            .px_8()
                            .py_6()
                            .child(content),
                    ),
            )
            .child(Scrollbar::vertical(&data.conv_scroll))
            .into_any_element()
    }
}

impl Render for ReviewApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.resync_comment_wrap(window);
        let pane: gpui::AnyElement = if self.show_queue {
            self.render_queue(cx)
        } else {
            self.render_item_pane(window, cx)
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
            .on_action(cx.listener(|this, _: &NextFile, _, cx| this.step_file(1, cx)))
            .on_action(cx.listener(|this, _: &PrevFile, _, cx| this.step_file(-1, cx)))
            .on_action(cx.listener(|this, _: &NextHunk, _, cx| this.step_hunk(1, cx)))
            .on_action(cx.listener(|this, _: &PrevHunk, _, cx| this.step_hunk(-1, cx)))
            .on_action(cx.listener(|this, _: &GoToTop, _, cx| this.jump(0, cx)))
            .on_action(cx.listener(|this, _: &GoToBottom, _, cx| {
                if let Some(last) = this.active_data().map(|d| d.rows.len().saturating_sub(1)) {
                    this.jump(last, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &ScrollLeft, window, cx| {
                this.scroll_horizontal(-1, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ScrollRight, window, cx| {
                this.scroll_horizontal(1, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleView, _, cx| this.toggle_view(cx)))
            .on_action(cx.listener(|this, _: &ToggleMinimap, _, cx| {
                this.minimap_visible = !this.minimap_visible;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleComments, _, cx| this.toggle_comments(cx)))
            .on_action(cx.listener(|this, _: &NewComment, window, cx| {
                this.open_composer_on_selection(window, cx);
            }))
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
            .on_action(cx.listener(|this, _: &ShowConversation, _, cx| {
                this.flip_pane(ItemPane::Conversation, cx)
            }))
            .on_action(
                cx.listener(|this, _: &ShowChanges, _, cx| this.flip_pane(ItemPane::Changes, cx)),
            )
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
    use crate::test_util::{
        add, hunk, queued, rc, row_name, sample_comments, sample_diff, two_file_diff,
    };
    use diff_core::{diff_texts, FileDiff};

    fn mention(login: &str, name: Option<&str>) -> github::Mention {
        github::Mention {
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

    // --- File mode (user stories 28–31) --------------------------------

    /// ItemData exactly as a fresh load builds it: file mode on the first
    /// file (unified, no comments), revealed at its first change.
    fn file_mode_data(diff: PrDiff) -> Box<ItemData> {
        let rows = build_rows(
            &diff,
            ViewMode::Unified,
            &HashMap::new(),
            None,
            true,
            COMMENT_WRAP_CHARS,
            0,
        );
        Box::new(ItemData::new(
            None,
            diff,
            ViewMode::Unified,
            0,
            rows,
            None,
            None,
        ))
    }

    #[test]
    fn opening_a_pr_shows_one_file_revealed_at_its_first_hunk() {
        let data = file_mode_data(two_file_diff());
        assert_eq!(data.file_ix, 0);
        // One file fills the pane: a single header and nothing from b.rs.
        assert_eq!(data.file_rows, vec![0]);
        assert!(matches!(data.rows[0], Row::FileHeader { .. }));
        // Only a.rs's two hunks are built.
        assert_eq!(data.hunk_rows.len(), 2);
        // The first change is on screen: the cursor sits on the first hunk's
        // first row, right after the file header — no `@@` marker row.
        assert_eq!(data.cursor, 1);
        assert!(matches!(data.rows[data.cursor], Row::Line { .. }));
    }

    #[test]
    fn hunk_navigation_wraps_file_to_file() {
        let mut data = file_mode_data(two_file_diff());
        // `n` walks a.rs's two hunks before running off the end.
        assert_eq!(data.step_hunk(1), Some(HunkNav::Row(data.hunk_rows[1])));
        data.cursor = data.hunk_rows[1];
        // Past a.rs's last hunk: wrap into b.rs, landing on its first hunk.
        assert_eq!(
            data.step_hunk(1),
            Some(HunkNav::File {
                file_ix: 1,
                hunk_ix: 0
            })
        );
        match data.step_hunk(1) {
            Some(HunkNav::File { file_ix, hunk_ix }) => data.open_file(file_ix, Some(hunk_ix)),
            _ => unreachable!(),
        }
        assert_eq!(data.file_ix, 1);
        assert_eq!(data.hunk_rows.len(), 1);
        assert_eq!(data.cursor, data.hunk_rows[0]);
        // Past the last hunk of the last file: wraps around to a.rs again.
        assert_eq!(
            data.step_hunk(1),
            Some(HunkNav::File {
                file_ix: 0,
                hunk_ix: 0
            })
        );
        data.open_file(0, Some(0));
        // And `p` from a.rs's first hunk wraps back into b.rs's last hunk.
        assert_eq!(
            data.step_hunk(-1),
            Some(HunkNav::File {
                file_ix: 1,
                hunk_ix: 0
            })
        );
    }

    #[test]
    fn file_navigation_wraps_over_the_file_list() {
        let mut data = file_mode_data(two_file_diff());
        // `]` from the last file wraps to the first, `[` from the first
        // wraps to the last; inside the list they step normally.
        data.open_file(1, None);
        assert_eq!(data.step_file(1), Some(0));
        data.open_file(0, None);
        assert_eq!(data.step_file(-1), Some(1));
        assert_eq!(data.step_file(1), Some(1));
        data.open_file(1, None);
        assert_eq!(data.step_file(-1), Some(0));
        // Opening a file applies the reveal: first change on screen.
        data.open_file(1, None);
        assert_eq!(data.file_ix, 1);
        assert!(matches!(data.rows[0], Row::FileHeader { .. }));
        assert_eq!(data.cursor, data.hunk_rows[0]);
    }

    #[test]
    fn file_mode_renders_deleted_and_renamed_files() {
        // Deleted: every hunk row is a removed line with no new-side number.
        let hunks = diff_texts("a\nb\nc\n", "", 3);
        let deleted = FileDiff {
            old_path: Some("gone.txt".into()),
            new_path: Some("gone.txt".into()),
            status: FileStatus::Deleted,
            hunks,
            additions: 0,
            deletions: 3,
        };
        let data = file_mode_data(PrDiff {
            files: vec![deleted],
        });
        assert!(
            data.rows
                .iter()
                .filter(|row| matches!(
                    row,
                    Row::Line {
                        kind: LineKind::Removed,
                        new_no: None,
                        ..
                    }
                ))
                .count()
                >= 3
        );

        // Renamed: the header carries the old path, hunks render normally.
        let renamed = FileDiff {
            old_path: Some("old.txt".into()),
            new_path: Some("new.txt".into()),
            status: FileStatus::Renamed,
            hunks: vec![hunk(1, 1, vec![add(1, "moved", Vec::new())])],
            additions: 1,
            deletions: 0,
        };
        let data = file_mode_data(PrDiff {
            files: vec![renamed],
        });
        match &data.rows[0] {
            Row::FileHeader {
                old_path,
                status,
                additions,
                ..
            } => {
                assert_eq!(
                    old_path.as_ref().map(|p| p.to_string()),
                    Some("old.txt".to_string())
                );
                assert_eq!(*status, FileStatus::Renamed);
                assert_eq!(*additions, 1);
            }
            other => panic!("expected file header, got {}", row_name(other)),
        }
        assert!(matches!(data.rows[1], Row::Line { .. }));
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

        // The queue screen's tests never open a PR's contents: Enter
        // opens the row's item (asserted in its Loading state before any
        // fetch lands), and the three fetch calls fail cleanly rather
        // than inventing data. The rest of the contract is out of scope
        // — each stub says so loudly.
        fn resolve_pr(&self, _arg: &str) -> Result<github::PrLocator, github::ClientError> {
            unimplemented!("queue tests never resolve a PR")
        }

        fn pr_meta(&self, _loc: &github::PrLocator) -> Result<github::PrMeta, github::ClientError> {
            Err(github::ClientError::Failed(
                "queue tests never read PR meta".into(),
            ))
        }

        fn list_prs(
            &self,
            _owner: &str,
            _repo: &str,
        ) -> Result<Vec<github::PrSummary>, github::ClientError> {
            unimplemented!("queue tests never list PRs")
        }

        fn pr_patch(&self, _loc: &github::PrLocator) -> Result<String, github::ClientError> {
            Err(github::ClientError::Failed(
                "queue tests never fetch a patch".into(),
            ))
        }

        fn review_comments(
            &self,
            _loc: &github::PrLocator,
        ) -> Result<Vec<github::ReviewComment>, github::ClientError> {
            Err(github::ClientError::Failed(
                "queue tests never read comments".into(),
            ))
        }

        fn mentionable_users(
            &self,
            _loc: &github::PrLocator,
        ) -> Result<Vec<github::Mention>, github::ClientError> {
            unimplemented!("queue tests never fetch mentionable users")
        }

        fn file_at(
            &self,
            _loc: &github::PrLocator,
            _commit_oid: &str,
            _path: &str,
        ) -> Result<Option<String>, github::ClientError> {
            unimplemented!("queue tests never fetch file contents")
        }

        fn post_review_comment(
            &self,
            _loc: &github::PrLocator,
            _commit_id: &str,
            _path: &str,
            _side: &str,
            _line: u64,
            _body: &str,
        ) -> Result<(), github::ClientError> {
            unimplemented!("queue tests never post comments")
        }

        fn post_reply(
            &self,
            _loc: &github::PrLocator,
            _comment_id: u64,
            _body: &str,
        ) -> Result<(), github::ClientError> {
            unimplemented!("queue tests never post replies")
        }

        fn submit_review(
            &self,
            _loc: &github::PrLocator,
            _verdict: github::ReviewVerdict,
            _body: &str,
        ) -> Result<(), github::ClientError> {
            unimplemented!("queue tests never submit reviews")
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

        // The queue screen never opens a PR in these tests, so the rest of
        // the contract is out of scope — each stub says so loudly rather
        // than inventing data.
        fn resolve_pr(&self, _arg: &str) -> Result<github::PrLocator, github::ClientError> {
            unimplemented!("queue tests never resolve a PR")
        }

        fn pr_meta(&self, _loc: &github::PrLocator) -> Result<github::PrMeta, github::ClientError> {
            unimplemented!("queue tests never read PR meta")
        }

        fn list_prs(
            &self,
            _owner: &str,
            _repo: &str,
        ) -> Result<Vec<github::PrSummary>, github::ClientError> {
            unimplemented!("queue tests never list PRs")
        }

        fn pr_patch(&self, _loc: &github::PrLocator) -> Result<String, github::ClientError> {
            unimplemented!("queue tests never fetch a patch")
        }

        fn review_comments(
            &self,
            _loc: &github::PrLocator,
        ) -> Result<Vec<github::ReviewComment>, github::ClientError> {
            unimplemented!("queue tests never read comments")
        }

        fn mentionable_users(
            &self,
            _loc: &github::PrLocator,
        ) -> Result<Vec<github::Mention>, github::ClientError> {
            unimplemented!("queue tests never fetch mentionable users")
        }

        fn file_at(
            &self,
            _loc: &github::PrLocator,
            _commit_oid: &str,
            _path: &str,
        ) -> Result<Option<String>, github::ClientError> {
            unimplemented!("queue tests never fetch file contents")
        }

        fn post_review_comment(
            &self,
            _loc: &github::PrLocator,
            _commit_id: &str,
            _path: &str,
            _side: &str,
            _line: u64,
            _body: &str,
        ) -> Result<(), github::ClientError> {
            unimplemented!("queue tests never post comments")
        }

        fn post_reply(
            &self,
            _loc: &github::PrLocator,
            _comment_id: u64,
            _body: &str,
        ) -> Result<(), github::ClientError> {
            unimplemented!("queue tests never post replies")
        }

        fn submit_review(
            &self,
            _loc: &github::PrLocator,
            _verdict: github::ReviewVerdict,
            _body: &str,
        ) -> Result<(), github::ClientError> {
            unimplemented!("queue tests never submit reviews")
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
                        source: Source::Pr(github::PrLocator {
                            owner: "acme".into(),
                            repo: "widgets".into(),
                            number: 1,
                        }),
                        state: ItemState::Failed("stale".into()),
                        reloading: false,
                        refresh_error: None,
                        upgrade_gen: 0,
                        pane: ItemPane::default(),
                    },
                    ReviewItem {
                        id: 1,
                        source: Source::Pr(github::PrLocator {
                            owner: "acme".into(),
                            repo: "widgets".into(),
                            number: 2,
                        }),
                        state: ItemState::Failed("stale".into()),
                        reloading: false,
                        refresh_error: None,
                        upgrade_gen: 0,
                        pane: ItemPane::default(),
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
            source: Source::Pr(github::PrLocator {
                owner: "acme".into(),
                repo: "widgets".into(),
                number: 7,
            }),
            state: ItemState::Loading,
            reloading: false,
            refresh_error: None,
            upgrade_gen: 0,
            pane: ItemPane::default(),
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
            source: Source::Pr(github::PrLocator {
                owner: "acme".into(),
                repo: "widgets".into(),
                number: 8,
            }),
            state: ItemState::Loading,
            reloading: false,
            refresh_error: None,
            upgrade_gen: 0,
            pane: ItemPane::default(),
        };
        assert!(!unknown.install_offline_snapshot(&store));
        assert!(matches!(unknown.state, ItemState::Loading));
    }

    /// ⇧⌘←/→ flip the open item between its changes and its conversation
    /// (user stories 22, 11). The flip is per item, PRs open onto changes,
    /// re-opening an item keeps it where it was, and the conversation —
    /// whose text comes from the store snapshot here — paints through the
    /// real render (the Kit TextView's markdown setup plus the
    /// conversation pipe) without fetching anything.
    #[gpui::test]
    fn the_flip_swaps_changes_and_conversation(cx: &mut gpui::TestAppContext) {
        let fake = Arc::new(FakeQueue {
            asked: std::sync::Mutex::new(Vec::new()),
        });
        let store = local_store::LocalStore::in_memory();
        store.save_snapshot(&local_store::StoredPrSnapshot {
            repository: "acme/widgets".into(),
            number: 7,
            title: "Add deactivation filters".into(),
            body: "## Summary\n\n| area | state |\n|---|---|\n| api | done |\n\n- [x] table\n- [ ] task list\n\nThanks @octocat for :tada: — closes #12.\n\n```rust\nlet x = 1; // highlighted, and :not_emoji: stays\n```".into(),
            author: "alice".into(),
            head_sha: "abc123".into(),
            base_ref: "main".into(),
            head_ref: "feature/deactivation".into(),
            threads: Vec::new(),
            is_draft: false,
            fetched_at_ms: 1_700_000_000_000,
        });
        let (window, app) = queue_window_with_store(cx, fake, store.clone());
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);

        // The offline stand-in: the snapshot makes the PR readable without
        // the network, description and all.
        let loc = github::PrLocator {
            owner: "acme".into(),
            repo: "widgets".into(),
            number: 7,
        };
        let mut item = ReviewItem {
            id: 1,
            source: Source::Pr(loc.clone()),
            state: ItemState::Loading,
            reloading: false,
            refresh_error: None,
            upgrade_gen: 0,
            pane: ItemPane::default(),
        };
        assert!(item.install_offline_snapshot(&store));
        install_item(&app, item, cx);

        // PRs open onto changes — the tracer decision — and ⇧⌘← lands on
        // the conversation, ⇧⌘→ back on the changes, through the real
        // action handlers the keys dispatch.
        let pane = |cx: &gpui::TestAppContext| cx.update(|cx| app.read(cx).items[0].pane);
        assert_eq!(pane(cx), ItemPane::Changes);
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        vcx.dispatch_action(ShowConversation);
        assert_eq!(pane(cx), ItemPane::Conversation);

        // The pane paints from the snapshot's body — full GFM plus the
        // expanded @mention, :shortcode:, and #ref — with no fetch in
        // flight (the fake would have recorded one).
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.run_until_parked();
        vcx.dispatch_action(ShowChanges);
        assert_eq!(pane(cx), ItemPane::Changes);

        // Re-opening the item the session already has brings it forward
        // with its pane intact — flipping isn't something `u`/⌘K resets.
        vcx.dispatch_action(ShowConversation);
        cx.update(|cx| {
            app.update(cx, |this, cx| this.open_item(Source::Pr(loc), cx));
        });
        assert_eq!(pane(cx), ItemPane::Conversation);
        assert_eq!(
            cx.update(|cx| app.read(cx).items.len()),
            1,
            "re-opening forwards the item, never a duplicate"
        );
    }

    /// The conversation pane scrolls from anywhere the cursor is — the
    /// pane is the scroll container, not the 760px reading column inside
    /// it, so the gutters wheel-scroll too (and the scrollbar rides the
    /// pane's edge, not the column's). A regression here is the classic
    /// failure: the TextView's own `scrollable` mode puts the scroller
    /// inside the column, and only the column scrolls.
    #[gpui::test]
    fn the_conversation_scrolls_from_anywhere_over_the_pane(cx: &mut gpui::TestAppContext) {
        let fake = Arc::new(FakeQueue {
            asked: std::sync::Mutex::new(Vec::new()),
        });
        // A description much taller than the 1920×1080 test window, so
        // the pane has scroll range to move.
        let body = "A tall description.\n\n".repeat(200);
        let store = local_store::LocalStore::in_memory();
        store.save_snapshot(&local_store::StoredPrSnapshot {
            repository: "acme/widgets".into(),
            number: 7,
            title: "Tall".into(),
            body,
            author: "alice".into(),
            head_sha: "abc123".into(),
            base_ref: "main".into(),
            head_ref: "feature".into(),
            threads: Vec::new(),
            is_draft: false,
            fetched_at_ms: 1_700_000_000_000,
        });
        let (window, app) = queue_window_with_store(cx, fake, store.clone());
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        let loc = github::PrLocator {
            owner: "acme".into(),
            repo: "widgets".into(),
            number: 7,
        };
        let mut item = ReviewItem {
            id: 1,
            source: Source::Pr(loc),
            state: ItemState::Loading,
            reloading: false,
            refresh_error: None,
            upgrade_gen: 0,
            pane: ItemPane::default(),
        };
        assert!(item.install_offline_snapshot(&store));
        install_item(&app, item, cx);
        vcx.dispatch_action(ShowConversation);
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let conv_offset = |cx: &gpui::TestAppContext| {
            cx.update(|cx| match &app.read(cx).items[0].state {
                ItemState::Ready(data) => data.conv_scroll.offset(),
                _ => panic!("item not ready"),
            })
        };
        assert_eq!(conv_offset(cx).y, px(0.), "starts at the top");

        // The left gutter: inside the conversation pane, outside the
        // centered 760px column (which starts around x≈710 in the
        // 1920-wide test window). A wheel there must scroll — the whole
        // point of the pane-level scroller.
        vcx.simulate_event(gpui::ScrollWheelEvent {
            position: point(px(400.), px(500.)),
            delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-120.))),
            ..Default::default()
        });
        assert_eq!(
            conv_offset(cx).y,
            px(-120.),
            "the wheel scrolls from the gutter, outside the column"
        );

        // And over the column itself, where it always worked.
        vcx.simulate_event(gpui::ScrollWheelEvent {
            position: point(px(1100.), px(500.)),
            delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-120.))),
            ..Default::default()
        });
        assert_eq!(
            conv_offset(cx).y,
            px(-240.),
            "the wheel scrolls over the column too"
        );
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
            let _ = window.draw(cx);
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

    // --- Review comments ----------------------------------------------------

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
    fn comment_anchor_resolves_sides_and_skips_non_lines() {
        let index = sample_comments();
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Unified,
            &HashMap::new(),
            Some(&index),
            true,
            COMMENT_WRAP_CHARS,
            0,
        );
        // Headers and comment rows anchor nothing.
        assert_eq!(comment_anchor(&rows, 0, SelSide::Unified), None);
        assert_eq!(comment_anchor(&rows, 4, SelSide::Unified), None);
        // Context row (1,1) → RIGHT 1; removed old1 → LEFT 2; added new1 → RIGHT 2.
        assert_eq!(
            comment_anchor(&rows, 1, SelSide::Unified),
            Some((CommentSide::Right, 1))
        );
        assert_eq!(
            comment_anchor(&rows, 2, SelSide::Unified),
            Some((CommentSide::Left, 2))
        );
        assert_eq!(
            comment_anchor(&rows, 7, SelSide::Unified),
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
            0,
        );
        assert_eq!(
            comment_anchor(&rows, 2, SelSide::Left),
            Some((CommentSide::Left, 2))
        );
        assert_eq!(
            comment_anchor(&rows, 2, SelSide::Right),
            Some((CommentSide::Right, 2))
        );
        // Second hunk: r2 has no right cell (see split tests above).
        let h2 = hunk_rows[1];
        assert_eq!(comment_anchor(&rows, h2 + 1, SelSide::Right), None);
        assert_eq!(
            comment_anchor(&rows, h2 + 1, SelSide::Left),
            Some((CommentSide::Left, 11))
        );
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

    // --- The review loop: comment, reply, submit (user stories 40, 42, 46, 47)

    /// The meta of the fake PR every loop test runs against: open, head
    /// `dead10cc` — the commit new comments anchor against.
    fn loop_pr_meta() -> github::PrMeta {
        github::PrMeta {
            number: 7,
            title: "Add the thing".into(),
            author: github::Author {
                login: "alice".into(),
            },
            state: "OPEN".into(),
            url: "https://github.com/acme/widgets/pull/7".into(),
            body: String::new(),
            base_ref_name: "main".into(),
            head_ref_name: "feature".into(),
            base_ref_oid: "ba5ec0de".into(),
            head_ref_oid: "dead10cc".into(),
            additions: 1,
            deletions: 1,
            changed_files: 2,
            review_decision: String::new(),
        }
    }

    fn loop_loc() -> github::PrLocator {
        github::PrLocator {
            owner: "acme".into(),
            repo: "widgets".into(),
            number: 7,
        }
    }

    /// A ready PR item over two_file_diff — the state a finished open
    /// leaves behind: file mode on a.rs, cursor revealed at its first
    /// change (the added line 1), carrying `comments` on the diff.
    fn loop_pr_item(comments: Vec<github::ReviewComment>) -> ReviewItem {
        let diff = two_file_diff();
        let index = group_comments(comments);
        let rows = build_rows(
            &diff,
            ViewMode::Unified,
            &HashMap::new(),
            Some(&index),
            true,
            COMMENT_WRAP_CHARS,
            0,
        );
        ReviewItem {
            id: 0,
            source: Source::Pr(loop_loc()),
            state: ItemState::Ready(Box::new(ItemData::new(
                Some(loop_pr_meta()),
                diff,
                ViewMode::Unified,
                0,
                rows,
                Some(index),
                None,
            ))),
            reloading: false,
            refresh_error: None,
            upgrade_gen: 0,
            pane: ItemPane::default(),
        }
    }

    /// Every GitHub mutation the loop makes, as the recording fake saw it
    /// (in order — the tests pin that nothing posts twice or out of turn).
    #[derive(Debug, Clone, PartialEq)]
    enum Post {
        Comment {
            commit_id: String,
            path: String,
            side: String,
            line: u64,
            body: String,
        },
        Reply {
            comment_id: u64,
            body: String,
        },
        Review {
            verdict: github::ReviewVerdict,
            body: String,
        },
    }

    /// A PR-shaped facade for the loop tests: scripted threads on the PR,
    /// recording every mutation the app posts. Like GitHub, a successful
    /// post also lands in the threads the next `review_comments` returns —
    /// the tests watch the refetch show what was posted.
    #[derive(Default)]
    struct FakePr {
        comments: std::sync::Mutex<Vec<github::ReviewComment>>,
        posted: std::sync::Mutex<Vec<Post>>,
        /// While set, posting a new comment fails — the inline-error path.
        fail: std::sync::atomic::AtomicBool,
        /// The id the next posted comment gets.
        next_id: std::sync::atomic::AtomicU64,
    }

    impl FakePr {
        fn with_threads(comments: Vec<github::ReviewComment>) -> Arc<Self> {
            Arc::new(Self {
                comments: std::sync::Mutex::new(comments),
                ..Self::default()
            })
        }

        fn posts(&self) -> Vec<Post> {
            self.posted.lock().unwrap().clone()
        }
    }

    impl github::GitHubClient for FakePr {
        fn viewer(&self) -> Result<github::Viewer, github::ClientError> {
            Ok(github::Viewer {
                login: "tester".into(),
                avatar_url: String::new(),
            })
        }

        fn search_prs(
            &self,
            _search: &github::QueueSearch,
        ) -> Result<Vec<github::QueueItem>, github::ClientError> {
            Ok(Vec::new())
        }

        fn resolve_pr(&self, _arg: &str) -> Result<github::PrLocator, github::ClientError> {
            unimplemented!("loop tests never resolve a PR argument")
        }

        fn pr_meta(&self, _loc: &github::PrLocator) -> Result<github::PrMeta, github::ClientError> {
            Ok(loop_pr_meta())
        }

        fn list_prs(
            &self,
            _owner: &str,
            _repo: &str,
        ) -> Result<Vec<github::PrSummary>, github::ClientError> {
            unimplemented!("loop tests never list PRs")
        }

        fn pr_patch(&self, _loc: &github::PrLocator) -> Result<String, github::ClientError> {
            unimplemented!("loop tests install ready items; no patch is fetched")
        }

        fn review_comments(
            &self,
            _loc: &github::PrLocator,
        ) -> Result<Vec<github::ReviewComment>, github::ClientError> {
            Ok(self.comments.lock().unwrap().clone())
        }

        fn mentionable_users(
            &self,
            _loc: &github::PrLocator,
        ) -> Result<Vec<github::Mention>, github::ClientError> {
            Ok(Vec::new())
        }

        fn file_at(
            &self,
            _loc: &github::PrLocator,
            _commit_oid: &str,
            _path: &str,
        ) -> Result<Option<String>, github::ClientError> {
            unimplemented!("loop tests never fetch file contents")
        }

        fn post_review_comment(
            &self,
            _loc: &github::PrLocator,
            commit_id: &str,
            path: &str,
            side: &str,
            line: u64,
            body: &str,
        ) -> Result<(), github::ClientError> {
            self.posted.lock().unwrap().push(Post::Comment {
                commit_id: commit_id.to_string(),
                path: path.to_string(),
                side: side.to_string(),
                line,
                body: body.to_string(),
            });
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(github::ClientError::Failed("gh: the server said no".into()));
            }
            let id = self
                .next_id
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 100;
            self.comments.lock().unwrap().push(rc(
                id,
                path,
                Some(side),
                Some(line),
                body,
                "tester",
                "2026-10-22T00:00:00Z",
                None,
            ));
            Ok(())
        }

        fn post_reply(
            &self,
            _loc: &github::PrLocator,
            comment_id: u64,
            body: &str,
        ) -> Result<(), github::ClientError> {
            self.posted.lock().unwrap().push(Post::Reply {
                comment_id,
                body: body.to_string(),
            });
            let id = self
                .next_id
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 100;
            self.comments.lock().unwrap().push(rc(
                id,
                "a.rs",
                Some("RIGHT"),
                Some(1),
                body,
                "tester",
                "2026-10-22T00:00:00Z",
                Some(comment_id),
            ));
            Ok(())
        }

        fn submit_review(
            &self,
            _loc: &github::PrLocator,
            verdict: github::ReviewVerdict,
            body: &str,
        ) -> Result<(), github::ClientError> {
            self.posted.lock().unwrap().push(Post::Review {
                verdict,
                body: body.to_string(),
            });
            Ok(())
        }
    }

    /// Install `item` as the pane's only, active item, off the queue screen —
    /// the state after a finished open.
    fn install_item(
        app: &gpui::Entity<ReviewApp>,
        item: ReviewItem,
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            app.update(cx, |this, _| {
                this.show_queue = false;
                this.items = vec![item];
                this.active = 0;
                this.next_id = 1;
            });
        });
    }

    /// The `C` anchor when the *keyboard* picks the line: split rows take
    /// the side from their shape (right cell when there is one, else left),
    /// unified rows from their kind, and non-text rows anchor nothing.
    #[test]
    fn cursor_anchor_picks_the_split_side_from_row_shape() {
        // Split rows: a context pair anchors on the new side, an old1|new1
        // pair too (its right cell exists), and a removal-only row on the
        // left.
        let (rows, _, hunk_rows) = build_rows(
            &sample_diff(),
            ViewMode::Split,
            &HashMap::new(),
            None,
            true,
            COMMENT_WRAP_CHARS,
            0,
        );
        assert_eq!(cursor_anchor(&rows, 0), None, "a file header never anchors");
        assert_eq!(cursor_anchor(&rows, 1), Some((CommentSide::Right, 1)));
        assert_eq!(cursor_anchor(&rows, 2), Some((CommentSide::Right, 2)));
        let h2 = hunk_rows[1];
        assert_eq!(cursor_anchor(&rows, h2 + 1), Some((CommentSide::Left, 11)));

        // Comment rows never anchor, whichever half they sit in.
        let index = sample_comments();
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Split,
            &HashMap::new(),
            Some(&index),
            true,
            COMMENT_WRAP_CHARS,
            0,
        );
        assert_eq!(
            cursor_anchor(&rows, 3),
            None,
            "a comment header never anchors"
        );
        assert_eq!(
            cursor_anchor(&rows, 5),
            None,
            "a reply affordance never anchors"
        );
        // old2|new2 still anchors beneath the threads.
        assert_eq!(cursor_anchor(&rows, 11), Some((CommentSide::Right, 3)));

        // Unified rows: the side is irrelevant, the kind decides — removed
        // lines anchor LEFT, everything else RIGHT.
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Unified,
            &HashMap::new(),
            Some(&index),
            true,
            COMMENT_WRAP_CHARS,
            0,
        );
        assert_eq!(cursor_anchor(&rows, 2), Some((CommentSide::Left, 2)));
        assert_eq!(cursor_anchor(&rows, 7), Some((CommentSide::Right, 2)));
    }

    /// The composer card sits under the half its anchor is on in split view
    /// (regression: it always used to sit on the left), stays left in
    /// unified view, and clamps inside panes too narrow for the 520px card.
    #[test]
    fn composer_x_follows_the_anchored_half_in_split_view() {
        let (left, width) = (100., 1200.);
        // Split: a right-side card starts at the right half's edge; a
        // left-side one stays near the pane's left.
        let half = (width - SPLIT_DIVIDER) / 2.;
        assert_eq!(
            composer_x(left, width, ViewMode::Split, CommentSide::Right),
            left + half + SPLIT_DIVIDER
        );
        assert_eq!(
            composer_x(left, width, ViewMode::Split, CommentSide::Left),
            left + 72.
        );
        // Unified is a single column: the card is always on the left.
        assert_eq!(
            composer_x(left, width, ViewMode::Unified, CommentSide::Right),
            left + 72.
        );
        // A pane too narrow to hold the card: both sides clamp the same way
        // so the card never starts past the pane's right edge.
        assert_eq!(
            composer_x(0., 480., ViewMode::Split, CommentSide::Right),
            8.
        );
        assert_eq!(composer_x(0., 480., ViewMode::Split, CommentSide::Left), 8.);
    }

    /// The loop's first leg: `C` opens the composer on the selected line,
    /// and posting is a single comment at that anchor — the exact call
    /// GitHub needs, nothing more. (user stories 40 and 42)
    #[gpui::test]
    fn c_posts_a_single_comment_at_the_selected_line(cx: &mut gpui::TestAppContext) {
        let fake = FakePr::with_threads(Vec::new());
        let (window, app) = queue_window(cx, fake.clone());
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        install_item(&app, loop_pr_item(Vec::new()), cx);
        cx.run_until_parked();

        vcx.update(|window, cx| {
            app.update(cx, |this, cx| {
                // `C` with no mouse selection anchors on the cursor's line —
                // the reveal put it on a.rs's added line 1.
                this.open_composer_on_selection(window, cx);
                let composer = this.composer.as_ref().expect("C opens the composer");
                assert_eq!(composer.reply_to, None, "C starts a new thread");
                assert_eq!(composer.commit_id, "dead10cc");
                assert_eq!(composer.path, "a.rs");
                assert_eq!(composer.side, CommentSide::Right);
                assert_eq!(composer.line, 1);
                assert_eq!(composer.row_ix, 1, "the card sits beneath its line");
                composer.input.update(cx, |state, cx| {
                    state.set_value("nit: s/teh/the/", window, cx)
                });
                this.submit_composer(window, cx);
                assert!(
                    this.composer.as_ref().unwrap().in_flight,
                    "posting is in flight until gh answers"
                );
            });
        });
        cx.run_until_parked();

        // Exactly one post, at the anchor the composer showed.
        assert_eq!(
            fake.posts(),
            vec![Post::Comment {
                commit_id: "dead10cc".into(),
                path: "a.rs".into(),
                side: "RIGHT".into(),
                line: 1,
                body: "nit: s/teh/the/".into(),
            }]
        );
        cx.update(|cx| {
            app.update(cx, |this, _cx| {
                assert!(
                    this.composer.is_none(),
                    "a posted comment closes the composer"
                );
                // The post landed and the refetch shows it: the thread rides
                // under its anchor line.
                let data = this.active_data().expect("the item is ready");
                let thread = &data.comments.as_ref().expect("comments refetched").threads["a.rs"]
                    [&(CommentSide::Right, 1)][0];
                assert_eq!(thread.root.body, "nit: s/teh/the/");
            });
        });
    }

    /// A failed post surfaces inline in the composer (and doesn't stick
    /// in flight), and Esc cancels without posting anything. (criterion:
    /// errors surface inline; Esc never posts)
    #[gpui::test]
    fn a_failed_post_surfaces_inline_and_escape_cancels_cleanly(cx: &mut gpui::TestAppContext) {
        let fake = FakePr::with_threads(Vec::new());
        fake.fail.store(true, std::sync::atomic::Ordering::SeqCst);
        let (window, app) = queue_window(cx, fake.clone());
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        install_item(&app, loop_pr_item(Vec::new()), cx);
        cx.run_until_parked();

        vcx.update(|window, cx| {
            app.update(cx, |this, cx| {
                this.open_composer_on_selection(window, cx);
                this.composer
                    .as_ref()
                    .unwrap()
                    .input
                    .update(cx, |state, cx| state.set_value("doomed", window, cx));
                this.submit_composer(window, cx);
            });
        });
        cx.run_until_parked();

        cx.update(|cx| {
            app.update(cx, |this, _cx| {
                let composer = this
                    .composer
                    .as_ref()
                    .expect("the composer stays open on failure");
                assert!(!composer.in_flight, "a failed post is retryable");
                assert!(composer.error.is_some(), "gh's failure shows inline");
            });
        });

        // Esc cancels: the composer goes away and nothing more posts (the
        // failed attempt is the only thing gh ever saw).
        vcx.dispatch_action(InputEscape);
        cx.run_until_parked();
        cx.update(|cx| {
            app.update(cx, |this, _cx| {
                assert!(this.composer.is_none(), "escape closes the composer");
            });
        });
        assert_eq!(fake.posts().len(), 1, "cancelling posts nothing");
    }

    /// Replies post into their thread: the reply affordance opens the
    /// composer against the thread's root, and the post is a reply to
    /// that root — never a new top-level comment. (user story 47)
    #[gpui::test]
    fn a_reply_posts_into_its_thread(cx: &mut gpui::TestAppContext) {
        let fake = FakePr::with_threads(vec![rc(
            11,
            "a.rs",
            Some("RIGHT"),
            Some(1),
            "why remove the guard?",
            "alice",
            "2026-01-01T00:00:00Z",
            None,
        )]);
        let (window, app) = queue_window(cx, fake.clone());
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        install_item(
            &app,
            loop_pr_item(fake.comments.lock().unwrap().clone()),
            cx,
        );
        cx.run_until_parked();

        vcx.update(|window, cx| {
            app.update(cx, |this, cx| {
                // The thread is on the pane with its reply affordance; click
                // it the way the affordance does.
                let data = this.active_data().expect("the item is ready");
                let row_ix = data
                    .rows
                    .iter()
                    .position(|row| matches!(row, Row::CommentActions { root_id: 11, .. }))
                    .expect("the thread's reply affordance rides along in file mode");
                let (path, side, line) = match &data.rows[row_ix] {
                    Row::CommentActions {
                        path, side, line, ..
                    } => (path.clone(), *side, *line),
                    _ => unreachable!(),
                };
                this.open_composer(Some(11), path.to_string(), side, line, row_ix, window, cx);
                let composer = this.composer.as_ref().expect("the reply composer opens");
                assert_eq!(composer.reply_to, Some(11), "it targets the thread's root");
                composer.input.update(cx, |state, cx| {
                    state.set_value("nil is fine on this path", window, cx)
                });
                this.submit_composer(window, cx);
            });
        });
        cx.run_until_parked();

        // One post: a reply carrying the root's id — no new thread.
        assert_eq!(
            fake.posts(),
            vec![Post::Reply {
                comment_id: 11,
                body: "nil is fine on this path".into(),
            }]
        );
        cx.update(|cx| {
            app.update(cx, |this, _cx| {
                assert!(this.composer.is_none());
                // The refetch shows the reply inside the thread it belongs to.
                let data = this.active_data().expect("the item is ready");
                let thread = &data.comments.as_ref().expect("comments refetched").threads["a.rs"]
                    [&(CommentSide::Right, 1)][0];
                assert_eq!(thread.root.id, 11);
                assert_eq!(thread.replies.len(), 1);
                assert_eq!(thread.replies[0].body, "nil is fine on this path");
            });
        });
    }

    /// ⌘↵'s dialog: approve / request changes / comment with an optional
    /// body. A bodyless non-approve verdict is rejected locally — inline,
    /// no round trip — and a valid verdict posts exactly once and closes
    /// the dialog. (user story 46)
    #[gpui::test]
    fn the_review_dialog_submits_the_verdict_with_its_body(cx: &mut gpui::TestAppContext) {
        let fake = FakePr::with_threads(Vec::new());
        let (window, app) = queue_window(cx, fake.clone());
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        install_item(&app, loop_pr_item(Vec::new()), cx);
        cx.run_until_parked();

        vcx.update(|window, cx| {
            app.update(cx, |this, cx| {
                this.open_review(window, cx);
                // Request changes with no body: rejected inline, nothing sent.
                this.review.as_mut().unwrap().verdict = github::ReviewVerdict::RequestChanges;
                this.submit_review(window, cx);
                let review = this.review.as_ref().expect("the dialog stays up");
                assert!(!review.in_flight);
                assert!(review.error.is_some(), "the rejection says why inline");
                // Approve with a body: posts, closes, updates the meta.
                this.review.as_mut().unwrap().verdict = github::ReviewVerdict::Approve;
                this.review
                    .as_ref()
                    .unwrap()
                    .input
                    .update(cx, |state, cx| state.set_value("lgtm", window, cx));
                this.submit_review(window, cx);
            });
        });
        cx.run_until_parked();

        assert_eq!(
            fake.posts(),
            vec![Post::Review {
                verdict: github::ReviewVerdict::Approve,
                body: "lgtm".into(),
            }]
        );
        cx.update(|cx| {
            app.update(cx, |this, _cx| {
                assert!(
                    this.review.is_none(),
                    "a submitted review closes the dialog"
                );
            });
        });
    }
}
