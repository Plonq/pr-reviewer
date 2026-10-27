//! A thin stock Markdown host. Parsing, block/media layout and selection belong
//! to Kit; GitHub preprocessing is performed by callers before reaching here.
use crate::ReviewApp;
use gpui::component::text::TextView;
use gpui::{div, prelude::*, App, Context, SharedString, Window};

/// Whitespace and HTML-only metadata do not warrant a review summary card.
pub(super) fn has_content(mut body: &str) -> bool {
    loop {
        body = body.trim();
        let Some(rest) = body.strip_prefix("<!--") else {
            return !body.is_empty();
        };
        let Some(end) = rest.find("-->") else {
            return false;
        };
        body = &rest[end + 3..];
    }
}

pub(crate) fn render(
    id: &str,
    source: String,
    window: &mut Window,
    cx: &mut Context<ReviewApp>,
) -> gpui::AnyElement {
    text_view(id, source, window, cx)
}

pub(super) fn text_view(id: &str, source: String, _: &mut Window, _: &mut App) -> gpui::AnyElement {
    div()
        .w_full()
        .min_w_0()
        .flex_shrink_0()
        .debug_selector({
            let id = id.to_owned();
            move || format!("{id}-text")
        })
        .child(
            TextView::markdown(SharedString::from(id.to_owned()), source)
                .w_full()
                .h_auto()
                .scrollable(false)
                .selectable(true),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn metadata_only_reviews_are_empty_but_visible_content_is_not() {
        for body in [
            "",
            " \r\n",
            "<!-- hidden -->",
            " <!-- one -->\n<!-- two --> ",
        ] {
            assert!(!has_content(body));
        }
        for body in [
            "hello",
            "`<!-- code -->`",
            "<b>visible</b>",
            "<!-- hidden --> visible",
        ] {
            assert!(has_content(body));
        }
    }
}
