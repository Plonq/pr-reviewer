//! Prose configuration and bounded wrapped-content sizing for Kit's Editor.
//! Mentions use ordinary LSP text edits, never inline/atomic tokens.

use crate::mentions::{mention_prefix, mention_rank};
use gpui::component::{input::CompletionProvider, Rope, RopeExt};
use gpui::component::{
    input::{Editor, EditorState, MoveDown, MoveUp, WrappingIndent},
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
        .scroll_bounce(false)
}

/// Multiline Kit inputs reset the caret to zero when assigning a value. Prose
/// being opened for editing/restoration should instead be ready to append.
pub(crate) fn set_value_at_end(
    state: &mut EditorState,
    value: impl Into<gpui::SharedString>,
    window: &mut Window,
    cx: &mut Context<EditorState>,
) {
    state.set_value(value.into(), window, cx);
    let end = state.text().len();
    state.set_selected_range(end..end, cx);
    // A fresh editor has no layout yet, so the synchronous selection cannot
    // reveal a long comment's tail. Retry after its first frame, but never
    // override navigation/selection the user has made in the meantime.
    let input = cx.entity().downgrade();
    window.on_next_frame(move |_, cx| {
        let _ = input.update(cx, |state, cx| {
            if state.text().len() == end
                && state.cursor() == end
                && state.selected_range().is_empty()
            {
                state.set_selected_range(end..end, cx);
            }
        });
    });
}

/// Let Kit perform normal visual-row movement first (including wrap affinity,
/// column tracking, selection collapse and completion navigation). Only a plain
/// arrow that could not move the caret gets the prose-style document-edge move.
fn finish_vertical_movement(
    input: &Entity<EditorState>,
    down: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let state = input.read(cx);
    if !state.selected_range().is_empty()
        || state.line_height().is_none()
        || state.completion_menu_state().open
        || state.code_action_menu_state().open
    {
        return;
    }
    let before = state.cursor();
    let target = if down { state.text().len() } else { 0 };
    if before == target {
        return;
    }
    let input = input.downgrade();
    window.defer(cx, move |_, cx| {
        if let Some(input) = input.upgrade() {
            input.update(cx, |state, cx| {
                if state.cursor() == before && state.selected_range().is_empty() {
                    state.set_selected_range(target..target, cx);
                }
            });
        }
    });
}

fn height(rows: usize, line_height: gpui::Pixels) -> gpui::Pixels {
    // Stock medium Editor padding plus its two one-pixel borders. Reserve
    // complete text rows rather than introducing a tiny internal scroll range.
    line_height * rows.clamp(3, 8) + Size::Medium.input_py() * 2. + px(2.)
}

#[cfg(test)]
pub(crate) fn field<T: 'static>(state: &Entity<EditorState>, cx: &mut Context<T>) -> AnyElement {
    field_with_media(state, None, false, cx)
}

pub(crate) fn field_with_media<T: 'static>(
    state: &Entity<EditorState>,
    uploads: Option<&Entity<crate::comment_media::Uploads>>,
    disabled: bool,
    cx: &mut Context<T>,
) -> AnyElement {
    // Match the original Input's text-sm size (14px with the default theme).
    let font_size = cx.theme().font_size * 0.875;
    let line_height = font_size * 1.5;
    let h = height(
        state.read(cx).display_map().display_row_count(),
        line_height,
    );
    let owner = cx.entity().entity_id();
    let editor = state.clone();
    let up_input = state.clone();
    let down_input = state.clone();
    let paste_input = state.downgrade();
    let drop_input = state.downgrade();
    let mut editor_element = Editor::new(state)
        .h(h)
        .disabled(disabled)
        .font_family(cx.theme().font_family.clone())
        .text_size(font_size)
        .line_height(line_height);
    if let Some(uploads) = uploads.filter(|_| !disabled) {
        let uploads = uploads.clone();
        editor_element = editor_element.on_paste(move |item, window, cx| {
            let sources = crate::comment_media::clipboard_sources(item);
            if sources.is_empty() {
                return false;
            }
            if let Some(input) = paste_input.upgrade() {
                crate::comment_media::start(&uploads, &input, sources, window, cx);
            }
            true
        });
    }
    let drop_color = crate::theme::surface0(cx);
    let field = div()
        .debug_selector(|| "comment-editor-field".into())
        .relative()
        .w_full()
        .h(h)
        .when(!disabled, |field| {
            field
                .capture_action(move |_: &MoveUp, window, cx| {
                    finish_vertical_movement(&up_input, false, window, cx);
                })
                .capture_action(move |_: &MoveDown, window, cx| {
                    finish_vertical_movement(&down_input, true, window, cx);
                })
        })
        .when_some(uploads.filter(|_| !disabled).cloned(), |field, uploads| {
            field
                .on_drop(move |paths: &gpui::ExternalPaths, window, cx| {
                    if let Some(input) = drop_input.upgrade() {
                        let sources = paths
                            .0
                            .iter()
                            .cloned()
                            .map(crate::comment_media::Source::File)
                            .collect();
                        crate::comment_media::start(&uploads, &input, sources, window, cx);
                    }
                })
                .drag_over::<gpui::ExternalPaths>(move |style, _, _, _| style.bg(drop_color))
        })
        .child(editor_element)
        .child(
            // Wrapping is calculated during layout. Settle on the next frame
            // only when the clamped height changes, not on every paint.
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
        );
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .child(field)
        .when_some(uploads, |field, uploads| {
            let blocked = crate::comment_media::blocks_submission(Some(uploads), state, cx);
            let uploads = uploads.read(cx);
            field
                .when(blocked && uploads.pending() == 0, |field| {
                    field.child(div().text_sm().text_color(crate::theme::red(cx)).child(
                        "Remove the restored upload placeholder before submitting. Paste/drop the media again if needed.",
                    ))
                })
                .when(uploads.pending() > 0, |field| {
                    field.child(
                        div()
                            .text_sm()
                            .text_color(crate::theme::subtext(cx))
                            .child(format!("Uploading {} attachment(s)…", uploads.pending())),
                    )
                })
                .when_some(uploads.error.clone(), |field, error| {
                    field.child(
                        div()
                            .text_sm()
                            .text_color(crate::theme::red(cx))
                            .child(error),
                    )
                })
        })
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
            cx.update(|window, cx| {
                window.draw(cx).clear(cx);
                window.simulate_next_frame(cx);
            });
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
    fn long_comments_stop_at_the_edges_without_bouncing(cx: &mut TestAppContext) {
        let (mut vcx, _, input) = fixture(cx);
        vcx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value("line\n".repeat(40), window, cx);
            });
        });
        settle(&mut vcx);
        let position = vcx.debug_bounds("comment-editor-field").unwrap().center();
        for delta in [120., -120.] {
            // A line-wheel packet reaches the edge without triggering rubber-band
            // displacement, even if the platform default is accidentally restored.
            vcx.simulate_event(gpui::ScrollEvent {
                position,
                delta: gpui::ScrollDelta::Lines(gpui::point(0., delta * 100.)),
                ..Default::default()
            });
            settle(&mut vcx);
            let before = vcx.update(|_, cx| {
                let state = input.read(cx);
                (state.text_bounds(), state.scroll_offset())
            });
            for phase in [gpui::TouchPhase::Started, gpui::TouchPhase::Moved] {
                vcx.simulate_event(gpui::ScrollEvent {
                    position,
                    delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(delta))),
                    touch_phase: phase,
                    ..Default::default()
                });
                settle(&mut vcx);
                vcx.update(|_, cx| {
                    let state = input.read(cx);
                    assert_eq!((state.text_bounds(), state.scroll_offset()), before);
                });
            }
            vcx.simulate_event(gpui::ScrollEvent {
                position,
                touch_phase: gpui::TouchPhase::Ended,
                ..Default::default()
            });
        }
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

    #[gpui::test]
    fn loaded_prose_is_ready_to_append_including_unicode_and_trailing_newlines(
        cx: &mut TestAppContext,
    ) {
        let (mut vcx, _, input) = fixture(cx);
        let long = "many lines\n".repeat(40);
        for text in [
            long.as_str(),
            "",
            "a comment",
            "é🙂\nmore",
            "trailing newline\n",
        ] {
            vcx.update(|window, cx| {
                input.update(cx, |state, cx| set_value_at_end(state, text, window, cx));
                assert_eq!(input.read(cx).selected_range(), text.len()..text.len());
            });
            settle(&mut vcx);
            vcx.update(|_, cx| {
                let state = input.read(cx);
                // Kit caches caret y in content space (x is already scrolled).
                let (mut caret, _) = state.cursor_layout().unwrap();
                caret.origin.y += state.scroll_offset().y;
                assert!(
                    state.input_bounds().intersects(&caret),
                    "end caret must be visible: len={}, caret={caret:?}, viewport={:?}, scroll={:?}",
                    text.len(), state.input_bounds(), state.scroll_offset()
                );
            });
            vcx.simulate_input("!");
            vcx.update(|_, cx| assert_eq!(input.read(cx).value().as_str(), format!("{text}!")));
        }
        vcx.update(|window, cx| {
            input.update(cx, |state, cx| {
                set_value_at_end(state, long, window, cx);
                state.set_selected_range(2..5, cx);
            })
        });
        settle(&mut vcx);
        vcx.update(|_, cx| assert_eq!(input.read(cx).selected_range(), 2..5));
    }

    #[gpui::test]
    fn vertical_arrows_reach_text_edges_without_changing_normal_movement_or_selection(
        cx: &mut TestAppContext,
    ) {
        let (mut vcx, _, input) = fixture(cx);
        let text = "first line\nfirst line\nlast é🙂";
        vcx.update(|window, cx| {
            input.update(cx, |state, cx| set_value_at_end(state, text, window, cx));
        });
        settle(&mut vcx);
        // At the first visual row, Up reaches the start, and Shift+Up still
        // selects to the start through Kit's normal selection implementation.
        vcx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_cursor_position(lsp_types::Position::new(0, 4), window, cx)
            })
        });
        vcx.simulate_keystrokes("shift-up");
        vcx.update(|_, cx| assert_eq!(input.read(cx).selected_range(), 0..4));
        vcx.simulate_keystrokes("right up");
        vcx.update(|_, cx| assert_eq!(input.read(cx).cursor(), 0));
        // A normal move still retains the visual column; it must not jump to
        // an edge simply because the logical buffer row is first/last.
        vcx.simulate_keystrokes("right right down");
        vcx.update(|_, cx| {
            assert_eq!(
                input.read(cx).cursor_position(),
                lsp_types::Position::new(1, 2)
            )
        });
        vcx.simulate_keystrokes("up");
        vcx.update(|_, cx| assert_eq!(input.read(cx).cursor(), 2));
        vcx.simulate_keystrokes("up up");
        vcx.update(|_, cx| assert_eq!(input.read(cx).cursor(), 0));
        vcx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_cursor_position(lsp_types::Position::new(2, 3), window, cx)
            })
        });
        vcx.simulate_keystrokes("shift-down");
        vcx.update(|_, cx| assert_eq!(input.read(cx).selected_range(), 25..text.len()));
        vcx.simulate_keystrokes("left down down");
        vcx.update(|_, cx| assert_eq!(input.read(cx).selected_range(), text.len()..text.len()));
        // The same behavior applies to a single visual row, including empty text.
        for text in ["single é🙂 line", ""] {
            vcx.update(|window, cx| {
                input.update(cx, |state, cx| {
                    set_value_at_end(state, text, window, cx);
                    state.set_selected_range(3.min(text.len())..3.min(text.len()), cx);
                })
            });
            settle(&mut vcx);
            vcx.simulate_keystrokes("up");
            vcx.update(|_, cx| assert_eq!(input.read(cx).cursor(), 0));
            vcx.simulate_keystrokes("down");
            vcx.update(|_, cx| assert_eq!(input.read(cx).cursor(), text.len()));
        }
    }

    #[gpui::test]
    fn vertical_edge_movement_respects_soft_wrapped_rows(cx: &mut TestAppContext) {
        let (mut vcx, view, input) = fixture(cx);
        vcx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.width = px(180.);
                cx.notify();
            });
            input.update(cx, |state, cx| {
                set_value_at_end(state, "wrapped prose words ".repeat(8), window, cx)
            });
        });
        settle(&mut vcx);
        let end = vcx.update(|_, cx| {
            assert!(input.read(cx).display_map().display_row_count() > 3);
            input.read(cx).cursor()
        });
        vcx.simulate_keystrokes("up");
        let previous = vcx.update(|_, cx| input.read(cx).cursor());
        assert!(
            previous > 0 && previous < end,
            "Up moves one visual row, not to the buffer-line start"
        );
        vcx.simulate_keystrokes("down");
        vcx.update(|_, cx| assert_eq!(input.read(cx).cursor(), end));
        vcx.simulate_keystrokes("cmd-left");
        for _ in 0..30 {
            vcx.simulate_keystrokes("up");
        }
        vcx.update(|_, cx| assert_eq!(input.read(cx).cursor(), 0));
        vcx.simulate_keystrokes("down");
        let next = vcx.update(|_, cx| input.read(cx).cursor());
        assert!(
            next > 0 && next < end,
            "Down traverses wraps before reaching the text end"
        );
        for _ in 0..30 {
            vcx.simulate_keystrokes("down");
        }
        vcx.update(|_, cx| assert_eq!(input.read(cx).cursor(), end));
    }

    #[gpui::test]
    fn vertical_arrows_navigate_mentions_instead_of_moving_the_caret(cx: &mut TestAppContext) {
        let (mut vcx, _, input) = fixture(cx);
        vcx.update(|_, cx| {
            candidates(
                &input,
                Rc::new(RefCell::new(vec![
                    github::Mention {
                        login: "albert".into(),
                        name: None,
                    },
                    github::Mention {
                        login: "alice".into(),
                        name: None,
                    },
                ])),
                cx,
            )
        });
        settle(&mut vcx);
        vcx.simulate_input("Hello @al");
        settle(&mut vcx);
        vcx.update(|_, cx| assert!(input.read(cx).completion_menu_state().open));
        vcx.simulate_keystrokes("down up down");
        vcx.update(|_, cx| assert_eq!(input.read(cx).cursor(), "Hello @al".len()));
        vcx.update(|window, cx| window.press("enter", cx));
        settle(&mut vcx);
        vcx.update(|_, cx| assert_eq!(input.read(cx).value().as_str(), "Hello @alice "));
    }

    #[test]
    fn height_is_bounded_by_three_to_eight_rows() {
        assert_eq!(height(0, px(20.)), px(78.));
        assert_eq!(height(5, px(20.)), px(118.));
        assert_eq!(height(100, px(20.)), px(178.));
        assert_eq!(height(5, px(24.)), px(138.));
    }
}
