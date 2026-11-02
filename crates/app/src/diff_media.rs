//! Inline comment Markdown and images decorate reserved uniform-list rows.
//! Keeping the row height uniform preserves selection, anchors, hunk navigation,
//! saved positions and the minimap. Decorations are clipped/scrolled by the list
//! itself, so partially visible content survives virtualization of its top row.

use std::{collections::HashMap, ops::Range, sync::Arc};

use gpui::component::ActiveTheme;
use gpui::{
    div, px, size, App, AvailableSpace, Bounds, Context, Entity, InteractiveElement, IntoElement,
    MouseButton, ParentElement, Pixels, Point, Styled, UniformListDecoration, Window,
};

use crate::{
    conversation, row_height,
    rows::{BuiltRows, Row},
    text_size, theme,
    threads::{CommentSide, COMMENT_WRAP_CHARS, REPLY_INDENT},
    ItemPane, ItemState, ReviewApp, Source, SPLIT_DIVIDER,
};

pub(crate) struct CommentText {
    pub comment_id: u64,
    pub is_reply: bool,
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

fn reply_indent(is_reply: bool) -> Pixels {
    px(if is_reply { REPLY_INDENT } else { 0. })
}

fn text_width(card_width: Pixels, char_width: Pixels, is_reply: bool) -> Pixels {
    // Gutter + borders + px_3 padding, with the existing prose width cap.
    (card_width - px(99.) - reply_indent(is_reply))
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
        .font_family(cx.theme().font_family.clone())
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
                                || layout.width != text_width(card_width, char_width, text.is_reply)
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
            let width = text_width(card_width, char_width, text.is_reply);
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
                let available = text_width(
                    width,
                    app.char_width.unwrap_or(px(text_size() * 0.6)),
                    text.is_reply,
                );
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
                let owner = self.0.clone();
                let item_id = item.id;
                let key = text.key();
                let source = text.source.clone();
                let measured_width = layout.width;
                let measured_font = layout.font_size;
                let reserved = layout.rows;
                layer = layer.child(
                    div()
                        .absolute()
                        .left(x - scroll_offset.x + px(86.) + reply_indent(text.is_reply))
                        .top(item_height * start + px(4.))
                        .w(layout.width.min(available))
                        .h(item_height * layout.rows - px(8.))
                        .overflow_hidden()
                        .debug_selector({
                            let id = id.clone();
                            move || format!("{id}-card")
                        })
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        // Images resolve asynchronously inside stock TextView.
                        // Observe its outer content geometry, not its block or
                        // media internals, and resize only the matching source.
                        .on_children_prepainted(move |bounds, window, cx| {
                            let Some(content) = bounds.first() else {
                                return;
                            };
                            let rows = ((f32::from(content.size.height) + 8.) / row_height())
                                .ceil()
                                .max(1.) as usize;
                            if rows == reserved {
                                return;
                            }
                            let owner = owner.clone();
                            let source = source.clone();
                            window.defer(cx, move |_, cx| {
                                owner.update(cx, |app, cx| {
                                    let Some(ItemState::Ready(data)) = app
                                        .items
                                        .iter_mut()
                                        .find(|item| item.id == item_id)
                                        .map(|item| &mut item.state)
                                    else {
                                        return;
                                    };
                                    let Some(layout) = data.comment_text_layouts.get_mut(&key)
                                    else {
                                        return;
                                    };
                                    if layout.source != source
                                        || layout.width != measured_width
                                        || layout.font_size != measured_font
                                        || layout.rows == rows
                                    {
                                        return;
                                    }
                                    layout.rows = rows;
                                    data.rebuild_rows_anchored();
                                    cx.notify();
                                });
                            });
                        })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_width_accounts_for_indent_before_applying_the_prose_cap() {
        let root = text_width(px(400.), px(8.), false);
        let reply = text_width(px(400.), px(8.), true);
        assert_eq!(root - reply, px(REPLY_INDENT));
        assert_eq!(text_width(px(2000.), px(8.), true), px(640.));
        assert_eq!(text_width(px(80.), px(8.), true), px(1.));
    }

    #[test]
    fn measured_reservations_remap_navigation_and_ignore_stale_sources() {
        for measured_rows in [1, 6] {
            let text = Arc::new(CommentText {
                comment_id: 7,
                is_reply: false,
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
