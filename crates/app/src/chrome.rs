//! App chrome: loading strip, centered placeholder, and window title.

use crate::theme;
use gpui::component::{spinner::Spinner, tooltip::Tooltip, Sizable as _};
use gpui::{
    canvas, div, fill, point, prelude::*, px, size, Animation, AnimationExt, Bounds, SharedString,
};
use std::time::Duration;

const LOADING_STRIPE_PITCH: f32 = 52.;
const LOADING_STRIPE_WIDTH: f32 = 26.;
const LOADING_STRIP_HEIGHT: f32 = 1.;

/// One pitch per second, with fixed-size stripes (five at the default 260px).
/// Clipping the edge stripes makes phase 1 identical to phase 0: no loop jump.
fn loading_stripes(width: f32, phase: f32) -> impl Iterator<Item = (f32, f32)> {
    (-1..=(width / LOADING_STRIPE_PITCH).ceil() as i32).filter_map(move |ix| {
        let start = (ix as f32 + phase) * LOADING_STRIPE_PITCH;
        let left = start.max(0.);
        let right = (start + LOADING_STRIPE_WIDTH).min(width);
        (right > left).then_some((left, right - left))
    })
}

pub(crate) fn file_tree_loading_strip(
    message: Option<&'static str>,
    cx: &gpui::App,
) -> gpui::AnyElement {
    // Always reserve the same pixel, including when idle. Loading must
    // never move the filter, tree rows, or the user's current click target.
    div()
        .id("file-tree-loading-track")
        .debug_selector(|| "file-tree-loading-track".into())
        .w_full()
        .h(px(LOADING_STRIP_HEIGHT))
        .flex_shrink_0()
        .when_some(message, |track, message| {
            let color = gpui::Hsla::from(theme::blue(cx));
            track
                .tooltip(move |window, cx| Tooltip::new(message).build(window, cx))
                .bg(color.opacity(0.15))
                .child(
                    div()
                        .debug_selector(|| "file-tree-loading".into())
                        .size_full()
                        .with_animation(
                            "file-tree-loading-stripes",
                            // Linear by default; no easing, pulsing, or stretching.
                            // GPUI also keeps this static under reduced motion.
                            Animation::new(Duration::from_secs(1)).repeat(),
                            move |strip, phase| {
                                strip.child(
                                    canvas(
                                        |_, _, _| (),
                                        move |bounds, _, window, _| {
                                            for (left, width) in
                                                loading_stripes(bounds.size.width.into(), phase)
                                            {
                                                window.paint_quad(fill(
                                                    Bounds::new(
                                                        bounds.origin + point(px(left), px(0.)),
                                                        size(px(width), bounds.size.height),
                                                    ),
                                                    color,
                                                ));
                                            }
                                        },
                                    )
                                    .size_full(),
                                )
                            },
                        ),
                )
        })
        .into_any_element()
}

pub(crate) fn header_loading_spinner(message: &'static str, cx: &gpui::App) -> gpui::AnyElement {
    div()
        .id("header-loading")
        .debug_selector(|| "header-loading".into())
        .flex()
        .items_center()
        .flex_shrink_0()
        .tooltip(move |window, cx| Tooltip::new(message).build(window, cx))
        .child(Spinner::new().xsmall().color(theme::overlay0(cx).into()))
        .into_any_element()
}

pub(crate) fn centered_message(text: SharedString, color: gpui::Rgba) -> gpui::AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .text_color(color)
        .child(text)
        .into_any_element()
}

pub(crate) fn app_title(detail: Option<String>, cx: &gpui::App) -> gpui::AnyElement {
    let mut title = div()
        .flex()
        .items_center()
        .gap_2()
        .flex_1()
        .min_w_0()
        .child(
            div()
                .font_weight(gpui::FontWeight::BOLD)
                .child(SharedString::from("Gander")),
        );
    if let Some(detail) = detail {
        title = title.child(
            div()
                .text_color(theme::subtext(cx))
                .truncate()
                .child(SharedString::from(detail)),
        );
    }
    title.into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loading_stripes_are_fixed_width_and_move_at_constant_speed() {
        let stripes = loading_stripes(260., 0.).collect::<Vec<_>>();
        assert_eq!(stripes.len(), 5);
        assert!(stripes
            .iter()
            .all(|(_, width)| *width == LOADING_STRIPE_WIDTH));
        for phase in [0.25, 0.5, 0.75] {
            let moved = loading_stripes(260., phase).collect::<Vec<_>>();
            // This interior stripe is never clipped or stretched.
            assert!(moved.contains(&(LOADING_STRIPE_PITCH * (1. + phase), LOADING_STRIPE_WIDTH)));
        }
    }

    #[test]
    fn loading_stripes_loop_seamlessly_and_clip_at_any_pane_width() {
        for width in [1., 180., 260., 375., 600.] {
            assert_eq!(
                loading_stripes(width, 0.).collect::<Vec<_>>(),
                loading_stripes(width, 1.).collect::<Vec<_>>(),
            );
            for phase in [0., 0.25, 0.5, 0.75, 1.] {
                for (left, stripe_width) in loading_stripes(width, phase) {
                    assert!(left >= 0.);
                    assert!(stripe_width > 0. && stripe_width <= LOADING_STRIPE_WIDTH);
                    assert!(left + stripe_width <= width);
                }
            }
        }
    }
}
