//! GitHub comment attachments. Only explicit paste/drop gestures upload files;
//! URLs in ordinary text are never fetched or uploaded.

use anyhow::{bail, Context as _, Result};
use gpui::component::input::EditorState;
use gpui::{
    App, AppContext as _, ClipboardEntry, ClipboardItem, Context, Entity, EntityInputHandler as _,
    Window,
};
use http_client::Url;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::Duration;

const IMAGE_LIMIT: u64 = 10 * 1024 * 1024;
const VIDEO_LIMIT: u64 = 100 * 1024 * 1024;
const MARKER_PREFIX: &str = "[Uploading attachment #";
static NEXT_UPLOAD: AtomicU64 = AtomicU64::new(1);

type Uploader = Arc<dyn Fn(&github::PrLocator, Source) -> Result<String> + Send + Sync>;

#[derive(Clone)]
pub(crate) enum Source {
    File(PathBuf),
    Image(gpui::Image),
}

impl Source {
    fn name(&self) -> String {
        match self {
            Self::File(path) => path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            Self::Image(image) => format!("image.{}", image.format.extension()),
        }
    }
}

/// Owned by one composer, never shared between drafts or PRs.
pub(crate) struct Uploads {
    loc: github::PrLocator,
    pending: usize,
    pub(crate) error: Option<String>,
    upload: Uploader,
}

impl Uploads {
    pub(crate) fn new<T: 'static>(
        loc: github::PrLocator,
        input: &Entity<EditorState>,
        cx: &mut Context<T>,
    ) -> Entity<Self> {
        let uploads = cx.new(|_| Self {
            loc,
            pending: 0,
            error: None,
            upload: Arc::new(upload),
        });
        cx.observe(&uploads, |_, _, cx| cx.notify()).detach();
        // Undo or manual edits can restore/remove placeholders without changing
        // the field's height. Keep submit buttons and the explanation in sync.
        cx.observe(input, |_, _, cx| cx.notify()).detach();
        uploads
    }

    pub(crate) fn pending(&self) -> usize {
        self.pending
    }
}

pub(crate) fn blocks_submission(
    uploads: Option<&Entity<Uploads>>,
    input: &Entity<EditorState>,
    cx: &App,
) -> bool {
    uploads.is_some_and(|uploads| uploads.read(cx).pending > 0)
        // Undo may restore an already replaced placeholder. Never publish it.
        || input.read(cx).value().contains(MARKER_PREFIX)
}

pub(crate) fn clipboard_sources(item: &ClipboardItem) -> Vec<Source> {
    // Finder may offer both file URLs and an image representation. Prefer the
    // original files, rather than uploading two representations of one item.
    let files: Vec<_> = item
        .entries()
        .iter()
        .flat_map(|entry| match entry {
            ClipboardEntry::ExternalPaths(paths) => {
                paths.0.iter().cloned().map(Source::File).collect()
            }
            _ => Vec::new(),
        })
        .collect();
    if !files.is_empty() {
        return files;
    }
    item.entries()
        .iter()
        .filter_map(|entry| match entry {
            ClipboardEntry::Image(image) => Some(Source::Image(image.clone())),
            _ => None,
        })
        .collect()
}

pub(crate) fn start(
    uploads: &Entity<Uploads>,
    input: &Entity<EditorState>,
    sources: Vec<Source>,
    window: &mut Window,
    cx: &mut App,
) {
    if sources.is_empty() {
        return;
    }
    let jobs: Vec<_> = sources
        .into_iter()
        .map(|source| {
            let marker = format!(
                "{MARKER_PREFIX}{}…]",
                NEXT_UPLOAD.fetch_add(1, Ordering::Relaxed)
            );
            (marker, source)
        })
        .collect();
    let placeholders = jobs
        .iter()
        .map(|(marker, _)| marker.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    input.update(cx, |input, cx| {
        input.focus(window, cx);
        let value = input.value();
        let selection = input.selected_range();
        let prefix = if selection.start > 0 && !value[..selection.start].ends_with('\n') {
            "\n"
        } else {
            ""
        };
        let reuse_newline = value[selection.end..].starts_with('\n');
        let suffix = if reuse_newline { "" } else { "\n" };
        input.replace(format!("{prefix}{placeholders}{suffix}"), window, cx);
        if reuse_newline {
            // Leave the cursor on the following line even when that newline
            // already existed, so subsequent typing stays off the attachment.
            let cursor = input.cursor() + 1;
            input.set_selected_range(cursor..cursor, cx);
        }
    });
    let input = input.downgrade();
    uploads.update(cx, |uploads, cx| {
        uploads.pending += jobs.len();
        uploads.error = None;
        cx.notify();
        let loc = uploads.loc.clone();
        let upload = uploads.upload.clone();
        cx.spawn_in(window, async move |this, cx| {
            let results = cx
                .background_spawn(async move {
                    jobs.into_iter()
                        .map(|(marker, source)| {
                            let name = source.name();
                            (
                                marker,
                                upload(&loc, source)
                                    .with_context(|| format!("Could not upload {name}")),
                            )
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            // Both weak entities belong to the original draft. Closing it or
            // switching PRs cannot insert a late result into another composer.
            let _ = cx.update(|window, cx| {
                let _ = this.update(cx, |uploads, cx| {
                    for (marker, result) in results {
                        uploads.pending -= 1;
                        let replacement = match result {
                            Ok(markdown) => markdown,
                            Err(error) => {
                                let error =
                                    format!("{error:#}. Paste or drop the file again to retry.");
                                if let Some(previous) = &mut uploads.error {
                                    previous.push('\n');
                                    previous.push_str(&error);
                                } else {
                                    uploads.error = Some(error);
                                }
                                String::new()
                            }
                        };
                        let _ = input.update(cx, |input, cx| {
                            replace_marker(input, &marker, &replacement, window, cx)
                        });
                    }
                    cx.notify();
                });
            });
        })
        .detach();
    });
}

fn replace_marker(
    input: &mut EditorState,
    marker: &str,
    replacement: &str,
    window: &mut Window,
    cx: &mut Context<EditorState>,
) {
    let value = input.value();
    let Some(start) = value.find(marker) else {
        return;
    };
    // GPUI's input-handler ranges are UTF-16, not UTF-8 byte offsets.
    let start_utf16 = value[..start].encode_utf16().count();
    let range = start_utf16..start_utf16 + marker.encode_utf16().count();
    let selection = input.selected_range();
    let end = start + marker.len();
    let remap = |offset: usize| {
        if offset <= start {
            offset
        } else if offset >= end {
            offset - marker.len() + replacement.len()
        } else {
            start + replacement.len()
        }
    };
    input.replace_text_in_range(Some(range), replacement, window, cx);
    input.set_selected_range(remap(selection.start)..remap(selection.end), cx);
}

fn content_type(name: &str) -> Result<&'static str> {
    let extension = Path::new(name)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    Ok(match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        _ => bail!("Unsupported media type. Use PNG, JPEG, GIF, WebP, SVG, MP4, MOV, or WebM"),
    })
}

fn validate_size(size: u64, mime: &str) -> Result<()> {
    let limit = if mime.starts_with("video/") {
        VIDEO_LIMIT
    } else {
        IMAGE_LIMIT
    };
    if size == 0 {
        bail!("The file is empty");
    }
    if size > limit {
        bail!(
            "The file exceeds GitHub's {} MB upload limit",
            limit / 1024 / 1024
        );
    }
    Ok(())
}

fn gh(args: &[&str]) -> Result<String> {
    let output = Command::new("gh")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("Could not run GitHub CLI")?;
    if !output.status.success() {
        bail!(
            "GitHub CLI: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

/// Matches gh's attachment upload protocol. This endpoint is not a versioned
/// public REST API: keep failures visible, and never fall back to public gists
/// or release assets (especially for private-repository media).
fn upload(loc: &github::PrLocator, source: Source) -> Result<String> {
    let name = source.name();
    let mime = content_type(&name)?;
    let bytes = match source {
        Source::File(path) => {
            let metadata = std::fs::metadata(&path).context("Cannot read the file")?;
            if !metadata.is_file() {
                bail!("Not a regular file");
            }
            validate_size(metadata.len(), mime)?;
            let file = std::fs::File::open(&path).context("Cannot open the file")?;
            let metadata = file.metadata()?;
            if !metadata.is_file() {
                bail!("Not a regular file");
            }
            validate_size(metadata.len(), mime)?;
            let mut bytes = Vec::new();
            // Bound reads even if the file grows after metadata was checked.
            file.take(VIDEO_LIMIT + 1).read_to_end(&mut bytes)?;
            bytes
        }
        Source::Image(image) => image.bytes,
    };
    validate_size(bytes.len() as u64, mime)?;
    let repo = format!("repos/{}/{}", loc.owner, loc.repo);
    let repository_id: u64 = gh(&["api", "--hostname", "github.com", &repo, "--jq", ".id"])?
        .parse()
        .context("GitHub returned no repository ID")?;
    let token = gh(&["auth", "token", "--hostname", "github.com"])?;
    if token.is_empty() {
        bail!("Sign in with gh auth login first");
    }
    let url = upload_url(&name, mime, repository_id);
    let body = send_upload(&url, &token, &bytes)?;
    attachment_markdown(&name, mime, &body)
}

fn send_upload(url: &Url, token: &str, bytes: &[u8]) -> Result<String> {
    let agent = ureq::AgentBuilder::new()
        .redirects(0)
        .timeout(Duration::from_secs(180))
        .try_proxy_from_env(true)
        .build();
    let response = agent.post(url.as_str())
        .set("Authorization", &format!("Bearer {token}"))
        .set("Accept", "application/vnd.github+json")
        .set("Content-Type", "application/octet-stream")
        .send_bytes(bytes).map_err(|error| match error {
            ureq::Error::Status(403 | 404, _) => anyhow::anyhow!("GitHub refused the upload. Attachments require repository write access and a supported gh credential"),
            ureq::Error::Status(status, _) => anyhow::anyhow!("GitHub attachment upload failed (HTTP {status})"),
            ureq::Error::Transport(error) => anyhow::anyhow!("Attachment upload failed: {error}"),
        })?;
    if response.status() != 201 && response.status() != 200 {
        bail!("GitHub returned an unexpected upload response");
    }
    let mut body = String::new();
    response
        .into_reader()
        .take(64 * 1024)
        .read_to_string(&mut body)?;
    Ok(body)
}

fn upload_url(name: &str, mime: &str, repository_id: u64) -> Url {
    let mut url = Url::parse("https://uploads.github.com/user-attachments/assets").unwrap();
    url.query_pairs_mut()
        .append_pair("name", name)
        .append_pair("content_type", mime)
        .append_pair("repository_id", &repository_id.to_string());
    url
}

fn attachment_markdown(name: &str, mime: &str, body: &str) -> Result<String> {
    let response: serde_json::Value =
        serde_json::from_str(body).context("Invalid attachment upload response")?;
    let url = response["url"]
        .as_str()
        .context("GitHub returned no attachment URL")?;
    let parsed = Url::parse(url).context("Invalid attachment URL")?;
    if parsed.scheme() != "https"
        || parsed.host_str() != Some("github.com")
        || !parsed.path().starts_with("/user-attachments/assets/")
        || parsed.username() != ""
        || parsed.password().is_some()
        || parsed.port().is_some()
        || url
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '(' | ')' | '<' | '>'))
    {
        bail!("GitHub returned an unexpected attachment URL");
    }
    if mime.starts_with("video/") {
        return Ok(url.to_owned());
    }
    let alt = name
        .replace('\\', "\\\\")
        .replace('[', "\\[")
        .replace(']', "\\]")
        .replace(['\n', '\r'], " ");
    Ok(format!("![{alt}]({url})"))
}

#[cfg(test)]
#[path = "comment_media_tests.rs"]
mod tests;
