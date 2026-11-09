//! Remember the normal window size, independently of fullscreen/zoom state.

use crate::local_store::LocalStore;
use gpui::{px, size, App, Bounds, Pixels, Size, Window, WindowBounds};
use serde_json::json;

const SETTING: &str = "window_size";
const DEFAULT_SIZE: Size<Pixels> = size(px(1280.), px(860.));

pub(crate) fn initial_bounds(store: &LocalStore, cx: &App) -> Bounds<Pixels> {
    // Centering also caps the size to the display's visible area, so a size
    // saved on a larger monitor still fits after that monitor is unplugged.
    Bounds::centered(None, restored_size(store).unwrap_or(DEFAULT_SIZE), cx)
}

fn restored_size(store: &LocalStore) -> Option<Size<Pixels>> {
    let value = store.setting(SETTING)?;
    valid_size(size(
        px(value.get("width")?.as_f64()? as f32),
        px(value.get("height")?.as_f64()? as f32),
    ))
}

fn valid_size(size: Size<Pixels>) -> Option<Size<Pixels>> {
    [size.width, size.height]
        .into_iter()
        .all(|dimension| f32::from(dimension).is_finite() && dimension > px(0.))
        .then_some(size)
}

pub(crate) fn normal_size(window: &Window) -> Option<Size<Pixels>> {
    windowed_size(window.window_bounds(), window.is_maximized())
}

fn windowed_size(bounds: WindowBounds, maximized: bool) -> Option<Size<Pixels>> {
    match bounds {
        WindowBounds::Windowed(bounds) if !maximized => valid_size(bounds.size),
        _ => None,
    }
}

pub(crate) fn persist(store: &LocalStore, size: Option<Size<Pixels>>) {
    let Some(size) = size.and_then(valid_size) else {
        return;
    };
    store.set_setting(
        SETTING,
        json!({ "width": f32::from(size.width), "height": f32::from(size.height) }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::point;
    use serde_json::Value;

    #[gpui::test]
    fn restored_bounds_are_centered_and_fit_the_current_display(cx: &mut gpui::TestAppContext) {
        let store = LocalStore::in_memory();
        cx.update(|cx| {
            assert_eq!(initial_bounds(&store, cx).size, DEFAULT_SIZE);
            let expected = size(px(1100.), px(750.));
            persist(&store, Some(expected));
            let display = cx.primary_display().unwrap().visible_bounds();
            let bounds = initial_bounds(&store, cx);
            assert_eq!(bounds.size, expected);
            assert_eq!(bounds.center(), display.center());

            persist(&store, Some(size(px(10000.), px(10000.))));
            assert_eq!(initial_bounds(&store, cx), display);
        });
    }

    #[test]
    fn size_round_trips_without_changing_other_settings() {
        let store = LocalStore::in_memory();
        store.set_setting("sidebar_width", json!(300.));
        let expected = size(px(1432.), px(947.));
        persist(&store, Some(expected));
        assert_eq!(restored_size(&store), Some(expected));
        assert_eq!(store.setting("sidebar_width"), Some(json!(300.)));

        // Closing while fullscreen, with no normal size available, must not
        // erase the size from a previous launch.
        persist(&store, None);
        assert_eq!(restored_size(&store), Some(expected));
    }

    #[test]
    fn missing_or_invalid_settings_use_the_default() {
        let store = LocalStore::in_memory();
        assert_eq!(restored_size(&store).unwrap_or(DEFAULT_SIZE), DEFAULT_SIZE);
        for invalid in [
            Value::Null,
            json!("invalid"),
            json!({}),
            json!({ "width": 1000 }),
            json!({ "width": "1000", "height": 800 }),
            json!({ "width": 0, "height": 800 }),
            json!({ "width": 1000, "height": -1 }),
            json!({ "width": 1e100, "height": 800 }),
        ] {
            store.set_setting(SETTING, invalid.clone());
            assert_eq!(restored_size(&store), None, "{invalid}");
        }
        for invalid in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0., -1.] {
            assert_eq!(valid_size(size(px(invalid), px(800.))), None);
            assert_eq!(valid_size(size(px(1000.), px(invalid))), None);
        }
    }

    #[test]
    fn fullscreen_and_maximized_sizes_are_not_remembered() {
        let bounds = Bounds::new(point(px(20.), px(30.)), size(px(1000.), px(700.)));
        assert_eq!(
            windowed_size(WindowBounds::Windowed(bounds), false),
            Some(bounds.size)
        );
        assert_eq!(windowed_size(WindowBounds::Windowed(bounds), true), None);
        assert_eq!(windowed_size(WindowBounds::Maximized(bounds), true), None);
        assert_eq!(windowed_size(WindowBounds::Fullscreen(bounds), false), None);
    }
}
