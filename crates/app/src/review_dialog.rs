//! Review state lives in the application/session, not in a dialog builder.
//! Kit owns the modal shell, layering and focus restoration.
use crate::{comment_editor, theme, InputEnter, InputEscape, ReviewApp, SubmitReview};
use gpui::component::{
    button::{Button, ButtonVariants},
    Disableable, Selectable, Sizable,
};
use gpui::{div, prelude::*, Context};

impl ReviewApp {
    pub(crate) fn render_review(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(review) = &self.review else {
            return div().into_any_element();
        };
        let selected = review.verdict;
        let busy = review.in_flight;
        let verdict = |id, label, value| {
            Button::new(id)
                .label(label)
                .small()
                .ghost()
                .selected(selected == value)
                .disabled(busy)
                .on_click(cx.listener(move |app, _, _, cx| {
                    if let Some(review) = &mut app.review {
                        review.verdict = value;
                        review.error = None;
                        cx.notify();
                    }
                }))
        };
        let submit_label = match selected {
            github::ReviewVerdict::Approve => "Approve",
            github::ReviewVerdict::RequestChanges => "Request changes",
            github::ReviewVerdict::Comment => "Comment",
        };
        div()
            .debug_selector(|| "review-dialog-content".into())
            .flex()
            .flex_col()
            .gap_3()
            // Kit layers are not descendants of the app content. Submission
            // must be mounted here, before Editor sees Enter/completion.
            .capture_action(cx.listener(|app, action: &InputEnter, window, cx| {
                if action.secondary {
                    cx.stop_propagation();
                    app.submit_review(window, cx);
                }
            }))
            .on_action(cx.listener(|app, _: &SubmitReview, window, cx| {
                app.submit_review(window, cx);
            }))
            .on_action(cx.listener(|app, _: &InputEscape, window, cx| {
                if !app.review.as_ref().is_some_and(|review| review.in_flight) {
                    app.close_review(window, cx);
                }
            }))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(verdict(
                        "approve",
                        "Approve",
                        github::ReviewVerdict::Approve,
                    ))
                    .child(verdict(
                        "request-changes",
                        "Request changes",
                        github::ReviewVerdict::RequestChanges,
                    ))
                    .child(verdict(
                        "comment",
                        "Comment",
                        github::ReviewVerdict::Comment,
                    )),
            )
            .child(comment_editor::field(&review.input, cx))
            .when_some(review.error.clone(), |body, error| {
                body.child(div().text_color(theme::red(cx)).child(error))
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .text_color(theme::subtext(cx))
                            .child("⌘⏎ to submit"),
                    )
                    .child(
                        Button::new("review-cancel")
                            .label("Cancel")
                            .ghost()
                            .small()
                            .disabled(busy)
                            .on_click(
                                cx.listener(|app, _, window, cx| app.close_review(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("review-submit")
                            .label(submit_label)
                            .primary()
                            .small()
                            .disabled(busy)
                            .loading(busy)
                            .on_click(
                                cx.listener(|app, _, window, cx| app.submit_review(window, cx)),
                            ),
                    ),
            )
            .into_any_element()
    }
}
