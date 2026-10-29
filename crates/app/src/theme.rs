//! One dark-only Catppuccin theme shared by Kit controls, Markdown and diffs.
use gpui::component::{ActiveTheme, Theme, ThemeConfig};
use gpui::{App, HighlightStyle, Rgba};
use std::rc::Rc;
use syntax::Token;

/// Install through Kit's supported write path so component tokens, TextView
/// defaults and the Base scrollbar/resize handles all receive the same palette.
/// Deliberately independent of the native window's light/dark appearance.
pub fn apply_ui_theme(cx: &mut App) {
    let config: ThemeConfig =
        serde_json::from_str(include_str!("../themes/catppuccin-macchiato.json"))
            .expect("bundled Catppuccin theme must be valid");
    Theme::update(cx, |theme| theme.apply_config(&Rc::new(config)));
}

macro_rules! role {
    ($($name:ident => $field:ident),* $(,)?) => {$ (
        pub fn $name(cx: &App) -> Rgba { cx.theme().$field.into() }
    )*};
}

role! {
    base => background, mantle => sidebar,
    surface0 => border, text => foreground, subtext => muted_foreground,
    overlay0 => muted_foreground, green => success, red => danger,
    blue => info, mauve => magenta, yellow => warning,
    selection_bg => selection,
}

pub fn crust(cx: &App) -> Rgba {
    cx.theme()
        .highlight_theme
        .style
        .editor_gutter_background
        .unwrap_or(cx.theme().sidebar)
        .into()
}

pub fn peach(cx: &App) -> Rgba {
    token_style(Token::Number, cx)
        .color
        .unwrap_or(cx.theme().warning)
        .into()
}

pub fn is_light(cx: &App) -> bool {
    !cx.theme().mode.is_dark()
}

/// Resolve our tree-sitter tokens through the same syntax theme as Kit's editors.
pub fn token_style(token: Token, cx: &App) -> HighlightStyle {
    let capture = match token {
        Token::Keyword => "keyword",
        Token::Function => "function",
        Token::Type => "type",
        Token::String => "string",
        Token::Number => "number",
        Token::Constant => "constant",
        Token::Comment => "comment",
        Token::Property => "property",
        Token::Variable => "variable",
        Token::Embedded => "embedded",
        Token::Parameter => "variable.special",
        Token::Operator => "operator",
        Token::Punctuation => "punctuation",
        Token::Attribute => "attribute",
        Token::Label => "label",
        Token::Namespace => "constant",
    };
    cx.theme()
        .highlight_theme
        .style
        .syntax
        .style(capture)
        .unwrap_or_default()
}

pub fn search_current_bg(cx: &App) -> Rgba {
    cx.theme().warning.opacity(0.45).into()
}
pub fn search_match_bg(cx: &App) -> Rgba {
    cx.theme().warning.opacity(0.20).into()
}
pub fn void_cell_bg(cx: &App) -> Rgba {
    cx.theme().muted.opacity(0.6).into()
}
pub fn active_line_gutter_bg(cx: &App) -> Rgba {
    cx.theme().info.opacity(0.08).into()
}
pub fn comment_range_bg(cx: &App) -> Rgba {
    cx.theme().info.opacity(0.12).into()
}
pub fn comment_bg(cx: &App) -> Rgba {
    cx.theme().info.opacity(0.08).into()
}
pub fn comment_header_bg(cx: &App) -> Rgba {
    cx.theme().info.opacity(0.17).into()
}
pub fn comment_outline(cx: &App) -> Rgba {
    cx.theme().info.opacity(0.55).into()
}
// Match Kit Diff's normal and emphasized change overlay strengths.
pub fn added_row_bg(cx: &App) -> Rgba {
    cx.theme().success.opacity(0.12).into()
}
pub fn removed_row_bg(cx: &App) -> Rgba {
    cx.theme().danger.opacity(0.12).into()
}
pub fn added_word_bg(cx: &App) -> Rgba {
    cx.theme().success.opacity(0.30).into()
}
pub fn removed_word_bg(cx: &App) -> Rgba {
    cx.theme().danger.opacity(0.30).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::component::ThemeMode;
    use gpui::{rgb, Hsla};

    #[gpui::test]
    fn catppuccin_is_dark_and_shared_by_controls_and_syntax(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            gpui::init(cx);
            for mode in [ThemeMode::Light, ThemeMode::Dark] {
                Theme::change(mode, None, cx);
                apply_ui_theme(cx);
                assert_eq!(cx.theme().mode, ThemeMode::Dark);
                assert_eq!(
                    cx.theme().theme_name().as_ref(),
                    "Gander · Catppuccin Macchiato"
                );
                assert_eq!(cx.theme().background, Hsla::from(rgb(0x24273a)));
                assert_eq!(cx.theme().tokens.background.color, cx.theme().background);
                assert_eq!(cx.theme().tokens.sidebar.color, cx.theme().sidebar);
                assert_eq!(cx.theme().button_primary, cx.theme().primary);
                assert_eq!(cx.theme().tokens.button_primary.color, cx.theme().primary);
                assert_eq!(cx.theme().scrollbar.a, 0.0);
                assert!(cx.theme().scrollbar_thumb.a > 0.0);
                assert_eq!(base(cx), Rgba::from(cx.theme().background));
                assert_eq!(selection_bg(cx), Rgba::from(cx.theme().selection));
                assert_eq!(
                    token_style(Token::Keyword, cx),
                    cx.theme()
                        .highlight_theme
                        .style
                        .syntax
                        .style("keyword")
                        .unwrap_or_default()
                );
                assert_eq!(
                    token_style(Token::Keyword, cx).color,
                    Some(rgb(0xc6a0f6).into())
                );
                assert_ne!(peach(cx), yellow(cx));
                assert_ne!(mauve(cx), blue(cx));
                assert_ne!(added_row_bg(cx), removed_row_bg(cx));
                assert!(added_word_bg(cx).a > added_row_bg(cx).a);
            }
        });
    }

    #[test]
    fn bundled_theme_has_no_unknown_fields() {
        // ThemeConfig accepts unknown JSON keys; a round-trip catches typos
        // that would silently fall back to Kit's default colors or syntax.
        let source: serde_json::Value =
            serde_json::from_str(include_str!("../themes/catppuccin-macchiato.json")).unwrap();
        let config: ThemeConfig = serde_json::from_value(source.clone()).unwrap();
        for (key, value) in source["colors"].as_object().unwrap() {
            gpui::component::try_parse_color(value.as_str().unwrap())
                .unwrap_or_else(|_| panic!("invalid theme color: {key}"));
        }
        let parsed = serde_json::to_value(config).unwrap();
        fn check(source: &serde_json::Value, parsed: &serde_json::Value) {
            if let Some(object) = source.as_object() {
                for (key, value) in object {
                    assert!(parsed.get(key).is_some(), "unknown theme key: {key}");
                    check(value, &parsed[key]);
                }
            }
        }
        check(&source, &parsed);
    }
}
