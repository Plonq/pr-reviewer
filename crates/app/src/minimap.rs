//! The diff-pane minimap: display rows reduced to bars, downsampled into coalesced paint runs.

use crate::rows::{Cell, LineKind, Row};
use crate::selection::SelSide;
use crate::threads::CommentSide;

/// Width of the minimap column on the right edge of the diff pane.
pub(crate) const MINIMAP_WIDTH: f32 = 100.0;
/// Horizontal inset of the bars inside the column.
pub(crate) const MINIMAP_PAD: f32 = 4.0;
/// Gap between the two half-columns mirroring split view.
pub(crate) const MINIMAP_GAP: f32 = 2.0;
/// A line this long (or longer) draws a full-width bar.
const MAX_MINIMAP_CHARS: usize = 160;

/// One display row reduced to what the minimap paints: a kind (color) and a
/// width fraction. Index-aligned with `ItemData::rows`, recomputed wherever
/// the rows are rebuilt.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct MinimapRow {
    pub(crate) kind: MinimapKind,
    /// Line length / MAX_MINIMAP_CHARS, capped at 1. For SplitPair rows this
    /// is the max of the two halves (used by downsample grouping).
    pub(crate) len_frac: f32,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum MinimapKind {
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
    /// A comment card fills its lane, independent of the comment's text length.
    Comment {
        lane: MinimapLane,
    },
    Blank,
}

/// Fraction of a full-width minimap bar for one line of text. Counting stops
/// at the cap, so pathological lines cost nothing extra.
fn line_frac(text: &str) -> f32 {
    text.chars().take(MAX_MINIMAP_CHARS).count() as f32 / MAX_MINIMAP_CHARS as f32
}

/// A match projected onto the minimap's character scale, shared by Find and
/// passive selection matching.
#[derive(Debug, PartialEq)]
pub(crate) struct MinimapMatch {
    pub(crate) row: usize,
    pub(crate) side: SelSide,
    pub(crate) range: std::ops::Range<f32>,
}

/// Project a source byte range onto the same character scale as the text bars.
/// The minimap truncates at MAX_MINIMAP_CHARS; do not highlight outside it.
pub(crate) fn text_range_frac(
    text: &str,
    range: &std::ops::Range<usize>,
) -> Option<std::ops::Range<f32>> {
    let start = text[..range.start].chars().take(MAX_MINIMAP_CHARS).count();
    let end = start
        + text[range.clone()]
            .chars()
            .take(MAX_MINIMAP_CHARS - start)
            .count();
    (start < end)
        .then(|| start as f32 / MAX_MINIMAP_CHARS as f32..end as f32 / MAX_MINIMAP_CHARS as f32)
}

/// Reduce display rows to minimap rows, one per row, index-aligned.
pub(crate) fn minimap_rows(rows: &[Row]) -> Vec<MinimapRow> {
    rows.iter()
        .map(|row| match crate::rows::source_row(row) {
            Row::Binary | Row::Continuation(_) | Row::Fold { .. } | Row::LargeFile { .. } => {
                MinimapRow {
                    kind: MinimapKind::Blank,
                    len_frac: 0.,
                }
            }
            Row::CommentHeader { half, .. }
            | Row::CommentEditor { half, .. }
            | Row::CommentReactions { half, .. }
            | Row::CommentBody { half, .. }
            | Row::CommentActions { half, .. } => MinimapRow {
                kind: MinimapKind::Comment {
                    lane: match half {
                        None => MinimapLane::Full,
                        Some(CommentSide::Left) => MinimapLane::Left,
                        Some(CommentSide::Right) => MinimapLane::Right,
                    },
                },
                len_frac: 1.,
            },
            Row::FileHeader { .. } => MinimapRow {
                kind: MinimapKind::Header,
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
pub(crate) enum MinimapLane {
    /// Bar from the left edge across the full usable width (unified rows and
    /// header ticks and unified comment cards).
    Full,
    /// Left half-column (split view's left cell).
    Left,
    /// Right half-column (split view's right cell).
    Right,
}

/// Bar colors as a plain enum so the precompute stays theme-free and testable;
/// painting maps them to theme colors.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MinimapColor {
    Added,
    Removed,
    Context,
    Header,
    Comment,
}

/// Downsample priority: when several rows share one pixel-row, the highest
/// wins (Blank rows contribute nothing and lose to everything).
fn minimap_priority(color: MinimapColor) -> u8 {
    match color {
        MinimapColor::Comment => 6,
        MinimapColor::Removed => 5,
        MinimapColor::Added => 4,
        MinimapColor::Header => 3,
        MinimapColor::Context => 1,
    }
}

/// A coalesced vertical run of identical bars, in slot space: a slot is one
/// painted row of the minimap, `slot_h` px tall, covering `group` display
/// rows. `tick` runs paint 1px tall at the slot top instead of filling it.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct MinimapRun {
    pub(crate) start: u32,
    /// Exclusive.
    pub(crate) end: u32,
    pub(crate) lane: MinimapLane,
    pub(crate) color: MinimapColor,
    pub(crate) frac: f32,
    pub(crate) tick: bool,
}

pub(crate) struct MinimapLayout {
    /// Painted height of one slot: clamp(pane_height / total_rows, 1, 3),
    /// including any blank tail rows.
    pub(crate) slot_h: f32,
    /// Display rows per slot; 1 unless the diff is taller than the pane at
    /// 1px per row, then ceil(total / pane_px).
    pub(crate) group: usize,
    pub(crate) runs: Vec<MinimapRun>,
}

pub(crate) fn minimap_scale(total: usize, pane_px: f32) -> (f32, usize) {
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
/// rebuilt, the pane height changes, or the blank tail changes — painting
/// just iterates the runs.
pub(crate) fn minimap_runs(rows: &[MinimapRow], pane_px: f32, tail_rows: usize) -> MinimapLayout {
    // Blank scroll-past-end/composer slots affect the scale but paint no bars.
    let (slot_h, group) = minimap_scale(rows.len() + tail_rows, pane_px);
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
                MinimapKind::Comment { lane } => fold(lane, MinimapColor::Comment, 1.),
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
        // A full-width card must not be overpainted by split code sharing
        // its downsampled slot. Half-width cards already paint above it.
        if winner[MinimapLane::Full as usize] == Some(MinimapColor::Comment) {
            winner[MinimapLane::Left as usize] = None;
            winner[MinimapLane::Right as usize] = None;
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
            let tick = matches!(color, MinimapColor::Header) && slot_h > 1.;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rows::{build_rows, BuiltRows, RowOptions, RowsView, ViewMode};
    use crate::test_util::{mrow, sample_comments, sample_diff};
    use crate::threads::COMMENT_WRAP_CHARS;
    use std::collections::HashMap;

    #[test]
    fn match_ranges_use_the_text_bar_character_scale() {
        assert_eq!(
            text_range_frac("    word tail", &(4..8)),
            Some((4. / 160.)..(8. / 160.))
        );
        // Byte offsets must not shift or widen Unicode matches.
        assert_eq!(
            text_range_frac("é word é", &(3..7)),
            Some((2. / 160.)..(6. / 160.))
        );
        assert_eq!(
            text_range_frac("word", &(0..4)).unwrap().end,
            line_frac("word")
        );
        assert_eq!(text_range_frac("word", &(0..0)), None);
    }

    #[test]
    fn match_ranges_clip_at_the_same_column_as_long_text_bars() {
        let text = "é".repeat(200);
        assert_eq!(text_range_frac(&text, &(316..326)), Some((158. / 160.)..1.));
        assert_eq!(text_range_frac(&text, &(320..326)), None);
    }

    #[test]
    fn minimap_rows_unified_kinds_and_fracs() {
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
        let mm = minimap_rows(&rows);
        assert_eq!(mm.len(), rows.len());
        // The file header maps to a Header tick; the hunk's first row
        // follows directly — no `@@` marker rows anymore.
        assert_eq!(mm[0], mrow(MinimapKind::Header, 1.));
        // Line rows: kind from the row, frac = chars / MAX_MINIMAP_CHARS.
        assert_eq!(mm[1], mrow(MinimapKind::Context, 3. / 160.)); // "ctx"
        assert_eq!(mm[2], mrow(MinimapKind::Removed, 4. / 160.)); // "old1"
        assert_eq!(mm[4], mrow(MinimapKind::Added, 4. / 160.)); // "new1"
    }

    #[test]
    fn minimap_rows_split_pairs() {
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
        let mm = minimap_rows(&rows);
        assert_eq!(mm.len(), rows.len());
        // Context pair: both halves, no change flags.
        assert_eq!(
            mm[1].kind,
            MinimapKind::SplitPair {
                left_frac: 3. / 160.,
                right_frac: 3. / 160.,
                left: false,
                right: false,
            }
        );
        // Paired removed/added ("old1" / "new1").
        assert_eq!(
            mm[2].kind,
            MinimapKind::SplitPair {
                left_frac: 4. / 160.,
                right_frac: 4. / 160.,
                left: true,
                right: true,
            }
        );
        assert_eq!(mm[2].len_frac, 4. / 160.);
        // One-sided rows: the absent half has a zero fraction and no flag.
        let h2 = 5; // the second hunk's first row
        assert_eq!(
            mm[h2 + 1].kind,
            MinimapKind::SplitPair {
                left_frac: 2. / 160., // "r2"
                right_frac: 0.,
                left: true,
                right: false,
            }
        );
        assert_eq!(
            mm[h2 + 3].kind,
            MinimapKind::SplitPair {
                left_frac: 0.,
                right_frac: 4. / 160., // "lone"
                left: false,
                right: true,
            }
        );
    }

    #[test]
    fn minimap_comment_rows_fill_their_card_lane() {
        let index = sample_comments();
        for mode in [ViewMode::Unified, ViewMode::Split] {
            let BuiltRows { mut rows, .. } = build_rows(RowsView {
                diff: &sample_diff(),
                upgrades: &HashMap::new(),
                comments: Some(&index),
                options: RowOptions {
                    mode,
                    file_ix: 0,
                    comments_visible: true,
                    comment_wrap: COMMENT_WRAP_CHARS,
                },
            });
            // Also cover in-place editors and collapsed headers, which can
            // occupy just one row and still need a visible marker.
            rows.extend([
                Row::CommentEditor { half: None, top: true },
                Row::CommentEditor {
                    half: Some(CommentSide::Left),
                    top: true,
                },
                Row::CommentEditor {
                    half: Some(CommentSide::Right),
                    top: true,
                },
                Row::CommentHeader {
                    comment_id: crate::comments::CommentKey::Inline(99),
                    author: "alice".into(),
                    when: "now".into(),
                    timestamp: None,
                    is_reply: false,
                    half: None,
                    top: true,
                    collapsed: true,
                },
            ]);
            let mm = minimap_rows(&rows);
            assert_eq!(mm.len(), rows.len());
            let mut count = 0;
            for (row, reduced) in rows.iter().zip(&mm) {
                let half = match row {
                    Row::CommentHeader { half, .. }
                    | Row::CommentBody { half, .. }
                    | Row::CommentActions { half, .. }
                    | Row::CommentEditor { half, .. } => half,
                    _ => continue,
                };
                let lane = match half {
                    None => MinimapLane::Full,
                    Some(CommentSide::Left) => MinimapLane::Left,
                    Some(CommentSide::Right) => MinimapLane::Right,
                };
                assert_eq!(*reduced, mrow(MinimapKind::Comment { lane }, 1.));
                count += 1;
            }
            assert!(count > 4, "fixture should include a thread and reply");
        }
    }

    #[test]
    fn minimap_comment_cards_coalesce_into_rectangles() {
        for lane in [MinimapLane::Full, MinimapLane::Left, MinimapLane::Right] {
            let rows = vec![
                mrow(MinimapKind::Comment { lane }, 1.),
                mrow(MinimapKind::Comment { lane }, 1.),
                mrow(MinimapKind::Comment { lane }, 1.),
                mrow(MinimapKind::Blank, 0.),
                mrow(MinimapKind::Comment { lane }, 1.),
            ];
            let layout = minimap_runs(&rows, 100., 0);
            assert_eq!(layout.slot_h, 3.);
            let card = |start, end| MinimapRun {
                start,
                end,
                lane,
                color: MinimapColor::Comment,
                frac: 1.,
                tick: false,
            };
            assert_eq!(layout.runs, vec![card(0, 3), card(4, 5)]);
        }
    }

    #[test]
    fn minimap_comments_survive_downsampling_with_changes() {
        for lane in [MinimapLane::Full, MinimapLane::Left, MinimapLane::Right] {
            let rows = vec![
                mrow(MinimapKind::Header, 1.),
                mrow(MinimapKind::Removed, 0.5),
                mrow(MinimapKind::Added, 0.5),
                mrow(MinimapKind::Context, 0.5),
                mrow(
                    MinimapKind::SplitPair {
                        left_frac: 0.5,
                        right_frac: 0.5,
                        left: true,
                        right: true,
                    },
                    0.5,
                ),
                mrow(MinimapKind::Comment { lane }, 1.),
            ];
            let layout = minimap_runs(&rows, 1., 0);
            assert_eq!((layout.slot_h, layout.group), (1., 6));
            assert!(layout.runs.contains(&MinimapRun {
                start: 0,
                end: 1,
                lane,
                color: MinimapColor::Comment,
                frac: 1.,
                tick: false,
            }));
            if lane == MinimapLane::Full {
                assert_eq!(layout.runs.len(), 1, "split code must not obscure the card");
            }
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
    fn minimap_blank_tail_scales_without_painting_extra_bars() {
        let rows = vec![mrow(MinimapKind::Added, 0.5); 80];
        let tail_rows = 40;
        let mut padded = rows.clone();
        padded.extend(vec![mrow(MinimapKind::Blank, 0.); tail_rows]);
        // Exercise both fractional slots and a tail that forces downsampling.
        for pane_px in [300., 100.] {
            let layout = minimap_runs(&rows, pane_px, tail_rows);
            let explicit = minimap_runs(&padded, pane_px, 0);
            assert_eq!(
                (layout.slot_h, layout.group),
                minimap_scale(rows.len() + tail_rows, pane_px)
            );
            assert_eq!(
                (layout.slot_h, layout.group),
                (explicit.slot_h, explicit.group)
            );
            assert_eq!(layout.runs, explicit.runs);
            assert_eq!(layout.runs.len(), 1);
            assert_eq!(
                layout.runs[0].end as usize,
                rows.len().div_ceil(layout.group)
            );
        }
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
        let layout = minimap_runs(&rows, 100., 0);
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
        let layout = minimap_runs(&rows, 100., 0);
        assert_eq!(layout.slot_h, 3.);
        assert_eq!(layout.runs.len(), 3);
        assert!(layout.runs[0].tick && layout.runs[1].tick);
        assert_eq!((layout.runs[0].start, layout.runs[0].end), (0, 1));
        assert_eq!((layout.runs[1].start, layout.runs[1].end), (1, 2));
        // 1px slots: ticks are plain bars and coalesce.
        let layout = minimap_runs(&rows, 3., 0);
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
        let layout = minimap_runs(&rows, 10., 0);
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
        let layout = minimap_runs(&rows, 90., 0);
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
}
