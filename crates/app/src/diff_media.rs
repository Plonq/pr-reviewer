//! Inline comment images are decorations over reserved uniform-list rows.
//! Keeping the row height uniform preserves selection, anchors, hunk navigation,
//! saved positions and the minimap. Decorations are clipped/scrolled by the list
//! itself, so a partially visible image survives virtualization of its top row.

use std::ops::Range;

use gpui::{
    div, px, App, Bounds, Entity, InteractiveElement, IntoElement, MouseButton, ParentElement,
    Pixels, Point, Styled, UniformListDecoration, Window,
};

use crate::{
    conversation,
    rows::{Row, COMMENT_MEDIA_ROWS},
    theme,
    threads::CommentSide,
    ItemState, ReviewApp, SPLIT_DIVIDER,
};

pub(crate) struct InlineMedia(pub Entity<ReviewApp>);

impl UniformListDecoration for InlineMedia {
    fn compute(
        &self,
        visible: Range<usize>,
        bounds: Bounds<Pixels>,
        scroll_offset: Point<Pixels>,
        item_height: Pixels,
        _item_count: usize,
        window: &mut Window,
        cx: &mut App,
    ) -> gpui::AnyElement {
        let mut cards = Vec::new();
        let mut last_start = None;
        let item_id = {
            let app = self.0.read(cx);
            let Some(item) = app.active_item() else {
                return div().into_any_element();
            };
            let ItemState::Ready(data) = &item.state else {
                return div().into_any_element();
            };
            // Only inspect visible slots, not every source row in a large diff.
            for ix in visible {
                if let Some(Row::CommentMedia {
                    comment_id,
                    image_ix,
                    media,
                    slot,
                    half,
                }) = data.rows.get(ix)
                {
                    let start = ix - slot;
                    if last_start == Some(start) {
                        continue;
                    }
                    last_start = Some(start);
                    cards.push((start, *comment_id, *image_ix, media.clone(), *half));
                }
            }
            item.id
        };
        let mut layer = div().relative().w(bounds.size.width).h(bounds.size.height);
        for (start, comment_id, image_ix, media, half) in cards {
            let width = match half {
                None => bounds.size.width,
                Some(_) => (bounds.size.width - px(SPLIT_DIVIDER)) / 2.,
            };
            let x = match half {
                Some(CommentSide::Right) => width + px(SPLIT_DIVIDER),
                _ => px(0.),
            };
            let height = item_height * COMMENT_MEDIA_ROWS;
            let id = format!("inline-comment-{item_id}-{comment_id}-{image_ix}");
            layer = layer.child(
                div()
                    .absolute()
                    // Decorations' origin already includes both scroll offsets.
                    // Like split code gutters, media stays inside its viewport.
                    .left(x - scroll_offset.x + px(72.))
                    .top(item_height * start)
                    .w((width - px(72.)).max(px(0.)))
                    .h(height)
                    .bg(theme::comment_bg())
                    .border_l_2()
                    .border_r_1()
                    .border_color(theme::comment_outline())
                    .px_3()
                    .py_1()
                    .overflow_hidden()
                    .debug_selector({
                        let id = id.clone();
                        move || format!("{id}-card")
                    })
                    // Image/source interactions must not start code selection or
                    // move the diff cursor to a media placeholder.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(conversation::render_inline(
                        &id,
                        &media,
                        f32::from(height) - 8.,
                        window,
                        cx,
                    )),
            );
        }
        layer.into_any_element()
    }
}
