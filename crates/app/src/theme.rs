//! Catppuccin palette, following the system appearance: Mocha (dark) and
//! Latte (light). The active mode is a process-global atomic set at startup
//! and from the window's appearance-change observer, so every color helper
//! stays a free function.

use gpui::component::{Theme, ThemeMode};
use gpui::{rgb, rgba, App, FontStyle, HighlightStyle, Hsla, Rgba, Window};
use std::sync::atomic::{AtomicBool, Ordering};
use syntax::Token;

/// The active appearance mode: true = Latte (light), false = Mocha (dark).
/// Reads are cheap (relaxed atomic load), so helpers branch on it freely.
static LIGHT: AtomicBool = AtomicBool::new(false);

/// Whether the light (Latte) palette is currently active.
pub fn is_light() -> bool {
    LIGHT.load(Ordering::Relaxed)
}

pub fn base() -> Rgba {
    if is_light() {
        rgb(0xeff1f5)
    } else {
        rgb(0x1e1e2e)
    }
}
pub fn mantle() -> Rgba {
    if is_light() {
        rgb(0xe6e9ef)
    } else {
        rgb(0x181825)
    }
}
pub fn crust() -> Rgba {
    if is_light() {
        rgb(0xdce0f8)
    } else {
        rgb(0x11111b)
    }
}
pub fn surface0() -> Rgba {
    if is_light() {
        rgb(0xccd0da)
    } else {
        rgb(0x313244)
    }
}
pub fn text() -> Rgba {
    if is_light() {
        rgb(0x4c4f69)
    } else {
        rgb(0xcdd6f4)
    }
}
pub fn subtext() -> Rgba {
    if is_light() {
        rgb(0x6c6f85)
    } else {
        rgb(0xa6adc8)
    }
}
pub fn overlay0() -> Rgba {
    if is_light() {
        rgb(0x9ca0b0)
    } else {
        rgb(0x6c7086)
    }
}
pub fn green() -> Rgba {
    if is_light() {
        rgb(0x40a02b)
    } else {
        rgb(0xa6e3a1)
    }
}
pub fn red() -> Rgba {
    if is_light() {
        rgb(0xd20f39)
    } else {
        rgb(0xf38ba8)
    }
}
pub fn blue() -> Rgba {
    if is_light() {
        rgb(0x1e66f5)
    } else {
        rgb(0x89b4fa)
    }
}
pub fn mauve() -> Rgba {
    if is_light() {
        rgb(0x8839ef)
    } else {
        rgb(0xcba6f7)
    }
}
pub fn peach() -> Rgba {
    if is_light() {
        rgb(0xfe640b)
    } else {
        rgb(0xfab387)
    }
}
pub fn yellow() -> Rgba {
    if is_light() {
        rgb(0xdf8e1d)
    } else {
        rgb(0xf9e2af)
    }
}

/// Follow the system appearance: set gpui-component's `Theme` mode and swap
/// the Catppuccin palette (Mocha ↔ Latte). Pass the window when one exists
/// so component repaints are flushed immediately; call again from the
/// window's appearance-change observer to track live switches. Call after
/// `gpui::component::init`.
pub fn apply_ui_theme(window: Option<&mut Window>, cx: &mut App) {
    let appearance = window
        .as_ref()
        .map(|window| window.appearance())
        .unwrap_or_else(|| cx.window_appearance());
    let mode = ThemeMode::from(appearance);
    LIGHT.store(!mode.is_dark(), Ordering::Relaxed);
    Theme::change(mode, window, cx);

    let base: Hsla = base().into();
    let mantle: Hsla = mantle().into();
    let crust: Hsla = crust().into();
    let surface0: Hsla = surface0().into();
    let text: Hsla = text().into();
    let overlay0: Hsla = overlay0().into();
    let green: Hsla = green().into();
    let red: Hsla = red().into();
    let blue: Hsla = blue().into();
    let peach: Hsla = peach().into();

    let theme = Theme::global_mut(cx);
    theme.background = base;
    theme.foreground = text;
    theme.muted = surface0;
    theme.muted_foreground = overlay0;
    theme.border = surface0;
    theme.input = surface0;
    theme.ring = blue;
    theme.primary = blue;
    theme.primary_hover = blue.opacity(0.9);
    theme.primary_active = blue.opacity(0.8);
    theme.primary_foreground = crust;
    theme.secondary = surface0;
    theme.secondary_hover = surface0.opacity(0.8);
    theme.secondary_active = surface0.opacity(0.6);
    theme.secondary_foreground = text;
    theme.accent = surface0;
    theme.accent_foreground = text;
    theme.danger = red;
    theme.danger_hover = red.opacity(0.9);
    theme.danger_active = red.opacity(0.8);
    theme.danger_foreground = crust;
    theme.success = green;
    theme.success_hover = green.opacity(0.9);
    theme.success_active = green.opacity(0.8);
    theme.success_foreground = crust;
    theme.warning = peach;
    theme.warning_hover = peach.opacity(0.9);
    theme.warning_active = peach.opacity(0.8);
    theme.warning_foreground = crust;
    theme.info = blue;
    theme.info_hover = blue.opacity(0.9);
    theme.info_active = blue.opacity(0.8);
    theme.info_foreground = crust;
    theme.link = blue;
    theme.link_hover = blue.opacity(0.9);
    theme.link_active = blue.opacity(0.8);
    theme.popover = mantle;
    theme.popover_foreground = text;
    theme.title_bar = mantle;
    theme.title_bar_border = surface0;
    theme.sidebar = mantle;
    theme.sidebar_foreground = text;
    theme.sidebar_border = surface0;
    theme.caret = text;
    theme.selection = blue.opacity(0.3);
    // Keep the track invisible when scrolling/hovering, just as it is when idle.
    theme.scrollbar = Hsla::transparent_black();
    theme.scrollbar_thumb = overlay0.opacity(0.5);
    theme.scrollbar_thumb_hover = overlay0;
    theme.window_border = surface0;
}

/// Catppuccin syntax palette for tree-sitter tokens: Latte in light mode,
/// Mocha in dark. Variable and Embedded map to the plain text color (syntax
/// spans for them are not emitted, so this is belt-and-braces).
pub fn token_style(token: Token) -> HighlightStyle {
    // (Latte / light, Mocha / dark, italic)
    let (light, dark, italic) = match token {
        Token::Keyword => (0x8839ef, 0xcba6f7, false), // mauve
        Token::Function => (0x1e66f5, 0x89b4fa, false), // blue
        Token::Type => (0xdf8e1d, 0xf9e2af, false),    // yellow
        Token::String => (0x40a02b, 0xa6e3a1, false),  // green
        Token::Number | Token::Constant => (0xfe640b, 0xfab387, false), // peach
        Token::Comment => (0x9ca0b0, 0x6c7086, true),  // overlay0
        Token::Property => (0x7287fd, 0xb4befe, false), // lavender
        Token::Variable | Token::Embedded => (0x4c4f69, 0xcdd6f4, false), // text
        Token::Parameter => (0xe64553, 0xeba0ac, true), // maroon
        Token::Operator => (0x04a5e5, 0x89dceb, false), // sky
        Token::Punctuation => (0x7c7f93, 0x9399b2, false), // overlay2
        Token::Attribute | Token::Label => (0xdf8e1d, 0xf9e2af, false), // yellow
        Token::Namespace => (0xfe640b, 0xfab387, true), // peach
    };
    let color = if is_light() { light } else { dark };
    HighlightStyle {
        color: Some(rgb(color).into()),
        font_style: italic.then_some(FontStyle::Italic),
        ..Default::default()
    }
}

/// Dimming layer behind the review modal.
pub fn modal_backdrop() -> Rgba {
    if is_light() {
        rgba(0xdce0f8aa)
    } else {
        rgba(0x11111baa)
    }
}

/// Split view: background for the absent side of a one-sided row — a fill
/// darker than any content row so it clearly reads as "nothing here".
pub fn void_cell_bg() -> Rgba {
    if is_light() {
        rgba(0xdce0f899)
    } else {
        rgba(0x11111b99)
    }
}

/// Text selection in the diff pane — same blue.opacity(0.3) injected into the
/// gpui-component theme as `selection` in `apply_ui_theme`.
pub fn selection_bg() -> Rgba {
    if is_light() {
        rgba(0x1e66f54d)
    } else {
        rgba(0x89b4fa4d)
    }
}

/// A quiet click anchor in the line-number gutter, never over the code.
pub fn active_line_gutter_bg() -> Rgba {
    if is_light() {
        rgba(0x1e66f514)
    } else {
        rgba(0x89b4fa14)
    }
}

/// A full-line comment anchor wash, distinct from selectable text and gentle
/// enough to retain the added/removed row and word-diff backgrounds beneath it.
pub fn comment_range_bg() -> Rgba {
    if is_light() {
        rgba(0x1e66f51f)
    } else {
        rgba(0x89b4fa1f)
    }
}

/// Review comment card: a blue tint that lifts the thread off the diff without
/// hiding it, with a stronger wash behind each comment's author line.
pub fn comment_bg() -> Rgba {
    if is_light() {
        rgba(0x1e66f514)
    } else {
        rgba(0x89b4fa14)
    }
}
pub fn comment_header_bg() -> Rgba {
    if is_light() {
        rgba(0x1e66f52b)
    } else {
        rgba(0x89b4fa2b)
    }
}

/// Outline drawn around a whole thread: sky at partial alpha, so it reads as a
/// soft neon edge against the pane without competing with the diff.
pub fn comment_outline() -> Rgba {
    if is_light() {
        rgba(0x04a5e58c)
    } else {
        rgba(0x89dceb8c)
    }
}

/// Low-alpha tints: syntax/text must stay readable on top. Never opaque.
pub fn added_row_bg() -> Rgba {
    if is_light() {
        rgba(0x40a02b20)
    } else {
        rgba(0xa6e3a120)
    }
}
pub fn removed_row_bg() -> Rgba {
    if is_light() {
        rgba(0xd20f3920)
    } else {
        rgba(0xf38ba820)
    }
}
pub fn added_word_bg() -> Rgba {
    if is_light() {
        rgba(0x40a02b48)
    } else {
        rgba(0xa6e3a148)
    }
}
pub fn removed_word_bg() -> Rgba {
    if is_light() {
        rgba(0xd20f3948)
    } else {
        rgba(0xf38ba848)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn scrollbar_track_is_transparent_but_thumbs_remain_visible(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            gpui::component::init(cx);
            apply_ui_theme(None, cx);

            let theme = Theme::global(cx);
            assert_eq!(theme.scrollbar.a, 0.0);
            assert!(theme.scrollbar_thumb.a > 0.0);
            assert!(theme.scrollbar_thumb_hover.a > 0.0);
        });
    }
}
