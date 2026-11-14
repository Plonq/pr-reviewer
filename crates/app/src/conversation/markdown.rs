//! Stock GFM prose/selection with GitHub PR line breaks and media hooks.
//! GitHub's image-only paste/drop HTML and reference-style Markdown images
//! use the same activation path; general HTML retains stock Kit behaviour.
mod breaks;
mod github_image;
mod mermaid;
mod video;
use crate::{image_viewer::ImageTarget, ReviewApp};
use gpui::component::text::{
    InlineElement, InlineRenderContext, MarkdownExtensions, MarkdownNode, MarkdownParseContext,
    MarkdownPlugin, TextView, TextViewState,
};
use gpui::{
    div, img, prelude::*, px, relative, App, Context, MouseButton, Resource, SharedString,
    StyledImage, WeakEntity, Window,
};
use markdown::mdast::Node;
use std::{
    collections::HashMap,
    sync::{Arc, OnceLock},
};

pub(super) use crate::comments::has_content;
pub(super) use breaks::hard_breaks;

#[derive(Clone)]
struct ImagePlugin {
    block: bool,
    owner: WeakEntity<ReviewApp>,
    host: SharedString,
    // Parse only on Kit's parser thread, once per document, to recover the
    // parent link without claiming/replacing a mixed text-and-image link.
    images: Arc<OnceLock<ImageContext>>,
}

#[derive(Clone)]
struct EmbeddedImage {
    offset: usize,
    target: ImageTarget,
    width: Option<f32>,
    height: Option<f32>,
}

/// Measure against the prose width, including Kit's offscreen block pass.
/// Percentage max-width alone is unresolved during that intrinsic pass.
struct FittedImage {
    width: f32,
    ratio: f32,
    image: Option<gpui::Stateful<gpui::Img>>,
}

impl IntoElement for FittedImage {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl gpui::Element for FittedImage {
    type RequestLayoutState = ();
    type PrepaintState = gpui::AnyElement;

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        _: &mut App,
    ) -> (gpui::LayoutId, ()) {
        let width = px(self.width);
        let ratio = self.ratio;
        (
            window.request_measured_layout(Default::default(), move |known, available, _, _| {
                let available = known.width.unwrap_or_else(|| match available.width {
                    gpui::AvailableSpace::Definite(width) => width,
                    _ => width,
                });
                let width = width.min(available);
                gpui::size(width, width / ratio)
            }),
            (),
        )
    }

    fn prepaint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: gpui::Bounds<gpui::Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> gpui::AnyElement {
        let width = bounds.size.width.min(px(self.width));
        let size = gpui::size(width, width / self.ratio);
        let mut image = self
            .image
            .take()
            .unwrap()
            .w(size.width)
            .h(size.height)
            .into_any_element();
        image.prepaint_as_root(
            bounds.origin,
            size.map(gpui::AvailableSpace::Definite),
            window,
            cx,
        );
        image
    }

    fn paint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: gpui::Bounds<gpui::Pixels>,
        _: &mut (),
        image: &mut gpui::AnyElement,
        window: &mut Window,
        cx: &mut App,
    ) {
        image.paint(window, cx);
    }
}

#[derive(Clone)]
struct CaptionedImage {
    html: SharedString,
    targets: Vec<ImageTarget>,
}

#[derive(Default)]
struct ImageContext {
    links: HashMap<usize, SharedString>,
    definitions: HashMap<String, String>,
}

fn image_nodes(node: &Node, images: &ImageContext) -> Option<Vec<EmbeddedImage>> {
    let offset = node.position()?.start.offset;
    let image = match node {
        Node::Image(image) => Some((&image.url, &image.alt)),
        Node::ImageReference(image) => {
            Some((images.definitions.get(&image.identifier)?, &image.alt))
        }
        _ => None,
    };
    if let Some((url, alt)) = image {
        // Never turn document-provided paths into filesystem access, including
        // URLs resolved through a reference definition.
        if !url.starts_with("https://") && !url.starts_with("http://") {
            return None;
        }
        return Some(vec![EmbeddedImage {
            offset,
            target: ImageTarget {
                url: url.clone().into(),
                alt: alt.clone().into(),
                link: images.links.get(&offset).cloned(),
            },
            width: None,
            height: None,
        }]);
    }
    match node {
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
                            link: images.links.get(&offset).cloned(),
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

fn image_context(source: &str) -> ImageContext {
    fn definitions(node: &Node, defs: &mut HashMap<String, String>) {
        if let Node::Definition(def) = node {
            // CommonMark resolves duplicate definitions to the first one.
            defs.entry(def.identifier.clone())
                .or_insert_with(|| def.url.clone());
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
        if matches!(node, Node::Image(_) | Node::ImageReference(_)) {
            if let (Some(pos), Some(link)) = (node.position(), link) {
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
    let mut images = ImageContext::default();
    let mut options = markdown::ParseOptions::gfm();
    options.constructs.math_text = true;
    options.constructs.math_flow = true;
    if let Ok(root) = markdown::to_mdast(source, &options) {
        definitions(&root, &mut images.definitions);
        visit(&root, None, &images.definitions, &mut images.links);
    }
    images
}

impl ImagePlugin {
    fn block_image(
        &self,
        image: &EmbeddedImage,
        window: &mut Window,
        cx: &mut App,
    ) -> gpui::AnyElement {
        let intrinsic = window
            .use_asset::<crate::svg_image::ImageLoader>(
                &Resource::Uri(image.target.url.clone()),
                cx,
            )
            .and_then(Result::ok)
            .map(|image| image.size(0));
        let Some(size) = intrinsic else {
            return self.image(image).into_any_element();
        };
        // Img resolves its auto height from the requested pixel width before
        // max-width is applied. That inflates the Markdown block's measured
        // height even though the images paint smaller. Let a ratio-sized box
        // own layout, and fit the image inside the resulting bounds.
        FittedImage {
            width: image.width.unwrap_or(size.width.0 as f32),
            ratio: size.width.0 as f32 / (size.height.0 as f32).max(1.),
            image: Some(self.image(image)),
        }
        .into_any_element()
    }

    fn image(&self, image: &EmbeddedImage) -> gpui::Stateful<gpui::Img> {
        let offset = image.offset;
        let owner = self.owner.clone();
        let target = image.target.clone();
        let activate = target.clone();
        let selector = format!("{}-image-{offset}", self.host);
        let fallback_selector = format!("{selector}-fallback");
        let block = self.block;
        img(crate::svg_image::remote_source(&target.url))
            .id(offset)
            .object_fit(gpui::ObjectFit::Contain)
            .max_w(relative(1.))
            .when(self.block, |element| {
                // Keep placeholders bounded until intrinsic sizing is known.
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
            })
            .with_fallback(move || {
                div()
                    .text_sm()
                    .debug_selector(|| fallback_selector.clone())
                    .when(!block, |element| {
                        element
                            .size_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .line_height(relative(1.))
                    })
                    .child(if block {
                        "Image unavailable — click to inspect"
                    } else {
                        "×"
                    })
                    .into_any_element()
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
            matches!(node, Node::Image(_) | Node::ImageReference(_))
                || matches!(node, Node::Html(html) if github_image::parse(&html.value).is_some())
                || node
                    .children()
                    .is_some_and(|children| children.iter().any(contains_image))
        }
        // Ordinary prose should not pay for a second AST walk/parse.
        match node {
            Node::Paragraph(_) if self.block && contains_image(node) => {}
            Node::Html(_) if self.block && contains_image(node) => {}
            Node::Image(_) | Node::ImageReference(_) | Node::Html(_) if !self.block => {}
            _ => return None,
        }
        let context = self.images.get_or_init(|| image_context(cx.source()));
        let images = if self.block && matches!(node, Node::Paragraph(_)) {
            let Node::Paragraph(paragraph) = node else {
                unreachable!()
            };
            // Image-only paragraphs keep stock full-size/block presentation.
            // Anything with text stays in Kit's normal paragraph/inline flow.
            fn collect(
                node: &Node,
                images: &ImageContext,
                out: &mut Vec<EmbeddedImage>,
            ) -> Option<()> {
                match node {
                    Node::Link(link) => {
                        for child in &link.children {
                            collect(child, images, out)?;
                        }
                    }
                    Node::LinkReference(link) => {
                        for child in &link.children {
                            collect(child, images, out)?;
                        }
                    }
                    _ => out.extend(image_nodes(node, images)?),
                }
                Some(())
            }
            let mut images = Vec::new();
            for child in &paragraph.children {
                collect(child, context, &mut images)?;
            }
            if images.is_empty() {
                return None;
            }
            images
        } else {
            image_nodes(node, context)?
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

    fn render(&self, node: &MarkdownNode, window: &mut Window, cx: &mut App) -> impl IntoElement {
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
                    .image_source(crate::svg_image::markdown_source)
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
            .flex()
            .flex_col()
            .gap_3()
            .children(
                node.data::<Vec<EmbeddedImage>>()
                    .unwrap()
                    .iter()
                    .map(|image| self.block_image(image, window, cx)),
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
            .use_asset::<crate::svg_image::ImageLoader>(&Resource::Uri(target.url.clone()), cx)
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
        // Replacements are ordinary children of Img: their natural text height
        // can exceed this reserved slot. Contain both fallback and loading paint.
        Some(InlineElement::new(
            self.image(image).w(width).h(height).overflow_hidden(),
        ))
    }
}

pub(crate) fn render(
    id: &str,
    source: String,
    app: &ReviewApp,
    _: &mut Window,
    cx: &mut Context<ReviewApp>,
) -> gpui::AnyElement {
    render_view(
        id,
        TextView::markdown(SharedString::from(id.to_owned()), source),
        app,
        cx,
    )
}

pub(crate) struct PersistentMarkdown {
    source: String,
    repo_slug: String,
    pub(crate) state: gpui::Entity<TextViewState>,
}

/// Conversation documents outlive their mounted elements. Otherwise returning
/// from Changes reparses long bodies asynchronously, temporarily shortens the
/// timeline, and irreversibly clamps the pane's retained scroll offset.
pub(crate) fn render_persistent(
    id: &str,
    source: &str,
    repo_slug: &str,
    app: &ReviewApp,
    _: &mut Window,
    cx: &mut Context<ReviewApp>,
) -> gpui::AnyElement {
    let mut documents = app
        .active_data()
        .expect("a ready conversation")
        .conv_markdown
        .borrow_mut();
    let document = documents.entry(id.to_owned()).or_insert_with(|| {
        let prepared = super::preprocess(source, repo_slug);
        PersistentMarkdown {
            source: source.to_owned(),
            repo_slug: repo_slug.to_owned(),
            state: cx.new(|cx| TextViewState::markdown(&prepared, cx)),
        }
    });
    // Preprocessing now includes an AST pass for GitHub's hard breaks. Only
    // repeat it after an edit/refresh, not on every frame of a long timeline.
    if document.source != source || document.repo_slug != repo_slug {
        let prepared = super::preprocess(source, repo_slug);
        document
            .state
            .update(cx, |state, cx| state.set_text(&prepared, cx));
        document.source = source.to_owned();
        document.repo_slug = repo_slug.to_owned();
    }
    let state = document.state.clone();
    drop(documents);
    render_view(id, TextView::new(&state), app, cx)
}

pub(crate) fn render_view(
    id: &str,
    view: TextView,
    app: &ReviewApp,
    cx: &mut Context<ReviewApp>,
) -> gpui::AnyElement {
    let plugin = ImagePlugin {
        block: true,
        owner: cx.entity().downgrade(),
        host: id.to_owned().into(),
        images: Arc::new(OnceLock::new()),
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
        .plugin(mermaid::MermaidPlugin {
            host: id.to_owned().into(),
        })
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
        .parser_revision(4);
    div()
        .w_full()
        .min_w_0()
        .flex_shrink_0()
        .debug_selector({
            let id = id.to_owned();
            move || format!("{id}-text")
        })
        .child(
            view.image_source(crate::svg_image::markdown_source)
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
    fn reference_images_resolve_full_collapsed_shortcut_and_parent_links() {
        let source = "[![full][asset]][dest]\n\nBefore ![asset][] and ![asset] after.\n\n[asset]: https://example.com/first \"Title\"\n[ASSET]: https://example.com/ignored\n[dest]: https://example.com/full";
        let context = image_context(source);
        let root = ::markdown::to_mdast(source, &::markdown::ParseOptions::gfm()).unwrap();
        fn references<'a>(node: &'a Node, out: &mut Vec<&'a Node>) {
            if matches!(node, Node::ImageReference(_)) {
                out.push(node);
            }
            if let Some(children) = node.children() {
                for child in children {
                    references(child, out);
                }
            }
        }
        let mut nodes = Vec::new();
        references(&root, &mut nodes);
        assert_eq!(nodes.len(), 3);
        for (index, node) in nodes.into_iter().enumerate() {
            let images = image_nodes(node, &context).unwrap();
            assert_eq!(images.len(), 1);
            assert_eq!(images[0].target.url.as_ref(), "https://example.com/first");
            assert_eq!(
                images[0].target.alt.as_ref(),
                if index == 0 { "full" } else { "asset" }
            );
            assert_eq!(
                images[0].target.link.as_deref(),
                (index == 0).then_some("https://example.com/full")
            );
        }
    }

    #[test]
    fn reference_images_never_resolve_to_local_or_script_resources() {
        for url in [
            "file:///etc/passwd",
            "/tmp/image.png",
            "javascript:alert(1)",
            "data:image/png;base64,abc",
        ] {
            let source = format!("![image][ref]\n\n[ref]: {url}");
            let context = image_context(&source);
            let Node::Root(root) =
                ::markdown::to_mdast(&source, &::markdown::ParseOptions::gfm()).unwrap()
            else {
                panic!()
            };
            let Node::Paragraph(paragraph) = &root.children[0] else {
                panic!()
            };
            assert!(
                image_nodes(&paragraph.children[0], &context).is_none(),
                "{url}"
            );
        }
    }

    #[test]
    fn image_parent_links_do_not_claim_surrounding_text_or_code() {
        let source = "[before ![a](https://github.com/a) after](https://example.com/one)\n\n[![b](https://github.com/b)][dest]\n\n[dest]: https://example.com/two\n\n`![code](https://github.com/code)`\n\n<img src='https://github.com/html'>";
        let links = image_context(source).links;
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
