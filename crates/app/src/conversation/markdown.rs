//! Keep TextView's inline styling/selection/highlighting, but own block layout.
//! Kit 0.5 merges a list item's later paragraphs into its first horizontal row;
//! markers also shrink to zero width. Explicit, nonshrinking marker columns and
//! vertically stacked blocks avoid both overlapping text and clipped content.

use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;

use ::markdown::{mdast::Node, ParseOptions};
use gpui::component::{
    text::{TextView, TextViewStyle},
    ActiveTheme,
};
use gpui::{
    div, px, rems, App, Context, InteractiveElement, IntoElement, ParentElement, SharedString,
    Styled, Window,
};

use super::media::{self, Media};
use crate::{text_size, theme, ReviewApp};

struct Document {
    source: String,
    root: Option<Node>,
    definitions: String,
    media: Vec<Media>,
}

/// Extract before soft wrapping inline diff comments: wrapping an image URL
/// into fixed-width text rows would destroy both Markdown and HTML embeds.
pub(crate) fn inline_images(source: &str) -> Vec<Media> {
    if !source.contains("![") && !source.to_ascii_lowercase().contains("<img") {
        return Vec::new();
    }
    Document::new(source.into()).media
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
        let mut media = Vec::new();
        if let Some(root) = &root {
            collect_media(root, &source, &mut media);
        }
        Self {
            source,
            root,
            definitions,
            media,
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
        self.fragment(pos.start.offset..pos.end.offset, node)
    }

    fn fragment(&self, range: Range<usize>, node: &Node) -> String {
        let raw = &self.source[range];
        let Some(pos) = node.position() else {
            return raw.into();
        };
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

    fn images_in<'a>(&'a self, node: &Node) -> impl Iterator<Item = &'a Media> {
        let range = node.position().map(|pos| pos.start.offset..pos.end.offset);
        self.media.iter().filter(move |image| {
            range.as_ref().is_some_and(|range| {
                image.range.start >= range.start && image.range.end <= range.end
            })
        })
    }
}

fn collect_media(root: &Node, source: &str, out: &mut Vec<Media>) {
    fn definitions<'a>(node: &'a Node, out: &mut HashMap<&'a str, &'a str>) {
        if let Node::Definition(def) = node {
            out.entry(&def.identifier).or_insert(&def.url);
        }
        for child in node.children().into_iter().flatten() {
            definitions(child, out);
        }
    }
    fn walk(node: &Node, source: &str, defs: &HashMap<&str, &str>, out: &mut Vec<Media>) {
        let Some(pos) = node.position() else { return };
        let range = pos.start.offset..pos.end.offset;
        let image = match node {
            Node::Image(image) => Some((image.url.as_str(), image.alt.as_str())),
            Node::ImageReference(image) => defs
                .get(image.identifier.as_str())
                .map(|url| (*url, image.alt.as_str())),
            _ => None,
        };
        if let Some((url, alt)) = image.filter(|(url, _)| media::is_remote_image(url)) {
            out.push(Media::new(range, url.into(), alt.into()));
        } else if let Node::Html(_) = node {
            out.extend(media::html_images(&source[range.clone()], range.start));
        } else if let Node::Link(link) = node {
            let start = out.len();
            for child in &link.children {
                walk(child, source, defs, out);
            }
            if link.children.len() == 1 && out.len() == start + 1 {
                // A clickable image, not dangling markdown around the control.
                out[start].range = range;
                out[start].link = Some(link.url.clone());
            }
        } else {
            for child in node.children().into_iter().flatten() {
                walk(child, source, defs, out);
            }
        }
    }
    let mut defs = HashMap::new();
    definitions(root, &mut defs);
    walk(root, source, &defs, out);
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
        Some(root) if !contains_list(root) && document.media.is_empty() => {
            text_view(id, document.source.clone(), window, cx)
        }
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
    // Tight lists need only a small row gap, not a full paragraph break.
    // Keep blank-line-separated (spread) lists/items and ordinary blocks roomy.
    let gap = match node {
        Node::List(list) if !list.spread => rems(0.25),
        Node::ListItem(item) if !item.spread => rems(0.25),
        _ => rems(0.75),
    };
    let column = || div().w_full().min_w_0().flex().flex_col().gap(gap);
    match node {
        Node::Root(_) | Node::ListItem(_) | Node::Blockquote(_) => {
            let mut block = column();
            if matches!(node, Node::Blockquote(_)) {
                block = block.border_l_2().border_color(theme::surface0(cx)).pl_4();
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
        Node::Table(table) if doc.images_in(node).next().is_some() => {
            // Splitting the table's markdown around an image would leave raw
            // pipes/half a table. Keep media in its cell and rows width-safe.
            let mut block = column()
                .gap_0()
                .border_1()
                .border_color(theme::surface0(cx));
            for (row_ix, row) in table.children.iter().enumerate() {
                let mut line = div().w_full().min_w_0().flex();
                if row_ix == 0 {
                    line = line.font_weight(gpui::FontWeight::SEMIBOLD);
                }
                for (cell_ix, cell) in row.children().into_iter().flatten().enumerate() {
                    line = line.child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .p_2()
                            .border_1()
                            .border_color(theme::surface0(cx))
                            .child(render_node(
                                cell,
                                doc,
                                &format!("{id}-{row_ix}-{cell_ix}"),
                                window,
                                cx,
                            )),
                    );
                }
                block = block.child(line);
            }
            block.into_any_element()
        }
        _ => {
            let images: Vec<_> = doc.images_in(node).collect();
            if images.is_empty() {
                return text_view(id, doc.leaf_source(node), window, cx);
            }
            let mut block = column();
            let pos = node.position().unwrap();
            let mut cursor = pos.start.offset;
            for (ix, image) in images.iter().enumerate() {
                if !doc.source[cursor..image.range.start].trim().is_empty() {
                    block = block.child(text_view(
                        &format!("{id}-before-{ix}"),
                        doc.fragment(cursor..image.range.start, node),
                        window,
                        cx,
                    ));
                }
                block = block.child(media::render(&format!("{id}-{ix}"), image, window, cx));
                cursor = image.range.end;
            }
            if !doc.source[cursor..pos.end.offset].trim().is_empty() {
                block = block.child(text_view(
                    &format!("{id}-after"),
                    doc.fragment(cursor..pos.end.offset, node),
                    window,
                    cx,
                ));
            }
            block.into_any_element()
        }
    }
}

pub(super) fn text_view(
    id: &str,
    source: String,
    window: &mut Window,
    cx: &mut App,
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
            TextView::markdown(SharedString::from(id.to_owned()), source)
                .w_full()
                .h_auto()
                .selectable(true)
                .style(TextViewStyle {
                    heading_base_font_size: px(text_size()),
                    paragraph_gap: rems(0.75),
                    highlight_theme: cx.theme().highlight_theme.clone(),
                    is_dark: !theme::is_light(cx),
                    ..Default::default()
                }),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_in_markdown_references_links_lists_and_html_has_exact_source_ranges() {
        let source = "Before café ![screenshot](https://github.com/user-attachments/assets/private) after.\n\n- [![diagram][svg]](https://example.com/full)\n\n  <img\n    width='320' src='https://example.com/screen.png' alt='screen'>\n\n[svg]: https://example.com/diagram.svg\n";
        let doc = Document::new(source.into());
        assert_eq!(doc.media.len(), 3);
        assert_eq!(
            &source[doc.media[0].range.clone()],
            "![screenshot](https://github.com/user-attachments/assets/private)"
        );
        assert_eq!(
            &source[doc.media[1].range.clone()],
            "[![diagram][svg]](https://example.com/full)"
        );
        assert_eq!(
            doc.media[1].link.as_deref(),
            Some("https://example.com/full")
        );
        assert_eq!(doc.media[1].url, "https://example.com/diagram.svg");
        assert!(source[doc.media[2].range.clone()].starts_with("<img\n"));
        assert_eq!(doc.media[2].width, Some(320.));
    }

    #[test]
    fn text_only_and_image_syntax_in_code_never_enter_the_media_path() {
        for source in [
            "Plain **text**, [a link](https://example.com/image.svg) and a table:\n\n| A | B |\n| - | - |\n| a | b |",
            "`![literal](https://example.com/a.svg)`\n\n```html\n<img src='https://example.com/b.svg'>\n```",
            "<!-- <img src='https://example.com/hidden.png'> -->",
        ] {
            let doc = Document::new(source.into());
            assert!(doc.media.is_empty(), "{source}");
            assert!(!contains_list(doc.root.as_ref().unwrap()));
        }
    }

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
