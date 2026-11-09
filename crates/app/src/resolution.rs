//! Review-thread resolution and expansion, shared by conversation and diff cards.

use std::collections::HashSet;

use gpui::component::{
    button::{Button, ButtonVariants as _},
    Disableable as _, Sizable as _,
};
use gpui::{div, prelude::*, Context, MouseButton, SharedString, Window};

use crate::{
    rows::{BuiltRows, Row},
    ItemState, ReviewApp, SessionCommand, SessionCompletion,
};

/// Keep only the root header for collapsed threads. Remap navigation indices
/// alongside the rows so file/hunk jumps and the minimap stay aligned.
pub(crate) fn collapse_threads(
    built: &mut BuiltRows,
    collapsed: &HashSet<crate::comments::CommentKey>,
) {
    if collapsed.is_empty() {
        return;
    }
    let mut skip = false;
    let mut next = 0;
    let mut indices = Vec::with_capacity(built.rows.len() + 1);
    built.rows.retain_mut(|row| {
        indices.push(next);
        let keep = match row {
            Row::CommentHeader {
                comment_id,
                top: true,
                collapsed: header_collapsed,
                ..
            } => {
                skip = collapsed.contains(comment_id);
                *header_collapsed = skip;
                true
            }
            row if crate::rows::is_comment_row(row) => !skip,
            _ => {
                skip = false;
                true
            }
        };
        next += usize::from(keep);
        keep
    });
    indices.push(next);
    for ix in built.file_rows.iter_mut().chain(&mut built.hunk_rows) {
        *ix = indices[*ix];
    }
}

impl ReviewApp {
    pub(crate) fn toggle_thread_expanded(
        &mut self,
        item_id: u64,
        thread_id: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = self.items.iter_mut().find(|item| item.id == item_id) else {
            return;
        };
        let ItemState::Ready(data) = &mut item.state else {
            return;
        };
        if !data.expanded_threads.remove(thread_id) {
            data.expanded_threads.insert(thread_id.to_owned());
        }
        data.rebuild_rows_anchored();
        cx.notify();
    }

    pub(crate) fn set_thread_resolved(
        &mut self,
        item_id: u64,
        thread_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = self.items.iter().find(|item| item.id == item_id) else {
            return;
        };
        if item.reloading || self.composer().is_some() || self.review.is_some() {
            return;
        }
        let ItemState::Ready(data) = &item.state else {
            return;
        };
        let Some(thread) = data
            .discussion
            .as_ref()
            .and_then(|d| d.threads.iter().find(|t| t.id == thread_id))
        else {
            return;
        };
        if !(if thread.is_resolved {
            thread.viewer_can_unresolve
        } else {
            thread.viewer_can_resolve
        }) {
            return;
        }
        let command = SessionCommand::SetThreadResolved {
            thread_id: thread.id.clone(),
            resolved: !thread.is_resolved,
        };
        if self.comment_write_blocked(item_id) {
            return;
        }
        self.run_session(item_id, command, SessionCompletion::None, window, cx);
    }

    pub(crate) fn thread_controls(
        &self,
        item_id: u64,
        thread: &github::ReviewThread,
        diff: bool,
        entity: &gpui::Entity<Self>,
        cx: &gpui::App,
    ) -> gpui::AnyElement {
        let item = self.items.iter().find(|item| item.id == item_id);
        let busy = item.is_none_or(|item| item.reloading)
            || self.comment_write_blocked(item_id)
            || self.composer().is_some()
            || self.review.is_some();
        let mut controls = div()
            .id(SharedString::from(format!(
                "thread-controls-{item_id}-{}",
                thread.id
            )))
            .flex()
            .items_center()
            .gap_2()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
        if diff {
            if thread.is_outdated {
                controls = controls.child(
                    div()
                        .debug_selector(|| {
                            format!("inline-thread-outdated-{item_id}-{}", thread.id)
                        })
                        .flex_shrink_0()
                        .whitespace_nowrap()
                        .text_color(crate::theme::subtext(cx))
                        .child("Outdated"),
                );
            }
            if thread.is_resolved {
                let expanded = item.is_some_and(|item| matches!(&item.state, ItemState::Ready(data) if data.expanded_threads.contains(&thread.id)));
                let thread_id = thread.id.clone();
                let entity = entity.clone();
                controls = controls.child(
                    div()
                        .debug_selector(|| format!("thread-expand-{item_id}-{}", thread.id))
                        .child(
                            Button::new(SharedString::from(format!(
                                "thread-expand-{item_id}-{}",
                                thread.id
                            )))
                            .label(if expanded {
                                "▾ Resolved"
                            } else {
                                "▸ Resolved"
                            })
                            .ghost()
                            .xsmall()
                            .disabled(self.composer().is_some())
                            .on_click(move |_, _, cx| {
                                entity.update(cx, |this, cx| {
                                    this.toggle_thread_expanded(item_id, &thread_id, cx)
                                })
                            }),
                        ),
                );
            } else {
                controls = controls.child(
                    div()
                        .text_color(crate::theme::subtext(cx))
                        .child("Unresolved"),
                );
            }
        }
        if if thread.is_resolved {
            thread.viewer_can_unresolve
        } else {
            thread.viewer_can_resolve
        } {
            let thread_id = thread.id.clone();
            let entity = entity.clone();
            controls = controls.child(
                div()
                    .debug_selector(|| format!("thread-resolve-{item_id}-{}", thread.id))
                    .child(
                        Button::new(SharedString::from(format!(
                            "thread-resolve-{item_id}-{}",
                            thread.id
                        )))
                        .label(if thread.is_resolved {
                            "Unresolve"
                        } else {
                            "Resolve"
                        })
                        .ghost()
                        .xsmall()
                        .disabled(busy)
                        .on_click(move |_, window, cx| {
                            entity.update(cx, |this, cx| {
                                this.set_thread_resolved(item_id, &thread_id, window, cx)
                            })
                        }),
                    ),
            );
        }
        controls.into_any_element()
    }

    pub(crate) fn diff_comment_controls(
        &self,
        key: crate::comments::CommentKey,
        top: bool,
        entity: &gpui::Entity<Self>,
        cx: &gpui::App,
    ) -> gpui::AnyElement {
        let mut controls = div().flex().items_center().gap_2();
        let Some(id) = self
            .active_comment_projection()
            .and_then(|p| p.remote_id(key))
        else {
            return controls.into_any_element();
        };
        if top {
            if let Some(item) = self.active_item() {
                if let ItemState::Ready(data) = &item.state {
                    if let Some(thread) = data.discussion.as_ref().and_then(|d| {
                        d.threads
                            .iter()
                            .find(|t| t.comments.first().is_some_and(|c| c.id == id))
                    }) {
                        controls =
                            controls.child(self.thread_controls(item.id, thread, true, entity, cx));
                    }
                }
            }
        }
        if self
            .composer()
            .is_some_and(|c| c.placement == crate::ComposerPlacement::Diff && c.edit_id == Some(id))
        {
            controls = controls.child(
                div()
                    .debug_selector(|| "comment-editing".into())
                    .text_color(crate::theme::overlay0(cx))
                    .text_size(gpui::px(11.))
                    .child("Editing"),
            );
        } else if let Some(target) = self.diff_comment_target(key) {
            controls = controls.child(self.comment_controls(
                target,
                crate::ComposerPlacement::Diff,
                entity,
            ));
        }
        controls.into_any_element()
    }

    pub(crate) fn apply_thread_resolution(&mut self, item_id: u64, command: &SessionCommand) {
        let SessionCommand::SetThreadResolved {
            thread_id,
            resolved,
        } = command
        else {
            return;
        };
        let Some(item) = self.items.iter_mut().find(|item| item.id == item_id) else {
            return;
        };
        let ItemState::Ready(data) = &mut item.state else {
            return;
        };
        let Some(discussion) = &mut data.discussion else {
            return;
        };
        let Some(thread) = discussion.threads.iter_mut().find(|t| t.id == *thread_id) else {
            return;
        };
        thread.is_resolved = *resolved;
        data.expanded_threads.remove(thread_id);
        if let (crate::Source::Pr(loc), Some(meta)) = (&item.source, &data.pr_meta) {
            if let Ok(snapshot) = crate::review_target::discussion_snapshot(
                loc,
                meta,
                discussion,
                data.commits.as_ref(),
            ) {
                self.store.save_snapshot(&snapshot);
            }
        }
        data.rebuild_rows_anchored();
    }
}
