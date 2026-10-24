//! Keep TextView's inline styling/selection/highlighting, but own block layout.
//! Kit 0.5 merges a list item's later paragraphs into its first horizontal row;
//! markers also shrink to zero width. Explicit, nonshrinking marker columns and
//! vertically stacked blocks avoid both overlapping text and clipped content.

use std::rc::Rc;

use ::markdown::{mdast::Node, ParseOptions};
use gpui::{
    div, px, rems, Context, InteractiveElement, IntoElement, ParentElement, SharedString, Styled,
    Window,
};
use gpui_component::{
    text::{TextView, TextViewStyle},
    ActiveTheme,
};

use crate::{text_size, theme, ReviewApp};

struct Document {
    source: String,
    root: Option<Node>,
    definitions: String,
}

impl Document {
    fn new(source: String) -> Self {
        let root = ::markdown::to_mdast(&source, &ParseOptions::gfm()).ok();
        let mut definitions = String::new();
        fn collect(node: &Node, source: &str, out: &mut String) {
            if let Node::Definition(def) = node {
                if let Some(pos) = &def.position {
                    out.push_str("\n\n");
                    out.push_str(&source[pos.start.offset..pos.end.offset]);
                }
            }
            if let Some(children) = node.children() {
                for child in children {
                    collect(child, source, out);
                }
            }
        }
        if let Some(root) = &root {
            collect(root, &source, &mut definitions);
        }
        Self {
            source,
            root,
            definitions,
        }
    }

    fn leaf_source(&self, node: &Node) -> String {
        // Reconstruct code from its parsed value, not indentation belonging to
        // an enclosing list/blockquote. Choose a fence the code cannot close.
        if let Node::Code(code) = node {
            let run = code
                .value
                .split(|c| c != '`')
                .map(str::len)
                .max()
                .unwrap_or(0);
            let fence = "`".repeat(3.max(run + 1));
            return format!(
                "{fence}{}\n{}\n{fence}",
                code.lang.as_deref().unwrap_or_default(),
                code.value
            );
        }
        let Some(pos) = node.position() else {
            return String::new();
        };
        let raw = &self.source[pos.start.offset..pos.end.offset];
        let indent = pos.start.column.saturating_sub(1);
        let mut lines = raw.lines();
        let mut text = lines.next().unwrap_or_default().to_owned();
        for line in lines {
            text.push('\n');
            // Strip container indentation only, never the first content char.
            let strip = line
                .bytes()
                .take(indent)
                .take_while(|b| *b == b' ' || *b == b'>')
                .count();
            text.push_str(&line[strip..]);
        }
        text.push_str(&self.definitions);
        text
    }
}

fn contains_list(node: &Node) -> bool {
    matches!(node, Node::List(_))
        || node
            .children()
            .is_some_and(|children| children.iter().any(contains_list))
}

/// Whitespace and HTML-only metadata do not warrant a review summary card.
/// Code and visible HTML remain content, even when containing comment syntax.
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
    // Parsing is keyed just like TextView; repainting/scrolling never reparses.
    let cached = window.use_keyed_state(SharedString::from(format!("{id}-blocks")), cx, |_, _| {
        Rc::new(Document::new(source.clone()))
    });
    let document = cached.update(cx, |document, _| {
        if document.source != source {
            *document = Rc::new(Document::new(source));
        }
        document.clone()
    });
    match &document.root {
        // Leave ordinary bodies intact, including cross-paragraph selection.
        Some(root) if !contains_list(root) => text_view(id, document.source.clone(), window, cx),
        Some(root) => render_node(root, &document, id, window, cx),
        None => text_view(id, document.source.clone(), window, cx),
    }
}

fn render_node(
    node: &Node,
    doc: &Document,
    id: &str,
    window: &mut Window,
    cx: &mut Context<ReviewApp>,
) -> gpui::AnyElement {
    let column = || div().w_full().min_w_0().flex().flex_col().gap(rems(0.75));
    match node {
        Node::Root(_) | Node::ListItem(_) | Node::Blockquote(_) => {
            let mut block = column();
            if matches!(node, Node::Blockquote(_)) {
                block = block.border_l_2().border_color(theme::surface0()).pl_4();
            }
            for (ix, child) in node.children().into_iter().flatten().enumerate() {
                if !matches!(child, Node::Definition(_)) {
                    block = block.child(render_node(child, doc, &format!("{id}-{ix}"), window, cx));
                }
            }
            block.into_any_element()
        }
        Node::List(list) => {
            let mut block = column();
            for (ix, item) in list.children.iter().enumerate() {
                let child_id = format!("{id}-{ix}");
                let marker = match item {
                    Node::ListItem(item) if item.checked.is_some() => {
                        if item.checked == Some(true) {
                            "☑".into()
                        } else {
                            "☐".into()
                        }
                    }
                    _ if list.ordered => format!("{}.", list.start.unwrap_or(1) + ix as u32),
                    _ => "•".into(),
                };
                block = block.child(
                    div()
                        .w_full()
                        .min_w_0()
                        .flex()
                        .items_start()
                        .gap_2()
                        .child(
                            div()
                                .debug_selector({
                                    let id = child_id.clone();
                                    move || format!("{id}-marker")
                                })
                                .flex_shrink_0()
                                .min_w(rems(1.5))
                                .child(marker),
                        )
                        .child(
                            div()
                                .debug_selector({
                                    let id = child_id.clone();
                                    move || format!("{id}-body")
                                })
                                .flex_1()
                                .min_w_0()
                                .child(render_node(item, doc, &child_id, window, cx)),
                        ),
                );
            }
            block.into_any_element()
        }
        _ => text_view(id, doc.leaf_source(node), window, cx),
    }
}

fn text_view(
    id: &str,
    source: String,
    window: &mut Window,
    cx: &mut Context<ReviewApp>,
) -> gpui::AnyElement {
    div()
        .w_full()
        .min_w_0()
        .flex_shrink_0()
        .debug_selector({
            let id = id.to_owned();
            move || format!("{id}-text")
        })
        .child(
            TextView::markdown(SharedString::from(id.to_owned()), source, window, cx)
                .w_full()
                .h_auto()
                .selectable(true)
                .style(TextViewStyle {
                    heading_base_font_size: px(text_size()),
                    paragraph_gap: rems(0.75),
                    highlight_theme: cx.theme().highlight_theme.clone(),
                    is_dark: !theme::is_light(),
                    ..Default::default()
                }),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_only_reviews_are_empty_but_code_and_visible_html_are_not() {
        for body in ["", " \r\n", "<!-- hidden -->", "<!-- a -->\n<!-- b -->"] {
            assert!(!has_content(body));
        }
        for body in [
            "<!-- hidden -->visible",
            "`<!-- -->`",
            "```\n<!-- -->\n```",
            "<b>text</b>",
        ] {
            assert!(has_content(body));
        }
    }

    #[test]
    fn list_leaves_preserve_unicode_formatting_reference_links_and_fenced_code() {
        let doc = Document::new("10. Unicode café **bold** [link][ref]\n    continued `code`\n\n    ```rust\n    let fence = \"```\";\n    ```\n\n[ref]: https://example.com\n".into());
        let root = doc.root.as_ref().unwrap();
        let list = &root.children().unwrap()[0];
        let Node::List(list) = list else {
            panic!("list")
        };
        assert_eq!(list.start, Some(10));
        let children = list.children[0].children().unwrap();
        let paragraph = doc.leaf_source(&children[0]);
        assert!(paragraph.starts_with("Unicode café **bold** [link][ref]\ncontinued `code`"));
        assert!(paragraph.contains("[ref]: https://example.com"));
        let source = doc.leaf_source(&children[1]);
        let parsed = ::markdown::to_mdast(&source, &ParseOptions::gfm()).unwrap();
        let Node::Code(code) = &parsed.children().unwrap()[0] else {
            panic!("code")
        };
        assert_eq!(code.lang.as_deref(), Some("rust"));
        assert_eq!(code.value, "let fence = \"```\";");
    }
}
