//! Local Mermaid blocks. Parsing only captures source; layout, SVG admission and
//! rasterization run on bounded workers. Neither PR URLs nor SVG reach a loader.
use gpui::component::{
    button::{Button, ButtonVariants},
    text::{MarkdownNode, MarkdownParseContext, MarkdownPlugin, TextView},
    ActiveTheme, Sizable,
};
use gpui::{
    div, img, prelude::*, px, App, ClipboardItem, Context, Entity, Global, MouseButton,
    RenderImage, SharedString, Window,
};
use markdown::mdast::Node;
use merman::{OperationControl, RenderOutput, RenderRequest, Renderer, SvgRequest};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

const REVISION: u8 = 1;
const FONT: &str = "Arial";
const MAX_SOURCE: usize = 16 * 1024;
const MAX_SVG: usize = 512 * 1024;
const MAX_PIXELS: u64 = 4 * 1024 * 1024;
const MAX_SIDE: u32 = 4096;
const MAX_CACHE: usize = 64 * 1024 * 1024;
const MAX_ENTRIES: usize = 128;
const MAX_JOBS: usize = 16;
const CONCURRENCY: usize = 2;
const DEADLINE: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub(super) struct MermaidPlugin {
    pub host: SharedString,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct Key {
    source: Arc<str>,
    dark: bool,
    font: &'static str,
    scale: u8,
    // Explicitly version layout/security/fonts/raster policy together. The font
    // is host-fixed, not inherited from prose or controlled by PR directives.
    revision: u8,
}

#[derive(Clone)]
struct Diagram {
    image: Arc<RenderImage>,
    width: f32,
    height: f32,
    cost: usize,
}

type Outcome = Result<Diagram, Arc<str>>;

struct Entry {
    result: Option<Outcome>,
    touched: u64,
    cost: usize,
}

#[derive(Default)]
struct Hub {
    entries: HashMap<Key, Entry>,
    queue: VecDeque<Key>,
    running: usize,
    clock: u64,
    bytes: usize,
    #[cfg(test)]
    started: usize,
}
struct MermaidGlobal(Entity<Hub>);
impl Global for MermaidGlobal {}

fn hub(cx: &mut App) -> Entity<Hub> {
    if !cx.has_global::<MermaidGlobal>() {
        let hub = cx.new(|_| Hub::default());
        cx.set_global(MermaidGlobal(hub));
    }
    cx.global::<MermaidGlobal>().0.clone()
}

impl Hub {
    fn lookup(&mut self, key: &Key) -> Option<Outcome> {
        self.clock += 1;
        let entry = self.entries.get_mut(key)?;
        entry.touched = self.clock;
        entry.result.clone()
    }

    fn evict(&mut self, cx: &mut App) {
        while self.bytes > MAX_CACHE || self.entries.len() > MAX_ENTRIES {
            let Some(key) = self
                .entries
                .iter()
                .filter(|(_, entry)| entry.result.is_some())
                .min_by_key(|(_, entry)| entry.touched)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            let entry = self.entries.get_mut(&key).unwrap();
            if self.bytes > MAX_CACHE && matches!(entry.result, Some(Ok(_))) {
                // Retain a small negative entry on memory eviction. Otherwise
                // several visible large diagrams can continuously evict and
                // re-layout one another on every frame.
                if let Some(Ok(diagram)) = entry.result.take() {
                    // A request may evict during Window::draw, when that window
                    // is temporarily removed from App. Drop textures only after
                    // it has returned, so the current window is cleaned too.
                    cx.defer(move |cx| cx.drop_image(diagram.image, None));
                }
                self.bytes -= entry.cost;
                entry.cost = key.source.len() + 512;
                entry.result = Some(Err("Mermaid cache memory limit — source shown".into()));
                self.bytes += entry.cost;
            } else {
                let entry = self.entries.remove(&key).unwrap();
                self.bytes -= entry.cost;
                if let Some(Ok(diagram)) = entry.result {
                    cx.defer(move |cx| cx.drop_image(diagram.image, None));
                }
            }
        }
    }

    fn request(&mut self, key: Key, cx: &mut Context<Self>) -> Option<Outcome> {
        if self.entries.contains_key(&key) {
            return self.lookup(&key);
        }
        // Admission failures and renderer failures are both cached. Hashing or
        // retaining arbitrarily large blocks is avoided by the plugin first.
        if self.queue.len() + self.running >= MAX_JOBS {
            return Some(Err("Mermaid queue full — source shown".into()));
        }
        self.clock += 1;
        let cost = key.source.len() + 256;
        self.bytes += cost;
        self.entries.insert(
            key.clone(),
            Entry {
                result: None,
                touched: self.clock,
                cost,
            },
        );
        self.queue.push_back(key);
        self.evict(cx);
        self.pump(cx);
        None
    }

    fn pump(&mut self, cx: &mut Context<Self>) {
        while self.running < CONCURRENCY {
            let Some(key) = self.queue.pop_front() else {
                break;
            };
            self.running += 1;
            #[cfg(test)]
            {
                self.started += 1;
            }
            let work_key = key.clone();
            let task = cx.background_executor().spawn(async move {
                // A backend panic must not strand the queue or turn into a
                // blank diagram. This is not a process isolation boundary.
                std::panic::catch_unwind(|| {
                    render_diagram(&work_key, OperationControl::new().with_deadline(DEADLINE))
                })
                .unwrap_or_else(|_| Err("Mermaid renderer failed — source shown".into()))
            });
            cx.spawn(async move |this, cx| {
                let result = task.await;
                let _ = this.update(cx, |hub, cx| {
                    hub.running -= 1;
                    if let Some(entry) = hub.entries.get_mut(&key) {
                        let added = result
                            .as_ref()
                            .map_or_else(|error| error.len(), |diagram| diagram.cost);
                        entry.cost += added;
                        entry.result = Some(result);
                        hub.bytes += added;
                    }
                    hub.evict(cx);
                    hub.pump(cx);
                    // No result is written to a document or active PR. Each
                    // frame reads its exact source/config key, so old results
                    // cannot replace edited text or a different PR's diagram.
                    cx.refresh_windows();
                    cx.notify();
                });
            })
            .detach();
        }
    }
}

fn code(node: &Node) -> Option<&str> {
    match node {
        Node::Code(code) if code.lang.as_deref() == Some("mermaid") => Some(&code.value),
        _ => None,
    }
}

impl MarkdownPlugin for MermaidPlugin {
    fn name(&self) -> &str {
        "gander-mermaid"
    }
    fn is_block(&self) -> bool {
        true
    }
    fn parse(&self, node: &Node, cx: &MarkdownParseContext<'_>) -> Option<MarkdownNode> {
        let source = code(node)?;
        // Oversized input stays entirely stock, with no copy/cache allocation.
        if source.len() > MAX_SOURCE {
            return None;
        }
        Some(
            MarkdownNode::new(self.name(), Arc::<str>::from(source))
                .text(source.to_owned())
                .markdown(cx.node_source(node).unwrap_or_default().to_owned())
                .accessibility_label(format!("Mermaid diagram source: {source}")),
        )
    }
    fn render(&self, node: &MarkdownNode, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let key = Key {
            source: node.data::<Arc<str>>().unwrap().clone(),
            dark: cx.theme().mode.is_dark(),
            font: FONT,
            scale: window.scale_factor().ceil().clamp(1., 3.) as u8,
            revision: REVISION,
        };
        let result = hub(cx).update(cx, |hub, cx| hub.request(key, cx));
        let offset = node.source_range().map_or(0, |range| range.start);
        let id = SharedString::from(format!("{}-mermaid-{offset}", self.host));
        let source_view = || {
            TextView::markdown(
                SharedString::from(format!("{id}-source")),
                source_markdown(node.as_text()),
            )
            .w_full()
            .h_auto()
            .scrollable(false)
            .selectable(true)
        };
        let mut card = div()
            .id(id.clone())
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_1()
            .debug_selector({
                let id = id.clone();
                move || id.to_string()
            });
        match result {
            Some(Ok(diagram)) => {
                let expanded = window.use_keyed_state(id.clone(), cx, |_, _| false);
                let open = *expanded.read(cx);
                let copy = node.as_text().to_owned();
                let fenced = node.as_markdown().to_owned();
                card = card
                    .child(
                        img(diagram.image)
                            .id("diagram")
                            .debug_selector({
                                let id = id.clone();
                                move || format!("{id}-diagram")
                            })
                            .w(px(diagram.width))
                            .max_w_full()
                            .aspect_ratio(diagram.width / diagram.height)
                            .object_fit(gpui::ObjectFit::Contain)
                            .with_fallback(|| {
                                div()
                                    .child("Diagram image unavailable — use Source")
                                    .into_any_element()
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_1()
                            .text_xs()
                            .child(
                                Button::new("source")
                                    .label(if open { "Hide source" } else { "Source" })
                                    .small()
                                    .ghost()
                                    .debug_selector({
                                        let id = id.clone();
                                        move || format!("{id}-source-toggle")
                                    })
                                    .on_mouse_down(MouseButton::Left, |_, window, cx| {
                                        window.prevent_default();
                                        cx.stop_propagation();
                                    })
                                    .on_click(move |_, _, cx| {
                                        cx.stop_propagation();
                                        expanded.update(cx, |open, cx| {
                                            *open = !*open;
                                            cx.notify();
                                        });
                                    }),
                            )
                            .child(
                                Button::new("copy")
                                    .label("Copy source")
                                    .small()
                                    .ghost()
                                    .debug_selector({
                                        let id = id.clone();
                                        move || format!("{id}-copy")
                                    })
                                    .on_mouse_down(MouseButton::Left, |_, window, cx| {
                                        window.prevent_default();
                                        cx.stop_propagation();
                                    })
                                    .on_click(move |_, _, cx| {
                                        cx.stop_propagation();
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            copy.clone(),
                                        ));
                                    }),
                            )
                            .child(
                                Button::new("copy-markdown")
                                    .label("Copy Markdown")
                                    .debug_selector({
                                        let id = id.clone();
                                        move || format!("{id}-copy-markdown")
                                    })
                                    .small()
                                    .ghost()
                                    .on_mouse_down(MouseButton::Left, |_, window, cx| {
                                        window.prevent_default();
                                        cx.stop_propagation();
                                    })
                                    .on_click(move |_, _, cx| {
                                        cx.stop_propagation();
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            fenced.clone(),
                                        ));
                                    }),
                            ),
                    );
                if open {
                    card = card.child(source_view());
                }
            }
            result => {
                let message: SharedString = result.map_or_else(
                    || "Rendering Mermaid…".into(),
                    |result| result.err().unwrap().to_string().into(),
                );
                card = card
                    .child(div().text_xs().child(message))
                    .child(source_view());
            }
        }
        card
    }
}

// A standalone stock-code view must use mdast's de-indented value, not the
// original nested quote/list prefixes. Copy Markdown still uses the exact slice.
fn source_markdown(source: &str) -> String {
    let fence = "~".repeat(
        source
            .split(|c| c != '~')
            .map(str::len)
            .max()
            .unwrap_or(0)
            .max(2)
            + 1,
    );
    format!("{fence}mermaid\n{source}\n{fence}")
}

/// Conservative admission: configuration/frontmatter, navigation, styling and
/// resource-bearing syntax remain stock code. Do not silently strip semantics.
fn admit(source: &str) -> Result<(), Arc<str>> {
    if source.len() > MAX_SOURCE {
        return Err("Mermaid source limit exceeded".into());
    }
    if source.trim().is_empty() {
        return Err("Empty Mermaid block".into());
    }
    if source
        .lines()
        .any(|line| line.trim().trim_start_matches('\u{feff}') == "---")
        || source.contains("%%{")
        || source
            .lines()
            .any(|line| line.trim_start().starts_with("%%") && line.contains('{'))
    {
        return Err("Mermaid configuration is not supported — source shown".into());
    }
    let lower = source.to_ascii_lowercase();
    if lower
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|word| {
            matches!(
                word,
                "click"
                    | "link"
                    | "links"
                    | "callback"
                    | "href"
                    | "img"
                    | "image"
                    | "icon"
                    | "style"
                    | "classdef"
                    | "cssclass"
            )
        })
        || lower.contains("<")
            && [
                "<a", "<img", "<style", "<script", "<iframe", "<svg", "<font",
            ]
            .iter()
            .any(|tag| lower.contains(tag))
    {
        return Err(
            "Mermaid resources, links or custom styles are not supported — source shown".into(),
        );
    }
    Ok(())
}

fn resource_policy() -> merman::svg::RenderResourcePolicy {
    use merman::svg::{RenderResourcePolicy, ResourceLimitId as L};
    let mut policy = RenderResourcePolicy::constrained();
    for (id, value) in [
        (L::MaxSourceBytes, MAX_SOURCE),
        (L::MaxModelItems, 256),
        (L::MaxModelTextBytes, MAX_SOURCE),
        (L::MaxModelNestingDepth, 16),
        (L::MaxLayoutWorkUnits, 200_000),
        (L::MaxPreparedTextRetainedBytes, 2 * 1024 * 1024),
        (L::MaxSvgBytes, MAX_SVG),
        (L::MaxSvgElements, 10_000),
    ] {
        policy.apply_limit(id, value).expect("host resource policy");
    }
    policy
}

fn render_diagram(key: &Key, control: OperationControl) -> Outcome {
    admit(&key.source)?;
    control
        .checkpoint()
        .map_err(|_| Arc::<str>::from("Mermaid cancelled"))?;
    let start = Instant::now();
    let fonts = native_fonts()?.for_operation(&control);
    // Only trusted host configuration enters the engine. Source directives are
    // rejected above; finite resource ceilings remain outside Mermaid config.
    let config = merman::MermaidConfig::from_value(serde_json::json!({
        "theme": if key.dark { "dark" } else { "default" },
        "fontFamily": key.font, "look": "classic", "securityLevel": "strict",
        "flowchart": { "htmlLabels": false }
    }));
    let renderer = Renderer::new()
        .with_semantic_parser(merman::SemanticParser::new().with_site_config(config));
    let request = SvgRequest {
        pipeline: Some(merman::svg::SvgPipeline::resvg_safe()),
        environment: merman::SvgEnvironment::deterministic()
            .with_resource_policy(resource_policy())
            .with_native_font_context(fonts.clone())
            .map_err(|_| {
                Arc::<str>::from("Mermaid font configuration unavailable — source shown")
            })?,
        ..Default::default()
    };
    let output = renderer
        .render(RenderRequest::svg(&key.source, control.clone(), request))
        .map_err(|_| {
            Arc::<str>::from("Mermaid invalid, unsupported or over limit — source shown")
        })?;
    let RenderOutput::Svg(Some(output)) = output else {
        return Err("Mermaid produced no diagram — source shown".into());
    };
    control
        .checkpoint()
        .map_err(|_| Arc::<str>::from("Mermaid deadline exceeded — source shown"))?;
    let diagram = rasterize(output.svg(), key, &fonts)?;
    if start.elapsed() > DEADLINE {
        return Err("Mermaid deadline exceeded — source shown".into());
    }
    Ok(diagram)
}

/// Reject rather than remove unsupported output: stripping could change what
/// the author meant. roxmltree forbids DTDs by default (including entities).
fn audit_svg(svg: &str) -> Result<roxmltree::Document<'_>, Arc<str>> {
    let bad = || Arc::<str>::from("Mermaid SVG outside host safety policy — source shown");
    if svg.len() > MAX_SVG {
        return Err(bad());
    }
    let doc = roxmltree::Document::parse(svg).map_err(|_| bad())?;
    if doc.descendants().any(|node| node.is_pi()) {
        return Err(bad());
    }
    let mut count = 0;
    for node in doc.descendants().filter(|node| node.is_element()) {
        count += 1;
        if count > 10_000 || node.ancestors().count() > 64 {
            return Err(bad());
        }
        if !matches!(
            node.tag_name().name(),
            "svg"
                | "g"
                | "defs"
                | "marker"
                | "path"
                | "rect"
                | "circle"
                | "ellipse"
                | "polygon"
                | "polyline"
                | "line"
                | "text"
                | "tspan"
                | "style"
                | "title"
                | "desc"
                | "clipPath"
                | "filter"
                | "feDropShadow"
                | "use"
                | "symbol"
                | "linearGradient"
                | "radialGradient"
                | "stop"
        ) {
            return Err(bad());
        }
        for attr in node.attributes() {
            if attr.name().to_ascii_lowercase().starts_with("on") {
                return Err(bad());
            }
            if attr.name() == "href" && !fragment(attr.value()) {
                return Err(bad());
            }
            if !safe_css(attr.value()) {
                return Err(bad());
            }
        }
        if node.tag_name().name() == "style" {
            if node.children().any(|child| !child.is_text()) {
                return Err(bad());
            }
            let css = node
                .children()
                .filter_map(|child| child.text())
                .collect::<String>();
            if !safe_css(&css) {
                return Err(bad());
            }
        }
        // The only filter emitted by our classic host theme is a small fixed
        // drop shadow definition. Bound even unused filter buffer geometry.
        if node.tag_name().name() == "filter" {
            for dimension in ["width", "height"] {
                if !matches!(
                    node.attribute(dimension),
                    None | Some("130%") | Some("150%")
                ) {
                    return Err(bad());
                }
            }
            if node
                .children()
                .filter(|n| n.is_element())
                .any(|n| n.tag_name().name() != "feDropShadow")
            {
                return Err(bad());
            }
        }
        if node.tag_name().name() == "feDropShadow" {
            for (name, max) in [("dx", 8.), ("dy", 8.), ("stdDeviation", 2.)] {
                let value = node
                    .attribute(name)
                    .unwrap_or("0")
                    .parse::<f32>()
                    .map_err(|_| bad())?;
                if !value.is_finite() || value.abs() > max {
                    return Err(bad());
                }
            }
        }
    }
    // Bound reference expansion *before* usvg allocation. Reject recursive or
    // deeply nested <use>, including references to an ancestor/root.
    fn expanded(
        node: roxmltree::Node<'_, '_>,
        doc: &roxmltree::Document<'_>,
        depth: usize,
        budget: &mut usize,
    ) -> Option<()> {
        if depth > 64 || *budget == 0 {
            return None;
        }
        *budget -= 1;
        if let Some(href) = node
            .attribute("href")
            .or_else(|| node.attribute(("http://www.w3.org/1999/xlink", "href")))
        {
            let target = doc
                .descendants()
                .find(|n| n.attribute("id") == href.strip_prefix('#'))?;
            expanded(target, doc, depth + 1, budget)?;
        }
        for child in node.children() {
            expanded(child, doc, depth + 1, budget)?;
        }
        Some(())
    }
    expanded(doc.root(), &doc, 0, &mut 20_000).ok_or_else(bad)?;
    Ok(doc)
}

fn fragment(value: &str) -> bool {
    value.strip_prefix('#').is_some_and(|id| {
        !id.is_empty()
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'))
    })
}
fn safe_css(value: &str) -> bool {
    // Reject CSS escape/comment/import obfuscation. Only plain local url(#id)
    // references can reach usvg; no external stylesheet/font/image sources.
    if value.contains(['\\', '@']) || value.contains("/*") {
        return false;
    }
    let lower = value.to_ascii_lowercase();
    let mut tail = lower.as_str();
    while let Some(index) = tail.find("url") {
        tail = tail[index + 3..].trim_start();
        let Some(rest) = tail.strip_prefix('(') else {
            continue;
        };
        let Some((url, rest)) = rest.split_once(')') else {
            return false;
        };
        if !fragment(url.trim().trim_matches(['\'', '"'])) {
            return false;
        }
        tail = rest;
    }
    true
}

fn raster_dimensions(width: f32, height: f32, scale: u8) -> Option<(u32, u32)> {
    if !width.is_finite() || !height.is_finite() || width <= 1. || height <= 1. {
        return None;
    }
    let w = (width * f32::from(scale)).ceil();
    let h = (height * f32::from(scale)).ceil();
    if w > MAX_SIDE as f32 || h > MAX_SIDE as f32 || w * h > MAX_PIXELS as f32 {
        return None;
    }
    Some((w as u32, h as u32))
}

fn native_fonts() -> Result<merman::svg::export::NativeFontContext, Arc<str>> {
    use merman::svg::export::{NativeFontContext, NativeFontLimits, NativeFontOptions};
    static FONTS: OnceLock<Result<NativeFontContext, Arc<str>>> = OnceLock::new();
    FONTS
        .get_or_init(|| {
            // Gander targets macOS. Use a closed, host-owned set of OS font files,
            // not discovery or PR-provided paths. Arial Unicode is optional; labels
            // outside the admitted glyph coverage fall back instead of using tofu.
            let limits = NativeFontLimits {
                max_files: 8,
                max_faces: 8,
                max_bytes: 32 * 1024 * 1024,
                max_label_bytes: MAX_SOURCE,
                max_measurements: 2048,
                max_cache_entries: 256,
                max_cache_bytes: 1024 * 1024,
                ..Default::default()
            };
            let mut options = NativeFontOptions::default().with_limits(limits);
            for name in [
                "Arial.ttf",
                "Arial Bold.ttf",
                "Arial Italic.ttf",
                "Arial Bold Italic.ttf",
                "Arial Unicode.ttf",
            ] {
                let path = std::path::Path::new("/System/Library/Fonts/Supplemental").join(name);
                if name != "Arial Unicode.ttf" || path.is_file() {
                    options = options.with_font_file(path);
                }
            }
            options
                .load(&OperationControl::new().with_deadline(DEADLINE))
                .map(|fonts| fonts.with_strict_glyph_coverage(true))
                .map_err(|_| Arc::<str>::from("Mermaid native fonts unavailable — source shown"))
        })
        .clone()
}

fn rasterize(svg: &str, key: &Key, fonts: &merman::svg::export::NativeFontContext) -> Outcome {
    audit_svg(svg)?;
    use resvg::{tiny_skia, usvg};
    let mut options = usvg::Options {
        resources_dir: None,
        image_href_resolver: usvg::ImageHrefResolver {
            resolve_data: Box::new(|_, _, _| None),
            resolve_string: Box::new(|_, _| None),
        },
        ..Default::default()
    };
    fonts.apply_to_usvg_options(&mut options);
    let tree = usvg::Tree::from_str(svg, &options)
        .map_err(|_| Arc::<str>::from("Mermaid SVG could not be decoded — source shown"))?;
    fonts.check().map_err(|_| {
        Arc::<str>::from("Mermaid glyph coverage or font limit exceeded — source shown")
    })?;
    let width = tree.size().width();
    let height = tree.size().height();
    fn text_is_complete(group: &usvg::Group, width: f32, height: f32) -> bool {
        group.children().iter().all(|node| match node {
            usvg::Node::Group(group) => text_is_complete(group, width, height),
            usvg::Node::Text(text) => {
                let bounds = text.abs_bounding_box();
                text.layouted()
                    .iter()
                    .all(|span| span.positioned_glyphs.iter().all(|glyph| glyph.id.0 != 0))
                    && bounds.left() >= -1.
                    && bounds.top() >= -1.
                    && bounds.right() <= width + 1.
                    && bounds.bottom() <= height + 1.
            }
            _ => true,
        })
    }
    if !text_is_complete(tree.root(), width, height) {
        return Err("Mermaid has missing or clipped labels — source shown".into());
    }
    let (w, h) = raster_dimensions(width, height, key.scale)
        .ok_or_else(|| Arc::<str>::from("Mermaid raster size limit exceeded — source shown"))?;
    let mut pixmap = tiny_skia::Pixmap::new(w, h)
        .ok_or_else(|| Arc::<str>::from("Mermaid raster allocation failed — source shown"))?;
    pixmap.fill(if key.dark {
        tiny_skia::Color::from_rgba8(31, 32, 36, 255)
    } else {
        tiny_skia::Color::WHITE
    });
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(f32::from(key.scale), f32::from(key.scale)),
        &mut pixmap.as_mut(),
    );
    // The opaque host background makes premultiplied RGBA ordinary RGBA.
    // GPUI expects BGRA. Pass already-decoded pixels, bypassing its persistent
    // asset/decoder cache altogether; eviction explicitly drops GPU textures.
    let mut pixels = pixmap.take();
    if pixels
        .as_chunks::<4>()
        .0
        .iter()
        .all(|pixel| pixel == &pixels[..4])
    {
        return Err("Mermaid produced an empty diagram — source shown".into());
    }
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    let buffer = image::RgbaImage::from_raw(w, h, pixels)
        .ok_or_else(|| Arc::<str>::from("Mermaid pixel dimensions invalid"))?;
    let image = Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]));
    let cost = w as usize * h as usize * 4 * 2;
    Ok(Diagram {
        image,
        width,
        height,
        cost,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_base::text::{
        editor_host::{parse_markdown, BlockNode},
        MarkdownExtensions,
    };

    fn key(source: &str) -> Key {
        Key {
            source: source.into(),
            dark: false,
            font: FONT,
            scale: 1,
            revision: REVISION,
        }
    }
    fn render(source: &str) -> Outcome {
        render_diagram(
            &key(source),
            OperationControl::new().with_deadline(DEADLINE),
        )
    }
    fn custom_nodes(blocks: &[BlockNode]) -> Vec<MarkdownNode> {
        let mut result = Vec::new();
        for block in blocks {
            match block {
                BlockNode::Custom { node, .. } => result.push(node.clone()),
                BlockNode::Root { children, .. }
                | BlockNode::Blockquote { children, .. }
                | BlockNode::List { children, .. }
                | BlockNode::ListItem { children, .. } => result.extend(custom_nodes(children)),
                _ => {}
            }
        }
        result
    }
    fn parse(source: &str) -> Vec<BlockNode> {
        parse_markdown(
            source,
            Arc::new(
                MarkdownExtensions::default()
                    .plugin(MermaidPlugin {
                        host: "test".into(),
                    })
                    .parser_revision(3),
            ),
        )
        .unwrap()
    }

    #[test]
    fn mermaid_parser_is_explicit_and_preserves_plain_and_fenced_source() {
        let fenced = "~~~mermaid extra\nflowchart LR\n  A[你好 🦀] --> B\n~~~";
        let nodes = custom_nodes(&parse(fenced));
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].as_markdown(), fenced);
        assert_eq!(nodes[0].as_text(), "flowchart LR\n  A[你好 🦀] --> B");
        assert_eq!(nodes[0].source_range(), Some(0..fenced.len()));
        assert!(nodes[0].accessibility_name().contains("你好 🦀"));
        for source in [
            "`mermaid`",
            "flowchart LR\nA-->B",
            "```rust\nflowchart LR\n```",
            "```Mermaid\nflowchart LR\n```",
            "```mermaid-js\nflowchart LR\n```",
        ] {
            assert!(custom_nodes(&parse(source)).is_empty(), "{source}");
        }
        let nested = "> ```mermaid\n> flowchart LR\n> A-->B\n> ```\n\n- nested\n\n  ~~~mermaid\n  sequenceDiagram\n  A->>B: Hi\n  ~~~";
        let nodes = custom_nodes(&parse(nested));
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].as_text(), "flowchart LR\nA-->B");
        assert!(
            custom_nodes(&parse(&source_markdown(nodes[0].as_text())))[0].as_text()
                == nodes[0].as_text()
        );
        let tricky = "flowchart LR\nA[\"~~~ ~~~~~\"]";
        assert_eq!(
            custom_nodes(&parse(&source_markdown(tricky)))[0].as_text(),
            tricky
        );
        assert!(custom_nodes(&parse(&format!(
            "```mermaid\n{}\n```",
            "A".repeat(MAX_SOURCE + 1)
        )))
        .is_empty());
    }

    #[test]
    fn mermaid_core_fixtures_produce_nonblank_native_rasters() {
        for source in [
            "flowchart LR\nA[Start] -->|yes| B{Review}\nB --> C[Done]\nC --> A",
            "flowchart TD\nsubgraph Review\nA[café Δ] --> B[\"<b>Bold</b><br/>Next\"]\nend",
            "flowchart LR\nA@{ shape: rect, label: \"Start\" } --> B",
            "sequenceDiagram\nAlice->>Bob: Hello\nBob-->>Alice: Hi",
            "classDiagram\nAnimal <|-- Duck\nAnimal : +int age\nAnimal : +run()",
            "stateDiagram-v2\n[*] --> Review\nReview --> Done\nDone --> [*]",
            "erDiagram\nUSER ||--o{ REVIEW : writes\nUSER { string name }",
        ] {
            for dark in [false, true] {
                let mut key = key(source);
                key.dark = dark;
                let diagram = render_diagram(&key, OperationControl::new().with_deadline(DEADLINE))
                    .unwrap_or_else(|err| panic!("{source} (dark={dark}): {err}"));
                assert!(diagram.width > 10. && diagram.height > 10.);
                let pixels = diagram.image.as_bytes(0).unwrap();
                assert!(
                    pixels
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .any(|pixel| pixel != &pixels[..4]),
                    "blank raster: {source}"
                );
            }
        }
    }

    #[test]
    fn mermaid_optional_cjk_font_never_degrades_to_missing_glyph_boxes() {
        let result = render("flowchart LR\nA[你好] --> B");
        if native_fonts().unwrap().face_count() > 4 {
            assert!(result.is_ok());
        } else {
            assert!(result.is_err(), "without the optional font, show source");
        }
    }

    #[test]
    fn mermaid_invalid_unsupported_and_resource_limited_inputs_fall_back() {
        for source in [
            "",
            "flowchart LR",
            "flowchart LR\n%% comment only",
            "flowchart LR\nA[unclosed --> B",
            "unknownDiagram\nA-->B",
            "pie\n\"A\": 1",
            // Unsupported glyphs must be readable source, not missing-glyph boxes.
            "flowchart LR\nA[🦀] --> B",
            "classDiagram\nclass \"🦀\" as Crab",
        ] {
            assert!(render(source).is_err(), "{source}");
        }
        assert!(render(&"x".repeat(MAX_SOURCE + 1)).is_err());
        let graph = (0..300)
            .map(|i| format!("A{i}-->A{}\n", i + 1))
            .collect::<String>();
        assert!(render(&format!("flowchart LR\n{graph}")).is_err());
        for size in [
            (f32::NAN, 10.),
            (10., f32::INFINITY),
            (0., 10.),
            (5000., 10.),
            (2048., 2048.),
        ] {
            assert!(raster_dimensions(size.0, size.1, 2).is_none());
        }
        assert_eq!(raster_dimensions(100., 80., 2), Some((200, 160)));
        assert!(raster_dimensions(2048., 2048., 1).is_some());
        let control = OperationControl::new();
        let fonts = native_fonts().unwrap().for_operation(&control);
        let result = rasterize("<svg xmlns='http://www.w3.org/2000/svg' width='5000' height='80'><rect width='20' height='20'/></svg>", &key("flowchart LR\nA-->B"), &fonts);
        assert!(result.err().unwrap().contains("raster size limit"));
    }

    #[test]
    fn mermaid_host_policy_rejects_resources_navigation_and_config() {
        for source in [
            "flowchart LR\nA@{img: 'file:///etc/passwd', shape: image}",
            "flowchart LR\nA@{img: 'https://example.com/a.svg', shape: image}",
            "flowchart LR\nA-->B\nclick A callback",
            "flowchart LR\nA-->B\nclick A \"https://example.com\"",
            "sequenceDiagram\nlinks Alice: {\"Site\": \"file:///tmp/a\"}",
            "%%{init: {'securityLevel':'loose', 'maxEdges':9999999}}%%\nflowchart LR\nA-->B",
            "%% {init: {}} %%\nflowchart LR\nA-->B",
            "---\nconfig:\n  securityLevel: loose\n---\nflowchart LR\nA-->B",
            "flowchart LR\nA[\"<img src='https://example.com/a'>\"]",
            "flowchart LR\nA-->B\nclassDef default font-family:url(file:///tmp/a)",
        ] {
            assert!(admit(source).is_err(), "{source}");
        }
        assert!(admit("flowchart LR\nA[\"https://example.com is plain text\"]").is_ok());
        let policy = resource_policy();
        assert!(merman::svg::ResourceLimitId::ALL
            .into_iter()
            .all(|id| policy.value(id).is_some()));
    }

    #[test]
    fn mermaid_svg_audit_fails_closed_before_any_decoder() {
        for body in [
            "<image href='file:///etc/passwd'/>",
            "<image href='/tmp/x'/>",
            "<image href='https://example.com/x'/>",
            "<image href='data:image/svg+xml;base64,eA=='/>",
            "<a href='https://example.com'><text>x</text></a>",
            "<script/>",
            "<foreignObject/>",
            "<rect onclick='callback()'/>",
            "<style>@font-face {font-family:x;src:url(file:///tmp/x)}</style>",
            "<style>rect {fill:uRl(https://example.com/x)}</style>",
            "<style>rect {fill:u\\72l(file:///tmp/x)}</style>",
            "<use href='file:///tmp/x#id'/>",
            "<g id='loop'><use href='#loop'/></g>",
            "<linearGradient id='loop' href='#loop'/>",
            "<style>rect {}<!-- split -->@font-face{src:url(file:///tmp/font)}</style>",
            "<?xml-stylesheet href='file:///tmp/style.css'?>",
            "<filter width='99999999%'/>",
            "<filter><feDropShadow stdDeviation='100000'/></filter>",
        ] {
            assert!(
                audit_svg(&format!(
                    "<svg xmlns='http://www.w3.org/2000/svg' width='10' height='10'>{body}</svg>"
                ))
                .is_err(),
                "{body}"
            );
        }
        assert!(audit_svg(
            "<!DOCTYPE svg [<!ENTITY x SYSTEM 'file:///etc/passwd'>]><svg>&x;</svg>"
        )
        .is_err());
        assert!(audit_svg(
            "<svg><defs><g id='label'><text>hello</text></g></defs><use href='#label'/></svg>"
        )
        .is_ok());
    }

    #[test]
    fn mermaid_theme_dpi_revision_keys_and_cancellation_are_explicit() {
        let light = key("flowchart LR\nA-->B");
        let mut dark = light.clone();
        dark.dark = true;
        let mut retina = light.clone();
        retina.scale = 2;
        let mut revised = light.clone();
        revised.revision += 1;
        let mut other_font = light.clone();
        other_font.font = "Arial Unicode MS";
        for other in [&dark, &retina, &revised, &other_font] {
            assert_ne!(&light, other);
        }
        let a = render_diagram(&light, OperationControl::new()).unwrap();
        let b = render_diagram(&dark, OperationControl::new()).unwrap();
        assert_ne!(a.image.as_bytes(0), b.image.as_bytes(0));
        let c = render_diagram(&retina, OperationControl::new()).unwrap();
        assert_eq!(c.width, a.width);
        assert!(c.image.as_bytes(0).unwrap().len() >= a.image.as_bytes(0).unwrap().len() * 3);
        let cancelled = OperationControl::new();
        cancelled.cancel();
        assert!(render_diagram(&light, cancelled).is_err());
        let expired = OperationControl::new().with_deadline(Duration::ZERO);
        assert!(render_diagram(&light, expired).is_err());
    }

    #[gpui::test]
    fn mermaid_cache_deduplicates_measure_display_failures_and_bounds_memory(
        cx: &mut gpui::TestAppContext,
    ) {
        let hub = cx.new(|_| Hub::default());
        let k = key("flowchart LR\nA-->B");
        for _ in 0..8 {
            hub.update(cx, |hub, cx| {
                hub.request(k.clone(), cx);
            });
        }
        cx.run_until_parked();
        hub.update(cx, |hub, _| {
            assert_eq!(hub.started, 1);
            assert!(matches!(hub.lookup(&k), Some(Ok(_))));
        });
        let invalid = key("flowchart LR\nA[unclosed");
        for _ in 0..8 {
            hub.update(cx, |hub, cx| {
                hub.request(invalid.clone(), cx);
            });
        }
        cx.run_until_parked();
        hub.update(cx, |hub, cx| {
            assert_eq!(hub.started, 2);
            assert!(matches!(hub.request(invalid.clone(), cx), Some(Err(_))));
            // Simulate completed large entries without allocating large bitmaps.
            let diagram = render("flowchart LR\nA-->B").unwrap();
            for i in 0..150 {
                let mut diagram = diagram.clone();
                diagram.cost = 32 * 1024 * 1024;
                let cost = diagram.cost;
                hub.entries.insert(
                    key(&format!("diagram {i}")),
                    Entry {
                        result: Some(Ok(diagram)),
                        touched: i,
                        cost,
                    },
                );
                hub.bytes += cost;
                hub.evict(cx);
                assert!(hub.bytes <= MAX_CACHE);
                assert!(hub.entries.len() <= MAX_ENTRIES);
            }
        });
    }
}

#[cfg(test)]
mod queue_tests {
    use super::*;

    #[gpui::test]
    fn mermaid_queue_and_workers_are_finite_and_completed_failures_stay_cached(
        cx: &mut gpui::TestAppContext,
    ) {
        let hub = cx.new(|_| Hub::default());
        hub.update(cx, |hub, cx| {
            for i in 0..MAX_JOBS + 4 {
                let key = Key {
                    source: format!("unknownDiagram {i}").into(),
                    dark: false,
                    font: FONT,
                    scale: 1,
                    revision: REVISION,
                };
                let result = hub.request(key, cx);
                if i >= MAX_JOBS {
                    assert!(matches!(result, Some(Err(_))));
                }
            }
            assert_eq!(hub.running, CONCURRENCY);
            assert_eq!(hub.started, CONCURRENCY);
            assert_eq!(hub.queue.len(), MAX_JOBS - CONCURRENCY);
            assert_eq!(hub.entries.len(), MAX_JOBS);
        });
        cx.run_until_parked();
        hub.update(cx, |hub, _| {
            assert_eq!(hub.running, 0);
            assert_eq!(hub.started, MAX_JOBS);
            assert!(hub.queue.is_empty());
            assert!(hub
                .entries
                .values()
                .all(|entry| matches!(entry.result, Some(Err(_)))));
        });
    }
}
