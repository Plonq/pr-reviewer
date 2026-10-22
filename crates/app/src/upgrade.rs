//! Phase-2 full-content upgrade: background refetch and re-diff of whole files.

use crate::FileUpgrade;
use diff_core::{diff_texts, DiffRow, FileStatus, Hunk};
use gpui::SharedString;

/// Never upgrade more than this many files per item; the rest stay on the
/// perfectly serviceable patch-derived view.
const MAX_UPGRADE_FILES: usize = 400;
/// Per-side blob cap; the facade's `file_at` enforces the same limit for
/// PRs, so this guards only the local working-tree read.
const MAX_UPGRADE_BLOB_BYTES: usize = 1024 * 1024;
/// gh/git are one subprocess per call, so a small worker pool is plenty.
const UPGRADE_WORKERS: usize = 4;

/// Where to fetch full file contents from, snapshotted when an item loads.
pub(crate) enum UpgradeSource {
    Pr {
        loc: github::PrLocator,
        base_oid: String,
        head_oid: String,
    },
    Local(git::LocalSource),
}

/// One file's fetch inputs, snapshotted from the parsed patch (paths and
/// statuses are already known — no extra API call needed).
pub(crate) struct UpgradeJob {
    pub(crate) file_ix: usize,
    pub(crate) old_path: Option<String>,
    pub(crate) new_path: Option<String>,
    pub(crate) status: FileStatus,
}

/// One file's completed upgrade: authoritative hunks plus the render state.
pub(crate) struct UpgradedFile {
    pub(crate) file_ix: usize,
    pub(crate) hunks: Vec<Hunk>,
    pub(crate) additions: u32,
    pub(crate) deletions: u32,
    pub(crate) upgrade: FileUpgrade,
}

/// Blocking fetch + re-diff + whole-file highlight for every eligible file;
/// runs on the background executor. Files that fail on either side are simply
/// missing from the result and keep their patch-derived view.
pub(crate) fn run_upgrade(
    github: &dyn github::GitHubClient,
    source: &UpgradeSource,
    mut jobs: Vec<UpgradeJob>,
) -> Vec<UpgradedFile> {
    if jobs.len() > MAX_UPGRADE_FILES {
        eprintln!(
            "gander: {} changed files; upgrading the first {MAX_UPGRADE_FILES} to full \
             contents, the rest stay patch-derived",
            jobs.len()
        );
        jobs.truncate(MAX_UPGRADE_FILES);
    }
    let queue = std::sync::Mutex::new(jobs.into_iter());
    let done = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..UPGRADE_WORKERS {
            scope.spawn(|| loop {
                let Some(job) = queue.lock().unwrap().next() else {
                    break;
                };
                if let Some(result) = upgrade_file(github, source, &job) {
                    done.lock().unwrap().push(result);
                }
            });
        }
    });
    let mut done = done.into_inner().unwrap();
    done.sort_by_key(|file| file.file_ix);
    done
}

/// One side's full contents, or None to leave the file un-upgraded: absent,
/// binary/non-UTF-8, over the size cap, or a fetch failure.
fn fetch_side(
    github: &dyn github::GitHubClient,
    source: &UpgradeSource,
    path: &str,
    old: bool,
) -> Option<String> {
    let text = match source {
        UpgradeSource::Pr {
            loc,
            base_oid,
            head_oid,
        } => {
            let oid = if old { base_oid } else { head_oid };
            match github.file_at(loc, oid, path) {
                Ok(text) => text?,
                Err(err) => {
                    eprintln!("gander: {path}: {err}");
                    return None;
                }
            }
        }
        UpgradeSource::Local(src) => {
            if old {
                git::file_at_base(src, path)?
            } else {
                String::from_utf8(std::fs::read(src.repo_root.join(path)).ok()?).ok()?
            }
        }
    };
    (text.len() <= MAX_UPGRADE_BLOB_BYTES).then_some(text)
}

fn upgrade_file(
    github: &dyn github::GitHubClient,
    source: &UpgradeSource,
    job: &UpgradeJob,
) -> Option<UpgradedFile> {
    // Presence must match the patch's story: Added has no old side, Deleted no
    // new side, everything else needs both. A wanted-but-missing side (404,
    // binary, huge) keeps the whole file patch-derived.
    let old_text = match (job.status, &job.old_path) {
        (FileStatus::Added, _) => String::new(),
        (_, Some(path)) => fetch_side(github, source, path, true)?,
        (_, None) => return None,
    };
    let new_text = match (job.status, &job.new_path) {
        (FileStatus::Deleted, _) => String::new(),
        (_, Some(path)) => fetch_side(github, source, path, false)?,
        (_, None) => return None,
    };
    // Normalize CRLF→LF up front: hunks, span tables, and gap rows must all
    // index the same line/byte space (diff_texts itself is ending-agnostic).
    let normalize = |text: String| {
        if text.contains('\r') {
            text.replace("\r\n", "\n")
        } else {
            text
        }
    };
    let old_text = normalize(old_text);
    let new_text = normalize(new_text);

    let hunks = diff_texts(&old_text, &new_text, 3);
    let (additions, deletions) =
        hunks
            .iter()
            .flat_map(|hunk| &hunk.rows)
            .fold((0, 0), |(a, d), row| match row {
                DiffRow::Added { .. } => (a + 1, d),
                DiffRow::Removed { .. } => (a, d + 1),
                DiffRow::Context { .. } => (a, d),
            });

    let path = job.new_path.as_deref().or(job.old_path.as_deref())?;
    let lang = syntax::language_for_path(path);
    let spans = |text: &str| match lang {
        Some(lang) if !text.is_empty() => syntax::highlight_lines(lang, text),
        _ => Vec::new(),
    };
    Some(UpgradedFile {
        file_ix: job.file_ix,
        hunks,
        additions,
        deletions,
        upgrade: FileUpgrade {
            old_spans: spans(&old_text),
            new_spans: spans(&new_text),
            new_lines: new_text
                .lines()
                .map(|line| SharedString::from(line.to_string()))
                .collect(),
        },
    })
}
