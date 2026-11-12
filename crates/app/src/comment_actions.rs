//! Comment-level controls shared by diff cards, the conversation, and the tray.

use crate::{
    rows::{BuiltRows, Row},
    CommentSide, ComposerPlacement, ItemState, ReviewApp, SessionCommand, SessionCompletion,
};
use gpui::component::{
    button::{Button, ButtonVariants as _},
    Disableable as _, Sizable as _,
};
use gpui::{div, prelude::*, Context, MouseButton, SharedString, Window};
use gpui_kit_assets::IconName;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CommentTarget {
    Pending(u64),
    Local(u64),
    Published(github::CommentTarget),
}

impl CommentTarget {
    pub(crate) fn key(&self) -> String {
        match self {
            Self::Pending(id) => format!("draft-{id}"),
            Self::Local(id) => format!("local-{id}"),
            Self::Published(target) => match target {
                github::CommentTarget::Issue(id) => format!("issue-{id}"),
                github::CommentTarget::Review(id) => format!("review-{id}"),
                github::CommentTarget::ReviewComment(id) => format!("review-comment-{id}"),
            },
        }
    }
}

pub(crate) fn published_target(kind: &str, comment: &github::DiscussionComment) -> CommentTarget {
    let id = comment.permissions.node_id.clone();
    CommentTarget::Published(if kind == "review" {
        github::CommentTarget::ReviewComment(id)
    } else if comment.review_state.is_some() {
        github::CommentTarget::Review(id)
    } else {
        github::CommentTarget::Issue(id)
    })
}

fn node_id(target: &github::CommentTarget) -> &str {
    match target {
        github::CommentTarget::Issue(id)
        | github::CommentTarget::Review(id)
        | github::CommentTarget::ReviewComment(id) => id,
    }
}

/// Preserve the uniform-list contract while an editor replaces a comment body.
pub(crate) fn reserve_editor_rows(
    built: &mut BuiltRows,
    id: crate::comments::CommentKey,
    count: usize,
) {
    let Some(header) = built
        .rows
        .iter()
        .position(|row| matches!(row, Row::CommentHeader { comment_id, .. } if *comment_id == id))
    else {
        return;
    };
    let half = match &built.rows[header] {
        Row::CommentHeader { half, .. } => *half,
        _ => unreachable!(),
    };
    let start = header + 1;
    let mut end = start
        + built.rows[start..]
            .iter()
            .take_while(|row| matches!(row, Row::CommentBody { .. }))
            .count();
    // The last comment's editor owns its footer. Do not leave a Reply button
    // sandwiched beneath its Save/Cancel actions. Earlier replies stay visible.
    if matches!(built.rows.get(end), Some(Row::CommentActions { .. })) {
        end += 1;
    }
    let count = count.max(1);
    for ix in built.file_rows.iter_mut().chain(&mut built.hunk_rows) {
        if *ix >= end {
            *ix = *ix - (end - start) + count;
        }
    }
    built.rows.splice(
        start..end,
        (0..count).map(|_| Row::CommentEditor { half, top: false }),
    );
}

/// Keep a reply inside its thread, reserving space below the current actions
/// row rather than retaining a display index captured before a row rebuild.
pub(crate) fn reserve_reply_rows(
    built: &mut BuiltRows,
    id: crate::comments::CommentKey,
    count: usize,
) {
    let Some(anchor) = built
        .rows
        .iter()
        .position(|row| matches!(row, Row::CommentActions { root_id, .. } if *root_id == id))
    else {
        return;
    };
    let half = match built.rows[anchor] {
        Row::CommentActions { half, .. } => half,
        _ => unreachable!(),
    };
    let start = anchor + 1;
    let count = count.max(1);
    for ix in built.file_rows.iter_mut().chain(&mut built.hunk_rows) {
        if *ix >= start {
            *ix += count;
        }
    }
    built.rows.splice(
        start..start,
        (0..count).map(|_| Row::CommentEditor { half, top: false }),
    );
}

/// Reserve a new card immediately below its last visual source row. Resolve
/// by line identity each time: stored display indices go stale after wrapping,
/// comment measurement, folding, or a view-mode change.
pub(crate) fn reserve_new_comment_rows(
    built: &mut BuiltRows,
    side: CommentSide,
    line: u64,
    half: Option<CommentSide>,
    count: usize,
) {
    let Some(anchor) = built
        .rows
        .iter()
        .rposition(|row| crate::rows::comment_line(row, side) == Some(line))
    else {
        return;
    };
    let start = anchor + 1;
    let count = count.max(1);
    for ix in built.file_rows.iter_mut().chain(&mut built.hunk_rows) {
        if *ix >= start {
            *ix += count;
        }
    }
    built.rows.splice(
        start..start,
        (0..count).map(|ix| Row::CommentEditor { half, top: ix == 0 }),
    );
}

impl ReviewApp {
    pub(crate) fn active_comment_projection(&self) -> Option<crate::comments::Projection<'_>> {
        let item = self.active_item()?;
        Some(
            self.active_data()?.comment_projection(
                self.sessions
                    .get(&item.id)
                    .and_then(|ui| ui.state.pending.as_ref()),
            ),
        )
    }

    pub(crate) fn published_comment(
        &self,
        target: &github::CommentTarget,
    ) -> Option<&github::DiscussionComment> {
        self.active_comment_projection()?.published(target)
    }

    pub(crate) fn diff_comment_target(
        &self,
        key: crate::comments::CommentKey,
    ) -> Option<CommentTarget> {
        let id = self.active_comment_projection()?.remote_id(key)?;
        let data = self.active_data()?;
        if let Some(local) = &data.local_review {
            return local
                .comments
                .iter()
                .any(|c| c.id == crate::comments::CommentKey::Inline(id))
                .then_some(CommentTarget::Local(id));
        }
        match self.active_comment_projection()?.inline(id)? {
            crate::comments::Comment::Pending(_) => Some(CommentTarget::Pending(id)),
            crate::comments::Comment::Published(c) => Some(published_target("review", c)),
            crate::comments::Comment::Posting(_) => None,
        }
    }

    pub(crate) fn comment_permissions(&self, target: &CommentTarget) -> (bool, bool) {
        match target {
            CommentTarget::Published(target) => {
                self.published_comment(target).map_or((false, false), |c| {
                    (
                        !c.permissions.node_id.is_empty() && c.permissions.can_edit,
                        !c.permissions.node_id.is_empty() && c.permissions.can_delete,
                    )
                })
            }
            CommentTarget::Pending(id) => {
                let exists = self
                    .active_comment_projection()
                    .is_some_and(|projection| projection.pending_comment(*id).is_some());
                (exists, exists)
            }
            CommentTarget::Local(id) => {
                let exists = self
                    .active_data()
                    .and_then(|data| data.local_review.as_ref())
                    .is_some_and(|review| {
                        review
                            .comments
                            .iter()
                            .any(|c| c.id == crate::comments::CommentKey::Inline(*id))
                    });
                (exists, exists)
            }
        }
    }

    pub(crate) fn comment_controls(
        &self,
        target: CommentTarget,
        placement: ComposerPlacement,
        entity: &gpui::Entity<Self>,
    ) -> gpui::AnyElement {
        let (can_edit, can_delete) = self.comment_permissions(&target);
        let Some(item) = self.active_item() else {
            return div().into_any_element();
        };
        let item_id = item.id;
        let busy = self.comment_write_blocked(item_id);
        let key = target.key();
        let mut controls = div()
            .flex()
            .items_center()
            .gap_2()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
        if can_edit {
            let target = target.clone();
            let entity = entity.clone();
            controls = controls.child(
                div()
                    .debug_selector(|| format!("comment-edit-{key}"))
                    .child(
                        Button::new(SharedString::from(format!("comment-edit-{key}")))
                            .icon(IconName::Pencil)
                            .accessibility_label("Edit comment")
                            .tooltip("Edit comment")
                            .ghost()
                            .xsmall()
                            .disabled(busy || self.composer().is_some() || self.review.is_some())
                            .on_click(move |_, window, cx| {
                                entity.update(cx, |this, cx| {
                                    if this.active_item().map(|item| item.id) == Some(item_id) {
                                        this.edit_comment(
                                            target.clone(),
                                            placement.clone(),
                                            window,
                                            cx,
                                        );
                                    }
                                })
                            }),
                    ),
            );
        }
        if can_delete {
            let entity = entity.clone();
            controls = controls.child(
                div()
                    .debug_selector(|| format!("comment-delete-{key}"))
                    .child(
                        Button::new(SharedString::from(format!("comment-delete-{key}")))
                            .icon(IconName::Trash)
                            .accessibility_label("Delete comment")
                            .tooltip("Delete comment")
                            .ghost()
                            .xsmall()
                            .disabled(busy || self.composer().is_some() || self.review.is_some())
                            .on_click(move |_, window, cx| {
                                entity.update(cx, |this, cx| {
                                    if this.active_item().map(|item| item.id) == Some(item_id) {
                                        this.confirm_delete_comment(target.clone(), window, cx);
                                    }
                                })
                            }),
                    ),
            );
        }
        controls.into_any_element()
    }

    pub(crate) fn edit_comment(
        &mut self,
        target: CommentTarget,
        placement: ComposerPlacement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.comment_permissions(&target).0
            || self.composer().is_some()
            || self.review.is_some()
        {
            return;
        }
        let Some(item) = self.active_item() else {
            return;
        };
        if self.comment_write_blocked(item.id) {
            return;
        }
        let (id, path, side, line, start_line, body) = match &target {
            CommentTarget::Pending(id) => {
                let c = self
                    .active_comment_projection()
                    .unwrap()
                    .pending_comment(*id)
                    .unwrap();
                (
                    c.id,
                    c.path.clone(),
                    c.side,
                    c.line,
                    c.start_line,
                    c.body.clone(),
                )
            }
            CommentTarget::Local(id) => {
                let c = self
                    .active_data()
                    .unwrap()
                    .local_review
                    .as_ref()
                    .unwrap()
                    .comments
                    .iter()
                    .find(|c| c.id == crate::comments::CommentKey::Inline(*id))
                    .unwrap();
                (
                    *id,
                    c.path.clone(),
                    c.side,
                    c.line,
                    c.start_line,
                    c.body.clone(),
                )
            }
            CommentTarget::Published(published) => {
                let c = self.published_comment(published).unwrap();
                let thread = if matches!(published, github::CommentTarget::ReviewComment(_)) {
                    self.active_comment_projection()
                        .unwrap()
                        .thread_for_inline(c.id)
                } else {
                    None
                };
                (
                    c.id,
                    thread.map_or(String::new(), |t| t.path.clone()),
                    if thread.is_some_and(|t| t.side == "LEFT") {
                        CommentSide::Left
                    } else {
                        CommentSide::Right
                    },
                    thread.and_then(|t| t.line),
                    thread.and_then(|t| t.start_line),
                    c.body.clone(),
                )
            }
        };
        let mut placement = placement;
        let mut row_ix = 0;
        if placement == ComposerPlacement::Diff {
            if let Some(data) = self.active_data_mut() {
                if let Some(thread) = data.comment_projection(None).thread_for_inline(id) {
                    data.expanded_threads.insert(thread.id.clone());
                }
                if let Some(file_ix) = data
                    .diff
                    .files
                    .iter()
                    .position(|file| file.display_path() == path)
                {
                    if data.file_ix != file_ix {
                        data.open_file(file_ix, None);
                    }
                    data.comments_visible = true;
                    data.rebuild_rows_anchored();
                }
                if let Some(ix) = data.rows.iter().position(
                    |row| matches!(row, Row::CommentHeader { comment_id, .. } if data.posts.remote_id(*comment_id) == Some(id)),
                ) {
                    row_ix = ix;
                } else {
                    // Outdated/unanchorable drafts are still editable in their tray row.
                    placement = ComposerPlacement::PendingTray(id);
                }
            }
        }
        // Existing identities do not need a current diff anchor (or head SHA).
        self.open_comment_composer(
            placement,
            Some(id),
            path,
            side,
            line.unwrap_or(0),
            row_ix,
            window,
            cx,
        );
        if let Some(composer) = self.composer_mut() {
            composer.reply_to = None;
            composer.edit_id = Some(id);
            composer.edit_target = Some(target);
            composer.start_line = start_line;
            composer.input.update(cx, |state, cx| {
                crate::comment_editor::set_value_at_end(state, body, window, cx)
            });
        }
        if let Some(composer) = self
            .composer()
            .filter(|composer| composer.placement == ComposerPlacement::Diff)
        {
            let height = composer.height.clone();
            if let Some(data) = self.active_data_mut() {
                data.replying_comments.remove(&id);
                data.editing_comments.insert(id, height);
                data.rebuild_rows_anchored();
            }
        }
        cx.notify();
    }

    pub(crate) fn confirm_delete_comment(
        &mut self,
        target: CommentTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.comment_permissions(&target).1 {
            return;
        }
        let Some(item_id) = self.active_item().map(|item| item.id) else {
            return;
        };
        let answer = window.prompt(
            gpui::PromptLevel::Warning,
            "Delete this comment?",
            Some("This cannot be undone."),
            &["Cancel", "Delete"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await.ok() != Some(1) {
                return;
            }
            this.update_in(cx, |app, window, cx| {
                if app.active_item().map(|item| item.id) == Some(item_id) {
                    app.delete_comment(target, window, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn delete_comment(
        &mut self,
        target: CommentTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.comment_permissions(&target).1
            || self.composer().is_some()
            || self.review.is_some()
        {
            return;
        }
        let Some(item_id) = self.active_item().map(|item| item.id) else {
            return;
        };
        if self.comment_write_blocked(item_id) {
            return;
        }
        match target {
            CommentTarget::Pending(id) => self.run_session(
                item_id,
                SessionCommand::Delete { id },
                SessionCompletion::None,
                window,
                cx,
            ),
            CommentTarget::Published(target) => self.run_session(
                item_id,
                SessionCommand::DeletePublished { target },
                SessionCompletion::None,
                window,
                cx,
            ),
            CommentTarget::Local(id) => {
                if let Some(data) = self.active_data_mut() {
                    if let Some(local) = &mut data.local_review {
                        // Promote the first reply rather than dropping the rest of the thread.
                        let parent = local
                            .comments
                            .iter()
                            .find(|c| c.id == crate::comments::CommentKey::Inline(id))
                            .and_then(|c| c.in_reply_to_id);
                        let replacement = parent.or_else(|| {
                            local
                                .comments
                                .iter()
                                .find(|c| c.in_reply_to_id == Some(id.into()))
                                .map(|c| c.id)
                        });
                        local
                            .comments
                            .retain(|c| c.id != crate::comments::CommentKey::Inline(id));
                        for c in &mut local.comments {
                            if c.in_reply_to_id == Some(id.into()) {
                                c.in_reply_to_id =
                                    replacement.filter(|replacement| *replacement != c.id);
                            }
                        }
                        data.comments = Some(local.index());
                        data.rebuild_rows_anchored();
                    }
                }
                cx.notify();
            }
        }
    }

    pub(crate) fn comment_change(
        &self,
        item_id: u64,
        command: &SessionCommand,
        token: u64,
        placement: ComposerPlacement,
    ) -> Option<crate::comments::mutations::Change> {
        use crate::comments::{mutations::Change, CommentKey};
        let item = self.items.iter().find(|i| i.id == item_id)?;
        let ItemState::Ready(data) = &item.state else {
            return None;
        };
        let (target, key, confirmed, body) = match command {
            SessionCommand::EditPublished { target, .. }
            | SessionCommand::DeletePublished { target } => {
                let c = data.comment_projection(None).published(target)?;
                let key = match target {
                    github::CommentTarget::ReviewComment(_) => CommentKey::Inline(c.id),
                    // Part 4 is inline-only. Issue comments and submitted-review
                    // summaries retain their existing completion/editor timing.
                    _ => return None,
                };
                let body = match command {
                    SessionCommand::EditPublished { body, .. } => Some(body.trim().to_owned()),
                    _ => None,
                };
                (
                    github::MutationTarget::Published(target.clone()),
                    key,
                    c.body.clone(),
                    body,
                )
            }
            SessionCommand::Edit { id, .. } | SessionCommand::Delete { id } => {
                let review = self.sessions.get(&item_id)?.state.pending.as_ref()?;
                let c = review.comments.iter().find(|c| c.id == *id)?;
                let body = match command {
                    SessionCommand::Edit { body, .. } => Some(body.trim().to_owned()),
                    _ => None,
                };
                (
                    github::MutationTarget::Pending {
                        review_id: review.id,
                        id: *id,
                        node_id: c.node_id.clone(),
                    },
                    CommentKey::Pending(*id),
                    c.body.clone(),
                    body,
                )
            }
            _ => return None,
        };
        Some(Change {
            token,
            key,
            target,
            recovery_body: body.clone().unwrap_or(confirmed),
            body,
            placement,
            uncertain: false,
            error: None,
        })
    }

    pub(crate) fn render_comment_changes(
        &self,
        item_id: u64,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let Some(item) = self.items.iter().find(|i| i.id == item_id) else {
            return Vec::new();
        };
        let ItemState::Ready(data) = &item.state else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        for change in data
            .posts
            .changes
            .active
            .iter()
            .chain(&data.posts.changes.failed)
        {
            let token = change.token;
            let editing = change.body.is_some();
            let active = data.posts.changes.active.iter().any(|c| c.token == token);
            if active && editing && !change.uncertain {
                continue;
            }
            let message = if change.uncertain && !active {
                "Outcome dismissed after checking; showing confirmed content. No write retried."
                    .to_owned()
            } else if change.uncertain {
                format!("{} unverified. Check GitHub before changing comments or submitting the review.", if editing { "Edit" } else { "Deletion" })
            } else if active {
                "Deleting comment…".to_owned()
            } else {
                format!(
                    "{} rejected: {}",
                    if editing {
                        "Edit"
                    } else {
                        "Deletion (comment restored)"
                    },
                    change.error.as_deref().unwrap_or("rejected")
                )
            };
            let mut row = div()
                .debug_selector(move || format!("comment-change-{item_id}-{token}"))
                .flex()
                .items_center()
                .gap_2()
                .child(div().flex_1().min_w_0().child(message));
            if change.error.is_some() {
                let body = change.recovery_body.clone();
                row = row.child(
                    Button::new(SharedString::from(format!("copy-change-{token}")))
                        .label("Copy text")
                        .ghost()
                        .small()
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(body.clone()))
                        }),
                );
                if change.uncertain {
                    let url = match &item.source {
                        crate::Source::Pr(loc) => {
                            format!("https://github.com/{}/pull/{}", loc.repo_slug(), loc.number)
                        }
                        _ => String::new(),
                    };
                    row = row.child(
                        Button::new(SharedString::from(format!("verify-change-{token}")))
                            .label("Check GitHub")
                            .ghost()
                            .small()
                            .on_click(move |_, _, cx| cx.open_url(&url)),
                    );
                    if active {
                        row = row.child(
                            Button::new(SharedString::from(format!("dismiss-change-{token}")))
                                .label("Dismiss after checking")
                                .ghost()
                                .small()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.dismiss_comment_change(item_id, token, cx)
                                })),
                        );
                    }
                } else if editing && !active {
                    row = row.child(
                        Button::new(SharedString::from(format!("restore-change-{token}")))
                            .label("Restore edit")
                            .ghost()
                            .small()
                            .disabled(
                                self.comment_write_blocked(item_id)
                                    || self.composers.iter().any(|c| c.item_id == item_id)
                                    || self.review.is_some(),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.restore_comment_change(item_id, token, window, cx)
                            })),
                    );
                }
            }
            rows.push(row.into_any_element());
        }
        rows
    }

    pub(crate) fn dismiss_comment_change(
        &mut self,
        item_id: u64,
        token: u64,
        cx: &mut Context<Self>,
    ) {
        if let Some(ItemState::Ready(data)) = self
            .items
            .iter_mut()
            .find(|i| i.id == item_id)
            .map(|i| &mut i.state)
        {
            if let Some(ix) = data
                .posts
                .changes
                .active
                .iter()
                .position(|c| c.token == token && c.uncertain)
            {
                let change = data.posts.changes.active.remove(ix);
                data.posts.changes.failed.push(change);
            }
        }
        self.sync_pending_comments(item_id);
        cx.notify();
    }

    pub(crate) fn restore_comment_change(
        &mut self,
        item_id: u64,
        token: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.active_item().map(|i| i.id) != Some(item_id)
            || self.composers.iter().any(|c| c.item_id == item_id)
            || self.review.is_some()
            || self.comment_write_blocked(item_id)
        {
            return;
        }
        let Some(change) = self
            .active_data()
            .and_then(|d| {
                d.posts
                    .changes
                    .failed
                    .iter()
                    .find(|c| c.token == token && !c.uncertain && c.body.is_some())
            })
            .cloned()
        else {
            return;
        };
        let target = match &change.target {
            github::MutationTarget::Published(target) => CommentTarget::Published(target.clone()),
            github::MutationTarget::Pending {
                review_id,
                id,
                node_id,
            } => {
                if !self
                    .sessions
                    .get(&item_id)
                    .and_then(|ui| ui.state.pending.as_ref())
                    .is_some_and(|r| {
                        r.id == *review_id
                            && r.comments
                                .iter()
                                .any(|c| c.id == *id && c.node_id == *node_id)
                    })
                {
                    return;
                }
                CommentTarget::Pending(*id)
            }
        };
        self.edit_comment(target.clone(), change.placement, window, cx);
        if let Some(composer) = self
            .composer_mut()
            .filter(|c| c.item_id == item_id && c.edit_target.as_ref() == Some(&target))
        {
            composer.input.update(cx, |input, cx| {
                crate::comment_editor::set_value_at_end(input, change.recovery_body, window, cx)
            });
            self.active_data_mut()
                .unwrap()
                .posts
                .changes
                .failed
                .retain(|c| c.token != token);
        }
        cx.notify();
    }

    /// Apply successful published writes before the refresh, even if the editor
    /// was closed or another PR was opened while GitHub was processing the write.
    pub(crate) fn apply_comment_mutation(
        &mut self,
        item_id: u64,
        receipt: &github::MutationReceipt,
    ) {
        use github::{MutationReceipt, MutationTarget};
        let (target, body) = match receipt {
            MutationReceipt::Edited {
                target: MutationTarget::Published(target),
                body,
            } => (target, Some(body.as_str())),
            MutationReceipt::Deleted {
                target: MutationTarget::Published(target),
            } => (target, None),
            _ => return,
        };
        let Some(ItemState::Ready(data)) = self
            .items
            .iter_mut()
            .find(|item| item.id == item_id)
            .map(|item| &mut item.state)
        else {
            return;
        };
        let Some(discussion) = &mut data.discussion else {
            return;
        };
        let update = |comments: &mut Vec<github::DiscussionComment>| {
            comments.retain_mut(|c| {
                if c.permissions.node_id != node_id(target) {
                    return true;
                }
                if let Some(body) = body {
                    c.body = body.into();
                    true
                } else {
                    false
                }
            });
        };
        match target {
            github::CommentTarget::ReviewComment(_) => {
                for thread in &mut discussion.threads {
                    update(&mut thread.comments);
                }
                discussion.threads.retain(|t| !t.comments.is_empty());
            }
            _ => update(&mut discussion.comments),
        }
        data.posts.reconcile(discussion);
        data.project_comments(
            self.sessions
                .get(&item_id)
                .and_then(|ui| ui.state.pending.as_ref()),
        );
        data.rebuild_rows_anchored();
    }
}
