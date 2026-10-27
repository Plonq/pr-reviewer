//! Inline comment Markdown and images decorate reserved uniform-list rows.
//! Keeping the row height uniform preserves selection, anchors, hunk navigation,
//! saved positions and the minimap. Decorations are clipped/scrolled by the list
//! itself, so partially visible content survives virtualization of its top row.

use std::{collections::HashMap, ops::Range, sync::Arc};

use gpui::{
    div, px, size, App, AvailableSpace, Bounds, Context, Entity, InteractiveElement, IntoElement,
    MouseButton, ParentElement, Pixels, Point, Styled, UniformListDecoration, Window,
};

use crate::{
    conversation, row_height,
    rows::{BuiltRows, Row, COMMENT_MEDIA_ROWS},
    text_size, theme,
    threads::{CommentSide, COMMENT_WRAP_CHARS},
    ItemPane, ItemState, ReviewApp, Source, MONO, SPLIT_DIVIDER,
};

pub(crate) struct CommentText {
    pub comment_id: u64,
    pub part: usize,
    pub source: String,
}

impl CommentText {
    fn key(&self) -> (u64, usize) {
        (self.comment_id, self.part)
    }

    fn id(&self, item_id: u64) -> String {
        format!("inline-text-{item_id}-{}-{}", self.comment_id, self.part)
    }
}

pub(crate) struct TextLayout {
    source: String,
    rendered: String,
    width: Pixels,
    font_size: f32,
    rows: usize,
}

pub(crate) type TextLayouts = HashMap<(u64, usize), TextLayout>;

fn text_width(card_width: Pixels, char_width: Pixels) -> Pixels {
    // Gutter + borders + px_3 padding, with the existing prose width cap.
    (card_width - px(99.))
        .max(px(1.))
        .min(char_width * COMMENT_WRAP_CHARS)
}

/// Replace initial plain-text estimates with measured Markdown reservations,
/// mapping navigation indices along with them. Source lines stay untouched.
pub(crate) fn resize_text_rows(built: &mut BuiltRows, layouts: &TextLayouts) {
    if layouts.is_empty() {
        return;
    }
    let old = std::mem::take(&mut built.rows);
    let mut mapping = vec![0; old.len() + 1];
    let mut rows = Vec::with_capacity(old.len());
    let mut old = old.into_iter().enumerate().peekable();
    while let Some((ix, row)) = old.next() {
        mapping[ix] = rows.len();
        if let Row::CommentBody {
            line, half, text, ..
        } = row
        {
            let start = rows.len();
            let mut lines = vec![line];
            while old.peek().is_some_and(|(_, row)| {
                matches!(row, Row::CommentBody { text: next, .. } if Arc::ptr_eq(&text, next))
            }) {
                let (_, Row::CommentBody { line, .. }) = old.next().unwrap() else {
                    unreachable!()
                };
                lines.push(line);
            }
            let count = layouts
                .get(&text.key())
                .filter(|layout| layout.source == text.source)
                .map_or(lines.len(), |layout| layout.rows);
            for slot in 0..lines.len() {
                mapping[ix + slot] = start + slot.min(count - 1);
            }
            let mut lines = lines.into_iter();
            for slot in 0..count {
                rows.push(Row::CommentBody {
                    line: lines.next().unwrap_or_default(),
                    half,
                    text: text.clone(),
                    slot,
                });
            }
        } else {
            rows.push(row);
        }
    }
    *mapping.last_mut().unwrap() = rows.len();
    for ix in built.file_rows.iter_mut().chain(&mut built.hunk_rows) {
        *ix = mapping[*ix];
    }
    built.rows = rows;
}

fn text_content(
    id: &str,
    source: String,
    width: Pixels,
    window: &mut Window,
    cx: &mut Context<ReviewApp>,
) -> gpui::AnyElement {
    div()
        .w(width)
        .debug_selector({
            let id = id.to_owned();
            move || format!("{id}-content")
        })
        .font_family(MONO)
        .text_size(px(text_size()))
        .line_height(px(row_height()))
        .text_color(theme::subtext(cx))
        .child(conversation::render_markdown(id, source, window, cx))
        .into_any_element()
}

impl ReviewApp {
    /// Measure only when source, width or zoom changes. This runs before the
    /// list is built, so even tall code blocks/lists have enough uniform slots
    /// on their first painted frame, without changing source-line geometry.
    pub(crate) fn resync_inline_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let char_width = self.char_width(window);
        let Some(item) = self.active_item() else {
            return;
        };
        if self.show_queue || item.pane != ItemPane::Changes {
            return;
        }
        let ItemState::Ready(data) = &item.state else {
            return;
        };
        let list_width = data.scroll.0.borrow().base_handle.bounds().size.width;
        if list_width <= px(0.) {
            return;
        }
        let card_width = if data.mode == crate::rows::ViewMode::Split {
            (list_width - px(SPLIT_DIVIDER)) / 2.
        } else {
            list_width
        };
        let width = text_width(card_width, char_width);
        let slug = match &item.source {
            Source::Pr(loc) => loc.repo_slug(),
            Source::Local(_) => String::new(),
        };
        let item_id = item.id;
        let texts: Vec<_> = data
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::CommentBody { text, slot: 0, .. }
                    if data
                        .comment_text_layouts
                        .get(&text.key())
                        .is_none_or(|layout| {
                            layout.source != text.source
                                || layout.width != width
                                || layout.font_size != text_size()
                        }) =>
                {
                    Some(text.clone())
                }
                _ => None,
            })
            .collect();
        if texts.is_empty() {
            return;
        }
        let mut layouts = Vec::new();
        for text in texts {
            let rendered = if slug.is_empty() {
                text.source.clone()
            } else {
                conversation::preprocess(&text.source, &slug)
            };
            let mut element = text_content(
                &format!("{}-measure", text.id(item_id)),
                rendered.clone(),
                width,
                window,
                cx,
            );
            let height = element
                .layout_as_root(
                    size(AvailableSpace::Definite(width), AvailableSpace::MaxContent),
                    window,
                    cx,
                )
                .height;
            layouts.push((
                text.key(),
                TextLayout {
                    source: text.source.clone(),
                    rendered,
                    width,
                    font_size: text_size(),
                    rows: ((f32::from(height) + 8.) / row_height()).ceil().max(1.) as usize,
                },
            ));
        }
        let data = self.active_data_mut().unwrap();
        data.comment_text_layouts.extend(layouts);
        data.rebuild_rows_anchored();
    }
}

pub(crate) struct InlineText(pub Entity<ReviewApp>);

impl UniformListDecoration for InlineText {
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
        self.0.update(cx, |app, cx| {
            let mut layer = div().relative().w(bounds.size.width).h(bounds.size.height);
            let Some(item) = app.active_item() else {
                return layer.into_any_element();
            };
            let ItemState::Ready(data) = &item.state else {
                return layer.into_any_element();
            };
            let mut last_start = None;
            for ix in visible {
                let Some(Row::CommentBody {
                    text, slot, half, ..
                }) = data.rows.get(ix)
                else {
                    continue;
                };
                let start = ix - slot;
                if last_start == Some(start) {
                    continue;
                }
                last_start = Some(start);
                let Some(layout) = data.comment_text_layouts.get(&text.key()) else {
                    // The first layout populates the new list's bounds. Ask for
                    // the next frame to measure against those actual bounds.
                    cx.notify();
                    continue;
                };
                let width = match half {
                    None => bounds.size.width,
                    Some(_) => (bounds.size.width - px(SPLIT_DIVIDER)) / 2.,
                };
                let available = text_width(width, app.char_width.unwrap_or(px(text_size() * 0.6)));
                if layout.width != available {
                    // A resize updates list bounds after the app renders.
                    // Remeasure on the next frame and clip to this pane meanwhile.
                    cx.notify();
                }
                let x = match half {
                    Some(CommentSide::Right) => width + px(SPLIT_DIVIDER),
                    _ => px(0.),
                };
                let id = text.id(item.id);
                layer = layer.child(
                    div()
                        .absolute()
                        .left(x - scroll_offset.x + px(86.))
                        .top(item_height * start + px(4.))
                        .w(layout.width.min(available))
                        .h(item_height * layout.rows - px(8.))
                        .overflow_hidden()
                        .debug_selector({
                            let id = id.clone();
                            move || format!("{id}-card")
                        })
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(text_content(
                            &id,
                            layout.rendered.clone(),
                            layout.width,
                            window,
                            cx,
                        )),
                );
            }
            layer.into_any_element()
        })
    }
}

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
                    .bg(theme::comment_bg(cx))
                    .border_l_2()
                    .border_r_1()
                    .border_color(theme::comment_outline(cx))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measured_reservations_remap_navigation_and_ignore_stale_sources() {
        for measured_rows in [1, 6] {
            let text = Arc::new(CommentText {
                comment_id: 7,
                part: 0,
                source: "**unwrapped café**\n\n```rust\nlet x = 1;\n```".into(),
            });
            let build = || BuiltRows {
                rows: vec![
                    Row::Binary,
                    Row::CommentBody {
                        line: "estimate one".into(),
                        half: None,
                        text: text.clone(),
                        slot: 0,
                    },
                    Row::CommentBody {
                        line: "estimate two".into(),
                        half: None,
                        text: text.clone(),
                        slot: 1,
                    },
                    Row::Binary,
                ],
                file_rows: vec![0],
                hunk_rows: vec![3],
            };
            let mut layouts = TextLayouts::new();
            layouts.insert(
                text.key(),
                TextLayout {
                    source: text.source.clone(),
                    rendered: text.source.clone(),
                    width: px(300.),
                    font_size: text_size(),
                    rows: measured_rows,
                },
            );
            let mut built = build();
            resize_text_rows(&mut built, &layouts);
            assert_eq!(built.rows.len(), measured_rows + 2);
            assert_eq!(built.file_rows, [0]);
            assert_eq!(built.hunk_rows, [measured_rows + 1]);
            for (slot, row) in built.rows[1..=measured_rows].iter().enumerate() {
                let Row::CommentBody {
                    text: actual,
                    slot: actual_slot,
                    ..
                } = row
                else {
                    panic!("expected reservation")
                };
                assert_eq!(*actual_slot, slot);
                assert!(Arc::ptr_eq(actual, &text), "source is never soft-wrapped");
                assert!(crate::rows::is_comment_row(row));
                assert_eq!(crate::rows::cursor_anchor(&built.rows, slot + 1), None);
            }
            layouts.get_mut(&text.key()).unwrap().source = "old body".into();
            let mut built = build();
            resize_text_rows(&mut built, &layouts);
            assert_eq!(built.rows.len(), 4);
            assert_eq!(built.hunk_rows, [3]);
        }
    }
}
