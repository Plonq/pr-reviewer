//! PR images never enter GPUI's default SVG decoder (including raster-named
//! resources). SVG file previews keep their parsed tree across resizes.
use std::{
    hash::{Hash, Hasher},
    io::{Cursor, Read as _},
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc, OnceLock,
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
    anyhow::ensure!(bytes.len() as u64 <= MAX_BYTES, "SVG exceeds 25 MiB");
    // Deliberately accept plain XML only, avoiding unbounded SVGZ expansion.
    let svg = std::str::from_utf8(bytes)?;
    let xml = roxmltree::Document::parse(svg)?;
    anyhow::ensure!(
        !xml.descendants()
            .any(|node| node.is_element() && matches!(node.tag_name().name(), "image" | "feImage")),
        "SVG image references are unsupported"
    );
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
    let options = usvg::Options {
        resources_dir: None,
        image_href_resolver: usvg::ImageHrefResolver {
            resolve_data: Box::new(|_, _, _| None),
            resolve_string: Box::new(|_, _| None),
        },
        font_family: "Arial".into(),
        fontdb: fonts.clone(),
        ..Default::default()
    };
    Ok(usvg::Tree::from_str(svg, &options)?)
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
    use gpui::{px, size};

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
