//! Review targets: where an item's diff comes from (PR or local tree)
//! and the blocking fetch that loads one.

use crate::threads::{group_comments, CommentIndex, COMMENT_WRAP_CHARS};
use crate::{build_rows, local_store, Row, ViewMode};
use anyhow::anyhow;
use diff_core::PrDiff;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

/// Where a review item's diff comes from.
#[derive(Clone)]
pub(crate) enum Source {
    Pr(github::PrLocator),
    Local(git::LocalSource),
}

/// Whether two sources describe the same thing to review: a PR by
/// owner/repo/number, a local diff by its working tree and base. The
/// session opens one item per source — [`ReviewApp::open_item`] brings an
/// existing item forward instead of duplicating it.
pub(crate) fn sources_match(a: &Source, b: &Source) -> bool {
    match (a, b) {
        (Source::Pr(x), Source::Pr(y)) => {
            x.owner == y.owner && x.repo == y.repo && x.number == y.number
        }
        (Source::Local(x), Source::Local(y)) => {
            x.repo_root == y.repo_root && x.base_ref == y.base_ref
        }
        _ => false,
    }
}

pub(crate) fn dir_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

pub(crate) enum LoadedMeta {
    Pr(github::PrMeta),
    Local(git::LocalSource),
}

pub(crate) struct Loaded {
    pub(crate) meta: LoadedMeta,
    pub(crate) diff: PrDiff,
    pub(crate) rows: Vec<Row>,
    pub(crate) file_rows: Vec<usize>,
    pub(crate) hunk_rows: Vec<usize>,
    pub(crate) mode: ViewMode,
    /// The file the rows were built for (file mode).
    pub(crate) file_ix: usize,
    /// Some (possibly empty) for PR items, None for local ones.
    pub(crate) comments: Option<CommentIndex>,
    /// The snapshot of a PR's fetch, for the local store: what the app
    /// saw of the PR, and when. None for local items.
    pub(crate) snapshot: Option<local_store::StoredPrSnapshot>,
}

/// Blocking fetch + parse + row building for one item; runs on the background
/// executor, so subprocess waits and tree-sitter work stay off the main thread.
/// PR items fetch meta, patch, and review comments concurrently — three
/// facade calls, one error vocabulary.
///
/// A PR's fetch also produces the snapshot the local store keeps: what the
/// app saw of the PR (its head SHA, its threads), so the next launch knows
/// what "since I last looked" means, and an offline launch can still read
/// the PR.
pub(crate) fn fetch_item(
    github: Arc<dyn github::GitHubClient>,
    source: &Source,
    mode: ViewMode,
    file_ix: usize,
) -> anyhow::Result<Loaded> {
    let (meta, patch, comments, snapshot) = match source {
        Source::Pr(loc) => {
            let meta_client = github.clone();
            let meta_loc = loc.clone();
            let meta_thread = std::thread::spawn(move || meta_client.pr_meta(&meta_loc));
            let comments_client = github.clone();
            let comments_loc = loc.clone();
            let comments_thread =
                std::thread::spawn(move || comments_client.review_comments(&comments_loc));
            let patch = github.pr_patch(loc)?;
            let meta = meta_thread
                .join()
                .map_err(|_| anyhow!("GitHub metadata fetch panicked"))??;
            let comments = comments_thread
                .join()
                .map_err(|_| anyhow!("GitHub comments fetch panicked"))??;
            let snapshot = pr_snapshot(loc, &meta, &comments);
            (
                LoadedMeta::Pr(meta),
                patch,
                Some(group_comments(comments)),
                Some(snapshot),
            )
        }
        Source::Local(src) => {
            let src = git::resolve_local_with_base(&src.repo_root, src.base_ref.as_deref())?;
            let patch = git::diff_patch(&src)?;
            (LoadedMeta::Local(src), patch, None, None)
        }
    };
    let diff = diff_core::parse_patch(&patch);
    let (rows, file_rows, hunk_rows) = build_rows(
        &diff,
        mode,
        &HashMap::new(),
        comments.as_ref(),
        true,
        COMMENT_WRAP_CHARS,
        file_ix,
    );
    Ok(Loaded {
        meta,
        diff,
        rows,
        file_rows,
        hunk_rows,
        mode,
        file_ix,
        comments,
        snapshot,
    })
}

/// The snapshot of a PR, as the local store keeps it: what the app saw
/// (its head SHA, its review threads) and when it saw it.
fn pr_snapshot(
    loc: &github::PrLocator,
    meta: &github::PrMeta,
    comments: &[github::ReviewComment],
) -> local_store::StoredPrSnapshot {
    // A thread is a root comment plus its replies: the root's id is the
    // thread's id, and the thread's timestamp is its last comment's.
    let mut threads: Vec<local_store::StoredThreadSeen> = Vec::new();
    let mut replies: HashMap<u64, u64> = HashMap::new();
    for comment in comments {
        if let Some(parent) = comment.in_reply_to_id {
            replies.insert(comment.id, parent);
        }
    }
    for comment in comments {
        if comment.in_reply_to_id.is_some() {
            continue; // a reply, not a thread root
        }
        // The thread's last comment: the newest reply under this root, or
        // the root itself.
        let mut updated_at = comment.created_at.clone();
        for other in comments {
            let mut root = other.in_reply_to_id;
            let mut guard = 0;
            while let Some(parent) = root {
                if parent == comment.id {
                    if other.created_at > updated_at {
                        updated_at = other.created_at.clone();
                    }
                    break;
                }
                root = replies.get(&parent).copied();
                guard += 1;
                if guard > 32 {
                    break; // a reply cycle can't happen, but don't spin
                }
            }
        }
        threads.push(local_store::StoredThreadSeen {
            id: comment.id.to_string(),
            updated_at,
            resolved: false,
        });
    }
    local_store::StoredPrSnapshot {
        repository: loc.repo_slug(),
        number: meta.number,
        title: meta.title.clone(),
        body: meta.body.clone(),
        author: meta.author.login.clone(),
        head_sha: meta.head_ref_oid.clone(),
        base_ref: meta.base_ref_name.clone(),
        head_ref: meta.head_ref_name.clone(),
        threads,
        is_draft: false,
        fetched_at_ms: local_store::now_ms(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One item per source: a PR is its owner/repo/number, a local diff
    /// is its working tree and base — anything else is a different thing
    /// to review and opens its own item.
    #[test]
    fn sources_match_on_identity_not_labels() {
        let pr = |owner: &str, number: u64| {
            Source::Pr(github::PrLocator {
                owner: owner.into(),
                repo: "widgets".into(),
                number,
            })
        };
        let local = |base: Option<&str>| {
            Source::Local(git::LocalSource {
                branch: "main".into(),
                base_ref: base.map(str::to_string),
                base_label: base.unwrap_or("…").into(),
                base_oid: None,
                repo_root: "/tmp/repo".into(),
            })
        };
        // Same PR, same local tree+base: the same thing.
        assert!(sources_match(&pr("acme", 1), &pr("acme", 1)));
        assert!(sources_match(&local(Some("main")), &local(Some("main"))));
        // Different number, different owner, different base: not the same.
        assert!(!sources_match(&pr("acme", 1), &pr("acme", 2)));
        assert!(!sources_match(&pr("acme", 1), &pr("beta", 1)));
        assert!(!sources_match(&local(Some("main")), &local(Some("dev"))));
        assert!(!sources_match(&local(Some("main")), &local(None)));
        // And never across kinds.
        assert!(!sources_match(&pr("acme", 1), &local(None)));
    }
}
