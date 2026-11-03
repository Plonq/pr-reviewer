//! Narrow video discovery: explicit embeds and video/attachment links only.
//! Code nodes, images, ordinary links, iframes and raw prose are never probed.
use crate::{
    http::video as download,
    video::{self, Origin, VideoHub, VideoTarget},
    ReviewApp,
};
use gpui::component::text::{
    InlineElement, InlineRenderContext, MarkdownNode, MarkdownParseContext, MarkdownPlugin,
};
use gpui::{div, prelude::*, px, App, Entity, SharedString, WeakEntity, Window};
use markdown::mdast::Node;
use std::collections::HashMap;

#[derive(Clone)]
pub(super) struct VideoPlugin {
    pub block: bool,
    pub hub: Entity<VideoHub>,
    pub owner: WeakEntity<ReviewApp>,
    pub item: u64,
    pub pane: crate::ItemPane,
    pub host: SharedString,
}

fn link_target(node: &Node) -> Option<VideoTarget> {
    match node {
        Node::Link(link)
            if (download::direct_video(&link.url) || download::attachment(&link.url))
                && link
                    .children
                    .iter()
                    .all(|child| matches!(child, Node::Text(_))) =>
        {
            Some(VideoTarget {
                url: link.url.clone().into(),
                label: link
                    .children
                    .iter()
                    .filter_map(|child| {
                        if let Node::Text(text) = child {
                            Some(text.value.as_str())
                        } else {
                            None
                        }
                    })
                    .collect::<String>()
                    .into(),
                explicit: false,
            })
        }
        Node::Html(html) => html_video(&html.value).map(|url| VideoTarget {
            url: url.into(),
            label: "Video".into(),
            explicit: true,
        }),
        _ => None,
    }
}

// Consume a single HTML tag while respecting quoted '>' and entity-encoded
// URLs. This is not an HTML renderer; nothing is executed and only HTTP(S)
// sources are returned. Duplicate attributes use HTML's first-value rule.
fn tag(source: &str) -> Option<(String, HashMap<String, String>, &str)> {
    let mut rest = source.trim_start().strip_prefix('<')?;
    let end = rest
        .find(|c: char| c.is_ascii_whitespace() || matches!(c, '>' | '/'))
        .unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    let name = rest[..end].to_ascii_lowercase();
    rest = &rest[end..];
    let mut attrs = HashMap::new();
    loop {
        rest = rest.trim_start();
        if let Some(tail) = rest.strip_prefix('>').or_else(|| rest.strip_prefix("/>")) {
            return Some((name, attrs, tail));
        }
        let end = rest
            .find(|c: char| c.is_ascii_whitespace() || matches!(c, '=' | '>' | '/'))
            .unwrap_or(rest.len());
        if end == 0 {
            return None;
        }
        let key = rest[..end].to_ascii_lowercase();
        rest = rest[end..].trim_start();
        let value = if let Some(after) = rest.strip_prefix('=') {
            rest = after.trim_start();
            match rest.as_bytes().first()? {
                b'\'' | b'"' => {
                    let quote = rest.as_bytes()[0] as char;
                    rest = &rest[1..];
                    let end = rest.find(quote)?;
                    let value = &rest[..end];
                    rest = &rest[end + 1..];
                    value
                }
                _ => {
                    let end = rest
                        .find(|c: char| c.is_ascii_whitespace() || c == '>')
                        .unwrap_or(rest.len());
                    if end == 0 {
                        return None;
                    }
                    let value = &rest[..end];
                    rest = &rest[end..];
                    value
                }
            }
        } else {
            ""
        };
        attrs
            .entry(key)
            .or_insert_with(|| super::github_image::decode_entities(value));
    }
}

fn html_video(source: &str) -> Option<String> {
    let (name, attrs, mut rest) = tag(source)?;
    if name != "video" {
        return None;
    }
    let mut url = attrs
        .get("src")
        .filter(|url| download::safe_url(url))
        .cloned();
    loop {
        rest = rest.trim_start();
        if rest
            .get(..8)
            .is_some_and(|s| s.eq_ignore_ascii_case("</video>"))
        {
            return rest[8..].trim().is_empty().then_some(url).flatten();
        }
        if rest.starts_with('<') {
            let (name, attrs, tail) = tag(rest)?;
            if name != "source" {
                return None;
            }
            if url.is_none() {
                url = attrs
                    .get("src")
                    .filter(|url| download::safe_url(url))
                    .cloned();
            }
            rest = tail;
        } else {
            // Plain fallback text is harmless; leave richer mixed HTML stock.
            rest = &rest[rest.find('<')?..];
        }
    }
}

impl VideoPlugin {
    fn card(&self, node: &MarkdownNode, cx: &mut App) -> gpui::AnyElement {
        let (offset, target) = node.data::<(usize, VideoTarget)>().unwrap();
        let selector = format!("{}-video-{offset}", self.host);
        div()
            .id(SharedString::from(selector.clone()))
            .when(self.block, |element| element.w_full())
            .debug_selector(move || selector.clone())
            .child(video::card(
                target.clone(),
                SharedString::from(node.as_markdown().to_owned()),
                Origin {
                    item: self.item,
                    pane: self.pane,
                    key: format!("{}-{offset}", self.host).into(),
                },
                self.hub.clone(),
                self.owner.clone(),
                cx,
            ))
            .into_any_element()
    }
}
impl MarkdownPlugin for VideoPlugin {
    fn name(&self) -> &str {
        if self.block {
            "gander-video-block"
        } else {
            "gander-video-inline"
        }
    }
    fn is_block(&self) -> bool {
        self.block
    }
    fn parse(&self, node: &Node, cx: &MarkdownParseContext<'_>) -> Option<MarkdownNode> {
        // CommonMark splits <video>...</video> into sibling inline HTML
        // nodes, even on an otherwise embed-only line. Parse the paragraph's
        // original source without replacing surrounding Markdown paragraphs.
        let html_target = if self.block && matches!(node, Node::Paragraph(_)) {
            cx.node_source(node)
                .and_then(html_video)
                .map(|url| VideoTarget {
                    url: url.into(),
                    label: "Video".into(),
                    explicit: true,
                })
        } else {
            None
        };
        let candidate = match node {
            Node::Paragraph(p)
                if self.block && (p.children.len() == 1 || html_target.is_some()) =>
            {
                &p.children[0]
            }
            Node::Html(_) if self.block => node,
            Node::Link(_) | Node::Html(_) if !self.block => node,
            _ => return None,
        };
        let target = html_target.or_else(|| link_target(candidate))?;
        let offset = node.position()?.start.offset;
        Some(
            MarkdownNode::new(self.name(), (offset, target.clone()))
                .text(target.label.to_string())
                .markdown(cx.node_source(node).unwrap_or_default().to_owned()),
        )
    }
    fn render(&self, node: &MarkdownNode, _: &mut Window, cx: &mut App) -> impl IntoElement {
        self.card(node, cx)
    }
    fn render_inline(
        &self,
        node: &MarkdownNode,
        _: &InlineRenderContext,
        _: &mut Window,
        cx: &mut App,
    ) -> Option<InlineElement> {
        let (_, target) = node.data::<(usize, VideoTarget)>()?;
        if self.hub.read(cx).non_video(&target.url) {
            return Some(InlineElement::new(self.card(node, cx)));
        }
        Some(InlineElement::new(
            div().w(px(320.)).h(px(260.)).child(self.card(node, cx)),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn html_video_is_narrow_and_does_not_autoplay_or_accept_local_files() {
        assert_eq!(
            html_video(
                "<VIDEO controls autoplay src='https://example.com/a.mov?x=1&amp;y=2'></VIDEO>"
            ),
            Some("https://example.com/a.mov?x=1&y=2".into())
        );
        assert_eq!(html_video("<video><source src=\"https://github.com/user-attachments/assets/id\" type='video/mp4'>fallback</video>"), Some("https://github.com/user-attachments/assets/id".into()));
        for html in [
            "<iframe src='https://example.com/a.mp4'></iframe>",
            "<video src='file:///secret'></video>",
            "<video src='https://example.com/a.mp4'></video><script>x</script>",
            "<video src='https://example.com/a.mp4'><b>text</b></video>",
        ] {
            assert!(html_video(html).is_none(), "{html}");
        }
    }
    #[test]
    fn only_actual_links_are_candidates_never_code_or_linked_images() {
        let source = "https://github.com/user-attachments/assets/id\n\n[clip](https://example.com/a.mp4)\n\n`https://example.com/code.mp4`\n\n[![image](https://example.com/a.png)](https://example.com/b.mp4)\n\n[ordinary](https://example.com/page)";
        let root = markdown::to_mdast(source, &markdown::ParseOptions::gfm()).unwrap();
        fn count(node: &Node) -> usize {
            usize::from(link_target(node).is_some())
                + node
                    .children()
                    .map_or(0, |children| children.iter().map(count).sum())
        }
        assert_eq!(count(&root), 2);
    }
}
