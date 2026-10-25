//! Native image loading with per-occurrence SVG source controls. All network
//! access goes through the app's authenticated, disk-cached HTTP client.

use std::{ops::Range, sync::Arc};

use futures::AsyncReadExt as _;
use gpui::{
    div, img, px, relative, App, Context, Image, ImageFormat, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, StyledImage, Task,
    Window,
};
use html5ever::{
    tendril::StrTendril,
    tokenizer::{BufferQueue, StartTag, TagToken, Token, TokenSink, TokenSinkResult, Tokenizer},
};
use http_client::AsyncBody;

use crate::http::is_svg;
use crate::theme;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Media {
    pub range: Range<usize>,
    pub url: String,
    pub alt: String,
    pub width: Option<f32>,
    pub link: Option<String>,
}

impl Media {
    pub fn new(range: Range<usize>, url: String, alt: String) -> Self {
        Self {
            range,
            url,
            alt,
            width: None,
            link: None,
        }
    }
}

pub(super) fn render(
    id: &str,
    media: &Media,
    window: &mut Window,
    cx: &mut App,
) -> gpui::AnyElement {
    render_with_height(id, media, None, window, cx)
}

/// Fixed-height host for the diff's uniform-row media reservations. SVG source
/// scrolls inside the host instead of changing source-line/display-row mapping.
pub(crate) fn render_inline(
    id: &str,
    media: &Media,
    height: f32,
    window: &mut Window,
    cx: &mut App,
) -> gpui::AnyElement {
    render_with_height(id, media, Some(height), window, cx)
}

fn render_with_height(
    id: &str,
    media: &Media,
    height: Option<f32>,
    window: &mut Window,
    cx: &mut App,
) -> gpui::AnyElement {
    // URL in the key also invalidates an in-flight load after an edited comment.
    let key = SharedString::from(format!("{id}-media-{}", media.url));
    let state = window.use_keyed_state(key, cx, |_, cx| MediaView::new(media.clone(), cx));
    state.update(cx, |state, _| {
        state.id = id.to_owned();
        state.media = media.clone();
        state.height = height;
    });
    state.into_any_element()
}

struct Svg {
    source: String,
    image: Arc<Image>,
}

struct MediaView {
    id: String,
    media: Media,
    loaded: Option<Result<Option<Svg>, ()>>,
    show_source: bool,
    height: Option<f32>,
    _load: Task<()>,
}

impl MediaView {
    fn new(media: Media, cx: &mut Context<Self>) -> Self {
        let url = media.url.clone();
        let client = cx.http_client();
        let load = cx.spawn(async move |this, cx| {
            let result = async {
                let response = client.get(&url, AsyncBody::empty(), true).await?;
                anyhow::ensure!(response.status().is_success(), "Image unavailable");
                let content_type = response
                    .headers()
                    .get("content-type")
                    .and_then(|h| h.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                let mut bytes = Vec::new();
                response.into_body().read_to_end(&mut bytes).await?;
                Ok::<_, anyhow::Error>(if is_svg(&content_type, &bytes) {
                    Some(Svg {
                        source: String::from_utf8(bytes.clone())?,
                        image: Arc::new(Image::from_bytes(ImageFormat::Svg, bytes)),
                    })
                } else {
                    None
                })
            }
            .await;
            let _ = this.update(cx, |this, cx| {
                // Do not expose signed URLs or HTTP errors containing credentials.
                this.loaded = Some(result.map_err(|_| ()));
                cx.notify();
            });
        });
        Self {
            id: String::new(),
            media,
            loaded: None,
            show_source: false,
            height: None,
            _load: load,
        }
    }
}

impl Render for MediaView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let id = self.id.clone();
        let mut block = div()
            .w_full()
            .min_w_0()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap_2()
            .debug_selector({
                let id = id.clone();
                move || format!("{id}-image")
            });
        let svg = match &self.loaded {
            Some(Ok(svg)) => svg.as_ref(),
            Some(Err(())) => {
                return block.child(
                    div()
                        .text_color(theme::overlay0())
                        .child(format!("Image unavailable: {}", self.media.alt)),
                )
            }
            None => {
                return block.child(div().text_color(theme::overlay0()).child("Loading image…"))
            }
        };
        if svg.is_some() {
            block = block.child(
                div()
                    .id("svg-toggle")
                    .debug_selector({
                        let id = id.clone();
                        move || format!("{id}-svg-toggle")
                    })
                    .cursor_pointer()
                    .text_color(theme::blue())
                    .child(if self.show_source {
                        "Show rendered SVG"
                    } else {
                        "Show SVG source"
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_source = !this.show_source;
                        cx.stop_propagation();
                        cx.notify();
                    })),
            );
        }
        if let Some(svg) = svg.filter(|_| self.show_source) {
            let mut source = div()
                .id("svg-source-scroll")
                .debug_selector({
                    let id = id.clone();
                    move || format!("{id}-svg-source")
                })
                .child(super::markdown::text_view(
                    &format!("{id}-source"),
                    fenced_source(&svg.source),
                    window,
                    cx,
                ));
            if let Some(height) = self.height {
                source = source.h(px((height - 48.).max(1.))).overflow_y_scroll();
            }
            block = block.child(source);
        } else {
            // Raster images keep gpui's native URL loader (including animation
            // and public user-attachments URLs). Its GET hits the disk cache
            // populated by the content-type probe above, not the network.
            let image = if let Some(svg) = svg {
                img(svg.image.clone())
            } else {
                img(self.media.url.clone())
            };
            let mut image = image
                .id("rendered")
                .max_w(relative(1.))
                .object_fit(gpui::ObjectFit::Contain)
                .debug_selector({
                    let id = id.clone();
                    move || format!("{id}-rendered")
                })
                .with_fallback(|| div().child("Image unavailable").into_any_element());
            if let Some(width) = self.media.width {
                image = image.w(px(width));
            }
            if let Some(height) = self.height {
                image = image.max_h(px((height - 48.).max(1.)));
            }
            let url = self
                .media
                .link
                .clone()
                .unwrap_or_else(|| self.media.url.clone());
            block = block.child(
                image
                    .cursor_pointer()
                    .on_click(move |_, _, cx| cx.open_url(&url)),
            );
        }
        block
    }
}

fn fenced_source(source: &str) -> String {
    let run = source.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(3.max(run + 1));
    format!("{fence}xml\n{source}\n{fence}")
}

/// GitHub's screenshot uploader also emits HTML `<img width="…" src="…">`.
/// Locate tags without regex (quoted `>` is legal), then let HTML5 decode
/// attributes/entities. Markdown code nodes never enter this function.
pub(super) fn html_images(html: &str, offset: usize) -> Vec<Media> {
    #[derive(Default)]
    struct Sink(Option<Media>);
    impl TokenSink for Sink {
        type Handle = ();
        fn process_token(&mut self, token: Token, _: u64) -> TokenSinkResult<()> {
            if let TagToken(tag) = token {
                if tag.kind == StartTag && tag.name.as_ref() == "img" {
                    let attr = |name: &str| {
                        tag.attrs
                            .iter()
                            .find(|a| a.name.local.as_ref() == name)
                            .map(|a| a.value.to_string())
                    };
                    if let Some(url) = attr("src") {
                        let mut media = Media::new(0..0, url, attr("alt").unwrap_or_default());
                        media.width = attr("width")
                            .and_then(|s| s.parse::<f32>().ok())
                            .filter(|w| w.is_finite() && *w > 0.);
                        self.0 = Some(media);
                    }
                }
            }
            TokenSinkResult::Continue
        }
    }
    let mut result = Vec::new();
    let mut cursor = 0;
    let mut raw_tag: Option<String> = None;
    while let Some(start) = html[cursor..].find('<').map(|i| cursor + i) {
        if html[start..].starts_with("<!--") {
            let Some(end) = html[start + 4..].find("-->") else {
                break;
            };
            cursor = start + 4 + end + 3;
            continue;
        }
        let mut quote = None;
        let end = html[start..].char_indices().find_map(|(i, c)| {
            match (quote, c) {
                (None, '\'' | '"') => quote = Some(c),
                (Some(q), c) if q == c => quote = None,
                (None, '>') => return Some(start + i + 1),
                _ => {}
            }
            None
        });
        let Some(end) = end else { break };
        cursor = end;
        let tag = &html[start..end];
        let lower = tag.to_ascii_lowercase();
        if let Some(raw) = &raw_tag {
            if lower.starts_with(&format!("</{raw}")) {
                raw_tag = None;
            }
            continue;
        }
        let name = lower[1..]
            .split(|c: char| !c.is_ascii_alphanumeric())
            .next()
            .unwrap_or_default();
        if matches!(name, "pre" | "code" | "script" | "style") {
            raw_tag = Some(name.into());
            continue;
        }
        if name != "img" {
            continue;
        }
        let mut input = BufferQueue::default();
        input.push_back(StrTendril::from(tag));
        let mut tokenizer = Tokenizer::new(Sink::default(), Default::default());
        let _ = tokenizer.feed(&mut input);
        tokenizer.end();
        if let Some(mut media) = tokenizer.sink.0 {
            media.range = offset + start..offset + end;
            if is_remote_image(&media.url) {
                result.push(media);
            }
        }
    }
    result
}

pub(super) fn is_remote_image(url: &str) -> bool {
    http_client::Url::parse(url).is_ok_and(|u| {
        matches!(u.scheme(), "https" | "http")
            && u.host_str().is_some()
            && u.username().is_empty()
            && u.password().is_none()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_screenshots_decode_attributes_and_ignore_comments_and_code() {
        let html = r#"<!-- <img src="https://bad/a"> --><pre><img src="https://bad/b"></pre><IMG width='640' alt='a &amp; b > c' src="https://github.com/user-attachments/assets/123?x=1&amp;y=2">"#;
        let images = html_images(html, 10);
        assert_eq!(images.len(), 1);
        let image = &images[0];
        assert_eq!(image.alt, "a & b > c");
        assert_eq!(image.width, Some(640.));
        assert!(image.url.ends_with("?x=1&y=2"));
        assert!(html[image.range.start - 10..image.range.end - 10].starts_with("<IMG"));
        assert!(html_images(
            r#"<img src="file:///etc/passwd"><img src="javascript:alert(1)">"#,
            0
        )
        .is_empty());
    }

    #[test]
    fn svg_detection_and_source_preserve_literal_content() {
        assert!(is_svg("image/svg+xml; charset=utf-8", b"<svg/>"));
        assert!(is_svg(
            "application/octet-stream",
            b"<?xml version='1.0'?><svg/>"
        ));
        assert!(!is_svg("image/png", b"PNG"));
        let source = "<svg><!-- ``` @literal #12 :rocket: --></svg>";
        let root =
            ::markdown::to_mdast(&fenced_source(source), &::markdown::ParseOptions::gfm()).unwrap();
        let ::markdown::mdast::Node::Code(code) = &root.children().unwrap()[0] else {
            panic!("code")
        };
        assert_eq!(code.value, source);
    }
}
