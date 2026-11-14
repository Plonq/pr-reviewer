//! Two scopes only: the complete PR, or the viewer's last submitted review.

use crate::{theme, ItemData, ItemState, ReviewApp, ReviewItem, Source};
use gpui::component::{
    button::{Button, ButtonVariants as _},
    tooltip::Tooltip,
    Disableable as _, Icon, Sizable as _,
};
use gpui::{div, prelude::*, px, Context, Window};
use gpui_kit_assets::IconName;

fn scope_status(
    changes: &github::ReviewChanges,
    active: bool,
    reloading: bool,
) -> (IconName, String) {
    let (icon, mut detail) = match changes {
        github::ReviewChanges::NotReviewed => (
            IconName::Info,
            "Submit a GitHub review to use Changed since last review.".into(),
        ),
        github::ReviewChanges::Unavailable(reason) => (IconName::TriangleAlert, reason.clone()),
        github::ReviewChanges::Available(review) => (
            IconName::Info,
            format!(
                "Last review: {} · {}{}",
                review.submitted_at,
                review.commit.chars().take(7).collect::<String>(),
                if reloading { " · loading…" } else { "" },
            ),
        ),
    };
    if active {
        detail.push_str("\n\nComments on the full PR's old revision remain in Conversation; new comments use explicit revision positioning.");
    }
    (icon, detail)
}

impl ItemData {
    pub(crate) fn diff_base(&self) -> &str {
        self.review_base
            .as_deref()
            .or(self.pr_base.as_deref())
            .unwrap_or("")
    }

    /// Incremental LEFT coordinates aren't full-PR LEFT coordinates. Force
    /// explicit blob positioning for incremental comments on either side.
    pub(crate) fn comment_hunks(&self) -> &[diff_core::Hunk] {
        if self.review_base.is_some() {
            &[]
        } else {
            self.anchor_hunks
                .get(self.file_ix)
                .map_or(&[], Vec::as_slice)
        }
    }

    /// The displayed baseline can include upstream lines that were never in
    /// the submitted review. Only send GitHub real, consecutive blob lines.
    pub(crate) fn selection_anchor(
        &self,
        points: &[(github::DiffSide, u64)],
    ) -> Result<github::CommentAnchor, String> {
        let file = self.diff.files.get(self.file_ix).ok_or("no source file")?;
        let mut anchor = crate::comment_anchor::selection_anchor(
            file,
            self.pr_meta
                .as_ref()
                .map_or("", |meta| meta.head_ref_oid.as_str()),
            self.diff_base(),
            self.comment_hunks(),
            points,
        )?;
        if anchor.side == github::DiffSide::Left {
            if let Some(files) = &self.review_base_files {
                let (start, end) = file.old_path.as_ref().and_then(|path| files.get(path))
                    .and_then(|base| base.reviewed_range(anchor.start_line.unwrap_or(anchor.line), anchor.line))
                    .ok_or("This range includes upstream-only lines with no position in your last review. Comment on the new revision instead.")?;
                anchor.line = end;
                anchor.start_line = (start != end).then_some(start);
            }
        }
        Ok(anchor)
    }

    pub(crate) fn filter_scope_comments(&mut self) {
        if self.review_base.is_some() {
            if let Some(comments) = &mut self.comments {
                comments.hide_left_anchors();
            }
        }
    }
}

impl ReviewApp {
    pub(crate) fn switch_review_scope(
        &mut self,
        since_review: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = self.items.get(self.active) else {
            return;
        };
        let ItemState::Ready(data) = &item.state else {
            return;
        };
        if item.reloading
            || data.review_base.is_some() == since_review
            || (since_review && data.review_changes.baseline().is_none())
            || !data.viewed.pending.is_empty()
            || self.sessions.get(&item.id).is_some_and(|ui| ui.busy)
            || self
                .composers
                .iter()
                .any(|composer| composer.item_id == item.id && composer.in_flight)
            || self.review.is_some()
        {
            return;
        }
        let (id, source, mode) = (item.id, item.source.clone(), data.mode);
        self.persist_positions();
        // Draft anchors belong to the old review scope, including hidden files.
        let drafts: Vec<_> = self
            .composers
            .iter()
            .filter(|composer| composer.item_id == id)
            .map(|composer| composer.id)
            .collect();
        for draft in drafts {
            self.close_composer_id(draft, window, cx);
        }
        self.drag_anchor = None;
        self.spawn_fetch_scoped(id, source, mode, since_review, cx);
        cx.notify();
    }

    pub(crate) fn render_changes_header(
        &self,
        item: &ReviewItem,
        data: &ItemData,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let is_pr = matches!(item.source, Source::Pr(_));
        let active = data.review_base.is_some();
        let busy = item.reloading
            || !data.viewed.pending.is_empty()
            || self.sessions.get(&item.id).is_some_and(|ui| ui.busy)
            || self
                .composers
                .iter()
                .any(|composer| composer.item_id == item.id && composer.in_flight)
            || self.review.is_some();
        let (icon, detail) = scope_status(&data.review_changes, active, item.reloading);
        let mut bar = div()
            .debug_selector(|| "changes-header".into())
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap_1()
            .px_2()
            .py_1()
            .bg(theme::mantle(cx))
            .border_b_1()
            .border_color(theme::surface0(cx))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .children(is_pr.then(|| {
                        div()
                            .debug_selector(|| "review-scope-controls".into())
                            .flex()
                            .flex_1()
                            .flex_basis(gpui::px(380.))
                            .min_w_0()
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .child(
                                Button::new("scope-all")
                                    .ghost()
                                    .small()
                                    .label(if active {
                                        "All changes"
                                    } else {
                                        "✓ All changes"
                                    })
                                    .disabled(busy)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.switch_review_scope(false, window, cx)
                                    })),
                            )
                            .child(
                                Button::new("scope-since-review")
                                    .ghost()
                                    .small()
                                    .label(if active {
                                        "✓ Changed since last review"
                                    } else {
                                        "Changed since last review"
                                    })
                                    .disabled(busy || data.review_changes.baseline().is_none())
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.switch_review_scope(true, window, cx)
                                    })),
                            )
                            .child(
                                // Keep this separate from the scope button so its tooltip
                                // remains available even when switching scope is disabled.
                                div()
                                    .id("review-scope-status")
                                    .debug_selector(|| "review-scope-status".into())
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .flex_shrink_0()
                                    .size(px(24.))
                                    .text_color(if icon == IconName::TriangleAlert {
                                        theme::yellow(cx)
                                    } else {
                                        theme::subtext(cx)
                                    })
                                    .tooltip(move |window, cx| {
                                        // Constrain the content too: Tooltip's flex child
                                        // can otherwise overflow the popup's max width.
                                        let detail = detail.clone();
                                        Tooltip::element(move |_, _| {
                                            div()
                                                .debug_selector(|| {
                                                    "review-scope-tooltip-text".into()
                                                })
                                                .max_w(px(440.))
                                                .whitespace_normal()
                                                .child(detail.clone())
                                        })
                                        .max_w(px(480.))
                                        .build(window, cx)
                                    })
                                    .child(Icon::new(icon).small()),
                            )
                    }))
                    .when(!data.shows_file_media(), |row| {
                        row.child(self.render_presentation_controls(data, cx))
                    }),
            );
        if data.omitted_untracked > 0 {
            bar = bar.child(
                div()
                    .debug_selector(|| "untracked-limit-warning".into())
                    .text_color(theme::yellow(cx))
                    .child(format!(
                        "Incomplete local changes: {} untracked files omitted (showing the first 200).",
                        data.omitted_untracked
                    )),
            );
        }
        if is_pr && active && data.diff.files.is_empty() {
            bar = bar.child(
                div()
                    .text_color(theme::subtext(cx))
                    .child("No changes since your last review."),
            );
        }
        bar.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_status_distinguishes_guidance_from_warnings_and_preserves_details() {
        let (icon, detail) = scope_status(&github::ReviewChanges::NotReviewed, false, false);
        assert_eq!(icon, IconName::Info);
        assert_eq!(
            detail,
            "Submit a GitHub review to use Changed since last review."
        );
        assert!(github::ReviewChanges::NotReviewed.baseline().is_none());

        for reason in [
            "History rewritten by a rebase or force-push",
            "Couldn't verify changes: network unavailable",
            "Couldn't load changes: diff unavailable",
        ] {
            let (icon, detail) = scope_status(
                &github::ReviewChanges::Unavailable(reason.into()),
                false,
                false,
            );
            assert_eq!(icon, IconName::TriangleAlert);
            assert_eq!(detail, reason);
        }
        assert_eq!(
            scope_status(&github::ReviewChanges::default(), false, false).0,
            IconName::TriangleAlert,
        );

        let review = github::ReviewChanges::Available(github::ReviewBaseline {
            commit: "abcdef0123456789".into(),
            submitted_at: "2026-03-03T00:00:00Z".into(),
        });
        let (icon, detail) = scope_status(&review, false, false);
        assert_eq!(icon, IconName::Info);
        assert_eq!(detail, "Last review: 2026-03-03T00:00:00Z · abcdef0");
        let (_, detail) = scope_status(&review, true, true);
        assert!(detail.contains(" · loading…"));
        assert!(detail.contains("Comments on the full PR's old revision remain in Conversation"));
        assert!(detail.contains("new comments use explicit revision positioning"));
    }
}
