//! Comment-level controls shared by diff cards, the conversation, and the tray.

use crate::{
    group_comments,
    rows::{BuiltRows, Row},
    CommentSide, ComposerPlacement, ItemState, ReviewApp, ReviewComment, SessionCommand,
    SessionCompletion,
};
use gpui::{div, prelude::*, Context, MouseButton, SharedString, Window};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    Disableable as _, Sizable as _,
};

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
pub(crate) fn reserve_editor_rows(built: &mut BuiltRows, id: u64, count: usize) {
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
    let end = start
        + built.rows[start..]
            .iter()
            .take_while(|row| matches!(row, Row::CommentBody { .. } | Row::CommentMedia { .. }))
            .count();
    let count = count.max(1);
    for ix in built.file_rows.iter_mut().chain(&mut built.hunk_rows) {
        if *ix >= end {
            *ix = *ix - (end - start) + count;
        }
    }
    built
        .rows
        .splice(start..end, (0..count).map(|_| Row::CommentEditor { half }));
}

impl ReviewApp {
    fn published_comment(
        &self,
        target: &github::CommentTarget,
    ) -> Option<&github::DiscussionComment> {
        let discussion = self.active_data()?.discussion.as_ref()?;
        match target {
            github::CommentTarget::ReviewComment(_) => discussion
                .threads
                .iter()
                .flat_map(|t| &t.comments)
                .find(|c| c.permissions.node_id == node_id(target)),
            _ => discussion
                .comments
                .iter()
                .find(|c| c.permissions.node_id == node_id(target)),
        }
    }

    pub(crate) fn diff_comment_target(&self, id: u64) -> Option<CommentTarget> {
        let data = self.active_data()?;
        if let Some(local) = &data.local_review {
            return local
                .comments
                .iter()
                .any(|c| c.id == id)
                .then_some(CommentTarget::Local(id));
        }
        if self
            .sessions
            .get(&self.active_item()?.id)
            .and_then(|ui| ui.state.pending.as_ref())
            .is_some_and(|review| review.comments.iter().any(|c| c.id == id))
        {
            return Some(CommentTarget::Pending(id));
        }
        data.discussion
            .as_ref()?
            .threads
            .iter()
            .flat_map(|t| &t.comments)
            .find(|c| c.id == id)
            .map(|c| published_target("review", c))
    }

    fn comment_permissions(&self, target: &CommentTarget) -> (bool, bool) {
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
                    .active_item()
                    .and_then(|item| self.sessions.get(&item.id))
                    .and_then(|ui| ui.state.pending.as_ref())
                    .is_some_and(|review| review.comments.iter().any(|c| c.id == *id));
                (exists, exists)
            }
            CommentTarget::Local(id) => {
                let exists = self
                    .active_data()
                    .and_then(|data| data.local_review.as_ref())
                    .is_some_and(|review| review.comments.iter().any(|c| c.id == *id));
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
        let busy = self.sessions.get(&item_id).is_some_and(|ui| ui.busy);
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
                            .label("Edit")
                            .ghost()
                            .xsmall()
                            .disabled(busy || self.composer.is_some() || self.review.is_some())
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
                            .label("Delete")
                            .ghost()
                            .xsmall()
                            .disabled(busy || self.composer.is_some() || self.review.is_some())
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
        if !self.comment_permissions(&target).0 || self.composer.is_some() || self.review.is_some()
        {
            return;
        }
        let Some(item) = self.active_item() else {
            return;
        };
        if self.sessions.get(&item.id).is_some_and(|ui| ui.busy) {
            return;
        }
        let (id, path, side, line, start_line, body) = match &target {
            CommentTarget::Pending(id) => {
                let c = self.sessions[&item.id]
                    .state
                    .pending
                    .as_ref()
                    .unwrap()
                    .comments
                    .iter()
                    .find(|c| c.id == *id)
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
                    .find(|c| c.id == *id)
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
            CommentTarget::Published(published) => {
                let c = self.published_comment(published).unwrap();
                let thread = self
                    .active_data()
                    .unwrap()
                    .discussion
                    .as_ref()
                    .unwrap()
                    .threads
                    .iter()
                    .find(|t| {
                        t.comments
                            .iter()
                            .any(|member| member.permissions.node_id == c.permissions.node_id)
                    });
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
                    |row| matches!(row, Row::CommentHeader { comment_id, .. } if *comment_id == id),
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
        if let Some(composer) = &mut self.composer {
            composer.reply_to = None;
            composer.edit_id = Some(id);
            composer.edit_target = Some(target);
            composer.start_line = start_line;
            composer
                .input
                .update(cx, |state, cx| state.set_value(body, window, cx));
        }
        if let Some(composer) = self
            .composer
            .as_ref()
            .filter(|composer| composer.placement == ComposerPlacement::Diff)
        {
            let height = composer.height.clone();
            if let Some(data) = self.active_data_mut() {
                data.editing_comment = Some((id, height));
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
        if !self.comment_permissions(&target).1 || self.composer.is_some() || self.review.is_some()
        {
            return;
        }
        let Some(item_id) = self.active_item().map(|item| item.id) else {
            return;
        };
        if self.sessions.get(&item_id).is_some_and(|ui| ui.busy) {
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
                            .find(|c| c.id == id)
                            .and_then(|c| c.in_reply_to_id);
                        let replacement = parent.or_else(|| {
                            local
                                .comments
                                .iter()
                                .find(|c| c.in_reply_to_id == Some(id))
                                .map(|c| c.id)
                        });
                        local.comments.retain(|c| c.id != id);
                        for c in &mut local.comments {
                            if c.in_reply_to_id == Some(id) {
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

    /// Apply successful published writes before the refresh, even if the editor
    /// was closed or another PR was opened while GitHub was processing the write.
    pub(crate) fn apply_comment_mutation(&mut self, item_id: u64, command: &SessionCommand) {
        let (target, body) = match command {
            SessionCommand::EditPublished { target, body } => (target, Some(body.trim())),
            SessionCommand::DeletePublished { target } => (target, None),
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
        data.comments = Some(group_comments(
            discussion
                .review_comments()
                .into_iter()
                .map(ReviewComment::from)
                .collect(),
        ));
        if let Some(ui) = self.sessions.get(&item_id) {
            data.comments
                .as_mut()
                .unwrap()
                .replace_pending(ui.state.pending.as_ref());
        }
        data.rebuild_rows_anchored();
    }
}
