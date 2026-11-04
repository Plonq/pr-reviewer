//! Pure display decorations and presentation actions. Source hunks and comment
//! coordinates are never mutated by toggles.
use crate::rows::{BuiltRows, Cell, FileUpgrade, LineKind, Row};
use crate::{ItemData, ItemState, ReviewApp, Source};
use gpui::{Context, SharedString};
use std::collections::HashSet;
use std::ops::Range;
use store::DiffPreferences;

pub(crate) const AUTO_COLLAPSE_LINES: usize = 20_000;

pub(crate) fn keybindings() -> [gpui::KeyBinding; 3] {
    [
        gpui::KeyBinding::new("shift-w", crate::ToggleWhitespace, Some("ReviewApp")),
        gpui::KeyBinding::new("shift-k", crate::ToggleUnchanged, Some("ReviewApp")),
        gpui::KeyBinding::new("alt-z", crate::ToggleSoftWrap, Some("ReviewApp")),
    ]
}

pub(crate) fn auto_collapse(lines: usize) -> bool {
    lines >= AUTO_COLLAPSE_LINES
}

pub(crate) fn file_lines(file: &diff_core::FileDiff, upgrade: Option<&FileUpgrade>) -> usize {
    let patch_end = file
        .hunks
        .iter()
        .map(|h| {
            (h.old_start.saturating_add(h.old_count.saturating_sub(1)))
                .max(h.new_start.saturating_add(h.new_count.saturating_sub(1))) as usize
        })
        .max()
        .unwrap_or(0);
    // Complete new-side contents also tell us the complete old-side length:
    // context cancels out of the authoritative hunk count delta. Looking only
    // at hunk end lines misses a large old file with a deletion near its start.
    let full_lines = upgrade
        .map(|u| {
            let new = u.new_lines.len();
            let delta: i64 = file
                .hunks
                .iter()
                .map(|h| i64::from(h.old_count) - i64::from(h.new_count))
                .sum();
            new.max((new as i64 + delta).max(0) as usize)
        })
        .unwrap_or(0);
    patch_end.max(full_lines)
}

fn context_line(row: &Row) -> Option<u32> {
    match row {
        Row::Line {
            kind: LineKind::Context,
            new_no,
            ..
        } => *new_no,
        Row::SplitLine {
            left: Some(l),
            right: Some(r),
        } if l.kind == LineKind::Context && r.kind == LineKind::Context => Some(r.no),
        _ => None,
    }
}

/// Fold long context runs (including whitespace-only changes), preserving
/// three lines at each edge and leaving all comment cards visible.
pub(crate) fn decorate(
    built: BuiltRows,
    prefs: DiffPreferences,
    expanded: &HashSet<(usize, u32)>,
    file: usize,
    cols: usize,
) -> BuiltRows {
    let mut map = vec![0; built.rows.len()];
    let mut rows = Vec::new();
    let mut iter = built.rows.into_iter().enumerate().peekable();
    while let Some((ix, row)) = iter.next() {
        if prefs.collapse_unchanged.unwrap_or(false) && context_line(&row).is_some() {
            let mut run = vec![(ix, row)];
            while iter
                .peek()
                .is_some_and(|(_, row)| context_line(row).is_some())
            {
                run.push(iter.next().unwrap());
            }
            let key = context_line(&run[0].1).unwrap();
            let count = run.len();
            let last = context_line(&run[count - 1].1).unwrap();
            let fold = count > 8
                && !expanded
                    .iter()
                    .any(|(f, line)| *f == file && *line >= key && *line <= last);
            for (j, (ix, row)) in run.into_iter().enumerate() {
                map[ix] = rows.len();
                if fold && (3..count - 3).contains(&j) {
                    if j == 3 {
                        rows.push(Row::Fold {
                            key,
                            count: (count - 6) as u32,
                        });
                    } else {
                        map[ix] = rows.len() - 1;
                    }
                } else {
                    push_wrapped(&mut rows, row, prefs.soft_wrap.unwrap_or(false), cols);
                }
            }
        } else {
            map[ix] = rows.len();
            push_wrapped(&mut rows, row, prefs.soft_wrap.unwrap_or(false), cols);
        }
    }
    let remap = |indices: Vec<usize>| {
        indices
            .into_iter()
            .filter_map(|ix| map.get(ix).copied())
            .collect()
    };
    BuiltRows {
        rows,
        file_rows: remap(built.file_rows),
        hunk_rows: remap(built.hunk_rows),
    }
}

fn chunks(text: &str, cols: usize) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut start = 0;
    for (n, (ix, _)) in text.char_indices().enumerate() {
        if n > 0 && n % cols.max(1) == 0 {
            ranges.push(start..ix);
            start = ix;
        }
    }
    ranges.push(start..text.len());
    ranges
}

fn clipped<T: Clone>(spans: &[(Range<usize>, T)], range: &Range<usize>) -> Vec<(Range<usize>, T)> {
    spans
        .iter()
        .filter_map(|(span, value)| {
            let lo = span.start.max(range.start);
            let hi = span.end.min(range.end);
            (lo < hi).then(|| ((lo - range.start)..(hi - range.start), value.clone()))
        })
        .collect()
}

fn intra(spans: &[Range<usize>], range: &Range<usize>) -> Vec<Range<usize>> {
    spans
        .iter()
        .filter_map(|span| {
            let lo = span.start.max(range.start);
            let hi = span.end.min(range.end);
            (lo < hi).then(|| (lo - range.start)..(hi - range.start))
        })
        .collect()
}

fn push_wrapped(out: &mut Vec<Row>, row: Row, wrap: bool, cols: usize) {
    if !wrap {
        out.push(row);
        return;
    }
    match row {
        Row::Line {
            old_no,
            new_no,
            kind,
            text,
            intra: spans,
            syntax,
        } => {
            for (ix, range) in chunks(&text, cols).into_iter().enumerate() {
                let row = Row::Line {
                    old_no,
                    new_no,
                    kind,
                    text: SharedString::from(text[range.clone()].to_string()),
                    intra: intra(&spans, &range),
                    syntax: clipped(&syntax, &range),
                };
                out.push(if ix == 0 {
                    row
                } else {
                    Row::Continuation(Box::new(row))
                });
            }
        }
        Row::SplitLine { left, right } => {
            let split = |cell: Option<Cell>| -> Vec<Cell> {
                cell.map(|cell| {
                    chunks(&cell.text, cols)
                        .into_iter()
                        .map(|range| Cell {
                            no: cell.no,
                            kind: cell.kind,
                            text: SharedString::from(cell.text[range.clone()].to_string()),
                            intra: intra(&cell.intra, &range),
                            syntax: clipped(&cell.syntax, &range),
                        })
                        .collect()
                })
                .unwrap_or_default()
            };
            let mut left = split(left).into_iter();
            let mut right = split(right).into_iter();
            let mut first = true;
            loop {
                let (left, right) = (left.next(), right.next());
                if left.is_none() && right.is_none() {
                    break;
                }
                let row = Row::SplitLine { left, right };
                out.push(if first {
                    row
                } else {
                    Row::Continuation(Box::new(row))
                });
                first = false;
            }
        }
        row => out.push(row),
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Preference {
    Whitespace,
    Collapse,
    Wrap,
}

impl ItemData {
    pub(crate) fn split_layout(&self, char_width: gpui::Pixels) -> crate::rows::SplitLayout {
        let state = self.scroll.0.borrow();
        let viewport = state
            .last_item_size
            .map(|size| size.item.width)
            .unwrap_or_else(|| state.base_handle.bounds().size.width);
        let half = ((f32::from(viewport) - crate::SPLIT_DIVIDER) / 2.).max(0.);
        let content = self.split_columns as f32 * f32::from(char_width) + crate::SPLIT_GUTTER;
        crate::rows::SplitLayout {
            viewport,
            scroll_width: viewport + gpui::px((content - half).max(0.)),
            offset: state.base_handle.offset().x,
        }
    }

    pub(crate) fn load_preferences(
        &mut self,
        store: &crate::local_store::LocalStore,
        loc: Option<&github::PrLocator>,
    ) {
        self.preference_overrides = store.diff_preferences(loc);
        // New PRs always start with the three decorations off. Legacy global
        // preferences (also used by local diffs) must not leak into a new PR.
        let defaults = if loc.is_some() {
            DiffPreferences::OFF
        } else {
            store.diff_preferences(None).over(DiffPreferences::OFF)
        };
        self.preferences = self.preference_overrides.over(defaults);
        self.rebuild_presentation();
    }

    pub(crate) fn rebuild_presentation(&mut self) {
        let position = self.review_position();
        self.set_rows(self.row_build());
        self.selection = None;
        self.cursor_side = None;
        if let Some(mut position) = position {
            if self.preferences.soft_wrap.unwrap_or(false) {
                position.horizontal_offset = 0.;
            }
            self.restore_review_position(position, false);
        }
    }
}

impl ReviewApp {
    pub(crate) fn toggle_presentation(&mut self, pref: Preference, cx: &mut Context<Self>) {
        let Some(item) = self.items.get_mut(self.active) else {
            return;
        };
        let ItemState::Ready(data) = &mut item.state else {
            return;
        };
        let value = match pref {
            Preference::Whitespace => &mut data.preferences.hide_whitespace,
            Preference::Collapse => &mut data.preferences.collapse_unchanged,
            Preference::Wrap => &mut data.preferences.soft_wrap,
        };
        *value = Some(!value.unwrap_or(false));
        match pref {
            Preference::Whitespace => data.preference_overrides.hide_whitespace = *value,
            Preference::Collapse => data.preference_overrides.collapse_unchanged = *value,
            Preference::Wrap => data.preference_overrides.soft_wrap = *value,
        }
        if matches!(pref, Preference::Collapse) {
            data.expanded_regions.retain(|(_, key)| *key == 0);
        }
        data.rebuild_presentation();
        let loc = match &item.source {
            Source::Pr(loc) => Some(loc),
            _ => None,
        };
        self.store
            .save_diff_preferences(loc, data.preference_overrides);
        cx.notify();
    }

    pub(crate) fn expand_region(&mut self, key: u32, cx: &mut Context<Self>) {
        let Some(data) = self.active_data_mut() else {
            return;
        };
        let region = (data.file_ix, key);
        if !data.expanded_regions.remove(&region) {
            data.expanded_regions.insert(region);
        }
        data.rebuild_presentation();
        cx.notify();
    }

    pub(crate) fn render_presentation_controls(
        &self,
        data: &ItemData,
        cx: &mut Context<Self>,
    ) -> impl gpui::IntoElement {
        use gpui::component::{button::Button, Selectable as _, Sizable as _};
        use gpui::{div, prelude::*, px};
        use gpui_kit_assets::IconName;
        let toggle = |id, name, shortcut, enabled| {
            Button::new(id)
                .small()
                .label(name)
                .icon(if enabled {
                    IconName::SquareCheck
                } else {
                    IconName::Square
                })
                .selected(enabled)
                .toggled(enabled)
                .tooltip(format!("{name} ({shortcut})"))
        };
        div()
            .h(px(36.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .bg(crate::theme::mantle(cx))
            .border_b_1()
            .border_color(crate::theme::surface0(cx))
            .child(
                toggle(
                    "hide-whitespace",
                    "Hide whitespace",
                    "W",
                    data.preferences.hide_whitespace.unwrap_or(false),
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.toggle_presentation(Preference::Whitespace, cx)
                })),
            )
            .child(
                toggle(
                    "collapse-unchanged",
                    "Collapse",
                    "K",
                    data.preferences.collapse_unchanged.unwrap_or(false),
                )
                .on_click(
                    cx.listener(|this, _, _, cx| {
                        this.toggle_presentation(Preference::Collapse, cx)
                    }),
                ),
            )
            .child(
                toggle(
                    "soft-wrap",
                    "Wrap",
                    "⌥Z",
                    data.preferences.soft_wrap.unwrap_or(false),
                )
                .on_click(
                    cx.listener(|this, _, _, cx| this.toggle_presentation(Preference::Wrap, cx)),
                ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rows::{build_presented_rows, comment_anchor, RowOptions, RowsView, ViewMode};
    use crate::selection::{selection_text, RowCol, SelSide, Selection};
    use crate::threads::{group_comments, CommentSide};
    use diff_core::{diff_texts, FileDiff, FileStatus, PrDiff};
    use std::collections::HashMap;

    fn fixture(n: usize) -> (PrDiff, HashMap<usize, FileUpgrade>) {
        let old: String = (1..=n).map(|n| format!("line {n}\n")).collect();
        let new = old.replace("line 10\n", "changed 10\n");
        let file = FileDiff {
            old_path: Some("a.txt".into()),
            new_path: Some("a.txt".into()),
            status: FileStatus::Modified,
            hunks: diff_texts(&old, &new, 3),
            additions: 1,
            deletions: 1,
        };
        let upgrade = FileUpgrade {
            new_lines: new.lines().map(|s| s.to_string().into()).collect(),
            old_spans: vec![],
            new_spans: vec![],
        };
        (PrDiff { files: vec![file] }, HashMap::from([(0, upgrade)]))
    }

    fn build(
        diff: &PrDiff,
        upgrades: &HashMap<usize, FileUpgrade>,
        prefs: DiffPreferences,
        expanded: &HashSet<(usize, u32)>,
        mode: ViewMode,
    ) -> BuiltRows {
        build_presented_rows(
            RowsView {
                diff,
                upgrades,
                comments: None,
                options: RowOptions {
                    mode,
                    file_ix: 0,
                    comments_visible: false,
                    comment_wrap: 80,
                },
            },
            prefs,
            expanded,
            8,
        )
    }

    #[test]
    fn exact_monster_threshold_collapses_before_display_work_in_both_modes() {
        assert!(!auto_collapse(AUTO_COLLAPSE_LINES - 1));
        assert!(auto_collapse(AUTO_COLLAPSE_LINES));
        assert!(auto_collapse(AUTO_COLLAPSE_LINES + 1));
        for n in [19_999, 20_000, 20_001] {
            let (diff, upgrades) = fixture(n);
            for mode in [ViewMode::Unified, ViewMode::Split] {
                let started = std::time::Instant::now();
                let built = build(
                    &diff,
                    &upgrades,
                    DiffPreferences::default(),
                    &HashSet::new(),
                    mode,
                );
                if n < AUTO_COLLAPSE_LINES {
                    assert!(built.rows.len() >= n);
                    assert!(!built
                        .rows
                        .iter()
                        .any(|row| matches!(row, Row::Fold { .. } | Row::LargeFile { .. })));
                } else {
                    assert_eq!(built.rows.len(), 2);
                    assert!(built.hunk_rows.is_empty());
                    assert!(
                        matches!(built.rows[1], Row::LargeFile { lines, expanded: false } if lines == n)
                    );
                    assert!(started.elapsed() < std::time::Duration::from_secs(1));
                }
            }
        }
    }

    #[test]
    fn patch_only_added_deleted_and_high_line_numbers_get_the_safety_banner() {
        for (old, new) in [
            ("x\n".repeat(20_000), String::new()),
            (String::new(), "x\n".repeat(20_000)),
        ] {
            let file = FileDiff {
                old_path: Some("a".into()),
                new_path: Some("a".into()),
                status: if new.is_empty() {
                    FileStatus::Deleted
                } else {
                    FileStatus::Added
                },
                hunks: diff_texts(&old, &new, 3),
                additions: 0,
                deletions: 0,
            };
            let diff = PrDiff { files: vec![file] };
            assert_eq!(
                build(
                    &diff,
                    &HashMap::new(),
                    DiffPreferences::default(),
                    &HashSet::new(),
                    ViewMode::Unified
                )
                .rows
                .len(),
                2
            );
        }
        let diff = diff_core::parse_patch(
            "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -21000 +21000 @@\n-old\n+new\n",
        );
        assert_eq!(
            build(
                &diff,
                &HashMap::new(),
                DiffPreferences::default(),
                &HashSet::new(),
                ViewMode::Unified
            )
            .rows
            .len(),
            2
        );
    }

    #[test]
    fn monster_threshold_uses_both_complete_sides_not_just_hunk_ends() {
        let old = "same\n".repeat(20_000);
        let new = "same\n".repeat(19_990);
        let diff = PrDiff {
            files: vec![FileDiff {
                old_path: Some("a".into()),
                new_path: Some("a".into()),
                status: FileStatus::Modified,
                hunks: diff_texts(&old, &new, 3),
                additions: 0,
                deletions: 10,
            }],
        };
        let upgrades = HashMap::from([(
            0,
            FileUpgrade {
                new_lines: new.lines().map(|s| s.to_string().into()).collect(),
                old_spans: vec![],
                new_spans: vec![],
            },
        )]);
        assert_eq!(file_lines(&diff.files[0], upgrades.get(&0)), 20_000);
        assert_eq!(
            build(
                &diff,
                &upgrades,
                DiffPreferences::default(),
                &HashSet::new(),
                ViewMode::Unified
            )
            .rows
            .len(),
            2
        );
    }

    #[test]
    fn monster_expansion_is_explicit_and_can_keep_unchanged_context_folded() {
        let (diff, upgrades) = fixture(20_000);
        let prefs = DiffPreferences {
            collapse_unchanged: Some(true),
            ..Default::default()
        };
        let built = build(
            &diff,
            &upgrades,
            prefs,
            &HashSet::from([(0, 0)]),
            ViewMode::Unified,
        );
        assert!(matches!(
            built.rows[1],
            Row::LargeFile { expanded: true, .. }
        ));
        assert!(!built.hunk_rows.is_empty());
        assert!(built.rows.len() < 25);
        assert!(built
            .rows
            .iter()
            .any(|row| matches!(row, Row::Fold { count, .. } if *count > 19_000)));
    }

    #[test]
    fn context_collapse_is_opt_in_and_gap_click_actually_expands_it() {
        let (diff, upgrades) = fixture(100);
        for mode in [ViewMode::Unified, ViewMode::Split] {
            let plain = build(
                &diff,
                &upgrades,
                DiffPreferences::default(),
                &HashSet::new(),
                mode,
            );
            assert!(!plain.rows.iter().any(|r| matches!(r, Row::Fold { .. })));
            let prefs = DiffPreferences {
                collapse_unchanged: Some(true),
                ..Default::default()
            };
            let collapsed = build(&diff, &upgrades, prefs, &HashSet::new(), mode);
            assert!(collapsed.rows.len() < plain.rows.len());
            let expanded: HashSet<_> = collapsed
                .rows
                .iter()
                .filter_map(|r| match r {
                    Row::Fold { key, .. } => Some((0, *key)),
                    _ => None,
                })
                .collect();
            let opened = build(&diff, &upgrades, prefs, &expanded, mode);
            assert_eq!(opened.rows.len(), plain.rows.len());
            assert!(collapsed
                .hunk_rows
                .iter()
                .all(|ix| *ix < collapsed.rows.len()));
        }
    }

    #[test]
    fn folding_keeps_threads_and_their_source_anchors_visible() {
        let (diff, upgrades) = fixture(100);
        let comments = group_comments(vec![crate::test_util::app_rc(
            1,
            "a.txt",
            Some("RIGHT"),
            Some(65),
            "keep me",
            "author",
            "2026-01-01T00:00:00Z",
            None,
        )]);
        let built = build_presented_rows(
            RowsView {
                diff: &diff,
                upgrades: &upgrades,
                comments: Some(&comments),
                options: RowOptions {
                    mode: ViewMode::Unified,
                    file_ix: 0,
                    comments_visible: true,
                    comment_wrap: 80,
                },
            },
            DiffPreferences {
                collapse_unchanged: Some(true),
                ..Default::default()
            },
            &HashSet::new(),
            80,
        );
        let ix = built
            .rows
            .iter()
            .position(|r| matches!(r, Row::CommentHeader { .. }))
            .unwrap();
        assert_eq!(
            comment_anchor(&built.rows, ix - 1, SelSide::Unified),
            Some((CommentSide::Right, 65))
        );
        assert!(built.rows.len() < 35);
    }

    #[test]
    fn wrap_is_opt_in_unicode_safe_and_copy_and_anchors_stay_in_source_space() {
        let text = "abcé世界123456789";
        let diff = diff_core::parse_patch(&format!(
            "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-old\n+{text}\n"
        ));
        for mode in [ViewMode::Unified, ViewMode::Split] {
            let plain = build(
                &diff,
                &HashMap::new(),
                DiffPreferences::default(),
                &HashSet::new(),
                mode,
            );
            assert!(!plain.rows.iter().any(|r| matches!(r, Row::Continuation(_))));
            let wrapped = build(
                &diff,
                &HashMap::new(),
                DiffPreferences {
                    soft_wrap: Some(true),
                    ..Default::default()
                },
                &HashSet::new(),
                mode,
            );
            let side = if mode == ViewMode::Unified {
                SelSide::Unified
            } else {
                SelSide::Right
            };
            let ix = wrapped
                .rows
                .iter()
                .position(|r| {
                    crate::selection::row_side_text(r, side).is_some_and(|s| s.starts_with("abcé"))
                })
                .unwrap();
            assert!(wrapped
                .rows
                .iter()
                .any(|r| matches!(r, Row::Continuation(_))));
            assert_eq!(
                comment_anchor(&wrapped.rows, ix + 1, side),
                Some((CommentSide::Right, 1))
            );
            let selected = Selection {
                side,
                anchor: RowCol { row: ix, col: 0 },
                head: RowCol {
                    row: wrapped.rows.len() - 1,
                    col: usize::MAX,
                },
            };
            assert_eq!(selection_text(&selected, &wrapped.rows), text);
            assert!(wrapped.hunk_rows.iter().all(|ix| *ix < wrapped.rows.len()));
        }
    }
}
