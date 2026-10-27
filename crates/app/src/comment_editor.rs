//! Prose configuration and bounded wrapped-content sizing for Kit's Editor.
//! Mentions use ordinary LSP text edits, never inline/atomic tokens.

use gpui::component::{
    input::{Editor, EditorState, WrappingIndent},
    ActiveTheme,
};
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
        .scroll_beyond_last_line(None)
}

fn height(rows: usize, line_height: gpui::Pixels) -> gpui::Pixels {
    line_height * rows.clamp(3, 8) + px(16.)
}

pub(crate) fn field<T: 'static>(state: &Entity<EditorState>, cx: &mut Context<T>) -> AnyElement {
    let line_height = cx.theme().font_size * 1.5;
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
        .relative()
        .w_full()
        .h(h)
        .child(
            Editor::new(state)
                .h(h)
                .font_family(cx.theme().font_family.clone())
                .text_size(cx.theme().font_size)
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

    #[test]
    fn height_is_bounded_by_three_to_eight_rows() {
        assert_eq!(height(0, px(20.)), px(76.));
        assert_eq!(height(5, px(20.)), px(116.));
        assert_eq!(height(100, px(20.)), px(176.));
        assert_eq!(height(5, px(24.)), px(136.));
    }
}
