//! Domain-role adaptation only. Kit owns UI appearance and syntax colors.
use gpui::component::{ActiveTheme, Theme, ThemeMode};
use gpui::{App, HighlightStyle, Rgba, Window};
use syntax::Token;

/// Initialization does not establish system following; synchronize explicitly
/// after Kit init and whenever the native window appearance changes.
pub fn apply_ui_theme(window: Option<&mut Window>, cx: &mut App) {
    let appearance = window
        .as_ref()
        .map(|w| w.appearance())
        .unwrap_or_else(|| cx.window_appearance());
    Theme::change(ThemeMode::from(appearance), window, cx);
}

macro_rules! role {
    ($($name:ident => $field:ident),* $(,)?) => {$ (
        pub fn $name(cx: &App) -> Rgba { cx.theme().$field.into() }
    )*};
}

role! {
    base => background, mantle => sidebar, crust => secondary,
    surface0 => border, text => foreground, subtext => muted_foreground,
    overlay0 => muted_foreground, green => success, red => danger,
    blue => info, mauve => link, peach => warning, yellow => warning,
    selection_bg => selection,
}

pub fn is_light(cx: &App) -> bool {
    !cx.theme().mode.is_dark()
}

/// Keep our tree-sitter token model, resolving captures through Kit's stock
/// syntax theme rather than maintaining another palette.
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
        Token::Parameter => "variable.parameter",
        Token::Operator => "operator",
        Token::Punctuation => "punctuation",
        Token::Attribute => "attribute",
        Token::Label => "label",
        Token::Namespace => "namespace",
    };
    cx.theme()
        .highlight_theme
        .style
        .syntax
        .style(capture)
        .unwrap_or_default()
}

pub fn modal_backdrop(cx: &App) -> Rgba {
    cx.theme().overlay.into()
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

    #[gpui::test]
    fn roles_and_syntax_follow_stock_appearance(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            gpui::init(cx);
            for mode in [ThemeMode::Light, ThemeMode::Dark] {
                Theme::change(mode, None, cx);
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
                assert_ne!(added_row_bg(cx), removed_row_bg(cx));
                assert!(added_word_bg(cx).a > added_row_bg(cx).a);
            }
        });
    }
}
