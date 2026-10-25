//! File-scoped literal search. Match coordinates stay in display-row byte
//! space; wrapped fragments are joined before matching, never across lines.
use crate::local_store::ReviewPosition;
use crate::rows::Row;
use crate::selection::{row_side_text, SelSide};
use crate::{theme, ItemData, ItemPane, ReviewApp};
use gpui::{
    div, prelude::*, px, Context, Entity, KeyBinding, ScrollStrategy, Subscription, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    input::{Escape, Input, InputEvent, InputState},
    Sizable as _,
};
use std::ops::Range;

gpui::actions!(diff_search, [OpenSearch, NextMatch, PreviousMatch]);

pub(crate) fn keybindings() -> [KeyBinding; 6] {
    [
        KeyBinding::new("cmd-f", OpenSearch, Some("ReviewApp")),
        KeyBinding::new("cmd-f", OpenSearch, Some("DiffSearch")),
        KeyBinding::new("enter", NextMatch, Some("ReviewApp")),
        KeyBinding::new("enter", NextMatch, Some("DiffSearch > Input")),
        KeyBinding::new("shift-enter", PreviousMatch, Some("DiffSearch > Input")),
        KeyBinding::new("shift-enter", PreviousMatch, Some("ReviewApp")),
    ]
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Span {
    row: usize,
    side: SelSide,
    range: Range<usize>,
}

#[derive(Default)]
struct Matches {
    occurrences: Vec<Vec<Span>>,
    current: Option<usize>,
    by_row: Vec<Vec<(usize, Span)>>,
}

impl Matches {
    fn find(rows: &[Row], query: &str) -> Self {
        let mut occurrences = Vec::new();
        if !query.is_empty() {
            for side in [SelSide::Unified, SelSide::Left, SelSide::Right] {
                let mut ix = 0;
                while ix < rows.len() {
                    let Some(text) = row_side_text(&rows[ix], side) else {
                        ix += 1;
                        continue;
                    };
                    let mut text = text.to_string();
                    let mut fragments = vec![(ix, 0..text.len())];
                    ix += 1;
                    while ix < rows.len() && matches!(rows[ix], Row::Continuation(_)) {
                        if let Some(part) = row_side_text(&rows[ix], side) {
                            let start = text.len();
                            text.push_str(part);
                            fragments.push((ix, start..text.len()));
                        }
                        ix += 1;
                    }
                    for (start, _) in text.match_indices(query) {
                        let end = start + query.len();
                        occurrences.push(
                            fragments
                                .iter()
                                .filter_map(|(row, fragment)| {
                                    let lo = start.max(fragment.start);
                                    let hi = end.min(fragment.end);
                                    (lo < hi).then_some(Span {
                                        row: *row,
                                        side,
                                        range: lo.saturating_sub(fragment.start)
                                            ..hi.saturating_sub(fragment.start),
                                    })
                                })
                                .collect::<Vec<_>>(),
                        );
                    }
                }
            }
        }
        occurrences.sort_by_key(|spans| {
            let span = &spans[0];
            (span.row, span.side as u8, span.range.start)
        });
        let mut by_row = vec![Vec::new(); rows.len()];
        for (ix, spans) in occurrences.iter().enumerate() {
            for span in spans {
                by_row[span.row].push((ix, span.clone()));
            }
        }
        Self {
            current: (!occurrences.is_empty()).then_some(0),
            occurrences,
            by_row,
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
        self.occurrences.get(self.current?)?.first()
    }
}

pub(crate) struct Search {
    pub(crate) prior: ReviewPosition,
    input: Entity<InputState>,
    query: String,
    matches: Matches,
    _subscription: Subscription,
}

impl Search {
    pub(crate) fn highlights(&self, row: usize, side: SelSide) -> Vec<(Range<usize>, gpui::Rgba)> {
        self.matches
            .by_row
            .get(row)
            .into_iter()
            .flatten()
            .filter(|(_, span)| span.side == side)
            .map(|(ix, span)| {
                let color = if Some(*ix) == self.matches.current {
                    theme::peach()
                } else {
                    theme::yellow()
                };
                (
                    span.range.clone(),
                    gpui::Hsla::from(color).opacity(0.4).into(),
                )
            })
            .collect()
    }
}

impl ItemData {
    /// Every row rebuild (phase 2, comments, wrap, etc.) reindexes the search.
    pub(crate) fn reindex_search(&mut self) {
        if let Some(search) = &mut self.search {
            if self.diff.files.get(self.file_ix).map(|f| f.display_path())
                != Some(search.prior.path.as_str())
            {
                self.search = None;
                return;
            }
            let previous = search.matches.current_span().cloned();
            search.matches = Matches::find(&self.rows, &search.query);
            if let Some(previous) = previous {
                if let Some(ix) = search
                    .matches
                    .occurrences
                    .iter()
                    .position(|spans| spans.first() == Some(&previous))
                {
                    search.matches.current = Some(ix);
                }
            }
        }
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

    pub(crate) fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.search_visible()
            || self.palette.is_some()
            || self.composer.is_some()
            || self.review.is_some()
        {
            return;
        }
        let Some(data) = self.active_data() else {
            return;
        };
        if let Some(search) = &data.search {
            search.input.update(cx, |input, cx| input.focus(window, cx));
            return;
        }
        let Some(mut prior) = data.review_position() else {
            return;
        };
        // Position restoration resolves a source line to its first visual row.
        // Remember which wrapped fragment was at the top, including when a
        // comment immediately beneath that fragment was the viewport anchor.
        let mut base = prior.row.saturating_sub(prior.row_delta);
        while base > 0 && matches!(data.rows.get(base), Some(Row::Continuation(_))) {
            prior.row_delta += 1;
            base -= 1;
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Find in current diff…"));
        let subscription = cx.subscribe_in(&input, window, |this, input, event, window, cx| {
            // An input belonging to a background item must never search another file.
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
                data.search.as_mut().unwrap().query = query;
                data.reindex_search();
                this.reveal_search_match(window, cx);
            }
        });
        let data = self.active_data_mut().unwrap();
        data.search = Some(Search {
            prior,
            input: input.clone(),
            query: String::new(),
            matches: Matches::default(),
            _subscription: subscription,
        });
        // Search includes folded context, without changing the saved preference.
        // The explicit large-file safety banner still requires a click to expand.
        data.rebuild_presentation();
        input.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    pub(crate) fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.search_visible() || self.active_data().and_then(|d| d.search.as_ref()).is_none() {
            return false;
        }
        self.active_data_mut().unwrap().end_search();
        window.focus(&self.focus_handle);
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
            .bg(theme::mantle())
            .border_1()
            .border_color(theme::surface0())
            .on_action(cx.listener(|this, _: &OpenSearch, window, cx| this.open_search(window, cx)))
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
                            .text_color(theme::subtext())
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
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rows::{build_presented_rows, RowOptions, RowsView, ViewMode};

    fn rows(mode: ViewMode, wrap: bool) -> Vec<Row> {
        let diff = diff_core::parse_patch("diff --git a/needle.txt b/needle.txt\n--- a/needle.txt\n+++ b/needle.txt\n@@ -1,3 +1,3 @@\n shared\n-old é世界 old\n+new é世界 new\n tail\ndiff --git a/other b/other\n--- a/other\n+++ b/other\n@@ -1 +1 @@\n-old\n+elsewhere\n");
        build_presented_rows(
            RowsView {
                diff: &diff,
                upgrades: &Default::default(),
                comments: None,
                options: RowOptions {
                    mode,
                    file_ix: 0,
                    comments_visible: true,
                    comment_wrap: 80,
                },
            },
            store::DiffPreferences {
                soft_wrap: Some(wrap),
                ..Default::default()
            },
            &Default::default(),
            6,
        )
        .rows
    }

    #[test]
    fn literal_unicode_matches_cover_both_sides_but_not_headers_or_other_files() {
        for mode in [ViewMode::Unified, ViewMode::Split] {
            let rows = rows(mode, false);
            for query in ["", "needle", "elsewhere", "NEW", ".*"] {
                assert!(
                    Matches::find(&rows, query).occurrences.is_empty(),
                    "{query}"
                );
            }
            let matches = Matches::find(&rows, "é世界");
            assert_eq!(matches.occurrences.len(), 2);
            for occurrence in &matches.occurrences {
                let span = &occurrence[0];
                assert_eq!(
                    &row_side_text(&rows[span.row], span.side).unwrap()[span.range.clone()],
                    "é世界"
                );
            }
            let old = Matches::find(&rows, "old");
            assert_eq!(old.occurrences.len(), 2);
            assert!(old.occurrences.iter().all(|spans| spans[0].side
                == if mode == ViewMode::Unified {
                    SelSide::Unified
                } else {
                    SelSide::Left
                }));
        }
    }

    #[test]
    fn navigation_wraps_both_ends_and_empty_results_are_safe() {
        let mut found = Matches::find(&rows(ViewMode::Unified, false), "old");
        assert_eq!(found.current, Some(0));
        found.step(1);
        assert_eq!(found.current, Some(1));
        found.step(1);
        assert_eq!(found.current, Some(0));
        found.step(-1);
        assert_eq!(found.current, Some(1));
        let mut empty = Matches::find(&[], "old");
        empty.step(1);
        empty.step(-1);
        assert_eq!(empty.current, None);
    }

    #[test]
    fn wrapped_match_is_one_occurrence_with_highlights_on_each_fragment() {
        for mode in [ViewMode::Unified, ViewMode::Split] {
            let rows = rows(mode, true);
            let matches = Matches::find(&rows, "é世界");
            assert_eq!(matches.occurrences.len(), 2);
            for spans in matches.occurrences {
                assert_eq!(spans.len(), 2);
                let text: String = spans
                    .iter()
                    .map(|span| {
                        &row_side_text(&rows[span.row], span.side).unwrap()[span.range.clone()]
                    })
                    .collect();
                assert_eq!(text, "é世界");
            }
            assert!(Matches::find(&rows, "sharedold").occurrences.is_empty());
        }
    }
}
