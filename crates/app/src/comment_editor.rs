//! Prose configuration and bounded wrapped-content sizing for Kit's Editor.
//! Mentions use ordinary LSP text edits, never inline/atomic tokens.

use crate::mentions::{mention_prefix, mention_rank};
use gpui::component::{input::CompletionProvider, Rope, RopeExt};
use gpui::component::{
    input::{Editor, EditorState, WrappingIndent},
    ActiveTheme, Size,
};
use gpui::{App, Task};
use std::{cell::RefCell, rc::Rc};

struct MentionProvider {
    users: Rc<RefCell<Vec<github::Mention>>>,
}

pub(crate) fn candidates(
    state: &Entity<EditorState>,
    users: Rc<RefCell<Vec<github::Mention>>>,
    cx: &mut App,
) {
    state.update(cx, |state, _| {
        state.lsp_mut().completion_provider = Some(Rc::new(MentionProvider { users }));
    });
}

impl CompletionProvider for MentionProvider {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        _: lsp_types::CompletionContext,
        _: &mut Window,
        _: &mut App,
    ) -> Task<anyhow::Result<lsp_types::CompletionResponse>> {
        let Some((at, prefix)) = mention_prefix(text, offset) else {
            return Task::ready(Ok(lsp_types::CompletionResponse::Array(vec![])));
        };
        let range = lsp_types::Range {
            start: text.offset_to_position(at),
            end: text.offset_to_position(offset),
        };
        let users = self.users.borrow();
        let mut ranked: Vec<_> = users
            .iter()
            .filter_map(|u| mention_rank(u, &prefix).map(|rank| (rank, u)))
            .collect();
        ranked.sort_by_key(|(rank, _)| *rank);
        let items = ranked
            .into_iter()
            .take(50)
            .map(|(_, u)| lsp_types::CompletionItem {
                label: u.login.clone(),
                filter_text: Some(u.login.clone()),
                detail: u.name.clone(),
                text_edit: Some(lsp_types::CompletionTextEdit::Edit(lsp_types::TextEdit {
                    range,
                    new_text: format!("@{} ", u.login),
                })),
                ..Default::default()
            })
            .collect();
        Task::ready(Ok(lsp_types::CompletionResponse::Array(items)))
    }
    fn is_completion_trigger(&self, _: usize, _: &str, _: &mut App) -> bool {
        true
    }
}
use gpui::{canvas, div, prelude::*, px, AnyElement, Context, Entity, Window};

pub(crate) fn new(window: &mut Window, cx: &mut Context<EditorState>) -> EditorState {
    EditorState::new(window, cx)
        .line_number(false)
        .folding(false)
        .indent_guides(false)
        .searchable(false)
        .auto_close(false)
        .smart_indent(false)
        .soft_wrap(true)
        .wrapping_indent(WrappingIndent::None)
        .scroll_beyond_last_line(Some(0))
}

fn height(rows: usize, line_height: gpui::Pixels) -> gpui::Pixels {
    // Stock medium Editor padding plus its two one-pixel borders. Reserve
    // complete text rows rather than introducing a tiny internal scroll range.
    line_height * rows.clamp(3, 8) + Size::Medium.input_py() * 2. + px(2.)
}

pub(crate) fn field<T: 'static>(state: &Entity<EditorState>, cx: &mut Context<T>) -> AnyElement {
    // Match the original Input's text-sm size (14px with the default theme).
    let font_size = cx.theme().font_size * 0.875;
    let line_height = font_size * 1.5;
    let h = height(
        state.read(cx).display_map().display_row_count(),
        line_height,
    );
    let owner = cx.entity().entity_id();
    let editor = state.clone();
    // Wrapping is calculated by the stock editor during layout. If its actual
    // wrapped row count changed (typing, width or font), settle on the next
    // frame. Only notify when the clamped height differs, not on every paint.
    div()
        .debug_selector(|| "comment-editor-field".into())
        .relative()
        .w_full()
        .h(h)
        .child(
            Editor::new(state)
                .h(h)
                .font_family(cx.theme().font_family.clone())
                .text_size(font_size)
                .line_height(line_height),
        )
        .child(
            canvas(
                |_, _, _| (),
                move |_, _, _, cx| {
                    if height(
                        editor.read(cx).display_map().display_row_count(),
                        line_height,
                    ) != h
                    {
                        cx.notify(owner);
                    }
                },
            )
            .absolute()
            .size_full(),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::test::TestWindowExt;
    use gpui::{Focusable, TestAppContext, VisualTestContext};

    struct Field {
        input: Entity<EditorState>,
        width: gpui::Pixels,
    }

    impl Render for Field {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div().w(self.width).child(field(&self.input, cx))
        }
    }

    fn fixture(cx: &mut TestAppContext) -> (VisualTestContext, Entity<Field>, Entity<EditorState>) {
        cx.update(gpui::component::init);
        let mut input = None;
        let window = cx.add_window(|window, cx| {
            let state = cx.new(|cx| new(window, cx));
            state.update(cx, |state, cx| state.focus(window, cx));
            input = Some(state.clone());
            Field {
                input: state,
                width: px(600.),
            }
        });
        let view = window.root(cx).unwrap();
        (
            VisualTestContext::from_window(window.into(), cx),
            view,
            input.unwrap(),
        )
    }

    fn settle(cx: &mut VisualTestContext) -> gpui::Pixels {
        for _ in 0..4 {
            cx.update(|window, cx| window.draw(cx).clear(cx));
            cx.run_until_parked();
        }
        cx.debug_bounds("comment-editor-field").unwrap().size.height
    }

    #[gpui::test]
    fn field_grows_with_wrapping_and_reflows_when_resized(cx: &mut TestAppContext) {
        let (mut vcx, view, input) = fixture(cx);
        let minimum = settle(&mut vcx);
        vcx.update(|_, cx| {
            assert_eq!(input.read(cx).line_height(), Some(px(21.)));
            assert_eq!(
                input.read(cx).input_bounds().size.height,
                input.read(cx).line_height().unwrap() * 3.
            )
        });
        vcx.simulate_input(&"prose words ".repeat(24));
        let wide = settle(&mut vcx);
        vcx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.width = px(180.);
                cx.notify();
            })
        });
        let narrow = settle(&mut vcx);
        assert!(narrow > wide && wide >= minimum);
        vcx.simulate_input(&"\nmore text".repeat(30));
        let maximum = settle(&mut vcx);
        assert!(maximum >= narrow);
        vcx.update(|window, cx| {
            let line_height = input.read(cx).line_height().unwrap();
            assert_eq!(maximum, height(8, line_height));
            input.update(cx, |input, cx| input.set_value("short", window, cx));
        });
        assert_eq!(settle(&mut vcx), minimum);
        vcx.update(|window, cx| {
            gpui::component::Theme::global_mut(cx).font_size = px(24.);
            window.refresh();
        });
        assert_eq!(settle(&mut vcx), height(3, px(31.5)));
    }

    #[gpui::test]
    fn mention_acceptance_is_plain_text_with_clipboard_and_undo(cx: &mut TestAppContext) {
        let (mut vcx, _, input) = fixture(cx);
        vcx.update(|_, cx| {
            candidates(
                &input,
                Rc::new(RefCell::new(vec![github::Mention {
                    login: "alice".into(),
                    name: Some("Alice Example".into()),
                }])),
                cx,
            )
        });
        settle(&mut vcx);
        vcx.simulate_input("Hello @al");
        settle(&mut vcx);
        // Native control keys do not send simulated IME newline text after
        // Kit intentionally propagates a completion action.
        vcx.update(|window, cx| window.press("enter", cx));
        settle(&mut vcx);
        vcx.update(|window, cx| {
            assert_eq!(input.read(cx).value().as_str(), "Hello @alice ");
            assert!(input.focus_handle(cx).is_focused(window));
        });
        // Delete a single character in the accepted login, not the whole mention.
        vcx.simulate_keystrokes("left backspace");
        vcx.update(|_, cx| assert_eq!(input.read(cx).value().as_str(), "Hello @alic "));
        vcx.simulate_keystrokes("cmd-z");
        vcx.update(|_, cx| assert_eq!(input.read(cx).value().as_str(), "Hello @alice "));
        vcx.simulate_keystrokes("cmd-a cmd-c right cmd-v");
        vcx.update(|_, cx| {
            assert_eq!(
                input.read(cx).value().as_str(),
                "Hello @alice Hello @alice "
            )
        });
    }

    #[test]
    fn height_is_bounded_by_three_to_eight_rows() {
        assert_eq!(height(0, px(20.)), px(78.));
        assert_eq!(height(5, px(20.)), px(118.));
        assert_eq!(height(100, px(20.)), px(178.));
        assert_eq!(height(5, px(24.)), px(138.));
    }
}
