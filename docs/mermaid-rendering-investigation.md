# Mermaid rendering investigation

## Implementation in the current workspace

The implementation now retains Kit and adds the narrow block plugin in
`crates/app/src/conversation/markdown/mermaid.rs`, registered centrally in
`markdown.rs` for both persistent conversation documents and transient comments.
The original investigation below is preserved as historical context; its
proposal-only statements describe the state before this implementation.

### Supported scope and source access

- Pinned `merman = 0.10.0-alpha.2`, default features disabled, with SVG,
  native-font measurement, and flowchart, sequence, class, state and ER families.
  No browser, JavaScript runtime, Node/Chromium, or hosted rendering service.
- Only an mdast code node with the exact language `mermaid` is claimed. Other
  Markdown/code remains stock. Blocks over the source limit are left entirely
  stock, without adding them to the diagram cache.
- Plain node text, exact original fenced Markdown, source range and an accessible
  source label are preserved. Success cards have keyboard-capable Kit buttons for
  source disclosure, copying source and copying original Markdown. Pending/error
  cards show stock Kit code. The standalone source view uses the de-indented
  mdast value, so nested quote/list prefixes do not corrupt the displayed code;
  Markdown copy still uses the exact original source slice.
- This is deliberately a subset, not GitHub Mermaid parity. Source configuration
  directives/frontmatter, custom styles, image/icon nodes and diagram navigation
  fall back rather than being silently removed. Conservative token admission can
  also reject ordinary labels/comments containing words such as `image` or
  `link`; readable source is preferable to weakening this first policy.
- Diagram typography is host-fixed Arial, not the prose font. A closed set of
  macOS Arial regular/bold/italic/bold-italic files and optional Arial Unicode is
  loaded from fixed OS-owned paths. **No PR-supplied font path is opened.** Missing
  fonts or glyph coverage cause source fallback; in particular, emoji outside
  this closed set are not represented by missing-glyph boxes.

### Background work, geometry and cache

One per-App hub is shared across documents, PRs, and `-measure`/display hosts.
Keys contain source, light/dark mode, font and renderer/security policy revision,
plus a quantized device scale (1–3). Theme is read at render time, so a theme
change does not need to invalidate or reparse the Markdown AST. The central
parser revision was incremented for the new plugin registration.

Layout, SVG admission, text shaping and rasterization run off the UI thread.
There are two active workers and at most sixteen pending/running requests;
identical requests share their pending entry. Results are never written into an
active document/PR: a frame can only retrieve its exact source/config key. Old
work may finish within its bounded operation after an edit/switch, but cannot
replace the new diagram. Queue saturation is a transient readable-source state.
Normal failures are cached as well as successes.

Cached geometry is intrinsic logical size, fitted uniformly to available prose
width. Display uses already-decoded `Arc<RenderImage>` pixels, not a URL or
GPUI's default SVG loader, and does not create a new image per frame. Image
objects and geometry are shared by measurement and display. The existing
`on_children_prepainted` reservation updates and anchored rebuilds handle async
changes and source disclosure without modifying source-line geometry. Persistent
`TextViewState` ownership is unchanged.

The cache has a 128-entry / 64 MiB accounting budget, including decoded CPU and
GPU pixel copies. Entry eviction drops GPU textures explicitly, rather than
leaving a second decoder/asset cache resident. Memory-evicted successes retain
small negative entries (source fallback) to avoid continuous re-layout/eviction
loops when several large diagrams are visible. These entries eventually leave
through normal entry-count eviction; editing/configuration changes use new keys.
The budget excludes bounded worker scratch space, the shared font set, and
references held by the current/previous frame; it is not a process RSS limit.

### Host-owned security and resource limits

Merman uses `SvgPipeline::resvg_safe()` and strict host config. Finite resource
policies live outside source configuration, and source directives are rejected.
Generated SVG then passes an independent fail-closed XML/tag/attribute audit:
no image, foreignObject, navigation, script, event attributes, external hrefs,
CSS imports/fonts/escaped URLs, DTDs or processing instructions. Only local
fragment references are allowed, with bounded reference expansion. Unsupported
output is rejected, not stripped into a potentially misleading partial diagram.
The `usvg` data and string image resolvers both unconditionally return `None`.
Only the closed, bounded host font set is supplied for measurement and painting.
GPUI receives BGRA pixels, never diagram-provided SVG or resource locations.

| Resource | Host ceiling |
| --- | --- |
| Source / aggregate model label text | 16 KiB each |
| Model items / nesting | 256 / 16 |
| Layout work units / prepared text | 200,000 / 2 MiB |
| SVG serialized bytes / XML elements | 512 KiB / 10,000 |
| XML depth / expanded reference traversal | 64 / 20,000 nodes |
| Raster dimensions / pixel area | 4096 per side / 4,194,304 pixels, checked at device scale before allocation |
| Font acquisition | 8 fixed files, 8 faces, 32 MiB; no system discovery |
| Per-operation font shaping cache | 2048 measurements, 256 entries, 1 MiB retained cache; 16 KiB label limit |
| Deadline / workers / pending+running | 2 seconds / 2 / 16 |
| Result cache | 128 entries / 64 MiB including CPU+GPU pixels |

Cancellation/deadlines are cooperative during Merman layout and font shaping.
SVG parsing/rasterization is separately bounded by input/tree/pixel limits and
checked before/after the operation, not preempted mid-instruction. These controls
are **not** a hard memory/time sandbox or a security audit of Merman/resvg. An
out-of-process hard RSS/CPU boundary would be additional work if required by the
threat model. Native dependency defects remain a residual risk, as with the
existing image decoder path. There is no source-upload path.

### Validation and a concrete issue caught by visual inspection

- `cargo check -p gander`: passed.
- `cargo test -p gander mermaid -- --nocapture`: 11 passed.
- `cargo test --workspace`: **686 passed, 10 ignored**, no failures. Includes the
  persistent-conversation scroll test with a Mermaid description.
- Tests cover exact matching/source preservation, tilde/backtick and nested
  fences, core families in light/dark, modern shapes, cycles/subgraphs/edge and
  HTML-style labels, supported Unicode, malformed/empty/unsupported input,
  missing glyph fallback, source/model/raster limits, cancellation/deadlines,
  hostile SVG/resources/configuration, cache failures/eviction, queue bounds,
  async unified/split/narrow comments, partial virtualization, source disclosure
  and clipboard contents, edits and PR switches.
- `cargo clippy -p gander --tests` completes with existing warnings elsewhere;
  the new Mermaid module has no Clippy warnings.
- Formatting passes on the changed renderer files and `git diff --check` passes.
  Whole-workspace `cargo fmt --all -- --check` still reports pre-existing
  formatting in `main.rs`; those unrelated changes were not retained.
- This goes beyond the initial SVG-parse-only probe: native resvg rasters were
  exported temporarily and visually inspected for the five core families,
  modern shapes, dark flowchart and CJK/accented Latin/Greek labels. An initial
  deterministic-measurement implementation **clipped class labels and displayed
  missing glyphs** despite parsing successfully. It was replaced with bounded
  native font measurement/painting and strict glyph coverage; the inspected
  corrected class and Unicode fixtures have legible, complete labels. This is
  why merely passing SVG parsing or a nonblank-pixel test is insufficient.
- Diagram painting tests use the real native Merman/usvg/resvg path, while app
  layout/interaction tests use GPUI's fake test platform. **No live Metal/native
  window acceptance or comparison against GitHub screenshots was performed.**
  Remaining acceptance gates: live app/DPI/keyboard/accessibility checks, broader
  real-PR compatibility/visual fixtures, and long-running memory/CPU profiling.
  Current tests and finite policies do not establish universal rendering parity
  or a complete security assurance.

## Original investigation

## Recommendation

Add a narrow block-level Markdown plugin backed by **Merman**, rendering locally
into SVG and displaying it through GPUI. Keep the original Mermaid source
available, and fall back to a normal code block on unsupported/invalid input.

This is feasible without replacing Kit's Markdown renderer, adding a webview,
requiring Node/Chromium, or sending private PR content to a rendering service.
The investigation did not change application code or dependencies.

Merman is the preferred candidate, not yet a production-qualified dependency:
its current release is alpha, the SVG output needs visual regression coverage,
and resource/security policy needs explicit configuration and testing.

## Existing integration points

- `crates/app/src/conversation/markdown.rs`: both transient and persistent
  Markdown use `render_view`, which already registers image/video plugins via
  `MarkdownExtensions`. Add `mod mermaid` and register a block plugin here.
- Kit's `MarkdownPlugin::parse` receives `markdown::mdast::Node` before the stock
  code-block conversion. Match only `Node::Code` whose `lang` is `mermaid`.
  Other languages, inline code, and diagram-looking prose should remain stock.
  Markdown AST parsing handles backtick/tilde fences and nested blocks; no
  separate regex-based fence scanner is needed.
- `MarkdownNode` supports typed `Send + Sync` data, plain text, original Markdown,
  and an accessibility label. Preserve `code.value` as text and
  `cx.node_source(node)` as Markdown so diagram rendering does not destroy copy
  or source access. Verify the selection behavior in integration tests.
- Parsers may run on a background thread and cannot use `Window`/`App`. Rendering
  has access to both. Do not perform diagram layout on every UI render.
- `conversation::preprocess` already preserves fenced code, so issue references,
  mentions, and emoji shortcodes inside Mermaid should not be expanded.
- `crates/app/src/diff_media.rs`: inline comment Markdown is measured offscreen
  using a separate `-measure` host, then rendered as uniform-list decorations.
  Actual child height changes update reservations through
  `on_children_prepainted`. Mermaid must share cached geometry between the two
  hosts and preserve that asynchronous resizing/anchoring behavior.
- `crates/app/src/image_viewer.rs`: useful future zoom/pan UI, but currently
  `ImageTarget` contains a URL, not an in-memory image. Reuse is not entirely
  automatic; avoid inventing fake HTTP URLs for generated diagrams.

GPUI already decodes SVG via `usvg`/`resvg`. An in-memory display path exists:
`img(Arc::new(Image::from_bytes(ImageFormat::Svg, svg.into_bytes())))`.
Keep the `Arc<Image>` cached rather than creating a new image each frame.
This is distinct from the monochrome icon-oriented `svg()` element.

## Renderer comparison

| Option | Assessment |
| --- | --- |
| `merman = 0.10.0-alpha.2` | Preferred native candidate. Strict diagnostics, modern node syntax, a `resvg_safe` SVG pipeline, cancellation/resource-policy APIs, and an existing Zed integration. Larger dependency surface and alpha API stability are costs. |
| `mermaid-rs-renderer = 0.3.1`, default features disabled | Small, easy SVG API and very fast on tiny diagrams, but the probe found silent misrendering and incomplete validation. Not suitable as an unrestricted replacement for GitHub Mermaid. |
| Official Mermaid JS / Mermaid CLI | Best reference implementation, but needs a browser DOM/layout runtime. CLI uses Puppeteer/Chromium; embedding brings runtime distribution, startup, lifecycle, and sandboxing work. Plain JS execution alone is not a drop-in substitute for the DOM. |
| Hosted rendering, e.g. Kroki or Mermaid services | Operationally simple, but discloses diagram source from potentially private PRs and adds network dependency. Do not use by default. |

Merman requires Rust 1.95; this repository pins 1.98.1. Zed's
`crates/mermaid_render` is a useful integration reference, not a library to import
wholesale: it includes substantial editor-specific theming/postprocessing.

## Should we replace the Markdown renderer instead?

Considered separately from choosing the diagram engine. **Not recommended for
Mermaid alone.** Markdown parsing, prose layout/selection, and Mermaid rendering
are distinct layers; replacing the first two does not remove the third.

| Replacement | Potential benefit | Cost / recommendation |
| --- | --- | --- |
| Another parser, e.g. `pulldown-cmark` or `comrak`, retaining GPUI prose rendering | Different GFM/extension support and parser performance | Does not implement Mermaid layout. Requires adapting AST/source mappings and existing plugins without a demonstrated Markdown parsing problem. No reason to switch for this feature. |
| Another native GPUI Markdown renderer, notably Zed's | Existing Mermaid integration and mature editor-oriented behaviors | Not a drop-in dependency: Zed's Markdown crate depends on its language, settings, theme, UI and utility crates and its GPUI stack. It is GPL-3.0-or-later, while this project is MIT; adopting it entails distribution/license considerations as well as technical migration. Use as an architectural reference rather than transplanting it. |
| Browser/webview renderer with GFM-to-HTML and official Mermaid JS | Best route to upstream Mermaid fidelity, browser HTML/CSS layout and native browser selection | Feasible, but adds a second rendering/input system, navigation/resource security policy, native/web media/auth bridging, height reporting and focus/scroll coordination. Many inline comment bodies make a webview-per-comment approach unattractive; a shared web surface changes more of the conversation/diff architecture. Worth considering only for broader browser-level content fidelity requirements. |
| Markdown renderer for another native toolkit (e.g. egui) | May bundle a diagram widget | Not a GPUI element; embedding the toolkit or porting its rendering/input/selection is a disproportionate change. Bundled diagrams may still use a non-upstream Mermaid implementation. |
| Custom native Markdown renderer | Complete control | Takes ownership of prose layout, selection/copy, accessibility, tables, code blocks, links and media. Highest ongoing maintenance; no identified need here. |

A replacement would have to preserve selectable prose, source/Markdown copy,
code highlighting, tables/lists/links, authenticated images and video hooks,
inline comment measurement/reservations and partial virtualization, as well as
persistent conversation documents and scroll retention. Existing tests in
`markdown_tests.rs` cover several of these contracts. A recent Kit migration is
also documented in `docs/gpui-kit-migration-validation.md`; its native acceptance
checks are still incomplete, making another broad migration especially hard to
justify without a concrete deficiency.

The strongest reason to revisit this decision would be **a hard requirement for
official Mermaid JS behavior across all diagram types**, combined with broader
GitHub-like HTML/CSS rendering needs or measured deficiencies in Kit selection,
layout or performance. Even then, prototype a diagram-only browser-backed
renderer first: whole-document Markdown replacement is not necessary just to
use the official Mermaid engine.

This comparison is an architectural/dependency assessment, not a benchmark or
completed migration prototype. The recommendation remains: retain Kit's
Markdown layer, keep the diagram backend isolated, and qualify the native
renderer against real PR fixtures before committing to it.

## Local probe results

An isolated temporary Cargo project tested both pinned native versions, using
`usvg = 0.46.0` (the version used by GPUI's current image path) with system fonts.
Merman enabled only `svg` and the flowchart, sequence, class, state, and ER
families, with `SvgPipeline::resvg_safe()` selected explicitly.

This verified generation and SVG parsing, **not** visual parity with GitHub,
actual GPUI painting, interaction, or a security audit.

| Input | `mermaid-rs-renderer` | Merman |
| --- | --- | --- |
| Small flowchart, sequence, class, state, ER | All produced SVG accepted by `usvg` | All produced SVG accepted by `usvg` |
| HTML-style labels (`<br/>`, `<b>`) | SVG accepted; no `foreignObject` | SVG accepted; no `foreignObject` with raster-safe pipeline |
| `flowchart LR\n A[unclosed --> B` | **Success with an empty 16×16 SVG** | Error: unterminated node label, with source span |
| Unknown diagram header | Error | Error |
| `A@{ shape: rect, label: "Start" } --> B` | **Diamond labeled `shape: rect, label: "Start"`** | SVG text labels `Start` and `B`; rectangle syntax handled |
| YAML frontmatter title/config | Diagram rendered, but title absent and requested dark theme not reflected | Title present and SVG accepted |

The malformed-input and new-shape failures are why merely falling back when
`mermaid-rs-renderer::render` returns `Err` would be insufficient.

Indicative timings on this machine, with an optimized development build:

- Merman: first small flowchart roughly 9–15 ms; subsequent small core diagrams
  roughly 1.5–6 ms; chains of 10/50/200 edges roughly 5/18/110 ms.
- `mermaid-rs-renderer`: first small flowchart roughly 6–13 ms; subsequent small
  core diagrams mostly below 1 ms; chains of 10/50/200 edges roughly
  20/70–94/160–176 ms.

These are single-run observations, not benchmarks. They exclude subsequent
GPUI SVG rasterization and establish that larger diagrams belong off the UI
thread. SVG parsing success alone does not establish correct labels/layout.

## Proposed implementation

1. Add `crates/app/src/conversation/markdown/mermaid.rs` implementing a block
   plugin. Register it centrally. Update the parser revision if needed for
   changed parse-time configuration; plugin identity also participates in
   Kit's extension comparison.
2. Put renderer execution behind a small interface so compatibility fixtures do
   not depend on UI mounting. Start with the five core families tested above,
   or explicitly test additional families before enabling them. Disable default
   features rather than automatically pulling every layout backend into the app.
3. Cache successes and failures by source, renderer/configuration revision, and
   theme/font inputs. Reuse entries across persistent documents, transient
   inline comments, and `-measure` hosts. Bound the cache by memory, including
   decoded raster images, not just entry count.
4. Run layout on a bounded background queue, deduplicating identical requests.
   Show readable source while pending. Notify on completion and ignore stale
   results after edits/PR switches. Reserve known intrinsic SVG geometry before
   image decoding completes, fitting uniformly to the prose width.
5. Use the raster-safe pipeline, then the cached GPUI image path. Offer a small
   source/copy control; diagram labels are not naturally selectable as prose.
   Zoom inspection can follow by generalizing the existing media viewer to
   accept generated image data. Avoid general-purpose diagram editing.
6. On parse errors, resource-limit failures, or unsupported capabilities, show
   the original fenced code rather than a blank card or partial diagram.

### Untrusted content and limits

PR descriptions and comments are untrusted. A native renderer avoids running
Mermaid JavaScript but is not automatically safe:

- Configure finite source, graph/layout, output-size, deadline, and concurrency
  limits. Background tasks alone do not cap CPU/memory; cooperative cancellation
  is not a hard process-isolation boundary.
- Keep configuration safety policy host-owned; diagram directives must not relax
  it. Use app theme/font defaults without trusting arbitrary external resources.
- Disallow external image/font resources and filesystem reads. Inspect the
  generated SVG and resolver configuration, not just the input diagram header.
  `usvg`'s default string-image resolver can read local paths. The GPUI convenience
  image decoder must not receive output capable of exercising that behavior.
- Merman's image-node probe emitted an `<image>` element (the tested `file://`
  source had no `href` in the raster-safe output). This is not evidence of file
  access, but illustrates why resource policy needs dedicated tests.
- No diagram-defined callbacks/navigation. Any future link activation should go
  through the application's existing safe URL handling.
- Limit raster dimensions/total pixels at display scale, including zoom. GPUI's
  image path rasterizes SVG; one fixed low-resolution bitmap will blur when
  enlarged, while blindly using intrinsic size can allocate enormous images.

## Acceptance checks before shipping

- Parser: multiple diagrams, backtick/tilde fences, list/quote nesting, Unicode,
  empty/invalid blocks, other language blocks untouched, exact source copying.
- Compatibility: reference fixtures against the Mermaid version used by GitHub,
  including edge labels, subgraphs, cycles, frontmatter/directives, modern
  shapes, multiline/HTML labels, and unsupported syntax.
- Visual: light/dark themes, fonts/Unicode, narrow split comments, window/DPI
  changes, diagram edges/text visible and not clipped.
- UI lifecycle: unified/split diff, partially virtualized tall comments, async
  reservation growth, persistent conversation scroll across pane switches,
  edits, PR switching, cache eviction, and no duplicate measurement/render jobs.
- Security/performance: oversized/pathological graphs, cancellation, bounded
  queue/cache/raster memory, external/local image resources, malicious SVG
  attributes/styles, no network requests and no diagram-provided navigation.

## References

- [Merman](https://github.com/Latias94/merman) and
  [resource policy](https://github.com/Latias94/merman/blob/main/docs/integration/RESOURCE_POLICY.md)
- [Merman SVG pipeline](https://github.com/Latias94/merman/blob/main/docs/rendering/SVG_OUTPUT_PIPELINE.md)
- [Zed Mermaid integration](https://github.com/zed-industries/zed/tree/main/crates/mermaid_render)
- [mermaid-rs-renderer](https://github.com/1jehuang/mermaid-rs-renderer)
- [Official Mermaid usage/security](https://mermaid.js.org/config/usage.html)
- [Official Mermaid CLI](https://github.com/mermaid-js/mermaid-cli)
