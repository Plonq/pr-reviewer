//! Literal search in source coordinates, projected onto the visible file's
//! display rows. Indexing never builds/highlights off-screen files.
#[cfg(test)]
mod tests;

use crate::local_store::ReviewPosition;
use crate::minimap::{text_range_frac, MinimapMatch};
use crate::rows::{gap_span, source_row, FileUpgrade, LineKind, Row};
use crate::selection::{row_side_text, selection_text, SelSide};
use crate::{theme, ItemData, ItemPane, ReviewApp};
use diff_core::{DiffRow, FileStatus, PrDiff};
use gpui::component::{
    button::{Button, ButtonVariants as _},
    input::{Escape, Input, InputEvent, InputState},
    Sizable as _,
};
use gpui::{
    div, prelude::*, px, Context, Entity, KeyBinding, ScrollStrategy, Subscription, Window,
};
use std::collections::{HashMap, HashSet};
use std::ops::Range;

gpui::actions!(
    diff_search,
    [OpenSearch, OpenDiffSearch, NextMatch, PreviousMatch]
);

pub(crate) fn keybindings() -> [KeyBinding; 8] {
    [
        KeyBinding::new("cmd-f", OpenSearch, Some("ReviewApp")),
        KeyBinding::new("cmd-f", OpenSearch, Some("DiffSearch")),
        KeyBinding::new("cmd-shift-f", OpenDiffSearch, Some("ReviewApp")),
        KeyBinding::new("cmd-shift-f", OpenDiffSearch, Some("DiffSearch")),
        KeyBinding::new("enter", NextMatch, Some("ReviewApp")),
        KeyBinding::new("enter", NextMatch, Some("DiffSearch > Input")),
        KeyBinding::new("shift-enter", PreviousMatch, Some("DiffSearch > Input")),
        KeyBinding::new("shift-enter", PreviousMatch, Some("ReviewApp")),
    ]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SearchScope {
    CurrentFile,
    AllFiles,
    ChangedLines,
}

impl SearchScope {
    fn label(self) -> &'static str {
        match self {
            Self::CurrentFile => "Current file",
            Self::AllFiles => "All files",
            Self::ChangedLines => "Changed lines",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Occurrence {
    file: usize,
    // Shared context is indexed once, on the new side; removals use the old.
    removed: bool,
    line: u32,
    range: Range<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Span {
    row: usize,
    side: SelSide,
    range: Range<usize>,
}

#[derive(Default)]
struct Matches {
    occurrences: Vec<Occurrence>,
    current: Option<usize>,
    by_row: Vec<Vec<(usize, Span)>>,
    /// Cached minimap positions, rebuilt with the display-row projection.
    marks: Vec<MinimapMatch>,
    skipped_large: usize,
    patch_only: usize,
}

impl Matches {
    fn find(
        diff: &PrDiff,
        upgrades: &HashMap<usize, FileUpgrade>,
        expanded: &HashSet<(usize, u32)>,
        scope: SearchScope,
        origin: usize,
        query: &str,
    ) -> Self {
        let mut result = Self::default();
        for (file_ix, file) in diff.files.iter().enumerate() {
            if (scope == SearchScope::CurrentFile && file_ix != origin)
                || file.status == FileStatus::Binary
            {
                continue;
            }
            let upgrade = upgrades.get(&file_ix);
            if crate::presentation::auto_collapse(crate::presentation::file_lines(file, upgrade))
                && !expanded.contains(&(file_ix, 0))
            {
                result.skipped_large += 1;
                continue;
            }
            if upgrade.is_none() && scope != SearchScope::ChangedLines {
                result.patch_only += 1;
            }
            if query.is_empty() {
                continue;
            }
            let mut line = |removed, line, text: &str| {
                result
                    .occurrences
                    .extend(text.match_indices(query).map(|(start, _)| Occurrence {
                        file: file_ix,
                        removed,
                        line,
                        range: start..start + query.len(),
                    }));
            };
            for gap in 0..=file.hunks.len() {
                if scope != SearchScope::ChangedLines {
                    if let Some(upgrade) = upgrade {
                        let (_, new_lo, count) =
                            gap_span(&file.hunks, gap, upgrade.new_lines.len() as u32);
                        for no in new_lo..new_lo + count {
                            line(false, no, &upgrade.new_lines[no as usize - 1]);
                        }
                    }
                }
                let Some(hunk) = file.hunks.get(gap) else {
                    continue;
                };
                for row in &hunk.rows {
                    match row {
                        DiffRow::Added { new_no, text, .. } => line(false, *new_no, text),
                        DiffRow::Removed { old_no, text, .. } => line(true, *old_no, text),
                        DiffRow::Context { new_no, text, .. }
                            if scope != SearchScope::ChangedLines =>
                        {
                            line(false, *new_no, text);
                        }
                        _ => {}
                    }
                }
            }
        }
        result.current = (!result.occurrences.is_empty()).then(|| {
            result
                .occurrences
                .iter()
                .position(|m| m.file >= origin)
                .unwrap_or(0)
        });
        result
    }

    /// Reproject without rescanning source text when comments, wrapping, or the
    /// displayed file change. Split context gets two highlights, one result.
    fn project(&mut self, rows: &[Row], file: usize) {
        let mut lines: HashMap<(bool, u32), Vec<(usize, &Occurrence)>> = HashMap::new();
        let lo = self.occurrences.partition_point(|m| m.file < file);
        let hi = self.occurrences.partition_point(|m| m.file <= file);
        for ix in lo..hi {
            let m = &self.occurrences[ix];
            lines.entry((m.removed, m.line)).or_default().push((ix, m));
        }
        self.by_row = vec![Vec::new(); rows.len()];
        self.marks.clear();
        for side in [SelSide::Unified, SelSide::Left, SelSide::Right] {
            let mut offset = 0;
            for (row_ix, row) in rows.iter().enumerate() {
                if !matches!(row, Row::Continuation(_)) {
                    offset = 0;
                }
                let Some(text) = row_side_text(row, side) else {
                    continue;
                };
                let key = match (source_row(row), side) {
                    (
                        Row::Line {
                            kind: LineKind::Removed,
                            old_no: Some(no),
                            ..
                        },
                        SelSide::Unified,
                    ) => Some((true, *no)),
                    (
                        Row::Line {
                            new_no: Some(no), ..
                        },
                        SelSide::Unified,
                    ) => Some((false, *no)),
                    (
                        Row::SplitLine {
                            left: Some(cell),
                            right,
                        },
                        SelSide::Left,
                    ) => {
                        if cell.kind == LineKind::Removed {
                            Some((true, cell.no))
                        } else {
                            right.as_ref().map(|r| (false, r.no))
                        }
                    }
                    (
                        Row::SplitLine {
                            right: Some(cell), ..
                        },
                        SelSide::Right,
                    ) => Some((false, cell.no)),
                    _ => None,
                };
                if let Some(matches) = key.and_then(|key| lines.get(&key)) {
                    for (ix, m) in matches {
                        let start = m.range.start.max(offset);
                        let end = m.range.end.min(offset + text.len());
                        if start < end {
                            if let Some(range) =
                                text_range_frac(text, &(start - offset..end - offset))
                            {
                                self.marks.push(MinimapMatch {
                                    row: row_ix,
                                    side,
                                    range,
                                });
                            }
                            self.by_row[row_ix].push((
                                *ix,
                                Span {
                                    row: row_ix,
                                    side,
                                    range: start - offset..end - offset,
                                },
                            ));
                        }
                    }
                }
                offset += text.len();
            }
        }
    }

    fn step(&mut self, delta: isize) {
        if let Some(current) = self.current {
            self.current = Some(
                (current as isize + delta).rem_euclid(self.occurrences.len() as isize) as usize,
            );
        }
    }

    fn current_span(&self) -> Option<&Span> {
        let current = self.current?;
        self.by_row
            .iter()
            .flatten()
            .find(|(ix, _)| *ix == current)
            .map(|(_, span)| span)
    }
}

pub(crate) struct Search {
    pub(crate) prior: ReviewPosition,
    input: Entity<InputState>,
    query: String,
    scope: SearchScope,
    origin: usize,
    matches: Matches,
    dirty: bool,
    expanded_large: HashSet<usize>,
    _subscription: Subscription,
}

impl Search {
    pub(crate) fn minimap_marks(&self) -> &[MinimapMatch] {
        &self.matches.marks
    }

    pub(crate) fn highlights(
        &self,
        row: usize,
        side: SelSide,
        cx: &gpui::App,
    ) -> Vec<(Range<usize>, gpui::Rgba)> {
        self.matches
            .by_row
            .get(row)
            .into_iter()
            .flatten()
            .filter(|(_, span)| span.side == side)
            .map(|(ix, span)| {
                let color = if Some(*ix) == self.matches.current {
                    theme::search_current_bg(cx)
                } else {
                    theme::search_match_bg(cx)
                };
                (span.range.clone(), color)
            })
            .collect()
    }
}

impl ItemData {
    /// Source data is immutable for a search's lifetime (refresh replaces the
    /// item). Only query/scope/large-file expansion require a new source index.
    pub(crate) fn reindex_search(&mut self) {
        let Some(search) = &mut self.search else {
            return;
        };
        let expanded_large = self
            .expanded_regions
            .iter()
            .filter_map(|(file, key)| (*key == 0).then_some(*file))
            .collect();
        if search.dirty || search.expanded_large != expanded_large {
            let previous = search
                .matches
                .current
                .and_then(|ix| search.matches.occurrences.get(ix))
                .cloned();
            search.matches = Matches::find(
                &self.diff,
                &self.upgrades,
                &self.expanded_regions,
                search.scope,
                search.origin,
                &search.query,
            );
            if let Some(previous) = previous {
                if let Some(ix) = search
                    .matches
                    .occurrences
                    .iter()
                    .position(|m| *m == previous)
                {
                    search.matches.current = Some(ix);
                }
            }
            search.dirty = false;
            search.expanded_large = expanded_large;
        }
        search.matches.project(&self.rows, self.file_ix);
    }

    pub(crate) fn end_search(&mut self) {
        if let Some(search) = self.search.take() {
            self.set_rows(self.row_build());
            self.restore_review_position(search.prior, true);
        }
    }
}

impl ReviewApp {
    fn search_visible(&self) -> bool {
        !self.show_queue
            && self
                .active_item()
                .is_some_and(|item| item.pane == ItemPane::Changes)
    }

    pub(crate) fn open_diff_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.search_visible() || self.composer().is_some() || self.review.is_some() {
            return;
        }
        self.open_search(window, cx);
        self.set_search_scope(SearchScope::AllFiles, window, cx);
    }

    pub(crate) fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.search_visible() || self.composer().is_some() || self.review.is_some() {
            return;
        }
        let Some(data) = self.active_data() else {
            return;
        };
        // Read the selection before rebuilding rows clears it. Use the same
        // source-text extraction as copy (Unicode, split sides, and wrapping).
        let selected = data
            .selection
            .as_ref()
            .map(|selection| selection_text(selection, &data.rows))
            .filter(|text| !text.is_empty());
        if let Some(input) = data.search.as_ref().map(|search| search.input.clone()) {
            if let Some(query) = selected {
                let data = self.active_data_mut().unwrap();
                data.selection = None;
                let search = data.search.as_mut().unwrap();
                search.query = query.clone();
                search.dirty = true;
                data.reindex_search();
                input.update(cx, |input, cx| input.set_value(query, window, cx));
                self.reveal_search_match(window, cx);
            }
            input.update(cx, |input, cx| input.focus(window, cx));
            return;
        }
        let Some(prior) = data.review_position() else {
            return;
        };
        let query = selected.unwrap_or_default();
        let input = cx.new(|cx| {
            let mut input = InputState::new(window, cx).placeholder("Find literal text…");
            input.set_value(query.clone(), window, cx);
            input
        });
        let subscription = cx.subscribe_in(&input, window, |this, input, event, window, cx| {
            // An input belonging to a background item must never search another item.
            if !this.search_visible()
                || !this
                    .active_data()
                    .and_then(|d| d.search.as_ref())
                    .is_some_and(|s| s.input == *input)
            {
                return;
            }
            if matches!(event, InputEvent::Change) {
                let query = input.read(cx).value().to_string();
                let data = this.active_data_mut().unwrap();
                let search = data.search.as_mut().unwrap();
                search.query = query;
                search.dirty = true;
                data.reindex_search();
                this.reveal_search_match(window, cx);
            }
        });
        let data = self.active_data_mut().unwrap();
        data.search = Some(Search {
            prior,
            input: input.clone(),
            query,
            scope: SearchScope::CurrentFile,
            origin: data.file_ix,
            matches: Matches::default(),
            dirty: true,
            expanded_large: HashSet::new(),
            _subscription: subscription,
        });
        // Temporarily reveal folded/whitespace context, but keep explicit
        // large-file safety banners until the reviewer clicks to expand them.
        data.rebuild_presentation();
        self.reveal_search_match(window, cx);
        input.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    pub(crate) fn set_search_scope(
        &mut self,
        scope: SearchScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(data) = self.active_data_mut() else {
            return;
        };
        let Some(search) = &mut data.search else {
            return;
        };
        search.scope = scope;
        if scope == SearchScope::CurrentFile {
            search.origin = data.file_ix;
        }
        search.dirty = true;
        data.reindex_search();
        self.reveal_search_match(window, cx);
        if let Some(search) = self.active_data().and_then(|d| d.search.as_ref()) {
            search.input.update(cx, |input, cx| input.focus(window, cx));
        }
    }

    pub(crate) fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.search_visible() || self.active_data().and_then(|d| d.search.as_ref()).is_none() {
            return false;
        }
        self.active_data_mut().unwrap().end_search();
        window.focus(&self.focus_handle, cx);
        cx.notify();
        true
    }

    pub(crate) fn step_search(
        &mut self,
        delta: isize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.search_visible() {
            return;
        }
        if let Some(search) = self.active_data_mut().and_then(|data| data.search.as_mut()) {
            search.matches.step(delta);
            self.reveal_search_match(window, cx);
        }
    }

    fn reveal_search_match(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let width = self.char_width(window);
        let Some(data) = self.active_data_mut() else {
            return;
        };
        let file = data
            .search
            .as_ref()
            .and_then(|s| s.matches.current.map(|ix| s.matches.occurrences[ix].file));
        if let Some(file) = file.filter(|file| *file != data.file_ix) {
            // Unlike manual navigation, result navigation keeps the search and
            // its original viewport, and does not overwrite per-file positions.
            data.file_ix = file;
            data.selection = None;
            data.cursor_side = None;
            data.set_rows(data.row_build());
        }
        if let Some(span) = data.search.as_ref().and_then(|s| s.matches.current_span()) {
            let row = span.row;
            let side = span.side;
            let col = row_side_text(&data.rows[row], side).unwrap()[..span.range.start]
                .chars()
                .count();
            let mut state = data.scroll.0.borrow_mut();
            let gutter = if side == SelSide::Unified {
                crate::UNIFIED_GUTTER
            } else {
                crate::SPLIT_GUTTER
            };
            let viewport = f32::from(state.base_handle.bounds().size.width);
            let available = if side == SelSide::Unified {
                viewport
            } else {
                (viewport - crate::SPLIT_DIVIDER) / 2.
            } - gutter;
            let x = (col as f32 * f32::from(width) - available.max(0.) / 2.).max(0.);
            let offset = state.base_handle.offset();
            state.base_handle.set_offset(gpui::point(px(-x), offset.y));
            state.deferred_scroll_to_item = None;
            drop(state);
            data.position_restore = None;
            data.cursor = row;
            data.cursor_side = Some(side);
            data.scroll
                .scroll_to_item_strict(row, ScrollStrategy::Center);
        }
        cx.notify();
    }

    /// Root-level overlay: text input must not inherit the diff's letter keys.
    pub(crate) fn render_search(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let search = self
            .search_visible()
            .then(|| self.active_data()?.search.as_ref())
            .flatten();
        let Some(search) = search else {
            return div().into_any_element();
        };
        let count = match search.matches.current {
            Some(ix) => format!("{} / {}", ix + 1, search.matches.occurrences.len()),
            None if search.query.is_empty() => "Enter: next · Esc: close".into(),
            None => "No matches".into(),
        };
        let mut limitations = Vec::new();
        if search.matches.skipped_large > 0 {
            limitations.push(format!(
                "{} large files excluded (expand to search)",
                search.matches.skipped_large
            ));
        }
        if search.matches.patch_only > 0 {
            limitations.push(format!(
                "{} files have patch content only",
                search.matches.patch_only
            ));
        }
        div()
            .id("diff-search")
            .debug_selector(|| "diff-search".into())
            .key_context("DiffSearch")
            .absolute()
            .top(px(80.))
            .right(px(20.))
            .w(px(460.))
            .p_2()
            .rounded_md()
            .shadow_md()
            .bg(theme::mantle(cx))
            .border_1()
            .border_color(theme::surface0(cx))
            .on_action(cx.listener(|this, _: &OpenSearch, window, cx| this.open_search(window, cx)))
            .on_action(
                cx.listener(|this, _: &OpenDiffSearch, window, cx| {
                    this.open_diff_search(window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &NextMatch, window, cx| this.step_search(1, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &PreviousMatch, window, cx| this.step_search(-1, window, cx)),
            )
            .on_action(cx.listener(|this, _: &Escape, window, cx| {
                this.close_search(window, cx);
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&search.input).small()),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme::subtext(cx))
                            .child(count),
                    )
                    .child(
                        Button::new("close-diff-search")
                            .ghost()
                            .small()
                            .label("×")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.close_search(window, cx);
                            })),
                    ),
            )
            .child(
                div().flex().gap_1().mt_1().children(
                    [
                        SearchScope::CurrentFile,
                        SearchScope::AllFiles,
                        SearchScope::ChangedLines,
                    ]
                    .into_iter()
                    .enumerate()
                    .map(|(ix, scope)| {
                        Button::new(("search-scope", ix))
                            .ghost()
                            .small()
                            .label(format!(
                                "{}{}",
                                if search.scope == scope { "✓ " } else { "" },
                                scope.label()
                            ))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.set_search_scope(scope, window, cx)
                            }))
                    }),
                ),
            )
            .when(!limitations.is_empty(), |bar| {
                bar.child(
                    div()
                        .mt_1()
                        .text_size(px(11.))
                        .text_color(theme::subtext(cx))
                        .child(limitations.join(" · ")),
                )
            })
            .into_any_element()
    }
}
