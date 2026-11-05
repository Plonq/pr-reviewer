//! Window-filling media inspection, deliberately separate from review input.
//! Images retain the existing GPUI loader; video moves its live session here.
use crate::{ReviewApp, APP_HEADER_HEIGHT};
use gpui::component::{button::Button, Sizable, WindowExt};
use gpui::{
    div, img, point, prelude::*, px, size, Context, FocusHandle, ImgResourceLoader, KeyDownEvent,
    MouseButton, MouseMoveEvent, Pixels, Point, Resource, SharedString, SharedUri, Size,
    StyledImage, Window,
};

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ImageTarget {
    pub url: SharedUri,
    pub alt: SharedString,
    pub link: Option<SharedString>,
}

/// Zoom is relative to fit, not the source's pixel size. Pan is in window pixels.
#[derive(Debug)]
pub(crate) struct Transform {
    pub zoom: f32,
    pub pan: Point<f32>,
}

impl Default for Transform {
    fn default() -> Self {
        Self {
            zoom: 1.,
            pan: point(0., 0.),
        }
    }
}

impl Transform {
    pub fn fit(&mut self) {
        self.zoom = 1.;
        self.pan = point(0., 0.);
    }

    fn geometry(&mut self, image: Size<f32>, viewport: Size<f32>) -> (Size<f32>, Point<f32>, f32) {
        let fit = (viewport.width / image.width)
            .min(viewport.height / image.height)
            .min(1.);
        let scale = fit * self.zoom.clamp(0.1, 32.);
        let drawn = size(image.width * scale, image.height * scale);
        let limit = point(
            ((drawn.width - viewport.width) / 2.).max(0.),
            ((drawn.height - viewport.height) / 2.).max(0.),
        );
        self.pan.x = self.pan.x.clamp(-limit.x, limit.x);
        self.pan.y = self.pan.y.clamp(-limit.y, limit.y);
        (
            drawn,
            point(
                (viewport.width - drawn.width) / 2. + self.pan.x,
                (viewport.height - drawn.height) / 2. + self.pan.y,
            ),
            scale,
        )
    }

    fn zoom_by(&mut self, factor: f32) {
        self.zoom = (self.zoom * factor).clamp(0.1, 32.);
    }
}

pub(crate) struct ImageViewer {
    pub target: ImageTarget,
    pub item_id: u64,
    pub video: Option<crate::video::Origin>,
    focus: FocusHandle,
    previous_focus: Option<FocusHandle>,
    pub transform: Transform,
    drag: Option<(Point<Pixels>, Point<f32>)>,
}

fn control(id: &'static str, button: Button) -> gpui::AnyElement {
    div()
        .debug_selector(move || id.into())
        .child(button)
        .into_any_element()
}

impl ReviewApp {
    pub(crate) fn open_image(
        &mut self,
        target: ImageTarget,
        item_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_media(target, item_id, None, window, cx);
    }

    fn open_media(
        &mut self,
        target: ImageTarget,
        item_id: u64,
        video: Option<crate::video::Origin>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A composer stays mounted underneath, retaining its draft and focus
        // for when inspection ends. Don't stack over another modal, or let a
        // delayed click from an inactive review open an orphaned viewer.
        if self.image_viewer.is_some()
            || window.has_active_dialog(cx)
            || self.show_queue
            || self.active_item().is_none_or(|item| item.id != item_id)
        {
            return;
        }
        let focus = cx.focus_handle();
        let previous_focus = window.focused(cx);
        window.focus(&focus, cx);
        self.video_hub.update(cx, |hub, cx| {
            if video.is_some() {
                hub.expanded = true;
                if let Some(session) = &hub.session {
                    session.update(cx, |s, cx| s.expanded_controls(true, cx));
                }
            } else {
                hub.pause(cx);
            }
            cx.notify();
        });
        self.image_viewer = Some(ImageViewer {
            target,
            item_id,
            video,
            focus,
            previous_focus,
            transform: Transform::default(),
            drag: None,
        });
        cx.notify();
    }

    pub(crate) fn open_video(
        &mut self,
        target: crate::video::VideoTarget,
        origin: crate::video::Origin,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Keep the existing modal's focus/input boundary and presentation. The
        // session entity moves between inline and overlay, never recreated.
        if self.image_viewer.is_some()
            || self
                .active_item()
                .is_none_or(|item| item.id != origin.item || item.pane != origin.pane)
        {
            return;
        }
        if !self
            .video_hub
            .read(cx)
            .session
            .as_ref()
            .is_some_and(|s| s.read(cx).origin == origin)
        {
            return;
        }
        let item_id = origin.item;
        self.open_media(
            ImageTarget {
                url: target.url.to_string().into(),
                alt: target.label,
                link: None,
            },
            item_id,
            Some(origin),
            window,
            cx,
        );
    }

    pub(crate) fn close_image(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(viewer) = self.image_viewer.take() {
            if viewer.video.is_some() {
                self.video_hub.update(cx, |hub, cx| {
                    hub.expanded = false;
                    if let Some(session) = &hub.session {
                        session.update(cx, |s, cx| s.expanded_controls(false, cx));
                    }
                    cx.notify();
                });
            }
            if self.show_queue
                || self.active_item().is_none_or(|item| {
                    item.id != viewer.item_id
                        || viewer
                            .video
                            .as_ref()
                            .is_some_and(|origin| origin.pane != item.pane)
                })
            {
                // The originating pane no longer exists; its old input may
                // have disappeared with it. Focus the current review/queue.
                window.focus(&self.focus_handle, cx);
            } else if let Some(focus) = viewer.previous_focus {
                window.focus(&focus, cx);
            } else {
                window.blur(cx);
            }
            cx.notify();
        }
    }

    pub(crate) fn render_image_viewer(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let viewer = self.image_viewer.as_ref()?;
        if self.show_queue
            || self.active_item().is_none_or(|item| {
                item.id != viewer.item_id
                    || viewer
                        .video
                        .as_ref()
                        .is_some_and(|origin| origin.pane != item.pane)
            })
        {
            self.close_image(window, cx);
            return None;
        }
        let is_video = viewer.video.is_some();
        let resource = Resource::Uri(viewer.target.url.clone());
        let loaded = if is_video {
            None
        } else {
            window.use_asset::<ImgResourceLoader>(&resource, cx)
        };
        let session = self.video_hub.read(cx).session.clone();
        let viewport = window.viewport_size();
        let toolbar_height = if is_video {
            0.
        } else if viewport.width < px(760.) {
            92.
        } else {
            52.
        };
        // Keep the native traffic lights and app header above either viewer.
        // Use this same content height for fitting images and laying out video.
        let content_height = (viewport.height - APP_HEADER_HEIGHT).max(px(0.));
        let available = size(
            (f32::from(viewport.width) - 32.).max(1.),
            (f32::from(content_height) - toolbar_height - 32.).max(1.),
        );
        let viewer = self.image_viewer.as_mut().unwrap();
        let focus = viewer.focus.clone();
        let url = viewer.target.url.clone();
        let link = viewer.target.link.clone();
        let mut label = "Image".to_string();
        let content = if is_video {
            div()
                .id("video-overlay-surface")
                .absolute()
                .inset_0()
                .flex()
                .p_3()
                .children(session)
                .into_any_element()
        } else {
            match loaded {
                Some(Ok(image)) => {
                    // Let GPUI resolve logical dimensions, including its SVG
                    // rasterization scale; RenderImage::size is in device pixels.
                    let natural = img(image.clone()).into_any_element().layout_as_root(
                        gpui::AvailableSpace::min_size(),
                        window,
                        cx,
                    );
                    let (drawn, offset, scale) = viewer.transform.geometry(
                        size(
                            f32::from(natural.width).max(1.),
                            f32::from(natural.height).max(1.),
                        ),
                        available,
                    );
                    label = format!("{:.0}%", scale * 100.);
                    div()
                        .absolute()
                        .left(px(offset.x + 16.))
                        .top(px(offset.y + 16.))
                        .w(px(drawn.width))
                        .h(px(drawn.height))
                        .child(
                            img(image)
                                .id("viewer-image")
                                .size_full()
                                .object_fit(gpui::ObjectFit::Contain),
                        )
                        .id("viewer-image-surface")
                        .debug_selector(|| "viewer-image".into())
                        .cursor_grab()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|app, event: &gpui::MouseDownEvent, _, cx| {
                                cx.stop_propagation();
                                if let Some(viewer) = &mut app.image_viewer {
                                    viewer.drag = Some((event.position, viewer.transform.pan));
                                }
                            }),
                        )
                        .on_click(|_, _, cx| cx.stop_propagation())
                        .into_any_element()
                }
                Some(Err(_)) => div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .id("viewer-error")
                            .flex()
                            .flex_col()
                            .gap_3()
                            .debug_selector(|| "viewer-error".into())
                            .on_click(|_, _, cx| cx.stop_propagation())
                            .child("Could not load this image.")
                            .child(control(
                                "viewer-retry",
                                Button::new("viewer-retry").label("Retry").small().on_click(
                                    cx.listener(|app, _, _, cx| {
                                        if let Some(viewer) = &app.image_viewer {
                                            gpui::ImageSource::from(viewer.target.url.clone())
                                                .remove_asset(cx);
                                            cx.notify();
                                        }
                                    }),
                                ),
                            )),
                    )
                    .into_any_element(),
                None => div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .debug_selector(|| "viewer-loading".into())
                    .child("Loading image…")
                    .into_any_element(),
            }
        };
        Some(
            div()
                .id("image-viewer")
                .absolute()
                .top(APP_HEADER_HEIGHT)
                .left_0()
                .right_0()
                .h(content_height)
                .occlude()
                .debug_selector(|| "image-viewer".into())
                .bg(gpui::rgb(0x101114))
                .text_color(gpui::rgb(0xeeeeee))
                .key_context("ImageViewer")
                .track_focus(&focus)
                // This is a sibling of the review's action tree. Consume physical
                // keys too, including globally bound review shortcuts and Tab.
                .capture_key_down(cx.listener(|app, event: &KeyDownEvent, window, cx| {
                    cx.stop_propagation();
                    window.prevent_default();
                    if app.image_viewer.as_ref().is_some_and(|v| v.video.is_some())
                        && event.keystroke.key != "escape"
                    {
                        if let Some(s) = app.video_hub.read(cx).session.clone() {
                            s.update(cx, |s, cx| s.key(&event.keystroke.key, cx));
                        }
                        return;
                    }
                    match event.keystroke.key.as_str() {
                        "escape" => app.close_image(window, cx),
                        "+" | "=" => {
                            if let Some(v) = &mut app.image_viewer {
                                v.transform.zoom_by(1.5);
                                cx.notify();
                            }
                        }
                        "-" => {
                            if let Some(v) = &mut app.image_viewer {
                                v.transform.zoom_by(1. / 1.5);
                                cx.notify();
                            }
                        }
                        "0" | "f" => {
                            if let Some(v) = &mut app.image_viewer {
                                v.transform.fit();
                                cx.notify();
                            }
                        }
                        _ => {}
                    }
                }))
                .on_mouse_move(cx.listener(|app, event: &MouseMoveEvent, _, cx| {
                    cx.stop_propagation();
                    let Some(viewer) = &mut app.image_viewer else {
                        return;
                    };
                    if let Some((start, pan)) = viewer.drag {
                        if event.dragging() {
                            viewer.transform.pan = point(
                                pan.x + f32::from(event.position.x - start.x),
                                pan.y + f32::from(event.position.y - start.y),
                            );
                            cx.notify();
                        } else {
                            viewer.drag = None;
                        }
                    }
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|app, _, _, cx| {
                        cx.stop_propagation();
                        if let Some(viewer) = &mut app.image_viewer {
                            viewer.drag = None;
                        }
                    }),
                )
                .on_mouse_down(MouseButton::Left, |_, window, cx| {
                    window.prevent_default();
                    cx.stop_propagation();
                })
                .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                .on_mouse_down(
                    MouseButton::Navigate(gpui::NavigationDirection::Back),
                    |_, _, cx| cx.stop_propagation(),
                )
                .on_mouse_down(
                    MouseButton::Navigate(gpui::NavigationDirection::Forward),
                    |_, _, cx| cx.stop_propagation(),
                )
                .on_scroll(cx.listener(|app, event: &gpui::ScrollEvent, _, cx| {
                    cx.stop_propagation();
                    if let Some(viewer) = &mut app.image_viewer {
                        if viewer.video.is_some() {
                            return;
                        }
                        let delta = f32::from(event.delta.pixel_delta(px(20.)).y);
                        viewer.transform.zoom_by((delta * 0.005).exp());
                        cx.notify();
                    }
                }))
                .child(
                    div()
                        .absolute()
                        .top(px(toolbar_height))
                        .bottom_0()
                        .left_0()
                        .right_0()
                        .overflow_hidden()
                        .id("viewer-backdrop")
                        .debug_selector(|| "viewer-backdrop".into())
                        .on_click(cx.listener(|app, _, window, cx| app.close_image(window, cx)))
                        .child(content),
                )
                // Video has its own in-picture chrome. Do not mount this bar:
                // `.hidden()` before `.flex()` is overridden by display:flex,
                // leaving its children overflowing a zero-height video toolbar.
                .when(!is_video, |overlay| {
                    overlay.child(
                        div()
                            .debug_selector(|| "viewer-toolbar".into())
                            .absolute()
                            .top_0()
                            .left_0()
                            .right_0()
                            .h(px(toolbar_height))
                            .px_3()
                            .flex()
                            .flex_wrap()
                            .content_center()
                            .items_center()
                            .gap_2()
                            .child(control(
                                "viewer-out",
                                Button::new("viewer-out")
                                    .label("−")
                                    .small()
                                    .on_click(cx.listener(|app, _, _, cx| {
                                        if let Some(v) = &mut app.image_viewer {
                                            v.transform.zoom_by(1. / 1.5);
                                            cx.notify();
                                        }
                                    })),
                            ))
                            .child(label)
                            .child(control(
                                "viewer-in",
                                Button::new("viewer-in")
                                    .label("+")
                                    .small()
                                    .on_click(cx.listener(|app, _, _, cx| {
                                        if let Some(v) = &mut app.image_viewer {
                                            v.transform.zoom_by(1.5);
                                            cx.notify();
                                        }
                                    })),
                            ))
                            .child(control(
                                "viewer-fit",
                                Button::new("viewer-fit").label("Fit (0)").small().on_click(
                                    cx.listener(|app, _, _, cx| {
                                        if let Some(v) = &mut app.image_viewer {
                                            v.transform.fit();
                                            cx.notify();
                                        }
                                    }),
                                ),
                            ))
                            .child(div().flex_1())
                            .when_some(link, |bar, link| {
                                bar.child(control(
                                    "viewer-link",
                                    Button::new("viewer-link")
                                        .label("Follow link")
                                        .small()
                                        .on_click(move |_, _, cx| cx.open_url(&link)),
                                ))
                            })
                            .child(control(
                                "viewer-browser",
                                Button::new("viewer-browser")
                                    .label("Open image in browser")
                                    .small()
                                    .on_click(move |_, _, cx| cx.open_url(url.as_ref())),
                            ))
                            .child(control(
                                "viewer-close",
                                Button::new("viewer-close")
                                    .label("Close (Esc)")
                                    .small()
                                    .on_click(cx.listener(|app, _, window, cx| {
                                        app.close_image(window, cx)
                                    })),
                            )),
                    )
                })
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fit_zoom_pan_and_resize() {
        let mut t = Transform {
            zoom: 1.,
            ..Default::default()
        };
        let image = size(2400., 1200.);
        assert_eq!(
            t.geometry(image, size(800., 600.)),
            (size(800., 400.), point(0., 100.), 1. / 3.)
        );
        t.zoom_by(3.);
        t.pan = point(9999., -9999.);
        let (drawn, offset, _) = t.geometry(image, size(800., 600.));
        assert_eq!(drawn, image);
        assert_eq!(t.pan, point(800., -300.));
        assert_eq!(offset, point(0., -600.));
        t.fit();
        assert_eq!(t.geometry(image, size(400., 300.)).0, size(400., 200.));
        assert_eq!(t.pan, point(0., 0.));
        t.zoom_by(1e6);
        assert_eq!(t.zoom, 32.);
        t.zoom_by(0.);
        assert_eq!(t.zoom, 0.1);
        t.fit();
        assert_eq!(
            t.geometry(size(100., 50.), size(800., 600.)).0,
            size(100., 50.)
        );
    }
}
