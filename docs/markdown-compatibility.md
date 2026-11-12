# GitHub Markdown compatibility

The target is **GitHub PR descriptions, reviews, and comments**, not the rendering
of repository `.md` files. GitHub treats single newlines in PR/comment prose as
visible line breaks; repository Markdown normally reflows them. See [GitHub's
line-break documentation](https://docs.github.com/en/get-started/writing-on-github/getting-started-with-writing-and-formatting-on-github/basic-writing-and-formatting-syntax#line-breaks).

## Rendering path

- Kit's `TextView` provides native selectable GFM prose: headings, emphasis,
  links, code, quotes, lists, task lists, tables, and strikethrough.
- The shared conversation preprocessing pipe expands emoji shortcodes, mentions,
  issue references, and configured Jira keys. It then converts paragraph soft
  line endings into explicit Markdown hard breaks. This last step uses the AST,
  not a global newline replacement: code, raw HTML, math, tables, and reference
  definitions retain their original syntax. Existing hard breaks are unchanged.
- Images, including full/collapsed/shortcut reference images and GitHub's pasted
  image HTML, use the app's image loader and viewer. Only HTTP(S) Markdown image
  targets are loaded; resolving a reference must not grant filesystem access.
- Mermaid and video attachments use the existing native extension hooks.
- Conversation documents retain both their original input and their TextView
  state. Preprocessing runs again only when the body or repository context
  changes. Diff comments keep their prepared Markdown in the measured layout.

## Remaining differences

This is not a complete implementation of GitHub's web rendering pipeline:

- General HTML uses Kit's supported subset, not a browser. In particular,
  collapsible `details` and arbitrary HTML layout are not GitHub-equivalent.
- Math is not typeset, footnote presentation is limited, and GitHub alerts do not
  have their special callout presentation.
- Relative repository links/images do not yet resolve against a PR's repository,
  revision, and file path. User/team/issue links do not have GitHub hover cards.
- Emoji/mention/issue expansion is still the older line-based scanner; unlike
  the new hard-break pass, it is not syntax-aware in every Markdown construct.
- Typography, spacing, syntax highlighting, and some image sizing differ from
  GitHub's browser/CSS presentation.

Tests cover the line-break adapter's literal/structural exclusions, Unicode and
line endings, reference resolution and safe image URLs, and actual rendered
heights/selection in unified, split, and retained conversation views. Existing
media tests exercise tables, lists, links, HTML images, and asynchronous layout.
