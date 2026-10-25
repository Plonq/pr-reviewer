//! Per-PR viewport memory. Persist source coordinates, not transient row indices:
//! full-file upgrades, split view, and inline comments all change display rows.

use crate::local_store::ReviewPosition;
use crate::rows::{Row, ViewMode};
use crate::{row_height, ItemData, ItemState, ReviewApp, Source};
use gpui::{point, px, Context, Pixels, Point};
use std::time::Duration;

pub(crate) struct PositionRestore {
    position: ReviewPosition,
    offset: Point<Pixels>,
}

fn line_numbers(row: &Row) -> (Option<u32>, Option<u32>) {
    match crate::rows::source_row(row) {
        Row::Line { old_no, new_no, .. } => (*old_no, *new_no),
        Row::SplitLine { left, right } => (
            left.as_ref().map(|cell| cell.no),
            right.as_ref().map(|cell| cell.no),
        ),
        _ => (None, None),
    }
}

fn anchor_row(rows: &[Row], position: &ReviewPosition) -> usize {
    let nearest = rows
        .iter()
        .enumerate()
        .filter_map(|(ix, row)| {
            let (old, new) = line_numbers(row);
            let distance = if let Some(line) = position.new_line {
                new?.abs_diff(line)
            } else {
                old?.abs_diff(position.old_line?)
            };
            Some((distance, ix))
        })
        .min();
    nearest
        .map(|(_, ix)| ix.saturating_add(position.row_delta))
        .unwrap_or(position.row)
        .min(rows.len().saturating_sub(1))
}

impl ItemData {
    pub(crate) fn rescale_position_restore(&mut self, ratio: f32) {
        if let Some(restore) = &mut self.position_restore {
            restore.offset.y *= ratio;
        }
    }

    pub(crate) fn remember_jump(&mut self) {
        self.position_restore = None;
        if let Some(position) = self.review_position() {
            let offset = point(
                px(-position.horizontal_offset),
                px(-(position.row as f32 + position.fraction) * row_height()),
            );
            self.position_restore = Some(PositionRestore { position, offset });
        }
    }

    /// A deferred keyboard jump is already the new position, even before gpui
    /// has painted it. Otherwise use the actual viewport (wheel/scrollbar/map).
    pub(crate) fn review_position(&self) -> Option<ReviewPosition> {
        let path = self
            .diff
            .files
            .get(self.file_ix)?
            .display_path()
            .to_string();
        let state = self.scroll.0.borrow();
        let offset = state.base_handle.offset();
        if state.deferred_scroll_to_item.is_none() {
            if let Some(restore) = &self.position_restore {
                let max = state.base_handle.max_offset();
                let clamped = point(
                    restore.offset.x.max(-max.width).min(px(0.)),
                    restore.offset.y.max(-max.height).min(px(0.)),
                );
                // A patch may not contain this line yet, or may be too short
                // to put it at the top. Keep the requested anchor until phase 2,
                // unless the user has moved the viewport in the meantime.
                if offset == restore.offset || offset == clamped {
                    return Some(restore.position.clone());
                }
            }
        }
        let top = state
            .deferred_scroll_to_item
            .as_ref()
            .map(|jump| jump.item_index as f32)
            .unwrap_or_else(|| f32::from(-offset.y).max(0.) / row_height());
        let row = (top.floor() as usize).min(self.rows.len().saturating_sub(1));
        let (base, old_line, new_line) = (0..=row)
            .rev()
            .find_map(|ix| {
                let (old, new) = line_numbers(self.rows.get(ix)?);
                (old.is_some() || new.is_some()).then_some((ix, old, new))
            })
            .unwrap_or((row, None, None));
        Some(ReviewPosition {
            path,
            old_line,
            new_line,
            row,
            row_delta: row - base,
            fraction: top.fract(),
            horizontal_offset: f32::from(-offset.x).max(0.),
            split: self.mode == ViewMode::Split,
        })
    }

    /// Returns false if the saved file is no longer changed. The caller keeps
    /// the normal first-file/first-hunk reveal in that case.
    pub(crate) fn restore_review_position(
        &mut self,
        position: ReviewPosition,
        wait_for_upgrade: bool,
    ) -> bool {
        let Some(file_ix) = self
            .diff
            .files
            .iter()
            .position(|file| file.display_path() == position.path)
        else {
            return false;
        };
        let mode = if position.split {
            ViewMode::Split
        } else {
            ViewMode::Unified
        };
        if self.file_ix != file_ix || self.mode != mode {
            self.file_ix = file_ix;
            self.mode = mode;
            self.set_rows(self.row_build());
        }
        let row = anchor_row(&self.rows, &position);
        let fraction = if position.fraction.is_finite() {
            position.fraction.clamp(0., 0.999_999)
        } else {
            0.
        };
        let horizontal = if position.horizontal_offset.is_finite() {
            position.horizontal_offset.max(0.)
        } else {
            0.
        };
        let offset = point(px(-horizontal), px(-(row as f32 + fraction) * row_height()));
        let mut state = self.scroll.0.borrow_mut();
        // In particular, cancel ItemData::new's first-hunk reveal.
        state.deferred_scroll_to_item = None;
        state.base_handle.set_offset(offset);
        self.cursor = row;
        self.cursor_side = None;
        self.selection = None;
        self.position_restore = wait_for_upgrade.then_some(PositionRestore { position, offset });
        true
    }
}

impl ReviewApp {
    pub(crate) fn persist_positions(&mut self) {
        for item in &mut self.items {
            let (Source::Pr(loc), ItemState::Ready(data)) = (&item.source, &mut item.state) else {
                continue;
            };
            let Some(position) = data.review_position() else {
                continue;
            };
            if data.saved_position.as_ref() != Some(&position)
                && self.store.save_position(loc, &position)
            {
                data.saved_position = Some(position);
            }
        }
    }

    /// Scroll handles update without re-rendering ReviewApp. Sample them at a
    /// modest rate and write only changed anchors, also flushing on leave/quit.
    pub(crate) fn spawn_position_memory(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(500))
                .await;
            if this.update(cx, |app, _| app.persist_positions()).is_err() {
                break;
            }
        })
        .detach();
    }
}
