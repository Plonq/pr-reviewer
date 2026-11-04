mod chrome;
mod comment_actions;
mod comment_anchor;
mod comment_editor;
mod conversation;
mod diff_media;
mod file_history;
mod file_media;
mod http;
mod image_viewer;
#[cfg(target_os = "macos")]
mod launch_env;
mod local_store;
mod mentions;
mod minimap;
mod position;
mod presentation;
mod queue;
mod quick_open;
mod reactions;
mod resolution;
mod review_dialog;
mod review_scope;
mod review_target;
mod rows;
mod search;
mod selection;
mod svg_image;
mod theme;
mod threads;
mod tree;
mod upgrade;
mod video;
mod video_player;
mod viewed;

#[cfg(test)]
mod test_util;

use chrome::{app_title, centered_message};
use comment_actions::CommentTarget;
#[cfg(test)]
use diff_core::FileStatus;
use diff_core::PrDiff;
use github::review_session::{Command as SessionCommand, ReviewSession};
use gpui::component::{
    avatar::Avatar,
    button::{Button, ButtonVariants as _},
    input::{Enter as InputEnter, Escape as InputEscape, Input, InputEvent, InputState},
    kbd::Kbd,
    scroll::Scrollbar,
    tag::Tag,
    Disableable as _, IconName, Root, Selectable as _, Sizable as _, TitleBar, WindowExt as _,
};
use gpui::{
    actions, canvas, div, fill, font, list, point, prelude::*, px, relative, size, uniform_list,
    App, AssetSource, Bounds, ClipboardItem, Context, FocusHandle, Hsla, KeyBinding, Keystroke,
    ListAlignment, ListHorizontalSizingBehavior, ListOffset, ListState, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, NavigationDirection, Pixels, Point, ScrollHandle,
    ScrollStrategy, SharedString, Subscription, Task, TitlebarOptions, UniformListScrollHandle,
    Window, WindowBounds, WindowOptions,
};
use minimap::{
    minimap_rows, minimap_runs, minimap_scale, MinimapColor, MinimapLane, MinimapLayout,
    MinimapRow, MINIMAP_GAP, MINIMAP_PAD, MINIMAP_WIDTH,
};
use queue::{queue_chips, queue_header, queue_row, QUEUE_MAX_WIDTH, QUEUE_MIN_WIDTH};
#[cfg(test)]
use review_target::fetch_item;
use review_target::{dir_name, sources_match, Loaded, LoadedMeta, Source};
use rows::{
    build_rows, comment_anchor, cursor_anchor, is_comment_row, now_unix, nth_noncomment_row,
    render_row, widest_row_ix, BuiltRows, FileUpgrade, Row, RowOptions, RowsView, ViewMode,
};
use selection::{
    row_selection_range, selection_text, word_selection, RowCol, SelSide, Selection, SelectionDrag,
};
use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use threads::{
    assemble_threads, comment_wrap_cols, group_comments, CommentIndex, CommentSide, CommentThread,
    CommentTime, ReviewComment, COMMENT_WRAP_CHARS,
};
use tree::{
    build_tree, fuzzy_file_matches, render_tree_row, TreeEntry, TreeEntryKind, TreeListRow,
};
#[cfg(test)]
use upgrade::{run_upgrade, UpgradeJob, UpgradeSource};

#[derive(Default)]
struct SessionUi {
    state: ReviewSession,
    busy: bool,
    error: Option<SharedString>,
    /// Per-open-PR disclosure state; drafts start collapsed, not persisted.
    comments_expanded: bool,
}

#[derive(Clone, Copy)]
enum SessionCompletion {
    None,
    Composer(u64),
    Review(u64),
}

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

/// Uniform-list slots for half a viewport of blank space below the diff.
/// Round up so the padding is never shorter than half the pane (at most one
/// row taller). These are presentation-only slots, not source/display rows.
fn diff_padding_rows(pane_height: f32, row_height: f32) -> usize {
    (pane_height.max(0.) / (2. * row_height)).ceil() as usize
}

/// Gutter widths in px, matching render_row's fixed-width children: unified is
/// two 44px line-number columns + a 28px marker; each split cell is one of
/// each. Mouse→column math depends on these.
const UNIFIED_GUTTER: f32 = 44. + 44. + 28.;
const SPLIT_GUTTER: f32 = 44. + 28.;
const SPLIT_DIVIDER: f32 = 6.0;

const DEFAULT_SIDEBAR_WIDTH: f32 = 260.;
const MIN_SIDEBAR_WIDTH: f32 = 180.;
const MAX_SIDEBAR_WIDTH: f32 = 600.;
const MIN_MAIN_PANE_WIDTH: f32 = 320.;
const SIDEBAR_WIDTH_SETTING: &str = "sidebar_width";

fn sidebar_width(width: f32, window_width: f32) -> f32 {
    let max = (window_width - MIN_MAIN_PANE_WIDTH)
        .min(MAX_SIDEBAR_WIDTH)
        .max(0.);
    width.clamp(MIN_SIDEBAR_WIDTH.min(max), max)
}

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
                        _ => return None,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let sort = match value.get("sort").and_then(|sort| sort.as_str()) {
        Some("created_ascending") => github::QueueSort::CreatedAscending,
        _ => github::QueueSort::ReviewRequestedAscending,
    };
    github::QueueSearch::from_parts(filters, sort)
}

actions!(
    gander,
    [
        NextFile,
        PrevFile,
        FileBack,
        FileForward,
        NextHunk,
        PrevHunk,
        GoToTop,
        GoToBottom,
        ScrollLeft,
        ScrollRight,
        ToggleView,
        ToggleViewed,
        ToggleWhitespace,
        ToggleUnchanged,
        ToggleSoftWrap,
        Quit,
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

/// The app's assets, layered over gpui-component's default icon bundle.
/// Queue/media controls use extra Lucide icons; comments use Octicon speech bubble
/// and smiley icons (© GitHub, MIT: https://github.com/primer/octicons).
/// GPUI renders these as monochrome masks so they tint with the palette.
/// App-owned icons live under `icons/gander/` to avoid upstream collisions.
struct AppAssets;

// Catalog entries alone do not bundle SVGs. Include the queue/media/comment icons
// explicitly rather than relying on the default component bundle's subset.
gpui_kit_assets::icon_assets!(
    ExtraIcons,
    [
        Circle,
        Play,
        Pause,
        Volume2,
        VolumeX,
        Maximize2,
        Minimize2,
        RotateCcw,
        ExternalLink,
        X,
        Pencil,
        Trash,
        Square,
        SquareCheck
    ]
);

/// (asset path, embedded bytes) — served exactly as an `AssetSource`
/// expects, so the icon renders like any bundled one.
const OWNED_ASSETS: &[(&str, &[u8])] = &[
    (
        "icons/gander/comment-16.svg",
        include_bytes!("assets/icons/gander/comment-16.svg"),
    ),
    (
        "icons/gander/smiley-16.svg",
        include_bytes!("assets/icons/gander/smiley-16.svg"),
    ),
    (
        "icons/gander/check-16.svg",
        include_bytes!("assets/icons/gander/check-16.svg"),
    ),
    (
        "icons/gander/diff-16.svg",
        include_bytes!("assets/icons/gander/diff-16.svg"),
    ),
];

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        if let Some((_, bytes)) = OWNED_ASSETS.iter().find(|(known, _)| *known == path) {
            return Ok(Some(Cow::Borrowed(bytes)));
        }
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        let mut all = gpui::assets::Assets.list(path)?;
        all.extend(ExtraIcons.list(path)?);
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
    #[cfg(target_os = "macos")]
    launch_env::init();
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
    // stand in stays one explicit act away (`.` as an argument or in
    // quick-open), so local review keeps working
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
    gpui::application()
        .with_assets(AppAssets)
        // gpui ships a NullHttpClient: without a real one, every remote
        // `img()` (the account avatar, later PR media) silently fails.
        .with_http_client(http::http_client())
        .run(move |cx: &mut App| {
            gpui::component::init(cx);
            theme::apply_ui_theme(cx);
            cx.bind_keys(presentation::keybindings());
            cx.bind_keys(search::keybindings());
            cx.bind_keys(file_history::keybindings());
            cx.bind_keys([
                KeyBinding::new("]", NextFile, Some("ReviewApp")),
                KeyBinding::new("[", PrevFile, Some("ReviewApp")),
                KeyBinding::new("n", NextHunk, Some("ReviewApp")),
                KeyBinding::new("p", PrevHunk, Some("ReviewApp")),
                KeyBinding::new("home", GoToTop, Some("ReviewApp")),
                KeyBinding::new("end", GoToBottom, Some("ReviewApp")),
                // Sideways through long lines in either diff mode. Inputs
                // deeper in the tree (a focused filter input) keep
                // their own left/right for caret movement.
                KeyBinding::new("left", ScrollLeft, Some("ReviewApp")),
                KeyBinding::new("right", ScrollRight, Some("ReviewApp")),
                KeyBinding::new("v", ToggleView, Some("ReviewApp")),
                KeyBinding::new("shift-v", ToggleViewed, Some("ReviewApp")),
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
                // item stays open behind the scenes (ctrl-tab reaches
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
                // Selection: escape/cmd-c only fire while the diff pane has focus.
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
                KeyBinding::new("cmd-o", OpenInput, None),
                KeyBinding::new("cmd-w", CloseItem, None),
                KeyBinding::new("cmd-q", Quit, None),
            ]);
            cx.on_action(|_: &Quit, cx| cx.quit());
            // One window is the whole app: closing it quits the process.
            cx.on_window_closed(|cx, _| {
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
                    window.focus(&view.read(cx).focus_handle.clone(), cx);
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
    comments: Vec<ReviewComment>,
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
        self.comments.push(ReviewComment {
            id: self.next_id,
            path,
            line: Some(line),
            side,
            start_line: None,
            body,
            author: "you".to_string(),
            time: CommentTime::Draft,
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
    /// False for conversation-only data, not a genuinely empty diff.
    diff_loaded: bool,
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
    split_columns: usize,
    /// Minimap model, index-aligned with `rows`; rebuilt with them.
    minimap: Vec<MinimapRow>,
    /// Coalesced minimap quad runs for one pane height and blank tail, computed
    /// lazily on paint and reused until either changes or the rows are rebuilt.
    minimap_cache: RefCell<Option<(f32, usize, Rc<MinimapLayout>)>>,
    cursor: usize,
    cursor_side: Option<SelSide>,
    /// Original server-patch hunks, retained when full contents replace display
    /// hunks. Determines when comments need the experimental positioning API.
    anchor_hunks: Vec<Vec<diff_core::Hunk>>,
    scroll: UniformListScrollHandle,
    position_restore: Option<position::PositionRestore>,
    saved_position: Option<local_store::ReviewPosition>,
    /// In-session viewport memory for files visited in this item, keyed by
    /// path so refreshes that reorder the diff do not mix up positions.
    file_positions: HashMap<String, local_store::ReviewPosition>,
    file_history: file_history::FileHistory,
    search: Option<search::Search>,
    /// The conversation pane's scroll position. A pane-level container (not
    /// the TextView's own), so the wheel scrolls from anywhere over the pane
    /// and the scrollbar paints at the pane's edge — like the diff pane's.
    conv_scroll: ScrollHandle,
    discussion: Option<github::PrDiscussion>,
    /// Invalidate older comment fetches when a resolution write starts.
    discussion_generation: u64,
    posts: conversation::posting::Posts,
    commits: Option<github::PrCommits>,
    commits_expanded: bool,
    review_changes: github::ReviewChanges,
    pr_base: Option<String>,
    review_base: Option<String>,
    review_base_files: Option<Arc<HashMap<String, github::ComparisonBase>>>,
    viewed: viewed::Viewed,
    expanded_threads: HashSet<String>,
    additions: u32,
    deletions: u32,
    /// Mouse text selection, in display-row space. Per item (survives item
    /// switching); cleared on view-mode toggle and refresh, where row indices
    /// change meaning.
    selection: Option<Selection>,
    /// Full-file contents by file index into `diff.files`: whole-file span
    /// tables and full new-side lines. The re-diffed hunks themselves
    /// replace `diff.files[ix].hunks`. Reset on refresh.
    upgrades: HashMap<usize, FileUpgrade>,
    file_media: HashMap<usize, file_media::MediaFile>,
    /// SVG source-view choices, remembered per path for this open review.
    svg_code: HashSet<String>,
    /// Review threads grouped by anchor; Some for PR items (possibly empty),
    /// and for local items with in-memory draft comments.
    comments: Option<CommentIndex>,
    local_review: Option<LocalReview>,
    /// `c` toggles the comment rows; the file-header counts always show.
    comments_visible: bool,
    editing_comment: Option<(u64, Rc<Cell<Pixels>>)>,
    /// Columns used for initial comment-height estimates, derived from the
    /// measured pane width (see `comment_wrap_cols`). Changing it re-wraps the
    /// estimates, which changes the row count, so render rebuilds the rows when
    /// the pane resizes past a column boundary.
    comment_wrap: usize,
    comment_text_layouts: diff_media::TextLayouts,
    preferences: store::DiffPreferences,
    preference_overrides: store::DiffPreferences,
    expanded_regions: HashSet<(usize, u32)>,
    code_wrap: usize,
    /// Users offered by the composer's `@`-mention autocomplete. Shared live
    /// with any open `MentionProvider`; seeded from PR participants, then
    /// filled from the repo's mentionable set (see `mentions_fetched`).
    mentions: Rc<RefCell<Vec<github::Mention>>>,
    /// The one-time background mentionable-user fetch has been kicked off.
    mentions_fetched: bool,
    /// Sidebar file tree, rebuilt whenever the diff itself changes (load,
    /// refresh) — but not on view-mode toggles: entries map to
    /// file indices, not row indices, so they survive row rebuilds.
    pub(crate) tree: Vec<TreeEntry>,
    /// Collapsed directory paths, preserved across rebuilds where the
    /// directory still exists.
    pub(crate) collapsed: HashSet<String>,
    tree_view: Option<tree::FileTree>,
    /// The file last auto-centered in the tree, so follow-the-diff only
    /// scrolls the tree when the viewport's file changes — never while the
    /// user scrolls the tree themselves.
    tree_last_file: Option<usize>,
}

impl ItemData {
    /// A fresh item's data, showing the sidebar's first file at its first change.
    /// `file_ix` identifies the supplied background-built rows, not tree order.
    fn new(
        pr_meta: Option<github::PrMeta>,
        diff: PrDiff,
        mode: ViewMode,
        file_ix: usize,
        built_rows: BuiltRows,
        comments: Option<CommentIndex>,
        local_review: Option<LocalReview>,
    ) -> Self {
        let mut data = Self {
            pr_meta,
            anchor_hunks: diff.files.iter().map(|file| file.hunks.clone()).collect(),
            diff,
            diff_loaded: true,
            mode,
            file_ix,
            rows: Vec::new(),
            file_rows: Vec::new(),
            hunk_rows: Vec::new(),
            wide_row: 0,
            split_columns: 0,
            minimap: Vec::new(),
            minimap_cache: RefCell::new(None),
            cursor: 0,
            cursor_side: None,
            scroll: UniformListScrollHandle::new(),
            position_restore: None,
            saved_position: None,
            file_positions: HashMap::new(),
            file_history: Default::default(),
            search: None,
            conv_scroll: ScrollHandle::new(),
            discussion: None,
            discussion_generation: 0,
            posts: Default::default(),
            commits: None,
            commits_expanded: false,
            review_changes: github::ReviewChanges::default(),
            pr_base: None,
            review_base: None,
            review_base_files: None,
            viewed: Default::default(),
            expanded_threads: HashSet::new(),
            additions: 0,
            deletions: 0,
            selection: None,
            upgrades: HashMap::new(),
            file_media: HashMap::new(),
            svg_code: HashSet::new(),
            comments,
            local_review,
            comments_visible: true,
            editing_comment: None,
            comment_wrap: COMMENT_WRAP_CHARS,
            comment_text_layouts: HashMap::new(),
            preferences: Default::default(),
            preference_overrides: Default::default(),
            expanded_regions: HashSet::new(),
            code_wrap: 80,
            mentions: Rc::new(RefCell::new(Vec::new())),
            mentions_fetched: false,
            tree: Vec::new(),
            collapsed: HashSet::new(),
            tree_view: None,
            tree_last_file: None,
        };
        data.additions = data.diff.files.iter().fold(0, |a, f| a + f.additions);
        data.deletions = data.diff.files.iter().fold(0, |d, f| d + f.deletions);
        data.rebuild_tree();
        let first_file = data.file_order().next().unwrap_or(0);
        if data.file_ix == first_file {
            data.set_rows(built_rows);
        } else {
            data.file_ix = first_file;
            data.set_rows(data.row_build());
        }
        data.reveal_hunk(None);
        data
    }

    /// Install freshly built display rows, keeping the minimap model in sync
    /// (every row rebuild goes through here).
    fn set_rows(&mut self, mut built_rows: BuiltRows) {
        let collapsed = self
            .discussion
            .iter()
            .flat_map(|d| &d.threads)
            .filter(|thread| {
                thread.is_resolved
                    && !self.expanded_threads.contains(&thread.id)
                    && !self
                        .editing_comment
                        .as_ref()
                        .is_some_and(|(id, _)| thread.comments.iter().any(|c| c.id == *id))
            })
            .filter_map(|thread| thread.comments.first().map(|c| c.id))
            .collect();
        resolution::collapse_threads(&mut built_rows, &collapsed);
        diff_media::resize_text_rows(&mut built_rows, &self.comment_text_layouts);
        if let Some((id, height)) = &self.editing_comment {
            comment_actions::reserve_editor_rows(
                &mut built_rows,
                *id,
                (f32::from(height.get()) / row_height()).ceil() as usize,
            );
        }
        reactions::reserve_reaction_rows(&mut built_rows, self.discussion.as_ref());
        let BuiltRows {
            rows,
            file_rows,
            hunk_rows,
        } = built_rows;
        self.minimap = minimap_rows(&rows);
        self.minimap_cache.replace(None);
        self.wide_row = match self.mode {
            ViewMode::Unified => widest_row_ix(&rows),
            ViewMode::Split => rows
                .iter()
                .position(|row| matches!(row, Row::SplitLine { .. }))
                .unwrap_or(0),
        };
        self.split_columns = rows
            .iter()
            .filter_map(|row| match row {
                Row::SplitLine { left, right } => Some(
                    left.iter()
                        .chain(right.iter())
                        .map(|cell| cell.text.chars().count())
                        .max()
                        .unwrap_or(0),
                ),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        self.rows = rows;
        self.file_rows = file_rows;
        self.hunk_rows = hunk_rows;
        self.reindex_search();
    }

    /// Rebuild the display rows for the displayed file and mode.
    fn row_build(&self) -> BuiltRows {
        rows::build_presented_rows(
            RowsView {
                diff: &self.diff,
                upgrades: &self.upgrades,
                comments: self.comments.as_ref(),
                options: RowOptions {
                    mode: self.mode,
                    file_ix: self.file_ix,
                    comments_visible: self.comments_visible,
                    comment_wrap: self.comment_wrap,
                },
            },
            if self.search.is_some() {
                store::DiffPreferences {
                    collapse_unchanged: Some(false),
                    hide_whitespace: Some(false),
                    ..self.preferences
                }
            } else {
                self.preferences
            },
            &self.expanded_regions,
            self.code_wrap,
        )
    }

    /// Put `hunk_ix`-th hunk (the first when None) on screen: cursor + scroll
    /// to its first row, or the file header when the file has no hunks.
    /// This is the "first change on screen" reveal of file mode.
    fn reveal_hunk(&mut self, hunk_ix: Option<usize>) {
        self.position_restore = None;
        let target = hunk_ix
            .and_then(|ix| self.hunk_rows.get(ix).copied())
            .or_else(|| self.hunk_rows.first().copied())
            .unwrap_or(0);
        self.cursor = target;
        self.cursor_side = None;
        self.scroll
            .scroll_to_item_strict(target, ScrollStrategy::Top);
        self.remember_jump();
    }

    /// Show `file_ix` alone in the pane. An explicit hunk takes precedence;
    /// otherwise resume its viewport, or reveal the first hunk on first visit.
    fn open_file(&mut self, file_ix: usize, hunk_ix: Option<usize>) {
        let Some(target) = self.diff.files.get(file_ix) else {
            return;
        };
        if let Some(current) = self.diff.files.get(self.file_ix) {
            self.file_history
                .record(current.display_path(), target.display_path());
        }
        self.show_file(file_ix, hunk_ix);
    }

    /// Restore/reload/history navigation must not create new history entries.
    fn show_file(&mut self, file_ix: usize, hunk_ix: Option<usize>) {
        self.end_search();
        if let Some(position) = self.review_position() {
            self.file_positions.insert(position.path.clone(), position);
        }
        self.file_ix = file_ix;
        self.selection = None;
        self.set_rows(self.row_build());
        if hunk_ix.is_none() {
            let position = self
                .diff
                .files
                .get(file_ix)
                .and_then(|file| self.file_positions.get(file.display_path()))
                .cloned();
            if let Some(mut position) = position {
                // View mode belongs to the item, not the individual file.
                position.split = self.mode == ViewMode::Split;
                if self.restore_review_position(position, true) {
                    return;
                }
            }
        }
        self.reveal_hunk(hunk_ix);
    }

    /// All files in the sidebar's depth-first, directories-first order.
    /// Collapsing or filtering the tree must not skip files during a review.
    fn file_order(&self) -> impl Iterator<Item = usize> + '_ {
        self.tree.iter().filter_map(|entry| match entry.kind {
            TreeEntryKind::File { file_ix } => Some(file_ix),
            TreeEntryKind::Dir { .. } => None,
        })
    }

    /// The next (delta = +1) or previous (delta = -1) file in sidebar order,
    /// wrapping; None when there is nothing to move to.
    fn step_file(&self, delta: i32) -> Option<usize> {
        let files: Vec<_> = self.file_order().collect();
        let n = files.len();
        if n < 2 {
            return None;
        }
        let current = files.iter().position(|&file_ix| file_ix == self.file_ix)?;
        Some(files[(current as i32 + delta).rem_euclid(n as i32) as usize])
    }

    /// The next (delta = +1) or previous (delta = -1) file *with hunks*,
    /// wrapping: which one to open for hunk navigation that runs off the
    /// end of the current file, and which of its hunks to land on (the
    /// first when advancing, the last when retreating).
    fn step_hunk_file(&self, delta: i32) -> Option<(usize, usize)> {
        let files: Vec<_> = self.file_order().collect();
        let n = files.len();
        let start = files.iter().position(|&file_ix| file_ix == self.file_ix)? as i32;
        for step in 1..=n {
            let file_ix = files[(start + delta * step as i32).rem_euclid(n as i32) as usize];
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
        let pending = (self.position_restore.is_some()
            || self.scroll.0.borrow().deferred_scroll_to_item.is_some())
        .then(|| self.review_position())
        .flatten();
        let offset = self.scroll.0.borrow().base_handle.offset();
        let top_px = f32::from(-offset.y).max(0.);
        let top_row =
            ((top_px / row_height()).floor() as usize).min(self.rows.len().saturating_sub(1));
        let frac = top_px - top_row as f32 * row_height();
        let top_text = match self.rows.get(top_row) {
            Some(Row::CommentBody { text, slot, .. }) => Some((text.clone(), *slot)),
            _ => None,
        };
        let count_noncomment =
            |rows: &[Row]| rows.iter().filter(|row| !is_comment_row(row)).count();
        let top_base = count_noncomment(&self.rows[..top_row]);
        let cursor_base = count_noncomment(&self.rows[..self.cursor.min(self.rows.len())]);
        self.set_rows(self.row_build());
        self.selection = None;
        self.cursor = nth_noncomment_row(&self.rows, cursor_base);
        let new_top = top_text
            .and_then(|(text, slot)| {
                self.rows
                    .iter()
                    .enumerate()
                    .filter_map(|(ix, row)| match row {
                        Row::CommentBody {
                            text: next,
                            slot: next_slot,
                            ..
                        } if text.comment_id == next.comment_id
                            && text.part == next.part
                            && *next_slot <= slot =>
                        {
                            Some(ix)
                        }
                        _ => None,
                    })
                    .last()
            })
            .unwrap_or_else(|| nth_noncomment_row(&self.rows, top_base));
        self.scroll
            .0
            .borrow()
            .base_handle
            .set_offset(point(offset.x, px(-(new_top as f32 * row_height() + frac))));
        if let Some(mut position) = pending {
            position.split = self.mode == ViewMode::Split;
            self.restore_review_position(position, true);
            self.cursor = nth_noncomment_row(&self.rows, cursor_base);
        }
    }

    /// The minimap quad runs for this pane height and blank tail, from the
    /// cache when neither has changed since the last paint.
    fn minimap_layout(&self, pane_px: f32, tail_rows: usize) -> Rc<MinimapLayout> {
        let mut cache = self.minimap_cache.borrow_mut();
        if let Some((h, tail, layout)) = &*cache {
            if *h == pane_px && *tail == tail_rows {
                return layout.clone();
            }
        }
        let layout = Rc::new(minimap_runs(&self.minimap, pane_px, tail_rows));
        *cache = Some((pane_px, tail_rows, layout.clone()));
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
/// — the PR description, rendered as GFM. PRs open onto conversation, local
/// diffs onto changes; ⇧⌘←/→ flips between them. Per item, so ctrl-tab's
/// other item keeps its own.
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
    /// Bumped at fetch start; only the latest fetch may install its result.
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
        let built_rows = build_rows(RowsView {
            diff: &diff,
            upgrades: &HashMap::new(),
            comments: None,
            options: RowOptions {
                mode: ViewMode::Split,
                file_ix: 0,
                comments_visible: true,
                comment_wrap: COMMENT_WRAP_CHARS,
            },
        });
        let mut data = Box::new(ItemData::new(
            Some(meta),
            diff,
            ViewMode::Split,
            0,
            built_rows,
            None,
            None,
        ));
        data.diff_loaded = false;
        data.discussion = snapshot
            .discussion
            .and_then(|value| serde_json::from_value(value).ok());
        data.commits = snapshot
            .commits
            .and_then(|value| serde_json::from_value(value).ok());
        data.load_preferences(store, Some(loc));
        self.state = ItemState::Ready(data);
        true
    }

    /// Install the first conversation without waiting for any diff work.
    /// Refreshes retain their existing data until the complete fetch lands.
    fn install_conversation(&mut self, loaded: review_target::LoadedConversation, mode: ViewMode) {
        if matches!(self.state, ItemState::Ready(_)) {
            return;
        }
        let diff = PrDiff::default();
        let comments = group_comments(
            loaded
                .discussion
                .review_comments()
                .into_iter()
                .map(ReviewComment::from)
                .collect(),
        );
        let built = build_rows(RowsView {
            diff: &diff,
            upgrades: &HashMap::new(),
            comments: Some(&comments),
            options: RowOptions {
                mode,
                file_ix: 0,
                comments_visible: true,
                comment_wrap: COMMENT_WRAP_CHARS,
            },
        });
        let mut data = ItemData::new(
            Some(loaded.meta),
            diff,
            mode,
            0,
            built,
            Some(comments),
            None,
        );
        data.diff_loaded = false;
        data.discussion = Some(loaded.discussion);
        self.state = ItemState::Ready(Box::new(data));
    }

    fn dot_color(&self, cx: &gpui::App) -> gpui::Rgba {
        match &self.source {
            Source::Local(_) => theme::blue(cx),
            Source::Pr(_) => match &self.state {
                ItemState::Ready(data) => match data.pr_meta.as_ref().map(|m| m.state.as_str()) {
                    Some("OPEN") => theme::green(cx),
                    Some("MERGED") => theme::mauve(cx),
                    Some("CLOSED") => theme::red(cx),
                    _ => theme::overlay0(cx),
                },
                ItemState::Failed(_) => theme::red(cx),
                ItemState::Loading => theme::overlay0(cx),
            },
        }
    }

    /// Swap fetched data in. On refresh (already Ready) the file, scroll
    /// anchor, and view mode are preserved.
    ///
    /// Returns the PR's snapshot, when the fetch was a PR's: the caller
    /// keeps it in the local store, so the next launch knows what the app
    /// last saw of this PR.
    fn install(&mut self, loaded: Loaded) -> Option<local_store::StoredPrSnapshot> {
        let Loaded {
            meta,
            diff,
            anchor_hunks,
            upgrades,
            file_media,
            mut built_rows,
            mode,
            file_ix,
            comments,
            discussion,
            commits,
            review_changes,
            pr_base,
            review_base,
            review_base_files,
            viewed,
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
                let position = if data.review_base == review_base {
                    data.review_position()
                } else {
                    data.file_positions.clear();
                    data.file_history = Default::default();
                    data.saved_position = None;
                    None
                };
                data.search = None;
                data.pr_base = pr_base;
                data.review_changes = review_changes;
                data.review_base = review_base;
                data.review_base_files = review_base_files;
                data.discussion_generation += 1;
                data.upgrades = upgrades;
                data.file_media = file_media;
                data.expanded_regions.clear();
                data.discussion = discussion;
                if let Some(discussion) = &data.discussion {
                    data.posts.reconcile(discussion);
                }
                data.commits = commits;
                data.viewed.install(viewed);
                data.comments = match &data.local_review {
                    Some(local) => Some(local.index()),
                    None => comments,
                };
                if pr_meta.is_some() {
                    data.pr_meta = pr_meta;
                }
                data.anchor_hunks = anchor_hunks;
                data.diff = diff;
                data.diff_loaded = true;
                data.rebuild_tree();
                let first_file = data.file_order().next().unwrap_or(0);
                data.file_ix = position
                    .as_ref()
                    .and_then(|position| {
                        data.diff
                            .files
                            .iter()
                            .position(|file| file.display_path() == position.path)
                    })
                    .unwrap_or(first_file);
                // Reuse the background-built rows unless the displayed file
                // or presentation drifted while the fetch was in flight.
                if data.local_review.is_some()
                    || data.mode != mode
                    || !data.comments_visible
                    || data.comment_wrap != COMMENT_WRAP_CHARS
                    || file_ix != data.file_ix
                {
                    built_rows = build_rows(RowsView {
                        diff: &data.diff,
                        upgrades: &data.upgrades,
                        comments: data.comments.as_ref(),
                        options: RowOptions {
                            mode: data.mode,
                            file_ix: data.file_ix,
                            comments_visible: data.comments_visible,
                            comment_wrap: data.comment_wrap,
                        },
                    });
                }
                data.set_rows(built_rows);
                if !position.is_some_and(|position| data.restore_review_position(position, true)) {
                    data.reveal_hunk(None);
                }
                data.file_history.retain(
                    data.diff.files.iter().map(|file| file.display_path()),
                    data.diff
                        .files
                        .get(data.file_ix)
                        .map(|file| file.display_path()),
                );
                data.cursor = data.cursor.min(data.rows.len().saturating_sub(1));
                data.additions = additions;
                data.deletions = deletions;
                data.selection = None;
                data.cursor_side = None;
            }
            _ => {
                let local_review =
                    matches!(&self.source, Source::Local(_)).then(LocalReview::default);
                let comments = match &local_review {
                    Some(local) => Some(local.index()),
                    None => comments,
                };
                let mut data = Box::new(ItemData::new(
                    pr_meta,
                    diff,
                    mode,
                    file_ix,
                    built_rows,
                    comments,
                    local_review,
                ));
                data.anchor_hunks = anchor_hunks;
                data.upgrades = upgrades;
                data.file_media = file_media;
                data.discussion = discussion;
                data.commits = commits;
                data.pr_base = pr_base;
                data.review_changes = review_changes;
                data.review_base = review_base;
                data.review_base_files = review_base_files;
                data.viewed.install(viewed);
                data.set_rows(data.row_build());
                // Full-file context may move the first change beyond the rows
                // available while ItemData::new selected the top tree file.
                data.reveal_hunk(None);
                self.state = ItemState::Ready(data);
            }
        }
        snapshot
    }
}

// Explicitly shared with media overlays so native window controls stay clear.
const APP_HEADER_HEIGHT: Pixels = px(34.);

fn pr_titlebar_content(meta: &github::PrMeta, cx: &mut Context<ReviewApp>) -> gpui::AnyElement {
    let (state_color, state_label) = match meta.state.as_str() {
        "OPEN" => (theme::green(cx), "open"),
        "MERGED" => (theme::mauve(cx), "merged"),
        "CLOSED" => (theme::red(cx), "closed"),
        other => (theme::overlay0(cx), other),
    };
    let state: Hsla = state_color.into();
    // The PR's overall review decision, when it has one.
    let decision = match meta.review_decision.as_str() {
        "APPROVED" => Some((theme::green(cx), "approved")),
        "CHANGES_REQUESTED" => Some((theme::red(cx), "changes requested")),
        _ => None,
    };
    let url = meta.url.clone();
    let copy_url = meta.url.clone();
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
                        .text_color(theme::subtext(cx))
                        .child(SharedString::from(format!("#{}", meta.number))),
                )
                .child(
                    div()
                        .text_color(theme::subtext(cx))
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
                        .text_color(theme::overlay0(cx))
                        .child(SharedString::from(format!(
                            "{} ← {}",
                            meta.base_ref_name, meta.head_ref_name
                        ))),
                )
                .child(
                    div()
                        .text_color(theme::green(cx))
                        .child(SharedString::from(format!("+{}", meta.additions))),
                )
                .child(
                    div()
                        .text_color(theme::red(cx))
                        .child(SharedString::from(format!("−{}", meta.deletions))),
                )
                .child(
                    Button::new("copy-pr-link")
                        .debug_selector(|| "copy-pr-link".into())
                        .icon(gpui_kit_assets::IconName::Copy)
                        .ghost()
                        .xsmall()
                        .tooltip("Copy PR link")
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy_url.clone()));
                        }),
                )
                .child(
                    Button::new("open-in-browser")
                        .debug_selector(|| "open-in-browser".into())
                        .icon(IconName::ExternalLink)
                        .ghost()
                        .xsmall()
                        .tooltip("Open PR in GitHub")
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
    _item_id: u64,
    src: &git::LocalSource,
    data: &ItemData,
    cx: &mut Context<ReviewApp>,
) -> gpui::AnyElement {
    let blue: Hsla = theme::blue(cx).into();
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
                        .px_1()
                        .text_color(theme::overlay0(cx))
                        .child(SharedString::from(format!(
                            "{} ← {}",
                            src.base_label, src.branch
                        ))),
                )
                .child(
                    div()
                        .text_color(theme::green(cx))
                        .child(SharedString::from(format!("+{}", data.additions))),
                )
                .child(
                    div()
                        .text_color(theme::red(cx))
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
                    add(&thread.root.author);
                    for reply in &thread.replies {
                        add(&reply.author);
                    }
                }
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ComposerPlacement {
    Diff,
    ConversationMain,
    ConversationThread(String),
    ConversationComment(String),
    PendingTray(u64),
}

/// The comment/reply composer. One at a time, targeting a specific
/// anchor of a specific item (it survives item switches but only renders —
/// and can only post — for the item it was opened on).
struct Composer {
    item_id: u64,
    placement: ComposerPlacement,
    /// Some(root comment id) = reply to that thread; None = a new anchored
    /// comment, or a main-thread comment for ConversationMain placement.
    reply_to: Option<u64>,
    commit_id: String,
    path: String,
    side: CommentSide,
    line: u64,
    start_line: Option<u64>,
    position: Option<github::CommentPosition>,
    pending: bool,
    edit_id: Option<u64>,
    edit_target: Option<CommentTarget>,
    /// Display row the composer is anchored beneath (best effort; goes stale
    /// harmlessly if rows rebuild while it is open).
    row_ix: usize,
    input: gpui::Entity<gpui::component::input::EditorState>,
    /// Space below the diff's last row, measured from the floating card.
    height: Rc<Cell<gpui::Pixels>>,
    /// Reveal a composer once, after its card is laid out.
    reveal: Rc<Cell<bool>>,
    error: Option<SharedString>,
    in_flight: bool,
}

impl Composer {
    /// Resolve against source line numbers so wrapping and row rebuilds don't
    /// move the highlight, and never carry it into another item or file.
    fn highlighted_side(&self, item_id: u64, data: &ItemData, row: &Row) -> Option<SelSide> {
        if self.placement != ComposerPlacement::Diff
            || self.item_id != item_id
            || data.diff.files.get(data.file_ix)?.display_path() != self.path
        {
            return None;
        }
        let range = self.start_line.unwrap_or(self.line)..=self.line;
        let (side, line) = match (rows::source_row(row), self.side) {
            (Row::Line { old_no, .. }, CommentSide::Left) => (SelSide::Unified, (*old_no)?),
            (Row::Line { new_no, .. }, CommentSide::Right) => (SelSide::Unified, (*new_no)?),
            (Row::SplitLine { left, .. }, CommentSide::Left) => (SelSide::Left, left.as_ref()?.no),
            (Row::SplitLine { right, .. }, CommentSide::Right) => {
                (SelSide::Right, right.as_ref()?.no)
            }
            _ => return None,
        };
        range.contains(&(line as u64)).then_some(side)
    }
}

/// The "submit review" modal: a verdict (approve / request changes /
/// comment) plus an optional body, targeting the item it was opened on.
struct ReviewDialog {
    item_id: u64,
    verdict: github::ReviewVerdict,
    input: gpui::Entity<gpui::component::input::EditorState>,
    error: Option<SharedString>,
    in_flight: bool,
}

fn local_review_prompt(src: &git::LocalSource, review: &LocalReview) -> String {
    let threads = assemble_threads(review.comments.clone())
        .iter()
        .map(format_local_thread_prompt)
        .collect::<Vec<_>>()
        .join("\n=====\n");
    format!(
        "Diff: {} ← {}. Locations are GitHub diff-style: path:Rline for right/new, path:Lline for left/old.\n\n{}",
        src.base_label, src.branch, threads
    )
}

fn format_local_thread_prompt(thread: &CommentThread) -> String {
    let file = if thread.root.path.is_empty() {
        "<unknown file>"
    } else {
        &thread.root.path
    };
    let mut sections = vec![
        local_comment_location(file, thread.root.side, thread.root.line),
        thread.root.body.clone(),
    ];
    for (ix, reply) in thread.replies.iter().enumerate() {
        let author = if reply.author.trim().is_empty() {
            "Unknown"
        } else {
            reply.author.as_str()
        };
        sections.push(format!("Reply {} ({author})", ix + 1));
        sections.push(reply.body.clone());
    }
    sections.join("\n")
}

fn local_comment_location(file: &str, side: CommentSide, line: Option<u64>) -> String {
    let line = line.unwrap_or(0);
    match side {
        CommentSide::Left => format!("{file}:L{line}"),
        CommentSide::Right => format!("{file}:R{line}"),
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
/// oldest review request first.
///
/// The queue is **cache-first**: the rows it paints come from the local
/// store when the app opens, and the network's answer hot-swaps in when
/// it lands. The rows on screen therefore carry their origin — cached or
/// fresh — which is what the status line says.
struct QueueState {
    /// The search the queue runs: review requests by default, or past reviews.
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
    /// Variable-height virtualization: titles and metadata wrap without clipping.
    scroll: ListState,
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
            scroll: ListState::new(0, ListAlignment::Top, px(200.)),
        }
    }

    /// Invalidate cached heights even when the refresh has the same row count:
    /// titles and labels may have changed. Preserve the viewport by row position.
    fn replace_rows(&mut self, mut prs: Vec<github::QueueItem>) {
        self.search.sort_items(&mut prs);
        if self.search.is_past_reviews() {
            prs.truncate(10);
        }
        let offset = self.scroll.logical_scroll_top();
        self.selected = move_selection(self.selected, prs.len(), 0);
        self.scroll.reset(prs.len());
        self.scroll.scroll_to(ListOffset {
            item_ix: offset.item_ix.min(prs.len().saturating_sub(1)),
            offset_in_item: offset.offset_in_item,
        });
        self.prs = prs;
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
    sidebar_width: f32,
    /// Pointer x and rendered width at the start of a sidebar resize.
    sidebar_resize: Option<(Pixels, f32)>,
    open_input: gpui::Entity<InputState>,
    open_error: Option<SharedString>,
    quick_open_visible: bool,
    /// Fuzzy filter over the active item's file tree (`/` focuses it).
    tree_filter_input: gpui::Entity<InputState>,
    focus_handle: FocusHandle,
    next_id: u64,
    /// Where the current selection drag started (side locked at mouse-down);
    /// None when no drag is in progress.
    drag_anchor: Option<SelectionDrag>,
    /// `m` toggles the minimap column for every item.
    minimap_visible: bool,
    /// A minimap scrub drag is in progress (mouse went down on the minimap).
    minimap_scrub: bool,
    /// Advance width of one monospace cell at (MONO, text_size()), measured once.
    char_width: Option<Pixels>,
    /// Diff row + split half under the pointer where a hover "+" (new
    /// comment) affordance shows; None when the pointer isn't on a
    /// commentable line. Refreshed after list prepaint as well as on mouse
    /// moves, so scrolling under a stationary pointer updates the target.
    hover_plus: Option<(usize, SelSide)>,
    composer: Option<Composer>,
    /// Bumped on every composer open/close; an in-flight post only reports
    /// back into the composer generation it was submitted from.
    composer_gen: u64,
    review: Option<ReviewDialog>,
    image_viewer: Option<image_viewer::ImageViewer>,
    video_hub: gpui::Entity<video::VideoHub>,
    /// Bumped on every review-dialog open/close, same protocol as
    /// `composer_gen`.
    review_gen: u64,
    sessions: HashMap<u64, SessionUi>,
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
        let tree_filter_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("filter files…"));
        let video_hub = cx.new(|_| video::VideoHub::default());
        let _subscriptions = vec![
            cx.observe(&video_hub, |_, _, cx| cx.notify()),
            cx.on_app_quit(|this, cx| {
                this.video_hub.update(cx, |hub, cx| hub.release(cx));
                this.persist_positions();
                async {}
            }),
            cx.on_release(|this, cx| {
                this.video_hub.update(cx, |hub, cx| hub.release(cx));
                this.persist_positions();
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
                |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::PressEnter { .. } if this.quick_open_visible => {
                        this.submit_open(window, cx);
                    }
                    InputEvent::Change => {
                        this.open_error = None;
                        cx.notify();
                    }
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
                            if let Some(tree) = &data.tree_view {
                                tree.state
                                    .read(cx)
                                    .scroll_handle()
                                    .scroll_to_item(0, ScrollStrategy::Top);
                            }
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
            sidebar_width: store
                .setting(SIDEBAR_WIDTH_SETTING)
                .and_then(|value| value.as_f64())
                .map(|width| width.clamp(MIN_SIDEBAR_WIDTH as f64, MAX_SIDEBAR_WIDTH as f64) as f32)
                .unwrap_or(DEFAULT_SIDEBAR_WIDTH),
            sidebar_resize: None,
            open_input,
            open_error: errors.first().cloned().map(SharedString::from),
            quick_open_visible: false,
            tree_filter_input,
            focus_handle: cx.focus_handle(),
            next_id: 0,
            drag_anchor: None,
            minimap_visible: true,
            minimap_scrub: false,
            char_width: None,
            hover_plus: None,
            composer: None,
            composer_gen: 0,
            review: None,
            image_viewer: None,
            video_hub,
            review_gen: 0,
            sessions: HashMap::new(),
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
        this.spawn_position_memory(cx);
        if this.open_error.is_some() {
            // The modal shell needs the window's Root to be installed first.
            cx.defer_in(window, |app, window, cx| app.open_quick_open(window, cx));
        }
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
        if !frontmost {
            self.persist_positions();
        }
        if frontmost {
            for id in self.items.iter().map(|item| item.id).collect::<Vec<_>>() {
                self.resume_session(id, cx);
            }
            // Coming back to the front is the moment the queue is most
            // likely to have moved on: refresh it, and let the poll run
            // again (the new poll ends the old one).
            self.refresh_queue_if_idle(cx);
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
                this.update(cx, |app, cx| app.refresh_queue_if_idle(cx))
                    .ok();
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
        self.queue.replace_rows(self.store.cached_queue_items());
        // The origin stays `None` for a cache that belongs to another
        // search: the status line must not claim "cached" for a queue it
        // doesn't match.
        self.queue.origin = if matches {
            QueueOrigin::Cached
        } else {
            QueueOrigin::None
        };
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

    /// Automatic refreshes must not invalidate a slow fetch before it can
    /// land. Explicit refreshes and search changes can still supersede it.
    fn refresh_queue_if_idle(&mut self, cx: &mut Context<Self>) {
        if !self.queue.loading {
            self.spawn_queue_refresh(cx);
        }
    }

    /// Run the queue search in the background and hot-swap the rows in.
    /// Failures are non-destructive: stale rows stay visible with the error
    /// surfaced beside them (see `render_queue`), and the next success
    /// clears it. The generation guard keeps a stale fetch from landing
    /// after a newer refresh started.
    ///
    /// A successful current-queue fetch also lands in the local store, so the
    /// next launch paints this queue before the network is asked at all.
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
                        app.queue.replace_rows(prs);
                        app.queue.origin = QueueOrigin::Fresh;
                        app.queue.error = None;
                        // The fresh queue becomes the cache: the next
                        // launch paints it before the network answers. The
                        // write goes to the background executor — a disk
                        // write must never sit on the UI thread.
                        let store = app.store.clone();
                        let search = app.queue.search.clone();
                        let prs = app.queue.prs.clone();
                        // History must not replace the default queue's launch cache.
                        if !search.is_past_reviews() {
                            cx.background_spawn(async move {
                                store.cache_queue(&search, &prs);
                            })
                            .detach();
                        }
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
        self.queue.scroll.scroll_to_reveal_item(self.queue.selected);
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
    /// view — its own tab, fresh (including full-file loading) or straight
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
        cx.notify();
    }

    /// `u`: back to the queue. The open item stays open — the sidebar and
    /// ctrl-tab come right back to it — and the queue keeps the
    /// selection and scroll exactly where they were left.
    fn back_to_queue(&mut self, cx: &mut Context<Self>) {
        if self.show_queue {
            return;
        }
        self.persist_positions();
        self.show_queue = true;
        cx.notify();
    }

    /// Leave the queue without opening or closing anything. The active item
    /// keeps its pane and review position, just as it was before visiting home.
    fn return_to_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.show_queue && !self.items.is_empty() {
            self.activate(self.active, window, cx);
        }
    }

    /// A different view must never display the old view's rows, even while
    /// loading or offline. Keep filters and the default queue's sort preference.
    fn queue_toggle_past_reviews(&mut self, cx: &mut Context<Self>) {
        if !self.show_queue {
            return;
        }
        let past = !self.queue.search.is_past_reviews();
        self.queue.search = self.queue.search.clone().with_past_reviews(past);
        self.queue.replace_rows(Vec::new());
        self.queue.origin = QueueOrigin::None;
        self.queue.error = None;
        if !past {
            let cached = self.store.cached_queue();
            let (sort, order) = self.queue.search.sort();
            if cached.matches(&self.queue.search.query(), sort, order) {
                self.queue
                    .replace_rows(cached.prs.iter().map(local_store::queue_item).collect());
                self.queue.origin = QueueOrigin::Cached;
            }
        }
        self.queue_run_search(cx);
    }

    /// `s` and the sort toggle: flip between review-request time and creation
    /// time, both oldest-first. Reorder existing rows immediately (even offline),
    /// then refresh. The selection starts over: a position in one order means nothing in the
    /// other.
    fn queue_toggle_sort(&mut self, cx: &mut Context<Self>) {
        if !self.show_queue || self.queue.search.is_past_reviews() {
            return;
        }
        let flipped = match self.queue.search.ordering() {
            github::QueueSort::ReviewRequestedAscending => github::QueueSort::CreatedAscending,
            github::QueueSort::CreatedAscending => github::QueueSort::ReviewRequestedAscending,
        };
        self.queue.search = self.queue.search.clone().sorted_by(flipped);
        let prs = std::mem::take(&mut self.queue.prs);
        self.queue.replace_rows(prs);
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
            github::QueueSort::ReviewRequestedAscending => "review_requested_ascending",
            github::QueueSort::CreatedAscending => "created_ascending",
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
        self.queue.scroll.scroll_to(ListOffset::default());
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
        let code_width = match data.mode {
            ViewMode::Unified => list_width - UNIFIED_GUTTER,
            ViewMode::Split => (list_width - SPLIT_DIVIDER) / 2. - SPLIT_GUTTER,
        };
        let code_wrap = ((code_width / char_width).floor() as usize).max(1);
        let code_changed =
            code_wrap != data.code_wrap && data.preferences.soft_wrap.unwrap_or(false);
        data.code_wrap = code_wrap;
        if wrap == data.comment_wrap && !code_changed {
            return;
        }
        data.comment_wrap = wrap;
        if code_changed {
            data.rebuild_presentation();
        } else if has_threads {
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
            data.rescale_position_restore(new_rh / old_rh);
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

    /// Word selection targets the glyph under the pointer (floor), whereas
    /// character drags target the nearest caret (round). Ignore gutters and the
    /// blank tail rather than selecting a word on the clamped first/last row.
    fn pane_word_selection(
        &self,
        position: Point<Pixels>,
        char_width: Pixels,
    ) -> Option<Selection> {
        let data = self.active_data()?;
        let scroll = data.scroll.0.borrow();
        let bounds = scroll.base_handle.bounds();
        let y = f32::from(position.y - bounds.top() - scroll.base_handle.offset().y);
        if !bounds.contains(&position) || y < 0. || y >= data.rows.len() as f32 * row_height() {
            return None;
        }
        let (side, row, text_x) = self.pane_text_hit(position, None)?;
        if text_x < px(0.) {
            return None;
        }
        if data.mode == ViewMode::Split {
            // Split gutters stay fixed while the code scrolls horizontally.
            let half = (bounds.size.width - px(SPLIT_DIVIDER)) / 2.;
            let cell_x = position.x
                - bounds.left()
                - if side == SelSide::Right {
                    half + px(SPLIT_DIVIDER)
                } else {
                    px(0.)
                };
            if cell_x < px(SPLIT_GUTTER) || cell_x >= half {
                return None;
            }
        }
        let col = (f32::from(text_x) / f32::from(char_width)).floor() as usize;
        word_selection(&data.rows, side, RowCol { row, col })
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
            // offset.x is negative when scrolled right in either mode.
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
                (side, cell_x - SPLIT_GUTTER - f32::from(offset.x))
            }
        };
        Some((side, row, px(text_x)))
    }

    fn open_item(&mut self, source: Source, cx: &mut Context<Self>) {
        self.persist_positions();
        // The session keeps one item per source: opening something it
        // already has brings that item forward — scroll, cursor, view mode
        // and comments exactly where they were left — instead of a second
        // copy that would be a way to lose your place in the first. Every
        // open path (queue Enter, ⌘O, the CLI) shares this.
        if let Some(ix) = self
            .items
            .iter()
            .position(|item| sources_match(&item.source, &source))
        {
            self.active = ix;
            // Opening something leaves the home screen for the item's pane.
            self.show_queue = false;
            self.resume_session(self.items[ix].id, cx);
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
            pane: match &source {
                Source::Pr(_) => ItemPane::Conversation,
                Source::Local(_) => ItemPane::Changes,
            },
        });
        if let Source::Pr(loc) = &source {
            self.sessions.insert(
                id,
                SessionUi {
                    state: self.store.review_session(loc),
                    ..Default::default()
                },
            );
            self.resume_session(id, cx);
        }
        self.active = self.items.len() - 1;
        // Opening something leaves the home screen for the item's pane.
        self.show_queue = false;
        self.spawn_fetch(id, source, ViewMode::Split, cx);
        cx.notify();
    }

    fn spawn_fetch(&mut self, id: u64, source: Source, mode: ViewMode, cx: &mut Context<Self>) {
        let since_review = self.items.iter().find(|item| item.id == id).is_some_and(
            |item| matches!(&item.state, ItemState::Ready(data) if data.review_base.is_some()),
        );
        self.spawn_fetch_scoped(id, source, mode, since_review, cx);
    }

    fn spawn_fetch_scoped(
        &mut self,
        id: u64,
        source: Source,
        mode: ViewMode,
        since_review: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = self.items.iter_mut().find(|item| item.id == id) else {
            return;
        };
        item.reloading = true;
        // Supersede any older fetch before starting its replacement.
        item.upgrade_gen += 1;
        let fetch_gen = item.upgrade_gen;
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
            let (conversation_tx, conversation_rx) = futures::channel::oneshot::channel();
            let fetch = cx.background_spawn(async move {
                review_target::fetch_item_with_conversation(
                    github, &source, mode, file_ix, since_review, Some(conversation_tx),
                )
            });
            if let Ok(conversation) = conversation_rx.await {
                this.update(cx, |app, cx| {
                    let Some(item) = app.items.iter_mut().find(|item| item.id == id) else {
                        return;
                    };
                    if item.upgrade_gen != fetch_gen || matches!(item.state, ItemState::Ready(_)) {
                        return;
                    }
                    item.install_conversation(conversation, mode);
                    if let (Source::Pr(loc), ItemState::Ready(data)) = (&item.source, &mut item.state) {
                        data.load_preferences(&app.store, Some(loc));
                    }
                    app.sync_pending_comments(id);
                    cx.notify();
                }).ok();
            }
            let fetched = fetch.await;
            this.update(cx, |app, cx| {
                let Some(item) = app.items.iter_mut().find(|item| item.id == id) else {
                    return;
                };
                if item.upgrade_gen != fetch_gen {
                    return;
                }
                item.reloading = false;
                match fetched {
                    Ok(loaded) => {
                        // An offline metadata-only copy has no viewport and
                        // must not replace the last real diff anchor.
                        let needs_restore = !matches!(&item.state,
                            ItemState::Ready(data) if !data.diff.files.is_empty() || data.review_base.is_some());
                        let position = if needs_restore {
                            match &item.source {
                                Source::Pr(loc) => app.store.position(loc),
                                Source::Local(_) => None,
                            }
                        } else {
                            None
                        };
                        let snapshot = item.install(loaded);
                        if let ItemState::Ready(data) = &mut item.state {
                            let loc = match &item.source {
                                Source::Pr(loc) => Some(loc),
                                _ => None,
                            };
                            data.load_preferences(&app.store, loc);
                        }
                        if let (Some(position), ItemState::Ready(data)) =
                            (position, &mut item.state)
                        {
                            data.saved_position = Some(position.clone());
                            data.restore_review_position(position, true);
                        }
                        // The PR's snapshot lands in the local store: the
                        // next launch knows what the app last saw of it
                        // (its head SHA, its threads), and an offline
                        // launch can still read it.
                        if let Some(snapshot) = snapshot {
                            app.store.save_snapshot(&snapshot);
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
                app.sync_pending_comments(id);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix < self.items.len() {
            self.persist_positions();
            self.active = ix;
            self.show_queue = false;
            window.focus(&self.focus_handle, cx);
            cx.notify();
        }
    }

    fn close_item(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.items.len() {
            return;
        }
        self.persist_positions();
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
            self.persist_positions();
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
        if self.sessions.get(&item.id).is_some_and(|ui| ui.busy)
            || matches!(&item.state, ItemState::Ready(data) if !data.viewed.pending.is_empty())
        {
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
        self.resume_session(id, cx);
        self.spawn_fetch(id, source, mode, cx);
        cx.notify();
    }

    fn toggle_view(&mut self, cx: &mut Context<Self>) {
        let Some(data) = self.active_data_mut() else {
            return;
        };
        // The keyboard cursor can be far from the viewport after scrolling.
        // Anchor to source coordinates: split/unified rows have different indices.
        let position = data.review_position();
        data.selection = None;
        data.cursor_side = None;
        data.mode = match data.mode {
            ViewMode::Unified => ViewMode::Split,
            ViewMode::Split => ViewMode::Unified,
        };
        data.set_rows(data.row_build());
        if let Some(mut position) = position {
            position.split = data.mode == ViewMode::Split;
            data.restore_review_position(position, true);
        }
        cx.notify();
    }

    fn jump(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(data) = self.active_data_mut() {
            data.position_restore = None;
            data.cursor = ix;
            data.cursor_side = None;
            data.scroll.scroll_to_item_strict(ix, ScrollStrategy::Top);
            data.remember_jump();
        }
        cx.notify();
    }

    /// `left`/`right`: nudge the diff's horizontal scroll by ten columns.
    /// Both modes scroll unless soft wrap is enabled. The paint-time clamp pins x to
    /// [-max_offset, 0], so overshooting the ends is harmless.
    fn scroll_horizontal(&mut self, direction: i32, window: &mut Window, cx: &mut Context<Self>) {
        let step = f32::from(self.char_width(window)) * 10.;
        let Some(data) = self.active_data_mut() else {
            return;
        };
        if data.preferences.soft_wrap.unwrap_or(false) {
            return;
        }
        let (offset, max_x) = {
            let state = data.scroll.0.borrow();
            (state.base_handle.offset(), state.base_handle.max_offset().x)
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
    /// resuming its viewport, or revealing its first change on first visit.
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

    /// Browser-style history is local to the active review's Changes pane.
    fn navigate_file_history(
        &mut self,
        direction: NavigationDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.show_queue
            || !self
                .active_item()
                .is_some_and(|item| item.pane == ItemPane::Changes)
        {
            return;
        }
        let Some(data) = self.active_data_mut().filter(|data| data.diff_loaded) else {
            return;
        };
        let Some(current) = data.diff.files.get(data.file_ix) else {
            return;
        };
        let Some(path) = data.file_history.step(direction, current.display_path()) else {
            return;
        };
        if let Some(file_ix) = data
            .diff
            .files
            .iter()
            .position(|file| file.display_path() == path)
        {
            data.show_file(file_ix, None);
            window.focus(&self.focus_handle, cx);
            cx.notify();
        }
    }

    /// Switch to changes, resume `file_ix` (first change on first visit),
    /// and focus the diff pane — what clicking a sidebar file does.
    pub(crate) fn jump_to_file(
        &mut self,
        file_ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.flip_pane(ItemPane::Changes, cx);
        window.focus(&self.focus_handle, cx);
        if let Some(data) = self.active_data_mut() {
            data.open_file(file_ix, None);
        }
        cx.notify();
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
        let pane_h = self.active_data().map_or(0., |data| {
            f32::from(data.scroll.0.borrow().base_handle.bounds().size.height)
        });
        let tail_rows = self.diff_tail_rows(pane_h);
        let Some(data) = self.active_data_mut() else {
            return;
        };
        data.position_restore = None;
        data.scroll.0.borrow_mut().deferred_scroll_to_item = None;
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
        let total = total + tail_rows;
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

    /// `C`: open the composer below the selected range's last row (the side
    /// it runs through decides the anchor half), else the keyboard cursor's row —
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
            // Validate from the first line, but place the card below the last
            // selected row so it doesn't cover the rest of the range.
            Some(sel) => {
                let (start, end) = sel.ordered();
                (end.row, comment_anchor(&data.rows, start.row, sel.side))
            }
            // No selection: the keyboard cursor names the line; a split
            // row's side falls out of its shape.
            None => (
                data.cursor,
                match data.cursor_side {
                    Some(side) => comment_anchor(&data.rows, data.cursor, side),
                    None => cursor_anchor(&data.rows, data.cursor),
                },
            ),
        };
        let Some((anchor_side, line)) = anchor else {
            if let Some(item) = self.items.get_mut(self.active) {
                item.refresh_error = Some("select a source line, not a header or comment".into());
                cx.notify();
            }
            return;
        };
        let path = data.diff.files[data.file_ix].display_path().to_string();
        let range = if matches!(item.source, Source::Pr(_)) {
            let points = match data.selection {
                Some(sel) => {
                    let (start, end) = sel.ordered();
                    if comment_anchor(&data.rows, end.row, sel.side).is_none() {
                        if let Some(item) = self.items.get_mut(self.active) {
                            item.refresh_error = Some("end the range on a source line".into());
                            cx.notify();
                        }
                        return;
                    }
                    (start.row..=end.row)
                        .filter_map(|row| comment_anchor(&data.rows, row, sel.side))
                        .collect::<Vec<_>>()
                }
                None => vec![(anchor_side, line)],
            };
            match data.selection_anchor(&points) {
                Ok(anchor) => Some(anchor),
                Err(error) => {
                    self.items[self.active].refresh_error = Some(error.into());
                    cx.notify();
                    return;
                }
            }
        } else {
            None
        };
        self.open_composer(None, path, anchor_side, line, row_ix, window, cx);
        if let (Some(composer), Some(anchor)) = (&mut self.composer, range) {
            composer.line = anchor.line;
            composer.start_line = anchor.start_line;
            composer.position = anchor.position;
        }
    }

    fn extend_comment_selection(
        &mut self,
        side: SelSide,
        head: RowCol,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(data) = self.active_data_mut() else {
            return;
        };
        if data.cursor_side.is_some_and(|origin| origin != side) {
            self.items[self.active].refresh_error =
                Some("a comment range must stay on the same side".into());
            cx.notify();
            return;
        }
        let anchor = data.selection.map(|sel| sel.anchor).unwrap_or(RowCol {
            row: data.cursor,
            col: 0,
        });
        data.selection = Some(Selection { side, anchor, head });
        self.drag_anchor = None;
        self.open_composer_on_selection(window, cx);
        // The selection only supplies the comment range; Shift+click should
        // not leave ordinary diff text selected behind the composer.
        if let Some(data) = self.active_data_mut() {
            data.selection = None;
        }
        cx.notify();
    }

    /// Serialize per-PR operations, not dialog generations: a closed dialog
    /// must not lose the result of a write that has already reached GitHub.
    fn session_job(
        &mut self,
        item_id: u64,
        command: SessionCommand,
        cx: &mut Context<Self>,
    ) -> Option<Task<(ReviewSession, Result<Option<u64>, github::ClientError>)>> {
        let item = self.items.iter().find(|item| item.id == item_id)?;
        let Source::Pr(loc) = &item.source else {
            return None;
        };
        let loc = loc.clone();
        let ui = self.sessions.entry(item_id).or_default();
        if ui.busy {
            return None;
        }
        ui.busy = true;
        ui.error = None;
        let mut session = ui.state.clone();
        let github = self.github.clone();
        let store = self.store.clone();
        cx.notify();
        Some(cx.background_spawn(async move {
            let result = session.execute_with_receipt(github.as_ref(), &loc, command);
            // Also mirror partial success (e.g. start succeeded, add failed).
            store.save_review_session(&loc, &session);
            (session, result)
        }))
    }

    fn finish_session(
        &mut self,
        item_id: u64,
        state: ReviewSession,
        result: &Result<(), github::ClientError>,
        cx: &mut Context<Self>,
    ) {
        if self.items.iter().any(|item| item.id == item_id) {
            let ui = self.sessions.entry(item_id).or_default();
            // Reconciliation must not collapse a tray the user has expanded.
            // Once the review ends, the next review starts collapsed again.
            ui.comments_expanded &= state.pending.is_some();
            ui.state = state;
            ui.busy = false;
            ui.error = result.as_ref().err().map(|error| error.to_string().into());
        } else {
            self.sessions.remove(&item_id);
        }
        self.sync_pending_comments(item_id);
        cx.notify();
    }

    /// Keep the diff and tray on the same session snapshot, including cached
    /// drafts and operations that finish after their composer was closed.
    fn sync_pending_comments(&mut self, item_id: u64) {
        let Some(ui) = self.sessions.get(&item_id) else {
            return;
        };
        let Some(item) = self.items.iter_mut().find(|item| item.id == item_id) else {
            return;
        };
        let ItemState::Ready(data) = &mut item.state else {
            return;
        };
        data.comments
            .get_or_insert_with(CommentIndex::default)
            .replace_pending(ui.state.pending.as_ref());
        data.filter_scope_comments();
        data.rebuild_rows_anchored();
    }

    fn resume_session(&mut self, item_id: u64, cx: &mut Context<Self>) {
        let Some(job) = self.session_job(item_id, SessionCommand::Resume, cx) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let (session, result) = job.await;
            let result = result.map(|_| ());
            this.update(cx, |app, cx| {
                app.finish_session(item_id, session, &result, cx)
            })
            .ok();
        })
        .detach();
    }

    fn run_session(
        &mut self,
        item_id: u64,
        command: SessionCommand,
        completion: SessionCompletion,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let submitted = matches!(command, SessionCommand::Submit { .. });
        let replied = matches!(
            command,
            SessionCommand::Reply { .. } | SessionCommand::ConversationReply { .. }
        );
        let commented = matches!(command, SessionCommand::Conversation { .. });
        let mutation = command.clone();
        let mutated = matches!(
            command,
            SessionCommand::EditPublished { .. }
                | SessionCommand::DeletePublished { .. }
                | SessionCommand::SetReaction { .. }
                | SessionCommand::SetThreadResolved { .. }
        );
        let Some(job) = self.session_job(item_id, command, cx) else {
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            let (session, result) = job.await;
            let receipt = result.as_ref().ok().copied().flatten();
            let result = result.map(|_| ());
            this.update_in(cx, |app, window, cx| {
                app.finish_session(item_id, session, &result, cx);
                if result.is_ok() {
                    app.apply_comment_mutation(item_id, &mutation);
                    app.apply_reaction(item_id, &mutation);
                    app.apply_thread_resolution(item_id, &mutation);
                }
                if let SessionCompletion::Composer(gen) = completion {
                    if let Some(item) = app.items.iter_mut().find(|item| item.id == item_id) {
                        if let ItemState::Ready(data) = &mut item.state {
                            data.posts.complete(gen, receipt);
                            if let Some(discussion) = &data.discussion {
                                data.posts.reconcile(discussion);
                            }
                        }
                    }
                }
                match completion {
                    SessionCompletion::Composer(gen) if app.composer_gen == gen => match &result {
                        Ok(()) => app.close_composer(window, cx),
                        Err(error) => {
                            if let Some(composer) = &mut app.composer {
                                composer.in_flight = false;
                                composer.error = Some(error.to_string().into());
                            }
                        }
                    },
                    SessionCompletion::Review(gen) if app.review_gen == gen => match &result {
                        Ok(()) => app.close_review(window, cx),
                        Err(error) => {
                            if let Some(review) = &mut app.review {
                                review.in_flight = false;
                                review.error = Some(error.to_string().into());
                            }
                        }
                    },
                    _ => {}
                }
                if ((submitted || replied || commented || mutated) && result.is_ok())
                    || matches!(mutation, SessionCommand::SetThreadResolved { .. })
                {
                    if let Some(Source::Pr(loc)) = app
                        .items
                        .iter()
                        .find(|item| item.id == item_id)
                        .map(|item| item.source.clone())
                    {
                        if submitted {
                            app.refetch_meta(item_id, loc, cx);
                        } else {
                            app.refetch_comments(item_id, loc, cx);
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
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
        self.open_comment_composer(
            ComposerPlacement::Diff,
            reply_to,
            path,
            side,
            line,
            row_ix,
            window,
            cx,
        );
    }

    fn open_comment_composer(
        &mut self,
        placement: ComposerPlacement,
        reply_to: Option<u64>,
        path: String,
        side: CommentSide,
        line: u64,
        row_ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.image_viewer.is_some() {
            return;
        }
        let Some(item) = self.items.get(self.active) else {
            return;
        };
        if placement == ComposerPlacement::Diff
            && (item.reloading || self.active_data().is_some_and(ItemData::shows_file_media))
        {
            return;
        }
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
        let mut position = None;
        let mut line = line;
        if reply_to.is_none() && placement != ComposerPlacement::ConversationMain {
            match &item.source {
                Source::Pr(_) if commit_id.is_empty() => return,
                Source::Pr(_) | Source::Local(_) => {}
            }
            if matches!(item.source, Source::Pr(_)) {
                let Some(data) = self.active_data() else {
                    return;
                };
                if data.diff.files.get(data.file_ix).is_none() {
                    return;
                }
                match data.selection_anchor(&[(side, line)]) {
                    Ok(anchor) => {
                        position = anchor.position;
                        line = anchor.line;
                    }
                    Err(error) => {
                        self.items[self.active].refresh_error = Some(error.into());
                        cx.notify();
                        return;
                    }
                }
            }
        }
        if self.composer.is_some() {
            self.close_composer(window, cx);
        }
        self.composer_gen += 1;
        let input = cx.new(|cx| comment_editor::new(window, cx).placeholder("leave a comment…"));
        input.update(cx, |state, cx| state.focus(window, cx));
        // Wire up @-mention autocomplete for PR items: seed the pool with known
        // participants now, attach the provider, then fetch the full list once.
        if let Some(loc) = pr_loc {
            if let Some(data) = self.active_data() {
                let mentions = data.mentions.clone();
                seed_mentions(&mentions, data);
                comment_editor::candidates(&input, mentions, cx);
            }
            self.ensure_mentions(item_id, loc, cx);
        }
        self.composer = Some(Composer {
            item_id,
            placement,
            reply_to,
            commit_id,
            path,
            side,
            line,
            start_line: None,
            position,
            pending: false,
            edit_id: None,
            edit_target: None,
            row_ix,
            input,
            height: Rc::new(Cell::new(px(250.))),
            reveal: Rc::new(Cell::new(true)),
            error: None,
            in_flight: false,
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
        if let Some(composer) = self.composer.take() {
            if let Some(ItemState::Ready(data)) = self
                .items
                .iter_mut()
                .find(|item| item.id == composer.item_id)
                .map(|item| &mut item.state)
            {
                if data.editing_comment.take().is_some() {
                    data.rebuild_rows_anchored();
                }
            }
            self.composer_gen += 1;
            window.focus(&self.focus_handle, cx);
            cx.notify();
        }
    }

    fn composer_uses_pending_review(&self, composer: &Composer) -> bool {
        if matches!(
            composer.edit_target,
            Some(CommentTarget::Published(_)) | Some(CommentTarget::Local(_))
        ) {
            return false;
        }
        composer.pending
            || self
                .sessions
                .get(&composer.item_id)
                .is_some_and(|ui| ui.state.pending.is_some())
    }

    fn composer_submit_label(&self, composer: &Composer) -> &'static str {
        if composer.edit_id.is_some() {
            if matches!(composer.edit_target, Some(CommentTarget::Published(_))) {
                "Save changes"
            } else {
                "Save draft"
            }
        } else if self.composer_uses_pending_review(composer) {
            "Add to review"
        } else if composer.reply_to.is_some() {
            "Reply"
        } else if composer.placement == ComposerPlacement::ConversationMain {
            "Comment"
        } else {
            "Add single comment"
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
        if self.active_item().map(|item| item.id) != Some(item_id) {
            return;
        }
        let gen = self.composer_gen;
        // Keyboard submission must obey the same per-PR serialization as the
        // disabled button, including while pending-review reconciliation runs.
        if matches!(source, Source::Pr(_)) && self.sessions.get(&item_id).is_some_and(|ui| ui.busy)
        {
            return;
        }
        if let Some(target) = composer.edit_target.clone() {
            match target {
                CommentTarget::Local(id) => {
                    if let Some(data) = self.active_data_mut() {
                        if let Some(local) = &mut data.local_review {
                            if let Some(comment) = local.comments.iter_mut().find(|c| c.id == id) {
                                comment.body = body;
                            }
                            data.comments = Some(local.index());
                            data.rebuild_rows_anchored();
                        }
                    }
                    self.close_composer(window, cx);
                    return;
                }
                CommentTarget::Pending(id) => {
                    let composer = self.composer.as_mut().unwrap();
                    composer.in_flight = true;
                    composer.error = None;
                    self.run_session(
                        item_id,
                        SessionCommand::Edit { id, body },
                        SessionCompletion::Composer(gen),
                        window,
                        cx,
                    );
                    return;
                }
                CommentTarget::Published(target) => {
                    let composer = self.composer.as_mut().unwrap();
                    composer.in_flight = true;
                    composer.error = None;
                    self.run_session(
                        item_id,
                        SessionCommand::EditPublished { target, body },
                        SessionCompletion::Composer(gen),
                        window,
                        cx,
                    );
                    return;
                }
            }
        }
        if matches!(source, Source::Pr(_)) && composer.placement != ComposerPlacement::Diff {
            let pending = self.composer_uses_pending_review(composer);
            let reply_to = composer.reply_to;
            let placement = composer.placement.clone();
            let optimistic_body = body.clone();
            let command = if let Some(comment_id) = composer.reply_to {
                SessionCommand::ConversationReply {
                    comment_id,
                    commit_id: composer.commit_id.clone(),
                    body,
                    pending: composer.pending,
                }
            } else if pending {
                SessionCommand::AddSummary {
                    commit_id: composer.commit_id.clone(),
                    body,
                }
            } else {
                SessionCommand::Conversation { body }
            };
            let composer = self.composer.as_mut().unwrap();
            composer.in_flight = true;
            composer.error = None;
            if !pending {
                let author = match &self.account {
                    AccountStatus::SignedIn(viewer) => viewer.login.clone(),
                    _ => "you".into(),
                };
                if let Some(ItemState::Ready(data)) = self
                    .items
                    .iter_mut()
                    .find(|item| item.id == item_id)
                    .map(|item| &mut item.state)
                {
                    data.posts.begin(gen, reply_to, author, optimistic_body);
                    if let ComposerPlacement::ConversationThread(id) = placement {
                        data.expanded_threads.insert(id);
                    }
                }
            }
            self.run_session(
                item_id,
                command,
                SessionCompletion::Composer(gen),
                window,
                cx,
            );
            return;
        }
        let anchor = github::CommentAnchor {
            commit_id: composer.commit_id.clone(),
            path: composer.path.clone(),
            side: composer.side,
            line: composer.line,
            start_line: composer.start_line,
            position: composer.position.clone(),
        };
        if matches!(source, Source::Pr(_))
            && (self.composer_uses_pending_review(composer)
                || composer.edit_id.is_some()
                || composer.reply_to.is_some())
        {
            let command = match (composer.edit_id, composer.reply_to) {
                (Some(id), _) => SessionCommand::Edit { id, body },
                (_, Some(comment_id)) => SessionCommand::Reply { comment_id, body },
                _ => SessionCommand::Add { anchor, body },
            };
            let composer = self.composer.as_mut().unwrap();
            composer.in_flight = true;
            composer.error = None;
            self.run_session(
                item_id,
                command,
                SessionCompletion::Composer(gen),
                window,
                cx,
            );
            return;
        }
        let (reply_to, path, side, line) = {
            let composer = self.composer.as_mut().unwrap();
            composer.in_flight = true;
            composer.error = None;
            (
                composer.reply_to,
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
                    ReviewSession::default().execute(
                        github.as_ref(),
                        &post_loc,
                        SessionCommand::Immediate { anchor, body },
                    )
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

    /// Refetch the published discussion (not meta/patch), regroup, and
    /// rebuild the rows with the viewport anchored — the counterpart of a
    /// full refresh for the post-comment path.
    fn refetch_comments(&mut self, item_id: u64, loc: github::PrLocator, cx: &mut Context<Self>) {
        let generation = self
            .items
            .iter()
            .find(|item| item.id == item_id)
            .and_then(|item| match &item.state {
                ItemState::Ready(data) => Some(data.discussion_generation),
                _ => None,
            });
        let github = self.github.clone();
        cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_spawn(async move { github.pr_discussion(&loc) })
                .await;
            this.update(cx, |app, cx| {
                let Some(item) = app.items.iter_mut().find(|item| item.id == item_id) else {
                    return;
                };
                let ItemState::Ready(data) = &mut item.state else {
                    return;
                };
                if generation != Some(data.discussion_generation) {
                    return;
                }
                match fetched {
                    Ok(discussion) => {
                        data.posts.reconcile(&discussion);
                        if item
                            .refresh_error
                            .as_ref()
                            .is_some_and(|error| error.starts_with("comment refresh failed:"))
                        {
                            item.refresh_error = None;
                        }
                        if let (Source::Pr(loc), Some(meta)) = (&item.source, &data.pr_meta) {
                            if let Ok(snapshot) = review_target::discussion_snapshot(
                                loc,
                                meta,
                                &discussion,
                                data.commits.as_ref(),
                            ) {
                                app.store.save_snapshot(&snapshot);
                            }
                        }
                        data.comments = Some(group_comments(
                            discussion
                                .review_comments()
                                .into_iter()
                                .map(ReviewComment::from)
                                .collect(),
                        ));
                        data.discussion = Some(discussion);
                        if let Some(ui) = app.sessions.get(&item_id) {
                            data.comments
                                .as_mut()
                                .unwrap()
                                .replace_pending(ui.state.pending.as_ref());
                        }
                        data.filter_scope_comments();
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
        // Do not stack modal interfaces.
        if self.review.is_some() || self.image_viewer.is_some() {
            return;
        }
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
            comment_editor::new(window, cx)
                .placeholder("review summary… (optional with pending comments)")
        });
        if let Some(pending) = self
            .sessions
            .get(&item_id)
            .and_then(|ui| ui.state.pending.as_ref())
        {
            input.update(cx, |state, cx| {
                state.set_value(pending.body.clone(), window, cx)
            });
        }
        if let Some(data) = self.active_data() {
            seed_mentions(&data.mentions, data);
            comment_editor::candidates(&input, data.mentions.clone(), cx);
        }
        if let Some(Source::Pr(loc)) = self
            .items
            .iter()
            .find(|item| item.id == item_id)
            .map(|item| item.source.clone())
        {
            self.ensure_mentions(item_id, loc, cx);
        }
        self.review = Some(ReviewDialog {
            item_id,
            verdict: github::ReviewVerdict::Approve,
            input,
            error: None,
            in_flight: false,
        });
        let generation = self.review_gen;
        let owner = cx.entity();
        let initial_focus = Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            let (content, input, busy) = owner.update(cx, |app, cx| {
                let input = app
                    .review
                    .as_ref()
                    .filter(|_| app.review_gen == generation)
                    .map(|review| review.input.clone());
                let busy = app.review.as_ref().is_some_and(|review| review.in_flight);
                (app.render_review(cx), input, busy)
            });
            if initial_focus.replace(false) {
                if let Some(input) = input {
                    input.update(cx, |state, cx| state.focus(window, cx));
                }
            }
            let closed = owner.clone();
            let cancelling = owner.clone();
            dialog
                .title("Finish your review")
                .w(px(560.))
                .overlay_closable(!busy)
                .close_button(!busy)
                .keyboard(!busy)
                .on_cancel(move |_, _, cx| {
                    !cancelling
                        .read(cx)
                        .review
                        .as_ref()
                        .is_some_and(|review| review.in_flight)
                })
                .on_close(move |_, _, cx| {
                    closed.update(cx, |app, cx| {
                        app.cleanup_review(generation, cx);
                    });
                })
                .child(content)
        });
        cx.notify();
    }

    /// Toolkit user dismissal and imperative close do not share callbacks.
    /// Cleanup is generation-owned and never requests another toolkit close.
    fn cleanup_review(&mut self, generation: u64, cx: &mut Context<Self>) -> bool {
        if self.review_gen != generation || self.review.take().is_none() {
            return false;
        }
        self.review_gen += 1;
        cx.notify();
        true
    }

    fn close_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.cleanup_review(self.review_gen, cx) {
            // Kit restores the pre-dialog focus; don't force the diff's handle.
            window.close_dialog(cx);
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
        let item_id = review.item_id;
        if self.sessions.get(&item_id).is_some_and(|ui| ui.busy) {
            return;
        }
        let Some(item) = self.items.iter().find(|item| item.id == review.item_id) else {
            return;
        };
        let Source::Pr(_) = &item.source else {
            return;
        };
        let item_id = item.id;
        let gen = self.review_gen;
        if let Some(review) = &mut self.review {
            review.in_flight = true;
            review.error = None;
        }
        self.run_session(
            item_id,
            SessionCommand::Submit { verdict, body },
            SessionCompletion::Review(gen),
            window,
            cx,
        );
    }

    /// A submitted review changes the comparison baseline as well as the
    /// titlebar. Reload them together; never pair a new head with an old diff.
    fn refetch_meta(&mut self, item_id: u64, loc: github::PrLocator, cx: &mut Context<Self>) {
        let Some(item) = self.items.iter().find(|item| item.id == item_id) else {
            return;
        };
        let ItemState::Ready(data) = &item.state else {
            return;
        };
        let mode = data.mode;
        self.spawn_fetch(item_id, Source::Pr(loc), mode, cx);
    }

    /// The hover "+" affordance: a small blue box at the far left of the
    /// hovered line (its half, in split mode), absolutely positioned over the
    /// list like the minimap viewport. Clicking opens the composer for that
    /// row's anchor.
    fn render_plus(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let (row_ix, side) = self.hover_plus?;
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
                .debug_selector(|| "comment-plus".to_string())
                .absolute()
                .left(px(x))
                .top(px(y + (row_height() - 16.) / 2.))
                .w(px(16.))
                .h(px(16.))
                .rounded_sm()
                .bg(theme::blue(cx))
                .flex()
                .items_center()
                .justify_center()
                .text_color(gpui::white())
                .text_size(px(13.))
                .cursor_pointer()
                .child(SharedString::from("+"))
                .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                    cx.stop_propagation();
                    let path = path.clone();
                    entity.update(cx, |this, cx| {
                        if event.modifiers.shift {
                            this.extend_comment_selection(
                                side,
                                RowCol {
                                    row: row_ix,
                                    col: 0,
                                },
                                window,
                                cx,
                            );
                        } else {
                            this.open_composer(None, path, anchor_side, line, row_ix, window, cx);
                        }
                    });
                })
                .into_any_element(),
        )
    }

    /// The floating composer card: absolutely positioned at root level (so
    /// its input sits outside the "ReviewApp" key context and plain letters
    /// stay text), anchored below the target row. Reveal it by scrolling the
    /// diff, never by moving the card upward over the selected source lines.
    fn render_composer(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let empty = || div().into_any_element();
        let Some(composer) = &self.composer else {
            return empty();
        };
        if composer.placement != ComposerPlacement::Diff
            || self.show_queue
            || self.active_data().is_some_and(ItemData::shows_file_media)
        {
            return empty();
        }
        // Only render for the item it was opened on, and only while active.
        if self.items.get(self.active).map(|item| item.id) != Some(composer.item_id) {
            return empty();
        }
        let Some(data) = self.active_data() else {
            return empty();
        };
        if self
            .active_item()
            .is_none_or(|item| item.pane != ItemPane::Changes)
        {
            return empty();
        }
        let (bounds, offset) = {
            let state = data.scroll.0.borrow();
            (state.base_handle.bounds(), state.base_handle.offset())
        };
        let row_ix = if let Some(id) = composer.edit_id {
            let Some(ix) = data.rows.iter().position(
                |row| matches!(row, Row::CommentHeader { comment_id, .. } if *comment_id == id),
            ) else {
                return empty();
            };
            ix
        } else {
            composer.row_ix
        };
        let row_y = row_ix as f32 * row_height() + f32::from(offset.y);
        let y = f32::from(bounds.top()) + (row_y + row_height()).max(8.);
        let x = composer_x(
            f32::from(bounds.left()),
            f32::from(bounds.size.width),
            data.mode,
            composer.side,
        );
        let scroll = data.scroll.clone();
        let entity = cx.entity();
        let item_id = composer.item_id;
        let editing = composer.edit_id.is_some();
        let half_width = (bounds.size.width - px(SPLIT_DIVIDER)) / 2.;
        let inline_left = px(72.)
            + if data.mode == ViewMode::Split && composer.side == CommentSide::Right {
                half_width + px(SPLIT_DIVIDER)
            } else {
                px(0.)
            };
        let height = composer.height.clone();
        let reveal = composer.reveal.clone();
        let card = div()
            .absolute()
            .left(if editing { inline_left } else { px(x) })
            .top(if editing {
                px(row_y + row_height())
            } else {
                px(y)
            })
            .w(if editing {
                (if data.mode == ViewMode::Split {
                    half_width
                } else {
                    bounds.size.width
                } - px(72.))
                .max(px(1.))
            } else {
                px(520.)
            })
            // Keep clicks/hover off the diff underneath, but let wheel events
            // reach its scroller when the composer's input cannot scroll.
            .block_mouse_except_scroll()
            .shadow_lg()
            .on_children_prepainted(move |bounds, window, cx| {
                let Some(card) = bounds.first() else {
                    return;
                };
                let height_changed =
                    height.replace(card.size.height + px(8.)) != card.size.height + px(8.);
                if !reveal.replace(false) && !height_changed {
                    return;
                }
                let state = scroll.0.borrow();
                let viewport = state.base_handle.bounds();
                let handle = state.base_handle.clone();
                let mut offset = handle.offset();
                let row_bottom = offset.y + px((row_ix + 1) as f32 * row_height());
                let latest_top = (viewport.size.height - height.get()).max(px(row_height()));
                let delta = row_bottom.clamp(px(row_height()), latest_top) - row_bottom;
                offset.y += delta;
                if height_changed || delta != px(0.) {
                    // The next layout reserves the measured tail space, so even
                    // a comment on the file's final line has room to scroll.
                    window.defer(cx, move |window, _| {
                        handle.set_offset(offset);
                        window.refresh();
                    });
                }
                if editing && height_changed {
                    let entity = entity.clone();
                    window.defer(cx, move |_, cx| {
                        entity.update(cx, |app, cx| {
                            if let Some(ItemState::Ready(data)) = app
                                .items
                                .iter_mut()
                                .find(|item| item.id == item_id)
                                .map(|item| &mut item.state)
                            {
                                data.rebuild_rows_anchored();
                                cx.notify();
                            }
                        })
                    });
                }
            })
            .child(self.render_composer_card(cx))
            .into_any_element();
        if editing {
            // Match the list's clipping: an edited comment scrolls with its
            // thread, never over the title bar, tray, or neighboring split pane.
            div()
                .absolute()
                .left(bounds.left())
                .top(bounds.top())
                .w(bounds.size.width)
                .h(bounds.size.height)
                .overflow_hidden()
                .child(card)
                .into_any_element()
        } else {
            card
        }
    }

    fn render_composer_card(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let composer = self.composer.as_ref().expect("an open composer");
        let pending = self.composer_uses_pending_review(composer);
        let session_busy = self
            .sessions
            .get(&composer.item_id)
            .is_some_and(|ui| ui.busy);
        let action = self.composer_submit_label(composer);
        let target = if let Some(CommentTarget::Published(target)) = &composer.edit_target {
            match target {
                github::CommentTarget::Issue(_) => "Edit PR conversation comment".to_owned(),
                github::CommentTarget::Review(_) => "Edit review summary".to_owned(),
                github::CommentTarget::ReviewComment(_) => {
                    format!("Edit comment · {}", composer.path)
                }
            }
        } else if composer.placement == ComposerPlacement::ConversationMain {
            if pending {
                "PR conversation · saved in your review summary"
            } else {
                "PR conversation"
            }
            .to_owned()
        } else {
            format!(
                "{}{}:{}{} ({})",
                if composer.reply_to.is_some() {
                    "reply · "
                } else {
                    ""
                },
                composer.path,
                composer
                    .start_line
                    .map(|start| format!("{start}–"))
                    .unwrap_or_default(),
                composer.line,
                composer.side.api_str()
            )
        };
        div()
            .debug_selector(|| "composer-card".into())
            .w_full()
            .min_w_0()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            // The multiline input inserts a newline before emitting PressEnter.
            // Capture Cmd+Enter first; leave plain Enter to the input.
            .capture_action(cx.listener(|this, action: &InputEnter, window, cx| {
                if action.secondary {
                    cx.stop_propagation();
                    this.submit_composer(window, cx);
                }
            }))
            // The input propagates Escape when it has nothing of its own to
            // dismiss; catch it here (before the root's handler) to cancel.
            .on_action(cx.listener(|this, _: &InputEscape, window, cx| {
                this.close_composer(window, cx);
            }))
            .rounded_lg()
            .border_1()
            .border_color(theme::surface0(cx))
            .bg(theme::mantle(cx))
            .p_2()
            .flex()
            .flex_col()
            .gap_2()
            .text_size(px(12.))
            .child(
                div()
                    .text_color(theme::overlay0(cx))
                    .truncate()
                    .child(SharedString::from(target)),
            )
            .child(comment_editor::field(&composer.input, cx))
            .when_some(composer.error.clone(), |card, err| {
                card.child(div().text_color(theme::red(cx)).child(err))
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
                            .text_color(theme::overlay0(cx))
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
                            .disabled(composer.in_flight || session_busy)
                            .loading(composer.in_flight)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.submit_composer(window, cx);
                            })),
                    )
                    .when(
                        (composer.reply_to.is_none()
                            || composer.placement != ComposerPlacement::Diff)
                            && composer.edit_id.is_none()
                            && !pending
                            && matches!(
                                self.active_item().map(|item| &item.source),
                                Some(Source::Pr(_))
                            ),
                        |row| {
                            row.child(
                                div()
                                    .debug_selector(|| "composer-start-review".into())
                                    .child(
                                        Button::new("composer-add-to-review")
                                            .label("Start review")
                                            .small()
                                            .disabled(composer.in_flight || session_busy)
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                if let Some(composer) = &mut this.composer {
                                                    composer.pending = true;
                                                }
                                                this.submit_composer(window, cx);
                                            })),
                                    ),
                            )
                        },
                    ),
            )
            .into_any_element()
    }

    fn edit_pending(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let target = CommentTarget::Pending(id);
        let placement = if self
            .active_item()
            .is_some_and(|item| item.pane == ItemPane::Conversation)
        {
            ComposerPlacement::PendingTray(id)
        } else {
            ComposerPlacement::Diff
        };
        self.edit_comment(target, placement, window, cx);
    }

    fn render_pending_tray(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let empty = || div().into_any_element();
        if self.show_queue {
            return empty();
        }
        let Some(item) = self.active_item() else {
            return empty();
        };
        if !matches!(item.source, Source::Pr(_)) {
            return empty();
        }
        let Some(ui) = self.sessions.get(&item.id) else {
            return empty();
        };
        let id = item.id;
        let commit = match &item.state {
            ItemState::Ready(data) => data
                .pr_meta
                .as_ref()
                .map(|meta| meta.head_ref_oid.clone())
                .unwrap_or_default(),
            _ => String::new(),
        };
        let pending = ui.state.pending.is_some();
        // A fallback editor for an outdated/unanchorable draft lives here.
        // Keep it visible until it is submitted or cancelled, even if the
        // user had not expanded the tray before opening that editor.
        let editing_in_tray = self.composer.as_ref().is_some_and(|composer| {
            composer.item_id == id
                && matches!(composer.placement, ComposerPlacement::PendingTray(_))
        });
        let expanded = ui.comments_expanded || editing_in_tray;
        let label = if pending {
            format!(
                "Pending review · {} comment{}{}",
                ui.state.count(),
                if ui.state.count() == 1 { "" } else { "s" },
                if ui.busy {
                    " · syncing…"
                } else if !ui.state.verified {
                    " · cached"
                } else {
                    ""
                }
            )
        } else if ui.busy {
            "Checking for a pending review…".into()
        } else {
            "Comments can be posted now or batched in a review".into()
        };
        let mut tray = div()
            .flex()
            .flex_col()
            .gap_1()
            .px_3()
            .py_2()
            .border_t_1()
            .border_color(theme::surface0(cx))
            .bg(theme::mantle(cx))
            .text_size(px(12.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .debug_selector(|| "pending-status".into())
                            .flex_1()
                            .child(SharedString::from(label)),
                    )
                    .when(ui.state.count() > 0, |row| {
                        row.child(
                            Button::new("pending-comments-toggle")
                                .debug_selector(|| "pending-comments-toggle".into())
                                .label(if expanded {
                                    "Hide comments"
                                } else {
                                    "Show comments"
                                })
                                .ghost()
                                .small()
                                .disabled(editing_in_tray)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(ui) = this.sessions.get_mut(&id) {
                                        ui.comments_expanded = !ui.comments_expanded;
                                        cx.notify();
                                    }
                                })),
                        )
                    })
                    .child(
                        Button::new("pending-refresh")
                            .debug_selector(|| "pending-refresh".into())
                            .label("Refresh drafts")
                            .ghost()
                            .small()
                            .disabled(ui.busy)
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.resume_session(id, cx)),
                            ),
                    )
                    .child(
                        Button::new("pending-review")
                            .debug_selector(|| "pending-review".into())
                            .label(if pending {
                                "Finish review · ⌘↵"
                            } else {
                                "Start review"
                            })
                            .small()
                            .disabled(ui.busy || (!pending && commit.is_empty()))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if pending {
                                    this.open_review(window, cx);
                                } else {
                                    this.run_session(
                                        id,
                                        SessionCommand::Start {
                                            commit_id: commit.clone(),
                                        },
                                        SessionCompletion::None,
                                        window,
                                        cx,
                                    );
                                }
                            })),
                    ),
            );
        if let Some(error) = &ui.error {
            tray = tray.child(div().text_color(theme::red(cx)).child(error.clone()));
        }
        if let Some(review) = ui.state.pending.as_ref().filter(|_| expanded) {
            let mut comments = div()
                .id("pending-comments")
                .debug_selector(|| "pending-comments".into())
                .max_h(px(160.))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap_1();
            for comment in &review.comments {
                let comment_id = comment.id;
                if self.composer.as_ref().is_some_and(|composer| {
                    composer.item_id == id
                        && composer.placement == ComposerPlacement::PendingTray(comment_id)
                }) {
                    comments = comments.child(self.render_composer_card(cx));
                    continue;
                }
                let location = match comment.line {
                    Some(line) => format!(
                        "{}:{}{} ({})",
                        comment.path,
                        comment
                            .start_line
                            .map(|start| format!("{start}–"))
                            .unwrap_or_default(),
                        line,
                        comment.side.api_str()
                    ),
                    None => format!("{} (outdated)", comment.path),
                };
                comments = comments.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(SharedString::from(format!(
                                    "{location} · {}",
                                    comment.body
                                ))),
                        )
                        .child(
                            Button::new(("pending-edit", comment_id))
                                .label("Edit")
                                .ghost()
                                .small()
                                .disabled(
                                    ui.busy || self.composer.is_some() || self.review.is_some(),
                                )
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.edit_pending(comment_id, window, cx)
                                })),
                        )
                        .child(
                            Button::new(("pending-delete", comment_id))
                                .label("Delete")
                                .ghost()
                                .small()
                                .disabled(
                                    ui.busy || self.composer.is_some() || self.review.is_some(),
                                )
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.confirm_delete_comment(
                                        CommentTarget::Pending(comment_id),
                                        window,
                                        cx,
                                    )
                                })),
                        ),
                );
            }
            tray = tray.child(comments);
        }
        tray.into_any_element()
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
            .bg(Hsla::from(theme::crust(cx)).opacity(0.5))
            .border_l_1()
            .border_color(theme::surface0(cx))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    window.focus(&this.focus_handle, cx);
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
                        let tail_rows = this.diff_tail_rows(pane_h);
                        let total = total + tail_rows;
                        let layout = data.minimap_layout(pane_h, tail_rows);
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
                                MinimapColor::Added => Hsla::from(theme::green(cx)).opacity(0.8),
                                MinimapColor::Removed => Hsla::from(theme::red(cx)).opacity(0.8),
                                MinimapColor::Context => {
                                    Hsla::from(theme::overlay0(cx)).opacity(0.35)
                                }
                                MinimapColor::Header => Hsla::from(theme::blue(cx)).opacity(0.5),
                                MinimapColor::Comment => Hsla::from(theme::blue(cx)).opacity(0.8),
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
                                Hsla::from(theme::text(cx)).opacity(0.08),
                            )
                            .border_widths(1.)
                            .border_color(Hsla::from(theme::overlay0(cx)).opacity(0.4)),
                        );
                    },
                )
                .size_full(),
            )
            .into_any_element()
    }

    fn render_titlebar(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if self.image_viewer.is_some() {
            // Keep the review's geometry unchanged, but unmount all header
            // controls (including its drag/double-click handlers) while modal.
            return div()
                .debug_selector(|| "media-header-spacer".into())
                .h(APP_HEADER_HEIGHT)
                .w_full()
                .flex_shrink_0()
                .bg(gpui::rgb(0x101114))
                .occlude()
                .on_any_mouse_down(|_, window, cx| {
                    window.prevent_default();
                    cx.stop_propagation();
                })
                .on_mouse_up(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_mouse_move(|_, _, cx| cx.stop_propagation())
                .on_scroll(|_, _, cx| cx.stop_propagation())
                .into_any_element();
        }
        let content: gpui::AnyElement = if self.show_queue {
            app_title(None, cx)
        } else {
            match self.active_item() {
                None => app_title(None, cx),
                Some(item) => match &item.state {
                    ItemState::Ready(data) => match &item.source {
                        Source::Pr(_) => match &data.pr_meta {
                            Some(meta) => pr_titlebar_content(meta, cx),
                            None => app_title(None, cx),
                        },
                        Source::Local(src) => local_titlebar_content(item.id, src, data, cx),
                    },
                    ItemState::Loading => {
                        app_title(Some(format!("loading {}…", item.primary())), cx)
                    }
                    ItemState::Failed(_) => {
                        app_title(Some(format!("{} — failed", item.primary())), cx)
                    }
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
                    Some(
                        if matches!(&item.state, ItemState::Ready(data) if !data.diff_loaded) {
                            "loading changes…".into()
                        } else {
                            "reloading…".into()
                        },
                    )
                } else {
                    item.refresh_error
                        .as_ref()
                        .map(|err| SharedString::from(format!("refresh failed: {err}")))
                        .or_else(|| match &item.state {
                            ItemState::Ready(data) => data.viewed.error.clone(),
                            _ => None,
                        })
                }
            })
        };
        // The title bar spreads its children edge to edge; one right group
        // keeps the item note beside the account chip instead of stranded
        // mid-bar.
        let mut right = div().flex().items_center().flex_shrink_0().h_full().gap_2();
        // Keep screen navigation in the title bar so it is available even
        // while the queue is empty, loading, filtered out, or offline.
        if self.show_queue {
            if let Some(item) = self.active_item() {
                right = right.child(
                    div().debug_selector(|| "back-to-review".into()).child(
                        Button::new("back-to-review")
                            .small()
                            .label("Back to review")
                            .tooltip(format!("Return to {} (⌘W)", item.primary()))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.return_to_review(window, cx);
                            })),
                    ),
                );
            }
        } else {
            right = right.child(
                div()
                    .debug_selector(|| "review-queue-navigation".into())
                    .child(
                        Button::new("review-queue-navigation")
                            .small()
                            .label("Review queue")
                            .tooltip("Show review queue (u)")
                            .on_click(cx.listener(|this, _, window, cx| {
                                window.focus(&this.focus_handle, cx);
                                this.back_to_queue(cx);
                            })),
                    ),
            );
        }
        if let Some(data) = self.active_data().filter(|data| {
            !self.show_queue
                && data.diff_loaded
                && self
                    .active_item()
                    .is_some_and(|item| item.pane == ItemPane::Changes)
        }) {
            for (id, icon, tooltip, direction) in [
                (
                    "file-back",
                    IconName::ArrowLeft,
                    "Previous file selection (⌘[)",
                    NavigationDirection::Back,
                ),
                (
                    "file-forward",
                    IconName::ArrowRight,
                    "Next file selection (⌘])",
                    NavigationDirection::Forward,
                ),
            ] {
                right = right.child(
                    div().debug_selector(move || id.into()).child(
                        Button::new(id)
                            .small()
                            .ghost()
                            .icon(icon)
                            .tooltip(tooltip)
                            .disabled(!data.file_history.can_go(direction))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.navigate_file_history(direction, window, cx);
                            })),
                    ),
                );
            }
        }
        if let Some(item) = self.active_item().filter(|item| {
            !self.show_queue
                && matches!(item.source, Source::Pr(_))
                && matches!(item.state, ItemState::Ready(_))
        }) {
            let (target, label, tooltip) = match item.pane {
                ItemPane::Changes => (
                    ItemPane::Conversation,
                    "Conversation",
                    "Show conversation (⇧⌘←)",
                ),
                ItemPane::Conversation => (ItemPane::Changes, "Changes", "Show changes (⇧⌘→)"),
            };
            right = right.child(
                div()
                    .id("pane-toggle-container")
                    .debug_selector(|| "pane-toggle".to_string())
                    .child(
                        Button::new("pane-toggle")
                            .small()
                            .label(label)
                            .tooltip(tooltip)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                window.focus(&this.focus_handle, cx);
                                this.flip_pane(target, cx);
                            })),
                    ),
            );
        }
        if let Some(note) = note {
            right = right.child(
                div()
                    .max_w(px(280.))
                    .truncate()
                    .text_color(theme::overlay0(cx))
                    .child(note),
            );
        }
        right = right.child(self.render_account(cx));
        TitleBar::new()
            .h(APP_HEADER_HEIGHT)
            .text_size(px(13.))
            .child(content)
            .child(right)
            .into_any_element()
    }

    /// The account chip at the title bar's right edge: avatar + login when
    /// GitHub knows who we are; the fix in one line when it doesn't — the
    /// empty pane spells the failure out in full. While the check is still
    /// running, nothing: the chip appears rather than flickers.
    fn render_account(&self, cx: &gpui::App) -> gpui::AnyElement {
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
                        .text_color(theme::subtext(cx))
                        .child(SharedString::from(viewer.login.clone())),
                )
                .into_any_element(),
            AccountStatus::SignedOut(err) => div()
                .pr_3()
                .max_w(px(360.))
                .truncate()
                .text_size(px(12.))
                .text_color(theme::peach(cx))
                .child(SharedString::from(err.to_string()))
                .into_any_element(),
        }
    }

    fn finish_sidebar_resize(&mut self, cx: &mut Context<Self>) {
        if self.sidebar_resize.take().is_some() {
            self.store
                .set_setting(SIDEBAR_WIDTH_SETTING, serde_json::json!(self.sidebar_width));
            cx.notify();
        }
    }

    fn render_sidebar(&mut self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let width = sidebar_width(self.sidebar_width, window.viewport_size().width.into());
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
            let dot: Hsla = item.dot_color(cx).into();
            let status: gpui::AnyElement = match &item.state {
                ItemState::Ready(data) => div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .flex_shrink_0()
                    .text_size(px(11.))
                    .child(
                        div()
                            .text_color(theme::green(cx))
                            .child(SharedString::from(format!("+{}", data.additions))),
                    )
                    .child(
                        div()
                            .text_color(theme::red(cx))
                            .child(SharedString::from(format!("−{}", data.deletions))),
                    )
                    .into_any_element(),
                ItemState::Loading => div()
                    .flex_shrink_0()
                    .text_size(px(11.))
                    .text_color(theme::overlay0(cx))
                    .child(SharedString::from("loading…"))
                    .into_any_element(),
                ItemState::Failed(_) => div()
                    .flex_shrink_0()
                    .text_size(px(11.))
                    .text_color(theme::red(cx))
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
                .when(active, |entry| entry.bg(theme::surface0(cx)))
                .when(!active, |entry| {
                    entry.hover(|style| style.bg(Hsla::from(theme::surface0(cx)).opacity(0.5)))
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
                                .text_color(theme::text(cx))
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
                            .text_color(theme::subtext(cx))
                            .child(secondary),
                    )
                });
            list = list.child(entry);
        }

        // --- stock file tree, with application-owned topology and identities ---
        let query = self.tree_filter_input.read(cx).value().trim().to_string();
        if self
            .active_data()
            .is_some_and(|data| data.tree_view.is_none())
        {
            let view = tree::FileTree::new(self.active_item().unwrap().id, cx);
            self.active_data_mut().unwrap().tree_view = Some(view);
        }
        let tree_state = self.active_data_mut().and_then(|data| {
            let paths: Vec<_> = data
                .diff
                .files
                .iter()
                .map(|file| file.display_path())
                .collect();
            let view = data.tree_view.as_mut()?;
            view.reconcile(&data.tree, &paths, &data.collapsed, &query, cx);
            if data.tree_last_file != Some(data.file_ix) {
                data.tree_last_file = Some(data.file_ix);
                if let Some(path) = paths.get(data.file_ix) {
                    let state = view.state.read(cx);
                    if let Some(ix) = state.index_of(&tree::file_id(path)) {
                        state
                            .scroll_handle()
                            .scroll_to_item(ix, ScrollStrategy::Center);
                    }
                }
            }
            Some(view.state.clone())
        });
        let entity = cx.entity();
        let filtered = !query.is_empty();
        let tree_list: gpui::AnyElement = match tree_state {
            Some(state) if state.read(cx).entry(0).is_some() => {
                gpui::component::tree::tree(&state, move |pos, entry, _, _, cx| {
                    let app = entity.read(cx);
                    let Some(data) = app.active_data() else {
                        return gpui::component::list::ListItem::new(pos).disabled(true);
                    };
                    let id = entry.item().id.as_ref();
                    let file = id.strip_prefix("file:").and_then(|path| {
                        data.diff
                            .files
                            .iter()
                            .position(|file| file.display_path() == path)
                    });
                    let row = if filtered {
                        file.map(TreeListRow::FilteredFile)
                    } else {
                        data.tree
                            .iter()
                            .position(|entry| match &entry.kind {
                                TreeEntryKind::Dir { path } => {
                                    id.strip_prefix("directory:") == Some(path.as_str())
                                }
                                TreeEntryKind::File { file_ix } => Some(*file_ix) == file,
                            })
                            .map(TreeListRow::Entry)
                    };
                    let Some(row) = row else {
                        return gpui::component::list::ListItem::new(pos).disabled(true);
                    };
                    render_tree_row(
                        row,
                        pos,
                        file == Some(data.file_ix),
                        data,
                        &entity,
                        app.active_item().is_some_and(|item| item.reloading),
                        cx,
                    )
                })
                .into_any_element()
            }
            Some(_) if filtered => div()
                .px_3()
                .py_2()
                .text_color(theme::overlay0(cx))
                .child("no matching files")
                .into_any_element(),
            _ => div().into_any_element(),
        };

        div()
            .id("sidebar")
            .debug_selector(|| "sidebar".to_string())
            .relative()
            .w(px(width))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(theme::mantle(cx))
            .text_size(px(12.))
            .child(list)
            .child(div().h(px(1.)).flex_shrink_0().bg(theme::surface0(cx)))
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
                        window.focus(&this.focus_handle, cx);
                        cx.notify();
                    }))
                    .child(
                        div()
                            .p_2()
                            .child(Input::new(&self.tree_filter_input).small()),
                    )
                    .when_some(
                        self.active_data()
                            .and_then(|data| data.viewed.error.clone()),
                        |area, error| {
                            area.child(
                                div()
                                    .px_2()
                                    .pb_2()
                                    .text_size(px(11.))
                                    .text_color(theme::red(cx))
                                    .child(error),
                            )
                        },
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            // Tree's default press focuses its keyboard context.
                            // Return focus after it observes the packet, without
                            // suppressing its selection or active interaction state.
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|app, _, window, cx| {
                                    window.focus(&app.focus_handle, cx);
                                }),
                            )
                            .child(tree_list),
                    ),
            )
            .child(
                gpui::component::resize_handle::<(), ReviewApp>(
                    "sidebar-resize",
                    gpui::Axis::Horizontal,
                )
                .inside(gpui::component::resizable::HandleEdge::Trailing)
                .debug_selector(|| "sidebar-resize".to_string())
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                        this.sidebar_resize = Some((event.position.x, width));
                        cx.notify();
                    }),
                ),
            )
    }

    /// The review queue screen — what a bare launch opens on. Rows are the
    /// facade's queue items in the search's order (oldest request first,
    /// oldest-created at a toggle); `j`/`k` walk them with a visible
    /// selection, the control row sorts and filters (see
    /// [`Self::queue_controls`]). Failures are non-destructive: the last
    /// rows stay up with the error beside them, and a first-fetch failure
    /// explains itself where the rows would be, naming the fix. An empty
    /// queue says so instead of a blank pane — an empty *filtered* one
    /// offers the way back out.
    fn render_queue(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let count = self.queue.prs.len();
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .px_6()
            .py_4()
            .child(
                div()
                    .debug_selector(|| "queue-table".into())
                    .w_full()
                    .min_w(px(QUEUE_MIN_WIDTH))
                    .max_w(px(QUEUE_MAX_WIDTH))
                    .h_full()
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
                            .border_color(theme::surface0(cx))
                            .child(div().text_color(theme::text(cx)).child(
                                if self.queue.search.is_past_reviews() {
                                    "Past reviews"
                                } else {
                                    "Review queue"
                                },
                            ))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_right()
                                    .text_size(px(12.))
                                    .text_color(theme::subtext(cx))
                                    .child(SharedString::from(format!(
                                        "{count} {}",
                                        if count == 1 { "PR" } else { "PRs" }
                                    ))),
                            ),
                    )
                    // Controls remain reachable when loading, empty, or offline.
                    .child(self.queue_controls(cx.entity(), cx))
                    .child(self.render_queue_body(cx)),
            )
            .into_any_element()
    }

    fn render_queue_body(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let queue = &self.queue;
        if queue.prs.is_empty() && queue.loading {
            return centered_message("loading review queue…".into(), theme::overlay0(cx));
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
                            .text_color(theme::peach(cx))
                            .child(SharedString::from(err.clone())),
                    )
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(theme::overlay0(cx))
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
                            .text_color(theme::overlay0(cx))
                            .child("no PRs match the current filters"),
                    )
                    .child(
                        Button::new("queue-clear-empty")
                            .small()
                            .label("clear filters (escape)")
                            .on_click(move |_, window, cx| {
                                entity.update(cx, |this, cx| {
                                    window.focus(&this.focus_handle, cx);
                                    this.queue_clear_filters(cx);
                                })
                            }),
                    )
                    .into_any_element();
            }
            return centered_message(
                if queue.search.is_past_reviews() {
                    "no past reviews".into()
                } else {
                    "no PRs awaiting your review".into()
                },
                theme::overlay0(cx),
            );
        }
        let entity = cx.entity();
        let now = now_unix();
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            // Outside the scrolling list, so headings stay aligned and visible.
            .child(queue_header(cx))
            .child(
                list(queue.scroll.clone(), move |pos, _window, cx| {
                    let this = entity.read(cx);
                    queue_row(
                        &this.queue.prs[pos],
                        pos,
                        pos == this.queue.selected,
                        now,
                        entity.clone(),
                        cx,
                    )
                })
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
    fn queue_controls(&self, entity: gpui::Entity<Self>, cx: &gpui::App) -> gpui::AnyElement {
        let search = &self.queue.search;
        let created = search.ordering() == github::QueueSort::CreatedAscending;
        let past = search.is_past_reviews();
        // The label names the active sort; both options are ascending.
        let mut row = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1()
            .px_3()
            .py_1()
            .flex_shrink_0()
            .border_b_1()
            .border_color(theme::surface0(cx))
            .child(
                Button::new("queue-past-reviews-toggle")
                    .debug_selector(|| "queue-past-reviews-toggle".into())
                    .xsmall()
                    .selected(past)
                    .label("Past reviews")
                    .tooltip("Show your 10 most recently reviewed PRs, newest review first")
                    .on_click({
                        let entity = entity.clone();
                        move |_, window, cx| {
                            entity.update(cx, |this, cx| {
                                window.focus(&this.focus_handle, cx);
                                this.queue_toggle_past_reviews(cx);
                            })
                        }
                    }),
            )
            .when(!past, |row| {
                row.child(
                    Button::new("queue-sort-toggle")
                        .debug_selector(|| "queue-sort-toggle".into())
                        .xsmall()
                        .label(if created {
                            "sort: created ↑"
                        } else {
                            "sort: review requested ↑"
                        })
                        .tooltip("Toggle queue order (s)")
                        .on_click({
                            let entity = entity.clone();
                            move |_, window, cx| {
                                entity.update(cx, |this, cx| {
                                    window.focus(&this.focus_handle, cx);
                                    this.queue_toggle_sort(cx);
                                })
                            }
                        }),
                )
            })
            .when(past, |row| {
                row.child(
                    div()
                        .px_2()
                        .text_color(theme::subtext(cx))
                        .child("latest 10 · reviewed ↓"),
                )
            });
        // Chip ids run across the whole row: groups re-enumerated would
        // collide ("org"'s first chip and "repo"'s are both
        // ("queue-chip", 0)).
        let mut chip_ix = 0usize;
        for group in queue_chips(&self.queue.prs, search) {
            for chip in group.chips {
                let ix = chip_ix;
                chip_ix += 1;
                row = row.child(
                    Button::new(("queue-chip", ix))
                        .debug_selector(move || format!("queue-chip-{ix}"))
                        .xsmall()
                        .ghost()
                        .selected(chip.active)
                        .label(format!(
                            "{}:{} · {}",
                            group.kind.prefix(),
                            chip.filter.value(),
                            chip.count
                        ))
                        .max_w(px(300.))
                        .tooltip(format!("{}:{}", group.kind.prefix(), chip.filter.value()))
                        .on_click({
                            let entity = entity.clone();
                            let filter = chip.filter.clone();
                            move |_, window, cx| {
                                entity.update(cx, |this, cx| {
                                    window.focus(&this.focus_handle, cx);
                                    this.queue_toggle_filter(filter.clone(), cx);
                                })
                            }
                        }),
                );
            }
            if group.hidden > 0 {
                row = row.child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme::overlay0(cx))
                        .child(SharedString::from(format!("+{} more", group.hidden))),
                );
            }
        }
        if !search.filters().is_empty() {
            row = row.child(
                Button::new("queue-clear-filters")
                    .xsmall()
                    .ghost()
                    .label("✕ clear")
                    .tooltip("Clear queue filters (escape)")
                    .on_click({
                        let entity = entity.clone();
                        move |_, window, cx| {
                            entity.update(cx, |this, cx| {
                                window.focus(&this.focus_handle, cx);
                                this.queue_clear_filters(cx);
                            })
                        }
                    }),
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
                gpui::Hsla::from(theme::overlay0(cx)),
            ));
        } else if let Some(err) = self.queue.error.as_deref() {
            status.push((err.to_string(), gpui::Hsla::from(theme::peach(cx))));
        } else if self.queue.origin == QueueOrigin::Cached {
            let age = self.store.cached_queue().age_seconds(local_store::now_ms());
            status.push((
                format!("cached · {age}s ago"),
                gpui::Hsla::from(theme::overlay0(cx)),
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
                gpui::Hsla::from(theme::peach(cx)),
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

    /// The pane with nothing open: the ⌘O hint — plus, when GitHub isn't
    /// usable, the facade's sign-in message above it, so a fresh launch
    /// without `gh auth login` explains itself instead of showing a near-
    /// blank screen. With something open, local diffs included, only the
    /// title-bar chip carries the message.
    fn render_empty_pane(&self, cx: &gpui::App) -> gpui::AnyElement {
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
                        .text_color(theme::peach(cx))
                        .child(SharedString::from(err.to_string())),
                )
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(theme::overlay0(cx))
                        .child(SharedString::from("⌘O to open a PR or path")),
                )
                .into_any_element(),
            _ => centered_message("⌘O to open a PR or path".into(), theme::overlay0(cx)),
        }
    }

    fn render_footer(&self, cx: &gpui::App) -> impl IntoElement {
        let hint = |keys: &[&'static str], label: &'static str| {
            let mut hint = div().flex().items_center().gap_1();
            for key in keys {
                hint = hint.child(Kbd::new(Keystroke::parse(key).unwrap()));
            }
            hint.child(
                div()
                    .text_color(theme::overlay0(cx))
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
            .bg(theme::mantle(cx))
            .border_t_1()
            .border_color(theme::surface0(cx))
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
                .child(hint(&["cmd-o"], "open"));
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
                .child(hint(&["cmd-o"], "open"));
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
                .child(hint(&["cmd-o"], "open"))
                .child(hint(&["r"], "refresh"))
                .child(hint(&["cmd-enter"], "review"));
        }
        footer
    }
    /// Blank list slots shared by the diff's scroll range and minimap. Keep
    /// composer space in addition to the half-viewport scroll-past-end area.
    fn diff_tail_rows(&self, pane_height: f32) -> usize {
        let composer_rows = self
            .composer
            .as_ref()
            .filter(|composer| {
                self.active_item()
                    .is_some_and(|item| item.id == composer.item_id)
                    && composer.placement == ComposerPlacement::Diff
                    && composer.edit_id.is_none()
            })
            .map_or(0, |composer| {
                (f32::from(composer.height.get()) / row_height()).ceil() as usize
            });
        diff_padding_rows(pane_height, row_height()) + composer_rows
    }

    /// The pane for an open item — loading, failure message, the
    /// conversation, or the diff itself — everything the main pane shows
    /// when the queue screen isn't up.
    fn render_item_pane(&self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let entity = cx.entity();
        let plus_entity = entity.clone();
        match self.active_item() {
            None => self.render_empty_pane(cx),
            Some(item) => match &item.state {
                ItemState::Loading => centered_message("loading…".into(), theme::overlay0(cx)),
                ItemState::Failed(msg) => div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .p_8()
                    .child(
                        div()
                            .max_w(px(720.))
                            .text_color(theme::red(cx))
                            .child(SharedString::from(msg.clone())),
                    )
                    .into_any_element(),
                ItemState::Ready(data) if item.pane == ItemPane::Conversation => {
                    self.render_conversation(item, data, window, cx)
                }
                ItemState::Ready(data) if !data.diff_loaded => centered_message(
                    if item.reloading {
                        "loading changes…".into()
                    } else {
                        "Changes unavailable — refresh to try again.".into()
                    },
                    theme::overlay0(cx),
                ),
                ItemState::Ready(data) if data.shows_file_media() => div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .when(matches!(item.source, Source::Pr(_)), |pane| {
                        pane.child(self.render_review_scope(item, data, cx))
                    })
                    .children(file_media::render_svg_controls(data, cx))
                    .child(file_media::render(
                        &data.diff.files[data.file_ix],
                        data.file_media.get(&data.file_ix),
                        cx,
                    ))
                    .into_any_element(),
                ItemState::Ready(data) => div()
                    .debug_selector(|| "code-diff-pane".into())
                    .size_full()
                    .flex()
                    .flex_col()
                    .when(matches!(item.source, Source::Pr(_)), |pane| {
                        pane.child(self.render_review_scope(item, data, cx))
                    })
                    .children(file_media::render_svg_controls(data, cx))
                    .child(self.render_presentation_controls(data, cx))
                    .child(
                        div()
                            .size_full()
                            .min_h_0()
                            .relative()
                            .flex()
                            .font_family(MONO)
                            .text_size(px(text_size()))
                            .line_height(px(row_height()))
                            // Selection mouse listeners live on the diff pane only.
                            // The Scrollbar stops propagation of its own
                            // mouse-downs and thumb drags before they reach us.
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                                    window.focus(&this.focus_handle, cx);
                                    let char_width = this.char_width(window);
                                    if event.modifiers.shift {
                                        let Some((side, head)) =
                                            this.pane_hit(event.position, char_width, None)
                                        else {
                                            return;
                                        };
                                        // The composer takes focus below; don't let
                                        // the parent diff container's default
                                        // mouse-down behavior take it back.
                                        window.prevent_default();
                                        this.extend_comment_selection(side, head, window, cx);
                                        return;
                                    }
                                    let hit = this.pane_hit(event.position, char_width, None);
                                    let selection = if event.click_count == 2 {
                                        this.pane_word_selection(event.position, char_width)
                                    } else {
                                        None
                                    };
                                    this.drag_anchor = if event.click_count == 2 {
                                        selection.map(SelectionDrag::Word)
                                    } else {
                                        hit.map(|(side, anchor)| SelectionDrag::Character {
                                            side,
                                            anchor,
                                        })
                                    };
                                    // A plain click also moves the keyboard cursor:
                                    // `C` and `n`/`p` follow the line the reviewer
                                    // last touched, mouse or keyboard alike.
                                    if let Some(data) = this.active_data_mut() {
                                        let previous = (data.cursor, data.cursor_side);
                                        if let Some((side, hit)) = hit {
                                            data.cursor = hit.row;
                                            data.cursor_side = Some(side);
                                        }
                                        // Repaint even without a text selection: the
                                        // gutter cue follows a plain click immediately.
                                        let selection_changed = data.selection != selection;
                                        data.selection = selection;
                                        if selection_changed
                                            || previous != (data.cursor, data.cursor_side)
                                        {
                                            cx.notify();
                                        }
                                    }
                                }),
                            )
                            .on_mouse_move(cx.listener(
                                |this, event: &MouseMoveEvent, window, cx| {
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
                                    let Some(drag) = this.drag_anchor else {
                                        return;
                                    };
                                    let char_width = this.char_width(window);
                                    let hit = match drag {
                                        SelectionDrag::Character { .. } => this.pane_hit(
                                            event.position,
                                            char_width,
                                            Some(drag.side()),
                                        ),
                                        SelectionDrag::Word(_) => this
                                            .pane_text_hit(event.position, Some(drag.side()))
                                            .map(|(side, row, x)| {
                                                let col = (f32::from(x) / f32::from(char_width))
                                                    .floor()
                                                    .max(0.)
                                                    as usize;
                                                (side, RowCol { row, col })
                                            }),
                                    };
                                    let Some((_, head)) = hit else {
                                        return;
                                    };
                                    if let Some(data) = this.active_data_mut() {
                                        let selection = drag.selection(&data.rows, head);
                                        if data.selection != selection {
                                            data.selection = selection;
                                            cx.notify();
                                        }
                                    }
                                },
                            ))
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
                                // Resolve the actual list viewport before creating its
                                // blank tail, including on the first frame and resize.
                                // Padding the list's style would shrink its rendered
                                // viewport; extra virtual slots leave it full-height.
                                canvas(
                                    move |bounds, window, cx| {
                                        let this = entity.read(cx);
                                        let data = this.active_data().expect("ready diff pane");
                                        let diff_rows = data.rows.len()
                                            + this.diff_tail_rows(f32::from(bounds.size.height));
                                        let rows_entity = entity.clone();
                                        let mut list = uniform_list(
                                            "diff",
                                            diff_rows,
                                            move |range, _window, cx| {
                                                let this = rows_entity.read(cx);
                                                let Some(data) = this.active_data() else {
                                                    return Vec::new();
                                                };
                                                let sel = data.selection;
                                                range
                                                    .map(|ix| {
                                                        let Some(row) = data.rows.get(ix) else {
                                                            return div()
                                                                .h(px(row_height()))
                                                                .into_any_element();
                                                        };
                                                        let row_sel = sel.and_then(|sel| {
                                                            row_selection_range(&sel, ix, row)
                                                                .filter(|range| !range.is_empty())
                                                                .map(|range| (sel.side, range))
                                                        });
                                                        let active = (ix == data.cursor)
                                                            .then_some(data.cursor_side)
                                                            .flatten();
                                                        let commenting = this
                                                            .composer
                                                            .as_ref()
                                                            .and_then(|composer| {
                                                                composer.highlighted_side(
                                                                    this.items[this.active].id,
                                                                    data,
                                                                    row,
                                                                )
                                                            });
                                                        let split_layout = ((data.mode
                                                            == ViewMode::Split
                                                            || matches!(
                                                                row,
                                                                Row::CommentBody { .. }
                                                                    | Row::CommentHeader { .. }
                                                                    | Row::CommentEditor { .. }
                                                                    | Row::CommentReactions { .. }
                                                            ))
                                                            && !data
                                                                .preferences
                                                                .soft_wrap
                                                                .unwrap_or(false))
                                                        .then(|| {
                                                            data.split_layout(
                                                                this.char_width.unwrap_or(px(
                                                                    text_size() * 0.6,
                                                                )),
                                                            )
                                                        });
                                                        render_row(
                                                            ix,
                                                            row,
                                                            data.rows.get(ix + 1),
                                                            row_sel,
                                                            active,
                                                            commenting,
                                                            split_layout,
                                                            data.search.as_ref(),
                                                            &rows_entity,
                                                            match row {
                                                                Row::CommentHeader {
                                                                    comment_id,
                                                                    top,
                                                                    ..
                                                                } => Some(
                                                                    this.diff_comment_controls(
                                                                        *comment_id,
                                                                        *top,
                                                                        &rows_entity,
                                                                        cx,
                                                                    ),
                                                                ),
                                                                Row::CommentReactions {
                                                                    comment_id,
                                                                    ..
                                                                } => Some(
                                                                    this.diff_comment_reactions(
                                                                        *comment_id,
                                                                        &rows_entity,
                                                                    ),
                                                                ),
                                                                _ => None,
                                                            },
                                                            cx,
                                                        )
                                                    })
                                                    .collect()
                                            },
                                        )
                                        .with_decoration(diff_media::InlineText(entity.clone()))
                                        .track_scroll(&data.scroll)
                                        .with_horizontal_sizing_behavior(
                                            if data.preferences.soft_wrap.unwrap_or(false) {
                                                ListHorizontalSizingBehavior::FitList
                                            } else {
                                                ListHorizontalSizingBehavior::Unconstrained
                                            },
                                        )
                                        // Unconstrained sizing turns on horizontal overflow,
                                        // but gpui sizes the scroll range from one measured
                                        // item — row 0, the file header, which never exceeds
                                        // the viewport, so `offset.x` was clamped to zero and
                                        // long lines were unreachable. Measure the widest
                                        // row instead. Split rows expose their scroll
                                        // extent without widening the visible panes.
                                        .with_width_from_item(
                                            (!data.preferences.soft_wrap.unwrap_or(false))
                                                .then_some(data.wide_row),
                                        )
                                        .size_full()
                                        .into_any_element();
                                        list.layout_as_root(
                                            bounds.size.map(gpui::AvailableSpace::Definite),
                                            window,
                                            cx,
                                        );
                                        list.prepaint_at(bounds.origin, window, cx);
                                        list
                                    },
                                    |_, mut list, window, cx| list.paint(window, cx),
                                )
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
                            // Resolve hover only after the list's prepaint has applied
                            // deferred scrolling and clamped its offset. A cached row
                            // from the last mouse move (or a render-time offset) can
                            // lag the code, especially under a stationary pointer.
                            .child(
                                canvas(
                                    move |bounds, window, cx| {
                                        let mut plus = plus_entity.update(cx, |this, cx| {
                                            this.hover_plus =
                                                this.hover_target(window.mouse_position(), window);
                                            this.render_plus(cx).map(|plus| {
                                                div().size_full().child(plus).into_any_element()
                                            })
                                        });
                                        if let Some(plus) = &mut plus {
                                            plus.layout_as_root(
                                                bounds.size.map(gpui::AvailableSpace::Definite),
                                                window,
                                                cx,
                                            );
                                            plus.prepaint_at(bounds.origin, window, cx);
                                        }
                                        plus
                                    },
                                    |_, plus, window, cx| {
                                        if let Some(mut plus) = plus {
                                            plus.paint(window, cx);
                                        }
                                    },
                                )
                                .absolute()
                                .size_full(),
                            ),
                    )
                    .into_any_element(),
            },
        }
    }

    /// The conversation pane: description and published timeline as full GFM
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
        // Only a PR has a conversation to read; its title stays in the titlebar.
        let (Some(meta), Source::Pr(loc)) = (&data.pr_meta, &item.source) else {
            return centered_message(
                "A local diff has no conversation".into(),
                theme::overlay0(cx),
            );
        };
        // The pipe runs on every paint; bodies are small, the output is
        // what the TextView's keyed state diffs against (same text → no
        // re-parse), and keeping it out of ItemData means a refresh swaps
        // the description without any cache to invalidate.
        let body = conversation::preprocess(&meta.body, &loc.repo_slug());
        let content = if body.trim().is_empty() {
            // GitHub's own empty state, verbatim.
            div()
                .text_color(theme::overlay0(cx))
                .child(SharedString::from("No description provided."))
                .into_any_element()
        } else {
            conversation::render_markdown(
                &format!("conversation-{}", item.id),
                body,
                self,
                window,
                cx,
            )
        };
        // The pane itself is the scroll container — like the diff pane, not
        // like the TextView's own scrollable mode — so the wheel scrolls
        // from anywhere over the pane (the centered column's gutters
        // included). The timeline shares this scroller. TextViews keep keyed
        // parse state; resolved bodies are not rendered until expanded.
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
                    .debug_selector(|| "conversation".into())
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
                            .child(content)
                            .child(self.render_commits(item, data, cx))
                            .children(self.render_discussion(item, data, window, cx))
                            .when_some(
                                self.sessions
                                    .get(&item.id)
                                    .and_then(|ui| ui.state.pending.as_ref())
                                    .filter(|review| !review.body.trim().is_empty()),
                                |column, review| {
                                    column.child(
                                        div()
                                            .mt_6()
                                            .p_4()
                                            .border_1()
                                            .border_color(theme::surface0(cx))
                                            .rounded_md()
                                            .child(div().mb_2().text_color(theme::yellow(cx)).child(
                                                "Pending review summary · only you can see this",
                                            ))
                                            .child(conversation::render_markdown(
                                                &format!("conversation-summary-{}", item.id),
                                                conversation::preprocess(
                                                    &review.body,
                                                    &loc.repo_slug(),
                                                ),
                                                self,
                                                window,
                                                cx,
                                            )),
                                    )
                                },
                            )
                            .child(
                                div()
                                    .mt_6()
                                    .child(self.conversation_composer(item.id, None, cx)),
                            ),
                    ),
            )
            .child(Scrollbar::vertical(&data.conv_scroll))
            .into_any_element()
    }
}

impl Render for ReviewApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.resync_comment_wrap(window);
        self.resync_inline_text(window, cx);
        let item_ids = self.items.iter().map(|item| item.id).collect::<Vec<_>>();
        let active = (!self.show_queue)
            .then(|| self.active_item().map(|item| (item.id, item.pane)))
            .flatten();
        self.video_hub
            .update(cx, |hub, cx| hub.sync(&item_ids, active, cx));
        let image_viewer = self.render_image_viewer(window, cx);
        let pane: gpui::AnyElement = if self.show_queue {
            self.render_queue(cx)
        } else {
            self.render_item_pane(window, cx)
        };
        let content = div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(theme::base(cx))
            .text_color(theme::text(cx))
            .on_action(cx.listener(|this, _: &FileBack, window, cx| {
                this.navigate_file_history(NavigationDirection::Back, window, cx)
            }))
            .on_action(cx.listener(|this, _: &FileForward, window, cx| {
                this.navigate_file_history(NavigationDirection::Forward, window, cx)
            }))
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Back),
                cx.listener(|this, _, window, cx| {
                    this.navigate_file_history(NavigationDirection::Back, window, cx)
                }),
            )
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Forward),
                cx.listener(|this, _, window, cx| {
                    this.navigate_file_history(NavigationDirection::Forward, window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &NextFile, _, cx| this.step_file(1, cx)))
            .on_action(
                cx.listener(|this, _: &search::OpenSearch, window, cx| {
                    this.open_search(window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &search::OpenDiffSearch, window, cx| {
                this.open_diff_search(window, cx)
            }))
            .on_action(cx.listener(|this, _: &search::NextMatch, window, cx| {
                this.step_search(1, window, cx)
            }))
            .on_action(cx.listener(|this, _: &search::PreviousMatch, window, cx| {
                this.step_search(-1, window, cx)
            }))
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
            .on_action(cx.listener(|this, _: &ToggleViewed, _, cx| this.toggle_current_viewed(cx)))
            .on_action(cx.listener(|this, _: &ToggleWhitespace, _, cx| {
                this.toggle_presentation(presentation::Preference::Whitespace, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleUnchanged, _, cx| {
                this.toggle_presentation(presentation::Preference::Collapse, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleSoftWrap, _, cx| {
                this.toggle_presentation(presentation::Preference::Wrap, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleMinimap, _, cx| {
                this.minimap_visible = !this.minimap_visible;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleComments, _, cx| this.toggle_comments(cx)))
            .on_action(cx.listener(|this, _: &NewComment, window, cx| {
                this.open_composer_on_selection(window, cx);
            }))
            .on_action(cx.listener(|this, _: &SubmitReview, window, cx| {
                // Backstop: with the dialog open, its card captures the
                // input's secondary-enter action to handle submission.
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
            .when(self.sidebar_resize.is_some(), |root| {
                let entity = cx.entity();
                root.cursor_col_resize().child(
                    // Capture moves even outside the window, not just while
                    // the root's hitbox is hovered.
                    canvas(
                        |_, _, _| (),
                        move |_, _, window, _| {
                            window.on_mouse_event(
                                move |event: &MouseMoveEvent, phase, window, cx| {
                                    if phase != gpui::DispatchPhase::Capture {
                                        return;
                                    }
                                    entity.update(cx, |this, cx| {
                                        let Some((start_x, start_width)) = this.sidebar_resize
                                        else {
                                            return;
                                        };
                                        if event.dragging() {
                                            this.sidebar_width = sidebar_width(
                                                start_width + f32::from(event.position.x - start_x),
                                                window.viewport_size().width.into(),
                                            );
                                            // Let the stock handle observe the
                                            // same packet and update its state.
                                            cx.notify();
                                        } else {
                                            this.finish_sidebar_resize(cx);
                                        }
                                    });
                                },
                            );
                        },
                    )
                    .size(px(0.)),
                )
            })
            .capture_any_mouse_up(cx.listener(|this, event: &MouseUpEvent, _, cx| {
                if event.button == MouseButton::Left {
                    this.finish_sidebar_resize(cx);
                }
            }))
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| this.finish_sidebar_resize(cx)),
            )
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
            // diff pane has focus.
            .on_action(cx.listener(|this, _: &ClearSelection, window, cx| {
                if this.close_search(window, cx) {
                    return;
                }
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
            .on_action(cx.listener(|this, _: &ZoomIn, _, cx| this.zoom(1., false, cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| this.zoom(-1., false, cx)))
            .on_action(cx.listener(|this, _: &ZoomReset, _, cx| this.zoom(0., true, cx)))
            .on_action(cx.listener(|this, _: &FocusTreeFilter, window, cx| {
                this.tree_filter_input
                    .update(cx, |state, cx| state.focus(window, cx));
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &OpenInput, window, cx| {
                this.open_quick_open(window, cx);
            }))
            .on_action(cx.listener(|this, _: &CloseItem, window, cx| {
                if this.show_queue {
                    // The queue is a screen, not a tab: never close an item
                    // the user cannot see behind it.
                    this.return_to_review(window, cx);
                } else {
                    this.close_item(this.active, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &NextItem, _, cx| this.cycle_items(1, cx)))
            .on_action(cx.listener(|this, _: &PrevItem, _, cx| this.cycle_items(-1, cx)))
            .on_action(cx.listener(|this, _: &ShowConversation, _, cx| {
                this.flip_pane(ItemPane::Conversation, cx)
            }))
            .on_action(
                cx.listener(|this, _: &ShowChanges, _, cx| this.flip_pane(ItemPane::Changes, cx)),
            )
            // Inputs propagate Escape when they have nothing of their own
            // to dismiss: hand focus back to the diff.
            .on_action(cx.listener(|this, _: &InputEscape, window, cx| {
                window.focus(&this.focus_handle, cx);
                cx.notify();
            }))
            .child(self.render_titlebar(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    // The queue is a standalone screen; navigation belongs
                    // only to the review screen (Changes or Conversation).
                    .when(!self.show_queue, |main| {
                        main.child(self.render_sidebar(window, cx))
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
                            } else if self.composer.as_ref().is_some_and(|composer| {
                                composer.placement != ComposerPlacement::Diff
                                    && self.active_item().is_some_and(|item| {
                                        item.id == composer.item_id
                                            && item.pane == ItemPane::Conversation
                                    })
                            }) {
                                // Inline conversation inputs must not inherit
                                // the diff's single-letter shortcuts.
                                "ConversationComposer"
                            } else {
                                "ReviewApp"
                            })
                            .track_focus(&self.focus_handle)
                            .child(pane),
                    ),
            )
            .child(self.render_footer(cx))
            .child(self.render_pending_tray(cx))
            .child(self.render_search(cx))
            // Root-level so the composer's input escapes the "ReviewApp" key
            // context (plain letters must stay text, like the quick-open input).
            .when(self.composer.is_some(), |root| {
                root.child(self.render_composer(cx))
            });
        // A sibling, not a descendant of review shortcut handlers. Keep the
        // review mounted so its selection, scroll and element state survive.
        div()
            .size_full()
            .relative()
            .child(content)
            .children(image_viewer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mentions::{mention_prefix, mention_rank};
    use crate::rows::LineKind;
    use gpui::component::Rope;
    use std::path::PathBuf;
    fn file_mode_data(diff: PrDiff) -> Box<ItemData> {
        let built_rows = build_rows(RowsView {
            diff: &diff,
            upgrades: &HashMap::new(),
            comments: None,
            options: RowOptions {
                mode: ViewMode::Unified,
                file_ix: 0,
                comments_visible: true,
                comment_wrap: COMMENT_WRAP_CHARS,
            },
        });
        Box::new(ItemData::new(
            None,
            diff,
            ViewMode::Unified,
            0,
            built_rows,
            None,
            None,
        ))
    }

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

    mod markdown_hosts {
        include!("markdown_tests.rs");
        include!("image_viewer_tests.rs");
        include!("file_media_tests.rs");
        include!("svg_image_tests.rs");
        include!("video_tests.rs");
    }

    #[gpui::test]
    fn stock_tree_preserves_selection_without_rebuilding_for_badges_or_opening_collapsed_paths(
        cx: &mut gpui::TestAppContext,
    ) {
        let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        let mut item = loop_pr_item(Vec::new());
        let ItemState::Ready(data) = &mut item.state else {
            unreachable!()
        };
        data.diff = two_file_diff();
        data.diff.files[0].new_path = Some("src/a.rs".into());
        data.diff.files[1].new_path = Some("src/b.rs".into());
        data.rebuild_tree();
        install_item(&app, item, cx);
        let draw = |vcx: &mut gpui::VisualTestContext| {
            for _ in 0..3 {
                vcx.update(|window, cx| {
                    window.draw(cx).clear(cx);
                });
                vcx.run_until_parked();
            }
        };
        draw(&mut vcx);
        let state = cx.update(|cx| {
            app.read(cx)
                .active_data()
                .unwrap()
                .tree_view
                .as_ref()
                .unwrap()
                .state
                .clone()
        });
        cx.update(|cx| {
            state.update(cx, |state, cx| {
                state.set_selected_index(state.index_of(&tree::file_id("src/b.rs")), cx);
            })
        });
        vcx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.tree_filter_input
                    .update(cx, |state, cx| state.set_value("b.rs", window, cx));
                cx.notify();
            })
        });
        draw(&mut vcx);
        cx.update(|cx| {
            assert_eq!(
                state.read(cx).selected_item().unwrap().id,
                tree::file_id("src/b.rs")
            )
        });
        vcx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.tree_filter_input
                    .update(cx, |state, cx| state.set_value("", window, cx));
                // Reverse file-array order like a refreshed diff. Identity is path,
                // never the old index or visible position.
                let data = app.active_data_mut().unwrap();
                data.diff.files.swap(0, 1);
                data.rebuild_tree();
                cx.notify();
            })
        });
        draw(&mut vcx);
        cx.update(|cx| {
            assert_eq!(
                state.read(cx).selected_item().unwrap().id,
                tree::file_id("src/b.rs")
            )
        });
        let directory = vcx.debug_bounds("tree-row-0").unwrap().center();
        vcx.update(|window, cx| {
            assert!(
                app.read(cx).focus_handle.is_focused(window),
                "focus before Tree click: {:?}",
                window.focused(cx)
            );
        });
        vcx.simulate_mouse_down(directory, MouseButton::Left, gpui::Modifiers::default());
        vcx.update(|window, cx| {
            assert!(
                app.read(cx).focus_handle.is_focused(window),
                "Tree must not take focus even during a press"
            )
        });
        vcx.simulate_mouse_up(directory, MouseButton::Left, gpui::Modifiers::default());
        draw(&mut vcx);
        cx.update(|cx| {
            assert!(app
                .read(cx)
                .active_data()
                .unwrap()
                .collapsed
                .contains("src"));
            assert!(state
                .read(cx)
                .index_of(&tree::file_id("src/b.rs"))
                .is_none());
            assert_eq!(
                state.read(cx).selected_item().unwrap().id.as_ref(),
                "directory:src"
            );
        });
        cx.update(|cx| {
            app.update(cx, |app, cx| {
                app.active_data_mut().unwrap().file_ix = 1;
                cx.notify();
            })
        });
        draw(&mut vcx);
        cx.update(|cx| {
            assert!(state
                .read(cx)
                .index_of(&tree::file_id("src/a.rs"))
                .is_none());
            assert!(state
                .read(cx)
                .selected_item()
                .unwrap()
                .id
                .starts_with("directory:"));
        });
        vcx.update(|window, cx| {
            assert!(
                app.read(cx).focus_handle.is_focused(window),
                "Tree mouse selection must not create a new keyboard focus route"
            )
        });
    }
    use crate::test_util::{
        add, hunk, queued, rc, row_name, sample_comments, sample_diff, two_file_diff,
    };
    use diff_core::{diff_texts, FileDiff};
    use gpui::Focusable;

    fn sample_commits() -> github::PrCommits {
        github::PrCommits {
            commits: vec![github::PrCommit {
                sha: "abc123456789".into(),
                message: "Fix the build\n\nKeep the full message.".into(),
                author: "alice".into(),
                ci: github::CiState::Fail,
                actions: vec![github::ActionsRun {
                    name: "Build".into(),
                    url: "https://github.com/acme/widgets/actions/runs/123".into(),
                }],
            }],
        }
    }

    const SINCE_PATCH: &str =
        "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-reviewed\n+new\n";
    const FULL_PATCH: &str = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old\n+new\ndiff --git a/earlier.rs b/earlier.rs\nnew file mode 100644\n--- /dev/null\n+++ b/earlier.rs\n@@ -0,0 +1 @@\n+already reviewed\n";

    fn since_review_fake() -> Arc<FakePr> {
        Arc::new(FakePr {
            patch: Some(FULL_PATCH.into()),
            review_changes: std::sync::Mutex::new(github::ReviewChanges::Available(
                github::ReviewBaseline {
                    commit: "reviewed-sha".into(),
                    submitted_at: "2026-03-01T00:00:00Z".into(),
                },
            )),
            comparison_patch: std::sync::Mutex::new(Some(SINCE_PATCH.into())),
            file_contents: HashMap::from([
                (("reviewed-sha".into(), "a.rs".into()), "reviewed\n".into()),
                (("ba5ec0de".into(), "a.rs".into()), "old\n".into()),
                (("dead10cc".into(), "a.rs".into()), "new\n".into()),
            ]),
            ..Default::default()
        })
    }

    /// Opt-in end-to-end check for one file (e.g. main.rs in
    /// Plonq/pr-reviewer#1). Pick a file without newline-only changes, which
    /// our full-content diff deliberately normalizes away.
    #[test]
    #[ignore = "requires gh authentication, network, GANDER_DIFF_PR, and GANDER_DIFF_PATH"]
    fn real_full_pr_upgrade_preserves_patch_counts() {
        use github::GitHubClient as _;
        let client = Arc::new(github::gh_cli::GhCliClient::new());
        let loc = client
            .resolve_pr(&std::env::var("GANDER_DIFF_PR").unwrap())
            .unwrap();
        let patch = diff_core::parse_patch(&client.pr_patch(&loc).unwrap());
        let loaded = fetch_item(client, &Source::Pr(loc), ViewMode::Split, 0).unwrap();
        assert!(loaded.pr_base.is_some());
        assert!(!loaded.upgrades.is_empty());
        let path = std::env::var("GANDER_DIFF_PATH").unwrap();
        let expected = patch
            .files
            .iter()
            .find(|file| file.display_path() == path)
            .unwrap();
        let (ix, actual) = loaded
            .diff
            .files
            .iter()
            .enumerate()
            .find(|(_, file)| file.display_path() == path)
            .unwrap();
        assert!(
            loaded.upgrades.contains_key(&ix),
            "exercise actual blob re-diffing"
        );
        assert_eq!(
            (expected.additions, expected.deletions),
            (actual.additions, actual.deletions),
            "{path}",
        );
    }

    #[test]
    #[ignore = "requires gh authentication, network, GANDER_DIFF_PR/PATH/COMMENT_LINES"]
    fn real_full_pr_left_comments_and_drafts_render() {
        use github::GitHubClient as _;
        let client = Arc::new(github::gh_cli::GhCliClient::new());
        let loc = client
            .resolve_pr(&std::env::var("GANDER_DIFF_PR").unwrap())
            .unwrap();
        let path = std::env::var("GANDER_DIFF_PATH").unwrap();
        // Comma-separated comment-id:line pairs. Explicit expected positions
        // make this check independent of GitHub's misleading outdated flag.
        let expected: Vec<(u64, u64)> = std::env::var("GANDER_DIFF_COMMENT_LINES")
            .unwrap()
            .split(',')
            .map(|pair| {
                let (id, line) = pair.split_once(':').unwrap();
                (id.parse().unwrap(), line.parse().unwrap())
            })
            .collect();
        let mut loaded =
            fetch_item(client.clone(), &Source::Pr(loc.clone()), ViewMode::Split, 0).unwrap();
        let pending = client.pending_review(&loc).unwrap();
        let comments = loaded.comments.as_mut().unwrap();
        comments.replace_pending(pending.as_ref());
        let file_ix = loaded
            .diff
            .files
            .iter()
            .position(|file| file.display_path() == path)
            .unwrap();
        assert!(loaded.upgrades.contains_key(&file_ix));
        for (id, line) in &expected {
            assert!(
                comments.threads[&path][&(github::DiffSide::Left, *line)]
                    .iter()
                    .any(|thread| thread.root.id == *id),
                "missing {id} at {line}"
            );
        }
        for mode in [ViewMode::Unified, ViewMode::Split] {
            let built = build_rows(RowsView {
                diff: &loaded.diff,
                upgrades: &loaded.upgrades,
                comments: Some(comments),
                options: RowOptions {
                    mode,
                    file_ix,
                    comments_visible: true,
                    comment_wrap: COMMENT_WRAP_CHARS,
                },
            });
            for (id, _) in &expected {
                assert!(
                    built.rows.iter().any(|row| matches!(row,
                    Row::CommentHeader { comment_id, .. } if comment_id == id)),
                    "missing row for {id}"
                );
            }
        }
    }

    #[test]
    fn full_pr_upgrades_use_merge_base_not_target_tip_and_keep_left_positions() {
        let patch = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -6 +6 @@\n-old\n+new\n";
        let fake = Arc::new(FakePr {
            patch: Some(patch.into()),
            pr_merge_base: Some("merge-base".into()),
            file_contents: HashMap::from([
                (
                    ("merge-base".into(), "a.rs".into()),
                    "one\ntwo\nthree\nfour\nfive\nold\n".into(),
                ),
                (
                    ("dead10cc".into(), "a.rs".into()),
                    "one\ntwo\nthree\nfour\nfive\nnew\n".into(),
                ),
                (
                    ("ba5ec0de".into(), "a.rs".into()),
                    "unrelated upstream work\n".repeat(100),
                ),
            ]),
            ..Default::default()
        });
        // Also cover fallback from an unavailable incremental scope.
        for since_review in [false, true] {
            let mut item = loop_pr_item(Vec::new());
            let loaded = review_target::fetch_item_scoped(
                fake.clone(),
                &item.source,
                ViewMode::Split,
                0,
                since_review,
            )
            .unwrap();
            assert_eq!(loaded.upgrades.len(), 1, "exercise full-file re-diffing");
            let file = &loaded.diff.files[0];
            assert_eq!((file.additions, file.deletions), (1, 1));
            assert!(file.hunks.iter().flat_map(|h| &h.rows).any(
                |row| matches!(row, diff_core::DiffRow::Removed { old_no: 6, text, .. } if text == "old")
            ));
            item.install(loaded);
            let ItemState::Ready(data) = &item.state else {
                panic!()
            };
            assert_eq!(data.diff_base(), "merge-base");
            // Outside the server patch: explicit LEFT positioning must refer
            // to the same tree used for full-file context, not the target tip.
            let anchor = data
                .selection_anchor(&[(github::DiffSide::Left, 1)])
                .unwrap();
            assert_eq!(anchor.position.unwrap().commit_id, "merge-base");
        }
    }

    #[test]
    fn missing_pr_merge_base_keeps_patch_without_guessing_target_tip() {
        let fake = Arc::new(FakePr {
            fail_merge_base: true,
            file_contents: HashMap::from([
                (
                    ("ba5ec0de".into(), "a.rs".into()),
                    "unrelated upstream\n".repeat(100),
                ),
                (("dead10cc".into(), "a.rs".into()), "new\n".into()),
            ]),
            ..Default::default()
        });
        let mut item = loop_pr_item(Vec::new());
        let loaded = fetch_item(fake, &item.source, ViewMode::Split, 0).unwrap();
        assert!(loaded.upgrades.is_empty());
        assert!(loaded.pr_base.is_none());
        assert_eq!(
            (
                loaded.diff.files[0].additions,
                loaded.diff.files[0].deletions
            ),
            (1, 1)
        );
        item.install(loaded);
        let ItemState::Ready(data) = &item.state else {
            panic!()
        };
        assert_eq!(data.diff_base(), "");
        assert!(data
            .selection_anchor(&[(github::DiffSide::Left, 2)])
            .is_err());
        assert!(data
            .selection_anchor(&[(github::DiffSide::Left, 1)])
            .unwrap()
            .position
            .is_none());
    }

    #[test]
    fn since_review_is_a_real_range_and_upgrades_and_comments_use_that_range() {
        let fake = since_review_fake();
        let mut item = loop_pr_item(Vec::new());
        let full = fetch_item(fake.clone(), &item.source, ViewMode::Split, 0).unwrap();
        assert_eq!(full.diff.files.len(), 2);
        assert!(full.review_base.is_none());
        assert!(
            fake.comparison_requests.lock().unwrap().is_empty(),
            "don't fetch comparison patches for All changes"
        );
        item.install(full);
        item.install(
            review_target::fetch_item_scoped(fake.clone(), &item.source, ViewMode::Split, 0, true)
                .unwrap(),
        );
        let ItemState::Ready(data) = &item.state else {
            panic!()
        };
        assert_eq!(data.diff.files.len(), 1);
        assert_eq!(data.diff_base(), "reviewed-sha");
        assert_eq!(
            *fake.comparison_requests.lock().unwrap(),
            [("reviewed-sha".into(), "dead10cc".into(), "ba5ec0de".into())]
        );
        let upgraded = run_upgrade(
            fake.as_ref(),
            &UpgradeSource::Pr {
                loc: loop_loc(),
                base_oid: data.diff_base().into(),
                head_oid: "dead10cc".into(),
                base_files: data.review_base_files.clone(),
            },
            vec![UpgradeJob {
                file_ix: 0,
                old_path: Some("a.rs".into()),
                new_path: Some("a.rs".into()),
                status: FileStatus::Modified,
            }],
        );
        assert_eq!(upgraded.len(), 1);
        assert!(upgraded[0].hunks.iter().flat_map(|h| &h.rows).any(
            |row| matches!(row, diff_core::DiffRow::Removed { text, .. } if text == "reviewed")
        ));
        for side in [github::DiffSide::Left, github::DiffSide::Right] {
            let anchor = comment_anchor::selection_anchor(
                &data.diff.files[0],
                "dead10cc",
                data.diff_base(),
                data.comment_hunks(),
                &[(side, 1)],
            )
            .unwrap();
            assert_eq!(
                anchor.position.unwrap().commit_id,
                if side == github::DiffSide::Left {
                    "reviewed-sha"
                } else {
                    "dead10cc"
                }
            );
        }
        item.install(fetch_item(fake, &item.source, ViewMode::Split, 0).unwrap());
        let ItemState::Ready(data) = &item.state else {
            panic!()
        };
        assert_eq!(data.diff.files.len(), 2);
        assert_eq!(data.diff_base(), "ba5ec0de");
        assert!(!data.comment_hunks().is_empty());
    }

    #[test]
    fn upstream_adjusted_baseline_survives_upgrades_and_maps_left_comments() {
        let mut fake = since_review_fake();
        Arc::get_mut(&mut fake)
            .unwrap()
            .file_contents
            .insert(("dead10cc".into(), "a.rs".into()), "upstream\nnew\n".into());
        *fake.comparison_patch.lock().unwrap() = Some(github::ReviewComparison {
            patch: "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,2 +1,2 @@\n upstream\n-reviewed\n+new\n".into(),
            base_files: Some(HashMap::from([("a.rs".into(), github::ComparisonBase {
                text: "upstream\nreviewed\n".into(), reviewed_lines: vec![None, Some(1)],
            })])),
        });
        let mut item = loop_pr_item(Vec::new());
        item.install(
            review_target::fetch_item_scoped(fake.clone(), &item.source, ViewMode::Split, 0, true)
                .unwrap(),
        );
        let ItemState::Ready(data) = &item.state else {
            panic!()
        };
        let upgraded = run_upgrade(
            fake.as_ref(),
            &UpgradeSource::Pr {
                loc: loop_loc(),
                base_oid: data.diff_base().into(),
                head_oid: "dead10cc".into(),
                base_files: data.review_base_files.clone(),
            },
            vec![UpgradeJob {
                file_ix: 0,
                old_path: Some("a.rs".into()),
                new_path: Some("a.rs".into()),
                status: FileStatus::Modified,
            }],
        );
        assert_eq!(upgraded.len(), 1);
        assert_eq!((upgraded[0].additions, upgraded[0].deletions), (1, 1));
        let left = data
            .selection_anchor(&[(github::DiffSide::Left, 2)])
            .unwrap();
        assert_eq!(left.line, 1);
        assert_eq!(left.position.unwrap().commit_id, "reviewed-sha");
        assert!(data
            .selection_anchor(&[(github::DiffSide::Left, 1)])
            .is_err());
        assert!(data
            .selection_anchor(&[(github::DiffSide::Left, 1), (github::DiffSide::Left, 2)])
            .is_err());
        let right = data
            .selection_anchor(&[(github::DiffSide::Right, 2)])
            .unwrap();
        assert_eq!(right.line, 2);
        assert_eq!(right.position.unwrap().commit_id, "dead10cc");
        item.install(fetch_item(fake, &item.source, ViewMode::Split, 0).unwrap());
        let ItemState::Ready(data) = &item.state else {
            panic!()
        };
        assert!(data.review_base.is_none());
        assert!(data.review_base_files.is_none());
    }

    #[test]
    fn since_review_unavailable_history_or_patch_falls_back_without_hiding_the_pr() {
        for failure in ["rebase", "history", "patch"] {
            let fake = since_review_fake();
            match failure {
                "rebase" => {
                    *fake.review_changes.lock().unwrap() = github::ReviewChanges::Unavailable(
                        "History rewritten by a rebase or force-push".into(),
                    )
                }
                "history" => fake
                    .fail_review_history
                    .store(true, std::sync::atomic::Ordering::SeqCst),
                _ => *fake.comparison_patch.lock().unwrap() = None,
            }
            let loaded = review_target::fetch_item_scoped(
                fake.clone(),
                &Source::Pr(loop_loc()),
                ViewMode::Split,
                0,
                true,
            )
            .unwrap();
            assert_eq!(loaded.diff.files.len(), 2);
            assert!(loaded.review_base.is_none());
            let github::ReviewChanges::Unavailable(reason) = loaded.review_changes else {
                panic!()
            };
            assert!(!reason.is_empty());
            if failure != "patch" {
                assert!(fake.comparison_requests.lock().unwrap().is_empty());
            }
        }
    }

    #[test]
    fn since_review_keeps_right_threads_but_does_not_reuse_full_pr_left_coordinates() {
        let fake = since_review_fake();
        *fake.comments.lock().unwrap() = vec![
            rc(
                1,
                "a.rs",
                Some("LEFT"),
                Some(1),
                "old revision",
                "alice",
                "2026-03-01T00:00:00Z",
                None,
            ),
            rc(
                2,
                "a.rs",
                Some("RIGHT"),
                Some(1),
                "current revision",
                "alice",
                "2026-03-01T00:00:00Z",
                None,
            ),
        ];
        let mut item = loop_pr_item(Vec::new());
        item.install(
            review_target::fetch_item_scoped(fake, &item.source, ViewMode::Split, 0, true).unwrap(),
        );
        let ItemState::Ready(data) = &mut item.state else {
            panic!()
        };
        assert_eq!(data.discussion.as_ref().unwrap().threads.len(), 2);
        assert!(
            !data.comments.as_ref().unwrap().threads["a.rs"].contains_key(&(CommentSide::Left, 1))
        );
        assert!(
            data.comments.as_ref().unwrap().threads["a.rs"].contains_key(&(CommentSide::Right, 1))
        );
        let pending = github::PendingReview {
            id: 91,
            node_id: "pending".into(),
            body: String::new(),
            comments: vec![github::PendingComment {
                id: 9,
                node_id: "draft".into(),
                path: "a.rs".into(),
                side: CommentSide::Left,
                line: Some(1),
                start_line: None,
                body: "draft".into(),
                in_reply_to_id: None,
            }],
        };
        data.comments
            .as_mut()
            .unwrap()
            .replace_pending(Some(&pending));
        data.filter_scope_comments();
        assert!(
            !data.comments.as_ref().unwrap().threads["a.rs"].contains_key(&(CommentSide::Left, 1))
        );
    }

    #[gpui::test]
    fn since_review_scope_survives_refresh_and_rebase_disables_it(cx: &mut gpui::TestAppContext) {
        let fake = since_review_fake();
        let (window, app) = queue_window(cx, fake.clone());
        cx.update(|cx| app.update(cx, |this, cx| this.open_item(Source::Pr(loop_loc()), cx)));
        cx.run_until_parked();
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        vcx.update(|window, cx| {
            app.update(cx, |this, cx| {
                assert!(this.active_data().unwrap().review_base.is_none());
                this.switch_review_scope(true, window, cx);
            })
        });
        vcx.run_until_parked();
        vcx.update(|window, cx| {
            assert_eq!(
                app.read(cx).active_data().unwrap().diff_base(),
                "reviewed-sha"
            );
            let _ = window.draw(cx);
            app.update(cx, |this, cx| this.refresh(cx));
        });
        vcx.run_until_parked();
        vcx.update(|_, cx| assert!(app.read(cx).active_data().unwrap().review_base.is_some()));
        *fake.review_changes.lock().unwrap() = github::ReviewChanges::Unavailable(
            "History rewritten by a rebase or force-push".into(),
        );
        vcx.update(|_, cx| app.update(cx, |this, cx| this.refresh(cx)));
        vcx.run_until_parked();
        vcx.update(|window, cx| {
            let data = app.read(cx).active_data().unwrap();
            assert!(data.review_base.is_none());
            assert_eq!(data.diff.files.len(), 2);
            assert!(data.review_changes.baseline().is_none());
            let _ = window.draw(cx);
        });
    }

    #[gpui::test]
    fn since_review_new_baseline_can_be_empty_and_switching_back_restores_full_diff(
        cx: &mut gpui::TestAppContext,
    ) {
        let fake = since_review_fake();
        let (window, app) = queue_window(cx, fake.clone());
        cx.update(|cx| app.update(cx, |this, cx| this.open_item(Source::Pr(loop_loc()), cx)));
        cx.run_until_parked();
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        vcx.update(|window, cx| {
            app.update(cx, |this, cx| this.switch_review_scope(true, window, cx))
        });
        vcx.run_until_parked();
        *fake.review_changes.lock().unwrap() =
            github::ReviewChanges::Available(github::ReviewBaseline {
                commit: "dead10cc".into(),
                submitted_at: "2026-03-02T00:00:00Z".into(),
            });
        *fake.comparison_patch.lock().unwrap() = Some(String::new().into());
        vcx.update(|_, cx| {
            app.update(cx, |this, cx| {
                this.refetch_meta(this.items[this.active].id, loop_loc(), cx)
            })
        });
        vcx.run_until_parked();
        vcx.update(|window, cx| {
            let data = app.read(cx).active_data().unwrap();
            assert_eq!(data.diff_base(), "dead10cc");
            assert!(data.diff.files.is_empty());
            let _ = window.draw(cx);
            app.update(cx, |this, cx| this.switch_review_scope(false, window, cx));
        });
        vcx.run_until_parked();
        vcx.update(|_, cx| {
            let data = app.read(cx).active_data().unwrap();
            assert!(data.review_base.is_none());
            assert_eq!(data.diff.files.len(), 2);
        });
    }

    #[gpui::test]
    fn since_review_ignores_superseded_fetches_and_does_not_persist_range_coordinates(
        cx: &mut gpui::TestAppContext,
    ) {
        let fake = since_review_fake();
        let (window, app) = queue_window(cx, fake);
        cx.update(|cx| app.update(cx, |this, cx| this.open_item(Source::Pr(loop_loc()), cx)));
        cx.run_until_parked();
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        let mut expected_gen = 0;
        vcx.update(|_, cx| {
            app.update(cx, |this, cx| {
                let id = this.items[this.active].id;
                let old_gen = this.items[this.active].upgrade_gen;
                this.spawn_fetch_scoped(id, Source::Pr(loop_loc()), ViewMode::Split, true, cx);
                this.spawn_fetch_scoped(id, Source::Pr(loop_loc()), ViewMode::Split, false, cx);
                assert_eq!(
                    this.items[this.active].upgrade_gen,
                    old_gen + 2,
                    "supersede old fetches at fetch start"
                );
                expected_gen = old_gen + 2; // Installing a result does not start another phase.
            })
        });
        vcx.run_until_parked();
        vcx.update(|window, cx| {
            app.update(cx, |this, cx| {
                assert_eq!(this.items[this.active].upgrade_gen, expected_gen);
                assert!(this.active_data().unwrap().review_base.is_none());
                this.switch_review_scope(true, window, cx);
            })
        });
        vcx.run_until_parked();
        vcx.update(|_, cx| {
            app.update(cx, |this, _| {
                let saved = this.store.position(&loop_loc());
                assert!(saved.is_some());
                let data = this.active_data_mut().unwrap();
                assert!(data.review_base.is_some());
                data.cursor = 0;
                data.remember_jump();
                this.persist_positions();
                assert_eq!(
                    this.store.position(&loop_loc()),
                    saved,
                    "incremental positions must not overwrite All changes anchors"
                );
            })
        });
    }

    #[test]
    fn sidebar_width_keeps_both_panes_usable() {
        assert_eq!(sidebar_width(260., 1200.), 260.);
        assert_eq!(sidebar_width(10., 1200.), MIN_SIDEBAR_WIDTH);
        assert_eq!(sidebar_width(900., 1200.), MAX_SIDEBAR_WIDTH);
        assert_eq!(sidebar_width(600., 800.), 480.);
        assert_eq!(sidebar_width(260., 400.), 80.);
        assert_eq!(sidebar_width(260., 300.), 0.);
    }

    #[gpui::test]
    fn sidebar_drag_resizes_clamps_and_remembers_width(cx: &mut gpui::TestAppContext) {
        let store = local_store::LocalStore::in_memory();
        let (window, app) = queue_window_with_store(cx, Arc::new(FakePr::default()), store.clone());
        install_item(&app, loop_pr_item(Vec::new()), cx);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        let draw = |vcx: &mut gpui::VisualTestContext| {
            vcx.update(|window, cx| {
                let _ = window.draw(cx);
            });
        };
        draw(&mut vcx);
        assert_eq!(vcx.debug_bounds("sidebar").unwrap().size.width, px(260.));

        for (delta, requested) in [
            (100., 360.),
            (-1000., MIN_SIDEBAR_WIDTH),
            (2000., MAX_SIDEBAR_WIDTH),
        ] {
            let start = vcx.debug_bounds("sidebar-resize").unwrap().center();
            let end = point(start.x + px(delta), start.y);
            vcx.simulate_mouse_down(start, MouseButton::Left, gpui::Modifiers::default());
            draw(&mut vcx);
            vcx.simulate_mouse_move(end, Some(MouseButton::Left), gpui::Modifiers::default());
            draw(&mut vcx);
            vcx.update(|window, cx| {
                let expected = sidebar_width(requested, window.viewport_size().width.into());
                assert_eq!(app.read(cx).sidebar_width, expected);
                assert!(app.read(cx).drag_anchor.is_none());
            });
            // The release can be outside both the handle and the app window.
            vcx.simulate_mouse_up(end, MouseButton::Left, gpui::Modifiers::default());
            vcx.update(|_, cx| assert!(app.read(cx).sidebar_resize.is_none()));
            draw(&mut vcx);
        }
        let width = vcx.debug_bounds("sidebar").unwrap().size.width;
        vcx.simulate_mouse_move(point(px(400.), px(200.)), None, gpui::Modifiers::default());
        // The former toggle shortcut must leave the permanent sidebar alone.
        vcx.simulate_keystrokes("cmd-b");
        draw(&mut vcx);
        assert_eq!(vcx.debug_bounds("sidebar").unwrap().size.width, width);
        assert_eq!(
            store.setting(SIDEBAR_WIDTH_SETTING),
            Some(serde_json::json!(f32::from(width)))
        );

        let (_, restored) = queue_window_with_store(cx, Arc::new(FakePr::default()), store);
        cx.update(|cx| assert_eq!(restored.read(cx).sidebar_width, f32::from(width)));
    }

    #[test]
    fn detail_fetch_installs_and_caches_commits_but_commits_failure_keeps_the_diff() {
        for commits in [
            Some(sample_commits()),
            Some(github::PrCommits::default()),
            None,
        ] {
            let fake = Arc::new(FakePr::default());
            *fake.commits.lock().unwrap() = commits.clone();
            let mut item = loop_pr_item(Vec::new());
            let loaded = fetch_item(fake.clone(), &item.source, ViewMode::Split, 0).unwrap();
            assert!(!loaded.diff.files.is_empty());
            assert_eq!(loaded.commits, commits);
            assert_eq!(
                loaded.snapshot.as_ref().unwrap().commits,
                commits
                    .as_ref()
                    .map(|commits| serde_json::to_value(commits).unwrap())
            );
            let snapshot = item.install(loaded).unwrap();
            let ItemState::Ready(data) = &item.state else {
                panic!("diff should load")
            };
            assert_eq!(data.commits, commits);
            assert_eq!(
                item.pane,
                ItemPane::Changes,
                "commits must not change the default pane"
            );
            let store = local_store::LocalStore::in_memory();
            store.save_snapshot(&snapshot);
            item.state = ItemState::Loading;
            assert!(item.install_offline_snapshot(&store));
            let ItemState::Ready(data) = &item.state else {
                unreachable!()
            };
            assert_eq!(data.commits, commits);
            assert_eq!(
                fake.commit_fetches
                    .load(std::sync::atomic::Ordering::SeqCst),
                1
            );
        }
    }

    #[test]
    fn viewed_detail_fetch_round_trips_server_states_and_survives_failure() {
        use github::FileViewedState::*;
        let fake = Arc::new(FakePr::default());
        let mut item = loop_pr_item(Vec::new());
        for state in [Viewed, Unviewed, Dismissed] {
            fake.viewed_files
                .lock()
                .unwrap()
                .insert("a.rs".into(), state);
            let loaded = fetch_item(fake.clone(), &item.source, ViewMode::Unified, 0).unwrap();
            assert_eq!(
                loaded.viewed.as_ref().unwrap().as_ref().unwrap().files["a.rs"],
                state
            );
            item.install(loaded);
            let ItemState::Ready(data) = &item.state else {
                panic!()
            };
            assert_eq!(data.viewed.state("a.rs"), Some(state));
            assert!(data.viewed.error.is_none());
        }
        fake.fail_viewed_fetch
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let loaded = fetch_item(fake, &item.source, ViewMode::Unified, 0).unwrap();
        assert!(!loaded.diff.files.is_empty());
        item.install(loaded);
        let ItemState::Ready(data) = &item.state else {
            panic!()
        };
        assert!(data.viewed.state("a.rs").is_none());
        assert!(data
            .viewed
            .error
            .as_ref()
            .unwrap()
            .contains("viewed fetch failed"));
    }

    #[gpui::test]
    fn file_tree_comment_badges_survive_filtering_and_hidden_comments(
        cx: &mut gpui::TestAppContext,
    ) {
        let comment = |id, line, parent| {
            rc(
                id,
                "a.rs",
                Some("RIGHT"),
                line,
                "body",
                "alice",
                "same",
                parent,
            )
        };
        // Use a fresh window per case: GPUI retains old debug bounds even
        // after an element disappears, so absence checks need a clean frame.
        for (query, has_comments, comments_visible) in [
            ("", true, true),
            ("", true, false),
            ("a.rs", true, false),
            ("b.rs", true, false),
            ("", false, true),
        ] {
            let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
            install_item(
                &app,
                loop_pr_item(if has_comments {
                    vec![
                        comment(1, Some(1), None),
                        comment(2, Some(1), Some(1)),
                        comment(3, None, None),
                    ]
                } else {
                    Vec::new()
                }),
                cx,
            );
            let mut vcx = gpui::VisualTestContext::from_window(window, cx);
            vcx.update(|window, cx| {
                app.update(cx, |this, cx| {
                    let data = this.active_data_mut().unwrap();
                    data.comments_visible = comments_visible;
                    data.rebuild_rows_anchored();
                    this.tree_filter_input
                        .update(cx, |input, cx| input.set_value(query, window, cx));
                    cx.notify();
                });
            });
            vcx.run_until_parked();
            vcx.update(|window, cx| {
                let _ = window.draw(cx);
            });
            assert_eq!(
                vcx.debug_bounds("file-comments-0").is_some(),
                has_comments && query != "b.rs"
            );
            assert!(vcx.debug_bounds("file-comments-1").is_none());
        }
    }

    #[gpui::test]
    fn viewed_keyboard_and_far_right_checkboxes_sync_without_opening_files(
        cx: &mut gpui::TestAppContext,
    ) {
        use github::FileViewedState::*;
        let fake = Arc::new(FakePr {
            patch: Some("diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old\n+new\ndiff --git a/b.rs b/b.rs\n--- a/b.rs\n+++ b/b.rs\n@@ -1 +1 @@\n-old\n+new\n".into()),
            ..Default::default()
        });
        let (window, app) = queue_window(cx, fake.clone());
        cx.update(|cx| {
            app.update(cx, |this, cx| {
                this.open_item(Source::Pr(loop_loc()), cx);
            })
        });
        cx.run_until_parked();
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        vcx.update(|_, cx| {
            cx.bind_keys([
                KeyBinding::new("v", ToggleView, Some("ReviewApp")),
                KeyBinding::new("shift-v", ToggleViewed, Some("ReviewApp")),
            ])
        });
        vcx.simulate_keystrokes("shift-v");
        vcx.run_until_parked();
        vcx.update(|window, cx| {
            let data = app.read(cx).active_data().unwrap();
            assert_eq!(data.viewed.state("a.rs"), Some(Viewed));
            assert!(
                data.mode == ViewMode::Split,
                "uppercase V must not switch diff layouts"
            );
            let _ = window.draw(cx);
        });
        let sidebar = vcx.debug_bounds("sidebar").unwrap();
        let checkbox = vcx.debug_bounds("viewed-1").unwrap();
        assert_eq!(
            checkbox.right(),
            sidebar.right() - px(20.),
            "checkbox leaves room for the overlay scrollbar"
        );
        vcx.simulate_mouse_down(
            checkbox.center(),
            MouseButton::Left,
            gpui::Modifiers::default(),
        );
        vcx.simulate_mouse_up(
            checkbox.center(),
            MouseButton::Left,
            gpui::Modifiers::default(),
        );
        vcx.run_until_parked();
        vcx.update(|window, cx| {
            let data = app.read(cx).active_data().unwrap();
            assert_eq!(data.file_ix, 0, "checkbox must not also open b.rs");
            assert_eq!(data.viewed.state("b.rs"), Some(Viewed));
            app.update(cx, |this, cx| {
                this.tree_filter_input
                    .update(cx, |input, cx| input.set_value("b.rs", window, cx))
            });
            let _ = window.draw(cx);
        });
        let checkbox = vcx.debug_bounds("viewed-1").unwrap();
        assert_eq!(
            checkbox.right(),
            sidebar.right() - px(20.),
            "filtered rows also leave room for the overlay scrollbar"
        );
        vcx.simulate_mouse_down(
            checkbox.center(),
            MouseButton::Left,
            gpui::Modifiers::default(),
        );
        vcx.simulate_mouse_up(
            checkbox.center(),
            MouseButton::Left,
            gpui::Modifiers::default(),
        );
        vcx.run_until_parked();
        vcx.update(|_, cx| {
            let data = app.read(cx).active_data().unwrap();
            assert_eq!(data.file_ix, 0);
            assert_eq!(data.viewed.state("b.rs"), Some(Unviewed));
        });
        assert_eq!(
            *fake.viewed_writes.lock().unwrap(),
            [
                ("a.rs".into(), true),
                ("b.rs".into(), true),
                ("b.rs".into(), false)
            ]
        );
        fake.viewed_files
            .lock()
            .unwrap()
            .insert("a.rs".into(), Dismissed);
        vcx.update(|_, cx| app.update(cx, |this, cx| this.refresh(cx)));
        vcx.run_until_parked();
        vcx.update(|_, cx| {
            assert_eq!(
                app.read(cx).active_data().unwrap().viewed.state("a.rs"),
                Some(Dismissed)
            )
        });
    }

    #[gpui::test]
    fn viewed_toggles_are_optimistic_serialized_and_reconcile_failures(
        cx: &mut gpui::TestAppContext,
    ) {
        use github::FileViewedState::*;
        use std::sync::atomic::Ordering::SeqCst;
        for (previous, committed, fetch_failed) in [
            (Unviewed, false, false),
            (Unviewed, true, false),
            (Unviewed, false, true),
            (Viewed, false, false),
            (Viewed, true, false),
            (Viewed, false, true),
        ] {
            let fake = Arc::new(FakePr::default());
            fake.viewed_files
                .lock()
                .unwrap()
                .insert("a.rs".into(), previous);
            let desired = if previous == Viewed { Unviewed } else { Viewed };
            let (_, app) = queue_window(cx, fake.clone());
            let mut item = loop_pr_item(Vec::new());
            item.install(fetch_item(fake.clone(), &item.source, ViewMode::Unified, 0).unwrap());
            install_item(&app, item, cx);
            fake.fail.store(true, SeqCst);
            fake.viewed_commit_then_fail.store(committed, SeqCst);
            fake.fail_viewed_fetch.store(fetch_failed, SeqCst);
            cx.update(|cx| {
                app.update(cx, |this, cx| {
                    this.toggle_current_viewed(cx);
                    let data = this.active_data().unwrap();
                    assert_eq!(
                        data.viewed.state("a.rs"),
                        Some(desired),
                        "optimistic before network returns"
                    );
                    assert!(data.viewed.pending.contains("a.rs"));
                    this.toggle_current_viewed(cx); // ignored while in flight
                    this.refresh(cx); // cannot install a pre-mutation snapshot
                    assert!(!this.active_item().unwrap().reloading);
                })
            });
            cx.run_until_parked();
            cx.update(|cx| {
                let data = app.read(cx).active_data().unwrap();
                assert_eq!(
                    data.viewed.state("a.rs"),
                    Some(if committed { desired } else { previous })
                );
                assert!(data.viewed.pending.is_empty());
                assert!(data
                    .viewed
                    .error
                    .as_ref()
                    .unwrap()
                    .contains("viewed mutation failed"));
                assert_eq!(data.viewed.uncertain.contains("a.rs"), fetch_failed);
            });
            assert_eq!(fake.viewed_writes.lock().unwrap().len(), 1);
            if fetch_failed {
                cx.update(|cx| app.update(cx, |this, cx| this.toggle_current_viewed(cx)));
                cx.run_until_parked();
                assert_eq!(
                    fake.viewed_writes.lock().unwrap().len(),
                    1,
                    "uncertain state is disabled"
                );
            }
            fake.fail.store(false, SeqCst);
            fake.viewed_commit_then_fail.store(false, SeqCst);
            fake.fail_viewed_fetch.store(false, SeqCst);
            cx.update(|cx| {
                app.update(cx, |this, cx| {
                    this.refresh(cx);
                    assert!(this.active_item().unwrap().reloading);
                    this.toggle_current_viewed(cx);
                    assert!(
                        this.active_data().unwrap().viewed.pending.is_empty(),
                        "toggles cannot race an in-flight refresh"
                    );
                })
            });
            cx.run_until_parked();
            cx.update(|cx| {
                let data = app.read(cx).active_data().unwrap();
                assert!(data.viewed.error.is_none());
                assert!(data.viewed.uncertain.is_empty());
            });
        }
    }

    #[gpui::test]
    fn discussion_only_refresh_preserves_cached_commits_without_refetching_them(
        cx: &mut gpui::TestAppContext,
    ) {
        let fake = Arc::new(FakePr::default());
        let store = local_store::LocalStore::in_memory();
        let (window, app) = queue_window_with_store(cx, fake.clone(), store.clone());
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        let mut item = loop_pr_item(Vec::new());
        let loc = match &item.source {
            Source::Pr(loc) => loc.clone(),
            _ => unreachable!(),
        };
        let ItemState::Ready(data) = &mut item.state else {
            unreachable!()
        };
        data.commits = Some(sample_commits());
        install_item(&app, item, cx);
        vcx.update(|_, cx| app.update(cx, |app, cx| app.refetch_comments(0, loc.clone(), cx)));
        vcx.run_until_parked();
        let snapshot = store
            .snapshot(&loc.repo_slug(), loc.number)
            .expect("refresh saved the snapshot");
        assert_eq!(
            snapshot.commits,
            Some(serde_json::to_value(sample_commits()).unwrap())
        );
        assert!(snapshot.discussion.is_some());
        assert_eq!(
            fake.commit_fetches
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }

    #[gpui::test]
    fn titlebar_copies_the_pr_url_next_to_the_github_button(cx: &mut gpui::TestAppContext) {
        let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        for state in ["OPEN", "CLOSED", "MERGED"] {
            for pane in [ItemPane::Conversation, ItemPane::Changes] {
                let mut item = loop_pr_item(Vec::new());
                item.pane = pane;
                let ItemState::Ready(data) = &mut item.state else {
                    unreachable!()
                };
                let meta = data.pr_meta.as_mut().unwrap();
                meta.state = state.into();
                // Use metadata's URL, not a URL reconstructed from the locator.
                let url = format!("https://github.com/another/repo/pull/42?state={state}");
                meta.url = url.clone();
                install_item(&app, item, &mut vcx);
                vcx.update(|_, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string("previous clipboard".into()));
                });
                click_commenting(&mut vcx, "copy-pr-link");
                vcx.update(|_, cx| {
                    assert_eq!(cx.read_from_clipboard().unwrap().text().unwrap(), url);
                });
                let open = vcx.debug_bounds("open-in-browser").unwrap();
                let copy = vcx.debug_bounds("copy-pr-link").unwrap();
                assert_eq!(copy.top(), open.top());
                assert!(copy.right() <= open.left());
            }
        }
        assert!(cx.opened_url().is_none(), "copying must not open a browser");
        click_commenting(&mut vcx, "open-in-browser");
        assert_eq!(
            cx.opened_url().as_deref(),
            Some("https://github.com/another/repo/pull/42?state=MERGED")
        );
    }

    #[gpui::test]
    fn conversation_commits_draw_and_actions_click_opens_the_exact_run_url(
        cx: &mut gpui::TestAppContext,
    ) {
        let fake = Arc::new(FakePr::default());
        let (window, app) = queue_window(cx, fake.clone());
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        let mut item = loop_pr_item(Vec::new());
        item.pane = ItemPane::Conversation;
        let ItemState::Ready(data) = &mut item.state else {
            unreachable!()
        };
        data.pr_meta.as_mut().unwrap().body = "Description before commits.".into();
        data.commits = Some(sample_commits());
        data.discussion = Some(conversation::tests::sample_discussion());
        install_item(&app, item, cx);
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let bounds = vcx
            .debug_bounds("commit-action-0-0-0")
            .expect("Actions link is rendered in the shared scroller");
        let point = bounds.center();
        vcx.simulate_mouse_move(point, None, gpui::Modifiers::default());
        vcx.simulate_mouse_down(point, gpui::MouseButton::Left, gpui::Modifiers::default());
        vcx.simulate_mouse_up(point, gpui::MouseButton::Left, gpui::Modifiers::default());
        vcx.run_until_parked();
        assert_eq!(
            cx.opened_url().as_deref(),
            Some("https://github.com/acme/widgets/actions/runs/123")
        );
        assert_eq!(
            fake.commit_fetches
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "rendering never fetches commits"
        );
        for payload in [None, Some(github::PrCommits::default())] {
            vcx.update(|window, cx| {
                app.update(cx, |app, _| {
                    let ItemState::Ready(data) = &mut app.items[0].state else {
                        unreachable!()
                    };
                    data.commits = payload;
                });
                let _ = window.draw(cx);
            });
        }
    }

    #[gpui::test]
    fn commits_default_to_latest_and_expand_and_collapse_with_real_clicks(
        cx: &mut gpui::TestAppContext,
    ) {
        let fake = Arc::new(FakePr::default());
        let (window, app) = queue_window(cx, fake);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        let mut item = loop_pr_item(Vec::new());
        item.pane = ItemPane::Conversation;
        let ItemState::Ready(data) = &mut item.state else {
            unreachable!()
        };
        data.pr_meta.as_mut().unwrap().body.clear();
        let mut commits = sample_commits();
        let mut latest = commits.commits[0].clone();
        latest.sha = "latest-head".into();
        latest.ci = github::CiState::Pass;
        latest.actions[0].url = "https://github.com/acme/widgets/actions/runs/456".into();
        commits.commits.push(latest);
        data.commits = Some(commits);
        assert!(!data.commits_expanded);
        install_item(&app, item, cx);
        let draw = |vcx: &mut gpui::VisualTestContext| {
            vcx.update(|window, cx| {
                let _ = window.draw(cx);
            })
        };
        let click = |vcx: &mut gpui::VisualTestContext, selector| {
            let point = vcx.debug_bounds(selector).unwrap().center();
            vcx.simulate_mouse_move(point, None, gpui::Modifiers::default());
            vcx.simulate_mouse_down(point, gpui::MouseButton::Left, gpui::Modifiers::default());
            vcx.simulate_mouse_up(point, gpui::MouseButton::Left, gpui::Modifiers::default());
            vcx.run_until_parked();
        };
        draw(&mut vcx);
        assert!(
            vcx.debug_bounds("commit-0-0").is_none(),
            "older failed CI is hidden"
        );
        assert!(
            vcx.debug_bounds("commit-0-1").is_some(),
            "latest CI remains visible"
        );
        click(&mut vcx, "commit-action-0-1-0");
        assert_eq!(
            cx.opened_url().as_deref(),
            Some("https://github.com/acme/widgets/actions/runs/456")
        );
        click(&mut vcx, "commits-toggle-0");
        draw(&mut vcx);
        assert!(vcx.debug_bounds("commit-0-0").is_some());
        assert!(vcx.debug_bounds("commit-0-1").is_some());
        // A full data refresh keeps the user's disclosure choice.
        cx.update(|cx| {
            app.update(cx, |app, _| {
                let loaded = fetch_item(
                    Arc::new(FakePr::default()),
                    &app.items[0].source,
                    ViewMode::Split,
                    0,
                )
                .unwrap();
                app.items[0].install(loaded);
                let ItemState::Ready(data) = &app.items[0].state else {
                    unreachable!()
                };
                assert!(data.commits_expanded);
            })
        });
        draw(&mut vcx);
        click(&mut vcx, "commits-toggle-0");
        cx.update(|cx| {
            let ItemState::Ready(data) = &app.read(cx).items[0].state else {
                unreachable!()
            };
            assert!(!data.commits_expanded);
        });
    }

    #[gpui::test]
    fn inline_comments_render_markdown_without_clipping_in_both_views(
        cx: &mut gpui::TestAppContext,
    ) {
        let body = "- **Bold** and *emphasis*, `inline code`, [a link](https://example.com).\n\n  Another paragraph, café @octocat :tada: #12.\n\n  ```rust\n  fn demo() {}\n  ```\n\n- [x] A task\n\n> A quote\n\n| area | state |\n|---|---|\n| api | done |";
        let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
        let mut item = loop_pr_item(vec![
            rc(
                11,
                "a.rs",
                Some("RIGHT"),
                Some(1),
                body,
                "alice",
                "2026-01-01T00:00:00Z",
                None,
            ),
            rc(
                12,
                "a.rs",
                Some("RIGHT"),
                Some(1),
                "A **formatted** reply.",
                "bob",
                "2026-01-02T00:00:00Z",
                Some(11),
            ),
            rc(
                13,
                "a.rs",
                Some("LEFT"),
                Some(1),
                "Left-side `code`.",
                "carol",
                "2026-01-01T00:00:00Z",
                None,
            ),
        ]);
        let ItemState::Ready(data) = &mut item.state else {
            unreachable!()
        };
        data.diff = diff_core::parse_patch("diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,2 +1,2 @@\n-old\n+new\n tail\n");
        data.set_rows(data.row_build());
        install_item(&app, item, cx);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        let draw = |vcx: &mut gpui::VisualTestContext| {
            for _ in 0..3 {
                vcx.update(|window, cx| {
                    let _ = window.draw(cx);
                });
                vcx.run_until_parked();
            }
        };
        for mode in [ViewMode::Unified, ViewMode::Split] {
            for width in [1500., 850.] {
                vcx.simulate_resize(size(px(width), px(2400.)));
                cx.update(|cx| {
                    app.update(cx, |this, _| {
                        let data = this.active_data_mut().unwrap();
                        data.mode = mode;
                        data.set_rows(data.row_build());
                        let mut scroll = data.scroll.0.borrow_mut();
                        scroll.deferred_scroll_to_item = None;
                        scroll.base_handle.set_offset(point(px(0.), px(0.)));
                    })
                });
                draw(&mut vcx);
                for selector in ["inline-avatar-11", "inline-avatar-12", "inline-avatar-13"] {
                    let avatar = vcx.debug_bounds(selector).expect("comment author avatar");
                    assert!(avatar.size.width > px(0.));
                    assert_eq!(avatar.size.width, avatar.size.height);
                    assert!(avatar.size.height < px(row_height()));
                }
                // Stock Markdown owns list markers; assert the host's measured
                // content and anchor geometry, not a private block widget tree.
                let root = vcx.debug_bounds("inline-text-0-11-0-card").unwrap();
                let content = vcx.debug_bounds("inline-text-0-11-0-content").unwrap();
                assert!(content.size.height > px(row_height() * 5.));
                assert!(content.bottom() <= root.bottom());
                let reply = vcx.debug_bounds("inline-text-0-12-0-card").unwrap();
                assert!(reply.top() >= root.bottom());
                let left = vcx.debug_bounds("inline-text-0-13-0-card").unwrap();
                let (viewport, rows) = cx.update(|cx| {
                    let data = app.read(cx).active_data().unwrap();
                    let source = data
                        .rows
                        .iter()
                        .find_map(|row| match row {
                            Row::CommentBody { text, slot: 0, .. } if text.comment_id == 11 => {
                                Some(&text.source)
                            }
                            _ => None,
                        })
                        .unwrap();
                    assert_eq!(source, body, "wrapping must not alter Markdown source");
                    (data.scroll.0.borrow().base_handle.bounds(), data.rows.len())
                });
                if mode == ViewMode::Split {
                    assert!(root.left() > viewport.center().x);
                    assert!(left.right() <= viewport.center().x);
                }
                assert!(
                    root.right() <= viewport.right(),
                    "Markdown stays inside the viewport"
                );
                draw(&mut vcx);
                assert_eq!(
                    rows,
                    cx.update(|cx| app.read(cx).active_data().unwrap().rows.len()),
                    "layout settles without growing reservations"
                );
            }
        }
        let cursor = cx.update(|cx| app.read(cx).active_data().unwrap().cursor);
        let content = vcx.debug_bounds("inline-text-0-11-0-content").unwrap();
        let marker = point(content.left() + px(8.), content.top() + px(8.));
        vcx.simulate_mouse_down(marker, MouseButton::Left, gpui::Modifiers::default());
        vcx.simulate_mouse_up(marker, MouseButton::Left, gpui::Modifiers::default());
        vcx.run_until_parked();
        assert_eq!(
            cursor,
            cx.update(|cx| app.read(cx).active_data().unwrap().cursor),
            "comment interactions must not select diff code"
        );

        // Scrolling past a block's first slot must not unmount its Markdown.
        vcx.simulate_resize(size(px(850.), px(450.)));
        let first = cx.update(|cx| {
            let data = app.read(cx).active_data().unwrap();
            let first = data
                .rows
                .iter()
                .position(|row| {
                    matches!(row,
                Row::CommentBody { text, slot: 0, .. } if text.comment_id == 11)
                })
                .unwrap();
            data.scroll
                .0
                .borrow()
                .base_handle
                .set_offset(point(px(0.), px(-((first + 2) as f32) * row_height())));
            first
        });
        draw(&mut vcx);
        let card = vcx.debug_bounds("inline-text-0-11-0-card").unwrap();
        let viewport = cx.update(|cx| {
            app.read(cx)
                .active_data()
                .unwrap()
                .scroll
                .0
                .borrow()
                .base_handle
                .bounds()
        });
        assert!(card.top() < viewport.top() && card.bottom() > viewport.top());
        assert!(cx.update(|cx| matches!(
            &app.read(cx).active_data().unwrap().rows[first],
            Row::CommentBody { slot: 0, .. }
        )));

        vcx.simulate_resize(size(px(1100.), px(450.)));
        draw(&mut vcx);
        let card = vcx.debug_bounds("inline-text-0-11-0-card").unwrap();
        assert!(
            card.top() < viewport.top() && card.bottom() > viewport.top(),
            "remeasuring keeps a partially visible comment anchored"
        );

        // Editing a body invalidates the measured source and rendered blocks.
        cx.update(|cx| {
            app.update(cx, |this, _| {
                let data = this.active_data_mut().unwrap();
                data.comments = Some(group_comments(vec![rc(
                    11,
                    "a.rs",
                    Some("RIGHT"),
                    Some(1),
                    "Edited **body**.",
                    "alice",
                    "2026-01-01T00:00:00Z",
                    None,
                )
                .into()]));
                data.set_rows(data.row_build());
                data.scroll
                    .0
                    .borrow()
                    .base_handle
                    .set_offset(point(px(0.), px(0.)));
            })
        });
        draw(&mut vcx);
        assert!(vcx.debug_bounds("inline-text-0-11-0-text").is_some());
        assert!(
            vcx.debug_bounds("inline-text-0-11-0-content")
                .unwrap()
                .size
                .height
                < px(row_height() * 3.)
        );
    }

    #[gpui::test]
    fn comment_plus_tracks_the_painted_row_while_scrolling(cx: &mut gpui::TestAppContext) {
        let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
        let mut item = loop_pr_item(Vec::new());
        let ItemState::Ready(data) = &mut item.state else {
            unreachable!()
        };
        let context: String = (1..=200).map(|line| format!(" line {line}\n")).collect();
        data.diff = diff_core::parse_patch(&format!(
            "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,200 +1,200 @@\n{context}"
        ));
        install_item(&app, item, cx);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        for (mode, side) in [
            (ViewMode::Unified, SelSide::Unified),
            (ViewMode::Split, SelSide::Left),
            (ViewMode::Split, SelSide::Right),
        ] {
            vcx.update(|_, cx| {
                app.update(cx, |this, cx| {
                    let data = this.active_data_mut().unwrap();
                    data.mode = mode;
                    data.set_rows(data.row_build());
                    data.position_restore = None;
                    let mut scroll = data.scroll.0.borrow_mut();
                    scroll.deferred_scroll_to_item = None;
                    scroll.base_handle.set_offset(point(px(0.), px(0.)));
                    cx.notify();
                });
            });
            draw_window(&mut vcx);
            let viewport = vcx.update(|_, cx| {
                app.read(cx)
                    .active_data()
                    .unwrap()
                    .scroll
                    .0
                    .borrow()
                    .base_handle
                    .bounds()
            });
            let pointer = point(
                viewport.left()
                    + viewport.size.width * if side == SelSide::Right { 0.75 } else { 0.25 },
                viewport.top() + px(6.5 * row_height()),
            );
            vcx.simulate_mouse_move(pointer, None, gpui::Modifiers::default());
            draw_window(&mut vcx);
            assert!(vcx.debug_bounds("comment-plus").is_some());

            // No mouse moves between scroll packets: the source row under the
            // pointer changes, but the button must remain on that painted row.
            // Include fractional rows, overshooting the top, and both directions.
            for delta in [-137., -83.5, 61., 10000., -47.25] {
                vcx.simulate_event(gpui::ScrollEvent {
                    position: pointer,
                    delta: gpui::ScrollDelta::Pixels(point(px(0.), px(delta))),
                    ..Default::default()
                });
                // Assert the very first frame, not an eventually-correct redraw.
                vcx.update(|window, cx| {
                    let _ = window.draw(cx);
                });
                let (row, expected_y) = vcx.update(|_, cx| {
                    let this = app.read(cx);
                    let data = this.active_data().unwrap();
                    let scroll = data.scroll.0.borrow();
                    let offset = scroll.base_handle.offset().y;
                    let top = scroll.base_handle.bounds().top();
                    let row = (f32::from(pointer.y - top - offset) / row_height()).floor() as usize;
                    (row, top + offset + px((row as f32 + 0.5) * row_height()))
                });
                let plus = vcx
                    .debug_bounds("comment-plus")
                    .expect("hovered source row has a plus");
                assert!((f32::from(plus.center().y - expected_y)).abs() <= 0.5,
                    "button must match the painted row {row} at {expected_y:?} after scrolling {delta}: {plus:?}");
                if side == SelSide::Right {
                    assert!(plus.left() > viewport.center().x);
                } else {
                    assert!(plus.right() < viewport.center().x);
                }
            }
            let plus = vcx.debug_bounds("comment-plus").unwrap();
            let expected_row = vcx.update(|window, cx| {
                app.update(cx, |this, _| {
                    this.hover_target(plus.center(), window).unwrap().0
                })
            });
            vcx.simulate_click(plus.center(), gpui::Modifiers::default());
            vcx.update(|window, cx| {
                app.update(cx, |this, cx| {
                    assert_eq!(this.composer.as_ref().unwrap().row_ix, expected_row);
                    this.close_composer(window, cx);
                });
            });
            vcx.simulate_mouse_move(point(px(0.), px(0.)), None, gpui::Modifiers::default());
            draw_window(&mut vcx);
            assert!(vcx.debug_bounds("comment-plus").is_none());
        }
    }

    #[gpui::test]
    fn diff_scrolls_over_inline_comments_and_the_composer(cx: &mut gpui::TestAppContext) {
        let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
        let mut item = loop_pr_item(vec![rc(
            11,
            "a.rs",
            Some("RIGHT"),
            Some(1),
            "A **formatted** inline comment.",
            "alice",
            "2026-01-01T00:00:00Z",
            None,
        )]);
        let ItemState::Ready(data) = &mut item.state else {
            unreachable!()
        };
        let context: String = (1..=200).map(|line| format!(" line {line}\n")).collect();
        data.diff = diff_core::parse_patch(&format!("diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,201 +1,201 @@\n{context}-old\n+new\n"));
        install_item(&app, item, cx);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        let draw = |vcx: &mut gpui::VisualTestContext| {
            for _ in 0..3 {
                vcx.update(|window, cx| {
                    let _ = window.draw(cx);
                });
                vcx.run_until_parked();
            }
        };
        let offset = |cx: &gpui::TestAppContext| {
            cx.update(|cx| {
                app.read(cx)
                    .active_data()
                    .unwrap()
                    .scroll
                    .0
                    .borrow()
                    .base_handle
                    .offset()
                    .y
            })
        };
        for (mode, side) in [
            (ViewMode::Unified, SelSide::Unified),
            (ViewMode::Split, SelSide::Left),
            (ViewMode::Split, SelSide::Right),
        ] {
            vcx.update(|_, cx| {
                app.update(cx, |this, cx| {
                    let data = this.active_data_mut().unwrap();
                    data.mode = mode;
                    data.set_rows(data.row_build());
                    data.cursor = 1;
                    data.cursor_side = Some(side);
                    let mut scroll = data.scroll.0.borrow_mut();
                    scroll.deferred_scroll_to_item = None;
                    scroll.base_handle.set_offset(point(px(0.), px(0.)));
                    cx.notify();
                });
            });
            draw(&mut vcx);
            let body = vcx.debug_bounds("inline-text-0-11-0-text").unwrap();
            vcx.simulate_event(gpui::ScrollEvent {
                position: body.center(),
                delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-32.))),
                ..Default::default()
            });
            assert_eq!(offset(cx), px(-32.), "posted Markdown must allow scrolling");

            vcx.dispatch_action(NewComment);
            draw(&mut vcx);
            // Both the card chrome and its empty input must let the diff scroll.
            for over_input in [false, true] {
                let card = vcx.debug_bounds("composer-card").unwrap();
                let at = if over_input {
                    card.center()
                } else {
                    point(card.left() + px(20.), card.top() + px(12.))
                };
                let before = offset(cx);
                vcx.simulate_event(gpui::ScrollEvent {
                    position: at,
                    delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-32.))),
                    ..Default::default()
                });
                assert_eq!(
                    offset(cx),
                    before - px(32.),
                    "composer must allow scrolling"
                );
                draw(&mut vcx);
            }
            let card = vcx.debug_bounds("composer-card").unwrap();
            let at = point(card.left() + px(20.), card.top() + px(12.));
            vcx.simulate_mouse_down(at, MouseButton::Left, gpui::Modifiers::default());
            vcx.simulate_mouse_up(at, MouseButton::Left, gpui::Modifiers::default());
            vcx.update(|window, cx| {
                app.update(cx, |this, cx| {
                    assert!(this.drag_anchor.is_none());
                    assert_eq!(this.active_data().unwrap().cursor, 1);
                    assert!(this.active_data().unwrap().selection.is_none());
                    this.close_composer(window, cx);
                });
            });
        }
    }

    #[gpui::test]
    fn diff_search_keyboard_cycle_wrap_and_escape_restore(cx: &mut gpui::TestAppContext) {
        let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
        cx.update(|cx| cx.bind_keys(search::keybindings()));
        let diff = diff_core::parse_patch(&format!(
            "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1,200 +1,200 @@\n{}\ndiff --git a/other.txt b/other.txt\n--- a/other.txt\n+++ b/other.txt\n@@ -1 +1 @@\n-old\n+needle\n",
            (1..=200).map(|n| if n == 20 || n == 150 { format!(" needle {n}\n") } else { format!(" line {n} {}\n", "x".repeat(2000)) }).collect::<String>().trim_end()
        ));
        let mut item = loop_pr_item(Vec::new());
        item.state = ItemState::Ready(file_mode_data(diff));
        install_item(&app, item, cx);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        let prior = vcx.update(|window, cx| {
            let _ = window.draw(cx);
            app.update(cx, |app, _| {
                let data = app.active_data_mut().unwrap();
                data.scroll.0.borrow_mut().deferred_scroll_to_item = None;
                data.position_restore = None;
                data.scroll
                    .0
                    .borrow()
                    .base_handle
                    .set_offset(point(px(-45.), px(-60.375 * row_height())));
            });
            let _ = window.draw(cx);
            app.read(cx)
                .active_data()
                .unwrap()
                .scroll
                .0
                .borrow()
                .base_handle
                .offset()
        });
        vcx.simulate_keystrokes("cmd-f");
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        assert!(vcx.debug_bounds("diff-search").is_some());
        // Plain n/p remain text, rather than invoking the diff navigation keys.
        vcx.simulate_input("needle");
        let active_match = |cx: &App| {
            let data = app.read(cx).active_data().unwrap();
            let search = data.search.as_ref().unwrap();
            let highlights: Vec<_> = (0..data.rows.len())
                .flat_map(|row| {
                    search
                        .highlights(row, SelSide::Unified, cx)
                        .into_iter()
                        .map(move |(_, color)| (row, color))
                })
                .collect();
            assert_eq!(data.file_ix, 0, "cmd-f stays in the current file");
            assert_eq!(highlights.len(), 2);
            let active_color = theme::search_current_bg(cx);
            (
                data.file_ix,
                highlights
                    .into_iter()
                    .find(|(_, color)| *color == active_color)
                    .unwrap()
                    .0,
            )
        };
        let first = vcx.update(|_, cx| active_match(cx));
        vcx.simulate_keystrokes("enter");
        let second = vcx.update(|_, cx| active_match(cx));
        assert!(second > first);
        vcx.simulate_keystrokes("enter");
        assert_eq!(vcx.update(|_, cx| active_match(cx)), first);
        vcx.simulate_keystrokes("shift-enter");
        assert_eq!(vcx.update(|_, cx| active_match(cx)), second);
        // Refocusing the existing search must not overwrite its original anchor.
        vcx.simulate_keystrokes("cmd-f escape");
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
            let data = app.read(cx).active_data().unwrap();
            assert!(data.search.is_none());
            assert_eq!(data.scroll.0.borrow().base_handle.offset(), prior);
            assert!(app.read(cx).focus_handle.is_focused(window));
        });
        // Restoring a wrapped continuation must not snap to the source line's
        // first visual row. Keep the fractional row offset as well.
        let wrapped_prior = vcx.update(|window, cx| {
            app.update(cx, |app, _| {
                let data = app.active_data_mut().unwrap();
                data.preferences.soft_wrap = Some(true);
                data.rebuild_presentation();
            });
            let _ = window.draw(cx);
            app.update(cx, |app, _| {
                let data = app.active_data_mut().unwrap();
                let row = data
                    .rows
                    .iter()
                    .position(|r| matches!(r, Row::Continuation(_)))
                    .unwrap();
                data.position_restore = None;
                data.scroll.0.borrow_mut().deferred_scroll_to_item = None;
                data.scroll
                    .0
                    .borrow()
                    .base_handle
                    .set_offset(point(px(0.), px(-(row as f32 + 0.375) * row_height())));
                data.scroll.0.borrow().base_handle.offset()
            })
        });
        vcx.simulate_keystrokes("cmd-f");
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        vcx.simulate_input("needle");
        vcx.simulate_keystrokes("escape");
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
            assert_eq!(
                app.read(cx)
                    .active_data()
                    .unwrap()
                    .scroll
                    .0
                    .borrow()
                    .base_handle
                    .offset(),
                wrapped_prior
            );
        });
    }

    #[gpui::test]
    fn diff_search_seeds_selected_text_on_open_and_refocus(cx: &mut gpui::TestAppContext) {
        let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
        cx.update(|cx| cx.bind_keys(search::keybindings()));
        let diff = diff_core::parse_patch("diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-old é世界 old\n+new é世界 tail\ndiff --git a/b.txt b/b.txt\n--- a/b.txt\n+++ b/b.txt\n@@ -1 +1 @@\n-old\n+elsewhere\n");
        let mut item = loop_pr_item(Vec::new());
        item.state = ItemState::Ready(file_mode_data(diff));
        install_item(&app, item, cx);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        let select = |side, prefix: &str, start, end, data: &mut ItemData| {
            let row = data
                .rows
                .iter()
                .position(|row| {
                    selection::row_side_text(row, side).is_some_and(|text| text.starts_with(prefix))
                })
                .unwrap();
            data.selection = Some(Selection {
                side,
                anchor: RowCol { row, col: end },
                head: RowCol { row, col: start },
            });
        };
        let assert_query = |query: &str, count, cx: &App| {
            let data = app.read(cx).active_data().unwrap();
            let search = data.search.as_ref().unwrap();
            let hits: Vec<_> = (0..data.rows.len())
                .flat_map(|row| {
                    [SelSide::Unified, SelSide::Left, SelSide::Right]
                        .into_iter()
                        .flat_map(move |side| {
                            search
                                .highlights(row, side, cx)
                                .into_iter()
                                .map(move |(range, _)| {
                                    &selection::row_side_text(&data.rows[row], side).unwrap()[range]
                                })
                        })
                })
                .collect();
            assert_eq!(hits, vec![query; count]);
        };
        for side in [SelSide::Unified, SelSide::Left, SelSide::Right] {
            vcx.update(|window, cx| {
                app.update(cx, |app, _| {
                    let data = app.active_data_mut().unwrap();
                    data.position_restore = None;
                    data.scroll.0.borrow_mut().deferred_scroll_to_item = None;
                    data.mode = if side == SelSide::Unified {
                        ViewMode::Unified
                    } else {
                        ViewMode::Split
                    };
                    data.rebuild_presentation();
                    select(
                        side,
                        if side == SelSide::Left { "old" } else { "new" },
                        4,
                        7,
                        data,
                    );
                });
                let _ = window.draw(cx);
            });
            vcx.simulate_keystrokes("cmd-f");
            vcx.update(|_, cx| assert_query("é世界", 2, cx));
            // Typing appends to the autofilled input, proving its value and focus
            // agree with the search index (not just an invisible seeded query).
            vcx.simulate_input(" tail");
            vcx.update(|_, cx| assert_query("é世界 tail", 1, cx));
            vcx.simulate_keystrokes("cmd-shift-f");
            vcx.update(|_, cx| assert_query("é世界 tail", 1, cx));
            vcx.update(|window, cx| {
                app.update(cx, |app, cx| {
                    let data = app.active_data_mut().unwrap();
                    select(
                        if side == SelSide::Unified {
                            side
                        } else {
                            SelSide::Right
                        },
                        "new",
                        8,
                        12,
                        data,
                    );
                    window.focus(&app.focus_handle, cx);
                });
            });
            vcx.simulate_keystrokes("cmd-shift-f");
            vcx.update(|_, cx| assert_query("tail", 1, cx));
            vcx.update(|_, cx| {
                app.update(cx, |app, _| {
                    let data = app.active_data_mut().unwrap();
                    select(
                        if side == SelSide::Unified {
                            side
                        } else {
                            SelSide::Right
                        },
                        "new",
                        0,
                        0,
                        data,
                    );
                })
            });
            vcx.simulate_keystrokes("cmd-f");
            vcx.update(|_, cx| assert_query("tail", 1, cx));
            vcx.simulate_keystrokes("escape");
            vcx.update(|window, cx| {
                let _ = window.draw(cx);
            });
        }
    }

    #[gpui::test]
    fn whole_diff_search_navigates_scopes_and_restores_original_file(
        cx: &mut gpui::TestAppContext,
    ) {
        let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
        cx.update(|cx| cx.bind_keys(search::keybindings()));
        let diff = diff_core::parse_patch("diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,2 @@\n needle shared\n-old\n+new\ndiff --git a/b.txt b/b.txt\n--- a/b.txt\n+++ b/b.txt\n@@ -1 +1 @@\n-old\n+needle added\n");
        let mut item = loop_pr_item(Vec::new());
        item.state = ItemState::Ready(file_mode_data(diff));
        install_item(&app, item, cx);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        for mode in [ViewMode::Unified, ViewMode::Split] {
            vcx.update(|window, cx| {
                app.update(cx, |app, _| {
                    let data = app.active_data_mut().unwrap();
                    data.position_restore = None;
                    data.scroll.0.borrow_mut().deferred_scroll_to_item = None;
                    data.mode = mode;
                    data.preferences.hide_whitespace = Some(true);
                    data.preferences.collapse_unchanged = Some(true);
                    data.rebuild_presentation();
                });
                let _ = window.draw(cx);
            });
            vcx.simulate_keystrokes("cmd-shift-f");
            vcx.update(|window, cx| {
                let _ = window.draw(cx);
            });
            vcx.simulate_input("needle");
            let assert_result = |file, cx: &App| {
                let data = app.read(cx).active_data().unwrap();
                assert_eq!(data.file_ix, file);
                let search = data.search.as_ref().unwrap();
                assert!((0..data.rows.len()).any(|row| {
                    [SelSide::Unified, SelSide::Left, SelSide::Right]
                        .into_iter()
                        .any(|side| !search.highlights(row, side, cx).is_empty())
                }));
            };
            vcx.update(|_, cx| assert_result(0, cx));
            // Shared split context counts once; the next result is in b.txt.
            vcx.simulate_keystrokes("enter");
            vcx.update(|_, cx| assert_result(1, cx));
            vcx.simulate_keystrokes("enter");
            vcx.update(|_, cx| assert_result(0, cx));
            vcx.simulate_keystrokes("shift-enter");
            vcx.update(|_, cx| assert_result(1, cx));
            vcx.update(|window, cx| {
                app.update(cx, |app, cx| {
                    app.set_search_scope(search::SearchScope::ChangedLines, window, cx);
                    // Row rebuilds must preserve both the source result and search.
                    let data = app.active_data_mut().unwrap();
                    data.preferences.soft_wrap = Some(true);
                    data.rebuild_presentation();
                })
            });
            vcx.simulate_keystrokes("enter");
            vcx.update(|_, cx| assert_result(1, cx));
            // Current-file search can also be selected from whole-diff search.
            vcx.update(|window, cx| {
                app.update(cx, |app, cx| {
                    app.set_search_scope(search::SearchScope::CurrentFile, window, cx);
                })
            });
            vcx.simulate_keystrokes("cmd-f enter");
            vcx.update(|_, cx| assert_result(1, cx));
            // The whole-diff shortcut switches back to searching all files.
            vcx.simulate_keystrokes("cmd-shift-f enter");
            vcx.update(|_, cx| assert_result(0, cx));
            vcx.simulate_keystrokes("escape");
            vcx.update(|window, cx| {
                let _ = window.draw(cx);
                let data = app.read(cx).active_data().unwrap();
                assert_eq!(data.file_ix, 0);
                assert!(data.search.is_none());
                assert!(
                    data.file_positions.is_empty(),
                    "search must not overwrite file memory"
                );
                assert!(data.preferences.hide_whitespace.unwrap());
                assert!(data.preferences.collapse_unchanged.unwrap());
                assert!(data.mode == mode);
            });
        }
    }

    #[gpui::test]
    fn diff_search_restores_folds_and_does_not_leak_across_files_or_panes(
        cx: &mut gpui::TestAppContext,
    ) {
        let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
        cx.update(|cx| cx.bind_keys(search::keybindings()));
        let mut item = loop_pr_item(Vec::new());
        let ItemState::Ready(data) = &mut item.state else {
            unreachable!()
        };
        let other = diff_core::parse_patch("diff --git a/other.rs b/other.rs\n--- a/other.rs\n+++ b/other.rs\n@@ -1 +1 @@\n-old\n+new\n").files.pop().unwrap();
        data.diff.files.push(other);
        let lines: Vec<SharedString> = (1..=100).map(|n| format!("line {n}").into()).collect();
        data.upgrades.insert(
            0,
            FileUpgrade {
                new_lines: lines,
                old_spans: vec![],
                new_spans: vec![],
            },
        );
        data.preferences.collapse_unchanged = Some(true);
        data.rebuild_presentation();
        install_item(&app, item, cx);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        vcx.simulate_keystrokes("cmd-f");
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        vcx.simulate_input("line 90");
        vcx.update(|_, cx| {
            let data = app.read(cx).active_data().unwrap();
            assert!(data.preferences.collapse_unchanged.unwrap());
            assert!(!data.rows.iter().any(|r| matches!(r, Row::Fold { .. })));
            assert!((0..data.rows.len()).any(|row| !data
                .search
                .as_ref()
                .unwrap()
                .highlights(row, SelSide::Unified, cx)
                .is_empty()));
        });
        vcx.simulate_keystrokes("escape");
        vcx.update(|_, cx| {
            assert!(app
                .read(cx)
                .active_data()
                .unwrap()
                .rows
                .iter()
                .any(|r| matches!(r, Row::Fold { .. })));
        });
        vcx.simulate_keystrokes("cmd-f");
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        vcx.simulate_input("line");
        vcx.update(|_, cx| {
            app.update(cx, |app, _| {
                let data = app.active_data_mut().unwrap();
                data.open_file(1, None);
                assert!(data.search.is_none());
            })
        });
        vcx.update(|window, cx| {
            app.update(cx, |app, cx| {
                window.focus(&app.focus_handle, cx);
                app.flip_pane(ItemPane::Conversation, cx);
            })
        });
        vcx.simulate_keystrokes("cmd-f");
        vcx.update(|_, cx| assert!(app.read(cx).active_data().unwrap().search.is_none()));
        vcx.update(|_, cx| app.update(cx, |app, cx| app.back_to_queue(cx)));
        vcx.simulate_keystrokes("cmd-f");
        vcx.update(|_, cx| assert!(app.read(cx).active_data().unwrap().search.is_none()));
    }

    #[gpui::test]
    fn toggling_view_keeps_the_scrolled_source_line_not_the_last_hunk(
        cx: &mut gpui::TestAppContext,
    ) {
        let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
        cx.update(|cx| {
            cx.bind_keys([
                KeyBinding::new("n", NextHunk, Some("ReviewApp")),
                KeyBinding::new("p", PrevHunk, Some("ReviewApp")),
                KeyBinding::new("v", ToggleView, Some("ReviewApp")),
            ]);
        });
        let context = |range: std::ops::RangeInclusive<u32>| {
            range.map(|n| format!(" line {n}\n")).collect::<String>()
        };
        let diff = diff_core::parse_patch(&format!(
            "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1,80 +1,80 @@\n-old\n+new\n{}@@ -100,120 +100,120 @@\n-old\n+new\n{}",
            context(2..=80),
            context(101..=219),
        ));
        let mut item = loop_pr_item(Vec::new());
        item.state = ItemState::Ready(file_mode_data(diff));
        install_item(&app, item, cx);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        vcx.update(|window, cx| {
            let focus = app.read(cx).focus_handle.clone();
            window.focus(&focus, cx);
        });
        // Exercise both directions, after both next- and previous-hunk jumps.
        for (navigation, hunk) in [("n", 1), ("p p", 0)] {
            vcx.simulate_keystrokes(navigation);
            let bounds = vcx.update(|window, cx| {
                let _ = window.draw(cx);
                let data = app.read(cx).active_data().unwrap();
                assert_eq!(data.cursor, data.hunk_rows[hunk]);
                data.scroll.0.borrow().base_handle.bounds()
            });
            vcx.simulate_event(gpui::ScrollEvent {
                position: bounds.center(),
                delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-137.))),
                ..Default::default()
            });
            let prior = vcx.update(|window, cx| {
                let _ = window.draw(cx);
                let data = app.read(cx).active_data().unwrap();
                let position = data.review_position().unwrap();
                assert_ne!(position.row, data.cursor, "wheel moved away from the hunk");
                assert!(position.fraction > 0., "retain partial rows too");
                position
            });
            for _ in 0..2 {
                vcx.simulate_keystrokes("v");
                vcx.update(|window, cx| {
                    let _ = window.draw(cx);
                    let data = app.read(cx).active_data().unwrap();
                    let position = data.review_position().unwrap();
                    assert_eq!(position.new_line, prior.new_line);
                    assert_eq!(position.fraction, prior.fraction);
                    // Check the actual painted viewport, not just restore memory.
                    let top =
                        f32::from(-data.scroll.0.borrow().base_handle.offset().y) / row_height();
                    assert_eq!(top.fract(), prior.fraction);
                    assert_eq!(
                        rows::comment_anchor(&data.rows, top.floor() as usize, SelSide::Right),
                        Some((CommentSide::Right, prior.new_line.unwrap() as u64))
                    );
                });
            }
        }
    }

    #[gpui::test]
    fn toggling_view_preserves_a_hunk_jump_before_it_paints(cx: &mut gpui::TestAppContext) {
        let (_, app) = queue_window(cx, Arc::new(FakePr::default()));
        let mut item = loop_pr_item(Vec::new());
        // The second hunk starts on a removed line, so restoration must also
        // work with an old-side-only anchor, not just new-side line numbers.
        item.state = ItemState::Ready(file_mode_data(test_util::sample_diff()));
        install_item(&app, item, cx);
        cx.update(|cx| {
            app.update(cx, |this, cx| {
                let target = this.active_data().unwrap().hunk_rows[1];
                this.jump(target, cx);
                let prior = this.active_data().unwrap().review_position().unwrap();
                assert_eq!(prior.old_line, Some(10));
                assert_eq!(prior.new_line, None);
                for split in [true, false] {
                    this.toggle_view(cx);
                    let data = this.active_data().unwrap();
                    assert_eq!(data.mode == ViewMode::Split, split);
                    assert_eq!(
                        rows::comment_anchor(&data.rows, data.cursor, SelSide::Left),
                        Some((CommentSide::Left, 10))
                    );
                    let position = data.review_position().unwrap();
                    assert_eq!(position.old_line, prior.old_line);
                    assert_eq!(position.fraction, prior.fraction);
                    assert!(data.scroll.0.borrow().deferred_scroll_to_item.is_none());
                }
            });
        });
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

    /// Patch order deliberately differs from both path sorting and tree order.
    fn sidebar_order_diff() -> PrDiff {
        let mut diff = two_file_diff();
        diff.files = [
            "README.md",
            "src/z.rs",
            "src/core/flags/b.rs",
            "src/core/flags/a.rs",
            "src/a.rs",
            "aaa.rs",
        ]
        .into_iter()
        .map(|path| {
            let mut file = two_file_diff().files.remove(0);
            file.old_path = Some(path.into());
            file.new_path = Some(path.into());
            file
        })
        .collect();
        diff
    }

    #[test]
    fn files_open_and_wrap_in_sidebar_order_not_patch_or_path_order() {
        let mut data = file_mode_data(sidebar_order_diff());
        let order = [3, 2, 4, 1, 0, 5];
        assert_eq!(data.file_order().collect::<Vec<_>>(), order);
        assert_eq!(data.file_ix, order[0]);
        assert_eq!(data.cursor, data.hunk_rows[0]);
        assert!(
            matches!(&data.rows[0], Row::FileHeader { path, .. } if path.as_ref() == "src/core/flags/a.rs")
        );
        // A collapsed directory still has to be reviewed, in the same order.
        data.collapsed.insert("src".into());
        for file_ix in order.into_iter().cycle().skip(1).take(order.len() * 2) {
            assert_eq!(data.step_file(1), Some(file_ix));
            data.open_file(file_ix, None);
        }
        for file_ix in order.into_iter().rev().cycle().take(order.len() * 2) {
            assert_eq!(data.step_file(-1), Some(file_ix));
            data.open_file(file_ix, None);
        }
    }

    #[test]
    fn cross_file_hunks_follow_sidebar_order_and_skip_files_without_hunks() {
        let mut diff = sidebar_order_diff();
        diff.files[2].hunks.clear();
        diff.files[0].hunks.clear();
        let mut data = file_mode_data(diff);
        let order = [3, 4, 1, 5];
        for file_ix in order.into_iter().cycle().skip(1).take(order.len() * 2) {
            data.cursor = *data.hunk_rows.last().unwrap();
            assert_eq!(
                data.step_hunk(1),
                Some(HunkNav::File {
                    file_ix,
                    hunk_ix: 0
                })
            );
            data.open_file(file_ix, Some(0));
        }
        for file_ix in order.into_iter().rev().cycle().take(order.len() * 2) {
            data.cursor = data.hunk_rows[0];
            let hunk_ix = data.diff.files[file_ix].hunks.len() - 1;
            assert_eq!(data.step_hunk(-1), Some(HunkNav::File { file_ix, hunk_ix }));
            data.open_file(file_ix, Some(hunk_ix));
        }
        // Starting on a no-hunk file uses its location in the same tree order.
        data.open_file(2, None);
        assert_eq!(data.step_hunk_file(1), Some((4, 0)));
        assert_eq!(data.step_hunk_file(-1), Some((3, 1)));
        for file in &mut data.diff.files {
            file.hunks.clear();
        }
        assert_eq!(data.step_hunk_file(1), None);
        assert_eq!(data.step_hunk_file(-1), None);
        let empty = file_mode_data(PrDiff::default());
        assert_eq!(empty.step_file(1), None);
        assert_eq!(empty.step_hunk_file(1), None);
    }

    #[test]
    fn installing_diffs_uses_sidebar_first_file_unless_restoring_a_position() {
        let fake = Arc::new(FakePr::default());
        for conversation_first in [false, true] {
            let mut item = loop_pr_item(Vec::new());
            item.state = ItemState::Loading;
            if conversation_first {
                item.install_conversation(
                    review_target::LoadedConversation {
                        meta: loop_pr_meta(),
                        discussion: github::PrDiscussion::default(),
                    },
                    ViewMode::Unified,
                );
            }
            let mut loaded = fetch_item(fake.clone(), &item.source, ViewMode::Split, 0).unwrap();
            loaded.diff = sidebar_order_diff();
            let before = "unchanged\n".repeat(30) + "old\n";
            let after = "unchanged\n".repeat(30) + "new\n";
            loaded.diff.files[3].hunks = diff_texts(&before, &after, 3);
            loaded.upgrades.clear();
            loaded.upgrades.insert(
                3,
                FileUpgrade {
                    new_lines: after.lines().map(|line| line.to_owned().into()).collect(),
                    old_spans: Vec::new(),
                    new_spans: Vec::new(),
                },
            );
            item.install(loaded);
            let ItemState::Ready(data) = &mut item.state else {
                unreachable!()
            };
            assert_eq!(data.file_ix, 3);
            assert!(
                data.hunk_rows[0] > 1,
                "full-file context precedes the first change"
            );
            assert_eq!(data.cursor, data.hunk_rows[0]);
            assert_eq!(data.review_position().unwrap().row, data.hunk_rows[0]);
            assert!(
                matches!(&data.rows[0], Row::FileHeader { path, .. } if path.as_ref() == "src/core/flags/a.rs")
            );
            assert!(!data.file_history.can_go(NavigationDirection::Back));
            data.open_file(1, None);
            let position = data.review_position().unwrap();
            let mut diff = sidebar_order_diff();
            diff.files.reverse();
            let mut loaded = fetch_item(fake.clone(), &item.source, ViewMode::Split, 0).unwrap();
            loaded.diff = diff;
            loaded.upgrades.clear();
            item.install(loaded);
            let ItemState::Ready(data) = &mut item.state else {
                unreachable!()
            };
            assert_eq!(data.review_position().unwrap().path, position.path);
            assert_eq!(
                data.step_file(1),
                Some(5),
                "README follows src/z.rs even after patch reordering"
            );
            // If the selected file disappears, fall back to the tree's top leaf.
            let mut diff = sidebar_order_diff();
            diff.files
                .retain(|file| file.display_path() != position.path);
            let mut loaded = fetch_item(fake.clone(), &item.source, ViewMode::Split, 0).unwrap();
            loaded.diff = diff;
            loaded.upgrades.clear();
            item.install(loaded);
            let ItemState::Ready(data) = &item.state else {
                unreachable!()
            };
            assert_eq!(
                data.diff.files[data.file_ix].display_path(),
                "src/core/flags/a.rs"
            );
        }
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
        // Selecting the current file keeps its position.
        data.open_file(1, None);
        assert_eq!(data.file_ix, 1);
        assert!(matches!(data.rows[0], Row::FileHeader { .. }));
        assert_eq!(data.cursor, data.hunk_rows[0]);
    }

    #[test]
    fn file_history_starts_from_the_restored_file_and_ignores_same_file_hunks() {
        let mut data = file_mode_data(two_file_diff());
        assert!(data.restore_review_position(
            local_store::ReviewPosition {
                path: "b.rs".into(),
                ..Default::default()
            },
            true,
        ));
        assert!(!data.file_history.can_go(NavigationDirection::Back));
        data.open_file(0, Some(0));
        data.open_file(0, Some(1));
        assert_eq!(
            data.file_history
                .step(NavigationDirection::Back, "a.rs")
                .as_deref(),
            Some("b.rs")
        );
        assert!(!data.file_history.can_go(NavigationDirection::Back));
    }

    #[test]
    fn file_switching_remembers_positions_but_explicit_hunks_override_them() {
        let mut data = file_mode_data(two_file_diff());
        data.position_restore = None;
        data.scroll.0.borrow_mut().deferred_scroll_to_item = None;
        data.scroll
            .0
            .borrow()
            .base_handle
            .set_offset(point(px(-17.), px(-2.375 * row_height())));
        let first = data.review_position().unwrap();
        data.open_file(1, None);
        assert_eq!(data.cursor, data.hunk_rows[0], "first visit reveals a hunk");
        let second = data.review_position().unwrap();
        // Switch back before the deferred first-visit reveal has painted.
        data.open_file(0, None);
        assert_eq!(data.review_position(), Some(first.clone()));
        data.open_file(0, None);
        assert_eq!(data.review_position(), Some(first));
        data.open_file(1, None);
        assert_eq!(data.review_position(), Some(second));
        data.open_file(0, Some(1));
        assert_eq!(data.cursor, data.hunk_rows[1]);
        let jumped = data.review_position().unwrap();
        data.open_file(1, None);
        data.open_file(0, None);
        assert_eq!(data.review_position(), Some(jumped));
    }

    #[test]
    fn file_viewports_follow_paths_without_reverting_the_current_view_mode() {
        let mut data = file_mode_data(two_file_diff());
        data.position_restore = None;
        data.scroll.0.borrow_mut().deferred_scroll_to_item = None;
        data.scroll
            .0
            .borrow()
            .base_handle
            .set_offset(point(px(-17.), px(-2.375 * row_height())));
        let first = data.review_position().unwrap();
        data.open_file(1, None);
        // Model a refresh that reorders the files and a view-mode change
        // while a.rs is not on screen.
        data.diff.files.swap(0, 1);
        data.file_ix = 0;
        data.mode = ViewMode::Split;
        data.set_rows(data.row_build());
        data.open_file(1, None);
        let restored = data.review_position().unwrap();
        assert!(data.mode == ViewMode::Split);
        assert_eq!(restored.path, first.path);
        assert_eq!(restored.new_line, first.new_line);
        assert_eq!(restored.fraction, first.fraction);
        assert_eq!(restored.horizontal_offset, first.horizontal_offset);
    }

    #[test]
    fn review_positions_follow_paths_and_restore_fractional_scroll_and_mode() {
        let mut data = file_mode_data(two_file_diff());
        data.open_file(1, None);
        data.scroll.0.borrow_mut().deferred_scroll_to_item = None;
        data.position_restore = None;
        data.scroll
            .0
            .borrow()
            .base_handle
            .set_offset(point(px(-17.), px(-1.375 * row_height())));
        let position = data.review_position().unwrap();
        assert_eq!(position.path, "b.rs");
        assert_eq!(position.fraction, 0.375);
        let mut diff = two_file_diff();
        diff.files.swap(0, 1);
        let mut restored = file_mode_data(diff);
        restored.mode = ViewMode::Split;
        restored.set_rows(restored.row_build());
        assert!(restored.restore_review_position(position.clone(), false));
        assert_eq!(restored.file_ix, 0, "file identity is a path, not an index");
        assert!(restored.mode == ViewMode::Unified);
        assert_eq!(restored.review_position(), Some(position));
        assert_eq!(restored.minimap.len(), restored.rows.len());
    }

    #[test]
    fn deferred_hunk_jumps_replace_a_restored_anchor_before_the_next_frame() {
        let mut data = file_mode_data(two_file_diff());
        let position = local_store::ReviewPosition {
            path: "a.rs".into(),
            new_line: Some(1),
            ..Default::default()
        };
        data.restore_review_position(position.clone(), true);
        data.reveal_hunk(Some(1));
        let jumped = data.review_position().unwrap();
        assert_ne!(jumped.new_line, position.new_line);
        assert_eq!(jumped.row, data.hunk_rows[1]);
        // Even after a short file clamps the jump to its top, retain the
        // requested hunk rather than resurrecting the restored anchor.
        data.scroll.0.borrow_mut().deferred_scroll_to_item = None;
        data.scroll
            .0
            .borrow()
            .base_handle
            .set_offset(point(px(0.), px(0.)));
        assert_eq!(data.review_position(), Some(jumped));
        data.open_file(1, None);
        assert_eq!(data.review_position().unwrap().path, "b.rs");
    }

    #[test]
    fn missing_files_and_shortened_files_degrade_without_invalid_rows() {
        let mut data = file_mode_data(two_file_diff());
        let cursor = data.cursor;
        assert!(!data.restore_review_position(
            local_store::ReviewPosition {
                path: "gone.rs".into(),
                ..Default::default()
            },
            false
        ));
        assert_eq!(data.cursor, cursor);
        assert!(data.restore_review_position(
            local_store::ReviewPosition {
                path: "a.rs".into(),
                new_line: Some(u32::MAX),
                row_delta: usize::MAX,
                ..Default::default()
            },
            false
        ));
        assert_eq!(data.cursor, data.rows.len() - 1);
    }

    #[test]
    fn moving_the_viewport_supersedes_a_pending_restore() {
        let mut data = file_mode_data(two_file_diff());
        data.restore_review_position(
            local_store::ReviewPosition {
                path: "a.rs".into(),
                new_line: Some(1),
                ..Default::default()
            },
            true,
        );
        data.scroll
            .0
            .borrow()
            .base_handle
            .set_offset(point(px(0.), px(-2.25 * row_height())));
        let moved = data.review_position().unwrap();
        assert_eq!(moved.row, 2);
        assert_eq!(moved.fraction, 0.25);
        data.restore_review_position(moved.clone(), false);
        assert_eq!(data.review_position(), Some(moved));
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

    #[test]
    fn queue_rows_use_ascending_request_or_creation_time_even_from_cache() {
        let mut rows = Vec::new();
        for (number, created, requested, updated) in [
            (1, "2026-01-01T00:00:00Z", None, "2026-01-09T00:00:00Z"),
            (
                2,
                "2026-01-02T00:00:00Z",
                Some("2026-01-06T00:00:00Z"),
                "2026-01-08T00:00:00Z",
            ),
            (
                3,
                "2026-01-03T00:00:00Z",
                Some("2026-01-05T00:00:00Z"),
                "2026-01-10T00:00:00Z",
            ),
            (
                4,
                "2026-01-04T00:00:00Z",
                Some("2026-01-06T00:00:00Z"),
                "2026-01-07T00:00:00Z",
            ),
            (5, "2026-01-05T00:00:00Z", Some(""), "2026-01-06T00:00:00Z"),
        ] {
            let mut pr = queued(number, "acme/widgets", "alice");
            pr.created_at = created.into();
            pr.review_requested_at = requested.map(str::to_string);
            pr.updated_at = updated.into();
            rows.push(pr);
        }
        rows.reverse(); // Neither the backend nor a cache need already be sorted.
        let mut queue = QueueState::new();
        queue.replace_rows(rows.clone());
        assert_eq!(
            queue.prs.iter().map(|pr| pr.number).collect::<Vec<_>>(),
            vec![3, 2, 4, 1, 5]
        );

        // Cache metadata must distinguish the two modes even though GitHub
        // fetches both using created ascending.
        let store = local_store::LocalStore::in_memory();
        store.cache_queue(&queue.search, &rows);
        queue.replace_rows(store.cached_queue_items());
        assert_eq!(
            queue.prs.iter().map(|pr| pr.number).collect::<Vec<_>>(),
            vec![3, 2, 4, 1, 5]
        );
        queue.search = queue.search.sorted_by(github::QueueSort::CreatedAscending);
        let (sort, order) = queue.search.sort();
        assert!(!store
            .cached_queue()
            .matches(&queue.search.query(), sort, order));
        queue.replace_rows(store.cached_queue_items());
        assert_eq!(
            queue.prs.iter().map(|pr| pr.number).collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5]
        );
    }

    /// A queue-shaped facade for the app-level tests: scripted search
    /// results keyed by what the app asks for, recording every query it
    /// compiled — the observer for the exact chain a chip click runs.
    /// The scripted rows share an org but live in different repositories,
    /// so repository chips can narrow the queue.
    struct FakeQueue {
        asked: std::sync::Mutex<Vec<(String, (&'static str, &'static str))>>,
    }

    impl FakeQueue {
        fn rows(&self, query: &str) -> Vec<github::QueueItem> {
            let rows = if query.contains("reviewed-by:@me") {
                (1..=14)
                    .map(|number| {
                        let mut pr = queued(number, "acme/widgets", "alice");
                        pr.reviewed_at = Some(format!("2026-03-{number:02}T00:00:00Z"));
                        pr
                    })
                    .collect()
            } else {
                vec![
                    queued(1, "acme/widgets", "alice"),
                    queued(2, "acme/gadgets", "bob"),
                ]
            };
            rows.into_iter()
                .filter(|pr| {
                    !query.contains("repo:") || query.contains(&format!("repo:{}", pr.repository))
                })
                .collect()
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
            gpui::component::init(cx);
            theme::apply_ui_theme(cx);
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
            window.focus(&view.read(cx).focus_handle.clone(), cx);
            *stash2.borrow_mut() = Some(view.clone());
            Root::new(view, window, cx)
        });
        let app = stash.borrow().clone().unwrap();
        cx.run_until_parked();
        (window.into(), app)
    }

    fn bind_quick_open(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| cx.bind_keys([KeyBinding::new("cmd-o", OpenInput, None)]));
    }

    fn draw_window(vcx: &mut gpui::VisualTestContext) {
        vcx.update(|window, cx| window.draw(cx).clear(cx));
        vcx.run_until_parked();
    }

    #[gpui::test]
    fn sidebar_belongs_to_review_screen_not_queue(cx: &mut gpui::TestAppContext) {
        let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
        cx.update(|cx| {
            app.update(cx, |app, cx| {
                app.queue
                    .replace_rows(vec![queued(7, "acme/widgets", "alice")]);
                cx.notify();
            });
        });
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        draw_window(&mut vcx);
        assert!(vcx.debug_bounds("sidebar").is_none());
        assert!(vcx.debug_bounds("queue-table").is_some());

        vcx.dispatch_action(QueueOpen);
        draw_window(&mut vcx);
        let sidebar = vcx.debug_bounds("sidebar").unwrap();
        assert!(vcx.debug_bounds("queue-table").is_none());
        vcx.dispatch_action(ShowConversation);
        draw_window(&mut vcx);
        assert_eq!(vcx.debug_bounds("sidebar").unwrap(), sidebar);
        vcx.dispatch_action(ShowChanges);
        draw_window(&mut vcx);
        assert_eq!(vcx.debug_bounds("sidebar").unwrap(), sidebar);

        vcx.dispatch_action(BackToQueue);
        draw_window(&mut vcx);
        assert!(vcx.debug_bounds("sidebar").is_none());
        assert!(vcx.debug_bounds("queue-table").is_some());
        vcx.update(|_, cx| assert_eq!(app.read(cx).items.len(), 1));

        // Re-entering from the queue restores the same review, not a new tab.
        vcx.dispatch_action(QueueOpen);
        draw_window(&mut vcx);
        assert_eq!(vcx.debug_bounds("sidebar").unwrap(), sidebar);
        vcx.update(|_, cx| assert_eq!(app.read(cx).items.len(), 1));
        vcx.simulate_keystrokes("cmd-b");
        draw_window(&mut vcx);
        assert_eq!(vcx.debug_bounds("sidebar").unwrap(), sidebar);

        vcx.dispatch_action(CloseItem);
        draw_window(&mut vcx);
        assert!(vcx.debug_bounds("sidebar").is_none());
        assert!(vcx.debug_bounds("queue-table").is_some());
    }

    #[gpui::test]
    fn titlebar_queue_navigation_preserves_active_review_and_guards_empty_queue(
        cx: &mut gpui::TestAppContext,
    ) {
        let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
        cx.update(|cx| cx.bind_keys([KeyBinding::new("cmd-w", CloseItem, None)]));
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        draw_window(&mut vcx);
        assert!(vcx.debug_bounds("back-to-review").is_none());
        assert!(vcx.debug_bounds("review-queue-navigation").is_none());
        vcx.simulate_keystrokes("cmd-w");
        vcx.update(|_, cx| assert!(app.read(cx).show_queue));

        install_item(&app, loop_pr_item(Vec::new()), cx);
        vcx.update(|_, cx| {
            app.update(cx, |app, cx| {
                let mut second = loop_pr_item(Vec::new());
                second.id = 1;
                second.source = Source::Pr(github::PrLocator {
                    number: 8,
                    ..loop_loc()
                });
                app.items.push(second);
                app.active = 1;
                app.next_id = 2;
                cx.notify();
            });
        });
        let click = |vcx: &mut gpui::VisualTestContext, selector| {
            draw_window(vcx);
            let point = vcx.debug_bounds(selector).unwrap().center();
            vcx.simulate_mouse_move(point, None, gpui::Modifiers::default());
            vcx.simulate_mouse_down(point, MouseButton::Left, gpui::Modifiers::default());
            vcx.simulate_mouse_up(point, MouseButton::Left, gpui::Modifiers::default());
            draw_window(vcx);
        };
        let position = vcx.update(|_, cx| app.read(cx).active_data().unwrap().review_position());
        for pane in [ItemPane::Changes, ItemPane::Conversation] {
            vcx.update(|_, cx| app.update(cx, |app, cx| app.flip_pane(pane, cx)));
            click(&mut vcx, "review-queue-navigation");
            assert!(vcx.debug_bounds("sidebar").is_none());
            assert!(vcx.debug_bounds("back-to-review").is_some());
            click(&mut vcx, "back-to-review");
            assert!(vcx.debug_bounds("sidebar").is_some());
            assert!(vcx.debug_bounds("back-to-review").is_none());
            vcx.update(|window, cx| {
                let app = app.read(cx);
                assert!(!app.show_queue);
                assert_eq!(app.active, 1);
                assert_eq!(app.items.len(), 2);
                assert_eq!(app.active_item().unwrap().pane, pane);
                assert_eq!(app.active_data().unwrap().review_position(), position);
                assert!(app.focus_handle.is_focused(window));
            });
        }
        // Cmd+W dismisses the queue, rather than closing the hidden PR.
        click(&mut vcx, "review-queue-navigation");
        vcx.simulate_keystrokes("cmd-w");
        draw_window(&mut vcx);
        vcx.update(|_, cx| {
            let app = app.read(cx);
            assert!(!app.show_queue);
            assert_eq!(app.items.len(), 2);
            assert_eq!(app.active, 1);
        });
        // On the review screen Cmd+W still closes the active item.
        vcx.simulate_keystrokes("cmd-w cmd-w");
        draw_window(&mut vcx);
        assert!(vcx.debug_bounds("back-to-review").is_none());
        vcx.update(|_, cx| {
            assert!(app.read(cx).show_queue);
            assert!(app.read(cx).items.is_empty());
        });
    }

    #[gpui::test]
    fn quick_open_is_modal_and_dismissal_restores_focus_on_queue(cx: &mut gpui::TestAppContext) {
        let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
        bind_quick_open(cx);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        draw_window(&mut vcx);
        assert!(vcx.debug_bounds("quick-open-dialog").is_none());
        assert!(vcx.debug_bounds("sidebar").is_none());
        vcx.simulate_keystrokes("cmd-o");
        draw_window(&mut vcx);
        assert!(vcx.debug_bounds("quick-open-dialog").is_some());
        assert!(vcx.debug_bounds("sidebar").is_none());
        vcx.update(|window, cx| {
            assert!(app.read(cx).open_input.focus_handle(cx).is_focused(window));
        });
        // Repeating the shortcut must not stack another modal.
        vcx.simulate_keystrokes("cmd-o");
        vcx.simulate_input("draft");
        vcx.simulate_keystrokes("escape");
        draw_window(&mut vcx);
        assert!(vcx.debug_bounds("sidebar").is_none());
        vcx.update(|window, cx| {
            assert!(!window.has_active_dialog(cx));
            let app = app.read(cx);
            assert!(!app.quick_open_visible);
            assert!(app.focus_handle.is_focused(window));
            assert_eq!(app.open_input.read(cx).value().as_str(), "draft");
            assert!(app.items.is_empty());
        });
    }

    #[gpui::test]
    fn quick_open_surfaces_startup_errors_and_can_reopen_after_backdrop_dismissal(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            gpui::component::init(cx);
            theme::apply_ui_theme(cx);
        });
        let stash = Rc::new(RefCell::new(None));
        let stash2 = stash.clone();
        let window = cx.add_window(move |window, cx| {
            let app = cx.new(|cx| {
                ReviewApp::new(
                    Vec::new(),
                    vec!["could not resolve the supplied PR".into()],
                    Arc::new(FakePr::default()),
                    local_store::LocalStore::in_memory(),
                    None,
                    window,
                    cx,
                )
            });
            window.focus(&app.read(cx).focus_handle.clone(), cx);
            *stash2.borrow_mut() = Some(app.clone());
            Root::new(app, window, cx)
        });
        let app = stash.borrow().clone().unwrap();
        let mut vcx = gpui::VisualTestContext::from_window(window.into(), cx);
        bind_quick_open(cx);
        draw_window(&mut vcx);
        assert!(vcx.debug_bounds("quick-open-error").is_some());
        assert!(vcx.debug_bounds("sidebar").is_none());
        vcx.update(|window, cx| {
            let app = app.read(cx);
            assert!(app.quick_open_visible);
            assert!(app.open_input.focus_handle(cx).is_focused(window));
            assert_eq!(
                app.open_error.as_deref(),
                Some("could not resolve the supplied PR")
            );
        });
        let backdrop = point(px(10.), px(200.));
        vcx.simulate_mouse_down(backdrop, MouseButton::Left, gpui::Modifiers::default());
        vcx.simulate_mouse_up(backdrop, MouseButton::Left, gpui::Modifiers::default());
        draw_window(&mut vcx);
        vcx.update(|_, cx| assert!(!app.read(cx).quick_open_visible));
        vcx.simulate_keystrokes("cmd-o");
        draw_window(&mut vcx);
        vcx.update(|_, cx| assert!(app.read(cx).quick_open_visible));
    }

    #[gpui::test]
    fn quick_open_keeps_errors_in_modal_and_opens_or_reactivates_on_enter(
        cx: &mut gpui::TestAppContext,
    ) {
        let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
        bind_quick_open(cx);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        draw_window(&mut vcx);
        vcx.simulate_keystrokes("cmd-o");
        draw_window(&mut vcx);
        vcx.simulate_keystrokes("enter");
        vcx.update(|_, cx| {
            assert!(app.read(cx).quick_open_visible);
            assert!(app.read(cx).open_error.is_none());
        });
        vcx.simulate_input("not-a-pr");
        vcx.simulate_keystrokes("enter");
        draw_window(&mut vcx);
        assert!(vcx.debug_bounds("quick-open-error").is_some());
        vcx.update(|window, cx| {
            let app = app.read(cx);
            assert!(app.quick_open_visible);
            assert!(app.open_input.focus_handle(cx).is_focused(window));
            assert_eq!(app.open_input.read(cx).value().as_str(), "not-a-pr");
            assert!(app.items.is_empty());
        });
        for value in ["acme/widgets#7", "https://github.com/acme/widgets/pull/7"] {
            vcx.update(|window, cx| {
                app.update(cx, |app, cx| {
                    app.open_input
                        .update(cx, |input, cx| input.set_value(value, window, cx));
                });
            });
            vcx.simulate_keystrokes("enter");
            draw_window(&mut vcx);
            assert!(vcx.debug_bounds("sidebar").is_some());
            vcx.update(|window, cx| {
                assert!(!window.has_active_dialog(cx));
                let app = app.read(cx);
                assert!(!app.quick_open_visible);
                assert!(!app.show_queue);
                assert_eq!(
                    app.items.len(),
                    1,
                    "the second open reactivates the same PR"
                );
                assert!(app.open_input.read(cx).value().is_empty());
                assert!(app.open_error.is_none());
                assert!(app.focus_handle.is_focused(window));
            });
            vcx.simulate_keystrokes("cmd-o");
            draw_window(&mut vcx);
        }
        // The explicit Cancel button uses the same dismissal path.
        let point = vcx.debug_bounds("quick-open-cancel").unwrap().center();
        vcx.simulate_mouse_down(point, MouseButton::Left, gpui::Modifiers::default());
        vcx.simulate_mouse_up(point, MouseButton::Left, gpui::Modifiers::default());
        draw_window(&mut vcx);
        vcx.update(|_, cx| assert!(!app.read(cx).quick_open_visible));
    }

    #[gpui::test]
    fn queue_author_names_are_visible_without_avatars_and_stay_bounded(
        cx: &mut gpui::TestAppContext,
    ) {
        let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
        cx.update(|cx| {
            app.update(cx, |app, cx| {
                app.queue.replace_rows(vec![
                    queued(1, "acme/repo", "alice"),
                    queued(2, "acme/repo", "a-very-long-automation-account-name[bot]"),
                ]);
                cx.notify();
            });
        });
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        for (author_id, row_id) in [
            ("queue-author-0", "queue-row-0"),
            ("queue-author-1", "queue-row-1"),
        ] {
            let author = vcx
                .debug_bounds(author_id)
                .expect("the author login is rendered even without an avatar URL");
            let row = vcx.debug_bounds(row_id).unwrap();
            assert!(author.size.width > px(0.));
            assert!(author.size.width <= px(160.));
            assert!(author.size.height > px(0.));
            assert!(row.contains(&author.center()));
            assert!(row.size.height >= author.size.height + px(16.));
        }
    }

    #[gpui::test]
    fn queue_labels_share_the_repository_line_when_they_fit(cx: &mut gpui::TestAppContext) {
        let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
        let mut labeled = queued(2, "acme/repo", "alice");
        labeled.is_draft = true;
        labeled.labels = ["bug", "queue"]
            .map(|name| github::QueueLabel {
                name: name.into(),
                color: "1D76DB".into(),
            })
            .to_vec();
        cx.update(|cx| {
            app.update(cx, |app, cx| {
                app.queue
                    .replace_rows(vec![queued(1, "acme/repo", "alice"), labeled]);
                cx.notify();
            })
        });
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        vcx.simulate_resize(size(px(1440.), px(900.)));
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let repository = vcx.debug_bounds("queue-repository-1").unwrap();
        for label in ["queue-label-1-0", "queue-label-1-1"] {
            let label = vcx.debug_bounds(label).unwrap();
            assert!((label.center().y - repository.center().y).abs() <= px(1.));
            assert!(label.left() > repository.right());
        }
        assert!(
            (vcx.debug_bounds("queue-row-1").unwrap().size.height
                - vcx.debug_bounds("queue-row-0").unwrap().size.height)
                .abs()
                <= px(1.),
            "labels that fit inline must not add a row of height"
        );
    }

    #[test]
    fn diff_toggle_and_comment_action_icons_are_bundled() {
        use gpui_kit_assets::IconName;
        let listed = AppAssets.list("icons/").unwrap();
        for icon in [
            IconName::Square,
            IconName::SquareCheck,
            IconName::Pencil,
            IconName::Trash,
        ] {
            let path = icon.path();
            let svg = AppAssets
                .load(&path)
                .unwrap()
                .unwrap_or_else(|| panic!("missing styling icon: {path}"));
            assert!(std::str::from_utf8(&svg).unwrap().contains("<svg"));
            assert!(listed.contains(&path));
        }
    }

    #[test]
    fn queue_status_icons_are_bundled_in_the_application_assets() {
        use gpui_kit_assets::IconName;
        let listed = AppAssets.list("icons/").unwrap();
        for icon in [
            IconName::Circle,
            IconName::CircleCheck,
            IconName::CircleX,
            IconName::LoaderCircle,
            IconName::Minus,
        ] {
            let path = icon.path();
            let svg = AppAssets
                .load(path.as_ref())
                .unwrap()
                .unwrap_or_else(|| panic!("queue status SVG is missing from AppAssets: {path}"));
            assert!(std::str::from_utf8(&svg).unwrap().contains("<svg"));
            assert!(
                listed.contains(&path),
                "queue status SVG is not listed: {path}"
            );
        }
    }

    #[gpui::test]
    fn queue_polish_keeps_rows_dense_and_status_labels_aligned(cx: &mut gpui::TestAppContext) {
        let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
        cx.update(|cx| {
            app.update(cx, |app, cx| {
                app.queue.replace_rows(
                    [
                        (github::CiState::Pass, github::ReviewState::Awaiting),
                        (github::CiState::Fail, github::ReviewState::ChangesRequested),
                        (github::CiState::Pending, github::ReviewState::Approved),
                        (github::CiState::NoChecks, github::ReviewState::Awaiting),
                    ]
                    .into_iter()
                    .enumerate()
                    .map(|(ix, (ci, review_state))| {
                        let mut pr = queued(ix as u64, "acme/repo", "alice");
                        pr.ci = ci;
                        pr.review_state = review_state;
                        pr.review_requested_at = Some("2026-01-01T00:00:00Z".into());
                        pr
                    })
                    .collect(),
                );
                cx.notify();
            })
        });
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        vcx.simulate_resize(size(px(1440.), px(900.)));
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let awaiting_icon = vcx
            .debug_bounds("queue-review-icon-0")
            .expect("awaiting review has a visible icon");
        let changes_icon = vcx.debug_bounds("queue-review-icon-1").unwrap();
        assert_eq!(awaiting_icon.size, changes_icon.size);
        let first_status = vcx.debug_bounds("queue-ci-label-0").unwrap();
        for (row, title, ci, review) in [
            (
                "queue-row-0",
                "queue-title-0",
                "queue-ci-label-0",
                "queue-review-label-0",
            ),
            (
                "queue-row-1",
                "queue-title-1",
                "queue-ci-label-1",
                "queue-review-label-1",
            ),
            (
                "queue-row-2",
                "queue-title-2",
                "queue-ci-label-2",
                "queue-review-label-2",
            ),
            (
                "queue-row-3",
                "queue-title-3",
                "queue-ci-label-3",
                "queue-review-label-3",
            ),
        ] {
            let row = vcx.debug_bounds(row).unwrap();
            let title = vcx.debug_bounds(title).unwrap();
            let ci = vcx.debug_bounds(ci).unwrap();
            let review = vcx.debug_bounds(review).unwrap();
            assert!(row.size.height <= px(60.), "ordinary rows stay compact");
            assert_eq!(title.top() - row.top(), px(8.));
            assert_eq!(ci.left(), first_status.left());
            assert_eq!(review.left(), first_status.left());
            assert_eq!(ci.size.height, px(18.));
            assert_eq!(review.size.height, px(18.));
        }
        let first_row = vcx.debug_bounds("queue-row-0").unwrap();
        let accent = vcx.debug_bounds("queue-selection-0").unwrap();
        assert_eq!(accent.left(), first_row.left());
        assert_eq!(accent.size.width, px(2.));
        assert!(accent.size.height > px(0.));
        assert!(accent.top() >= first_row.top() + px(8.));
        assert!(accent.bottom() <= first_row.bottom() - px(8.));
        assert!(vcx.debug_bounds("queue-selection-1").is_none());
        // Hover alone must not move the keyboard-selection accent.
        let hover = vcx.debug_bounds("queue-row-1").unwrap().center();
        vcx.simulate_mouse_move(hover, None, gpui::Modifiers::default());
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        assert!(vcx.debug_bounds("queue-selection-0").is_some());
        assert!(vcx.debug_bounds("queue-selection-1").is_none());
        cx.update(|cx| app.update(cx, |app, cx| app.queue_move(1, cx)));
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        assert!(vcx.debug_bounds("queue-selection-0").is_none());
        assert!(vcx.debug_bounds("queue-selection-1").is_some());
    }

    /// The queue is a bounded table, but rows are not bounded-height:
    /// long titles, identities, large counts and every label remain readable.
    #[gpui::test]
    fn queue_table_aligns_columns_and_wraps_content_on_resize(cx: &mut gpui::TestAppContext) {
        let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
        let mut long = queued(
            2,
            &format!("acme/{}", "long-repository-name-".repeat(5)),
            "a-very-long-automation-account-name[bot]",
        );
        long.title = "Keep all of this unusually long pull request title readable without losing important details. ".repeat(5);
        long.is_draft = true;
        long.ci = github::CiState::Fail;
        long.review_state = github::ReviewState::ChangesRequested;
        long.additions = Some(123_456_789);
        long.deletions = Some(987_654_321);
        long.comments = 12345;
        long.labels = (0..5)
            .map(|ix| github::QueueLabel {
                name: if ix == 4 {
                    "an exceptionally long label with meaningful details ".repeat(5)
                } else {
                    format!("label-{ix}")
                },
                color: "1D76DB".into(),
            })
            .collect();
        cx.update(|cx| {
            app.update(cx, |app, cx| {
                app.queue
                    .replace_rows(vec![queued(1, "acme/repo", "alice"), long]);
                cx.notify();
            })
        });
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        let mut wide_title_height = px(0.);
        for width in [1600., 1100.] {
            vcx.simulate_resize(size(px(width), px(2400.)));
            vcx.update(|window, cx| {
                let _ = window.draw(cx);
            });
            let table = vcx.debug_bounds("queue-table").unwrap();
            assert!(table.size.width <= px(QUEUE_MAX_WIDTH));
            assert!(table.size.width >= px(QUEUE_MIN_WIDTH));
            assert!(vcx.debug_bounds("sidebar").is_none());
            assert!((table.center().x - px(width / 2.)).abs() <= px(1.));
            let short = vcx.debug_bounds("queue-row-0").unwrap();
            let long = vcx.debug_bounds("queue-row-1").unwrap();
            let title = vcx.debug_bounds("queue-title-1").unwrap();
            assert!(long.size.height > short.size.height);
            assert!(title.size.height > px(20.));
            assert!(vcx.debug_bounds("queue-author-1").unwrap().size.height > px(18.));
            assert!(vcx.debug_bounds("queue-repository-1").unwrap().size.height > px(18.));
            assert!(vcx.debug_bounds("queue-label-1-4").unwrap().size.height > px(20.));
            assert!(title.right() <= vcx.debug_bounds("queue-author-cell-1").unwrap().left());
            if width == 1600. {
                wide_title_height = title.size.height;
            } else {
                assert!(
                    title.size.height > wide_title_height,
                    "resize must remeasure wrapped rows"
                );
            }
            for (header, first, second) in [
                (
                    "queue-header-author",
                    "queue-author-cell-0",
                    "queue-author-cell-1",
                ),
                ("queue-header-status", "queue-status-0", "queue-status-1"),
                ("queue-header-changes", "queue-changes-0", "queue-changes-1"),
                ("queue-header-age", "queue-age-0", "queue-age-1"),
            ] {
                let header = vcx.debug_bounds(header).unwrap();
                let first = vcx.debug_bounds(first).unwrap();
                let second = vcx.debug_bounds(second).unwrap();
                assert_eq!(header.left(), first.left());
                assert_eq!(header.left(), second.left());
                assert_eq!(header.size.width, second.size.width);
                assert!(second.bottom() <= long.bottom());
                assert!(second.right() <= long.right());
            }
            for label in [
                "queue-label-1-0",
                "queue-label-1-1",
                "queue-label-1-2",
                "queue-label-1-3",
                "queue-label-1-4",
            ] {
                let label = vcx
                    .debug_bounds(label)
                    .expect("all labels are shown, including the fifth");
                assert!(label.left() >= title.left());
                assert!(label.right() <= title.right());
                assert!(label.bottom() <= long.bottom());
            }
        }
        // A same-length refresh must discard old height measurements too.
        cx.update(|cx| {
            app.update(cx, |app, cx| {
                app.queue.replace_rows(vec![
                    queued(1, "acme/repo", "alice"),
                    queued(2, "acme/repo", "bob"),
                ]);
                cx.notify();
            })
        });
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        assert_eq!(
            vcx.debug_bounds("queue-row-0").unwrap().size.height,
            vcx.debug_bounds("queue-row-1").unwrap().size.height
        );
        // Unknown diff size reserves its slot, keeping comments on line two.
        cx.update(|cx| {
            app.update(cx, |app, cx| {
                let mut prs = app.queue.prs.clone();
                prs[1].additions = Some(12);
                prs[1].deletions = Some(4);
                app.queue.replace_rows(prs);
                cx.notify();
            })
        });
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let first_comment = vcx.debug_bounds("queue-comments-0").unwrap();
        let second_comment = vcx.debug_bounds("queue-comments-1").unwrap();
        assert_eq!(
            first_comment.top() - vcx.debug_bounds("queue-row-0").unwrap().top(),
            second_comment.top() - vcx.debug_bounds("queue-row-1").unwrap().top()
        );
    }

    #[gpui::test]
    fn queue_keyboard_reveals_variable_height_rows_and_keeps_headers_fixed(
        cx: &mut gpui::TestAppContext,
    ) {
        let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
        cx.update(|cx| {
            app.update(cx, |app, cx| {
                app.queue.replace_rows(
                    (0..50)
                        .map(|ix| {
                            let mut pr = queued(ix, "acme/repo", "alice");
                            if ix % 2 == 0 {
                                pr.title = "A title that takes more than one line of the queue. "
                                    .repeat(5);
                            }
                            pr
                        })
                        .collect(),
                );
                cx.notify();
            })
        });
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        vcx.simulate_resize(size(px(1440.), px(700.)));
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let header = vcx.debug_bounds("queue-header-author").unwrap();
        for _ in 0..35 {
            cx.update(|cx| app.update(cx, |app, cx| app.queue_move(1, cx)));
            vcx.update(|window, cx| {
                let _ = window.draw(cx);
            });
            cx.update(|cx| {
                let app = app.read(cx);
                let row = app
                    .queue
                    .scroll
                    .bounds_for_item(app.queue.selected)
                    .expect("selected row is measured");
                let viewport = app.queue.scroll.viewport_bounds();
                assert!(row.top() >= viewport.top());
                assert!(row.bottom() <= viewport.bottom());
            });
            assert_eq!(vcx.debug_bounds("queue-header-author").unwrap(), header);
        }
        cx.update(|cx| {
            app.update(cx, |app, cx| {
                let offset = app.queue.scroll.logical_scroll_top();
                app.queue.replace_rows(app.queue.prs.clone());
                assert_eq!(
                    app.queue.scroll.logical_scroll_top(),
                    offset,
                    "refresh keeps the scroll anchor"
                );
                app.queue_run_search(cx);
                assert_eq!(
                    app.queue.scroll.logical_scroll_top().item_ix,
                    0,
                    "new search starts at the top"
                );
            })
        });
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
                ("review-requested", "asc")
            )]
        );

        // Click a repository chip: the search and rows narrow together.
        cx.update(|cx| {
            app.update(cx, |this, cx| {
                this.queue_toggle_filter(github::QueueFilter::repo("acme/gadgets"), cx)
            })
        });
        cx.run_until_parked();
        cx.update(|cx| {
            assert_eq!(
                app.read(cx).queue.search.filters(),
                &[github::QueueFilter::repo("acme/gadgets")]
            );
            assert_eq!(app.read(cx).queue.prs.len(), 1, "the queue must narrow");
            assert_eq!(app.read(cx).queue.prs[0].author, "bob");
        });
        assert_eq!(
            fake.asked.lock().unwrap().last().unwrap(),
            &(
                "is:pr is:open review-requested:@me repo:acme/gadgets".to_string(),
                ("review-requested", "asc")
            )
        );

        // Click the active chip again: the dimension lifts, and the rows
        // come back.
        cx.update(|cx| {
            app.update(cx, |this, cx| {
                this.queue_toggle_filter(github::QueueFilter::repo("acme/gadgets"), cx)
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
                github::QueueSort::CreatedAscending
            )
        });
        assert_eq!(
            fake.asked.lock().unwrap().last().unwrap(),
            &(
                "is:pr is:open review-requested:@me".to_string(),
                ("created", "asc")
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

    #[gpui::test]
    fn queue_sort_toggle_reorders_existing_rows_even_when_refresh_fails(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_window, app) = queue_window(
            cx,
            Arc::new(OfflineQueue {
                asked: std::sync::Mutex::new(0),
            }),
        );
        cx.update(|cx| {
            app.update(cx, |app, cx| {
                let mut older = queued(1, "acme/widgets", "alice");
                older.created_at = "2026-01-01T00:00:00Z".into();
                older.review_requested_at = Some("2026-01-04T00:00:00Z".into());
                let mut newer = queued(2, "acme/widgets", "bob");
                newer.created_at = "2026-01-02T00:00:00Z".into();
                newer.review_requested_at = Some("2026-01-03T00:00:00Z".into());
                app.queue.replace_rows(vec![older, newer]);
                assert_eq!(app.queue.prs[0].number, 2);
                app.queue_toggle_sort(cx);
                assert_eq!(app.queue.prs[0].number, 1);
            })
        });
        cx.run_until_parked();
        cx.update(|cx| {
            app.update(cx, |app, cx| {
                assert!(app.queue.error.is_some());
                assert_eq!(app.queue.prs[0].number, 1);
                assert_eq!(app.restore_queue_search(), app.queue.search);
                app.queue_toggle_sort(cx);
                assert_eq!(app.queue.prs[0].number, 2);
            })
        });
        cx.run_until_parked();
        cx.update(|cx| {
            let app = app.read(cx);
            assert_eq!(app.queue.prs[0].number, 2);
            assert_eq!(app.restore_queue_search(), app.queue.search);
        });
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
        assert_eq!(cached.sort, "review-requested");
        assert_eq!(cached.order, "asc");
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
    /// number — everything ⌘O's open input parses into, derived from
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
                // j to the second row (bob, acme/gadgets#2), Enter to open.
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
                    ("acme", "gadgets", 2)
                );
                // `u`: back to the queue — the item stays open behind the
                // scenes (ctrl-tab reaches it) and the queue keeps the
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
            .filtered_by(github::QueueFilter::repo("acme/gadgets"))
            .sorted_by(github::QueueSort::CreatedAscending);
        store.set_setting(
            "queue_search",
            serde_json::json!({
                "filters": [{"kind": "repo", "value": "acme/gadgets"}],
                "sort": "created_ascending",
            }),
        );
        let app_search = restore_queue_search(store.setting("queue_search"));
        assert_eq!(app_search.query(), search.query());
        assert_eq!(app_search.ordering(), search.ordering());
        assert_eq!(app_search.filters(), search.filters());
    }

    #[test]
    fn legacy_queue_preferences_drop_author_filters_and_updated_sorts() {
        for sort in ["recent", "oldest", "review_requested_ascending"] {
            let search = restore_queue_search(Some(serde_json::json!({
                "filters": [{"kind": "author", "value": "bob"}],
                "sort": sort,
            })));
            assert_eq!(search, github::QueueSearch::review_requested());
        }
    }

    /// A setting the app doesn't understand reads as the default queue:
    /// a corrupt preference must never keep the app from starting.
    #[test]
    fn an_unreadable_queue_search_setting_reads_as_the_default_queue() {
        let default = restore_queue_search(None);
        assert_eq!(default.query(), "is:pr is:open review-requested:@me");
        assert_eq!(
            default.ordering(),
            github::QueueSort::ReviewRequestedAscending
        );
        // A blob of the wrong shape.
        let drifted = restore_queue_search(Some(serde_json::json!("nonsense")));
        assert_eq!(drifted.query(), "is:pr is:open review-requested:@me");
        // A filter kind this build doesn't know is dropped, the rest kept.
        let mixed = restore_queue_search(Some(serde_json::json!({
            "filters": [
                {"kind": "author", "value": "bob"},
                {"kind": "org", "value": "acme"},
                {"kind": "colour", "value": "blue"},
            ],
            "sort": "oldest",
        })));
        assert_eq!(mixed.query(), "is:pr is:open review-requested:@me org:acme");
        assert_eq!(
            mixed.ordering(),
            github::QueueSort::ReviewRequestedAscending
        );
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
            discussion: None,
            commits: None,
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
        let discussion = conversation::tests::sample_discussion();
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
            discussion: Some(serde_json::to_value(&discussion).unwrap()),
            commits: Some(serde_json::to_value(sample_commits()).unwrap()),
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
        assert_eq!(data.discussion.as_ref(), Some(&discussion));
        assert_eq!(data.commits, Some(sample_commits()));
        assert!(
            data.review_position().is_none(),
            "metadata-only offline views have no anchor to overwrite the saved diff position"
        );
        assert!(
            data.expanded_threads.is_empty(),
            "cached resolved threads start collapsed too"
        );
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
    /// (user stories 22, 11). The flip is per item, PRs open onto conversation,
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
            discussion: None,
            commits: None,
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
        cx.update(|cx| {
            app.update(cx, |this, cx| this.open_item(Source::Pr(loc.clone()), cx));
        });
        cx.run_until_parked();

        // PRs open onto conversation, including when the fetch falls back
        // to an offline snapshot. The keyboard actions switch both ways.
        let pane = |cx: &gpui::TestAppContext| cx.update(|cx| app.read(cx).items[0].pane);
        assert_eq!(pane(cx), ItemPane::Conversation);
        cx.update(|cx| assert!(matches!(app.read(cx).items[0].state, ItemState::Ready(_))));

        // The pane paints from the snapshot's body — full GFM plus the
        // expanded @mention, :shortcode:, and #ref — after the fake's
        // failed fetch falls back to the stored snapshot.
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.run_until_parked();
        vcx.dispatch_action(ShowChanges);
        assert_eq!(pane(cx), ItemPane::Changes);

        // Re-opening the item keeps either selected pane, rather than
        // resetting it to the conversation default.
        for expected in [ItemPane::Changes, ItemPane::Conversation] {
            if expected == ItemPane::Conversation {
                vcx.dispatch_action(ShowConversation);
            }
            cx.update(|cx| {
                app.update(cx, |this, cx| this.open_item(Source::Pr(loc.clone()), cx));
            });
            assert_eq!(pane(cx), expected);
        }
        assert_eq!(
            cx.update(|cx| app.read(cx).items.len()),
            1,
            "re-opening forwards the item, never a duplicate"
        );
    }

    #[gpui::test]
    fn new_prs_open_in_conversation_and_local_diffs_in_changes(cx: &mut gpui::TestAppContext) {
        let (_window, app) = queue_window(cx, Arc::new(FakePr::default()));
        cx.update(|cx| {
            app.update(cx, |this, cx| {
                this.open_item(Source::Pr(loop_loc()), cx);
                assert_eq!(this.active_item().unwrap().pane, ItemPane::Conversation);
            });
        });
        cx.run_until_parked();
        cx.update(|cx| {
            let item = app.read(cx).active_item().unwrap();
            assert!(matches!(item.state, ItemState::Ready(_)));
            assert_eq!(item.pane, ItemPane::Conversation);
        });

        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            app.update(cx, |this, cx| {
                this.open_item(
                    Source::Local(git::LocalSource {
                        repo_root: dir.path().to_path_buf(),
                        branch: "feature".into(),
                        base_ref: None,
                        base_label: "HEAD".into(),
                        base_oid: None,
                    }),
                    cx,
                );
                assert_eq!(this.active_item().unwrap().pane, ItemPane::Changes);
            });
        });
    }

    #[gpui::test]
    fn conversation_renders_before_patch_and_keeps_its_view_when_changes_land(
        cx: &mut gpui::TestAppContext,
    ) {
        let (patch_tx, patch_rx) = std::sync::mpsc::channel();
        let fake = Arc::new(FakePr {
            patch_gate: std::sync::Mutex::new(Some(patch_rx)),
            ..Default::default()
        });
        let (tx, rx) = futures::channel::oneshot::channel();
        let client = fake.clone();
        // A real thread lets this test hold the blocking patch request while
        // driving the UI; GPUI's test background executor is single-threaded.
        let fetch = std::thread::spawn(move || {
            review_target::fetch_item_with_conversation(
                client,
                &Source::Pr(loop_loc()),
                ViewMode::Split,
                0,
                false,
                Some(tx),
            )
        });
        let conversation =
            futures::executor::block_on(rx).expect("conversation arrives before the patch");
        assert!(!fetch.is_finished(), "patch must still be loading");
        let (window, app) = queue_window(cx, fake);
        let mut item = loop_pr_item(Vec::new());
        item.state = ItemState::Loading;
        item.pane = ItemPane::Conversation;
        item.reloading = true;
        item.install_conversation(conversation, ViewMode::Split);
        install_item(&app, item, cx);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        vcx.update(|window, cx| {
            let data = app.read(cx).active_data().unwrap();
            assert!(!data.diff_loaded);
            assert!(data.discussion.is_some());
            let _ = window.draw(cx);
        });
        assert!(vcx.debug_bounds("conversation").is_some());
        vcx.dispatch_action(ShowChanges);
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        vcx.dispatch_action(ShowConversation);
        let offset = point(px(0.), px(-42.));
        cx.update(|cx| {
            app.update(cx, |app, _| {
                app.active_data_mut()
                    .unwrap()
                    .conv_scroll
                    .set_offset(offset);
            })
        });

        patch_tx
            .send(Ok(
                "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old\n+new\n"
                    .into(),
            ))
            .unwrap();
        let loaded = fetch.join().unwrap().unwrap();
        cx.update(|cx| {
            app.update(cx, |app, _| {
                let item = &mut app.items[0];
                item.install(loaded);
                item.reloading = false;
                assert_eq!(item.pane, ItemPane::Conversation);
                let ItemState::Ready(data) = &item.state else {
                    panic!("ready")
                };
                assert!(data.diff_loaded);
                assert_eq!(data.diff.files.len(), 1);
                assert_eq!(data.conv_scroll.offset(), offset);
            })
        });
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
    }

    #[gpui::test]
    fn diff_waits_for_full_contents_on_open_and_refresh(cx: &mut gpui::TestAppContext) {
        for refresh in [false, true] {
            let old: String = (1..=20).map(|n| format!("line {n}\n")).collect();
            let new = old.replace("line 10\n", "changed 10\n");
            let patch = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -10 +10 @@\n-line 10\n+changed 10\ndiff --git a/b.rs b/b.rs\n--- a/b.rs\n+++ b/b.rs\n@@ -1 +1 @@\n-old\n+new\n";
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let fake = Arc::new(FakePr {
                patch: Some(patch.into()),
                file_gate: std::sync::Mutex::new(Some((started_tx, release_rx))),
                file_contents: HashMap::from([
                    (("ba5ec0de".into(), "a.rs".into()), old),
                    (("dead10cc".into(), "a.rs".into()), new),
                ]),
                ..Default::default()
            });
            let (tx, rx) = futures::channel::oneshot::channel();
            let client = fake.clone();
            let fetch = std::thread::spawn(move || {
                review_target::fetch_item_with_conversation(
                    client,
                    &Source::Pr(loop_loc()),
                    ViewMode::Unified,
                    0,
                    false,
                    Some(tx),
                )
            });
            let conversation = futures::executor::block_on(rx).unwrap();
            started_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            assert!(
                !fetch.is_finished(),
                "full contents must finish before the diff is published"
            );

            let (window, app) = queue_window(cx, fake);
            let mut item = loop_pr_item(Vec::new());
            let old_view = match &item.state {
                ItemState::Ready(data) => (data.diff.files[0].hunks[0].new_start, data.rows.len()),
                _ => unreachable!(),
            };
            if !refresh {
                item.state = ItemState::Loading;
                item.install_conversation(conversation, ViewMode::Unified);
            }
            item.reloading = true;
            install_item(&app, item, cx);
            let mut vcx = gpui::VisualTestContext::from_window(window, cx);
            vcx.update(|window, cx| {
                let data = app.read(cx).active_data().unwrap();
                assert_eq!(data.diff_loaded, refresh);
                if refresh {
                    assert_eq!(
                        (data.diff.files[0].hunks[0].new_start, data.rows.len()),
                        old_view,
                        "keep the previous diff while refreshing"
                    );
                } else {
                    assert!(
                        data.diff.files.is_empty(),
                        "never show the intermediate patch"
                    );
                }
                let _ = window.draw(cx);
            });

            release_tx.send(()).unwrap();
            let loaded = fetch.join().unwrap().unwrap();
            assert_eq!(loaded.anchor_hunks[0][0].new_start, 10);
            assert_eq!(loaded.diff.files[0].hunks[0].new_start, 7);
            cx.update(|cx| {
                app.update(cx, |app, _| {
                    app.items[0].install(loaded);
                    let data = app.active_data().unwrap();
                    assert!(data.diff_loaded);
                    assert_eq!(data.upgrades[&0].new_lines.len(), 20);
                    assert_eq!(data.anchor_hunks[0][0].new_start, 10);
                    assert_eq!(data.diff.files[0].hunks[0].new_start, 7);
                    assert!(
                        data.rows.iter().any(|row| matches!(
                            row,
                            Row::Line {
                                new_no: Some(20),
                                ..
                            }
                        )),
                        "the first installed view includes full-file context"
                    );
                    assert!(
                        !data.upgrades.contains_key(&1),
                        "missing full contents keep the patch fallback"
                    );
                    assert_eq!(data.diff.files[1].additions, 1);
                })
            });
        }
    }

    #[gpui::test]
    fn patch_failure_keeps_the_fetched_conversation_readable(cx: &mut gpui::TestAppContext) {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Err(github::ClientError::Failed("patch unavailable".into())))
            .unwrap();
        let fake = Arc::new(FakePr {
            patch_gate: std::sync::Mutex::new(Some(rx)),
            ..Default::default()
        });
        let (window, app) = queue_window(cx, fake);
        cx.update(|cx| app.update(cx, |app, cx| app.open_item(Source::Pr(loop_loc()), cx)));
        cx.run_until_parked();
        cx.update(|cx| {
            let item = app.read(cx).active_item().unwrap();
            assert!(!item.reloading);
            assert!(item
                .refresh_error
                .as_ref()
                .unwrap()
                .contains("patch unavailable"));
            let ItemState::Ready(data) = &item.state else {
                panic!("conversation stays ready")
            };
            assert!(!data.diff_loaded);
            assert!(data.discussion.is_some());
            assert_eq!(item.pane, ItemPane::Conversation);
        });
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        assert!(vcx.debug_bounds("conversation").is_some());
        vcx.dispatch_action(ShowChanges);
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
    }

    #[gpui::test]
    fn titlebar_pane_toggle_switches_both_ways_with_real_clicks(cx: &mut gpui::TestAppContext) {
        let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
        let mut item = loop_pr_item(Vec::new());
        let ItemState::Ready(data) = &mut item.state else {
            unreachable!()
        };
        data.open_file(1, None);
        let position = data.review_position();
        install_item(&app, item, cx);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        vcx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.open_input
                    .update(cx, |input, cx| input.focus(window, cx));
                cx.notify();
            });
            let _ = window.draw(cx);
        });

        for expected in [ItemPane::Conversation, ItemPane::Changes] {
            let point = vcx.debug_bounds("pane-toggle").unwrap().center();
            vcx.simulate_mouse_move(point, None, gpui::Modifiers::default());
            vcx.simulate_mouse_down(point, MouseButton::Left, gpui::Modifiers::default());
            vcx.simulate_mouse_up(point, MouseButton::Left, gpui::Modifiers::default());
            vcx.run_until_parked();
            vcx.update(|window, cx| {
                let app = app.read(cx);
                assert_eq!(app.active_item().unwrap().pane, expected);
                assert!(app.focus_handle.is_focused(window));
                let data = app.active_data().unwrap();
                assert_eq!(data.review_position(), position);
                assert!(data.mode == ViewMode::Unified);
                let _ = window.draw(cx);
            });
        }

        // Mouse navigation shares the keyboard actions' state, in both directions.
        vcx.dispatch_action(ShowConversation);
        vcx.update(|window, cx| {
            assert_eq!(
                app.read(cx).active_item().unwrap().pane,
                ItemPane::Conversation
            );
            let _ = window.draw(cx);
        });
        let point = vcx.debug_bounds("pane-toggle").unwrap().center();
        vcx.simulate_mouse_down(point, MouseButton::Left, gpui::Modifiers::default());
        vcx.simulate_mouse_up(point, MouseButton::Left, gpui::Modifiers::default());
        vcx.run_until_parked();
        cx.update(|cx| assert_eq!(app.read(cx).active_item().unwrap().pane, ItemPane::Changes));
    }

    #[gpui::test]
    fn sidebar_file_clicks_show_changes_from_conversation(cx: &mut gpui::TestAppContext) {
        let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
        let mut item = loop_pr_item(Vec::new());
        let mut diff = test_util::two_file_diff();
        for file in &mut diff.files {
            file.old_path = file.old_path.take().map(|path| format!("src/{path}"));
            file.new_path = file.new_path.take().map(|path| format!("src/{path}"));
        }
        item.state = ItemState::Ready(file_mode_data(diff));
        item.pane = ItemPane::Conversation;
        install_item(&app, item, cx);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        // Directory clicks only toggle collapse, leaving the conversation visible.
        for collapsed in [true, false] {
            let point = vcx.debug_bounds("tree-row-0").unwrap().center();
            vcx.simulate_mouse_down(point, MouseButton::Left, gpui::Modifiers::default());
            vcx.simulate_mouse_up(point, MouseButton::Left, gpui::Modifiers::default());
            vcx.run_until_parked();
            vcx.update(|window, cx| {
                let app = app.read(cx);
                assert_eq!(app.active_item().unwrap().pane, ItemPane::Conversation);
                assert_eq!(
                    app.active_data().unwrap().collapsed.contains("src"),
                    collapsed
                );
                let _ = window.draw(cx);
            });
        }

        // Both the regular tree and fuzzy results open changes, including
        // when the selected file is already the diff's current file.
        for query in ["", "src/"] {
            for file_ix in [0, 1, 1] {
                vcx.update(|window, cx| {
                    app.update(cx, |app, cx| {
                        app.flip_pane(ItemPane::Conversation, cx);
                        app.tree_filter_input.update(cx, |input, cx| {
                            input.set_value(query, window, cx);
                            input.focus(window, cx);
                        });
                        cx.notify();
                    });
                    let _ = window.draw(cx);
                });
                let selector = match (query.is_empty(), file_ix) {
                    (true, 0) | (false, 1) => "tree-row-1",
                    (true, _) => "tree-row-2",
                    (false, _) => "tree-row-0",
                };
                let point = vcx.debug_bounds(selector).unwrap().center();
                vcx.simulate_mouse_down(point, MouseButton::Left, gpui::Modifiers::default());
                vcx.simulate_mouse_up(point, MouseButton::Left, gpui::Modifiers::default());
                vcx.run_until_parked();
                vcx.update(|window, cx| {
                    let app = app.read(cx);
                    assert_eq!(app.active_item().unwrap().pane, ItemPane::Changes);
                    assert_eq!(app.active_data().unwrap().file_ix, file_ix);
                    assert!(app.focus_handle.is_focused(window));
                    let _ = window.draw(cx);
                });
            }
        }
    }

    #[gpui::test]
    fn titlebar_pane_toggle_only_appears_for_visible_ready_prs(cx: &mut gpui::TestAppContext) {
        let mut loading = loop_pr_item(Vec::new());
        loading.state = ItemState::Loading;
        let mut failed = loop_pr_item(Vec::new());
        failed.state = ItemState::Failed("failed".into());
        let mut local = loop_pr_item(Vec::new());
        local.source = Source::Local(git::LocalSource {
            repo_root: PathBuf::from("/repo"),
            branch: "feature".into(),
            base_ref: Some("main".into()),
            base_label: "main".into(),
            base_oid: None,
        });
        // Fresh windows avoid gpui's debug-bounds map retaining removed selectors.
        for (item, show_queue, visible) in [
            (None, false, false),
            (Some(loop_pr_item(Vec::new())), true, false),
            (Some(loading), false, false),
            (Some(failed), false, false),
            (Some(local), false, false),
            (Some(loop_pr_item(Vec::new())), false, true),
        ] {
            let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
            if let Some(item) = item {
                install_item(&app, item, cx);
            }
            let mut vcx = gpui::VisualTestContext::from_window(window, cx);
            vcx.update(|_, cx| {
                app.update(cx, |app, cx| {
                    app.show_queue = show_queue;
                    cx.notify();
                });
            });
            vcx.update(|window, cx| {
                let _ = window.draw(cx);
            });
            assert_eq!(vcx.debug_bounds("pane-toggle").is_some(), visible);
        }
    }

    #[gpui::test]
    fn resolved_conversation_threads_expand_and_collapse_through_real_clicks(
        cx: &mut gpui::TestAppContext,
    ) {
        let fake = Arc::new(FakeQueue {
            asked: std::sync::Mutex::new(Vec::new()),
        });
        let (window, app) = queue_window(cx, fake);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        let mut item = loop_pr_item(Vec::new());
        item.pane = ItemPane::Conversation;
        let ItemState::Ready(data) = &mut item.state else {
            unreachable!()
        };
        let mut discussion = conversation::tests::sample_discussion();
        let mut summary = discussion.comments[0].clone();
        summary.body = "Review summary above the inline thread.".into();
        summary.review_state = Some("CHANGES_REQUESTED".into());
        summary.review_id = Some("R1".into());
        discussion.comments.clear();
        discussion.comments.push(summary);
        discussion.threads[0].comments[0].review_id = Some("R1".into());
        // Outside-diff comments have no captured patch. Expanding the grouped
        // stub must render their immutable blob excerpt through the same UI.
        discussion.threads[0].diff_hunk.clear();
        discussion.threads[0].code_context = Some(github::ReviewCodeContext {
            commit_id: "captured-base".into(),
            line: 12,
            start_line: Some(10),
            first_line: 9,
            lines: vec![
                "context".into(),
                "first".into(),
                "second".into(),
                "third".into(),
            ],
        });
        assert!(matches!(
            &conversation::timeline(&discussion)[..],
            [conversation::Entry::Review(_, threads)] if threads.len() == 1
        ));
        data.discussion = Some(discussion);
        install_item(&app, item, cx);
        let expanded = |cx: &gpui::TestAppContext| {
            cx.update(|cx| {
                let ItemState::Ready(data) = &app.read(cx).items[0].state else {
                    unreachable!()
                };
                data.expanded_threads.contains("T1")
            })
        };
        assert!(!expanded(cx));
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let click = |vcx: &mut gpui::VisualTestContext, y| {
            let point = gpui::point(px(1100.), px(y));
            vcx.simulate_mouse_move(point, None, gpui::Modifiers::default());
            vcx.simulate_mouse_down(point, gpui::MouseButton::Left, gpui::Modifiers::default());
            vcx.simulate_mouse_up(point, gpui::MouseButton::Left, gpui::Modifiers::default());
            vcx.run_until_parked();
        };
        let mut header_y = None;
        for y in (80..600).step_by(4) {
            click(&mut vcx, y as f32);
            if expanded(cx) {
                header_y = Some(y as f32);
                break;
            }
        }
        let header_y = header_y.expect("the resolved one-line stub must be clickable");
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        click(&mut vcx, header_y);
        assert!(!expanded(cx), "a second click collapses the thread");
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
    }

    /// Read-only end-to-end demo: real fetch → snapshot → install → GFM draw
    /// and scroll through the entire busy discussion, then expand every stub.
    #[gpui::test]
    #[ignore = "requires authenticated gh and network; read-only public PR demo"]
    fn busy_real_pr_discussion_draws_end_to_end(cx: &mut gpui::TestAppContext) {
        let client: Arc<dyn github::GitHubClient> = Arc::new(github::gh_cli::GhCliClient::new());
        let loc = client.resolve_pr("zed-industries/zed#26893").unwrap();
        let loaded = fetch_item(client, &Source::Pr(loc.clone()), ViewMode::Split, 0).unwrap();
        let discussion = loaded.discussion.as_ref().unwrap();
        assert!(discussion.comments.len() > 100, "a busy merged discussion");
        assert!(discussion.threads.iter().any(|t| t.comments.len() > 1));
        assert!(discussion.threads.iter().any(|t| t.is_resolved));
        assert!(discussion.comments.iter().any(|c| !c.reactions.is_empty()));
        let snapshot = loaded.snapshot.as_ref().unwrap();
        assert_eq!(
            serde_json::from_value::<github::PrDiscussion>(snapshot.discussion.clone().unwrap())
                .unwrap(),
            *discussion
        );
        assert!(snapshot.threads.iter().any(|t| t.resolved));
        let entries = conversation::timeline(discussion);
        assert!(
            entries.iter().any(|entry| matches!(entry,
                conversation::Entry::Review(_, threads) if !threads.is_empty()
            )),
            "real inline threads are grouped under their submitted review"
        );
        assert!(
            discussion
                .threads
                .iter()
                .all(|thread| !thread.diff_hunk.is_empty() || thread.code_context.is_some()),
            "captured code is available for each real inline thread"
        );
        let count = entries.len();
        eprintln!(
            "Real PR: {} chronological entries, {} thread comments",
            count,
            discussion
                .threads
                .iter()
                .map(|t| t.comments.len())
                .sum::<usize>()
        );

        let fake = Arc::new(FakeQueue {
            asked: std::sync::Mutex::new(Vec::new()),
        });
        let (window, app) = queue_window(cx, fake);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        let mut item = loop_pr_item(Vec::new());
        item.source = Source::Pr(loc);
        item.install(loaded);
        item.pane = ItemPane::Conversation;
        install_item(&app, item, cx);
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let offset = |cx: &gpui::TestAppContext| {
            cx.update(|cx| {
                let ItemState::Ready(data) = &app.read(cx).items[0].state else {
                    unreachable!()
                };
                data.conv_scroll.offset().y
            })
        };
        let mut pages = 0;
        loop {
            let before = offset(cx);
            vcx.simulate_event(gpui::ScrollEvent {
                position: gpui::point(px(1100.), px(500.)),
                delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(-800.))),
                ..Default::default()
            });
            vcx.update(|window, cx| {
                let _ = window.draw(cx);
            });
            if offset(cx) == before {
                break;
            }
            pages += 1;
            assert!(pages < 1000, "scroll must reach the end");
        }
        assert!(pages > 1);
        eprintln!("Drew the entire conversation across {pages} scroll steps");
        cx.update(|cx| {
            app.update(cx, |this, cx| {
                let ItemState::Ready(data) = &mut this.items[0].state else {
                    unreachable!()
                };
                data.expanded_threads = data
                    .discussion
                    .as_ref()
                    .unwrap()
                    .threads
                    .iter()
                    .filter(|t| t.is_resolved)
                    .map(|t| t.id.clone())
                    .collect();
                cx.notify();
            })
        });
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
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
            discussion: None,
            is_draft: false,
            commits: None,
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
        vcx.simulate_event(gpui::ScrollEvent {
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
        vcx.simulate_event(gpui::ScrollEvent {
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

    #[gpui::test]
    fn automatic_refreshes_do_not_restart_an_in_flight_queue_fetch(cx: &mut gpui::TestAppContext) {
        let fake = Arc::new(FakeQueue {
            asked: std::sync::Mutex::new(Vec::new()),
        });
        let (_window, app) = queue_window(cx, fake.clone());
        let generation = cx.update(|cx| {
            app.update(cx, |app, cx| {
                app.queue.loading = true; // model a fetch that outlasts the poll interval
                app.queue.search = app.queue.search.clone().with_past_reviews(true);
                app.set_frontmost(false, cx);
                app.set_frontmost(true, cx);
                app.queue_gen
            })
        });
        let requests = fake.asked.lock().unwrap().len();
        cx.run_until_parked();
        cx.executor().advance_clock(BACKGROUND_POLL_INTERVAL * 2);
        cx.run_until_parked();
        cx.update(|cx| {
            assert_eq!(app.read(cx).queue_gen, generation);
            assert!(app.read(cx).queue.loading);
        });
        assert_eq!(fake.asked.lock().unwrap().len(), requests);
    }

    #[gpui::test]
    fn past_reviews_toggle_orders_limits_and_returns_to_the_default_queue(
        cx: &mut gpui::TestAppContext,
    ) {
        let fake = Arc::new(FakeQueue {
            asked: std::sync::Mutex::new(Vec::new()),
        });
        let (window, app) = queue_window(cx, fake.clone());
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        vcx.simulate_resize(size(px(1440.), px(900.)));
        let click_toggle = |vcx: &mut gpui::VisualTestContext| {
            vcx.update(|window, cx| {
                let _ = window.draw(cx);
            });
            let at = vcx
                .debug_bounds("queue-past-reviews-toggle")
                .unwrap()
                .center();
            vcx.simulate_mouse_move(at, None, gpui::Modifiers::default());
            vcx.simulate_mouse_down(at, gpui::MouseButton::Left, gpui::Modifiers::default());
            vcx.simulate_mouse_up(at, gpui::MouseButton::Left, gpui::Modifiers::default());
            vcx.run_until_parked();
        };
        vcx.cx.update(|cx| {
            assert!(!app.read(cx).queue.search.is_past_reviews());
            app.update(cx, |app, cx| app.queue_toggle_sort(cx));
        });
        vcx.run_until_parked();
        click_toggle(&mut vcx);
        vcx.cx.update(|cx| {
            let this = app.read(cx);
            assert!(this.queue.search.is_past_reviews());
            assert_eq!(
                this.queue
                    .prs
                    .iter()
                    .map(|pr| pr.number)
                    .collect::<Vec<_>>(),
                (5..=14).rev().collect::<Vec<_>>()
            );
            assert_eq!(this.queue.selected, 0);
            assert_eq!(this.queue.search.sort(), ("reviewed", "desc"));
            // Neither history rows nor the mode overwrite launch state.
            assert!(!this.restore_queue_search().is_past_reviews());
            assert_eq!(this.store.cached_queue_items().len(), 2);
            app.update(cx, |app, cx| app.queue_toggle_sort(cx));
            assert_eq!(app.read(cx).queue.search.sort(), ("reviewed", "desc"));
        });
        click_toggle(&mut vcx);
        vcx.cx.update(|cx| {
            let app = app.read(cx);
            assert!(!app.queue.search.is_past_reviews());
            assert_eq!(app.queue.prs.len(), 2);
            assert_eq!(
                app.queue.search.ordering(),
                github::QueueSort::CreatedAscending
            );
        });
        assert!(fake
            .asked
            .lock()
            .unwrap()
            .iter()
            .any(|(query, _)| query == "is:pr reviewed-by:@me"));
    }

    #[gpui::test]
    fn past_reviews_controls_stay_visible_on_empty_loading_and_failed_queues(
        cx: &mut gpui::TestAppContext,
    ) {
        let (window, app) = queue_window(
            cx,
            Arc::new(OfflineQueue {
                asked: std::sync::Mutex::new(0),
            }),
        );
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        vcx.simulate_resize(size(px(1440.), px(900.)));
        vcx.cx.update(|cx| {
            app.update(cx, |app, cx| {
                app.queue
                    .replace_rows(vec![queued(1, "acme/widgets", "alice")]);
                app.store.cache_queue(&app.queue.search, &app.queue.prs);
                app.queue_toggle_past_reviews(cx);
                assert!(
                    app.queue.prs.is_empty(),
                    "never show pending rows as history"
                );
                assert!(app.queue.loading);
            })
        });
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        assert!(vcx.debug_bounds("queue-past-reviews-toggle").is_some());
        vcx.run_until_parked();
        vcx.cx
            .update(|cx| assert!(app.read(cx).queue.error.is_some()));
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        assert!(vcx.debug_bounds("queue-past-reviews-toggle").is_some());
        vcx.cx.update(|cx| {
            app.update(cx, |app, cx| {
                app.queue.error = None;
                cx.notify();
            })
        });
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        assert!(vcx.debug_bounds("queue-past-reviews-toggle").is_some());
        vcx.cx.update(|cx| {
            app.update(cx, |app, cx| {
                app.queue_toggle_past_reviews(cx);
                assert!(!app.queue.search.is_past_reviews());
                assert_eq!(
                    app.queue.prs.len(),
                    1,
                    "return to the launch cache even offline"
                );
            })
        });
        vcx.run_until_parked();
        vcx.cx
            .update(|cx| assert_eq!(app.read(cx).queue.prs.len(), 1));
    }

    /// The mouse path itself: a real down-up click on a chip must narrow
    /// the queue — hitbox, element state, pending mouse-down, click
    /// listeners — everything between the platform mouse event and the
    /// handler the previous test drives directly. Use painted bounds so
    /// the test also works when the queue is centered in a wider window.
    #[gpui::test]
    fn the_mouse_click_path_narrows_the_queue(cx: &mut gpui::TestAppContext) {
        let fake = Arc::new(FakeQueue {
            asked: std::sync::Mutex::new(Vec::new()),
        });
        let (window, app) = queue_window(cx, fake.clone());
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        vcx.simulate_resize(size(px(1440.), px(900.)));
        let click = |vcx: &mut gpui::VisualTestContext, selector| {
            vcx.update(|window, cx| {
                let _ = window.draw(cx);
            });
            let at = vcx
                .debug_bounds(selector)
                .expect("chip is painted")
                .center();
            vcx.simulate_mouse_move(at, None, gpui::Modifiers::default());
            vcx.simulate_mouse_down(at, gpui::MouseButton::Left, gpui::Modifiers::default());
            vcx.simulate_mouse_up(at, gpui::MouseButton::Left, gpui::Modifiers::default());
            vcx.run_until_parked();
        };
        let ordering = |vcx: &gpui::VisualTestContext| {
            vcx.cx.update(|cx| {
                let app = app.read(cx);
                assert_eq!(app.restore_queue_search(), app.queue.search);
                app.queue.search.ordering()
            })
        };
        click(&mut vcx, "queue-sort-toggle");
        assert_eq!(ordering(&vcx), github::QueueSort::CreatedAscending);
        click(&mut vcx, "queue-sort-toggle");
        assert_eq!(ordering(&vcx), github::QueueSort::ReviewRequestedAscending);

        // Both PRs share an org, so only repository chips appear.
        click(&mut vcx, "queue-chip-0");
        let filter = github::QueueFilter::repo("acme/gadgets");

        // The click narrowed the queue — the mouse path works end to end.
        vcx.cx.update(|cx| {
            let app = app.read(cx);
            assert_eq!(app.queue.prs.len(), 1, "the click must narrow the queue");
            assert_eq!(app.queue.prs[0].repository, filter.value());
            assert_eq!(app.queue.search.filters(), std::slice::from_ref(&filter));
        });
        assert!(fake
            .asked
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .0
            .contains("repo:"));
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
        let BuiltRows { rows, .. } = build_rows(RowsView {
            diff: &sample_diff(),
            upgrades: &HashMap::new(),
            comments: Some(&index),
            options: RowOptions {
                mode: ViewMode::Unified,
                file_ix: 0,
                comments_visible: true,
                comment_wrap: COMMENT_WRAP_CHARS,
            },
        });
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
        let BuiltRows {
            rows, hunk_rows, ..
        } = build_rows(RowsView {
            diff: &sample_diff(),
            upgrades: &HashMap::new(),
            comments: None,
            options: RowOptions {
                mode: ViewMode::Split,
                file_ix: 0,
                comments_visible: true,
                comment_wrap: COMMENT_WRAP_CHARS,
            },
        });
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

        assert_eq!(review.comments[0].id, 1);
        assert_eq!(review.comments[1].in_reply_to_id, Some(1));
        assert!(review.comments.iter().all(|c| c.time == CommentTime::Draft));
        assert!(review.comments.iter().all(|c| c.author == "you"));
        assert!(review.comments.iter().all(|c| c.start_line.is_none()));
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
    fn local_review_prompt_keeps_global_draft_order_outdated_threads_and_fallbacks() {
        let src = git::LocalSource {
            repo_root: "/tmp/myrepo".into(),
            branch: "feature".into(),
            base_ref: None,
            base_label: "main".into(),
            base_oid: None,
        };
        let mut review = LocalReview::default();
        review.add_comment(
            None,
            "z.rs".into(),
            CommentSide::Left,
            7,
            "first\nbody".into(),
        );
        review.add_comment(None, "".into(), CommentSide::Right, 0, "outdated".into());
        review.comments[1].line = None;
        review.add_comment(
            Some(2),
            "".into(),
            CommentSide::Right,
            0,
            "reply\nbody".into(),
        );
        review.comments[2].author = "  ".into();
        review.add_comment(Some(3), "".into(), CommentSide::Right, 0, "nested".into());
        review.comments[3].author = "bob".into();
        review.add_comment(None, "a.rs".into(), CommentSide::Right, 2, "last".into());
        review.add_comment(
            Some(999),
            "a.rs".into(),
            CommentSide::Right,
            2,
            "orphan".into(),
        );
        review.comments.reverse();

        assert_eq!(local_review_prompt(&src, &review),
            "Diff: main ← feature. Locations are GitHub diff-style: path:Rline for right/new, path:Lline for left/old.\n\nz.rs:L7\nfirst\nbody\n=====\n<unknown file>:R0\noutdated\nReply 1 (Unknown)\nreply\nbody\nReply 2 (bob)\nnested\n=====\na.rs:R2\nlast");
        let index = review.index();
        assert_eq!(index.counts[""], (0, 3));
        assert_eq!(index.counts["z.rs"], (1, 0));
        assert_eq!(index.counts["a.rs"], (1, 0));
        assert!(!index.threads.contains_key(""));
    }

    #[test]
    fn local_review_rows_show_draft_age_and_numeric_root_reply_action() {
        let mut review = LocalReview::default();
        review.add_comment(None, "a.rs".into(), CommentSide::Right, 1, "root".into());
        review.add_comment(
            Some(1),
            "a.rs".into(),
            CommentSide::Right,
            1,
            "reply".into(),
        );
        let index = review.index();
        let built = build_rows(RowsView {
            diff: &two_file_diff(),
            upgrades: &HashMap::new(),
            comments: Some(&index),
            options: RowOptions {
                mode: ViewMode::Unified,
                file_ix: 0,
                comments_visible: true,
                comment_wrap: COMMENT_WRAP_CHARS,
            },
        });
        let headers = built
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::CommentHeader {
                    author,
                    when,
                    is_reply,
                    ..
                } => Some((author.as_ref(), when.as_ref(), *is_reply)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            headers,
            vec![("you", "draft", false), ("you", "draft", true)]
        );
        assert_eq!(
            built
                .rows
                .iter()
                .filter(|row| matches!(
                    row,
                    Row::CommentActions {
                        root_id: 1,
                        side: CommentSide::Right,
                        line: 1,
                        ..
                    }
                ))
                .count(),
            1
        );
    }

    #[test]
    fn seed_mentions_reads_app_comment_authors_and_keeps_existing_users() {
        let item = loop_pr_item(vec![
            rc(1, "a.rs", None, Some(1), "root", "BOB", "a", None),
            rc(2, "a.rs", None, Some(1), "reply", "carol", "b", Some(1)),
            rc(3, "a.rs", None, Some(1), "blank author", "", "c", Some(1)),
        ]);
        let ItemState::Ready(data) = item.state else {
            panic!("ready fixture")
        };
        let pool = Rc::new(RefCell::new(vec![
            mention("bob", None),
            mention("fetched", None),
        ]));
        seed_mentions(&pool, &data);
        seed_mentions(&pool, &data);
        assert_eq!(
            pool.borrow()
                .iter()
                .map(|m| m.login.clone())
                .collect::<Vec<_>>(),
            vec!["bob", "fetched", "alice", "carol"]
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
        let index = group_comments(comments.into_iter().map(ReviewComment::from).collect());
        let built_rows = build_rows(RowsView {
            diff: &diff,
            upgrades: &HashMap::new(),
            comments: Some(&index),
            options: RowOptions {
                mode: ViewMode::Unified,
                file_ix: 0,
                comments_visible: true,
                comment_wrap: COMMENT_WRAP_CHARS,
            },
        });
        ReviewItem {
            id: 0,
            source: Source::Pr(loop_loc()),
            state: ItemState::Ready(Box::new({
                let mut data = ItemData::new(
                    Some(loop_pr_meta()),
                    diff,
                    ViewMode::Unified,
                    0,
                    built_rows,
                    Some(index),
                    None,
                );
                data.pr_base = Some("ba5ec0de".into());
                data
            })),
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
        Reaction {
            target: github::CommentTarget,
            content: String,
            reacted: bool,
        },
        Resolve {
            thread_id: String,
            resolved: bool,
        },
        Edit {
            target: github::CommentTarget,
            body: String,
        },
        Delete {
            target: github::CommentTarget,
        },
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
        Conversation {
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
        resolutions: std::sync::Mutex<HashMap<String, bool>>,
        reactions: std::sync::Mutex<HashMap<String, Vec<github::Reaction>>>,
        comment_permissions: std::sync::Mutex<HashMap<u64, github::CommentPermissions>>,
        comments: std::sync::Mutex<Vec<github::ReviewComment>>,
        conversation: std::sync::Mutex<Vec<github::DiscussionComment>>,
        posted: std::sync::Mutex<Vec<Post>>,
        /// While set, posting a new comment fails — the inline-error path.
        fail: std::sync::atomic::AtomicBool,
        stale_discussion: std::sync::atomic::AtomicBool,
        fail_discussion: std::sync::atomic::AtomicBool,
        /// The id the next posted comment gets.
        next_id: std::sync::atomic::AtomicU64,
        pending: std::sync::Mutex<Option<github::PendingReview>>,
        commits: std::sync::Mutex<Option<github::PrCommits>>,
        review_changes: std::sync::Mutex<github::ReviewChanges>,
        comparison_patch: std::sync::Mutex<Option<github::ReviewComparison>>,
        comparison_requests: std::sync::Mutex<Vec<(String, String, String)>>,
        fail_review_history: std::sync::atomic::AtomicBool,
        pr_merge_base: Option<String>,
        fail_merge_base: bool,
        commit_fetches: std::sync::atomic::AtomicUsize,
        patch: Option<String>,
        /// Hold patch completion independently of the conversation in staged-load tests.
        patch_gate: std::sync::Mutex<
            Option<std::sync::mpsc::Receiver<Result<String, github::ClientError>>>,
        >,
        /// Signal and hold the first full-content request in staged-load tests.
        file_gate:
            std::sync::Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
        file_contents: HashMap<(String, String), String>,
        viewed_files: std::sync::Mutex<HashMap<String, github::FileViewedState>>,
        viewed_writes: std::sync::Mutex<Vec<(String, bool)>>,
        fail_viewed_fetch: std::sync::atomic::AtomicBool,
        viewed_commit_then_fail: std::sync::atomic::AtomicBool,
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

        fn patch_text(&self) -> String {
            self.patch.clone().unwrap_or_else(|| {
                "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old\n+new\n".into()
            })
        }
    }

    impl github::GitHubClient for FakePr {
        fn set_comment_reaction(
            &self,
            target: &github::CommentTarget,
            content: &str,
            reacted: bool,
        ) -> Result<(), github::ClientError> {
            self.posted.lock().unwrap().push(Post::Reaction {
                target: target.clone(),
                content: content.into(),
                reacted,
            });
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(github::ClientError::Failed("reaction rejected".into()));
            }
            let (github::CommentTarget::Issue(id)
            | github::CommentTarget::Review(id)
            | github::CommentTarget::ReviewComment(id)) = target;
            self.reactions.lock().unwrap().insert(
                id.clone(),
                if reacted {
                    vec![github::Reaction {
                        content: content.into(),
                        count: 1,
                        viewer_has_reacted: true,
                    }]
                } else {
                    vec![]
                },
            );
            Ok(())
        }

        fn set_thread_resolved(
            &self,
            thread_id: &str,
            resolved: bool,
        ) -> Result<(), github::ClientError> {
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(github::ClientError::Failed("resolution failed".into()));
            }
            self.resolutions
                .lock()
                .unwrap()
                .insert(thread_id.into(), resolved);
            self.posted.lock().unwrap().push(Post::Resolve {
                thread_id: thread_id.into(),
                resolved,
            });
            Ok(())
        }
        fn edit_comment(
            &self,
            target: &github::CommentTarget,
            body: &str,
        ) -> Result<(), github::ClientError> {
            self.posted.lock().unwrap().push(Post::Edit {
                target: target.clone(),
                body: body.into(),
            });
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(github::ClientError::Failed("edit rejected".into()));
            }
            match target {
                github::CommentTarget::ReviewComment(node_id) => {
                    let permissions = self.comment_permissions.lock().unwrap();
                    let id = permissions
                        .iter()
                        .find(|(_, p)| &p.node_id == node_id)
                        .unwrap()
                        .0;
                    self.comments
                        .lock()
                        .unwrap()
                        .iter_mut()
                        .find(|c| c.id == *id)
                        .unwrap()
                        .body = body.into();
                }
                github::CommentTarget::Issue(node_id) | github::CommentTarget::Review(node_id) => {
                    self.conversation
                        .lock()
                        .unwrap()
                        .iter_mut()
                        .find(|c| &c.permissions.node_id == node_id)
                        .unwrap()
                        .body = body.into();
                }
            }
            Ok(())
        }

        fn delete_comment(
            &self,
            target: &github::CommentTarget,
        ) -> Result<(), github::ClientError> {
            self.posted.lock().unwrap().push(Post::Delete {
                target: target.clone(),
            });
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(github::ClientError::Failed("delete rejected".into()));
            }
            match target {
                github::CommentTarget::ReviewComment(node_id) => {
                    let permissions = self.comment_permissions.lock().unwrap();
                    let id = permissions
                        .iter()
                        .find(|(_, p)| &p.node_id == node_id)
                        .unwrap()
                        .0;
                    self.comments.lock().unwrap().retain(|c| c.id != *id);
                }
                github::CommentTarget::Issue(node_id) | github::CommentTarget::Review(node_id) => {
                    self.conversation
                        .lock()
                        .unwrap()
                        .retain(|c| &c.permissions.node_id != node_id);
                }
            }
            Ok(())
        }

        fn viewed_files(
            &self,
            _loc: &github::PrLocator,
        ) -> Result<github::ViewedFiles, github::ClientError> {
            if self
                .fail_viewed_fetch
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return Err(github::ClientError::Failed("viewed fetch failed".into()));
            }
            let mut files: HashMap<_, _> = diff_core::parse_patch(&self.patch_text())
                .files
                .iter()
                .map(|file| {
                    (
                        file.display_path().to_owned(),
                        github::FileViewedState::Unviewed,
                    )
                })
                .collect();
            files.extend(self.viewed_files.lock().unwrap().clone());
            Ok(github::ViewedFiles {
                pull_request_id: "PR7".into(),
                files,
            })
        }

        fn set_file_viewed(
            &self,
            id: &str,
            path: &str,
            viewed: bool,
        ) -> Result<(), github::ClientError> {
            assert_eq!(id, "PR7");
            self.viewed_writes
                .lock()
                .unwrap()
                .push((path.into(), viewed));
            let commit_then_fail = self
                .viewed_commit_then_fail
                .load(std::sync::atomic::Ordering::SeqCst);
            if !self.fail.load(std::sync::atomic::Ordering::SeqCst) || commit_then_fail {
                self.viewed_files.lock().unwrap().insert(
                    path.into(),
                    if viewed {
                        github::FileViewedState::Viewed
                    } else {
                        github::FileViewedState::Unviewed
                    },
                );
            }
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) || commit_then_fail {
                Err(github::ClientError::Failed("viewed mutation failed".into()))
            } else {
                Ok(())
            }
        }

        fn pr_discussion(
            &self,
            _loc: &github::PrLocator,
        ) -> Result<github::PrDiscussion, github::ClientError> {
            if self
                .fail_discussion
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return Err(github::ClientError::Failed(
                    "discussion refresh failed".into(),
                ));
            }
            let mut discussion =
                github::PrDiscussion::from_review_comments(self.comments.lock().unwrap().clone());
            discussion.comments = self.conversation.lock().unwrap().clone();
            for thread in &mut discussion.threads {
                if let Some(resolved) = self.resolutions.lock().unwrap().get(&thread.id) {
                    thread.is_resolved = *resolved;
                    thread.viewer_can_resolve = true;
                    thread.viewer_can_unresolve = true;
                }
            }
            let permissions = self.comment_permissions.lock().unwrap();
            for comment in discussion.threads.iter_mut().flat_map(|t| &mut t.comments) {
                comment.permissions = permissions.get(&comment.id).cloned().unwrap_or_default();
            }
            let reactions = self.reactions.lock().unwrap();
            for comment in discussion
                .comments
                .iter_mut()
                .chain(discussion.threads.iter_mut().flat_map(|t| &mut t.comments))
            {
                if let Some(reactions) = reactions.get(&comment.permissions.node_id) {
                    comment.reactions = reactions.clone();
                }
            }
            if self
                .stale_discussion
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                discussion.comments.retain(|comment| comment.id < 100);
                for thread in &mut discussion.threads {
                    thread.comments.retain(|comment| comment.id < 100);
                }
            }
            Ok(discussion)
        }

        fn post_conversation_comment_id(
            &self,
            loc: &github::PrLocator,
            body: &str,
        ) -> Result<Option<u64>, github::ClientError> {
            self.post_conversation_comment(loc, body)?;
            Ok(self
                .conversation
                .lock()
                .unwrap()
                .last()
                .map(|comment| comment.id))
        }

        fn post_reply_id(
            &self,
            loc: &github::PrLocator,
            comment_id: u64,
            body: &str,
        ) -> Result<Option<u64>, github::ClientError> {
            self.post_reply(loc, comment_id, body)?;
            Ok(self
                .comments
                .lock()
                .unwrap()
                .last()
                .map(|comment| comment.id))
        }

        fn post_conversation_comment(
            &self,
            _loc: &github::PrLocator,
            body: &str,
        ) -> Result<(), github::ClientError> {
            self.posted
                .lock()
                .unwrap()
                .push(Post::Conversation { body: body.into() });
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(github::ClientError::Failed("comment rejected".into()));
            }
            self.conversation
                .lock()
                .unwrap()
                .push(github::DiscussionComment {
                    permissions: Default::default(),
                    id: self
                        .next_id
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                        + 100,
                    author: "tester".into(),
                    body: body.into(),
                    created_at: "2026-10-25T00:00:00Z".into(),
                    reactions: vec![],
                    review_state: None,
                    review_id: None,
                });
            Ok(())
        }

        fn update_pending_body(
            &self,
            _loc: &github::PrLocator,
            _review: &github::PendingReview,
            body: &str,
        ) -> Result<(), github::ClientError> {
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(github::ClientError::Failed("summary rejected".into()));
            }
            self.pending.lock().unwrap().as_mut().unwrap().body = body.into();
            Ok(())
        }

        fn pr_commits(
            &self,
            _loc: &github::PrLocator,
        ) -> Result<github::PrCommits, github::ClientError> {
            self.commit_fetches
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.commits
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| github::ClientError::Failed("commits unavailable".into()))
        }

        fn review_changes(
            &self,
            _loc: &github::PrLocator,
            _head: &str,
        ) -> Result<github::ReviewChanges, github::ClientError> {
            if self
                .fail_review_history
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return Err(github::ClientError::Failed("history unavailable".into()));
            }
            Ok(self.review_changes.lock().unwrap().clone())
        }

        fn comparison_patch(
            &self,
            _loc: &github::PrLocator,
            base: &str,
            head: &str,
            upstream: &str,
        ) -> Result<github::ReviewComparison, github::ClientError> {
            self.comparison_requests.lock().unwrap().push((
                base.into(),
                head.into(),
                upstream.into(),
            ));
            self.comparison_patch
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| github::ClientError::Failed("comparison unavailable".into()))
        }

        fn pending_review(
            &self,
            _loc: &github::PrLocator,
        ) -> Result<Option<github::PendingReview>, github::ClientError> {
            Ok(self.pending.lock().unwrap().clone())
        }

        fn start_review(
            &self,
            _loc: &github::PrLocator,
            _commit: &str,
        ) -> Result<github::PendingReview, github::ClientError> {
            let review = github::PendingReview {
                id: 91,
                node_id: "pending".into(),
                body: String::new(),
                comments: vec![],
            };
            *self.pending.lock().unwrap() = Some(review.clone());
            Ok(review)
        }

        fn add_pending_comment(
            &self,
            _loc: &github::PrLocator,
            review: &github::PendingReview,
            anchor: &github::CommentAnchor,
            body: &str,
        ) -> Result<github::PendingComment, github::ClientError> {
            assert_eq!(review.id, 91);
            let comment = github::PendingComment {
                id: 12,
                in_reply_to_id: None,
                node_id: "draft_node".into(),
                path: anchor.path.clone(),
                side: anchor.side,
                line: Some(anchor.line),
                start_line: anchor.start_line,
                body: body.into(),
            };
            self.pending
                .lock()
                .unwrap()
                .as_mut()
                .unwrap()
                .comments
                .push(comment.clone());
            Ok(comment)
        }

        fn add_pending_reply(
            &self,
            _loc: &github::PrLocator,
            review: &github::PendingReview,
            comment_id: u64,
            body: &str,
        ) -> Result<github::PendingComment, github::ClientError> {
            assert_eq!(review.id, 91);
            let parent = self
                .comments
                .lock()
                .unwrap()
                .iter()
                .find(|comment| comment.id == comment_id)
                .unwrap()
                .clone();
            let comment = github::PendingComment {
                id: self
                    .next_id
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    + 100,
                node_id: "draft_reply_node".into(),
                path: parent.path,
                side: CommentSide::Right,
                line: parent.line,
                start_line: parent.start_line,
                body: body.into(),
                in_reply_to_id: Some(comment_id),
            };
            self.pending
                .lock()
                .unwrap()
                .as_mut()
                .unwrap()
                .comments
                .push(comment.clone());
            Ok(comment)
        }

        fn edit_pending_comment(
            &self,
            _loc: &github::PrLocator,
            comment: &github::PendingComment,
            body: &str,
        ) -> Result<(), github::ClientError> {
            self.pending
                .lock()
                .unwrap()
                .as_mut()
                .unwrap()
                .comments
                .iter_mut()
                .find(|draft| draft.id == comment.id)
                .unwrap()
                .body = body.into();
            Ok(())
        }

        fn delete_pending_comment(
            &self,
            _loc: &github::PrLocator,
            comment: &github::PendingComment,
        ) -> Result<(), github::ClientError> {
            self.pending
                .lock()
                .unwrap()
                .as_mut()
                .unwrap()
                .comments
                .retain(|draft| draft.id != comment.id);
            Ok(())
        }

        fn submit_pending_review(
            &self,
            _loc: &github::PrLocator,
            _review: &github::PendingReview,
            verdict: github::ReviewVerdict,
            body: &str,
        ) -> Result<(), github::ClientError> {
            self.pending.lock().unwrap().take().unwrap();
            self.posted.lock().unwrap().push(Post::Review {
                verdict,
                body: body.into(),
            });
            Ok(())
        }

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

        fn resolve_pr(&self, arg: &str) -> Result<github::PrLocator, github::ClientError> {
            match arg {
                "acme/widgets#7" | "https://github.com/acme/widgets/pull/7" => {
                    Ok(github::PrLocator {
                        owner: "acme".into(),
                        repo: "widgets".into(),
                        number: 7,
                    })
                }
                _ => Err(github::ClientError::Failed("invalid PR identifier".into())),
            }
        }

        fn pr_meta(&self, _loc: &github::PrLocator) -> Result<github::PrMeta, github::ClientError> {
            Ok(loop_pr_meta())
        }

        fn merge_base(
            &self,
            loc: &github::PrLocator,
            base: &str,
            head: &str,
        ) -> Result<String, github::ClientError> {
            assert_eq!(loc.repo_slug(), loop_loc().repo_slug());
            assert_eq!(base, "ba5ec0de");
            assert_eq!(head, "dead10cc");
            if self.fail_merge_base {
                return Err(github::ClientError::Failed("merge base unavailable".into()));
            }
            Ok(self.pr_merge_base.clone().unwrap_or_else(|| base.into()))
        }

        fn pr_patch(&self, _loc: &github::PrLocator) -> Result<String, github::ClientError> {
            if let Some(gate) = self.patch_gate.lock().unwrap().take() {
                return gate
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap_or_else(|error| {
                        Err(github::ClientError::Failed(format!("patch gate: {error}")))
                    });
            }
            Ok(self.patch_text())
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
            commit_oid: &str,
            path: &str,
        ) -> Result<Option<String>, github::ClientError> {
            if path == "a.rs" {
                if let Some((started, release)) = self.file_gate.lock().unwrap().take() {
                    started.send(()).unwrap();
                    release
                        .recv_timeout(std::time::Duration::from_secs(5))
                        .unwrap();
                }
            }
            Ok(self
                .file_contents
                .get(&(commit_oid.into(), path.into()))
                .cloned())
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
            if self.pending.lock().unwrap().is_some() {
                return Err(github::ClientError::Failed(
                    "pending review already exists".into(),
                ));
            }
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
            if self.pending.lock().unwrap().is_some() {
                return Err(github::ClientError::Failed(
                    "pending review already exists".into(),
                ));
            }
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
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(github::ClientError::Failed("review rejected".into()));
            }
            Ok(())
        }
    }

    /// Install `item` as the pane's only, active item, off the queue screen —
    /// the state after a finished open.
    fn install_item(
        app: &gpui::Entity<ReviewApp>,
        mut item: ReviewItem,
        cx: &mut gpui::TestAppContext,
    ) {
        // Fixtures may replace the diff on an existing item. Keep the now
        // always-rendered sidebar's tree in sync, as a real fetch does.
        if let ItemState::Ready(data) = &mut item.state {
            data.rebuild_tree();
        }
        cx.update(|cx| {
            app.update(cx, |this, _| {
                this.show_queue = false;
                this.items = vec![item];
                this.active = 0;
                this.next_id = 1;
            });
        });
    }

    #[gpui::test]
    fn file_history_keyboard_buttons_and_mouse(cx: &mut gpui::TestAppContext) {
        let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
        cx.update(|cx| cx.bind_keys(file_history::keybindings()));
        install_item(&app, loop_pr_item(Vec::new()), cx);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        draw_window(&mut vcx);
        // Disabled back is a no-op at the start.
        let back = vcx.debug_bounds("file-back").unwrap().center();
        vcx.simulate_click(back, gpui::Modifiers::default());
        vcx.update(|_, cx| assert_eq!(app.read(cx).active_data().unwrap().file_ix, 0));
        vcx.update(|window, cx| {
            app.update(cx, |this, cx| this.jump_to_file(1, window, cx));
        });
        draw_window(&mut vcx);
        vcx.simulate_keystrokes("cmd-[");
        vcx.update(|_, cx| assert_eq!(app.read(cx).active_data().unwrap().file_ix, 0));
        vcx.simulate_keystrokes("cmd-]");
        vcx.update(|_, cx| assert_eq!(app.read(cx).active_data().unwrap().file_ix, 1));
        draw_window(&mut vcx);
        let back = vcx.debug_bounds("file-back").unwrap().center();
        vcx.simulate_click(back, gpui::Modifiers::default());
        vcx.update(|_, cx| assert_eq!(app.read(cx).active_data().unwrap().file_ix, 0));
        draw_window(&mut vcx);
        let forward = vcx.debug_bounds("file-forward").unwrap().center();
        vcx.simulate_click(forward, gpui::Modifiers::default());
        vcx.update(|_, cx| assert_eq!(app.read(cx).active_data().unwrap().file_ix, 1));
        draw_window(&mut vcx);
        for (direction, expected) in [
            (NavigationDirection::Back, 0),
            (NavigationDirection::Forward, 1),
        ] {
            vcx.simulate_mouse_down(
                point(px(700.), px(300.)),
                MouseButton::Navigate(direction),
                gpui::Modifiers::default(),
            );
            vcx.simulate_mouse_up(
                point(px(700.), px(300.)),
                MouseButton::Navigate(direction),
                gpui::Modifiers::default(),
            );
            vcx.update(|_, cx| assert_eq!(app.read(cx).active_data().unwrap().file_ix, expected));
            draw_window(&mut vcx);
        }
        // A new selection after going back invalidates the forward branch.
        vcx.simulate_keystrokes("cmd-[");
        vcx.update(|_, cx| {
            app.update(cx, |this, cx| this.step_file(1, cx));
            assert!(!app
                .read(cx)
                .active_data()
                .unwrap()
                .file_history
                .can_go(NavigationDirection::Forward));
        });
        vcx.simulate_keystrokes("cmd-]");
        vcx.update(|_, cx| assert_eq!(app.read(cx).active_data().unwrap().file_ix, 1));
    }

    #[gpui::test]
    fn file_history_is_per_item_and_inactive_outside_changes(cx: &mut gpui::TestAppContext) {
        let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
        install_item(&app, loop_pr_item(Vec::new()), cx);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        vcx.update(|_, cx| app.update(cx, |this, cx| this.step_file(1, cx)));
        vcx.dispatch_action(ShowConversation);
        vcx.dispatch_action(FileBack);
        vcx.update(|_, cx| assert_eq!(app.read(cx).active_data().unwrap().file_ix, 1));
        vcx.dispatch_action(BackToQueue);
        vcx.dispatch_action(FileBack);
        vcx.update(|window, cx| {
            assert_eq!(app.read(cx).active_data().unwrap().file_ix, 1);
            app.update(cx, |this, cx| {
                this.return_to_review(window, cx);
                this.flip_pane(ItemPane::Changes, cx);
                let mut other = loop_pr_item(Vec::new());
                other.id = 1;
                this.items.push(other);
                this.active = 1;
            });
        });
        vcx.dispatch_action(FileBack);
        vcx.update(|_, cx| {
            assert_eq!(app.read(cx).active_data().unwrap().file_ix, 0);
            app.update(cx, |this, _| this.active = 0);
        });
        vcx.dispatch_action(FileBack);
        vcx.update(|_, cx| assert_eq!(app.read(cx).active_data().unwrap().file_ix, 0));
        vcx.dispatch_action(FileForward);
        vcx.update(|_, cx| assert_eq!(app.read(cx).active_data().unwrap().file_ix, 1));
    }

    #[test]
    fn file_history_survives_refresh_but_resets_with_review_scope() {
        let mut item = loop_pr_item(Vec::new());
        let ItemState::Ready(data) = &mut item.state else {
            unreachable!()
        };
        data.open_file(1, None);
        // A normal refresh reorders the files without changing their identity.
        let fake = Arc::new(FakePr::default());
        let mut loaded = fetch_item(fake.clone(), &item.source, ViewMode::Split, 0).unwrap();
        loaded.diff = two_file_diff();
        loaded.diff.files.swap(0, 1);
        loaded.upgrades.clear();
        item.install(loaded);
        let ItemState::Ready(data) = &mut item.state else {
            unreachable!()
        };
        assert_eq!(data.diff.files[data.file_ix].display_path(), "b.rs");
        assert_eq!(
            data.file_history
                .step(NavigationDirection::Back, "b.rs")
                .as_deref(),
            Some("a.rs")
        );
        data.show_file(1, None);
        // Removing b.rs prunes forward history; refresh itself adds no visit.
        let loaded = fetch_item(fake.clone(), &item.source, ViewMode::Split, 0).unwrap();
        item.install(loaded);
        let ItemState::Ready(data) = &mut item.state else {
            unreachable!()
        };
        assert!(!data.file_history.can_go(NavigationDirection::Back));
        assert!(!data.file_history.can_go(NavigationDirection::Forward));
        data.file_history.record("b.rs", "a.rs");
        let mut loaded = fetch_item(fake, &item.source, ViewMode::Split, 0).unwrap();
        // Both paths still exist, so only the scope reset can remove this stop.
        loaded.diff = two_file_diff();
        loaded.upgrades.clear();
        loaded.review_base = Some("different-comparison".into());
        item.install(loaded);
        let ItemState::Ready(data) = &item.state else {
            unreachable!()
        };
        assert!(!data.file_history.can_go(NavigationDirection::Back));
        assert!(!data.file_history.can_go(NavigationDirection::Forward));
    }

    #[gpui::test]
    fn file_switching_restores_the_painted_viewport(cx: &mut gpui::TestAppContext) {
        let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        for mode in [ViewMode::Unified, ViewMode::Split] {
            for wrap in [false, true] {
                let lines = "abcdefghij".repeat(60) + "\n";
                let mut diff = two_file_diff();
                diff.files[0].hunks = diff_texts("", &lines.repeat(200), 3);
                let mut data = file_mode_data(diff);
                data.mode = mode;
                data.preferences.soft_wrap = Some(wrap);
                data.set_rows(data.row_build());
                let mut item = loop_pr_item(Vec::new());
                item.state = ItemState::Ready(data);
                install_item(&app, item, &mut vcx);
                vcx.update(|window, cx| {
                    let focus = app.read(cx).focus_handle.clone();
                    window.focus(&focus, cx);
                    let _ = window.draw(cx);
                });
                let prior = vcx.update(|window, cx| {
                    app.update(cx, |this, _| {
                        let data = this.active_data_mut().unwrap();
                        // In wrap mode, anchor to a continuation, not just
                        // the source line's first visual row.
                        let row = if wrap {
                            data.rows
                                .iter()
                                .enumerate()
                                .skip(60)
                                .find(|(_, row)| matches!(row, Row::Continuation(_)))
                                .unwrap()
                                .0
                        } else {
                            60
                        };
                        data.position_restore = None;
                        data.scroll.0.borrow_mut().deferred_scroll_to_item = None;
                        data.scroll.0.borrow().base_handle.set_offset(point(
                            px(if wrap { 0. } else { -45. }),
                            px(-(row as f32 + 0.375) * row_height()),
                        ));
                    });
                    let _ = window.draw(cx);
                    app.read(cx)
                        .active_data()
                        .unwrap()
                        .scroll
                        .0
                        .borrow()
                        .base_handle
                        .offset()
                });
                // Keyboard navigation to a short file clamps the shared
                // scroll handle. Returning via the sidebar must restore it.
                vcx.update(|window, cx| {
                    app.update(cx, |this, cx| this.step_file(1, cx));
                    let _ = window.draw(cx);
                    assert_eq!(app.read(cx).active_data().unwrap().file_ix, 1);
                    app.update(cx, |this, cx| this.jump_to_file(0, window, cx));
                    let _ = window.draw(cx);
                    assert_eq!(
                        app.read(cx)
                            .active_data()
                            .unwrap()
                            .scroll
                            .0
                            .borrow()
                            .base_handle
                            .offset(),
                        prior
                    );
                    app.update(cx, |this, cx| {
                        this.navigate_file_history(NavigationDirection::Back, window, cx)
                    });
                    let _ = window.draw(cx);
                    assert_eq!(app.read(cx).active_data().unwrap().file_ix, 1);
                    app.update(cx, |this, cx| {
                        this.navigate_file_history(NavigationDirection::Forward, window, cx)
                    });
                    let _ = window.draw(cx);
                    assert_eq!(
                        app.read(cx)
                            .active_data()
                            .unwrap()
                            .scroll
                            .0
                            .borrow()
                            .base_handle
                            .offset(),
                        prior
                    );
                });
            }
        }
    }

    #[test]
    fn diff_padding_tracks_half_the_viewport_at_any_zoom() {
        for row_px in [12., 22., 48.] {
            for pane_px in [0., 1., 300., 441., 900.] {
                let padding = diff_padding_rows(pane_px, row_px) as f32 * row_px;
                assert!(padding >= pane_px / 2.);
                assert!(padding < pane_px / 2. + row_px);
            }
        }
    }

    #[gpui::test]
    fn diff_padding_and_minimap_share_the_full_scroll_range(cx: &mut gpui::TestAppContext) {
        let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        for mode in [ViewMode::Unified, ViewMode::Split] {
            let mut diff = two_file_diff();
            diff.files[0].hunks = diff_texts("", &"line\n".repeat(200), 3);
            let mut data = file_mode_data(diff);
            data.mode = mode;
            data.set_rows(data.row_build());
            let source_rows = data.rows.len();
            let mut item = loop_pr_item(Vec::new());
            item.state = ItemState::Ready(data);
            install_item(&app, item, &mut vcx);
            for height in [600., 900., 450.] {
                vcx.simulate_resize(size(px(1100.), px(height)));
                for minimap_visible in [true, false] {
                    vcx.update(|window, cx| {
                        app.update(cx, |this, cx| {
                            this.minimap_visible = minimap_visible;
                            cx.notify();
                        });
                        let _ = window.draw(cx);
                        let this = app.read(cx);
                        let data = this.active_data().unwrap();
                        let state = data.scroll.0.borrow();
                        let bounds = state.base_handle.bounds();
                        let pane_h = f32::from(bounds.size.height);
                        let tail = diff_padding_rows(pane_h, row_height());
                        let total = source_rows + tail;
                        assert_eq!(data.rows.len(), source_rows, "padding is not source data");
                        assert_eq!(data.minimap.len(), source_rows);
                        assert_eq!(
                            state.last_item_size.unwrap().contents.height,
                            px(total as f32 * row_height())
                        );
                        assert_eq!(
                            state.last_item_size.unwrap().item.height,
                            bounds.size.height,
                            "the list must still render a full viewport"
                        );
                        let layout = data.minimap_layout(pane_h, tail);
                        assert_eq!((layout.slot_h, layout.group), minimap_scale(total, pane_h));
                        drop(state);

                        app.update(cx, |this, cx| {
                            // Scrubbing into the minimap's blank tail must reach
                            // the same bottom as the actual list/scrollbar.
                            this.minimap_scrub_to(bounds.bottom_right(), cx);
                        });
                        let _ = window.draw(cx);
                        let data = app.read(cx).active_data().unwrap();
                        let state = data.scroll.0.borrow();
                        let bottom = -state.base_handle.offset().y;
                        assert_eq!(bottom, state.base_handle.max_offset().y);
                        let code_bottom = source_rows as f32 * row_height() - f32::from(bottom);
                        assert!(code_bottom <= pane_h / 2. + 0.01);
                        assert!(code_bottom > pane_h / 2. - row_height() - 0.01);
                    });
                }
            }
        }
    }

    #[gpui::test]
    fn unified_diff_highlights_fill_the_row_width(cx: &mut gpui::TestAppContext) {
        let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        for long_line in [false, true] {
            let last = if long_line {
                "abcdefghij".repeat(60)
            } else {
                "short".to_string()
            };
            let mut diff = two_file_diff();
            diff.files[0].hunks = diff_texts("old\n\nremoved\n", &format!("new\n\n{last}\n"), 3);
            let mut data = file_mode_data(diff);
            data.mode = ViewMode::Unified;
            data.set_rows(data.row_build());
            let mut item = loop_pr_item(Vec::new());
            item.state = ItemState::Ready(data);
            install_item(&app, item, &mut vcx);
            for wrap in [false, true] {
                for width in [1100., 800.] {
                    vcx.simulate_resize(size(px(width), px(600.)));
                    vcx.update(|_, cx| {
                        app.update(cx, |this, cx| {
                            let data = this.active_data_mut().unwrap();
                            data.preferences.soft_wrap = Some(wrap);
                            data.rebuild_presentation();
                            cx.notify();
                        });
                    });
                    for _ in 0..2 {
                        vcx.update(|window, cx| {
                            let _ = window.draw(cx);
                        });
                    }
                    let (viewport, rows) = vcx.update(|_, cx| {
                        let data = app.read(cx).active_data().unwrap();
                        let rows = data
                            .rows
                            .iter()
                            .enumerate()
                            .filter_map(|(ix, row)| {
                                matches!(row, Row::Line { .. } | Row::Continuation(_)).then_some(ix)
                            })
                            .collect::<Vec<_>>();
                        (data.scroll.0.borrow().base_handle.bounds(), rows)
                    });
                    assert!(!rows.is_empty());
                    for ix in rows {
                        let selector = Box::leak(format!("diff-line-{ix}").into_boxed_str());
                        let row = vcx.debug_bounds(selector).unwrap();
                        assert_eq!(row.left(), viewport.left());
                        assert!(
                            row.right() >= viewport.right(),
                            "every unified line background must fill the viewport, including empty and wrapped lines: row {ix}: {row:?}, viewport {viewport:?}"
                        );
                    }
                }
            }
        }
    }

    #[gpui::test]
    fn horizontal_scroll_is_default_and_wrap_fits_both_views(cx: &mut gpui::TestAppContext) {
        let long = "abcdefghij".repeat(60);
        let client = Arc::new(FakePr {
            patch: Some(format!(
                "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old\n+{long}\n"
            )),
            ..Default::default()
        });
        let (window, app) = queue_window(cx, client);
        cx.update(|cx| app.update(cx, |this, cx| this.open_item(Source::Pr(loop_loc()), cx)));
        cx.run_until_parked();
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        vcx.dispatch_action(ShowChanges);
        for mode in [ViewMode::Unified, ViewMode::Split] {
            vcx.update(|window, cx| {
                app.update(cx, |this, cx| {
                    let data = this.active_data_mut().unwrap();
                    data.mode = mode;
                    data.preferences.soft_wrap = Some(false);
                    data.rebuild_presentation();
                    cx.notify();
                });
                let _ = window.draw(cx);
                let data = app.read(cx).active_data().unwrap();
                assert!(
                    data.scroll.0.borrow().base_handle.max_offset().x > px(0.),
                    "long lines must be reachable without wrapping"
                );
            });
            let code = match mode {
                ViewMode::Unified => "diff-code-2",
                ViewMode::Split => "diff-code-right-1",
            };
            let code_bounds = vcx.debug_bounds(code).expect("added code is laid out");
            vcx.update(|_, cx| {
                let data = app.read(cx).active_data().unwrap();
                let viewport = data.scroll.0.borrow().base_handle.bounds();
                assert!(code_bounds.left() < viewport.right() && code_bounds.right() > viewport.left(), "added code must start inside the viewport: {code_bounds:?}, viewport {viewport:?}");
                assert!(code_bounds.size.height > px(0.));
            });
            if mode == ViewMode::Split {
                let left_clip = vcx.debug_bounds("diff-code-left-clip-1").unwrap();
                let right_clip = vcx.debug_bounds("diff-code-right-clip-1").unwrap();
                vcx.update(|_, cx| {
                    let viewport = app
                        .read(cx)
                        .active_data()
                        .unwrap()
                        .scroll
                        .0
                        .borrow()
                        .base_handle
                        .bounds();
                    for clip in [left_clip, right_clip] {
                        assert!(
                            clip.size.width > px(0.),
                            "code must have a nonzero visible clip, not just a nonzero text layout"
                        );
                        assert!(clip.left() >= viewport.left() && clip.right() <= viewport.right());
                    }
                });
                vcx.update(|window, cx| {
                    app.update(cx, |this, cx| {
                        let data = this.active_data_mut().unwrap();
                        let handle = &data.scroll.0.borrow().base_handle;
                        let offset = handle.offset();
                        handle.set_offset(point(px(-150.), offset.y));
                        cx.notify();
                    });
                    let _ = window.draw(cx);
                });
                assert_eq!(
                    vcx.debug_bounds("diff-code-right-clip-1").unwrap(),
                    right_clip,
                    "horizontal scrolling must not move the split panes"
                );
                let scrolled_code = vcx.debug_bounds("diff-code-right-1").unwrap();
                assert_eq!(scrolled_code.left(), code_bounds.left() - px(150.));
                vcx.update(|_, cx| {
                    let hit = app
                        .read(cx)
                        .pane_text_hit(right_clip.center(), None)
                        .unwrap();
                    assert_eq!(
                        hit.0,
                        SelSide::Right,
                        "hit testing must follow the stationary divider"
                    );
                });
            }
            vcx.dispatch_action(ToggleSoftWrap);
            for _ in 0..2 {
                vcx.update(|window, cx| {
                    let _ = window.draw(cx);
                });
            }
            vcx.update(|_, cx| {
                let data = app.read(cx).active_data().unwrap();
                assert_eq!(data.scroll.0.borrow().base_handle.max_offset().x, px(0.));
                assert!(data
                    .rows
                    .iter()
                    .any(|row| matches!(row, Row::Continuation(_))));
            });
            vcx.dispatch_action(ToggleSoftWrap);
            for _ in 0..2 {
                vcx.update(|window, cx| {
                    let _ = window.draw(cx);
                });
            }
            let unwrapped = vcx
                .debug_bounds(code)
                .expect("code survives wrapping being turned off again");
            assert!(unwrapped.size.width > px(0.) && unwrapped.size.height > px(0.));
            if mode == ViewMode::Split {
                assert!(
                    vcx.debug_bounds("diff-code-right-clip-1")
                        .unwrap()
                        .size
                        .width
                        > px(0.)
                );
            }
        }
    }

    #[gpui::test]
    fn presentation_actions_persist_through_refresh_and_a_new_app(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| cx.bind_keys(presentation::keybindings()));
        let client = Arc::new(FakePr::default());
        let store = local_store::LocalStore::in_memory();
        store.save_diff_preferences(
            None,
            store::DiffPreferences {
                hide_whitespace: Some(true),
                collapse_unchanged: Some(true),
                soft_wrap: Some(true),
            },
        );
        let (window, app) = queue_window_with_store(cx, client.clone(), store.clone());
        cx.update(|cx| app.update(cx, |this, cx| this.open_item(Source::Pr(loop_loc()), cx)));
        cx.run_until_parked();
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
            let data = app.read(cx).active_data().unwrap();
            assert_eq!(data.preferences, store::DiffPreferences::OFF);
            assert_eq!(data.preference_overrides.hide_whitespace, None);
        });
        vcx.simulate_keystrokes("shift-w shift-k alt-z");
        let wanted = store::DiffPreferences {
            hide_whitespace: Some(true),
            collapse_unchanged: Some(true),
            soft_wrap: Some(true),
        };
        assert_eq!(store.diff_preferences(Some(&loop_loc())), wanted);
        cx.update(|cx| app.update(cx, |this, cx| this.refresh(cx)));
        cx.run_until_parked();
        cx.update(|cx| assert_eq!(app.read(cx).active_data().unwrap().preferences, wanted));
        let (second_window, second) = queue_window_with_store(cx, client, store.clone());
        cx.update(|cx| second.update(cx, |this, cx| this.open_item(Source::Pr(loop_loc()), cx)));
        cx.run_until_parked();
        let mut second_vcx = gpui::VisualTestContext::from_window(second_window, cx);
        second_vcx.update(|window, cx| {
            let _ = window.draw(cx);
            let data = second.read(cx).active_data().unwrap();
            assert_eq!(data.preferences, wanted);
            assert_eq!(data.minimap.len(), data.rows.len());
        });
        let mut new_pr = loop_loc();
        new_pr.number += 1;
        cx.update(|cx| second.update(cx, |this, cx| this.open_item(Source::Pr(new_pr), cx)));
        cx.run_until_parked();
        cx.update(|cx| {
            let data = second.read(cx).active_data().unwrap();
            assert_eq!(
                data.preferences,
                store::DiffPreferences::OFF,
                "new PRs must not inherit another PR's toggles or legacy global settings"
            );
            assert_eq!(
                store.diff_preferences(Some(&loop_loc())),
                wanted,
                "opening a new PR must not reset saved choices"
            );
        });
    }

    #[gpui::test]
    fn pr_position_resumes_through_full_fetch_wheel_queue_and_reopen(
        cx: &mut gpui::TestAppContext,
    ) {
        // The saved line is outside the patch: full contents must be ready
        // before the initial position is restored.
        let old: String = (1..=400).map(|n| format!("line {n}\n")).collect();
        let new = old.replace("line 200\n", "changed 200\n");
        let mut patch = String::new();
        let mut file_contents = HashMap::new();
        for path in ["a.rs", "b.rs"] {
            patch.push_str(&format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -197,7 +197,7 @@\n line 197\n line 198\n line 199\n-line 200\n+changed 200\n line 201\n line 202\n line 203\n"));
            file_contents.insert(("ba5ec0de".into(), path.into()), old.clone());
            file_contents.insert(("dead10cc".into(), path.into()), new.clone());
        }
        let client = Arc::new(FakePr {
            patch: Some(patch),
            file_contents,
            ..Default::default()
        });
        let store = local_store::LocalStore::in_memory();
        let saved = local_store::ReviewPosition {
            path: "b.rs".into(),
            new_line: Some(90),
            old_line: Some(90),
            row: 90,
            fraction: 0.375,
            ..Default::default()
        };
        store.save_position(&loop_loc(), &saved);
        let (window, app) = queue_window_with_store(cx, client.clone(), store.clone());
        cx.update(|cx| app.update(cx, |this, cx| this.open_item(Source::Pr(loop_loc()), cx)));
        cx.run_until_parked();
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        vcx.dispatch_action(ShowChanges);
        let (bounds, resumed) = vcx.update(|window, cx| {
            let _ = window.draw(cx);
            let data = app.read(cx).active_data().unwrap();
            assert_eq!(data.file_ix, 1);
            assert!(data.upgrades.contains_key(&1));
            assert_eq!(data.review_position().unwrap().new_line, Some(90));
            assert_eq!(data.review_position().unwrap().fraction, 0.375);
            assert_eq!(data.minimap.len(), data.rows.len());
            let state = data.scroll.0.borrow();
            assert_eq!(state.base_handle.offset().y, px(-90.375 * row_height()));
            (state.base_handle.bounds(), state.base_handle.offset())
        });
        vcx.simulate_event(gpui::ScrollEvent {
            position: bounds.center(),
            delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-137.))),
            ..Default::default()
        });
        let scrolled = vcx.update(|window, cx| {
            let _ = window.draw(cx);
            let data = app.read(cx).active_data().unwrap();
            assert_ne!(data.scroll.0.borrow().base_handle.offset(), resumed);
            data.review_position().unwrap()
        });
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(500));
        cx.run_until_parked();
        assert_eq!(
            store.position(&loop_loc()),
            Some(scrolled.clone()),
            "wheel movement is persisted without a navigation action"
        );
        cx.update(|cx| {
            app.update(cx, |this, cx| {
                this.back_to_queue(cx);
                this.open_item(Source::Pr(loop_loc()), cx);
                assert_eq!(
                    this.active_data().unwrap().review_position(),
                    Some(scrolled.clone())
                );
                this.close_item(this.active, cx);
                this.open_item(Source::Pr(loop_loc()), cx);
            })
        });
        cx.run_until_parked();
        vcx.dispatch_action(ShowChanges);
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
            assert_eq!(
                app.read(cx).active_data().unwrap().review_position(),
                Some(scrolled.clone())
            );
            app.update(cx, |this, cx| {
                // Save immediately, before the hunk jump has painted.
                this.step_hunk(1, cx);
                this.back_to_queue(cx);
            });
        });
        let jumped = store.position(&loop_loc()).unwrap();
        assert_eq!(jumped.path, "b.rs");
        assert_eq!(jumped.new_line, Some(197));
        assert_eq!(jumped.fraction, 0.);

        // A new app has no open item/scroll handle to reuse.
        let (second_window, second) = queue_window_with_store(cx, client, store.clone());
        cx.update(|cx| second.update(cx, |this, cx| this.open_item(Source::Pr(loop_loc()), cx)));
        cx.run_until_parked();
        cx.update(|cx| {
            let data = second.read(cx).active_data().unwrap();
            assert_eq!(data.file_ix, 1);
            assert_eq!(data.review_position().unwrap().new_line, Some(197));
        });
        let mut second_vcx = gpui::VisualTestContext::from_window(second_window, cx);
        second_vcx.dispatch_action(ShowChanges);
        second_vcx.update(|window, cx| {
            let _ = window.draw(cx);
            second.update(cx, |this, cx| {
                let data = this.active_data().unwrap();
                let bounds = data.scroll.0.borrow().base_handle.bounds();
                this.minimap_scrub_to(
                    point(bounds.right(), bounds.top() + bounds.size.height * 0.75),
                    cx,
                );
                let scrubbed = this.active_data().unwrap().review_position().unwrap();
                assert_ne!(scrubbed.new_line, Some(197));
                this.back_to_queue(cx);
                assert_eq!(store.position(&loop_loc()), Some(scrubbed));
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
        let BuiltRows {
            rows, hunk_rows, ..
        } = build_rows(RowsView {
            diff: &sample_diff(),
            upgrades: &HashMap::new(),
            comments: None,
            options: RowOptions {
                mode: ViewMode::Split,
                file_ix: 0,
                comments_visible: true,
                comment_wrap: COMMENT_WRAP_CHARS,
            },
        });
        assert_eq!(cursor_anchor(&rows, 0), None, "a file header never anchors");
        assert_eq!(cursor_anchor(&rows, 1), Some((CommentSide::Right, 1)));
        assert_eq!(cursor_anchor(&rows, 2), Some((CommentSide::Right, 2)));
        let h2 = hunk_rows[1];
        assert_eq!(cursor_anchor(&rows, h2 + 1), Some((CommentSide::Left, 11)));

        // Comment rows never anchor, whichever half they sit in.
        let index = sample_comments();
        let BuiltRows { rows, .. } = build_rows(RowsView {
            diff: &sample_diff(),
            upgrades: &HashMap::new(),
            comments: Some(&index),
            options: RowOptions {
                mode: ViewMode::Split,
                file_ix: 0,
                comments_visible: true,
                comment_wrap: COMMENT_WRAP_CHARS,
            },
        });
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
        let BuiltRows { rows, .. } = build_rows(RowsView {
            diff: &sample_diff(),
            upgrades: &HashMap::new(),
            comments: Some(&index),
            options: RowOptions {
                mode: ViewMode::Unified,
                file_ix: 0,
                comments_visible: true,
                comment_wrap: COMMENT_WRAP_CHARS,
            },
        });
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

    fn commenting_item(fake: &FakePr) -> ReviewItem {
        let comments = fake.comments.lock().unwrap().clone();
        let mut item = loop_pr_item(comments.clone());
        item.pane = ItemPane::Conversation;
        let ItemState::Ready(data) = &mut item.state else {
            unreachable!()
        };
        data.pr_meta.as_mut().unwrap().body.clear();
        data.commits = Some(github::PrCommits { commits: vec![] });
        data.discussion = Some(github::PrDiscussion::from_review_comments(comments));
        item
    }

    #[gpui::test]
    fn conversation_comments_reviews_and_replies_show_author_avatars(
        cx: &mut gpui::TestAppContext,
    ) {
        let fake = FakePr::with_threads(vec![
            rc(
                11,
                "a.rs",
                Some("RIGHT"),
                Some(1),
                "Question",
                "alice",
                "2026-01-01T00:00:00Z",
                None,
            ),
            rc(
                12,
                "a.rs",
                Some("RIGHT"),
                Some(1),
                "Reply",
                "dependabot[bot]",
                "2026-01-02T00:00:00Z",
                Some(11),
            ),
        ]);
        let (window, app) = queue_window(cx, fake.clone());
        let mut item = commenting_item(&fake);
        let ItemState::Ready(data) = &mut item.state else {
            unreachable!()
        };
        let discussion = data.discussion.as_mut().unwrap();
        for (id, review_state) in [(21, None), (22, Some("APPROVED"))] {
            discussion.comments.push(github::DiscussionComment {
                id,
                permissions: Default::default(),
                author: "alice".into(),
                body: "Looks good".into(),
                created_at: "2026-01-01T00:00:00Z".into(),
                reactions: vec![],
                review_state: review_state.map(str::to_owned),
                review_id: None,
            });
        }
        install_item(&app, item, cx);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        vcx.simulate_resize(size(px(1100.), px(1800.)));
        for _ in 0..3 {
            vcx.update(|window, cx| {
                let _ = window.draw(cx);
            });
            vcx.run_until_parked();
        }
        for selector in [
            "discussion-avatar-0-review-false-11",
            "discussion-avatar-0-review-false-12",
            "discussion-avatar-0-main-false-21",
            "discussion-avatar-0-main-true-22",
        ] {
            let avatar = vcx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("missing comment author avatar: {selector}"));
            assert!(avatar.size.width > px(0.));
            assert_eq!(avatar.size.width, avatar.size.height);
        }
    }

    #[gpui::test]
    fn conversation_timeline_hierarchy_and_review_reaction_regression(
        cx: &mut gpui::TestAppContext,
    ) {
        let fake = Arc::new(FakePr::default());
        let (window, app) = queue_window(cx, fake.clone());
        let mut item = commenting_item(&fake);
        let ItemState::Ready(data) = &mut item.state else {
            unreachable!()
        };
        let mut discussion = conversation::tests::sample_discussion();
        let mut comment = discussion.comments[0].clone();
        comment.id = 21;
        comment.body = "Top-level comment".into();
        comment.permissions = permitted_comment("IC21", false, false);
        discussion.comments = vec![comment.clone()];
        for (id, state, body) in [
            (22, "APPROVED", ""),
            (23, "CHANGES_REQUESTED", "Please adjust this."),
        ] {
            comment.id = id;
            comment.review_state = Some(state.into());
            comment.review_id = Some(format!("R{id}"));
            comment.body = body.into();
            // Old caches may advertise reactions on a submitted review. Neither
            // the cached counts nor the picker should appear in the timeline.
            comment.permissions = permitted_comment(&format!("R{id}"), false, false);
            discussion.comments.push(comment.clone());
        }
        let thread = &mut discussion.threads[0];
        thread.is_resolved = false;
        thread.comments.truncate(1);
        thread.comments[0].review_id = Some("R23".into());
        thread.comments[0].permissions = permitted_comment("PRRC1", false, false);
        let mut standalone = thread.clone();
        standalone.id = "T2".into();
        standalone.comments[0].id = 31;
        standalone.comments[0].review_id = None;
        standalone.comments[0].permissions = permitted_comment("PRRC31", false, false);
        discussion.threads.push(standalone);
        data.discussion = Some(discussion);
        data.posts
            .begin(99, None, "you".into(), "Posting comment".into());
        install_item(&app, item, cx);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        for width in [1100., 840.] {
            vcx.simulate_resize(size(px(width), px(1800.)));
            for _ in 0..3 {
                vcx.update(|window, cx| {
                    let _ = window.draw(cx);
                });
                vcx.run_until_parked();
            }
            let spine = vcx.debug_bounds("timeline-spine-0").unwrap();
            for selector in ["timeline-comment-0-21", "timeline-post-0-99"] {
                let card = vcx.debug_bounds(selector).unwrap();
                assert!(
                    card.left() < spine.left() && card.right() > spine.right(),
                    "top-level comment cards overlap the spine: {selector}"
                );
            }
            for (marker, summary, picker, reaction) in [
                (
                    "timeline-review-icon-0-22",
                    "discussion-0-main-true-22",
                    "reaction-picker-review-R22",
                    "reaction-review-R22-THUMBS_UP",
                ),
                (
                    "timeline-review-icon-0-23",
                    "discussion-0-main-true-23",
                    "reaction-picker-review-R23",
                    "reaction-review-R23-THUMBS_UP",
                ),
            ] {
                let marker = vcx.debug_bounds(marker).unwrap();
                assert_eq!(marker.center().x, spine.center().x);
                let summary = vcx.debug_bounds(summary).unwrap();
                assert!(
                    summary.left() > spine.right(),
                    "reviews are events to the right of the spine"
                );
                assert!(vcx.debug_bounds(picker).is_none());
                assert!(vcx.debug_bounds(reaction).is_none());
            }
            let grouped = vcx.debug_bounds("thread-header-0-T1").unwrap();
            let standalone = vcx.debug_bounds("thread-header-0-T2").unwrap();
            assert!(grouped.left() > spine.right());
            assert_eq!(
                grouped.left(),
                standalone.left(),
                "all inline threads share the same indentation"
            );
            let review = vcx.debug_bounds("discussion-0-main-true-23").unwrap();
            assert!(
                grouped.top() > review.bottom(),
                "inline threads follow their review summary"
            );
            for selector in [
                "reaction-picker-issue-IC21",
                "reaction-picker-review-comment-PRRC1",
                "reaction-picker-review-comment-PRRC31",
            ] {
                assert!(
                    vcx.debug_bounds(selector).is_some(),
                    "comments retain reactions: {selector}"
                );
            }
        }
        vcx.update(|window, cx| {
            app.update(cx, |this, cx| {
                this.set_reaction(
                    0,
                    github::CommentTarget::Review("R22".into()),
                    "HEART".into(),
                    true,
                    window,
                    cx,
                );
            });
        });
        vcx.run_until_parked();
        assert!(
            fake.posts().is_empty(),
            "submitted reviews cannot send reaction mutations"
        );
    }

    #[gpui::test]
    fn conversation_replies_are_indented_including_optimistic_and_pending(
        cx: &mut gpui::TestAppContext,
    ) {
        let fake = FakePr::with_threads(vec![
            rc(
                11,
                "a.rs",
                Some("RIGHT"),
                Some(1),
                "Question",
                "alice",
                "a",
                None,
            ),
            rc(
                12,
                "a.rs",
                Some("RIGHT"),
                Some(1),
                "Reply",
                "bob",
                "b",
                Some(11),
            ),
        ]);
        *fake.pending.lock().unwrap() = Some(github::PendingReview {
            id: 91,
            node_id: "pending".into(),
            body: String::new(),
            comments: vec![github::PendingComment {
                id: 13,
                node_id: "draft".into(),
                path: "a.rs".into(),
                side: CommentSide::Right,
                line: Some(1),
                start_line: None,
                body: "Pending reply".into(),
                in_reply_to_id: Some(11),
            }],
        });
        let (window, app) = queue_window(cx, fake.clone());
        let mut item = commenting_item(&fake);
        let ItemState::Ready(data) = &mut item.state else {
            unreachable!()
        };
        data.posts
            .begin(99, Some(11), "you".into(), "Posting reply".into());
        install_item(&app, item, cx);
        cx.update(|cx| app.update(cx, |this, cx| this.resume_session(0, cx)));
        cx.run_until_parked();
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        vcx.simulate_resize(size(px(1100.), px(1800.)));
        for _ in 0..3 {
            vcx.update(|window, cx| {
                let _ = window.draw(cx);
            });
            vcx.run_until_parked();
        }
        let root = vcx.debug_bounds("discussion-0-review-false-11").unwrap();
        for selector in [
            "discussion-0-review-false-12",
            "discussion-0-post-99",
            "conversation-draft-0-13",
        ] {
            let reply = vcx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("missing {selector}"));
            assert_eq!(
                reply.left() - root.left(),
                px(threads::REPLY_INDENT),
                "{selector}"
            );
            assert!(
                reply.right() <= root.right(),
                "reply must stay inside the thread"
            );
        }
    }

    #[gpui::test]
    fn inline_reply_headers_and_markdown_are_indented_in_both_views(cx: &mut gpui::TestAppContext) {
        for mode in [ViewMode::Unified, ViewMode::Split] {
            for side in ["LEFT", "RIGHT"] {
                let fake = FakePr::with_threads(vec![
                    rc(11, "a.rs", Some(side), Some(1), "Root", "alice", "a", None),
                    rc(
                        12,
                        "a.rs",
                        Some(side),
                        Some(1),
                        "Reply",
                        "bob",
                        "b",
                        Some(11),
                    ),
                    rc(
                        13,
                        "a.rs",
                        Some(side),
                        Some(1),
                        "Another reply",
                        "you",
                        "c",
                        Some(12),
                    ),
                ]);
                let (window, app) = queue_window(cx, fake.clone());
                let mut item = loop_pr_item(fake.comments.lock().unwrap().clone());
                let ItemState::Ready(data) = &mut item.state else {
                    unreachable!()
                };
                data.mode = mode;
                data.diff = sample_diff(); // Line 1 exists on both sides.
                data.set_rows(data.row_build());
                install_item(&app, item, cx);
                let mut vcx = gpui::VisualTestContext::from_window(window, cx);
                vcx.simulate_resize(size(px(1100.), px(1800.)));
                for _ in 0..3 {
                    vcx.update(|window, cx| {
                        let _ = window.draw(cx);
                    });
                    vcx.run_until_parked();
                }
                let root_header = vcx.debug_bounds("inline-avatar-11").unwrap();
                let root_body = vcx.debug_bounds("inline-text-0-11-0-card").unwrap();
                for (header_selector, body_selector) in [
                    ("inline-avatar-12", "inline-text-0-12-0-card"),
                    ("inline-avatar-13", "inline-text-0-13-0-card"),
                ] {
                    let header = vcx.debug_bounds(header_selector).unwrap();
                    let body = vcx.debug_bounds(body_selector).unwrap();
                    assert_eq!(
                        header.left() - root_header.left(),
                        px(threads::REPLY_INDENT)
                    );
                    assert_eq!(body.left() - root_body.left(), px(threads::REPLY_INDENT));
                    assert!(body.size.width <= root_body.size.width);
                    if mode == ViewMode::Split {
                        assert!(body.right() <= root_body.right());
                    }
                }
            }
        }
    }

    fn click_commenting(vcx: &mut gpui::VisualTestContext, selector: &'static str) {
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        // Markdown measurements can change the reserved rows after the first
        // frame. Click the settled card, not its initial plain-text estimate.
        vcx.run_until_parked();
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let bounds = vcx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("missing {selector}"));
        let point = bounds.center();
        vcx.simulate_mouse_move(point, None, gpui::Modifiers::default());
        vcx.simulate_mouse_down(point, MouseButton::Left, gpui::Modifiers::default());
        vcx.simulate_mouse_up(point, MouseButton::Left, gpui::Modifiers::default());
        vcx.run_until_parked();
    }

    #[gpui::test]
    fn inline_comment_times_build_tooltips_on_hover_in_both_views(cx: &mut gpui::TestAppContext) {
        let tooltips = std::rc::Rc::new(std::cell::Cell::new(0));
        let observed = tooltips.clone();
        let _subscription = cx.update(|cx| {
            cx.observe_new::<gpui::component::tooltip::Tooltip>(move |_, _, _| {
                observed.set(observed.get() + 1);
            })
        });
        for mode in [ViewMode::Unified, ViewMode::Split] {
            let fake = Arc::new(FakeQueue {
                asked: std::sync::Mutex::new(Vec::new()),
            });
            let (window, app) = queue_window(cx, fake);
            let mut vcx = gpui::VisualTestContext::from_window(window, cx);
            let mut item = loop_pr_item(vec![
                rc(
                    11,
                    "a.rs",
                    Some("RIGHT"),
                    Some(1),
                    "root",
                    "alice",
                    "2026-01-02T00:00:00Z",
                    None,
                ),
                rc(
                    12,
                    "a.rs",
                    Some("RIGHT"),
                    Some(1),
                    "reply",
                    "bob",
                    "2026-01-02T00:01:00Z",
                    Some(11),
                ),
            ]);
            let ItemState::Ready(data) = &mut item.state else {
                unreachable!()
            };
            data.mode = mode;
            data.set_rows(data.row_build());
            install_item(&app, item, cx);
            for selector in ["inline-comment-time-11", "inline-comment-time-12"] {
                vcx.simulate_mouse_move(
                    gpui::point(px(0.), px(0.)),
                    None,
                    gpui::Modifiers::default(),
                );
                vcx.update(|window, cx| {
                    let _ = window.draw(cx);
                });
                vcx.run_until_parked();
                vcx.update(|window, cx| {
                    let _ = window.draw(cx);
                });
                let bounds = vcx
                    .debug_bounds(selector)
                    .expect("inline timestamp is visible");
                let before = tooltips.get();
                vcx.simulate_mouse_move(bounds.center(), None, gpui::Modifiers::default());
                vcx.run_until_parked();
                vcx.executor()
                    .advance_clock(std::time::Duration::from_secs(1));
                vcx.run_until_parked();
                vcx.update(|window, cx| {
                    let _ = window.draw(cx);
                });
                assert!(
                    tooltips.get() > before,
                    "hovering {selector} must build a timestamp tooltip"
                );
            }
        }
    }

    #[test]
    fn resolved_diff_threads_are_one_row_in_both_modes_and_keep_navigation_counts() {
        let comments = vec![
            rc(
                11,
                "a.rs",
                Some("RIGHT"),
                Some(1),
                "root\n\n![image](https://example.com/a.png)",
                "alice",
                "a",
                None,
            ),
            rc(
                12,
                "a.rs",
                Some("RIGHT"),
                Some(1),
                "reply",
                "bob",
                "b",
                Some(11),
            ),
            rc(
                13,
                "a.rs",
                Some("RIGHT"),
                Some(1),
                "still open",
                "bob",
                "c",
                None,
            ),
        ];
        let mut item = loop_pr_item(comments.clone());
        let ItemState::Ready(data) = &mut item.state else {
            unreachable!()
        };
        let mut discussion = github::PrDiscussion::from_review_comments(comments);
        discussion.threads[0].is_resolved = true;
        data.discussion = Some(discussion);
        for mode in [ViewMode::Unified, ViewMode::Split] {
            data.mode = mode;
            data.expanded_threads.clear();
            data.set_rows(data.row_build());
            assert_eq!(
                data.rows
                    .iter()
                    .filter(|row| matches!(
                        row,
                        Row::CommentHeader {
                            comment_id: 11,
                            collapsed: true,
                            ..
                        }
                    ))
                    .count(),
                1
            );
            assert!(!data.rows.iter().any(|row| matches!(
                row,
                Row::CommentHeader { comment_id: 12, .. } | Row::CommentActions { root_id: 11, .. }
            )));
            assert!(!data.rows.iter().any(|row| matches!(row, Row::CommentBody { text, .. } if matches!(text.comment_id, 11 | 12))));
            assert!(data
                .rows
                .iter()
                .any(|row| matches!(row, Row::CommentActions { root_id: 13, .. })));
            assert_eq!(data.comments.as_ref().unwrap().counts["a.rs"], (3, 0));
            assert!(data
                .file_rows
                .iter()
                .all(|&ix| matches!(data.rows[ix], Row::FileHeader { .. })));
            assert!(data
                .hunk_rows
                .iter()
                .all(|&ix| matches!(data.rows[ix], Row::Line { .. } | Row::SplitLine { .. })));
            data.expanded_threads.insert("11".into());
            data.set_rows(data.row_build());
            assert!(data
                .rows
                .iter()
                .any(|row| matches!(row, Row::CommentHeader { comment_id: 12, .. })));
            assert!(data
                .rows
                .iter()
                .any(|row| matches!(row, Row::CommentBody { text, .. } if text.comment_id == 11)));
        }
    }

    #[gpui::test]
    fn thread_resolution_controls_sync_both_views_and_show_failures(cx: &mut gpui::TestAppContext) {
        use github::GitHubClient;
        use std::sync::atomic::Ordering::SeqCst;
        let fake = FakePr::with_threads(vec![rc(
            11,
            "a.rs",
            Some("RIGHT"),
            Some(1),
            "question",
            "alice",
            "a",
            None,
        )]);
        fake.resolutions.lock().unwrap().insert("11".into(), false);
        let (window, app) = queue_window(cx, fake.clone());
        let mut item = loop_pr_item(fake.comments.lock().unwrap().clone());
        if let ItemState::Ready(data) = &mut item.state {
            data.discussion = Some(fake.pr_discussion(&loop_loc()).unwrap());
        }
        install_item(&app, item, cx);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        click_commenting(&mut vcx, "thread-resolve-0-11");
        cx.update(|cx| {
            let data = app.read(cx).active_data().unwrap();
            assert!(data.discussion.as_ref().unwrap().threads[0].is_resolved);
            assert_eq!(
                data.rows
                    .iter()
                    .filter(|row| rows::is_comment_row(row))
                    .count(),
                1
            );
            assert!(
                !data.expanded_threads.contains("11"),
                "resolving collapses the thread"
            );
        });
        click_commenting(&mut vcx, "thread-expand-0-11");
        cx.update(|cx| {
            let data = app.read(cx).active_data().unwrap();
            assert!(data.expanded_threads.contains("11"));
            assert!(data
                .rows
                .iter()
                .any(|row| matches!(row, Row::CommentActions { root_id: 11, .. })));
        });
        click_commenting(&mut vcx, "thread-expand-0-11");
        cx.update(|cx| {
            let data = app.read(cx).active_data().unwrap();
            assert!(!data.expanded_threads.contains("11"));
            assert_eq!(
                data.rows
                    .iter()
                    .filter(|row| rows::is_comment_row(row))
                    .count(),
                1
            );
        });
        vcx.dispatch_action(ShowConversation);
        click_commenting(&mut vcx, "thread-resolve-0-11");
        cx.update(|cx| {
            let data = app.read(cx).active_data().unwrap();
            assert!(!data.discussion.as_ref().unwrap().threads[0].is_resolved);
            assert!(data
                .rows
                .iter()
                .any(|row| matches!(row, Row::CommentActions { root_id: 11, .. })));
            assert!(
                !data.expanded_threads.contains("11"),
                "the resolve button must not also toggle the header"
            );
        });
        assert_eq!(
            fake.posts(),
            vec![
                Post::Resolve {
                    thread_id: "11".into(),
                    resolved: true
                },
                Post::Resolve {
                    thread_id: "11".into(),
                    resolved: false
                }
            ]
        );
        fake.fail.store(true, SeqCst);
        click_commenting(&mut vcx, "thread-resolve-0-11");
        cx.update(|cx| {
            let app = app.read(cx);
            assert!(
                !app.active_data()
                    .unwrap()
                    .discussion
                    .as_ref()
                    .unwrap()
                    .threads[0]
                    .is_resolved
            );
            assert!(app.sessions[&0]
                .error
                .as_ref()
                .unwrap()
                .contains("resolution failed"));
        });
    }

    #[gpui::test]
    fn thread_resolution_requires_server_permissions(cx: &mut gpui::TestAppContext) {
        use github::GitHubClient;
        let fake = FakePr::with_threads(vec![rc(
            11,
            "a.rs",
            Some("RIGHT"),
            Some(1),
            "read only",
            "alice",
            "a",
            None,
        )]);
        let (window, app) = queue_window(cx, fake.clone());
        let mut item = loop_pr_item(fake.comments.lock().unwrap().clone());
        if let ItemState::Ready(data) = &mut item.state {
            data.discussion = Some(fake.pr_discussion(&loop_loc()).unwrap());
        }
        install_item(&app, item, cx);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        vcx.update(|window, cx| {
            app.update(cx, |this, cx| this.set_thread_resolved(0, "11", window, cx));
            let _ = window.draw(cx);
        });
        assert!(vcx.debug_bounds("thread-resolve-0-11").is_none());
        assert!(fake.posts().is_empty());
        vcx.dispatch_action(ShowConversation);
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        assert!(vcx.debug_bounds("thread-resolve-0-11").is_none());
    }

    #[gpui::test]
    fn conversation_main_composer_scrolls_into_view(cx: &mut gpui::TestAppContext) {
        conversation_composer_visibility(cx, false, true);
    }

    #[gpui::test]
    fn conversation_reply_composer_scrolls_into_view(cx: &mut gpui::TestAppContext) {
        conversation_composer_visibility(cx, true, true);
    }

    #[gpui::test]
    fn visible_conversation_composer_does_not_scroll(cx: &mut gpui::TestAppContext) {
        conversation_composer_visibility(cx, false, false);
    }

    fn conversation_composer_visibility(
        cx: &mut gpui::TestAppContext,
        reply: bool,
        near_fold: bool,
    ) {
        let fake = FakePr::with_threads(if reply {
            vec![rc(
                11,
                "a.rs",
                Some("RIGHT"),
                Some(1),
                "question",
                "alice",
                "2026-01-01T00:00:00Z",
                None,
            )]
        } else {
            vec![]
        });
        let (window, app) = queue_window(cx, fake.clone());
        let mut item = commenting_item(&fake);
        let ItemState::Ready(data) = &mut item.state else {
            unreachable!()
        };
        if near_fold {
            data.pr_meta.as_mut().unwrap().body = "A tall description.\n\n".repeat(100);
        }
        let scroll = data.conv_scroll.clone();
        install_item(&app, item, cx);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let selector = if reply {
            "conversation-compose-0-11"
        } else {
            "conversation-compose-0-0"
        };
        let button = vcx.debug_bounds(selector).expect("comment button");
        let viewport = scroll.bounds();
        // Put the button just above the fold: replacing it with the much
        // taller composer must reveal the full card, not just its input.
        // In the other case, the short conversation already has enough room.
        if near_fold {
            scroll.set_offset(point(px(0.), viewport.bottom() - button.bottom() - px(8.)));
        }
        vcx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        let before = scroll.offset();
        click_commenting(&mut vcx, selector);
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        vcx.run_until_parked();
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let card = vcx.debug_bounds("composer-card").expect("open composer");
        let viewport = scroll.bounds();
        assert!(
            card.top() >= viewport.top(),
            "composer header is visible: {card:?} in {viewport:?}"
        );
        assert!(
            card.bottom() <= viewport.bottom(),
            "composer actions are visible: {card:?} in {viewport:?}"
        );
        if near_fold {
            assert!(
                scroll.offset().y < before.y,
                "opening the card scrolls down"
            );
        } else {
            assert_eq!(
                scroll.offset(),
                before,
                "an already-visible card does not move the viewport"
            );
        }
        // Revealing is one-shot; later redraws must not fight manual scrolling.
        scroll.set_offset(point(px(0.), px(0.)));
        vcx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        vcx.run_until_parked();
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        assert_eq!(scroll.offset().y, px(0.));
    }

    #[gpui::test]
    fn conversation_main_comment_posts_and_refreshes_timeline(cx: &mut gpui::TestAppContext) {
        let fake = FakePr::with_threads(vec![]);
        let (window, app) = queue_window(cx, fake.clone());
        install_item(&app, commenting_item(&fake), cx);
        // Include conflicting diff shortcuts: normal text must stay text.
        cx.update(|cx| {
            cx.bind_keys([
                KeyBinding::new("c", ToggleComments, Some("ReviewApp")),
                KeyBinding::new("r", Refresh, Some("ReviewApp")),
                KeyBinding::new("cmd-enter", SubmitReview, Some("ReviewApp")),
            ])
        });
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        click_commenting(&mut vcx, "conversation-compose-0-0");
        vcx.simulate_input("conversation feedback");
        vcx.simulate_keystrokes("c r");
        cx.update(|cx| {
            let composer = app.read(cx).composer.as_ref().unwrap();
            assert_eq!(composer.placement, ComposerPlacement::ConversationMain);
            assert_eq!(
                composer.input.read(cx).value().as_str(),
                "conversation feedbackcr"
            );
        });
        vcx.simulate_keystrokes("cmd-enter");
        cx.run_until_parked();
        assert_eq!(
            fake.posts(),
            vec![Post::Conversation {
                body: "conversation feedbackcr".into()
            }]
        );
        cx.update(|cx| {
            let app = app.read(cx);
            assert!(app.composer.is_none());
            let discussion = app.active_data().unwrap().discussion.as_ref().unwrap();
            assert!(matches!(&conversation::timeline(discussion)[..], [conversation::Entry::Comment(c)] if c.body == "conversation feedbackcr"));
            assert!(discussion.threads.is_empty());
            assert_eq!(app.store.snapshot("acme/widgets", 7).unwrap().discussion.unwrap()["comments"][0]["body"], "conversation feedbackcr");
        });
    }

    #[gpui::test]
    fn conversation_replies_target_outdated_resolved_thread_roots(cx: &mut gpui::TestAppContext) {
        let fake = FakePr::with_threads(vec![rc(
            11,
            "deleted.rs",
            Some("LEFT"),
            None,
            "historical question",
            "alice",
            "2026-01-01T00:00:00Z",
            None,
        )]);
        let (window, app) = queue_window(cx, fake.clone());
        let mut item = commenting_item(&fake);
        let ItemState::Ready(data) = &mut item.state else {
            unreachable!()
        };
        data.discussion.as_mut().unwrap().threads[0].is_resolved = true;
        data.expanded_threads.insert("11".into());
        install_item(&app, item, cx);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        click_commenting(&mut vcx, "conversation-compose-0-11");
        vcx.update(|window, cx| {
            app.update(cx, |this, cx| {
                let composer = this.composer.as_ref().unwrap();
                assert_eq!(
                    composer.placement,
                    ComposerPlacement::ConversationThread("11".into())
                );
                assert_eq!(composer.reply_to, Some(11));
                assert_eq!(composer.line, 0, "no current line is required to reply");
                composer
                    .input
                    .update(cx, |state, cx| state.set_value("answered here", window, cx));
                this.submit_composer(window, cx);
            })
        });
        cx.run_until_parked();
        assert_eq!(
            fake.posts(),
            vec![Post::Reply {
                comment_id: 11,
                body: "answered here".into()
            }]
        );
        cx.update(|cx| {
            let app = app.read(cx);
            let discussion = app.active_data().unwrap().discussion.as_ref().unwrap();
            assert!(
                discussion.comments.is_empty(),
                "a reply never becomes a main-thread comment"
            );
            assert_eq!(discussion.threads[0].comments.len(), 2);
            assert_eq!(discussion.threads[0].comments[1].body, "answered here");
            assert!(app.composer.is_none());
        });
    }

    #[gpui::test]
    fn conversation_drafts_preserve_web_summary_and_reply_thread(cx: &mut gpui::TestAppContext) {
        let fake = FakePr::with_threads(vec![rc(
            11,
            "a.rs",
            Some("RIGHT"),
            Some(1),
            "question",
            "alice",
            "2026-01-01T00:00:00Z",
            None,
        )]);
        *fake.pending.lock().unwrap() = Some(github::PendingReview {
            id: 91,
            node_id: "pending".into(),
            body: "web summary".into(),
            comments: vec![],
        });
        let (window, app) = queue_window(cx, fake.clone());
        install_item(&app, commenting_item(&fake), cx);
        cx.update(|cx| app.update(cx, |this, cx| this.resume_session(0, cx)));
        cx.run_until_parked();
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        vcx.update(|window, cx| {
            app.update(cx, |this, cx| {
                this.open_comment_composer(
                    ComposerPlacement::ConversationMain,
                    None,
                    String::new(),
                    CommentSide::Right,
                    0,
                    0,
                    window,
                    cx,
                );
                let composer = this.composer.as_ref().unwrap();
                assert_eq!(this.composer_submit_label(composer), "Add to review");
                composer
                    .input
                    .update(cx, |state, cx| state.set_value("extra summary", window, cx));
                this.submit_composer(window, cx);
            })
        });
        cx.run_until_parked();
        vcx.update(|window, cx| {
            app.update(cx, |this, cx| {
                assert!(this.composer.is_none());
                assert_eq!(
                    this.sessions[&0].state.pending.as_ref().unwrap().body,
                    "web summary\n\nextra summary"
                );
                this.open_comment_composer(
                    ComposerPlacement::ConversationThread("11".into()),
                    Some(11),
                    "a.rs".into(),
                    CommentSide::Right,
                    1,
                    0,
                    window,
                    cx,
                );
                let composer = this.composer.as_ref().unwrap();
                assert_eq!(this.composer_submit_label(composer), "Add to review");
                composer
                    .input
                    .update(cx, |state, cx| state.set_value("draft reply", window, cx));
                this.submit_composer(window, cx);
            })
        });
        cx.run_until_parked();
        assert!(fake.posts().is_empty());
        cx.update(|cx| {
            let app = app.read(cx);
            let review = app.store.review_session(&loop_loc()).pending.unwrap();
            assert_eq!(review.body, "web summary\n\nextra summary");
            assert_eq!(review.comments[0].in_reply_to_id, Some(11));
            assert_eq!(review.comments[0].body, "draft reply");
            assert!(
                app.active_data()
                    .unwrap()
                    .discussion
                    .as_ref()
                    .unwrap()
                    .threads[0]
                    .comments
                    .len()
                    == 1
            );
        });
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        // The summary is prefilled and submitted by the existing finish-review flow.
        vcx.update(|window, cx| {
            app.update(cx, |this, cx| {
                this.open_review(window, cx);
                assert_eq!(
                    this.review
                        .as_ref()
                        .unwrap()
                        .input
                        .read(cx)
                        .value()
                        .as_str(),
                    "web summary\n\nextra summary"
                );
                this.submit_review(window, cx);
            })
        });
        cx.run_until_parked();
        assert_eq!(
            fake.posts(),
            vec![Post::Review {
                verdict: github::ReviewVerdict::Approve,
                body: "web summary\n\nextra summary".into()
            }]
        );
    }

    #[gpui::test]
    fn conversation_main_can_start_a_review(cx: &mut gpui::TestAppContext) {
        conversation_start_review(cx, false);
    }

    #[gpui::test]
    fn conversation_reply_can_start_a_review(cx: &mut gpui::TestAppContext) {
        conversation_start_review(cx, true);
    }

    fn conversation_start_review(cx: &mut gpui::TestAppContext, reply: bool) {
        let fake = FakePr::with_threads(if reply {
            vec![rc(
                11,
                "a.rs",
                Some("RIGHT"),
                Some(1),
                "question",
                "alice",
                "2026-01-01T00:00:00Z",
                None,
            )]
        } else {
            vec![]
        });
        let (window, app) = queue_window(cx, fake.clone());
        install_item(&app, commenting_item(&fake), cx);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        click_commenting(
            &mut vcx,
            if reply {
                "conversation-compose-0-11"
            } else {
                "conversation-compose-0-0"
            },
        );
        cx.update(|cx| {
            let app = app.read(cx);
            assert_eq!(
                app.composer_submit_label(app.composer.as_ref().unwrap()),
                if reply { "Reply" } else { "Comment" }
            );
        });
        vcx.simulate_input("draft feedback");
        click_commenting(&mut vcx, "composer-start-review");
        assert!(fake.posts().is_empty(), "Start review never publishes");
        cx.update(|cx| {
            let app = app.read(cx);
            assert!(app.composer.is_none(), "the draft was saved");
            let review = app.store.review_session(&loop_loc()).pending.unwrap();
            if reply {
                assert_eq!(review.comments.len(), 1);
                assert_eq!(review.comments[0].in_reply_to_id, Some(11));
                assert_eq!(review.comments[0].body, "draft feedback");
            } else {
                assert_eq!(review.body, "draft feedback");
                assert!(review.comments.is_empty());
            }
        });
    }

    #[gpui::test]
    fn conversation_main_placeholder_updates_in_place_after_delayed_refresh(
        cx: &mut gpui::TestAppContext,
    ) {
        conversation_placeholder(cx, false);
    }

    #[gpui::test]
    fn conversation_reply_placeholder_stays_in_thread_until_loaded(cx: &mut gpui::TestAppContext) {
        conversation_placeholder(cx, true);
    }

    fn conversation_placeholder(cx: &mut gpui::TestAppContext, reply: bool) {
        let fake = FakePr::with_threads(if reply {
            vec![rc(
                11,
                "a.rs",
                Some("RIGHT"),
                Some(1),
                "question",
                "alice",
                "2026-01-01T00:00:00Z",
                None,
            )]
        } else {
            vec![]
        });
        fake.stale_discussion
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let (window, app) = queue_window(cx, fake.clone());
        install_item(&app, commenting_item(&fake), cx);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        click_commenting(
            &mut vcx,
            if reply {
                "conversation-compose-0-11"
            } else {
                "conversation-compose-0-0"
            },
        );
        vcx.update(|window, cx| {
            app.update(cx, |this, cx| {
                this.composer
                    .as_ref()
                    .unwrap()
                    .input
                    .update(cx, |state, cx| {
                        state.set_value("visible immediately", window, cx)
                    });
                this.submit_composer(window, cx);
                let data = this.active_data().unwrap();
                assert_eq!(
                    data.posts.pending.len(),
                    1,
                    "the placeholder exists before any network response"
                );
                let post = &data.posts.pending[0];
                assert_eq!(post.token, 1);
                assert_eq!(post.reply_to, reply.then_some(11));
                assert_eq!(post.comment.body, "visible immediately");
                assert_eq!(post.comment.created_at, "Posting…");
            })
        });
        cx.run_until_parked();
        cx.update(|cx| {
            let app = app.read(cx);
            assert!(app.composer.is_none());
            let data = app.active_data().unwrap();
            assert_eq!(
                data.posts.pending.len(),
                1,
                "a successful POST plus a stale GET must not make the comment disappear"
            );
            assert_eq!(
                data.posts.pending[0].comment.created_at,
                "Posted · syncing…"
            );
            assert!(data.discussion.as_ref().unwrap().comments.is_empty());
            assert_eq!(
                app.store
                    .snapshot("acme/widgets", 7)
                    .unwrap()
                    .discussion
                    .unwrap()["comments"]
                    .as_array()
                    .unwrap()
                    .len(),
                0,
                "placeholders are not cached as published comments"
            );
        });
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let placeholder_bounds = vcx
            .debug_bounds("discussion-0-post-1")
            .expect("placeholder rendered in the conversation");
        // A failed GET must retain the already-posted placeholder too, without
        // retrying the write or reopening the composer.
        fake.fail_discussion
            .store(true, std::sync::atomic::Ordering::SeqCst);
        cx.update(|cx| app.update(cx, |this, cx| this.refetch_comments(0, loop_loc(), cx)));
        cx.run_until_parked();
        cx.update(|cx| assert_eq!(app.read(cx).active_data().unwrap().posts.pending.len(), 1));
        fake.fail_discussion
            .store(false, std::sync::atomic::Ordering::SeqCst);
        fake.stale_discussion
            .store(false, std::sync::atomic::Ordering::SeqCst);
        cx.update(|cx| app.update(cx, |this, cx| this.refetch_comments(0, loop_loc(), cx)));
        cx.run_until_parked();
        cx.update(|cx| {
            let app = app.read(cx);
            let data = app.active_data().unwrap();
            assert!(data.posts.pending.is_empty());
            let discussion = data.discussion.as_ref().unwrap();
            if reply {
                assert!(discussion.comments.is_empty());
                assert_eq!(discussion.threads[0].comments.len(), 2);
            } else {
                assert_eq!(discussion.comments.len(), 1);
                assert!(discussion.threads.is_empty());
            }
        });
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let loaded_bounds = vcx
            .debug_bounds("discussion-0-post-1")
            .expect("loaded comment reuses the placeholder identity");
        assert_eq!(
            loaded_bounds.origin, placeholder_bounds.origin,
            "the loaded comment replaces the placeholder in place"
        );
        assert_eq!(fake.posts().len(), 1, "refreshing must never repost");
    }

    #[gpui::test]
    fn cmd_enter_submits_diff_comment_without_a_newline(cx: &mut gpui::TestAppContext) {
        composer_cmd_enter(cx, ComposerPlacement::Diff);
    }

    #[gpui::test]
    fn cmd_enter_submits_conversation_comment_without_a_newline(cx: &mut gpui::TestAppContext) {
        composer_cmd_enter(cx, ComposerPlacement::ConversationMain);
    }

    #[gpui::test]
    fn cmd_enter_submits_conversation_reply_without_a_newline(cx: &mut gpui::TestAppContext) {
        composer_cmd_enter(cx, ComposerPlacement::ConversationThread("11".into()));
    }

    fn composer_cmd_enter(cx: &mut gpui::TestAppContext, placement: ComposerPlacement) {
        let fake = FakePr::with_threads(vec![rc(
            11,
            "a.rs",
            Some("RIGHT"),
            Some(1),
            "question",
            "alice",
            "2026-01-01T00:00:00Z",
            None,
        )]);
        let (window, app) = queue_window(cx, fake.clone());
        let mut item = commenting_item(&fake);
        if placement == ComposerPlacement::Diff {
            item.pane = ItemPane::Changes;
        }
        install_item(&app, item, cx);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        let input = vcx.update(|window, cx| {
            app.update(cx, |this, cx| {
                this.open_comment_composer(
                    placement.clone(),
                    matches!(placement, ComposerPlacement::ConversationThread(_)).then_some(11),
                    "a.rs".into(),
                    CommentSide::Right,
                    1,
                    1,
                    window,
                    cx,
                );
                this.composer.as_ref().unwrap().input.clone()
            })
        });
        vcx.simulate_input("beforeafter");
        // Plain Enter still inserts a newline in the middle of the comment.
        vcx.simulate_keystrokes("left left left left left enter");
        cx.update(|cx| assert_eq!(input.read(cx).value().as_str(), "before\nafter"));
        assert!(fake.posts().is_empty());
        vcx.simulate_keystrokes("cmd-enter");
        cx.run_until_parked();
        cx.update(|cx| {
            assert_eq!(input.read(cx).value().as_str(), "before\nafter");
            assert!(app.read(cx).composer.is_none());
            assert!(app.read(cx).review.is_none());
        });
        let body = "before\nafter".into();
        let expected = match placement {
            ComposerPlacement::Diff => Post::Comment {
                commit_id: "dead10cc".into(),
                path: "a.rs".into(),
                side: "RIGHT".into(),
                line: 1,
                body,
            },
            ComposerPlacement::ConversationMain => Post::Conversation { body },
            ComposerPlacement::ConversationThread(_) => Post::Reply {
                comment_id: 11,
                body,
            },
            ComposerPlacement::ConversationComment(_) | ComposerPlacement::PendingTray(_) => {
                unreachable!("this helper tests new comments only")
            }
        };
        assert_eq!(fake.posts(), vec![expected]);
    }

    #[gpui::test]
    fn conversation_failed_comment_keeps_text_and_escape_cancels(cx: &mut gpui::TestAppContext) {
        let fake = FakePr::with_threads(vec![]);
        fake.fail.store(true, std::sync::atomic::Ordering::SeqCst);
        let (window, app) = queue_window(cx, fake.clone());
        install_item(&app, commenting_item(&fake), cx);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        click_commenting(&mut vcx, "conversation-compose-0-0");
        vcx.simulate_input("keep this text");
        vcx.simulate_keystrokes("cmd-enter");
        cx.run_until_parked();
        cx.update(|cx| {
            let app = app.read(cx);
            let composer = app.composer.as_ref().unwrap();
            assert!(!composer.in_flight);
            assert!(composer.error.is_some());
            assert_eq!(composer.input.read(cx).value().as_str(), "keep this text");
            assert!(
                app.active_data().unwrap().posts.pending.is_empty(),
                "a failed post does not leave a fake published comment"
            );
        });
        vcx.simulate_keystrokes("escape");
        cx.run_until_parked();
        cx.update(|cx| assert!(app.read(cx).composer.is_none()));
        assert_eq!(fake.posts().len(), 1);
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

    #[gpui::test]
    fn pending_review_hides_single_comment_and_collects_replies_as_drafts(
        cx: &mut gpui::TestAppContext,
    ) {
        let fake = FakePr::with_threads(vec![rc(
            11,
            "a.rs",
            Some("RIGHT"),
            Some(1),
            "published root",
            "alice",
            "2026-01-01T00:00:00Z",
            None,
        )]);
        *fake.pending.lock().unwrap() = Some(github::PendingReview {
            id: 91,
            node_id: "pending".into(),
            body: "web review".into(),
            comments: vec![],
        });
        let (window, app) = queue_window(cx, fake.clone());
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        install_item(
            &app,
            loop_pr_item(fake.comments.lock().unwrap().clone()),
            cx,
        );
        vcx.update(|window, cx| {
            app.update(cx, |this, cx| {
                // Open before the server's pending review has arrived. The same
                // composer must switch to draft-only actions when resume finishes.
                this.open_composer(None, "a.rs".into(), CommentSide::Right, 1, 1, window, cx);
                assert_eq!(
                    this.composer_submit_label(this.composer.as_ref().unwrap()),
                    "Add single comment"
                );
                this.composer
                    .as_ref()
                    .unwrap()
                    .input
                    .update(cx, |state, cx| state.set_value("new draft", window, cx));
                this.resume_session(0, cx);
                this.submit_composer(window, cx);
                assert!(
                    !this.composer.as_ref().unwrap().in_flight,
                    "keyboard submit waits for reconciliation"
                );
            })
        });
        cx.run_until_parked();
        vcx.update(|window, cx| {
            app.update(cx, |this, cx| {
                let composer = this.composer.as_ref().unwrap();
                assert!(!composer.pending, "no manual draft toggle is necessary");
                assert!(this.composer_uses_pending_review(composer));
                assert_eq!(this.composer_submit_label(composer), "Add to review");
                composer
                    .input
                    .update(cx, |state, cx| state.set_value("new draft", window, cx));
                this.submit_composer(window, cx);
            })
        });
        cx.run_until_parked();
        vcx.update(|window, cx| {
            app.update(cx, |this, cx| {
                assert!(this.composer.is_none());
                assert_eq!(this.sessions[&0].state.count(), 1);
                this.open_composer(
                    Some(11),
                    "a.rs".into(),
                    CommentSide::Right,
                    1,
                    1,
                    window,
                    cx,
                );
                let composer = this.composer.as_ref().unwrap();
                assert_eq!(this.composer_submit_label(composer), "Add to review");
                composer
                    .input
                    .update(cx, |state, cx| state.set_value("draft reply", window, cx));
                this.submit_composer(window, cx);
            })
        });
        cx.run_until_parked();
        cx.update(|cx| {
            app.update(cx, |this, _| {
                assert!(
                    this.composer.is_none(),
                    "reply succeeded without an immediate-post error"
                );
                assert_eq!(this.sessions[&0].state.count(), 2);
                let review = this.sessions[&0].state.pending.as_ref().unwrap();
                assert_eq!(review.body, "web review");
                assert_eq!(review.comments[1].in_reply_to_id, Some(11));
                assert_eq!(
                    this.store
                        .review_session(&loop_loc())
                        .pending
                        .unwrap()
                        .comments[1]
                        .in_reply_to_id,
                    Some(11)
                );
                let threads = &this
                    .active_data()
                    .unwrap()
                    .comments
                    .as_ref()
                    .unwrap()
                    .threads["a.rs"][&(CommentSide::Right, 1)];
                let thread = threads.iter().find(|t| t.root.id == 11).unwrap();
                assert_eq!(thread.replies.len(), 1);
                assert_eq!(thread.replies[0].body, "draft reply");
                assert_eq!(thread.replies[0].time, CommentTime::Pending);
            })
        });
        assert!(
            fake.posts().is_empty(),
            "neither comments nor replies publish while a review is pending"
        );
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        // Reconciliation/restart retains the reply relationship too.
        cx.update(|cx| app.update(cx, |this, cx| this.resume_session(0, cx)));
        cx.run_until_parked();
        assert_eq!(
            fake.pending
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .comments
                .len(),
            2
        );
    }

    /// ⌘↵'s dialog: approve / request changes / comment with an optional
    /// body. A bodyless non-approve verdict with no pending comments is
    /// rejected inline after reconciliation, without posting a review;
    /// a valid verdict posts exactly once and closes
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
            });
        });
        cx.run_until_parked();
        assert!(fake.posts().is_empty());
        vcx.update(|window, cx| {
            app.update(cx, |this, cx| {
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

    #[gpui::test]
    fn review_dialog_retains_failed_draft_guards_pending_dismissal_and_restores_focus(
        cx: &mut gpui::TestAppContext,
    ) {
        let fake = FakePr::with_threads(vec![]);
        let (window, app) = queue_window(cx, fake.clone());
        install_item(&app, loop_pr_item(vec![]), cx);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        let prior = vcx.update(|window, cx| {
            let prior = app.read(cx).open_input.focus_handle(cx);
            window.focus(&prior, cx);
            app.update(cx, |app, cx| app.open_review(window, cx));
            prior
        });
        let draw = |vcx: &mut gpui::VisualTestContext| {
            vcx.update(|window, cx| window.draw(cx).clear(cx));
            vcx.run_until_parked();
        };
        draw(&mut vcx);
        vcx.update(|window, cx| {
            assert!(app
                .read(cx)
                .review
                .as_ref()
                .unwrap()
                .input
                .focus_handle(cx)
                .is_focused(window))
        });
        vcx.simulate_input("retain this draft");
        fake.fail.store(true, std::sync::atomic::Ordering::SeqCst);
        vcx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.review.as_mut().unwrap().verdict = github::ReviewVerdict::RequestChanges;
                app.submit_review(window, cx);
                assert!(app.review.as_ref().unwrap().in_flight);
            });
            // Render the pending dialog without running the background job.
            window.draw(cx).clear(cx);
        });
        vcx.simulate_keystrokes("escape");
        vcx.run_until_parked();
        draw(&mut vcx);
        vcx.update(|_, cx| {
            let review = app
                .read(cx)
                .review
                .as_ref()
                .expect("pending/failure must not dismiss");
            assert!(!review.in_flight);
            assert!(review.error.as_ref().unwrap().contains("review rejected"));
            assert_eq!(review.input.read(cx).value().as_str(), "retain this draft");
            assert_eq!(review.verdict, github::ReviewVerdict::RequestChanges);
        });
        fake.fail.store(false, std::sync::atomic::Ordering::SeqCst);
        vcx.simulate_keystrokes("cmd-enter");
        vcx.run_until_parked();
        draw(&mut vcx);
        vcx.update(|window, cx| {
            assert!(app.read(cx).review.is_none());
            assert!(
                prior.is_focused(window),
                "Kit restores the actual prior focus, not always the diff"
            );
        });
    }

    #[gpui::test]
    fn stale_review_completion_does_not_close_a_reopened_dialog(cx: &mut gpui::TestAppContext) {
        let fake = FakePr::with_threads(vec![]);
        let (window, app) = queue_window(cx, fake.clone());
        install_item(&app, loop_pr_item(vec![]), cx);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        let (generation, input) = vcx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.open_review(window, cx);
                let old_generation = app.review_gen;
                app.submit_review(window, cx);
                // An imperative lifecycle close is distinct from user dismissal.
                app.close_review(window, cx);
                app.open_review(window, cx);
                assert!(!app.cleanup_review(old_generation, cx));
                let review = app.review.as_ref().unwrap();
                review
                    .input
                    .update(cx, |input, cx| input.set_value("new draft", window, cx));
                (app.review_gen, review.input.clone())
            })
        });
        vcx.run_until_parked();
        vcx.update(|window, cx| {
            window.draw(cx).clear(cx);
            let app = app.read(cx);
            assert_eq!(app.review_gen, generation);
            let review = app
                .review
                .as_ref()
                .expect("old success cannot close new generation");
            assert_eq!(review.input, input);
            assert_eq!(input.read(cx).value().as_str(), "new draft");
            assert!(!review.in_flight);
            assert!(review.error.is_none());
            assert!(!app.sessions[&0].busy, "per-PR operation still completes");
        });
        assert_eq!(fake.posts().len(), 1);
        vcx.simulate_keystrokes("escape");
        vcx.update(|_, cx| {
            assert!(
                app.read(cx).review.is_none(),
                "user dismissal cleans application state"
            )
        });
    }

    #[gpui::test]
    fn cmd_enter_submits_instead_of_accepting_a_mention(cx: &mut gpui::TestAppContext) {
        let fake = FakePr::with_threads(vec![]);
        let (window, app) = queue_window(cx, fake.clone());
        install_item(&app, loop_pr_item(vec![]), cx);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        vcx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.active_data()
                    .unwrap()
                    .mentions
                    .borrow_mut()
                    .push(github::Mention {
                        login: "alice".into(),
                        name: None,
                    });
                app.open_review(window, cx);
            })
        });
        vcx.simulate_input("Thanks @al");
        for _ in 0..3 {
            vcx.update(|window, cx| window.draw(cx).clear(cx));
            vcx.run_until_parked();
        }
        vcx.simulate_keystrokes("cmd-enter");
        vcx.run_until_parked();
        assert_eq!(
            fake.posts(),
            vec![Post::Review {
                verdict: github::ReviewVerdict::Approve,
                body: "Thanks @al".into(),
            }]
        );
    }

    #[gpui::test]
    fn cmd_enter_submits_review_without_a_newline(cx: &mut gpui::TestAppContext) {
        let fake = FakePr::with_threads(vec![]);
        let (window, app) = queue_window(cx, fake.clone());
        install_item(&app, loop_pr_item(vec![]), cx);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        let input = vcx.update(|window, cx| {
            app.update(cx, |this, cx| {
                this.open_review(window, cx);
                this.review.as_ref().unwrap().input.clone()
            })
        });
        vcx.simulate_input("beforeafter");
        vcx.simulate_keystrokes("left left left left left enter");
        cx.update(|cx| assert_eq!(input.read(cx).value().as_str(), "before\nafter"));
        assert!(fake.posts().is_empty());
        vcx.simulate_keystrokes("cmd-enter");
        cx.run_until_parked();
        cx.update(|cx| {
            assert_eq!(input.read(cx).value().as_str(), "before\nafter");
            assert!(app.read(cx).review.is_none());
        });
        assert_eq!(
            fake.posts(),
            vec![Post::Review {
                verdict: github::ReviewVerdict::Approve,
                body: "before\nafter".into(),
            }]
        );
    }

    fn range_pr_item() -> ReviewItem {
        let mut item = loop_pr_item(Vec::new());
        let ItemState::Ready(data) = &mut item.state else {
            unreachable!()
        };
        data.diff = diff_core::parse_patch("diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -0,0 +1,3 @@\n+one\n+two\n+three\n@@ -9,0 +10 @@\n+ten\n");
        data.anchor_hunks = data
            .diff
            .files
            .iter()
            .map(|file| file.hunks.clone())
            .collect();
        data.set_rows(data.row_build());
        data.cursor = 1;
        item
    }

    #[gpui::test]
    fn removed_line_drafts_render_in_both_modes_and_track_resume_edit_delete(
        cx: &mut gpui::TestAppContext,
    ) {
        let fake = FakePr::with_threads(Vec::new());
        let (window, app) = queue_window(cx, fake.clone());
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        let mut item = loop_pr_item(Vec::new());
        let ItemState::Ready(data) = &mut item.state else {
            unreachable!()
        };
        data.diff = diff_core::parse_patch("diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -5,2 +5,2 @@\n-old\n-removed\n+new\n+added\n");
        data.anchor_hunks = data
            .diff
            .files
            .iter()
            .map(|file| file.hunks.clone())
            .collect();
        data.set_rows(data.row_build());
        install_item(&app, item, cx);
        vcx.update(|window, cx| {
            app.update(cx, |this, cx| {
                this.open_composer(None, "a.rs".into(), CommentSide::Left, 6, 2, window, cx);
                let composer = this.composer.as_mut().unwrap();
                composer.pending = true;
                composer.start_line = Some(5);
                composer
                    .input
                    .update(cx, |state, cx| state.set_value("removed draft", window, cx));
                this.submit_composer(window, cx);
                this.close_composer(window, cx);
            })
        });
        cx.run_until_parked();
        cx.update(|cx| app.update(cx, |this, cx| {
            let data = this.active_data_mut().unwrap();
            assert_eq!(data.comments.as_ref().unwrap().counts["a.rs"], (1, 0));
            for mode in [ViewMode::Unified, ViewMode::Split] {
                data.mode = mode;
                data.rebuild_rows_anchored();
                assert!(data.rows.iter().any(|row| matches!(row,
                    Row::CommentBody { line, half, .. } if line.as_ref() == "removed draft"
                        && *half == if mode == ViewMode::Split { Some(CommentSide::Left) } else { None }
                )));
                assert!(data.rows.iter().any(|row| matches!(row,
                    Row::CommentHeader { when, .. } if when.as_ref() == "draft")));
                assert!(!data.rows.iter().any(|row| matches!(row, Row::CommentActions { .. })));
            }
            // A comment-only refresh must not remove or duplicate the draft.
            this.refetch_comments(0, loop_loc(), cx);
            this.resume_session(0, cx);
        }));
        cx.run_until_parked();
        vcx.update(|window, cx| {
            app.update(cx, |this, cx| {
                let data = this.active_data().unwrap();
                assert_eq!(data.comments.as_ref().unwrap().counts["a.rs"], (1, 0));
                this.edit_pending(12, window, cx);
                this.composer
                    .as_ref()
                    .unwrap()
                    .input
                    .update(cx, |state, cx| {
                        state.set_value("edited removed draft", window, cx)
                    });
                this.submit_composer(window, cx);
            })
        });
        cx.run_until_parked();
        vcx.update(|window, cx| {
            app.update(cx, |this, cx| {
                assert!(this
                    .active_data()
                    .unwrap()
                    .rows
                    .iter()
                    .any(|row| matches!(row,
                Row::CommentBody { line, .. } if line.as_ref() == "edited removed draft")));
                this.run_session(
                    0,
                    SessionCommand::Delete { id: 12 },
                    SessionCompletion::None,
                    window,
                    cx,
                );
            })
        });
        cx.run_until_parked();
        cx.update(|cx| {
            app.update(cx, |this, _| {
                let data = this.active_data().unwrap();
                assert!(!data.rows.iter().any(rows::is_comment_row));
                assert!(!data.comments.as_ref().unwrap().counts.contains_key("a.rs"));
                assert_eq!(this.sessions[&0].state.count(), 0);
            })
        });
        assert!(fake.posts().is_empty());
    }

    fn permitted_comment(
        node_id: &str,
        can_edit: bool,
        can_delete: bool,
    ) -> github::CommentPermissions {
        github::CommentPermissions {
            node_id: node_id.into(),
            can_edit,
            can_delete,
            can_react: true,
        }
    }

    #[gpui::test]
    fn local_comment_cards_edit_replies_and_preserve_them_when_deleting_the_root(
        cx: &mut gpui::TestAppContext,
    ) {
        let fake = FakePr::with_threads(vec![]);
        let (window, app) = queue_window(cx, fake.clone());
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        let mut item = loop_pr_item(vec![]);
        item.source = Source::Local(git::LocalSource {
            repo_root: PathBuf::from("/repo"),
            branch: "topic".into(),
            base_ref: None,
            base_label: "main".into(),
            base_oid: None,
        });
        if let ItemState::Ready(data) = &mut item.state {
            let mut local = LocalReview::default();
            local.add_comment(
                None,
                "a.rs".into(),
                CommentSide::Right,
                1,
                "local root".into(),
            );
            local.add_comment(
                Some(1),
                "a.rs".into(),
                CommentSide::Right,
                1,
                "local reply".into(),
            );
            data.comments = Some(local.index());
            data.local_review = Some(local);
            data.rebuild_rows_anchored();
        }
        install_item(&app, item, cx);
        click_commenting(&mut vcx, "comment-edit-local-2");
        vcx.update(|window, cx| {
            app.update(cx, |this, cx| {
                this.composer
                    .as_ref()
                    .unwrap()
                    .input
                    .update(cx, |input, cx| {
                        input.set_value("edited local reply", window, cx)
                    });
                this.submit_composer(window, cx);
                assert_eq!(
                    this.active_data()
                        .unwrap()
                        .local_review
                        .as_ref()
                        .unwrap()
                        .comments
                        .len(),
                    2
                );
            })
        });
        click_commenting(&mut vcx, "comment-delete-local-1");
        cx.simulate_prompt_answer("Delete");
        cx.run_until_parked();
        cx.update(|cx| {
            let data = app.read(cx).active_data().unwrap();
            let local = data.local_review.as_ref().unwrap();
            assert_eq!(local.comments.len(), 1);
            assert_eq!(local.comments[0].body, "edited local reply");
            assert_eq!(local.comments[0].in_reply_to_id, None);
            assert_eq!(data.comments.as_ref().unwrap().counts["a.rs"], (1, 0));
        });
        assert!(fake.posts().is_empty());
    }

    #[gpui::test]
    fn pending_tray_starts_collapsed_and_keeps_controls_and_per_pr_disclosure(
        cx: &mut gpui::TestAppContext,
    ) {
        let fake = FakePr::with_threads(vec![]);
        *fake.pending.lock().unwrap() = Some(github::PendingReview {
            id: 91,
            node_id: "pending".into(),
            body: String::new(),
            comments: vec![github::PendingComment {
                id: 13,
                node_id: "draft".into(),
                path: "gone.rs".into(),
                side: CommentSide::Left,
                line: None,
                start_line: None,
                body: "outdated draft".into(),
                in_reply_to_id: None,
            }],
        });
        let (window, app) = queue_window(cx, fake);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        install_item(&app, loop_pr_item(vec![]), cx);
        cx.update(|cx| app.update(cx, |this, cx| this.resume_session(0, cx)));
        cx.run_until_parked();
        let draw = |vcx: &mut gpui::VisualTestContext| {
            vcx.update(|window, cx| {
                let _ = window.draw(cx);
            });
        };
        draw(&mut vcx);
        assert!(vcx.debug_bounds("pending-status").is_some());
        assert!(vcx.debug_bounds("pending-refresh").is_some());
        assert!(vcx.debug_bounds("pending-review").is_some());
        assert!(vcx.debug_bounds("pending-comments").is_none());
        cx.update(|cx| assert_eq!(app.read(cx).sessions[&0].state.count(), 1));

        click_commenting(&mut vcx, "pending-comments-toggle");
        draw(&mut vcx);
        assert!(vcx.debug_bounds("pending-comments").is_some());
        click_commenting(&mut vcx, "pending-refresh");
        draw(&mut vcx);
        assert!(
            vcx.debug_bounds("pending-comments").is_some(),
            "refresh preserves expansion"
        );

        // Another PR starts collapsed, without changing the first PR's choice.
        cx.update(|cx| {
            app.update(cx, |this, cx| {
                let mut other = loop_pr_item(vec![]);
                other.id = 1;
                this.items.push(other);
                let state = this.sessions[&0].state.clone();
                this.sessions.insert(
                    1,
                    SessionUi {
                        state,
                        ..Default::default()
                    },
                );
                this.active = 1;
                cx.notify();
            })
        });
        draw(&mut vcx);
        assert!(vcx.debug_bounds("pending-comments").is_none());
        cx.update(|cx| {
            app.update(cx, |this, cx| {
                this.active = 0;
                cx.notify();
            })
        });
        draw(&mut vcx);
        assert!(vcx.debug_bounds("pending-comments").is_some());

        // The same disclosure works in the conversation pane.
        vcx.dispatch_action(ShowConversation);
        click_commenting(&mut vcx, "pending-comments-toggle");
        draw(&mut vcx);
        assert!(vcx.debug_bounds("pending-comments").is_none());
        click_commenting(&mut vcx, "pending-refresh");
        draw(&mut vcx);
        assert!(
            vcx.debug_bounds("pending-comments").is_none(),
            "refresh preserves collapse"
        );
        click_commenting(&mut vcx, "pending-review");
        cx.update(|cx| {
            assert!(
                app.read(cx).review.is_some(),
                "Finish review still opens the modal"
            )
        });
        vcx.update(|window, cx| app.update(cx, |this, cx| this.close_review(window, cx)));

        // Finishing/losing a review resets the disclosure for the next review.
        cx.update(|cx| {
            app.update(cx, |this, cx| {
                this.sessions.get_mut(&0).unwrap().comments_expanded = true;
                this.finish_session(0, ReviewSession::default(), &Ok(()), cx);
                assert!(!this.sessions[&0].comments_expanded);
            })
        });
    }

    #[gpui::test]
    fn pending_reply_edits_stay_in_the_conversation_and_outdated_drafts_edit_in_the_tray(
        cx: &mut gpui::TestAppContext,
    ) {
        let fake = FakePr::with_threads(vec![rc(
            11,
            "a.rs",
            Some("RIGHT"),
            Some(1),
            "root",
            "alice",
            "2026-01-01T00:00:00Z",
            None,
        )]);
        *fake.pending.lock().unwrap() = Some(github::PendingReview {
            id: 91,
            node_id: "pending".into(),
            body: String::new(),
            comments: vec![
                github::PendingComment {
                    id: 12,
                    node_id: "reply".into(),
                    path: "a.rs".into(),
                    side: CommentSide::Right,
                    line: Some(1),
                    start_line: None,
                    body: "draft reply".into(),
                    in_reply_to_id: Some(11),
                },
                github::PendingComment {
                    id: 13,
                    node_id: "outdated".into(),
                    path: "gone.rs".into(),
                    side: CommentSide::Left,
                    line: None,
                    start_line: None,
                    body: "outdated draft".into(),
                    in_reply_to_id: None,
                },
            ],
        });
        let (window, app) = queue_window(cx, fake.clone());
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        install_item(&app, commenting_item(&fake), cx);
        cx.update(|cx| app.update(cx, |this, cx| this.resume_session(0, cx)));
        cx.run_until_parked();
        click_commenting(&mut vcx, "comment-edit-draft-12");
        vcx.update(|window, cx| {
            app.update(cx, |this, cx| {
                assert_eq!(
                    this.composer.as_ref().unwrap().placement,
                    ComposerPlacement::ConversationComment("draft-12".into())
                );
                this.composer
                    .as_ref()
                    .unwrap()
                    .input
                    .update(cx, |input, cx| input.set_value("edited reply", window, cx));
                this.submit_composer(window, cx);
            })
        });
        cx.run_until_parked();
        vcx.update(|window, cx| {
            app.update(cx, |this, cx| {
                this.items[0].pane = ItemPane::Changes;
                assert!(!this.sessions[&0].comments_expanded);
                this.edit_pending(13, window, cx);
                assert_eq!(
                    this.composer.as_ref().unwrap().placement,
                    ComposerPlacement::PendingTray(13)
                );
            });
            let _ = window.draw(cx);
        });
        assert!(vcx.debug_bounds("pending-comments").is_some());
        assert!(
            vcx.debug_bounds("composer-card").is_some(),
            "fallback editor is not hidden in the collapsed tray"
        );
        click_commenting(&mut vcx, "pending-comments-toggle");
        assert!(
            vcx.debug_bounds("composer-card").is_some(),
            "cannot collapse an active tray editor"
        );
        vcx.update(|window, cx| {
            app.update(cx, |this, cx| {
                this.composer
                    .as_ref()
                    .unwrap()
                    .input
                    .update(cx, |input, cx| {
                        input.set_value("edited outdated draft", window, cx)
                    });
                this.submit_composer(window, cx);
            })
        });
        cx.run_until_parked();
        let pending = fake.pending.lock().unwrap();
        let comments = &pending.as_ref().unwrap().comments;
        assert_eq!(comments[0].body, "edited reply");
        assert_eq!(comments[0].in_reply_to_id, Some(11));
        assert_eq!(comments[1].body, "edited outdated draft");
        assert!(fake.posts().is_empty());
    }

    #[gpui::test]
    fn draft_card_edit_replaces_its_body_in_place_and_delete_requires_confirmation(
        cx: &mut gpui::TestAppContext,
    ) {
        let fake = FakePr::with_threads(vec![]);
        *fake.pending.lock().unwrap() = Some(github::PendingReview {
            id: 91,
            node_id: "pending".into(),
            body: String::new(),
            comments: vec![github::PendingComment {
                id: 12,
                node_id: "draft_node".into(),
                path: "a.rs".into(),
                side: CommentSide::Right,
                line: Some(1),
                start_line: None,
                body: "original draft".into(),
                in_reply_to_id: None,
            }],
        });
        let (window, app) = queue_window(cx, fake.clone());
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        install_item(&app, loop_pr_item(vec![]), cx);
        cx.update(|cx| app.update(cx, |this, cx| this.resume_session(0, cx)));
        cx.run_until_parked();
        click_commenting(&mut vcx, "comment-edit-draft-12");
        vcx.update(|window, cx| app.update(cx, |this, cx| {
            let composer = this.composer.as_ref().unwrap();
            assert_eq!(composer.edit_target, Some(CommentTarget::Pending(12)));
            assert!(composer.row_ix > 0, "not the old top-of-diff editor");
            let data = this.active_data().unwrap();
            assert!(matches!(&data.rows[composer.row_ix], Row::CommentHeader { comment_id: 12, .. }));
            assert!(matches!(&data.rows[composer.row_ix + 1], Row::CommentEditor { .. }));
            assert!(!data.rows.iter().any(|r| matches!(r, Row::CommentBody { line, .. } if line.as_ref() == "original draft")));
            this.close_composer(window, cx);
            assert!(this.active_data().unwrap().rows.iter().any(|r| matches!(r, Row::CommentBody { line, .. } if line.as_ref() == "original draft")));
        }));
        click_commenting(&mut vcx, "comment-edit-draft-12");
        vcx.update(|window, cx| {
            app.update(cx, |this, cx| {
                this.composer
                    .as_ref()
                    .unwrap()
                    .input
                    .update(cx, |input, cx| input.set_value("edited draft", window, cx));
            })
        });
        vcx.simulate_keystrokes("cmd-enter");
        vcx.run_until_parked();
        cx.update(|cx| {
            assert!(app.read(cx).composer.is_none());
            assert_eq!(
                app.read(cx).sessions[&0]
                    .state
                    .pending
                    .as_ref()
                    .unwrap()
                    .comments[0]
                    .body,
                "edited draft"
            );
            assert!(app
                .read(cx)
                .active_data()
                .unwrap()
                .editing_comment
                .is_none());
        });
        click_commenting(&mut vcx, "comment-delete-draft-12");
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert_eq!(
            fake.pending
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .comments
                .len(),
            1
        );
        click_commenting(&mut vcx, "comment-delete-draft-12");
        cx.simulate_prompt_answer("Delete");
        cx.run_until_parked();
        assert!(fake
            .pending
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .comments
            .is_empty());
        assert!(
            fake.posts().is_empty(),
            "draft changes never publish comments"
        );
    }

    #[gpui::test]
    fn reactions_enforce_capabilities_busy_state_and_active_item(cx: &mut gpui::TestAppContext) {
        use github::GitHubClient;
        let fake = FakePr::with_threads(vec![rc(
            11,
            "a.rs",
            Some("RIGHT"),
            Some(1),
            "Read-only",
            "alice",
            "2026-01-01T00:00:00Z",
            None,
        )]);
        let mut permissions = permitted_comment("PRRC11", false, false);
        permissions.can_react = false;
        fake.comment_permissions
            .lock()
            .unwrap()
            .insert(11, permissions);
        fake.reactions.lock().unwrap().insert(
            "PRRC11".into(),
            vec![github::Reaction {
                content: "HEART".into(),
                count: 2,
                viewer_has_reacted: false,
            }],
        );
        let (window, app) = queue_window(cx, fake.clone());
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        let mut item = loop_pr_item(fake.comments.lock().unwrap().clone());
        if let ItemState::Ready(data) = &mut item.state {
            data.discussion = Some(fake.pr_discussion(&loop_loc()).unwrap());
        }
        install_item(&app, item, cx);
        vcx.update(|_, cx| {
            app.update(cx, |this, _| {
                this.active_data_mut().unwrap().rebuild_rows_anchored()
            })
        });
        click_commenting(&mut vcx, "reaction-review-comment-PRRC11-HEART");
        assert!(vcx
            .debug_bounds("reaction-picker-review-comment-PRRC11")
            .is_none());
        vcx.update(|window, cx| {
            app.update(cx, |this, cx| {
                let target = github::CommentTarget::ReviewComment("PRRC11".into());
                this.set_reaction(0, target.clone(), "HEART".into(), true, window, cx);
                this.active_data_mut()
                    .unwrap()
                    .discussion
                    .as_mut()
                    .unwrap()
                    .threads[0]
                    .comments[0]
                    .permissions
                    .can_react = true;
                this.sessions.entry(0).or_default().busy = true;
                this.set_reaction(0, target.clone(), "HEART".into(), true, window, cx);
                this.sessions.get_mut(&0).unwrap().busy = false;
                this.set_reaction(99, target, "HEART".into(), true, window, cx);
            })
        });
        vcx.run_until_parked();
        assert!(fake.posts().is_empty());
    }

    #[gpui::test]
    fn reaction_picker_and_toggles_share_state_between_diff_and_conversation(
        cx: &mut gpui::TestAppContext,
    ) {
        use github::GitHubClient;
        let fake = FakePr::with_threads(vec![rc(
            11,
            "a.rs",
            Some("RIGHT"),
            Some(1),
            "React to this",
            "alice",
            "2026-01-01T00:00:00Z",
            None,
        )]);
        fake.comment_permissions
            .lock()
            .unwrap()
            .insert(11, permitted_comment("PRRC11", false, false));
        let (window, app) = queue_window(cx, fake.clone());
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        let mut item = loop_pr_item(fake.comments.lock().unwrap().clone());
        if let ItemState::Ready(data) = &mut item.state {
            data.discussion = Some(fake.pr_discussion(&loop_loc()).unwrap());
        }
        install_item(&app, item, cx);
        vcx.update(|_, cx| app.update(cx, |this, _| {
            this.active_data_mut().unwrap().rebuild_rows_anchored();
            let rows = &this.active_data().unwrap().rows;
            let footer = rows.iter().position(|r| matches!(r, Row::CommentReactions { comment_id: 11, .. })).unwrap();
            assert!(matches!(&rows[footer - 1], Row::CommentBody { text, .. } if text.comment_id == 11));
            assert!(matches!(&rows[footer + 1], Row::CommentActions { root_id: 11, .. }));
        }));
        click_commenting(&mut vcx, "reaction-picker-review-comment-PRRC11");
        assert!(vcx.debug_bounds("reaction-options").is_some());
        for selector in [
            "reaction-option-THUMBS_UP",
            "reaction-option-THUMBS_DOWN",
            "reaction-option-LAUGH",
            "reaction-option-HOORAY",
            "reaction-option-CONFUSED",
            "reaction-option-HEART",
            "reaction-option-ROCKET",
            "reaction-option-EYES",
        ] {
            assert!(vcx.debug_bounds(selector).is_some());
        }
        click_commenting(&mut vcx, "reaction-option-HEART");
        vcx.run_until_parked();
        assert!(vcx.debug_bounds("reaction-options").is_none());
        assert_eq!(
            fake.posts(),
            vec![Post::Reaction {
                target: github::CommentTarget::ReviewComment("PRRC11".into()),
                content: "HEART".into(),
                reacted: true
            }]
        );
        cx.update(|cx| {
            let comment = &app
                .read(cx)
                .active_data()
                .unwrap()
                .discussion
                .as_ref()
                .unwrap()
                .threads[0]
                .comments[0];
            assert_eq!(comment.reactions[0].count, 1);
            assert!(comment.reactions[0].viewer_has_reacted);
        });
        vcx.update(|window, cx| {
            app.update(cx, |this, cx| {
                this.items[0].pane = ItemPane::Conversation;
                cx.notify();
            });
            let _ = window.draw(cx);
        });
        // A rejected write leaves the last confirmed count and selection intact.
        fake.fail.store(true, std::sync::atomic::Ordering::SeqCst);
        click_commenting(&mut vcx, "reaction-review-comment-PRRC11-HEART");
        vcx.run_until_parked();
        cx.update(|cx| {
            let this = app.read(cx);
            assert!(this.sessions[&0]
                .error
                .as_ref()
                .unwrap()
                .contains("reaction rejected"));
            let comment = &this
                .active_data()
                .unwrap()
                .discussion
                .as_ref()
                .unwrap()
                .threads[0]
                .comments[0];
            assert!(comment.reactions[0].viewer_has_reacted);
            assert_eq!(comment.reactions[0].count, 1);
        });
        fake.fail.store(false, std::sync::atomic::Ordering::SeqCst);
        click_commenting(&mut vcx, "reaction-review-comment-PRRC11-HEART");
        vcx.run_until_parked();
        cx.update(|cx| {
            let this = app.read(cx);
            assert!(this.sessions[&0].error.is_none());
            assert!(this
                .active_data()
                .unwrap()
                .discussion
                .as_ref()
                .unwrap()
                .threads[0]
                .comments[0]
                .reactions
                .is_empty());
        });
        assert_eq!(
            fake.posts().last(),
            Some(&Post::Reaction {
                target: github::CommentTarget::ReviewComment("PRRC11".into()),
                content: "HEART".into(),
                reacted: false
            })
        );
    }

    #[gpui::test]
    fn published_card_permissions_allow_foreign_comment_deletion_but_not_editing(
        cx: &mut gpui::TestAppContext,
    ) {
        use github::GitHubClient;
        let fake = FakePr::with_threads(vec![rc(
            11,
            "a.rs",
            Some("RIGHT"),
            Some(1),
            "someone else's comment",
            "alice",
            "2026-01-01T00:00:00Z",
            None,
        )]);
        fake.comment_permissions
            .lock()
            .unwrap()
            .insert(11, permitted_comment("PRRC11", false, true));
        let (window, app) = queue_window(cx, fake.clone());
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        let mut item = loop_pr_item(fake.comments.lock().unwrap().clone());
        if let ItemState::Ready(data) = &mut item.state {
            data.discussion = Some(fake.pr_discussion(&loop_loc()).unwrap());
        }
        install_item(&app, item, cx);
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        assert!(vcx
            .debug_bounds("comment-edit-review-comment-PRRC11")
            .is_none());
        assert!(vcx
            .debug_bounds("comment-delete-review-comment-PRRC11")
            .is_some());
        vcx.update(|window, cx| {
            app.update(cx, |this, cx| {
                this.edit_comment(
                    CommentTarget::Published(github::CommentTarget::ReviewComment("PRRC11".into())),
                    ComposerPlacement::Diff,
                    window,
                    cx,
                );
                assert!(
                    this.composer.is_none(),
                    "handlers also enforce edit permission"
                );
            })
        });
        click_commenting(&mut vcx, "comment-delete-review-comment-PRRC11");
        cx.simulate_prompt_answer("Delete");
        cx.run_until_parked();
        assert_eq!(
            fake.posts(),
            vec![Post::Delete {
                target: github::CommentTarget::ReviewComment("PRRC11".into())
            }]
        );
        cx.update(|cx| {
            assert!(app
                .read(cx)
                .active_data()
                .unwrap()
                .discussion
                .as_ref()
                .unwrap()
                .threads
                .is_empty());
            assert!(!app
                .read(cx)
                .active_data()
                .unwrap()
                .rows
                .iter()
                .any(rows::is_comment_row));
        });
    }

    #[gpui::test]
    fn published_inline_edit_saves_in_both_views_without_touching_the_pending_review(
        cx: &mut gpui::TestAppContext,
    ) {
        use github::GitHubClient;
        let fake = FakePr::with_threads(vec![rc(
            11,
            "a.rs",
            Some("RIGHT"),
            Some(1),
            "published original",
            "alice",
            "2026-01-01T00:00:00Z",
            None,
        )]);
        fake.comment_permissions
            .lock()
            .unwrap()
            .insert(11, permitted_comment("PRRC11", true, false));
        *fake.pending.lock().unwrap() = Some(github::PendingReview {
            id: 91,
            node_id: "pending".into(),
            body: "keep my draft".into(),
            comments: vec![],
        });
        let (window, app) = queue_window(cx, fake.clone());
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        let mut item = loop_pr_item(fake.comments.lock().unwrap().clone());
        if let ItemState::Ready(data) = &mut item.state {
            data.discussion = Some(fake.pr_discussion(&loop_loc()).unwrap());
        }
        install_item(&app, item, cx);
        cx.update(|cx| app.update(cx, |this, cx| this.resume_session(0, cx)));
        cx.run_until_parked();
        for (pane, mode, body) in [
            (ItemPane::Changes, ViewMode::Unified, "edit unified"),
            (ItemPane::Changes, ViewMode::Split, "edit split"),
            (ItemPane::Conversation, ViewMode::Split, "edit conversation"),
        ] {
            cx.update(|cx| {
                app.update(cx, |this, cx| {
                    this.items[0].pane = pane;
                    let data = this.active_data_mut().unwrap();
                    data.mode = mode;
                    data.rebuild_rows_anchored();
                    cx.notify();
                })
            });
            click_commenting(&mut vcx, "comment-edit-review-comment-PRRC11");
            vcx.update(|window, cx| {
                app.update(cx, |this, cx| {
                    let composer = this.composer.as_ref().unwrap();
                    assert_eq!(this.composer_submit_label(composer), "Save changes");
                    assert!(!this.composer_uses_pending_review(composer));
                    if pane == ItemPane::Conversation {
                        assert_eq!(
                            composer.placement,
                            ComposerPlacement::ConversationComment("review-comment-PRRC11".into())
                        );
                    }
                    composer
                        .input
                        .update(cx, |input, cx| input.set_value(body, window, cx));
                })
            });
            vcx.simulate_keystrokes("cmd-enter");
            vcx.run_until_parked();
            cx.update(|cx| {
                assert!(app.read(cx).composer.is_none());
                assert_eq!(
                    app.read(cx)
                        .active_data()
                        .unwrap()
                        .discussion
                        .as_ref()
                        .unwrap()
                        .threads[0]
                        .comments[0]
                        .body,
                    body
                );
                assert_eq!(
                    app.read(cx).sessions[&0]
                        .state
                        .pending
                        .as_ref()
                        .unwrap()
                        .body,
                    "keep my draft"
                );
            });
        }
        assert_eq!(fake.posts().len(), 3);
        assert!(fake
            .posts()
            .iter()
            .all(|post| matches!(post, Post::Edit { .. })));
    }

    #[gpui::test]
    fn published_issue_and_review_edit_errors_keep_text_and_closed_editors_keep_success(
        cx: &mut gpui::TestAppContext,
    ) {
        use github::GitHubClient;
        let fake = FakePr::with_threads(vec![]);
        for (id, node_id, state) in [(11, "IC11", None), (12, "PRR12", Some("APPROVED"))] {
            fake.conversation
                .lock()
                .unwrap()
                .push(github::DiscussionComment {
                    id,
                    permissions: permitted_comment(node_id, true, true),
                    author: "alice".into(),
                    body: "original".into(),
                    created_at: "2026-01-01T00:00:00Z".into(),
                    reactions: vec![],
                    review_state: state.map(str::to_owned),
                    review_id: state.map(|_| node_id.into()),
                });
        }
        let (window, app) = queue_window(cx, fake.clone());
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        let mut item = commenting_item(&fake);
        if let ItemState::Ready(data) = &mut item.state {
            data.discussion = Some(fake.pr_discussion(&loop_loc()).unwrap());
        }
        install_item(&app, item, cx);
        click_commenting(&mut vcx, "comment-edit-issue-IC11");
        fake.fail.store(true, std::sync::atomic::Ordering::SeqCst);
        vcx.update(|window, cx| {
            app.update(cx, |this, cx| {
                this.composer
                    .as_ref()
                    .unwrap()
                    .input
                    .update(cx, |input, cx| input.set_value("retry me", window, cx));
                this.submit_composer(window, cx);
            })
        });
        cx.run_until_parked();
        cx.update(|cx| {
            let composer = app.read(cx).composer.as_ref().unwrap();
            assert!(!composer.in_flight);
            assert!(composer.error.as_ref().unwrap().contains("edit rejected"));
            assert_eq!(composer.input.read(cx).value().as_str(), "retry me");
        });
        fake.fail.store(false, std::sync::atomic::Ordering::SeqCst);
        fake.fail_discussion
            .store(true, std::sync::atomic::Ordering::SeqCst);
        vcx.update(|window, cx| {
            app.update(cx, |this, cx| {
                this.submit_composer(window, cx);
                this.close_composer(window, cx);
            })
        });
        cx.run_until_parked();
        cx.update(|cx| {
            assert_eq!(
                app.read(cx)
                    .active_data()
                    .unwrap()
                    .discussion
                    .as_ref()
                    .unwrap()
                    .comments[0]
                    .body,
                "retry me"
            )
        });
        click_commenting(&mut vcx, "comment-edit-review-PRR12");
        vcx.update(|window, cx| {
            app.update(cx, |this, cx| {
                this.composer
                    .as_ref()
                    .unwrap()
                    .input
                    .update(cx, |input, cx| {
                        input.set_value("updated review summary", window, cx)
                    });
                this.submit_composer(window, cx);
            })
        });
        cx.run_until_parked();
        assert!(
            matches!(fake.posts().last(), Some(Post::Edit { target: github::CommentTarget::Review(id), body }) if id == "PRR12" && body == "updated review summary")
        );
    }

    #[gpui::test]
    fn range_drafts_resume_edit_delete_and_submit_without_posting_single_comments(
        cx: &mut gpui::TestAppContext,
    ) {
        let fake = FakePr::with_threads(Vec::new());
        *fake.pending.lock().unwrap() = Some(github::PendingReview {
            id: 91,
            node_id: "pending".into(),
            body: "from web".into(),
            comments: vec![github::PendingComment {
                id: 13,
                in_reply_to_id: None,
                node_id: "old_draft_node".into(),
                path: "old.rs".into(),
                side: CommentSide::Left,
                line: None,
                start_line: None,
                body: "web draft".into(),
            }],
        });
        let (window, app) = queue_window(cx, fake.clone());
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        install_item(&app, range_pr_item(), cx);
        cx.update(|cx| app.update(cx, |this, cx| this.resume_session(0, cx)));
        cx.run_until_parked();
        vcx.update(|window, cx| {
            app.update(cx, |this, cx| {
                assert_eq!(
                    this.sessions[&0].state.count(),
                    1,
                    "web drafts resume in the tray"
                );
                this.active_data_mut().unwrap().selection = Some(Selection {
                    side: SelSide::Unified,
                    anchor: RowCol { row: 3, col: 2 },
                    head: RowCol { row: 1, col: 0 },
                });
                this.open_composer_on_selection(window, cx);
                let composer = this.composer.as_mut().unwrap();
                assert_eq!((composer.start_line, composer.line), (Some(1), 3));
                composer.pending = true;
                composer
                    .input
                    .update(cx, |state, cx| state.set_value("range draft", window, cx));
                this.submit_composer(window, cx);
                assert!(this.sessions[&0].busy);
                assert!(
                    this.session_job(0, SessionCommand::Resume, cx).is_none(),
                    "one operation per PR, not per dialog"
                );
            })
        });
        cx.run_until_parked();
        vcx.update(|window, cx| {
            app.update(cx, |this, cx| {
                assert!(this.composer.is_none());
                assert_eq!(this.sessions[&0].state.count(), 2);
                assert_eq!(this.store.review_session(&loop_loc()).count(), 2);
                this.edit_pending(12, window, cx);
                let composer = this.composer.as_ref().unwrap();
                assert_eq!(composer.edit_id, Some(12));
                composer
                    .input
                    .update(cx, |state, cx| state.set_value("edited range", window, cx));
                this.submit_composer(window, cx);
            })
        });
        cx.run_until_parked();
        vcx.update(|window, cx| {
            app.update(cx, |this, cx| {
                assert_eq!(
                    this.sessions[&0].state.pending.as_ref().unwrap().comments[1].body,
                    "edited range"
                );
                this.run_session(
                    0,
                    SessionCommand::Delete { id: 13 },
                    SessionCompletion::None,
                    window,
                    cx,
                );
            })
        });
        cx.run_until_parked();
        assert!(
            fake.posts().is_empty(),
            "draft adds, edits, and deletes never post immediately"
        );
        vcx.update(|window, cx| {
            app.update(cx, |this, cx| {
                assert_eq!(this.sessions[&0].state.count(), 1);
                this.open_review(window, cx);
                this.review.as_mut().unwrap().verdict = github::ReviewVerdict::Comment;
                this.review
                    .as_ref()
                    .unwrap()
                    .input
                    .update(cx, |state, cx| state.set_value("", window, cx));
                this.submit_review(window, cx);
                // Closing a modal never cancels or loses a server mutation.
                this.close_review(window, cx);
            })
        });
        cx.run_until_parked();
        assert_eq!(
            fake.posts(),
            vec![Post::Review {
                verdict: github::ReviewVerdict::Comment,
                body: String::new()
            }]
        );
        cx.update(|cx| {
            app.update(cx, |this, _| {
                assert!(this.sessions[&0].state.pending.is_none());
                assert!(this.store.review_session(&loop_loc()).pending.is_none());
            })
        });
    }

    #[gpui::test]
    fn cross_hunk_ranges_and_whole_file_lines_use_blob_positions_in_the_ui(
        cx: &mut gpui::TestAppContext,
    ) {
        let fake = FakePr::with_threads(Vec::new());
        let (window, app) = queue_window(cx, fake);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        install_item(&app, range_pr_item(), cx);
        cx.run_until_parked();
        vcx.update(|window, cx| {
            app.update(cx, |this, cx| {
                let data = this.active_data_mut().unwrap();
                let end_row = data
                    .rows
                    .iter()
                    .enumerate()
                    .find(|(ix, _)| {
                        comment_anchor(&data.rows, *ix, SelSide::Unified)
                            == Some((CommentSide::Right, 10))
                    })
                    .unwrap()
                    .0;
                data.selection = Some(Selection {
                    side: SelSide::Unified,
                    anchor: RowCol { row: 1, col: 0 },
                    head: RowCol {
                        row: end_row,
                        col: 1,
                    },
                });
                this.open_composer_on_selection(window, cx);
                let composer = this.composer.as_ref().unwrap();
                assert_eq!((composer.start_line, composer.line), (Some(1), 10));
                assert_eq!(composer.position.as_ref().unwrap().commit_id, "dead10cc");
                this.close_composer(window, cx);
                // Simulate a full-content display hunk that merges both server hunks.
                let data = this.active_data_mut().unwrap();
                data.diff.files[0].hunks[0].new_count = 10;
                for line in 4..=10 {
                    data.diff.files[0].hunks[0]
                        .rows
                        .push(diff_core::DiffRow::Added {
                            new_no: line,
                            text: "synthetic".into(),
                            intra: vec![],
                        });
                }
                data.set_rows(data.row_build());
                data.selection = None;
                data.cursor = 5;
                this.open_composer_on_selection(window, cx);
                let composer = this.composer.as_ref().unwrap();
                assert_eq!((composer.start_line, composer.line), (None, 5));
                assert!(
                    composer.position.is_some(),
                    "whole-file lines use the preview API"
                );
                this.close_composer(window, cx);
                // Hover's + button reaches the same validation, without a selection.
                this.open_composer(None, "a.rs".into(), CommentSide::Right, 5, 5, window, cx);
                assert!(this.composer.as_ref().unwrap().position.is_some());
            })
        });
    }

    #[gpui::test]
    fn plain_line_clicks_repaint_the_gutter_without_selecting_code(cx: &mut gpui::TestAppContext) {
        let fake = FakePr::with_threads(Vec::new());
        let (window, app) = queue_window(cx, fake);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        let mut item = loop_pr_item(Vec::new());
        let ItemState::Ready(data) = &mut item.state else {
            unreachable!()
        };
        data.diff = diff_core::parse_patch("diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n");
        data.set_rows(data.row_build());
        install_item(&app, item, cx);
        vcx.run_until_parked();
        let notifications = Rc::new(std::cell::Cell::new(0usize));
        let _subscription = vcx.cx.update(|cx| {
            let notifications = notifications.clone();
            cx.observe(&app, move |_, _| notifications.set(notifications.get() + 1))
        });
        for mode in [ViewMode::Unified, ViewMode::Split] {
            vcx.cx.update(|cx| {
                app.update(cx, |this, cx| {
                    let data = this.active_data_mut().unwrap();
                    data.mode = mode;
                    data.cursor = 1;
                    data.cursor_side = None;
                    data.set_rows(data.row_build());
                    cx.notify();
                })
            });
            vcx.run_until_parked();
            let sides = match mode {
                ViewMode::Unified => vec![SelSide::Unified],
                ViewMode::Split => vec![SelSide::Left, SelSide::Right],
            };
            for row in [1, 3] {
                for &side in &sides {
                    let at = vcx.cx.update(|cx| {
                        let data = app.read(cx).active_data().unwrap();
                        let scroll = data.scroll.0.borrow();
                        let bounds = scroll.base_handle.bounds();
                        let x = if side == SelSide::Right {
                            (bounds.size.width - px(SPLIT_DIVIDER)) / 2. + px(SPLIT_DIVIDER + 22.)
                        } else {
                            px(22.)
                        };
                        point(
                            bounds.left() + x,
                            bounds.top()
                                + scroll.base_handle.offset().y
                                + px((row as f32 + 0.5) * row_height()),
                        )
                    });
                    let before = notifications.get();
                    vcx.simulate_mouse_down(at, MouseButton::Left, gpui::Modifiers::default());
                    vcx.simulate_mouse_up(at, MouseButton::Left, gpui::Modifiers::default());
                    vcx.run_until_parked();
                    assert!(
                        notifications.get() > before,
                        "a plain click must repaint, even without a selection"
                    );
                    vcx.cx.update(|cx| {
                        let app = app.read(cx);
                        let data = app.active_data().unwrap();
                        assert_eq!((data.cursor, data.cursor_side), (row, Some(side)));
                        assert!(
                            data.selection.is_none(),
                            "the cue must not select or recolour code"
                        );
                        assert!(app.composer.is_none());
                    });
                }
            }
        }
    }

    #[gpui::test]
    fn double_click_selects_and_copies_words_through_the_diff_mouse_handlers(
        cx: &mut gpui::TestAppContext,
    ) {
        let (window, app) = queue_window(cx, FakePr::with_threads(Vec::new()));
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        let mut item = range_pr_item();
        let ItemState::Ready(data) = &mut item.state else {
            unreachable!()
        };
        data.diff = diff_core::parse_patch("diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old_word héllo end\n+new_word héllo end\n");
        install_item(&app, item, cx);
        for (mode, side, row, expected) in [
            (ViewMode::Unified, SelSide::Unified, 1, "old_word"),
            (ViewMode::Unified, SelSide::Unified, 2, "new_word"),
            (ViewMode::Split, SelSide::Left, 1, "old_word"),
            (ViewMode::Split, SelSide::Right, 1, "new_word"),
        ] {
            vcx.update(|_, cx| {
                app.update(cx, |this, cx| {
                    let data = this.active_data_mut().unwrap();
                    data.mode = mode;
                    data.set_rows(data.row_build());
                    cx.notify();
                })
            });
            vcx.run_until_parked();
            let at = |col: f32, vcx: &mut gpui::VisualTestContext| {
                vcx.update(|window, cx| {
                    app.update(cx, |this, _| {
                        let width = this.char_width(window);
                        let data = this.active_data().unwrap();
                        let scroll = data.scroll.0.borrow();
                        let bounds = scroll.base_handle.bounds();
                        let gutter = if mode == ViewMode::Unified {
                            UNIFIED_GUTTER
                        } else {
                            SPLIT_GUTTER
                        };
                        let half = if side == SelSide::Right {
                            (bounds.size.width - px(SPLIT_DIVIDER)) / 2. + px(SPLIT_DIVIDER)
                        } else {
                            px(0.)
                        };
                        point(
                            bounds.left()
                                + half
                                + px(gutter)
                                + scroll.base_handle.offset().x
                                + width * col,
                            bounds.top()
                                + scroll.base_handle.offset().y
                                + px((row as f32 + 0.5) * row_height()),
                        )
                    })
                })
            };
            // The right half of the last glyph must still select its word,
            // rather than rounding to the following space/caret.
            for (col, expected) in [(7.8, expected), (13.8, "héllo")] {
                let position = at(col, &mut vcx);
                vcx.simulate_mouse_down(position, MouseButton::Left, gpui::Modifiers::default());
                vcx.simulate_mouse_up(position, MouseButton::Left, gpui::Modifiers::default());
                vcx.simulate_event(MouseDownEvent {
                    position,
                    button: MouseButton::Left,
                    click_count: 2,
                    ..Default::default()
                });
                // Even a small held-button movement must preserve whole-word selection.
                vcx.simulate_mouse_move(
                    position,
                    Some(MouseButton::Left),
                    gpui::Modifiers::default(),
                );
                vcx.simulate_mouse_up(position, MouseButton::Left, gpui::Modifiers::default());
                vcx.update(|window, cx| {
                    let this = app.read(cx);
                    let data = this.active_data().unwrap();
                    let selection = data.selection.expect("double click selects a word");
                    assert_eq!(selection.side, side);
                    assert_eq!(selection_text(&selection, &data.rows), expected);
                    assert_eq!((data.cursor, data.cursor_side), (row, Some(side)));
                    assert!(this.drag_anchor.is_none());
                    assert!(this.composer.is_none());
                    assert!(this.focus_handle.is_focused(window));
                });
                vcx.dispatch_action(CopySelection);
                vcx.update(|_, cx| {
                    assert_eq!(cx.read_from_clipboard().unwrap().text().unwrap(), expected);
                });
            }
            let start = at(2., &mut vcx);
            let end = at(15., &mut vcx);
            vcx.simulate_event(MouseDownEvent {
                position: start,
                button: MouseButton::Left,
                click_count: 2,
                ..Default::default()
            });
            vcx.simulate_mouse_move(end, Some(MouseButton::Left), gpui::Modifiers::default());
            vcx.simulate_mouse_up(end, MouseButton::Left, gpui::Modifiers::default());
            vcx.update(|_, cx| {
                let data = app.read(cx).active_data().unwrap();
                let selection = data.selection.unwrap();
                assert_eq!(selection.side, side);
                assert_eq!(
                    selection_text(&selection, &data.rows),
                    format!("{expected} héllo end")
                );
            });
            // Gutters, the blank area after code, and the virtual tail do not
            // select the first/last word merely because caret hits are clamped.
            let gutter = at(-1., &mut vcx);
            let after_text = at(40., &mut vcx);
            let mut tail = at(2., &mut vcx);
            tail.y += px(5. * row_height());
            for position in [gutter, after_text, tail] {
                vcx.simulate_event(MouseDownEvent {
                    position,
                    button: MouseButton::Left,
                    click_count: 2,
                    ..Default::default()
                });
                vcx.simulate_mouse_up(position, MouseButton::Left, gpui::Modifiers::default());
                vcx.update(|_, cx| {
                    assert!(app.read(cx).active_data().unwrap().selection.is_none());
                });
            }
            if mode == ViewMode::Split {
                vcx.update(|window, cx| {
                    app.update(cx, |this, _| {
                        let width = this.char_width(window);
                        let scroll = this.active_data().unwrap().scroll.clone();
                        let previous = scroll.0.borrow().base_handle.offset();
                        scroll
                            .0
                            .borrow_mut()
                            .base_handle
                            .set_offset(point(-width * 4., previous.y));
                        assert!(
                            this.pane_word_selection(gutter, width).is_none(),
                            "a frozen gutter is not code even when scrolled"
                        );
                        scroll.0.borrow_mut().base_handle.set_offset(previous);
                    })
                });
            }
        }
    }

    #[gpui::test]
    fn range_composer_sits_below_the_last_selected_row(cx: &mut gpui::TestAppContext) {
        let fake = FakePr::with_threads(Vec::new());
        let (window, app) = queue_window(cx, fake);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        let mut item = range_pr_item();
        let ItemState::Ready(data) = &mut item.state else {
            unreachable!()
        };
        let context: String = (1..=100).map(|line| format!(" line {line}\n")).collect();
        data.diff = diff_core::parse_patch(&format!("diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,101 +1,101 @@\n{context}-old\n+new\n"));
        data.anchor_hunks = vec![data.diff.files[0].hunks.clone()];
        install_item(&app, item, cx);

        for (mode, side, comment_side) in [
            (ViewMode::Unified, SelSide::Unified, CommentSide::Right),
            (ViewMode::Split, SelSide::Left, CommentSide::Left),
            (ViewMode::Split, SelSide::Right, CommentSide::Right),
        ] {
            let eof = if mode == ViewMode::Unified { 102 } else { 101 };
            for (anchor, head) in [(1, 3), (3, 1), (2, 2), (98, 100), (100, 98), (eof, eof)] {
                vcx.update(|_, cx| {
                    app.update(cx, |this, cx| {
                        let data = this.active_data_mut().unwrap();
                        data.mode = mode;
                        data.set_rows(data.row_build());
                        let mut scroll = data.scroll.0.borrow_mut();
                        scroll.deferred_scroll_to_item = None;
                        scroll.base_handle.set_offset(point(px(0.), px(0.)));
                        drop(scroll);
                        data.selection = Some(Selection {
                            side,
                            anchor: RowCol {
                                row: anchor,
                                col: 0,
                            },
                            head: RowCol { row: head, col: 1 },
                        });
                        cx.notify();
                    })
                });
                vcx.run_until_parked();
                vcx.dispatch_action(NewComment);
                vcx.run_until_parked();
                let initial_height = vcx.debug_bounds("composer-card").unwrap().size.height;
                if anchor == eof {
                    // Growing the card must reserve more space and reveal it
                    // again, including when there are no source rows below it.
                    vcx.update(|window, cx| {
                        let input = app.read(cx).composer.as_ref().unwrap().input.clone();
                        input.update(cx, |state, cx| {
                            state.set_value("a long comment\n".repeat(8), window, cx)
                        });
                    });
                    vcx.run_until_parked();
                }
                let card = vcx.debug_bounds("composer-card").unwrap();
                if anchor.max(head) <= 3 {
                    let (highlight, code, opposite) = match side {
                        SelSide::Unified => ("comment-range-Unified-2", "diff-code-2", None),
                        SelSide::Left => (
                            "comment-range-Left-2",
                            "diff-code-left-2",
                            Some("diff-code-right-clip-2"),
                        ),
                        SelSide::Right => (
                            "comment-range-Right-2",
                            "diff-code-right-2",
                            Some("diff-code-left-clip-2"),
                        ),
                    };
                    let highlight = vcx.debug_bounds(highlight).unwrap();
                    let code = vcx.debug_bounds(code).unwrap();
                    assert!(highlight.contains(&code.center()));
                    if let Some(opposite) = opposite {
                        let opposite = vcx.debug_bounds(opposite).unwrap();
                        assert!(!highlight.intersects(&opposite));
                    }
                }
                if anchor == eof {
                    assert!(
                        card.size.height > initial_height,
                        "the input must actually grow"
                    );
                }
                vcx.update(|window, cx| {
                    app.update(cx, |this, cx| {
                        let composer = this.composer.as_ref().unwrap();
                        let last = anchor.max(head);
                        assert_eq!(composer.row_ix, last);
                        assert_eq!(composer.line, last.min(101) as u64);
                        assert_eq!(composer.side, comment_side);
                        assert_eq!(
                            composer.start_line,
                            (anchor != head).then_some(anchor.min(head) as u64)
                        );
                        let data = this.active_data().unwrap();
                        let item_id = this.active_item().unwrap().id;
                        let highlighted: Vec<_> = data
                            .rows
                            .iter()
                            .enumerate()
                            .filter_map(|(ix, row)| {
                                composer.highlighted_side(item_id, data, row).map(
                                    |highlight_side| {
                                        assert_eq!(highlight_side, side);
                                        ix
                                    },
                                )
                            })
                            .collect();
                        assert_eq!(highlighted, (anchor.min(head)..=last).collect::<Vec<_>>());
                        for row in &data.rows {
                            assert!(composer.highlighted_side(item_id + 1, data, row).is_none());
                        }
                        let scroll = this.active_data().unwrap().scroll.0.borrow();
                        let line_bottom = scroll.base_handle.bounds().top()
                            + scroll.base_handle.offset().y
                            + px((last + 1) as f32 * row_height());
                        assert_eq!(card.top(), line_bottom, "the card must not cover the range");
                        assert!(
                            card.bottom() <= scroll.base_handle.bounds().bottom(),
                            "scroll the diff to leave room for the entire card"
                        );
                        if last <= 3 {
                            assert_eq!(scroll.base_handle.offset().y, px(0.));
                        }
                        drop(scroll);
                        this.close_composer(window, cx);
                    })
                });
                vcx.run_until_parked();
            }
        }
    }

    #[gpui::test]
    fn comment_range_highlight_tracks_source_lines_not_display_rows(cx: &mut gpui::TestAppContext) {
        let (window, app) = queue_window(cx, FakePr::with_threads(Vec::new()));
        install_item(&app, range_pr_item(), cx);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        vcx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.open_composer(None, "a.rs".into(), CommentSide::Right, 3, 3, window, cx);
                let composer = app.composer.as_mut().unwrap();
                composer.start_line = Some(1);
                let item_id = composer.item_id;
                // A wrapped context fragment has both source numbers, even
                // though its display index need not match either of them.
                let fragment = Row::Continuation(Box::new(Row::Line {
                    old_no: Some(12),
                    new_no: Some(2),
                    kind: LineKind::Context,
                    text: "wrapped fragment".into(),
                    intra: Vec::new(),
                    syntax: Vec::new(),
                }));
                let ItemState::Ready(data) = &app.items[app.active].state else {
                    unreachable!()
                };
                assert_eq!(
                    composer.highlighted_side(item_id, data, &fragment),
                    Some(SelSide::Unified)
                );
                assert!(composer
                    .highlighted_side(item_id, data, &Row::Binary)
                    .is_none());
                composer.side = CommentSide::Left;
                composer.line = 12;
                composer.start_line = None;
                assert_eq!(
                    composer.highlighted_side(item_id, data, &fragment),
                    Some(SelSide::Unified)
                );
                composer.path = "another.rs".into();
                assert!(composer
                    .highlighted_side(item_id, data, &fragment)
                    .is_none());
                composer.path = "a.rs".into();
                composer.placement = ComposerPlacement::ConversationMain;
                assert!(composer
                    .highlighted_side(item_id, data, &fragment)
                    .is_none());
                app.close_composer(window, cx);
            });
        });
    }

    #[gpui::test]
    fn shift_click_opens_a_range_composer_through_the_mouse_handlers(
        cx: &mut gpui::TestAppContext,
    ) {
        let fake = FakePr::with_threads(Vec::new());
        let (window, app) = queue_window(cx, fake);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        install_item(&app, range_pr_item(), cx);
        cx.run_until_parked();
        let at_row = |row: usize, cx: &mut gpui::TestAppContext| {
            cx.update(|cx| {
                let app = app.read(cx);
                let data = app.active_data().unwrap();
                let scroll = data.scroll.0.borrow();
                let bounds = scroll.base_handle.bounds();
                point(
                    bounds.left() + px(140.),
                    bounds.top()
                        + scroll.base_handle.offset().y
                        + px((row as f32 + 0.5) * row_height()),
                )
            })
        };
        let first = at_row(1, &mut vcx.cx);
        vcx.simulate_mouse_down(first, MouseButton::Left, gpui::Modifiers::default());
        vcx.simulate_mouse_up(first, MouseButton::Left, gpui::Modifiers::default());
        vcx.update(|window, cx| {
            assert!(app.read(cx).focus_handle.is_focused(window));
        });
        let last = at_row(3, &mut vcx.cx);
        let shift = gpui::Modifiers {
            shift: true,
            ..Default::default()
        };
        vcx.simulate_mouse_down(last, MouseButton::Left, shift);
        vcx.simulate_mouse_up(last, MouseButton::Left, shift);
        vcx.run_until_parked();
        let range_selectors = [
            (
                "comment-range-Unified-1",
                "comment-gutter-Unified-1",
                "diff-code-1",
            ),
            (
                "comment-range-Unified-2",
                "comment-gutter-Unified-2",
                "diff-code-2",
            ),
            (
                "comment-range-Unified-3",
                "comment-gutter-Unified-3",
                "diff-code-3",
            ),
        ];
        for (highlight, gutter, code) in range_selectors {
            let highlight = vcx.debug_bounds(highlight).unwrap();
            let gutter = vcx.debug_bounds(gutter).unwrap();
            let code = vcx.debug_bounds(code).unwrap();
            assert_eq!(highlight.top(), code.top());
            assert_eq!(highlight.size.height, px(row_height()));
            assert!(highlight.contains(&code.center()));
            assert!(highlight.contains(&gutter.center()));
        }
        assert!(vcx.debug_bounds("comment-range-Unified-4").is_none());
        vcx.simulate_input("range feedback");
        vcx.update(|window, cx| {
            let app = app.read(cx);
            let composer = app.composer.as_ref().expect("shift-click composes a range");
            assert!(composer.input.focus_handle(cx).is_focused(window));
            assert_eq!(composer.input.read(cx).value(), "range feedback");
            assert!(
                app.active_data().unwrap().selection.is_none(),
                "shift-click selects a comment range, not diff text"
            );
            assert!(app.drag_anchor.is_none());
            assert_eq!(composer.start_line, Some(1));
            assert_eq!(composer.line, 3);
            assert_eq!(composer.side, CommentSide::Right);
            assert_eq!(
                composer.row_ix, 3,
                "the card sits below the range's last row"
            );
        });
        vcx.update(|window, cx| {
            app.update(cx, |app, cx| app.close_composer(window, cx));
            assert!(app.read(cx).composer.is_none());
        });
    }
}
