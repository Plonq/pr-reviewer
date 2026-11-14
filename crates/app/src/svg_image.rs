//! PR images never enter GPUI's default SVG decoder (including raster-named
//! resources). SVG file previews keep their parsed tree across resizes.
use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    io::{Cursor, Read as _},
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc, Mutex, OnceLock,
    },
};

use futures::{AsyncReadExt as _, Future};
use gpui::{
    canvas, div, img, prelude::*, App, Asset, AvailableSpace, ImageCacheError, ImageSource,
    ObjectFit, RenderImage, Resource, SharedUri, Size, StyledImage,
};
use image::{AnimationDecoder as _, ImageDecoder as _};
use resvg::{tiny_skia, usvg};

const MAX_BYTES: u64 = 25 * 1024 * 1024;
const MAX_RASTER: u32 = 8192;
const MAX_SVG_DEPTH: usize = 4;
const MAX_EMBEDDED_BYTES: usize = 1024 * 1024;
const MAX_EMBEDDED_IMAGES: usize = 32;

fn load_bytes(
    source: Resource,
    cx: &App,
) -> impl Future<Output = anyhow::Result<Vec<u8>>> + Send + 'static {
    let client = cx.http_client();
    let assets = cx.asset_source().clone();
    async move {
        let bytes = match source {
            Resource::Path(path) => {
                let mut bytes = Vec::new();
                std::fs::File::open(path)?
                    .take(MAX_BYTES + 1)
                    .read_to_end(&mut bytes)?;
                bytes
            }
            Resource::Uri(uri) => {
                anyhow::ensure!(remote_url(&uri), "Image URL must use HTTP(S)");
                let mut response = client.get(uri.as_ref(), ().into(), true).await?;
                anyhow::ensure!(
                    response.status().is_success(),
                    "Could not load image: {}",
                    response.status()
                );
                let mut bytes = Vec::new();
                response
                    .body_mut()
                    .take(MAX_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .await?;
                bytes
            }
            Resource::Embedded(path) => assets
                .load(&path)?
                .ok_or_else(|| anyhow::anyhow!("Image asset not found"))?
                .into_owned(),
        };
        anyhow::ensure!(bytes.len() as u64 <= MAX_BYTES, "Image exceeds 25 MiB");
        Ok(bytes)
    }
}

fn remote_url(uri: &SharedUri) -> bool {
    http_client::Url::parse(uri.as_ref()).is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
}

/// Document-provided file paths and data URLs are unsupported.
pub(crate) fn remote_source(uri: &SharedUri) -> ImageSource {
    if !remote_url(uri) {
        return ImageSource::from(|_: &mut gpui::Window, _: &mut App| {
            Some(Err(anyhow::anyhow!("Image URL must use HTTP(S)").into()))
        });
    }
    image_source(Resource::Uri(uri.clone()))
}

/// Authoritative Kit hook for stock Markdown/HTML, including measurement and
/// nested fallbacks. Kit has no error element for these images: supply a small
/// crossed image marker so unsupported content is visible instead of blank.
pub(crate) fn markdown_source(uri: &SharedUri) -> ImageSource {
    let resource = Resource::Uri(uri.clone());
    ImageSource::from(move |window: &mut gpui::Window, cx: &mut App| {
        window
            .use_asset::<ImageLoader>(&resource, cx)
            .map(|result| {
                result.or_else(|_| {
                    static ERROR: OnceLock<Arc<RenderImage>> = OnceLock::new();
                    Ok(ERROR
                        .get_or_init(|| {
                            let pixels = image::RgbaImage::from_fn(20, 20, |x, y| {
                                if x == 0 || y == 0 || x == 19 || y == 19 || x == y || x + y == 19 {
                                    image::Rgba([0, 0, 220, 255]) // GPUI BGRA
                                } else {
                                    image::Rgba([0, 0, 0, 0])
                                }
                            });
                            Arc::new(RenderImage::new(vec![image::Frame::new(pixels)]))
                        })
                        .clone())
                })
            })
    })
}

pub(crate) fn image_source(resource: Resource) -> ImageSource {
    ImageSource::from(move |window: &mut gpui::Window, cx: &mut App| {
        window.use_asset::<ImageLoader>(&resource, cx)
    })
}

/// Shared by Markdown drawing/measurement and the viewer, including Retry.
pub(crate) struct ImageLoader;

impl Asset for ImageLoader {
    type Source = Resource;
    type Output = Result<Arc<RenderImage>, ImageCacheError>;

    fn load(source: Resource, cx: &mut App) -> impl Future<Output = Self::Output> + Send + 'static {
        let bytes = load_bytes(source, cx);
        async move { decode_image(&bytes.await?).map_err(Into::into) }
    }
}

fn decode_image(bytes: &[u8]) -> anyhow::Result<Arc<RenderImage>> {
    // Select solely by these bytes, never the name or Content-Type. In
    // particular a failed raster decoder must never call GPUI's SVG fallback.
    let Ok(format) = image::guess_format(bytes) else {
        let tree = parse_svg(bytes)?;
        let width = tree
            .size()
            .width()
            .min(MAX_RASTER as f32)
            .min(MAX_RASTER as f32 * tree.size().width() / tree.size().height())
            .ceil()
            .max(1.) as u32;
        return render_svg(&tree, width);
    };
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_RASTER);
    limits.max_image_height = Some(MAX_RASTER);
    let mut frames = match format {
        image::ImageFormat::Gif => {
            let mut decoder = image::codecs::gif::GifDecoder::new(Cursor::new(bytes))?;
            decoder.set_limits(limits)?;
            animation_frames(decoder.into_frames())?
        }
        image::ImageFormat::WebP => {
            let mut decoder = image::codecs::webp::WebPDecoder::new(Cursor::new(bytes))?;
            decoder.set_limits(limits)?;
            if decoder.has_animation() {
                let _ = decoder.set_background_color(image::Rgba([0, 0, 0, 0]));
                animation_frames(decoder.into_frames())?
            } else {
                vec![static_frame(decoder)?]
            }
        }
        _ => {
            let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
            reader.limits(limits);
            vec![static_frame(reader.into_decoder()?)?]
        }
    };
    for frame in &mut frames {
        for pixel in frame.buffer_mut().chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
    }
    Ok(Arc::new(RenderImage::new(frames)))
}

fn static_frame(mut decoder: impl image::ImageDecoder) -> anyhow::Result<image::Frame> {
    let orientation = decoder.orientation()?;
    let mut image = image::DynamicImage::from_decoder(decoder)?;
    image.apply_orientation(orientation);
    Ok(image::Frame::new(image.into_rgba8()))
}

fn animation_frames(frames: image::Frames<'_>) -> anyhow::Result<Vec<image::Frame>> {
    let mut result = Vec::new();
    let mut bytes = 0;
    for frame in frames {
        let frame = frame?;
        bytes += frame.buffer().len();
        anyhow::ensure!(
            bytes <= 256 * 1024 * 1024,
            "Animation exceeds raster budget"
        );
        result.push(frame);
    }
    anyhow::ensure!(!result.is_empty(), "Image has no frames");
    Ok(result)
}

fn parse_svg(bytes: &[u8]) -> anyhow::Result<usvg::Tree> {
    static FONTS: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    let fonts = FONTS.get_or_init(|| {
        let mut db = usvg::fontdb::Database::new();
        // The same closed OS-font set used for Mermaid, never a path supplied
        // by the SVG. Default font selection only queries this database.
        for name in [
            "Arial.ttf",
            "Arial Bold.ttf",
            "Arial Italic.ttf",
            "Arial Bold Italic.ttf",
            "Arial Unicode.ttf",
        ] {
            let _ = db.load_font_file(
                std::path::Path::new("/System/Library/Fonts/Supplemental").join(name),
            );
        }
        db.set_sans_serif_family("Arial");
        db.set_serif_family("Arial");
        Arc::new(db)
    });
    parse_svg_document(bytes, 0, fonts, &mut SvgBudget::default())
}

#[derive(Default)]
struct SvgWork {
    bytes: usize,
    images: usize,
}

impl SvgWork {
    fn charge(&mut self, bytes: usize) -> anyhow::Result<()> {
        self.bytes += bytes;
        self.images += 1;
        anyhow::ensure!(
            self.bytes <= MAX_EMBEDDED_BYTES,
            "SVG embedded bytes exceed 1 MiB"
        );
        anyhow::ensure!(
            self.images <= MAX_EMBEDDED_IMAGES,
            "SVG exceeds 32 embedded images"
        );
        Ok(())
    }
}

#[derive(Default)]
struct SvgBudget {
    embedded: SvgWork,
    // Conversion can resolve one image repeatedly via <use>. Bound that work
    // separately from validation, including across nested documents.
    resolved: SvgWork,
}

fn parse_svg_document(
    bytes: &[u8],
    depth: usize,
    fonts: &Arc<usvg::fontdb::Database>,
    budget: &mut SvgBudget,
) -> anyhow::Result<usvg::Tree> {
    anyhow::ensure!(bytes.len() as u64 <= MAX_BYTES, "SVG exceeds 25 MiB");
    anyhow::ensure!(depth <= MAX_SVG_DEPTH, "SVG nesting exceeds 4 levels");
    if depth > 0 {
        budget.embedded.charge(bytes.len())?;
    }
    // Plain XML only at every level: no SVGZ expansion or document entities.
    let xml = roxmltree::Document::parse(std::str::from_utf8(bytes)?)?;
    let mut embedded = HashMap::new();
    // Validate even unused/hidden images: usvg otherwise silently drops an
    // unsupported resource and reports a successful (but incomplete) badge.
    for node in xml.descendants().filter(|node| node.is_element()) {
        anyhow::ensure!(
            node.tag_name().name() != "feImage",
            "SVG feImage is unsupported"
        );
        if node.tag_name().name() != "image" {
            continue;
        }
        let mut has_href = false;
        for attr in node.attributes().filter(|attr| attr.name() == "href") {
            has_href = true;
            anyhow::ensure!(
                matches!(
                    attr.namespace(),
                    None | Some("http://www.w3.org/1999/xlink")
                ),
                "Unsupported SVG image href namespace"
            );
            let url = data_url::DataUrl::process(attr.value())
                .map_err(|_| anyhow::anyhow!("SVG images require a data:image/svg+xml URL"))?;
            anyhow::ensure!(
                url.mime_type().type_ == "image" && url.mime_type().subtype == "svg+xml",
                "Only embedded SVG images are supported"
            );
            let mut data = Vec::new();
            let fragment = url
                .decode(|chunk| {
                    anyhow::ensure!(
                        budget.embedded.bytes + data.len() + chunk.len() <= MAX_EMBEDDED_BYTES,
                        "SVG embedded bytes exceed 1 MiB"
                    );
                    data.extend_from_slice(chunk);
                    Ok::<_, anyhow::Error>(())
                })
                .map_err(|_| anyhow::anyhow!("Invalid or oversized embedded SVG data URL"))?;
            anyhow::ensure!(fragment.is_none(), "Embedded SVG fragments are unsupported");
            let tree = parse_svg_document(&data, depth + 1, fonts, budget)?;
            embedded.insert(data, tree);
        }
        anyhow::ensure!(has_href, "SVG image has no href");
    }
    let resolved = Mutex::new(&mut budget.resolved);
    let failed = AtomicBool::new(false);
    let options = usvg::Options {
        resources_dir: None,
        image_href_resolver: usvg::ImageHrefResolver {
            // Do not use usvg's default_data_resolver/from_data_nested: those
            // rebuild options. Only return trees parsed above with our policy.
            resolve_data: Box::new(|mime, data, _| {
                if mime == "image/svg+xml" {
                    if let Some(tree) = embedded.get(data.as_slice()) {
                        if resolved.lock().unwrap().charge(data.len()).is_ok() {
                            return Some(usvg::ImageKind::SVG(tree.clone()));
                        }
                    }
                }
                failed.store(true, Ordering::Relaxed);
                None
            }),
            resolve_string: Box::new(|_, _| {
                failed.store(true, Ordering::Relaxed);
                None
            }),
        },
        font_family: "Arial".into(),
        fontdb: fonts.clone(),
        ..Default::default()
    };
    let tree = usvg::Tree::from_xmltree(&xml, &options)?;
    anyhow::ensure!(
        !failed.load(Ordering::Relaxed),
        "SVG image resolution failed or exceeded its budget"
    );
    Ok(tree)
}

fn render_svg(tree: &usvg::Tree, width: u32) -> anyhow::Result<Arc<RenderImage>> {
    let width = width.clamp(1, MAX_RASTER);
    let height = (width as f32 * tree.size().height() / tree.size().width())
        .ceil()
        .clamp(1., MAX_RASTER as f32) as u32;
    let mut pixmap =
        tiny_skia::Pixmap::new(width, height).ok_or_else(|| anyhow::anyhow!("Invalid SVG size"))?;
    resvg::render(
        tree,
        tiny_skia::Transform::from_scale(
            width as f32 / tree.size().width(),
            height as f32 / tree.size().height(),
        ),
        &mut pixmap.as_mut(),
    );
    let mut pixels = pixmap.take();
    for pixel in pixels.chunks_exact_mut(4) {
        gpui::swap_rgba_pa_to_bgra(pixel);
    }
    let buffer = image::RgbaImage::from_raw(width, height, pixels)
        .ok_or_else(|| anyhow::anyhow!("Invalid SVG pixels"))?;
    Ok(Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])))
}

type LoadResult<T> = Result<Arc<T>, Arc<anyhow::Error>>;

struct Document {
    parsed: usvg::Tree,
    aspect: f32,
    raster_width: AtomicU32,
}

struct DocumentLoader;

impl Asset for DocumentLoader {
    type Source = Resource;
    type Output = LoadResult<Document>;

    fn load(source: Resource, cx: &mut App) -> impl Future<Output = Self::Output> + Send + 'static {
        let bytes = load_bytes(source, cx);
        async move {
            let load = async {
                let parsed = parse_svg(&bytes.await?)?;
                let aspect = parsed.size().width() / parsed.size().height();
                Ok(Document {
                    parsed,
                    aspect,
                    raster_width: AtomicU32::new(0),
                })
            };
            load.await.map(Arc::new).map_err(Arc::new)
        }
    }
}

#[derive(Clone)]
struct RasterSource {
    resource: Resource,
    document: Arc<Document>,
    width: u32,
}

impl Hash for RasterSource {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.resource.hash(state);
        self.width.hash(state);
    }
}

struct RasterLoader;

impl Asset for RasterLoader {
    type Source = RasterSource;
    type Output = LoadResult<RenderImage>;

    fn load(
        source: RasterSource,
        _: &mut App,
    ) -> impl Future<Output = Self::Output> + Send + 'static {
        async move { render_svg(&source.document.parsed, source.width).map_err(Arc::new) }
    }
}

/// Contain-fit in logical pixels, then rasterize at the monitor's density.
/// Match GPUI's 8192px texture cap while preserving the document's aspect ratio.
fn raster_width(viewport: Size<gpui::Pixels>, aspect: f32, density: f32) -> u32 {
    let width = f32::from(viewport.width).min(f32::from(viewport.height) * aspect);
    (width * density)
        .ceil()
        .min(8192.)
        .min(8192. * aspect)
        .max(1.) as u32
}

pub(crate) fn svg_image(resource: Resource, label: &'static str) -> gpui::AnyElement {
    canvas(
        move |bounds, window, cx| {
            let loaded = window
                .use_asset::<DocumentLoader>(&resource, cx)
                .and_then(|result| {
                    let document = match result {
                        Ok(document) => document,
                        Err(err) => return Some(Err(err)),
                    };
                    let width = raster_width(bounds.size, document.aspect, window.scale_factor());
                    let previous = document.raster_width.swap(width, Ordering::Relaxed);
                    let source = RasterSource {
                        resource,
                        document,
                        width,
                    };
                    if previous != 0 && previous != width {
                        // GPUI's asset cache otherwise retains every resize forever.
                        cx.remove_asset::<RasterLoader>(&RasterSource {
                            width: previous,
                            ..source.clone()
                        });
                    }
                    window.use_asset::<RasterLoader>(&source, cx)
                });
            let mut element = match loaded {
                Some(Ok(image)) => img(image)
                    .size_full()
                    .object_fit(ObjectFit::Contain)
                    .into_any_element(),
                Some(Err(_)) => div()
                    .debug_selector(move || format!("file-media-{label}-error"))
                    .child("Could not load image. Use Open file above.")
                    .into_any_element(),
                None => div()
                    .debug_selector(move || format!("file-media-{label}-loading"))
                    .child("Loading image…")
                    .into_any_element(),
            };
            element.layout_as_root(bounds.size.map(AvailableSpace::Definite), window, cx);
            element.prepaint_at(bounds.origin, window, cx);
            element
        },
        |_, mut element, window, cx| element.paint(window, cx),
    )
    .size_full()
    .into_any_element()
}

#[cfg(test)]
pub(crate) fn cached_raster(
    resource: &Resource,
    window: &mut gpui::Window,
    cx: &mut App,
) -> Option<Arc<RenderImage>> {
    let document = window.get_asset::<DocumentLoader>(resource, cx)?.ok()?;
    let width = document.raster_width.load(Ordering::Relaxed);
    window
        .get_asset::<RasterLoader>(
            &RasterSource {
                resource: resource.clone(),
                document,
                width,
            },
            cx,
        )?
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use gpui::{px, size};

    fn data_svg(bytes: &[u8]) -> String {
        format!(
            "data:image/svg+xml;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        )
    }

    fn with_image(href: &str) -> String {
        format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="20"><image href="{href}" width="20" height="20"/></svg>"#
        )
    }

    #[test]
    fn shields_style_badge_renders_embedded_logo_text_and_background() {
        let image = decode_image(include_bytes!("testdata/embedded-svg-badge.svg")).unwrap();
        assert_eq!(
            image.size(0),
            size(gpui::DevicePixels(95), gpui::DevicePixels(20))
        );
        let pixels = image.as_bytes(0).unwrap();
        let pixel = |x: usize, y: usize| &pixels[(y * 95 + x) * 4..(y * 95 + x + 1) * 4];
        // These colors occur only in the embedded SVG, not the badge itself.
        assert_eq!(pixel(6, 4), [255, 200, 0, 255]);
        assert_eq!(pixel(11, 9), [255, 255, 255, 255]);
        assert_eq!(pixel(40, 18)[3], 255);
        assert!(
            pixel(90, 18)[2] > pixel(40, 18)[2],
            "both background panels render"
        );
        assert!(
            (22..90).any(|x| (5..15).any(|y| { pixel(x, y).iter().all(|channel| *channel > 240) })),
            "badge text must produce white pixels outside the logo"
        );
    }

    #[test]
    fn nested_svg_keeps_the_host_font_policy_and_accepts_percent_encoded_data() {
        let leaf = br#"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="20"><text x="1" y="15" font-size="14">Hi</text></svg>"#;
        let expected = decode_image(leaf).unwrap();
        assert!(expected
            .as_bytes(0)
            .unwrap()
            .chunks_exact(4)
            .any(|pixel| pixel[3] > 0));
        let mut nested = leaf.to_vec();
        for _ in 0..MAX_SVG_DEPTH {
            nested = with_image(&data_svg(&nested)).into_bytes();
            let actual = decode_image(&nested).unwrap();
            assert_eq!(
                actual.as_bytes(0),
                expected.as_bytes(0),
                "nested text must keep Arial, including its default family"
            );
        }
        let percent = br#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="2" height="2"><image width="2" height="2" xlink:href="data:image/svg+xml,%3Csvg%20xmlns='http://www.w3.org/2000/svg'%20width='2'%20height='2'%3E%3Cpath%20fill='red'%20d='M0%200h2v2H0z'/%3E%3C/svg%3E"/></svg>"#;
        assert_eq!(
            &decode_image(percent).unwrap().as_bytes(0).unwrap()[..4],
            [0, 0, 255, 255]
        );
    }

    #[test]
    fn svg_rejects_unsafe_resources_at_every_embedding_level() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("private.svg");
        std::fs::write(
            &file,
            br#"<svg xmlns="http://www.w3.org/2000/svg" width="2" height="2"/>"#,
        )
        .unwrap();
        let mut invalid = Vec::new();
        for href in [
            file.to_str().unwrap().to_owned(),
            http_client::Url::from_file_path(&file).unwrap().to_string(),
            "relative.svg".into(),
            "https://example.test/private.svg".into(),
            "data:image/png;base64,iVBORw0KGgo=".into(),
            "data:text/plain,%3Csvg/%3E".into(),
            "data:image/svg+xml;base64,!!!!".into(),
            data_svg(b"\x1f\x8b\x08\x00"),
            data_svg(b"not SVG"),
        ] {
            invalid.push(with_image(&href).into_bytes());
        }
        invalid.push(br##"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="20"><defs><filter id="f"><feImage href="#local"/></filter></defs></svg>"##.to_vec());
        invalid.push(
            br#"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="20"><image/></svg>"#
                .to_vec(),
        );
        invalid.push(
            b"<!DOCTYPE svg [<!ENTITY x SYSTEM 'file:///tmp/private'>]><svg>&x;</svg>".to_vec(),
        );
        invalid.push(b"\x1f\x8b\x08\x00".to_vec());
        for mut svg in invalid {
            for depth in 0..=MAX_SVG_DEPTH {
                assert!(
                    decode_image(&svg).is_err(),
                    "unsafe SVG accepted at depth {depth}"
                );
                svg = with_image(&data_svg(&svg)).into_bytes();
            }
        }
    }

    #[test]
    fn svg_bounds_depth_aggregate_bytes_references_and_repeated_resolution() {
        let leaf = br#"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="20"><path d="M0 0h20v20H0z"/></svg>"#;
        let mut nested = leaf.to_vec();
        for _ in 0..MAX_SVG_DEPTH {
            nested = with_image(&data_svg(&nested)).into_bytes();
        }
        assert!(parse_svg(&nested).is_ok());
        assert!(parse_svg(with_image(&data_svg(&nested)).as_bytes()).is_err());

        let padded = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="20"><!--{}--></svg>"#,
            "x".repeat(MAX_EMBEDDED_BYTES / 2)
        );
        let image = format!(
            r#"<image href="{}" width="20" height="20"/>"#,
            data_svg(padded.as_bytes())
        );
        assert!(parse_svg(with_image(&data_svg(padded.as_bytes())).as_bytes()).is_ok());
        let aggregate = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="20">{image}{image}</svg>"#
        );
        assert!(
            parse_svg(aggregate.as_bytes()).is_err(),
            "aggregate decoded bytes must be bounded"
        );
        let oversized = format!(r#"<svg><!--{}--></svg>"#, "x".repeat(MAX_EMBEDDED_BYTES));
        assert!(parse_svg(with_image(&data_svg(oversized.as_bytes())).as_bytes()).is_err());

        let image = format!(
            r#"<image href="{}" width="20" height="20"/>"#,
            data_svg(leaf)
        );
        for count in [MAX_EMBEDDED_IMAGES, MAX_EMBEDDED_IMAGES + 1] {
            let svg = format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="20">{}</svg>"#,
                image.repeat(count)
            );
            assert_eq!(
                parse_svg(svg.as_bytes()).is_ok(),
                count == MAX_EMBEDDED_IMAGES
            );
        }
        let expanded = format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="20"><defs><g id="logo">{image}</g></defs>{}</svg>"##,
            r##"<use href="#logo"/>"##.repeat(MAX_EMBEDDED_IMAGES + 1)
        );
        assert!(
            parse_svg(expanded.as_bytes()).is_err(),
            "repeated <use> resolution must also be bounded"
        );
    }

    #[test]
    fn raster_resolution_follows_contain_fit_and_display_density() {
        let viewport = size(px(600.), px(400.));
        assert_eq!(raster_width(viewport, 2., 1.), 600);
        assert_eq!(raster_width(viewport, 2., 2.), 1200);
        assert_eq!(raster_width(viewport, 0.5, 2.), 400);
        assert_eq!(raster_width(size(px(20_000.), px(20_000.)), 0.5, 2.), 4096);
    }

    #[test]
    fn svg_rejects_local_remote_and_nested_image_references() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = dir.path().join("harmless.png");
        image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 255]))
            .save(&fixture)
            .unwrap();
        for href in [
            fixture.to_str().unwrap().to_owned(),
            http_client::Url::from_file_path(&fixture).unwrap().to_string(),
            "https://example.invalid/image.png".into(),
            "relative.png".into(),
            "data:image/png;base64,iVBORw0KGgo=".into(),
            // The nested SVG would otherwise get its own default resolver.
            "data:image/svg+xml,%3Csvg%20xmlns='http://www.w3.org/2000/svg'%3E%3Cimage%20href='/tmp/harmless.png'/%3E%3C/svg%3E".into(),
        ] {
            for tag in ["image", "feImage"] {
                for attr in ["href", "xlink:href"] {
                    let svg = format!(r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="2" height="2"><{tag} {attr}="{href}" width="2" height="2"/></svg>"#);
                    assert!(parse_svg(svg.as_bytes()).is_err(), "{svg}");
                    assert!(decode_image(svg.as_bytes()).is_err(), "{svg}");
                }
            }
        }
        assert!(parse_svg(
            b"<!DOCTYPE svg [<!ENTITY x SYSTEM 'file:///tmp/harmless'>]><svg>&x;</svg>"
        )
        .is_err());
    }

    #[test]
    fn useful_svg_paths_text_and_fragment_references_still_render() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="20">
            <defs><linearGradient id="paint"><stop stop-color="red"/><stop offset="1" stop-color="blue"/></linearGradient>
            <path id="shape" d="M0 0h20v20H0z"/></defs>
            <use href="#shape" fill="url(#paint)"/><text x="20" y="15" font-size="12">Hi</text></svg>"##;
        let tree = parse_svg(svg).unwrap();
        let image = render_svg(&tree, 80).unwrap();
        assert_eq!(
            image.size(0),
            size(gpui::DevicePixels(80), gpui::DevicePixels(40))
        );
        assert!(image
            .as_bytes(0)
            .unwrap()
            .chunks_exact(4)
            .any(|pixel| pixel[3] != 0));
        assert!(tree
            .root()
            .children()
            .iter()
            .any(|node| matches!(node, usvg::Node::Text(_))));

        let translucent = decode_image(br#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><path d="M0 0h1v1H0z" fill="red" opacity="0.5"/></svg>"#).unwrap();
        let pixel = translucent.as_bytes(0).unwrap();
        assert_eq!((pixel[0], pixel[1], pixel[3]), (0, 0, 128));
        assert!(
            pixel[2] >= 254,
            "premultiplied red must be restored to BGRA"
        );
    }

    #[test]
    fn raster_decoding_keeps_pixels_and_animation_without_svg_fallback() {
        let mut png = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            3,
            2,
            image::Rgba([255, 0, 0, 255]),
        ))
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
        let decoded = decode_image(png.get_ref()).unwrap();
        assert_eq!(
            decoded.size(0),
            size(gpui::DevicePixels(3), gpui::DevicePixels(2))
        );
        assert_eq!(&decoded.as_bytes(0).unwrap()[..4], &[0, 0, 255, 255]);
        let mut gif = Vec::new();
        image::codecs::gif::GifEncoder::new(&mut gif)
            .encode_frames([
                image::Frame::new(image::RgbaImage::new(3, 2)),
                image::Frame::new(image::RgbaImage::new(3, 2)),
            ])
            .unwrap();
        assert_eq!(decode_image(&gif).unwrap().frame_count(), 2);
        assert!(decode_image(b"\x89PNG\r\n\x1a\n<svg/>").is_err());
        assert!(decode_image(b"not an image").is_err());
    }

    #[test]
    fn large_svg_intrinsic_size_does_not_allocate_a_probe_and_rasters_stay_bounded() {
        let tree = parse_svg(br#"<svg xmlns="http://www.w3.org/2000/svg" width="100000000" height="1"><path d="M0 0h100000000v1H0z"/></svg>"#).unwrap();
        assert_eq!(render_svg(&tree, 16).unwrap().size(0).width.0, 16);
        let image =
            decode_image(br#"<svg xmlns="http://www.w3.org/2000/svg" width="20000" height="2"/>"#)
                .unwrap();
        assert_eq!(image.size(0).width.0, 8192);
        assert_eq!(image.size(0).height.0, 1);
    }
}
