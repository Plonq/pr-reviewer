//! File media comparisons, separate from Markdown attachments and video playback.
//! Raster images use GPUI's loader; SVG rasterization tracks display size.
//! Everything else is a link.
use std::{collections::HashMap, io::Write, path::Path};

use diff_core::{FileDiff, FileStatus, PrDiff};
use gpui::{
    component::{
        button::{Button, ButtonVariants as _},
        Selectable as _, Sizable as _,
    },
    div, img,
    prelude::*,
    px, Context, ImageSource, ObjectFit, Resource, StyledImage, Window,
};

use crate::{review_target::LoadedMeta, theme, ItemData, ReviewApp, Source};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Image,
    Other,
}

pub(crate) fn kind(path: &str) -> Option<Kind> {
    let ext = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "svg" | "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "tif" | "tiff"
        | "pnm" | "pbm" | "pgm" | "ppm" | "pam" | "tga" | "dds" | "hdr" | "exr" | "ff"
        | "farbfeld" | "qoi" => Some(Kind::Image),
        "mp4" | "m4v" | "mov" | "webm" | "avi" | "mkv" | "mpeg" | "mpg" | "ogv" | "mp3" | "m4a"
        | "wav" | "ogg" | "flac" | "aac" | "aiff" | "pdf" | "avif" | "heic" | "heif" => {
            Some(Kind::Other)
        }
        _ => None,
    }
}

pub(crate) fn file_kind(file: &FileDiff) -> Option<Kind> {
    kind(file.display_path())
        .or_else(|| file.old_path.as_deref().and_then(kind))
        .or((file.status == FileStatus::Binary).then_some(Kind::Other))
}

fn is_svg(path: &str) -> bool {
    Path::new(path)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("svg"))
}

/// Only offer source for text SVG comparisons, never a rename across binary
/// formats (e.g. SVG to PNG) or a file Git explicitly classified as binary.
pub(crate) fn supports_code(file: &FileDiff) -> bool {
    file.status != FileStatus::Binary
        && file
            .old_path
            .iter()
            .chain(&file.new_path)
            .any(|path| is_svg(path))
        && file
            .old_path
            .iter()
            .chain(&file.new_path)
            .all(|path| is_svg(path))
}

impl ItemData {
    pub(crate) fn shows_file_media(&self) -> bool {
        self.diff.files.get(self.file_ix).is_some_and(|file| {
            file_kind(file).is_some()
                && !(supports_code(file) && self.svg_code.contains(file.display_path()))
        })
    }
}

impl ReviewApp {
    fn set_svg_code(&mut self, code: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(data) = self.active_data_mut() else {
            return;
        };
        let Some(file) = data
            .diff
            .files
            .get(data.file_ix)
            .filter(|file| supports_code(file))
        else {
            return;
        };
        let path = file.display_path().to_owned();
        if code {
            data.svg_code.insert(path);
        } else {
            data.svg_code.remove(&path);
        }
        // Rows and the code viewport remain intact while the preview is shown.
        data.selection = None;
        data.cursor_side = None;
        self.drag_anchor = None;
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }
}

pub(crate) fn render_svg_controls(
    data: &ItemData,
    cx: &mut Context<ReviewApp>,
) -> Option<gpui::AnyElement> {
    let file = data
        .diff
        .files
        .get(data.file_ix)
        .filter(|file| supports_code(file))?;
    let code = data.svg_code.contains(file.display_path());
    Some(
        div()
            .h(px(30.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .bg(theme::mantle(cx))
            .border_b_1()
            .border_color(theme::surface0(cx))
            .children(
                [
                    (false, "svg-rendered", "Rendered"),
                    (true, "svg-code", "Code diff"),
                ]
                .map(|(value, id, label)| {
                    div().debug_selector(move || id.to_owned()).child(
                        Button::new(id)
                            .ghost()
                            .small()
                            .label(label)
                            .selected(code == value)
                            .on_click(cx.listener(move |app, _, window, cx| {
                                app.set_svg_code(value, window, cx)
                            })),
                    )
                }),
            )
            .into_any_element(),
    )
}

pub(crate) struct Side {
    image: Option<Resource>,
    link: String,
    // Local base and virtual comparison files must outlive their image loads
    // and external links. Dropped automatically when the review is refreshed.
    _snapshot: Option<tempfile::NamedTempFile>,
}

pub(crate) struct MediaFile {
    old: Option<Side>,
    new: Option<Side>,
}

fn encode_path(path: &str) -> String {
    path.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

fn remote(slug: &str, oid: &str, path: &str, preview: bool) -> Side {
    let path = encode_path(path);
    Side {
        image: preview.then(|| {
            Resource::Uri(format!("https://raw.githubusercontent.com/{slug}/{oid}/{path}").into())
        }),
        link: format!("https://github.com/{slug}/blob/{oid}/{path}"),
        _snapshot: None,
    }
}

fn local(path: &Path, preview: bool) -> Option<Side> {
    Some(Side {
        image: preview.then(|| Resource::from(path.to_path_buf())),
        link: http_client::Url::from_file_path(path).ok()?.to_string(),
        _snapshot: None,
    })
}

fn snapshot(path: &str, bytes: &[u8], preview: bool) -> Option<Side> {
    let suffix = Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("bin");
    let mut file = tempfile::Builder::new()
        .prefix("gander-")
        .suffix(&format!(".{suffix}"))
        .tempfile()
        .ok()?;
    file.write_all(bytes).ok()?;
    let mut side = local(file.path(), preview)?;
    side._snapshot = Some(file);
    Some(side)
}

/// Run on the fetch worker, never during rendering. PR images load lazily via
/// the authenticated/cached HTTP bridge, using immutable comparison revisions.
pub(crate) fn prepare(
    diff: &PrDiff,
    source: &Source,
    meta: &LoadedMeta,
    base: Option<&str>,
    virtual_base: Option<&HashMap<String, github::ComparisonBase>>,
) -> HashMap<usize, MediaFile> {
    diff.files
        .iter()
        .enumerate()
        .filter_map(|(ix, file)| {
            file_kind(file)?;
            let side = |path: &str, old: bool| -> Option<Side> {
                let preview = kind(path) == Some(Kind::Image);
                match (source, meta) {
                    (Source::Pr(loc), LoadedMeta::Pr(meta)) => {
                        if old {
                            if let Some(files) = virtual_base {
                                // Never silently substitute the reviewed blob for
                                // an upstream-adjusted virtual comparison side.
                                return snapshot(path, files.get(path)?.text.as_bytes(), preview);
                            }
                        }
                        let oid = if old { base? } else { &meta.head_ref_oid };
                        (!oid.is_empty()).then(|| remote(&loc.repo_slug(), oid, path, preview))
                    }
                    (_, LoadedMeta::Local(src)) => {
                        if old {
                            let mut file = tempfile::Builder::new()
                                .prefix("gander-base-")
                                .suffix(&format!(
                                    ".{}",
                                    Path::new(path)
                                        .extension()
                                        .and_then(|ext| ext.to_str())
                                        .unwrap_or("bin")
                                ))
                                .tempfile()
                                .ok()?;
                            git::copy_file_at_base(src, path, file.as_file_mut()).ok()?;
                            let mut side = local(file.path(), preview)?;
                            side._snapshot = Some(file);
                            Some(side)
                        } else {
                            local(&src.repo_root.join(path), preview)
                        }
                    }
                    _ => None,
                }
            };
            Some((
                ix,
                MediaFile {
                    old: file.old_path.as_deref().and_then(|path| side(path, true)),
                    new: file.new_path.as_deref().and_then(|path| side(path, false)),
                },
            ))
        })
        .collect()
}

fn render_side(
    label: &'static str,
    path: Option<&str>,
    side: Option<&Side>,
    cx: &mut Context<ReviewApp>,
) -> gpui::AnyElement {
    div()
        .id(label)
        .debug_selector(move || format!("file-media-{label}"))
        .flex_1()
        .min_w_0()
        .h_full()
        .flex()
        .flex_col()
        .gap_3()
        .p_3()
        .child(div().text_color(theme::subtext(cx)).child(label))
        .child(match side {
            Some(side) => {
                let link = side.link.clone();
                div()
                    .id("open-file")
                    .debug_selector(move || format!("file-media-{label}-link"))
                    .cursor_pointer()
                    .text_color(theme::blue(cx))
                    .child("Open file ↗")
                    .on_click(move |_, _, cx| cx.open_url(&link))
                    .into_any_element()
            }
            None => div()
                .text_color(theme::overlay0(cx))
                .child(if path.is_none() {
                    "Not present"
                } else {
                    "File unavailable"
                })
                .into_any_element(),
        })
        .child(
            div()
                .flex_1()
                .min_h_0()
                .w_full()
                .debug_selector(move || format!("file-media-{label}-viewport"))
                .overflow_hidden()
                .children(side.and_then(|side| side.image.clone()).map(|source| {
                    if path.is_some_and(is_svg) {
                        return crate::svg_image::svg_image(source, label);
                    }
                    img(ImageSource::Resource(source))
                        .id("image")
                        .size_full()
                        .object_fit(ObjectFit::Contain)
                        .with_loading(move || {
                            div()
                                .debug_selector(move || format!("file-media-{label}-loading"))
                                .child("Loading image…")
                                .into_any_element()
                        })
                        .with_fallback(move || {
                            div()
                                .debug_selector(move || format!("file-media-{label}-error"))
                                .child("Could not load image. Use Open file above.")
                                .into_any_element()
                        })
                        .into_any_element()
                })),
        )
        .into_any_element()
}

pub(crate) fn render(
    file: &FileDiff,
    media: Option<&MediaFile>,
    cx: &mut Context<ReviewApp>,
) -> gpui::AnyElement {
    div()
        .w_full()
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .child(div().px_3().py_2().child(file.display_path().to_owned()))
        .child(
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .child(render_side(
                    "Before",
                    file.old_path.as_deref(),
                    media.and_then(|m| m.old.as_ref()),
                    cx,
                ))
                .child(div().w(px(1.)).h_full().bg(theme::overlay0(cx)))
                .child(render_side(
                    "After",
                    file.new_path.as_deref(),
                    media.and_then(|m| m.new.as_ref()),
                    cx,
                )),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_include_svg_but_video_and_other_media_are_link_only() {
        for path in ["a.SVG", "a.png", "a.JPG", "a.webp", "a.gif"] {
            assert_eq!(kind(path), Some(Kind::Image));
        }
        for path in ["a.mp4", "a.MOV", "a.webm", "a.mp3", "a.pdf", "a.heic"] {
            assert_eq!(kind(path), Some(Kind::Other));
        }
        assert_eq!(kind("a.rs"), None);
    }

    #[test]
    fn source_toggle_is_only_for_text_svg_comparisons() {
        for (patch, expected) in [
            ("diff --git a/a.SVG b/b.svg\nsimilarity index 100%\nrename from a.SVG\nrename to b.svg\n", true),
            ("diff --git a/a.svg b/a.svg\nnew file mode 100644\n--- /dev/null\n+++ b/a.svg\n@@ -0,0 +1 @@\n+<svg/>\n", true),
            ("diff --git a/a.svg b/a.svg\ndeleted file mode 100644\n--- a/a.svg\n+++ /dev/null\n@@ -1 +0,0 @@\n-<svg/>\n", true),
            ("diff --git a/a.svg b/a.svg\nBinary files a/a.svg and b/a.svg differ\n", false),
            ("diff --git a/a.svg b/a.png\nsimilarity index 100%\nrename from a.svg\nrename to a.png\n", false),
            ("diff --git a/a.png b/a.svg\nsimilarity index 100%\nrename from a.png\nrename to a.svg\n", false),
            ("diff --git a/a.mp4 b/a.mp4\nBinary files a/a.mp4 and b/a.mp4 differ\n", false),
            ("diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old\n+new\n", false),
        ] {
            let diff = diff_core::parse_patch(patch);
            assert_eq!(supports_code(&diff.files[0]), expected, "{patch}");
        }
    }

    #[test]
    fn local_media_uses_binary_safe_base_snapshots_and_links_without_decoding_video() {
        let repo = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-b", "main"]);
        run(&["config", "user.name", "Test"]);
        run(&["config", "user.email", "test@example.com"]);
        std::fs::write(repo.path().join("a.svg"), "<svg>old</svg>").unwrap();
        std::fs::write(repo.path().join("movie.mp4"), [0, 255, 1]).unwrap();
        std::fs::write(repo.path().join("binary"), [0, 255, 1]).unwrap();
        run(&["add", "."]);
        run(&["-c", "commit.gpgsign=false", "commit", "-m", "base"]);
        let src = git::resolve_local_with_base(repo.path(), Some("HEAD")).unwrap();
        std::fs::write(repo.path().join("a.svg"), "<svg>new</svg>").unwrap();
        std::fs::write(repo.path().join("movie.mp4"), [0, 254, 2]).unwrap();
        std::fs::remove_file(repo.path().join("binary")).unwrap();
        let diff = diff_core::parse_patch(&git::diff_patch(&src).unwrap());
        let media = prepare(
            &diff,
            &Source::Local(src.clone()),
            &LoadedMeta::Local(src),
            None,
            None,
        );
        let get = |path: &str| {
            &media[&diff
                .files
                .iter()
                .position(|file| file.display_path() == path)
                .unwrap()]
        };
        let svg = get("a.svg");
        let base_file = svg
            .old
            .as_ref()
            .unwrap()
            ._snapshot
            .as_ref()
            .unwrap()
            .path()
            .to_owned();
        assert_eq!(std::fs::read(&base_file).unwrap(), b"<svg>old</svg>");
        assert!(svg.old.as_ref().unwrap().image.is_some());
        assert!(svg.new.as_ref().unwrap().image.is_some());
        let video = get("movie.mp4");
        assert!(video.old.as_ref().unwrap().image.is_none());
        assert!(video.new.as_ref().unwrap().image.is_none());
        assert_eq!(
            std::fs::read(
                video
                    .old
                    .as_ref()
                    .unwrap()
                    ._snapshot
                    .as_ref()
                    .unwrap()
                    .path()
            )
            .unwrap(),
            [0, 255, 1]
        );
        let binary = get("binary");
        assert!(binary.old.is_some());
        assert!(binary.new.is_none());
        drop(media);
        assert!(
            !base_file.exists(),
            "temporary snapshots belong to the loaded review"
        );
    }

    #[test]
    fn remote_paths_are_encoded_and_links_use_the_comparison_commit() {
        let side = remote("owner/repo", "commit", "dir/a #é.svg", true);
        assert_eq!(
            side.link,
            "https://github.com/owner/repo/blob/commit/dir/a%20%23%C3%A9.svg"
        );
        assert!(side.image.is_some());
        assert!(remote("owner/repo", "commit", "movie.mp4", false)
            .image
            .is_none());
    }
}
