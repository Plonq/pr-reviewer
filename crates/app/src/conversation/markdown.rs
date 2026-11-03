//! Stock Markdown prose/selection, with narrowly scoped media extension hooks.
//! GitHub's image-only paste/drop HTML uses the same activation path; general
//! HTML and reference-style images retain stock Kit behaviour.
mod github_image;
mod video;
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

#[derive(Clone)]
struct EmbeddedImage {
    offset: usize,
    target: ImageTarget,
    width: Option<f32>,
    height: Option<f32>,
}

#[derive(Clone)]
struct CaptionedImage {
    html: SharedString,
    targets: Vec<ImageTarget>,
}

fn image_nodes(node: &Node, links: &HashMap<usize, SharedString>) -> Option<Vec<EmbeddedImage>> {
    let offset = node.position()?.start.offset;
    match node {
        Node::Image(image) => {
            // Never turn document-provided paths into filesystem access.
            if !image.url.starts_with("https://") && !image.url.starts_with("http://") {
                return None;
            }
            Some(vec![EmbeddedImage {
                offset,
                target: ImageTarget {
                    url: image.url.clone().into(),
                    alt: image.alt.clone().into(),
                    link: links.get(&offset).cloned(),
                },
                width: None,
                height: None,
            }])
        }
        Node::Html(html) => Some(
            github_image::parse(&html.value)?
                .into_iter()
                .map(|(local, image)| {
                    let offset = offset + local;
                    EmbeddedImage {
                        offset,
                        target: ImageTarget {
                            url: image.url.into(),
                            alt: image.alt.into(),
                            link: links.get(&offset).cloned(),
                        },
                        width: image.width,
                        height: image.height,
                    }
                })
                .collect(),
        ),
        _ => None,
    }
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
        if let (Node::Html(html), Some(link), Some(pos)) = (node, link, node.position()) {
            if let Some(images) = github_image::parse(&html.value) {
                for (local, _) in images {
                    links.insert(pos.start.offset + local, link.to_owned().into());
                }
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
    fn image(&self, image: &EmbeddedImage) -> gpui::Stateful<gpui::Img> {
        let offset = image.offset;
        let owner = self.owner.clone();
        let target = image.target.clone();
        let activate = target.clone();
        let selector = format!("{}-image-{offset}", self.host);
        img(target.url)
            .id(offset)
            .object_fit(gpui::ObjectFit::Contain)
            .max_w(relative(1.))
            .when(self.block, |element| {
                // Match Kit's block-image path: explicit width, intrinsic
                // aspect ratio and a maximum of the available prose width.
                element
                    .when_some(image.width, |element, width| element.w(px(width)))
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
        if self.block {
            if let Node::Html(html) = node {
                if let Some(captioned) = github_image::captioned(&html.value) {
                    return Some(
                        MarkdownNode::new(
                            self.name(),
                            CaptionedImage {
                                html: captioned.html.into(),
                                targets: captioned
                                    .images
                                    .into_iter()
                                    .map(|image| ImageTarget {
                                        url: image.url.into(),
                                        alt: image.alt.into(),
                                        link: None,
                                    })
                                    .collect(),
                            },
                        )
                        .text(html.value.clone())
                        .markdown(cx.node_source(node).unwrap_or_default().to_owned()),
                    );
                }
            }
        }
        fn contains_image(node: &Node) -> bool {
            matches!(node, Node::Image(_))
                || matches!(node, Node::Html(html) if github_image::parse(&html.value).is_some())
                || node
                    .children()
                    .is_some_and(|children| children.iter().any(contains_image))
        }
        // Ordinary prose should not pay for a second AST walk/parse.
        match node {
            Node::Paragraph(_) if self.block && contains_image(node) => {}
            Node::Html(_) if self.block && contains_image(node) => {}
            Node::Image(_) | Node::Html(_) if !self.block => {}
            _ => return None,
        }
        let links = self.links.get_or_init(|| image_links(cx.source()));
        let images = if self.block && matches!(node, Node::Paragraph(_)) {
            let Node::Paragraph(paragraph) = node else {
                unreachable!()
            };
            // Image-only paragraphs keep stock full-size/block presentation.
            // Anything with text stays in Kit's normal paragraph/inline flow.
            fn collect(
                node: &Node,
                links: &HashMap<usize, SharedString>,
                out: &mut Vec<EmbeddedImage>,
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
                    _ => out.extend(image_nodes(node, links)?),
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
            image_nodes(node, links)?
        };
        if !self.block && images.len() != 1 {
            return None;
        }
        let alt = images
            .iter()
            .map(|image| image.target.alt.as_ref())
            .collect::<Vec<_>>()
            .join(" ");
        Some(
            MarkdownNode::new(self.name(), images)
                .text(alt)
                .markdown(cx.node_source(node).unwrap_or_default().to_owned()),
        )
    }

    fn render(&self, node: &MarkdownNode, _: &mut Window, _: &mut App) -> impl IntoElement {
        if let Some(captioned) = node.data::<CaptionedImage>() {
            let offset = node.source_range().map_or(0, |range| range.start);
            let owner = self.owner.clone();
            let targets = captioned.targets.clone();
            let selector = format!("{}-captioned-image-{offset}", self.host);
            return div()
                .w_full()
                .debug_selector(move || selector.clone())
                .child(
                    TextView::html(
                        SharedString::from(format!("{}-captioned-{offset}", self.host)),
                        captioned.html.clone(),
                    )
                    .w_full()
                    .h_auto()
                    .scrollable(false)
                    .selectable(true)
                    .on_link_click(move |link, _, window, cx| {
                        let Some(target) = link
                            .strip_prefix("gander-image:")
                            .and_then(|index| index.parse::<usize>().ok())
                            .and_then(|index| targets.get(index))
                        else {
                            return;
                        };
                        let _ = owner.update(cx, |app, cx| {
                            if let Some(item_id) = app.active_item().map(|item| item.id) {
                                app.open_image(target.clone(), item_id, window, cx);
                            }
                        });
                    }),
                )
                .into_any_element();
        }
        div()
            .w_full()
            .children(
                node.data::<Vec<EmbeddedImage>>()
                    .unwrap()
                    .iter()
                    .map(|image| self.image(image)),
            )
            .into_any_element()
    }

    fn render_inline(
        &self,
        node: &MarkdownNode,
        context: &InlineRenderContext,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<InlineElement> {
        let image = &node.data::<Vec<EmbeddedImage>>()?[0];
        let target = &image.target;
        // Match Kit's mixed-paragraph image geometry: Markdown images use
        // 3/4 line-height; HTML images retain their declared pixel dimensions.
        // The same GPUI asset and secure HTTP client supply both measurement
        // and drawing, including asynchronously resolved intrinsic sizing.
        let intrinsic = window
            .use_asset::<ImgResourceLoader>(&Resource::Uri(target.url.clone()), cx)
            .and_then(Result::ok)
            .map(|image| image.size(0));
        let ratio = intrinsic.map_or(1., |size| {
            size.width.0 as f32 / (size.height.0 as f32).max(1.)
        });
        let (width, height) = match (image.width, image.height) {
            (Some(width), Some(height)) => (px(width), px(height)),
            (Some(width), None) => (
                px(width),
                intrinsic.map_or(context.line_height(), |_| px(width / ratio)),
            ),
            (None, Some(height)) => (px(height * ratio), px(height)),
            (None, None) => {
                let height = context.line_height() * 0.75;
                (height * ratio, height)
            }
        };
        Some(InlineElement::new(self.image(image).w(width).h(height)))
    }
}

pub(crate) fn render(
    id: &str,
    source: String,
    app: &ReviewApp,
    _: &mut Window,
    cx: &mut Context<ReviewApp>,
) -> gpui::AnyElement {
    let plugin = ImagePlugin {
        block: true,
        owner: cx.entity().downgrade(),
        host: id.to_owned().into(),
        links: Arc::new(OnceLock::new()),
    };
    let video = video::VideoPlugin {
        block: true,
        hub: app.video_hub.clone(),
        owner: cx.entity().downgrade(),
        item: app.active_item().map_or(0, |item| item.id),
        pane: app
            .active_item()
            .map_or(crate::ItemPane::Changes, |item| item.pane),
        host: id.to_owned().into(),
    };
    let extensions = MarkdownExtensions::default()
        .plugin(video.clone())
        .plugin(video::VideoPlugin {
            block: false,
            ..video
        })
        .plugin(plugin.clone())
        .plugin(ImagePlugin {
            block: false,
            ..plugin
        })
        .parser_revision(2);
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
