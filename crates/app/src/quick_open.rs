//! Quick-open uses Kit's modal shell rather than taking up sidebar space.
use crate::{theme, InputEscape, OpenInput, ReviewApp, Source};
use gpui::component::{
    button::{Button, ButtonVariants as _},
    input::Input,
    Disableable as _, Sizable as _, WindowExt as _,
};
use gpui::{div, prelude::*, px, Context, Window};
use std::{cell::Cell, path::Path};

impl ReviewApp {
    pub(crate) fn open_quick_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.quick_open_visible {
            self.open_input
                .update(cx, |input, cx| input.focus(window, cx));
            return;
        }
        // Do not stack quick-open over another modal (for example a review).
        if window.has_active_dialog(cx) {
            return;
        }
        self.quick_open_visible = true;
        let owner = cx.entity();
        let initial_focus = Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            let content = owner.update(cx, |app, cx| {
                if initial_focus.replace(false) {
                    app.open_input
                        .update(cx, |input, cx| input.focus(window, cx));
                }
                app.render_quick_open(cx)
            });
            let closed = owner.clone();
            dialog
                .title("Open a pull request or repository")
                .w(px(560.))
                // The input event owns submission. Empty/invalid Enter must
                // not trigger the shell's default confirm-and-close behavior.
                .on_ok(|_, _, _| false)
                .on_close(move |_, _, cx| {
                    closed.update(cx, |app, cx| {
                        app.quick_open_visible = false;
                        cx.notify();
                    });
                })
                .child(content)
        });
        cx.notify();
    }

    fn close_quick_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.quick_open_visible {
            self.quick_open_visible = false;
            // Kit restores the focus that was active before the modal opened.
            window.close_dialog(cx);
            cx.notify();
        }
    }

    fn render_quick_open(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        div()
            .debug_selector(|| "quick-open-dialog".into())
            .flex()
            .flex_col()
            .pt(px(4.))
            .gap_3()
            // Modal layers aren't descendants of the app's action handlers.
            .on_action(cx.listener(|app, _: &OpenInput, window, cx| {
                app.open_quick_open(window, cx);
            }))
            .on_action(cx.listener(|app, _: &InputEscape, window, cx| {
                cx.stop_propagation();
                app.close_quick_open(window, cx);
            }))
            .child(Input::new(&self.open_input))
            .when_some(self.open_error.clone(), |body, error| {
                body.child(
                    div()
                        .debug_selector(|| "quick-open-error".into())
                        .text_sm()
                        .text_color(theme::red(cx))
                        .child(error),
                )
            })
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        div().debug_selector(|| "quick-open-cancel".into()).child(
                            Button::new("quick-open-cancel")
                                .label("Cancel")
                                .ghost()
                                .small()
                                .on_click(cx.listener(|app, _, window, cx| {
                                    app.close_quick_open(window, cx);
                                })),
                        ),
                    )
                    .child(
                        Button::new("quick-open-submit")
                            .label("Open")
                            .primary()
                            .small()
                            .disabled(self.open_input.read(cx).value().trim().is_empty())
                            .on_click(cx.listener(|app, _, window, cx| {
                                app.submit_open(window, cx);
                            })),
                    ),
            )
            .into_any_element()
    }

    pub(crate) fn submit_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.open_input.read(cx).value().trim().to_string();
        if value.is_empty() {
            return;
        }
        let parsed = if Path::new(&value).is_dir() {
            git::resolve_local(Path::new(&value))
                .map(Source::Local)
                .map_err(|err| format!("{err:#}"))
        } else {
            self.github
                .resolve_pr(&value)
                .map(Source::Pr)
                .map_err(|err| err.to_string())
        };
        match parsed {
            Ok(source) => {
                self.open_error = None;
                self.open_input
                    .update(cx, |state, cx| state.set_value("", window, cx));
                self.open_item(source, cx);
                self.close_quick_open(window, cx);
                window.focus(&self.focus_handle, cx);
            }
            Err(message) => self.open_error = Some(message.into()),
        }
        cx.notify();
    }
}
