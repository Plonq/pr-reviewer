//! Shared reaction counts and picker for published comments in either view.

use std::collections::HashSet;

use crate::{
    rows::{BuiltRows, Row},
    theme, ItemState, ReviewApp, SessionCommand, SessionCompletion,
};
use gpui::component::{
    button::{Button, ButtonVariants as _},
    Disableable as _, Icon, Selectable as _, Sizable as _,
};
use gpui::{div, prelude::*, Context, Entity, MouseButton, SharedString, Window};

/// Reserve a footer only for comments with reactions or permission to add them.
/// Run after Markdown/editor sizing so it follows the entire body, not each slot.
pub(crate) fn reserve_reaction_rows(
    built: &mut BuiltRows,
    discussion: Option<&github::PrDiscussion>,
) {
    let eligible: HashSet<_> = discussion
        .into_iter()
        .flat_map(|d| &d.threads)
        .flat_map(|t| &t.comments)
        .filter(|c| {
            c.reactions.iter().any(|r| r.count > 0)
                || (c.permissions.can_react && !c.permissions.node_id.is_empty())
        })
        .map(|c| c.id)
        .collect();
    if eligible.is_empty() {
        return;
    }
    let old = std::mem::take(&mut built.rows);
    let mut mapping = vec![0; old.len() + 1];
    let mut old = old.into_iter().enumerate().peekable();
    while let Some((ix, row)) = old.next() {
        mapping[ix] = built.rows.len();
        let footer = match &row {
            Row::CommentBody { text, half, .. } if eligible.contains(&text.comment_id)
                && !old.peek().is_some_and(|(_, next)| matches!(next, Row::CommentBody { text: next, .. } if next.comment_id == text.comment_id)) => {
                    Some(Row::CommentReactions { comment_id: text.comment_id, half: *half })
                }
            _ => None,
        };
        built.rows.push(row);
        built.rows.extend(footer);
    }
    *mapping.last_mut().unwrap() = built.rows.len();
    for ix in built.file_rows.iter_mut().chain(&mut built.hunk_rows) {
        *ix = mapping[*ix];
    }
}

fn react_button() -> Button {
    Button::new("add-reaction")
        .icon(
            Icon::default()
                .path("icons/gander/smiley-16.svg")
                .size(gpui::px(16.)),
        )
        .tooltip("Add reaction")
        .accessibility_label("Add reaction")
        .ghost()
        .xsmall()
}

impl ReviewApp {
    pub(crate) fn diff_comment_reactions(
        &self,
        id: u64,
        entity: &Entity<Self>,
    ) -> gpui::AnyElement {
        if let Some(crate::CommentTarget::Published(target)) = self.diff_comment_target(id) {
            if let Some(comment) = self.published_comment(&target) {
                return self.comment_reactions(target, comment, entity);
            }
        }
        div().into_any_element()
    }

    pub(crate) fn comment_reactions(
        &self,
        target: github::CommentTarget,
        comment: &github::DiscussionComment,
        entity: &Entity<Self>,
    ) -> gpui::AnyElement {
        let Some(item) = self.active_item() else {
            return div().into_any_element();
        };
        let item_id = item.id;
        let key = crate::CommentTarget::Published(target.clone()).key();
        let enabled = comment.permissions.can_react
            && !comment.permissions.node_id.is_empty()
            && !self.sessions.get(&item_id).is_some_and(|ui| ui.busy);
        let mut controls = div()
            .flex()
            .items_center()
            .gap_1()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
        for reaction in comment.reactions.iter().filter(|r| r.count > 0) {
            let entity = entity.clone();
            let target = target.clone();
            let content = reaction.content.clone();
            let reacted = !reaction.viewer_has_reacted;
            controls = controls.child(
                div()
                    .debug_selector(|| format!("reaction-{key}-{content}"))
                    .child(
                        Button::new(SharedString::from(format!("reaction-{key}-{content}")))
                            .label(format!("{} {}", reaction.emoji(), reaction.count))
                            .tooltip(format!(
                                "{} {}",
                                if reacted { "Add" } else { "Remove" },
                                content.to_lowercase().replace('_', " ")
                            ))
                            .ghost()
                            .xsmall()
                            .selected(reaction.viewer_has_reacted)
                            .disabled(!enabled)
                            .on_click(move |_, window, cx| {
                                entity.update(cx, |this, cx| {
                                    this.set_reaction(
                                        item_id,
                                        target.clone(),
                                        content.clone(),
                                        reacted,
                                        window,
                                        cx,
                                    );
                                });
                            }),
                    ),
            );
        }
        // Cached comments without capabilities remain display-only until refreshed.
        if enabled {
            let entity = entity.clone();
            let reactions = comment.reactions.clone();
            controls = controls.child(
                gpui::base::Popover::new(SharedString::from(format!("reaction-picker-{key}")))
                    .trigger_with(move |_, _, _| {
                        div()
                            .debug_selector(|| format!("reaction-picker-{key}"))
                            .child(react_button())
                            .into_any_element()
                    })
                    .content(move |_, _, cx| {
                        let popover = cx.entity();
                        div()
                            .debug_selector(|| "reaction-options".into())
                            .p_2()
                            .flex()
                            .gap_1()
                            .rounded_md()
                            .bg(theme::surface0(cx))
                            .children(github::Reaction::KINDS.into_iter().map(|content| {
                                let selected = reactions
                                    .iter()
                                    .any(|r| r.content == content && r.viewer_has_reacted);
                                let reaction = github::Reaction {
                                    content: content.into(),
                                    count: 0,
                                    viewer_has_reacted: selected,
                                };
                                let entity = entity.clone();
                                let target = target.clone();
                                let popover = popover.clone();
                                div()
                                    .debug_selector(move || format!("reaction-option-{content}"))
                                    .child(
                                        Button::new(content)
                                            .label(reaction.emoji().to_owned())
                                            .tooltip(content.to_lowercase().replace('_', " "))
                                            .ghost()
                                            .small()
                                            .selected(selected)
                                            .on_click(move |_, window, cx| {
                                                popover.update(cx, |state, cx| {
                                                    state.dismiss(window, cx)
                                                });
                                                entity.update(cx, |this, cx| {
                                                    this.set_reaction(
                                                        item_id,
                                                        target.clone(),
                                                        content.into(),
                                                        !selected,
                                                        window,
                                                        cx,
                                                    );
                                                });
                                            }),
                                    )
                            }))
                    }),
            );
        } else if comment.permissions.can_react && !comment.permissions.node_id.is_empty() {
            controls = controls.child(react_button().disabled(true));
        }
        controls.into_any_element()
    }

    pub(crate) fn set_reaction(
        &mut self,
        item_id: u64,
        target: github::CommentTarget,
        content: String,
        reacted: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.active_item().map(|item| item.id) != Some(item_id)
            || self.sessions.get(&item_id).is_some_and(|ui| ui.busy)
            || !self
                .published_comment(&target)
                .is_some_and(|c| c.permissions.can_react && !c.permissions.node_id.is_empty())
        {
            return;
        }
        self.run_session(
            item_id,
            SessionCommand::SetReaction {
                target,
                content,
                reacted,
            },
            SessionCompletion::None,
            window,
            cx,
        );
    }

    pub(crate) fn apply_reaction(&mut self, item_id: u64, command: &SessionCommand) {
        let SessionCommand::SetReaction {
            target,
            content,
            reacted,
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
        let (github::CommentTarget::Issue(id)
        | github::CommentTarget::Review(id)
        | github::CommentTarget::ReviewComment(id)) = target;
        let comments: &mut [github::DiscussionComment] = match target {
            github::CommentTarget::ReviewComment(_) => {
                let Some(thread) = discussion
                    .threads
                    .iter_mut()
                    .find(|t| t.comments.iter().any(|c| c.permissions.node_id == *id))
                else {
                    return;
                };
                &mut thread.comments
            }
            _ => &mut discussion.comments,
        };
        if let Some(comment) = comments.iter_mut().find(|c| c.permissions.node_id == *id) {
            update_reaction(&mut comment.reactions, content, *reacted);
        }
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
    }
}

fn update_reaction(reactions: &mut Vec<github::Reaction>, content: &str, reacted: bool) {
    if let Some(reaction) = reactions.iter_mut().find(|r| r.content == content) {
        if reaction.viewer_has_reacted != reacted {
            reaction.count = if reacted {
                reaction.count.saturating_add(1)
            } else {
                reaction.count.saturating_sub(1)
            };
            reaction.viewer_has_reacted = reacted;
        }
    } else if reacted {
        reactions.push(github::Reaction {
            content: content.into(),
            count: 1,
            viewer_has_reacted: true,
        });
    }
    reactions.retain(|r| r.count > 0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reaction_footers_follow_each_complete_body_and_preserve_navigation_indices() {
        let discussion = crate::conversation::tests::sample_discussion();
        let body = |id, slot, half| Row::CommentBody {
            line: "text".into(),
            half,
            slot,
            text: std::sync::Arc::new(crate::diff_media::CommentText {
                comment_id: id,
                part: 0,
                source: "text".into(),
            }),
        };
        let mut built = BuiltRows {
            rows: vec![
                body(1, 0, Some(crate::CommentSide::Left)),
                body(1, 1, Some(crate::CommentSide::Left)),
                body(3, 0, Some(crate::CommentSide::Right)),
                Row::Binary,
            ],
            file_rows: vec![0, 3],
            hunk_rows: vec![3],
        };
        reserve_reaction_rows(&mut built, Some(&discussion));
        assert_eq!(built.rows.len(), 6);
        assert!(matches!(
            built.rows[2],
            Row::CommentReactions {
                comment_id: 1,
                half: Some(crate::CommentSide::Left)
            }
        ));
        assert!(matches!(
            built.rows[4],
            Row::CommentReactions {
                comment_id: 3,
                half: Some(crate::CommentSide::Right)
            }
        ));
        assert!(matches!(built.rows[5], Row::Binary));
        assert_eq!(built.file_rows, [0, 5]);
        assert_eq!(built.hunk_rows, [5]);

        // No empty footer on read-only comments without reactions, or editors.
        let mut discussion = discussion;
        for comment in &mut discussion.threads[0].comments {
            comment.reactions.clear();
        }
        let mut built = BuiltRows {
            rows: vec![body(1, 0, None), Row::CommentEditor { half: None }],
            file_rows: vec![0],
            hunk_rows: vec![],
        };
        reserve_reaction_rows(&mut built, Some(&discussion));
        assert_eq!(built.rows.len(), 2);
        discussion.threads[0].comments[0].permissions = github::CommentPermissions {
            node_id: "RC1".into(),
            can_react: true,
            ..Default::default()
        };
        reserve_reaction_rows(&mut built, Some(&discussion));
        assert_eq!(built.rows.len(), 3);
        assert!(matches!(
            built.rows[1],
            Row::CommentReactions { comment_id: 1, .. }
        ));
        assert!(matches!(built.rows[2], Row::CommentEditor { .. }));
    }

    #[test]
    fn reaction_updates_preserve_other_users_and_are_idempotent() {
        let mut reactions = vec![github::Reaction {
            content: "HEART".into(),
            count: 3,
            viewer_has_reacted: false,
        }];
        update_reaction(&mut reactions, "HEART", true);
        update_reaction(&mut reactions, "HEART", true);
        assert_eq!(reactions[0].count, 4);
        assert!(reactions[0].viewer_has_reacted);
        update_reaction(&mut reactions, "ROCKET", true);
        assert_eq!(reactions.len(), 2);
        update_reaction(&mut reactions, "HEART", false);
        update_reaction(&mut reactions, "HEART", false);
        assert_eq!(reactions[0].count, 3);
        assert!(!reactions[0].viewer_has_reacted);
        update_reaction(&mut reactions, "ROCKET", false);
        assert_eq!(reactions.len(), 1);
        update_reaction(&mut reactions, "EYES", false);
        assert_eq!(reactions.len(), 1);
    }
}
