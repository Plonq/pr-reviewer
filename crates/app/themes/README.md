# Gander theme

`catppuccin-macchiato.json` is the app's bundled, dark-only GPUI Kit theme.
It supplies both UI colors and syntax highlighting; there is no light variant
and macOS appearance changes do not select another theme.

Based on the [Catppuccin Macchiato palette](https://catppuccin.com/palette/):
blue-grey base `#24273a`, mantle `#1e2030`, soft text `#cad3f5`, and pastel
accents. Compared with the former Mocha palette, Macchiato lifts the main
background. Comments use the brighter overlay2 color for readability, and
popovers sit on a slightly raised `#2b2f44` surface. Diff washes remain at
12% opacity, with 30% word-level highlights.

Edit the JSON and rebuild to tune the palette. `theme::apply_ui_theme` installs
it via `Theme::update` / `apply_config`, keeping Kit backgrounds, Base controls,
and Markdown defaults synchronized. The theme tests catch unknown JSON keys
(including syntax keys silently ignored by Kit) and verify the shared palette.
