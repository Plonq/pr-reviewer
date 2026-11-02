//! Stock Markdown prose/selection, with image-only Kit extension hooks.
//! Raw HTML and reference-style images retain stock Kit behaviour.
use crate::{image_viewer::ImageTarget, ReviewApp};
use gpui::component::text::{
    InlineElement, InlineRenderContext, MarkdownExtensions, MarkdownNode, MarkdownParseContext,
    MarkdownPlugin, TextView,
};
use gpui::{
    div, img, prelude::*, px, relative, App, Context, ImgResourceLoader, MouseButton, Resource,
    SharedString, StyledImage, WeakEntity, Window,
};
use markdown::mdast::Node;
use std::{
    collections::HashMap,
    sync::{Arc, OnceLock},
};

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

#[derive(Clone)]
struct ImagePlugin {
    block: bool,
    owner: WeakEntity<ReviewApp>,
    host: SharedString,
    // Parse only on Kit's parser thread, once per document, to recover the
    // parent link without claiming/replacing a mixed text-and-image link.
    links: Arc<OnceLock<HashMap<usize, SharedString>>>,
}

fn image_target(node: &Node, links: &HashMap<usize, SharedString>) -> Option<(usize, ImageTarget)> {
    let Node::Image(image) = node else {
        return None;
    };
    // Keep data URLs and other non-network images on the stock renderer. In
    // particular never turn document-provided paths into filesystem access.
    if !image.url.starts_with("https://") && !image.url.starts_with("http://") {
        return None;
    }
    let offset = image.position.as_ref()?.start.offset;
    Some((
        offset,
        ImageTarget {
            url: image.url.clone().into(),
            alt: image.alt.clone().into(),
            link: links.get(&offset).cloned(),
        },
    ))
}

fn image_links(source: &str) -> HashMap<usize, SharedString> {
    fn definitions(node: &Node, defs: &mut HashMap<String, String>) {
        if let Node::Definition(def) = node {
            defs.insert(def.identifier.clone(), def.url.clone());
        }
        if let Some(children) = node.children() {
            for child in children {
                definitions(child, defs);
            }
        }
    }
    fn visit(
        node: &Node,
        inherited: Option<&str>,
        defs: &HashMap<String, String>,
        links: &mut HashMap<usize, SharedString>,
    ) {
        let link = match node {
            Node::Link(link) => Some(link.url.as_str()),
            Node::LinkReference(link) => defs.get(&link.identifier).map(String::as_str),
            _ => inherited,
        };
        if let (Node::Image(image), Some(link)) = (node, link) {
            if let Some(pos) = &image.position {
                links.insert(pos.start.offset, link.to_owned().into());
            }
        }
        if let Some(children) = node.children() {
            for child in children {
                visit(child, link, defs, links);
            }
        }
    }
    let mut links = HashMap::new();
    let mut options = markdown::ParseOptions::gfm();
    options.constructs.math_text = true;
    options.constructs.math_flow = true;
    if let Ok(root) = markdown::to_mdast(source, &options) {
        let mut defs = HashMap::new();
        definitions(&root, &mut defs);
        visit(&root, None, &defs, &mut links);
    }
    links
}

impl ImagePlugin {
    fn image(&self, offset: usize, target: &ImageTarget) -> gpui::Stateful<gpui::Img> {
        let owner = self.owner.clone();
        let target = target.clone();
        let activate = target.clone();
        let selector = format!("{}-image-{offset}", self.host);
        img(target.url)
            .id(offset)
            .object_fit(gpui::ObjectFit::Contain)
            .max_w(relative(1.))
            .when(self.block, |image| {
                image
                    .with_loading(|| {
                        div()
                            .w(px(180.))
                            .h(px(32.))
                            .text_sm()
                            .child("Loading image…")
                            .into_any_element()
                    })
                    .with_fallback(|| {
                        div()
                            .text_sm()
                            .child("Image unavailable — click to inspect")
                            .into_any_element()
                    })
            })
            .cursor_pointer()
            .debug_selector(move || selector.clone())
            // Do not start Kit's prose selection or the diff's selection drag
            // when the gesture starts on an image.
            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
            })
            .on_click(move |_, window, cx| {
                cx.stop_propagation();
                let _ = owner.update(cx, |app, cx| {
                    if let Some(item_id) = app.active_item().map(|item| item.id) {
                        app.open_image(activate.clone(), item_id, window, cx);
                    }
                });
            })
    }
}

impl MarkdownPlugin for ImagePlugin {
    fn name(&self) -> &str {
        if self.block {
            "gander-block-images"
        } else {
            "gander-inline-image"
        }
    }
    fn is_block(&self) -> bool {
        self.block
    }

    fn parse(&self, node: &Node, cx: &MarkdownParseContext<'_>) -> Option<MarkdownNode> {
        fn contains_image(node: &Node) -> bool {
            matches!(node, Node::Image(_))
                || node
                    .children()
                    .is_some_and(|children| children.iter().any(contains_image))
        }
        // Ordinary prose should not pay for a second AST walk/parse.
        match node {
            Node::Paragraph(_) if self.block && contains_image(node) => {}
            Node::Image(_) if !self.block => {}
            _ => return None,
        }
        let links = self.links.get_or_init(|| image_links(cx.source()));
        let images = if self.block {
            let Node::Paragraph(paragraph) = node else {
                return None;
            };
            // Image-only paragraphs keep stock full-size/block presentation.
            // Anything with text stays in Kit's normal paragraph/inline flow.
            fn collect(
                node: &Node,
                links: &HashMap<usize, SharedString>,
                out: &mut Vec<(usize, ImageTarget)>,
            ) -> Option<()> {
                match node {
                    Node::Link(link) => {
                        for child in &link.children {
                            collect(child, links, out)?;
                        }
                    }
                    Node::LinkReference(link) => {
                        for child in &link.children {
                            collect(child, links, out)?;
                        }
                    }
                    _ => out.push(image_target(node, links)?),
                }
                Some(())
            }
            let mut images = Vec::new();
            for child in &paragraph.children {
                collect(child, links, &mut images)?;
            }
            if images.is_empty() {
                return None;
            }
            images
        } else {
            vec![image_target(node, links)?]
        };
        let alt = images
            .iter()
            .map(|(_, image)| image.alt.as_ref())
            .collect::<Vec<_>>()
            .join(" ");
        Some(
            MarkdownNode::new(self.name(), images)
                .text(alt)
                .markdown(cx.node_source(node).unwrap_or_default().to_owned()),
        )
    }

    fn render(&self, node: &MarkdownNode, _: &mut Window, _: &mut App) -> impl IntoElement {
        div().w_full().children(
            node.data::<Vec<(usize, ImageTarget)>>()
                .unwrap()
                .iter()
                .map(|(offset, target)| self.image(*offset, target)),
        )
    }

    fn render_inline(
        &self,
        node: &MarkdownNode,
        context: &InlineRenderContext,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<InlineElement> {
        let (offset, target) = &node.data::<Vec<(usize, ImageTarget)>>()?[0];
        // Match Kit's stock mixed-paragraph image geometry: 3/4 line-height,
        // with asynchronously resolved intrinsic aspect ratio. The same GPUI
        // asset (and secure HTTP client) supplies both measurement and drawing.
        let intrinsic = window
            .use_asset::<ImgResourceLoader>(&Resource::Uri(target.url.clone()), cx)
            .and_then(Result::ok)
            .map(|image| image.size(0));
        let height = context.line_height() * 0.75;
        let ratio = intrinsic.map_or(1., |size| {
            size.width.0 as f32 / (size.height.0 as f32).max(1.)
        });
        Some(InlineElement::new(
            self.image(*offset, target).w(height * ratio).h(height),
        ))
    }
}

pub(crate) fn render(
    id: &str,
    source: String,
    _: &mut Window,
    cx: &mut Context<ReviewApp>,
) -> gpui::AnyElement {
    let plugin = ImagePlugin {
        block: true,
        owner: cx.entity().downgrade(),
        host: id.to_owned().into(),
        links: Arc::new(OnceLock::new()),
    };
    let extensions = MarkdownExtensions::default()
        .plugin(plugin.clone())
        .plugin(ImagePlugin {
            block: false,
            ..plugin
        })
        .parser_revision(1);
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
                .markdown_extensions(extensions)
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

    #[test]
    fn image_parent_links_do_not_claim_surrounding_text_or_code() {
        let source = "[before ![a](https://github.com/a) after](https://example.com/one)\n\n[![b](https://github.com/b)][dest]\n\n[dest]: https://example.com/two\n\n`![code](https://github.com/code)`\n\n<img src='https://github.com/html'>";
        let links = image_links(source);
        assert_eq!(links.len(), 2);
        assert_eq!(
            links[&source.find("![a]").unwrap()].as_ref(),
            "https://example.com/one"
        );
        assert_eq!(
            links[&source.find("![b]").unwrap()].as_ref(),
            "https://example.com/two"
        );
    }
}
