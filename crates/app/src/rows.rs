//! The diff pane's display rows: the row/cell model, diff-to-rows building, and row rendering.

use crate::selection::SelSide;
use crate::threads::{wrap_body, CommentIndex, CommentSide, FileAnchors};
use crate::tree::status_style;
use crate::{row_height, theme, ReviewApp, SPLIT_DIVIDER};
use diff_core::{DiffRow, FileStatus, Hunk, PrDiff};
use gpui::{div, prelude::*, px, HighlightStyle, Hsla, SharedString, StyledText};
use gpui_component::{tag::Tag, Sizable as _};
use std::collections::HashMap;
use std::ops::Range;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum LineKind {
    Context,
    Added,
    Removed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ViewMode {
    Unified,
    Split,
}

/// One side of a split row: line number, kind, text, word-level highlights,
/// and tree-sitter token spans.
pub(crate) struct Cell {
    pub(crate) no: u32,
    pub(crate) kind: LineKind,
    pub(crate) text: SharedString,
    pub(crate) intra: Vec<Range<usize>>,
    pub(crate) syntax: Vec<(Range<usize>, syntax::Token)>,
}

pub(crate) enum Row {
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
    Binary,
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

pub(crate) fn is_comment_row(row: &Row) -> bool {
    matches!(
        row,
        Row::CommentHeader { .. } | Row::CommentBody { .. } | Row::CommentActions { .. }
    )
}

/// Index of the row with the widest min-content: the longest no-wrap text
/// (diff lines, comment bodies, file-header paths). `uniform_list` measures
/// exactly one item to derive its horizontal scroll range, so the list must
/// be pointed at this row — its default, row 0 (the file header), is always
/// narrower than the viewport, which pins `offset.x` to zero. See
/// `UniformList::with_width_from_item` where the diff list is built.
pub(crate) fn widest_row_ix(rows: &[Row]) -> usize {
    rows.iter()
        .enumerate()
        .max_by_key(|(_, row)| match row {
            Row::Line { text, .. } => text.chars().count(),
            Row::CommentBody { line, .. } => line.chars().count(),
            Row::FileHeader { path, .. } => path.chars().count(),
            _ => 0,
        })
        .map(|(ix, _)| ix)
        .unwrap_or(0)
}

/// Index of the `n`-th non-comment row (0-based), clamped to the last row.
/// Comment rows are pure insertions relative to the diff rows, so this maps
/// a position across rebuilds that only add or remove comment rows.
pub(crate) fn nth_noncomment_row(rows: &[Row], n: usize) -> usize {
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
                author: comment.author.clone().into(),
                when: comment.time.short_age(now).into(),
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

pub(crate) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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
/// span tables (per line, both sides). The authoritative re-diffed hunks
/// live in `diff.files[ix].hunks` — this is the extra state needed to render
/// spans and fill in the context around the hunks. Lives in
/// `ItemData.upgrades`, so a view-mode toggle preserves it and a
/// refresh (which rebuilds ItemData contents) resets it.
pub(crate) struct FileUpgrade {
    /// The complete new-side file, line-ending-normalized, split into lines.
    pub(crate) new_lines: Vec<SharedString>,
    pub(crate) old_spans: Vec<Vec<(Range<usize>, syntax::Token)>>,
    pub(crate) new_spans: Vec<Vec<(Range<usize>, syntax::Token)>>,
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

/// Emit gap `gap_ix` of an upgraded file into `rows` as plain context:
/// nothing when no lines are hidden there — the whole file always renders,
/// with no clickable `Gap` affordance.
fn push_gap_rows(
    rows: &mut Vec<Row>,
    upgrade: &FileUpgrade,
    hunks: &[Hunk],
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

/// Borrowed source data and presentation options for one row build.
pub(crate) struct RowsView<'a> {
    pub(crate) diff: &'a PrDiff,
    pub(crate) upgrades: &'a HashMap<usize, FileUpgrade>,
    pub(crate) comments: Option<&'a CommentIndex>,
    pub(crate) options: RowOptions,
}

pub(crate) struct RowOptions {
    pub(crate) mode: ViewMode,
    pub(crate) file_ix: usize,
    pub(crate) comments_visible: bool,
    pub(crate) comment_wrap: usize,
}

/// Display rows and navigation offsets, installed together by `ItemData::set_rows`.
pub(crate) struct BuiltRows {
    pub(crate) rows: Vec<Row>,
    pub(crate) file_rows: Vec<usize>,
    pub(crate) hunk_rows: Vec<usize>,
}

/// Flatten the diff into display rows plus the row indices of file headers and
/// hunk starts. Split mode pairs removed/added runs positionally into
/// two-cell rows; unequal runs leave one-sided rows. Files present in
/// `upgrades` use whole-file syntax spans, with the gaps between hunks
/// filled in as plain context — the whole file renders, with no `@@`
/// hunk-marker rows. Review threads render beneath the line they anchor to
/// when `comments_visible` is set; file headers always carry the comment counts.
///
/// `RowsView::options.file_ix` names the one file the build renders (file
/// mode is the only presentation).
pub(crate) fn build_rows(view: RowsView<'_>) -> BuiltRows {
    let RowsView {
        diff,
        upgrades,
        comments,
        options:
            RowOptions {
                mode,
                file_ix: focus,
                comments_visible: show_comments,
                comment_wrap: wrap,
            },
    } = view;
    let mut rows = Vec::new();
    let mut file_rows = Vec::new();
    let mut hunk_rows = Vec::new();
    let now = now_unix();

    for (file_ix, file) in diff.files.iter().enumerate() {
        if focus != file_ix {
            continue;
        }
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
            // The first row of the hunk — its `@@` marker is never rendered;
            // hunk boundaries read from the highlighting alone.
            hunk_rows.push(rows.len());
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
                file.hunks.len(),
                mode,
                path,
                anchors,
                now,
                wrap,
            );
        }
    }

    BuiltRows {
        rows,
        file_rows,
        hunk_rows,
    }
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
/// `entity` is used by CommentActions rows (click opens the reply
/// composer); `ix` is the display-row index.
pub(crate) fn render_row(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::minimap::{minimap_rows, MinimapKind};
    use crate::selection::row_side_text;
    use crate::test_util::{
        add, app_rc as rc, ctx, mrow, rem, row_name, sample_comments, sample_diff, two_file_diff,
        upgraded_diff,
    };
    use crate::threads::{group_comments, COMMENT_WRAP_CHARS};
    use diff_core::diff_texts;
    use syntax::Token;

    #[test]
    fn widest_row_ix_points_at_the_longest_nowrap_row() {
        let line = |text: &str| Row::Line {
            old_no: Some(1),
            new_no: Some(1),
            kind: LineKind::Context,
            text: text.to_string().into(),
            intra: Vec::new(),
            syntax: Vec::new(),
        };
        let rows = vec![
            Row::FileHeader {
                path: "src/a/very/long/path.rs".into(),
                old_path: None,
                status: FileStatus::Modified,
                additions: 1,
                deletions: 1,
                comments: 0,
                outdated: 0,
            },
            line("short"),
            line("a much longer line that forces the scroll range wide"),
            Row::Binary,
        ];
        assert_eq!(widest_row_ix(&rows), 2);
        // Empty rows must not panic — callers fall back to row 0.
        assert_eq!(widest_row_ix(&[]), 0);
    }

    fn cell(cell: &Option<Cell>) -> (u32, LineKind, &str, &[Range<usize>]) {
        let cell = cell.as_ref().expect("expected a cell");
        (cell.no, cell.kind, cell.text.as_ref(), &cell.intra)
    }

    #[test]
    fn build_rows_empty_diff_has_no_rows_or_offsets() {
        for mode in [ViewMode::Unified, ViewMode::Split] {
            let built = build_rows(RowsView {
                diff: &PrDiff::default(),
                upgrades: &HashMap::new(),
                comments: None,
                options: RowOptions {
                    mode,
                    file_ix: 0,
                    comments_visible: true,
                    comment_wrap: COMMENT_WRAP_CHARS,
                },
            });
            assert!(built.rows.is_empty());
            assert!(built.file_rows.is_empty());
            assert!(built.hunk_rows.is_empty());
        }
    }

    #[test]
    fn build_rows_out_of_range_file_has_no_rows_or_offsets() {
        let diff = two_file_diff();
        let comments = sample_comments();
        for mode in [ViewMode::Unified, ViewMode::Split] {
            for file_ix in [diff.files.len(), usize::MAX] {
                let built = build_rows(RowsView {
                    diff: &diff,
                    upgrades: &HashMap::new(),
                    comments: Some(&comments),
                    options: RowOptions {
                        mode,
                        file_ix,
                        comments_visible: true,
                        comment_wrap: COMMENT_WRAP_CHARS,
                    },
                });
                assert!(built.rows.is_empty());
                assert!(built.file_rows.is_empty());
                assert!(built.hunk_rows.is_empty());
            }
        }
    }

    #[test]
    fn split_context_fills_both_cells() {
        let BuiltRows { rows, .. } = build_rows(RowsView {
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
        // rows[0] = FileHeader, rows[1] = first context (no `@@` row).
        match &rows[1] {
            Row::SplitLine { left, right } => {
                assert_eq!(cell(left), (1, LineKind::Context, "ctx", &[][..]));
                assert_eq!(cell(right), (1, LineKind::Context, "ctx", &[][..]));
            }
            _ => panic!("expected split line"),
        }
    }

    #[test]
    fn split_pairs_equal_runs_positionally() {
        let BuiltRows { rows, .. } = build_rows(RowsView {
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
        match &rows[2] {
            Row::SplitLine { left, right } => {
                assert_eq!(cell(left), (2, LineKind::Removed, "old1", &[0..3][..]));
                assert_eq!(cell(right), (2, LineKind::Added, "new1", &[0..3][..]));
            }
            _ => panic!("expected split line"),
        }
        match &rows[3] {
            Row::SplitLine { left, right } => {
                assert_eq!(cell(left), (3, LineKind::Removed, "old2", &[][..]));
                assert_eq!(cell(right), (3, LineKind::Added, "new2", &[][..]));
            }
            _ => panic!("expected split line"),
        }
        // Equal run + 2 context rows: 4 split lines for a 6-row hunk.
        match &rows[4] {
            Row::SplitLine { left, right } => {
                assert_eq!(cell(left).2, "tail");
                assert_eq!(cell(right).2, "tail");
            }
            _ => panic!("expected split line"),
        }
    }

    #[test]
    fn split_unequal_and_lone_runs_are_one_sided() {
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
        let h2 = hunk_rows[1];
        // h2 itself pairs r1 with a1.
        match &rows[h2] {
            Row::SplitLine { left, right } => {
                assert_eq!(cell(left), (10, LineKind::Removed, "r1", &[][..]));
                assert_eq!(cell(right), (10, LineKind::Added, "a1", &[][..]));
            }
            _ => panic!("expected split line"),
        }
        // 2 removed / 1 added: the second removed row is left-only.
        match &rows[h2 + 1] {
            Row::SplitLine { left, right } => {
                assert_eq!(cell(left), (11, LineKind::Removed, "r2", &[][..]));
                assert!(right.is_none());
            }
            _ => panic!("expected split line"),
        }
        // Lone added run after context: right-only.
        match &rows[h2 + 3] {
            Row::SplitLine { left, right } => {
                assert!(left.is_none());
                assert_eq!(cell(right), (12, LineKind::Added, "lone", &[][..]));
            }
            _ => panic!("expected split line"),
        }
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

    #[test]
    fn upgraded_file_renders_whole_with_hunks_inline() {
        let (diff, upgrades) = upgraded_diff();
        for mode in [ViewMode::Unified, ViewMode::Split] {
            let BuiltRows {
                rows, hunk_rows, ..
            } = build_rows(RowsView {
                diff: &diff,
                upgrades: &upgrades,
                comments: None,
                options: RowOptions {
                    mode,
                    file_ix: 0,
                    comments_visible: true,
                    comment_wrap: COMMENT_WRAP_CHARS,
                },
            });
            // FileHeader, 6 leading context rows, the hunk (8 rows unified:
            // 3 context, 1 removed, 1 added, 3 context; 7 split: removed and
            // added pair up), 7 trailing context rows — the whole 20-line
            // file, no gap affordances.
            assert_eq!(
                rows.len(),
                1 + 6 + if mode == ViewMode::Unified { 8 } else { 7 } + 7
            );
            assert_eq!(hunk_rows, vec![7]);
            if mode == ViewMode::Unified {
                match &rows[1] {
                    Row::Line { new_no, text, .. } => {
                        assert_eq!(*new_no, Some(1));
                        assert_eq!(text.as_ref(), "line 1");
                    }
                    other => panic!("expected leading context, got {}", row_name(other)),
                }
                match rows.last().unwrap() {
                    Row::Line { new_no, text, .. } => {
                        assert_eq!(*new_no, Some(20));
                        assert_eq!(text.as_ref(), "line 20");
                    }
                    other => panic!("expected trailing context, got {}", row_name(other)),
                }
            }
        }
        // Un-upgraded build of the same diff paints just the hunk.
        let BuiltRows {
            rows, hunk_rows, ..
        } = build_rows(RowsView {
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
        assert_eq!(hunk_rows, vec![1]);
        assert!(matches!(rows[1], Row::Line { .. }));
        assert_eq!(
            rows.iter()
                .filter(|row| matches!(row, Row::Line { .. }))
                .count(),
            8
        );
    }

    #[test]
    fn gap_context_rows_carry_correct_numbers() {
        let (diff, upgrades) = upgraded_diff();

        let BuiltRows {
            rows, hunk_rows, ..
        } = build_rows(RowsView {
            diff: &diff,
            upgrades: &upgrades,
            comments: None,
            options: RowOptions {
                mode: ViewMode::Unified,
                file_ix: 0,
                comments_visible: true,
                comment_wrap: COMMENT_WRAP_CHARS,
            },
        });
        // The leading gap renders as 6 context rows before the hunk.
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
        // Split mode: same gap as two-cell context rows.
        let BuiltRows { rows, .. } = build_rows(RowsView {
            diff: &diff,
            upgrades: &upgrades,
            comments: None,
            options: RowOptions {
                mode: ViewMode::Split,
                file_ix: 0,
                comments_visible: true,
                comment_wrap: COMMENT_WRAP_CHARS,
            },
        });
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

        // The trailing gap continues the numbering past the hunk.
        let BuiltRows { rows, .. } = build_rows(RowsView {
            diff: &diff,
            upgrades: &upgrades,
            comments: None,
            options: RowOptions {
                mode: ViewMode::Unified,
                file_ix: 0,
                comments_visible: true,
                comment_wrap: COMMENT_WRAP_CHARS,
            },
        });
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

    #[test]
    fn file_mode_renders_the_whole_file_with_hunks_inline() {
        let (diff, upgrades) = upgraded_diff();
        let BuiltRows { rows, .. } = build_rows(RowsView {
            diff: &diff,
            upgrades: &upgrades,
            comments: None,
            options: RowOptions {
                mode: ViewMode::Unified,
                file_ix: 0,
                comments_visible: true,
                comment_wrap: COMMENT_WRAP_CHARS,
            },
        });
        // All 20 new-side lines are present, the change highlighted inline.
        let lines: Vec<(u32, String)> = rows
            .iter()
            .filter_map(|row| match row {
                Row::Line {
                    new_no: Some(no),
                    text,
                    ..
                } => Some((*no, text.to_string())),
                _ => None,
            })
            .collect();
        assert_eq!(lines.len(), 20);
        assert_eq!(lines[9], (10, "line ten".to_string()));
    }

    #[test]
    fn file_mode_paints_hunk_strips_before_the_upgrade_lands() {
        let (diff, _) = upgraded_diff();
        let BuiltRows { rows, .. } = build_rows(RowsView {
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
        // Phase 1: exactly the patch's hunk — 3 context, 1 removed, 1
        // added, 3 context — and nothing else, with no `@@` marker row.
        assert!(matches!(rows[1], Row::Line { .. }));
        assert_eq!(
            rows.iter()
                .filter(|row| matches!(row, Row::Line { .. }))
                .count(),
            8
        );
    }

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

    #[test]
    fn header_indices_are_correct_in_both_modes() {
        let diff = sample_diff();
        for mode in [ViewMode::Unified, ViewMode::Split] {
            let BuiltRows {
                rows,
                file_rows,
                hunk_rows,
            } = build_rows(RowsView {
                diff: &diff,
                upgrades: &HashMap::new(),
                comments: None,
                options: RowOptions {
                    mode,
                    file_ix: 0,
                    comments_visible: true,
                    comment_wrap: COMMENT_WRAP_CHARS,
                },
            });
            // Only the focused file builds: a.rs, header first.
            assert_eq!(file_rows, vec![0]);
            assert_eq!(hunk_rows.len(), 2);
            assert!(matches!(rows[0], Row::FileHeader { .. }));
            for &ix in &hunk_rows {
                assert!(matches!(rows[ix], Row::Line { .. } | Row::SplitLine { .. }));
            }
            // Binary file: focused alone, header followed by the binary row.
            let BuiltRows {
                rows, file_rows, ..
            } = build_rows(RowsView {
                diff: &diff,
                upgrades: &HashMap::new(),
                comments: None,
                options: RowOptions {
                    mode,
                    file_ix: 1,
                    comments_visible: true,
                    comment_wrap: COMMENT_WRAP_CHARS,
                },
            });
            assert_eq!(file_rows, vec![0]);
            assert!(matches!(rows[0], Row::FileHeader { .. }));
            assert!(matches!(rows[1], Row::Binary));
        }
        // Unified emits one row per diff row; split collapses the equal run.
        let BuiltRows { rows: unified, .. } = build_rows(RowsView {
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
        let BuiltRows { rows: split, .. } = build_rows(RowsView {
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

    #[test]
    fn unified_rows_insert_threads_beneath_their_anchor() {
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
        // rows: FileHeader, ctx, rem old1 (old_no 2) + LEFT thread, rem
        // old2, add new1 (new_no 2) + RIGHT thread, …
        let names: Vec<&str> = rows.iter().map(row_name).collect();
        assert_eq!(
            &names[..12],
            &[
                "FileHeader",
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
                "CommentBody",
            ]
        );
        assert_eq!(names[12], "CommentActions");
        // The LEFT thread is carol's; the reply flag follows position.
        match &rows[3] {
            Row::CommentHeader {
                author, is_reply, ..
            } => {
                assert_eq!(author.as_ref(), "carol");
                assert!(!is_reply);
            }
            other => panic!("expected comment header, got {}", row_name(other)),
        }
        match &rows[10] {
            Row::CommentHeader {
                author, is_reply, ..
            } => {
                assert_eq!(author.as_ref(), "bob");
                assert!(is_reply);
            }
            other => panic!("expected reply header, got {}", row_name(other)),
        }
        // Actions row targets the thread root and carries the anchor.
        match &rows[12] {
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
        // rows[2] pairs old1/new1 (both line 2): LEFT thread then RIGHT
        // thread directly beneath it.
        let names: Vec<&str> = rows.iter().map(row_name).collect();
        assert_eq!(
            &names[1..10],
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
        assert_eq!(names[10], "CommentActions");
        assert_eq!(names[11], "SplitLine"); // old2 | new2
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
        // rows[3..6] are carol's LEFT thread, rows[6..11] the RIGHT one; each
        // row of a thread carries the side it was anchored to.
        let halves: Vec<Option<Option<CommentSide>>> = rows[3..11].iter().map(row_half).collect();
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
            let BuiltRows { rows, .. } = build_rows(RowsView {
                diff: &sample_diff(),
                upgrades: &HashMap::new(),
                comments: Some(&index),
                options: RowOptions {
                    mode,
                    file_ix: 0,
                    comments_visible: true,
                    comment_wrap: cap,
                },
            });
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
        // Unified has one column, so no comment row is confined to a half.
        assert!(rows.iter().filter_map(row_half).all(|half| half.is_none()));
        // ...and the diff really does contain comment rows to check.
        assert!(rows.iter().any(is_comment_row));
    }

    #[test]
    fn hidden_comments_keep_header_counts() {
        let index = sample_comments();
        let BuiltRows { rows, .. } = build_rows(RowsView {
            diff: &sample_diff(),
            upgrades: &HashMap::new(),
            comments: Some(&index),
            options: RowOptions {
                mode: ViewMode::Unified,
                file_ix: 0,
                comments_visible: false,
                comment_wrap: COMMENT_WRAP_CHARS,
            },
        });
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
        let BuiltRows { rows, .. } = build_rows(RowsView {
            diff: &sample_diff(),
            upgrades: &HashMap::new(),
            comments: None,
            options: RowOptions {
                mode: ViewMode::Unified,
                file_ix: 0,
                comments_visible: true,
                comment_wrap: COMMENT_WRAP_CHARS,
            },
        });
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
    fn gap_context_rows_host_threads() {
        let (diff, upgrades) = upgraded_diff();
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
        let BuiltRows { rows, .. } = build_rows(RowsView {
            diff: &diff,
            upgrades: &upgrades,
            comments: Some(&index),
            options: RowOptions {
                mode: ViewMode::Unified,
                file_ix: 0,
                comments_visible: true,
                comment_wrap: COMMENT_WRAP_CHARS,
            },
        });
        // The gap always renders as context (the whole file shows), so the
        // thread anchors immediately: FileHeader, ctx 1, ctx 2, ctx 3, then
        // the thread.
        assert_eq!(row_name(&rows[3]), "Line");
        assert_eq!(row_name(&rows[4]), "CommentHeader");
        assert_eq!(row_name(&rows[5]), "CommentBody");
        assert_eq!(row_name(&rows[6]), "CommentActions");
    }

    #[test]
    fn nth_noncomment_row_maps_positions_across_comment_insertions() {
        let index = sample_comments();
        let BuiltRows { rows: plain, .. } = build_rows(RowsView {
            diff: &sample_diff(),
            upgrades: &HashMap::new(),
            comments: None,
            options: RowOptions {
                mode: ViewMode::Unified,
                file_ix: 0,
                comments_visible: true,
                comment_wrap: COMMENT_WRAP_CHARS,
            },
        });
        let BuiltRows { rows: with, .. } = build_rows(RowsView {
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
        // Every plain row maps to the same row content with comments shown.
        for (n, row) in plain.iter().enumerate() {
            let ix = nth_noncomment_row(&with, n);
            assert_eq!(row_name(&with[ix]), row_name(row), "row {n}");
        }
        // n beyond the end clamps to the last row.
        assert_eq!(nth_noncomment_row(&plain, 999), plain.len() - 1);
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

    /// File mode rides the threads along: the displayed file's threads
    /// render under their anchor — with the reply affordance — and other
    /// files' threads stay off the pane.
    #[test]
    fn file_mode_rows_carry_only_the_displayed_files_threads() {
        let index = group_comments(vec![
            rc(
                1,
                "a.rs",
                Some("RIGHT"),
                Some(1),
                "on a's added line",
                "alice",
                "2026-01-01T00:00:00Z",
                None,
            ),
            rc(
                2,
                "b.rs",
                Some("LEFT"),
                Some(1),
                "on b's removed line",
                "bob",
                "2026-01-02T00:00:00Z",
                None,
            ),
        ]);
        let diff = two_file_diff();

        // File mode on a.rs (file 0): header, its added line 1, the thread,
        // then the rest of the file — and nothing from b.rs.
        let BuiltRows { rows, .. } = build_rows(RowsView {
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
        let names: Vec<&str> = rows.iter().map(row_name).collect();
        // The whole pane: a.rs's header, its two added lines, the one
        // thread under its anchor — and nothing from b.rs.
        assert_eq!(
            &names[..],
            &[
                "FileHeader",
                "Line",          // add 1
                "CommentHeader", // alice on a.rs RIGHT 1
                "CommentBody",
                "CommentActions",
                "Line", // add 10
            ]
        );
        match &rows[4] {
            Row::CommentActions {
                root_id,
                path,
                side,
                line,
                ..
            } => {
                assert_eq!(
                    (*root_id, path.as_ref(), *side, *line),
                    (1, "a.rs", CommentSide::Right, 1)
                );
            }
            other => panic!("expected actions row, got {}", row_name(other)),
        }
        // The reply affordance rides along under the thread.

        // On b.rs (file 1): only b.rs's thread, anchored on its removed
        // line's left side.
        let BuiltRows { rows, .. } = build_rows(RowsView {
            diff: &diff,
            upgrades: &HashMap::new(),
            comments: Some(&index),
            options: RowOptions {
                mode: ViewMode::Unified,
                file_ix: 1,
                comments_visible: true,
                comment_wrap: COMMENT_WRAP_CHARS,
            },
        });
        let names: Vec<&str> = rows.iter().map(row_name).collect();
        // On b.rs (file 1): only b.rs's header, its removed line, and its
        // own thread, anchored on the left side.
        assert_eq!(
            &names[..],
            &[
                "FileHeader",
                "Line",          // rem 1
                "CommentHeader", // bob on b.rs LEFT 1
                "CommentBody",
                "CommentActions",
            ]
        );
        match &rows[4] {
            Row::CommentActions {
                root_id,
                path,
                side,
                line,
                ..
            } => {
                assert_eq!(
                    (*root_id, path.as_ref(), *side, *line),
                    (2, "b.rs", CommentSide::Left, 1)
                );
            }
            other => panic!("expected actions row, got {}", row_name(other)),
        }
    }
}
