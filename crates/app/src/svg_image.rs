//! SVG images rasterized for their actual device-pixel footprint, not their
//! intrinsic size. Keep the parsed document across resizes, but only one raster.
use std::{
    hash::{Hash, Hasher},
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
};

use futures::{AsyncReadExt as _, Future};
use gpui::{
    canvas, div, img, prelude::*, size, App, Asset, AvailableSpace, DevicePixels, ObjectFit,
    ParsedSvg, RenderImage, Resource, Size, StyledImage, SvgSize,
};

type LoadResult<T> = Result<Arc<T>, Arc<anyhow::Error>>;

struct Document {
    parsed: ParsedSvg,
    aspect: f32,
    raster_width: AtomicU32,
}

struct DocumentLoader;

impl Asset for DocumentLoader {
    type Source = Resource;
    type Output = LoadResult<Document>;

    fn load(source: Resource, cx: &mut App) -> impl Future<Output = Self::Output> + Send + 'static {
        let client = cx.http_client();
        let assets = cx.asset_source().clone();
        let renderer = cx.svg_renderer();
        async move {
            let load = async {
                const MAX_BYTES: u64 = 25 * 1024 * 1024;
                let bytes = match source {
                    Resource::Path(path) => std::fs::read(path)?,
                    Resource::Uri(uri) => {
                        // Use the same authenticated/cache-aware bridge as img().
                        let mut response = client.get(uri.as_ref(), ().into(), true).await?;
                        anyhow::ensure!(
                            response.status().is_success(),
                            "Could not load SVG: {}",
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
                        .ok_or_else(|| anyhow::anyhow!("SVG asset not found"))?
                        .into_owned(),
                };
                anyhow::ensure!(bytes.len() as u64 <= MAX_BYTES, "SVG exceeds 25 MiB");
                let parsed = renderer.parse_svg(&bytes)?;
                // ParsedSvg doesn't expose its intrinsic dimensions. A one-time
                // probe gives the aspect ratio; its pixels are never displayed.
                let probe = renderer.render_parsed(&parsed, 1.)?.size(0);
                Ok(Document {
                    parsed,
                    aspect: probe.width.0 as f32 / probe.height.0 as f32,
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
        cx: &mut App,
    ) -> impl Future<Output = Self::Output> + Send + 'static {
        let renderer = cx.svg_renderer();
        async move {
            renderer
                .render_parsed(
                    &source.document.parsed,
                    SvgSize::Size(size(DevicePixels(source.width as i32), DevicePixels(1))),
                )
                .map_err(|err| Arc::new(err.into()))
        }
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
    use gpui::px;

    #[test]
    fn raster_resolution_follows_contain_fit_and_display_density() {
        let viewport = size(px(600.), px(400.));
        assert_eq!(raster_width(viewport, 2., 1.), 600);
        assert_eq!(raster_width(viewport, 2., 2.), 1200);
        assert_eq!(raster_width(viewport, 0.5, 2.), 400);
        assert_eq!(raster_width(size(px(20_000.), px(20_000.)), 0.5, 2.), 4096);
    }
}
