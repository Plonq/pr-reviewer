//! Review targets: where an item's diff comes from (PR or local tree)
//! and the blocking fetch that loads one.

use crate::local_store;
use crate::rows::{build_rows, BuiltRows, FileUpgrade, RowOptions, RowsView, ViewMode};
use crate::threads::{group_comments, CommentIndex, ReviewComment, COMMENT_WRAP_CHARS};
use crate::upgrade::{run_upgrade, UpgradeJob, UpgradeSource};
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

/// Enough data to read the conversation before the patch and file details land.
pub(crate) struct LoadedConversation {
    pub(crate) meta: github::PrMeta,
    pub(crate) discussion: github::PrDiscussion,
}

pub(crate) struct Loaded {
    pub(crate) meta: LoadedMeta,
    pub(crate) diff: PrDiff,
    /// Original patch coordinates, before re-diffing full file contents.
    pub(crate) anchor_hunks: Vec<Vec<diff_core::Hunk>>,
    pub(crate) upgrades: HashMap<usize, FileUpgrade>,
    pub(crate) built_rows: BuiltRows,
    pub(crate) mode: ViewMode,
    /// The file the rows were built for (file mode).
    pub(crate) file_ix: usize,
    /// Some (possibly empty) for PR items, None for local ones.
    pub(crate) comments: Option<CommentIndex>,
    pub(crate) discussion: Option<github::PrDiscussion>,
    pub(crate) commits: Option<github::PrCommits>,
    pub(crate) review_changes: github::ReviewChanges,
    /// Active incremental diff's old revision; None means the full PR diff.
    pub(crate) review_base: Option<String>,
    pub(crate) review_base_files: Option<Arc<HashMap<String, github::ComparisonBase>>>,
    pub(crate) viewed: Option<Result<github::ViewedFiles, github::ClientError>>,
    /// The snapshot of a PR's fetch, for the local store: what the app
    /// saw of the PR, and when. None for local items.
    pub(crate) snapshot: Option<local_store::StoredPrSnapshot>,
}

#[cfg(test)]
pub(crate) fn fetch_item(
    github: Arc<dyn github::GitHubClient>,
    source: &Source,
    mode: ViewMode,
    file_ix: usize,
) -> anyhow::Result<Loaded> {
    fetch_item_scoped(github, source, mode, file_ix, false)
}

/// Blocking fetch, parse, and row building on the background executor. PR
/// metadata, patch, discussion, commits, and viewed state fetch concurrently;
/// the review baseline is then validated against the captured metadata head.
/// Optional comparison failures keep the complete PR diff usable.
///
/// The local snapshot records last-seen bookkeeping, independently of the
/// submitted GitHub review that determines the incremental diff's baseline.
#[cfg(test)]
pub(crate) fn fetch_item_scoped(
    github: Arc<dyn github::GitHubClient>,
    source: &Source,
    mode: ViewMode,
    file_ix: usize,
    since_review: bool,
) -> anyhow::Result<Loaded> {
    fetch_item_with_conversation(github, source, mode, file_ix, since_review, None)
}

/// Publish conversation data independently of the patch, commits, viewed state,
/// and review-baseline work. The final result still contains one consistent
/// metadata/discussion pair; there is no duplicate API fetch.
pub(crate) fn fetch_item_with_conversation(
    github: Arc<dyn github::GitHubClient>,
    source: &Source,
    mode: ViewMode,
    file_ix: usize,
    since_review: bool,
    conversation: Option<futures::channel::oneshot::Sender<LoadedConversation>>,
) -> anyhow::Result<Loaded> {
    let mut review_changes = github::ReviewChanges::default();
    let mut review_base = None;
    let mut review_base_files = None;
    let (meta, patch, comments, discussion, commits, viewed, snapshot) = match source {
        Source::Pr(loc) => {
            let meta_client = github.clone();
            let meta_loc = loc.clone();
            let meta_thread = std::thread::spawn(move || meta_client.pr_meta(&meta_loc));
            let comments_client = github.clone();
            let comments_loc = loc.clone();
            let comments_thread =
                std::thread::spawn(move || comments_client.pr_discussion(&comments_loc));
            let commits_client = github.clone();
            let commits_loc = loc.clone();
            let commits_thread =
                std::thread::spawn(move || commits_client.pr_commits(&commits_loc));
            let viewed_client = github.clone();
            let viewed_loc = loc.clone();
            let viewed_thread = std::thread::spawn(move || viewed_client.viewed_files(&viewed_loc));
            let patch_client = github.clone();
            let patch_loc = loc.clone();
            let patch_thread = std::thread::spawn(move || patch_client.pr_patch(&patch_loc));
            let meta = meta_thread
                .join()
                .map_err(|_| anyhow!("GitHub metadata fetch panicked"))??;
            let discussion = comments_thread
                .join()
                .map_err(|_| anyhow!("GitHub comments fetch panicked"))??;
            // Send from the background-executor task, not a raw API thread:
            // GPUI's deterministic test scheduler owns the receiver's waker.
            if let Some(sender) = conversation {
                let _ = sender.send(LoadedConversation {
                    meta: meta.clone(),
                    discussion: discussion.clone(),
                });
            }
            let patch = patch_thread
                .join()
                .map_err(|_| anyhow!("GitHub patch fetch panicked"))??;
            // Both the timeline and inline index derive from this discussion;
            // never fetch a second, potentially inconsistent comment collection.
            let comments = discussion.review_comments();
            // Optional detail data must never make a usable diff fail to open.
            let commits = commits_thread.join().ok().and_then(Result::ok);
            let viewed = viewed_thread.join().unwrap_or_else(|_| {
                Err(github::ClientError::Failed(
                    "viewed-state fetch panicked".into(),
                ))
            });
            review_changes = github.review_changes(loc, &meta.head_ref_oid).unwrap_or_else(|error| {
                github::ReviewChanges::Unavailable(format!(
                    "Couldn't verify changes since your last review: {error}. View all changes instead."
                ))
            });
            let mut patch = patch;
            if since_review {
                if let Some(review) = review_changes.baseline() {
                    match github.comparison_patch(loc, &review.commit, &meta.head_ref_oid, &meta.base_ref_oid) {
                        Ok(comparison) => {
                            review_base = Some(review.commit.clone());
                            patch = comparison.patch;
                            review_base_files = comparison.base_files.map(Arc::new);
                        }
                        Err(error) => review_changes = github::ReviewChanges::Unavailable(format!(
                            "Couldn't load changes since your last review: {error}. Showing all changes instead."
                        )),
                    }
                }
            }
            let snapshot = discussion_snapshot(loc, &meta, &discussion, commits.as_ref())?;
            (
                LoadedMeta::Pr(meta),
                patch,
                Some(group_comments(
                    comments.into_iter().map(ReviewComment::from).collect(),
                )),
                Some(discussion),
                commits,
                Some(viewed),
                Some(snapshot),
            )
        }
        Source::Local(src) => {
            let src = git::resolve_local_with_base(&src.repo_root, src.base_ref.as_deref())?;
            let patch = git::diff_patch(&src)?;
            (LoadedMeta::Local(src), patch, None, None, None, None, None)
        }
    };
    let mut comments = comments;
    if review_base.is_some() {
        if let Some(index) = &mut comments {
            index.hide_left_anchors();
        }
    }
    let mut diff = diff_core::parse_patch(&patch);
    let anchor_hunks = diff.files.iter().map(|file| file.hunks.clone()).collect();
    let jobs: Vec<_> = diff
        .files
        .iter()
        .enumerate()
        .filter(|(_, file)| file.status != diff_core::FileStatus::Binary && !file.hunks.is_empty())
        .map(|(file_ix, file)| UpgradeJob {
            file_ix,
            old_path: file.old_path.clone(),
            new_path: file.new_path.clone(),
            status: file.status,
        })
        .collect();
    let content_source = match (&meta, source) {
        (LoadedMeta::Pr(meta), Source::Pr(loc))
            if !meta.base_ref_oid.is_empty() && !meta.head_ref_oid.is_empty() =>
        {
            Some(UpgradeSource::Pr {
                loc: loc.clone(),
                base_oid: review_base.as_ref().unwrap_or(&meta.base_ref_oid).clone(),
                head_oid: meta.head_ref_oid.clone(),
                base_files: review_base_files.clone(),
            })
        }
        (LoadedMeta::Local(src), _) => Some(UpgradeSource::Local(src.clone())),
        _ => None,
    };
    // Finish full-content loading before publishing any diff rows. Unavailable
    // or oversized files keep their patch fallback, but never flash it first.
    let mut upgrades = HashMap::new();
    if let Some(source) = content_source.filter(|_| !jobs.is_empty()) {
        for file in run_upgrade(github.as_ref(), &source, jobs) {
            let target = &mut diff.files[file.file_ix];
            target.hunks = file.hunks;
            target.additions = file.additions;
            target.deletions = file.deletions;
            upgrades.insert(file.file_ix, file.upgrade);
        }
    }
    let built_rows = build_rows(RowsView {
        diff: &diff,
        upgrades: &upgrades,
        comments: comments.as_ref(),
        options: RowOptions {
            mode,
            file_ix,
            comments_visible: true,
            comment_wrap: COMMENT_WRAP_CHARS,
        },
    });
    Ok(Loaded {
        meta,
        diff,
        anchor_hunks,
        upgrades,
        built_rows,
        mode,
        file_ix,
        comments,
        discussion,
        commits,
        review_changes,
        review_base,
        review_base_files,
        viewed,
        snapshot,
    })
}

/// Shared by full fetches and post/reply refreshes, so the offline copy never
/// lags behind a successful comment refresh. Preserve existing root-comment
/// bookkeeping identities while carrying authoritative thread resolution.
pub(crate) fn discussion_snapshot(
    loc: &github::PrLocator,
    meta: &github::PrMeta,
    discussion: &github::PrDiscussion,
    commits: Option<&github::PrCommits>,
) -> anyhow::Result<local_store::StoredPrSnapshot> {
    let mut snapshot = pr_snapshot(loc, meta, &discussion.review_comments());
    snapshot.discussion = Some(serde_json::to_value(discussion)?);
    snapshot.commits = commits.map(serde_json::to_value).transpose()?;
    let resolved: std::collections::HashSet<_> = discussion
        .threads
        .iter()
        .filter(|thread| thread.is_resolved)
        .filter_map(|thread| {
            thread
                .comments
                .first()
                .map(|comment| comment.id.to_string())
        })
        .collect();
    for seen in &mut snapshot.threads {
        seen.resolved = resolved.contains(&seen.id);
    }
    Ok(snapshot)
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
        discussion: None,
        commits: None,
        is_draft: false,
        fetched_at_ms: local_store::now_ms(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_util::rc;
    use crate::threads::assemble_threads;

    fn snapshot(comments: &[github::ReviewComment]) -> local_store::StoredPrSnapshot {
        let loc = github::PrLocator {
            owner: "acme".into(),
            repo: "widgets".into(),
            number: 7,
        };
        let meta = github::PrMeta {
            number: 7,
            title: "Title".into(),
            body: "Body".into(),
            author: github::Author {
                login: "alice".into(),
            },
            state: "OPEN".into(),
            url: String::new(),
            base_ref_name: "main".into(),
            head_ref_name: "feature".into(),
            base_ref_oid: "base".into(),
            head_ref_oid: "head".into(),
            additions: 0,
            deletions: 0,
            changed_files: 0,
            review_decision: String::new(),
        };
        pr_snapshot(&loc, &meta, comments)
    }

    #[test]
    fn pr_snapshot_keeps_raw_root_order_outdated_roots_and_unordered_descendant_max() {
        let raw = vec![
            rc(
                20,
                "stale.rs",
                None,
                None,
                "outdated",
                "alice",
                "2026-03-01T00:00:00Z",
                None,
            ),
            rc(
                3,
                "a.rs",
                None,
                Some(1),
                "nested",
                "bob",
                "2026-04-01T00:00:00Z",
                Some(2),
            ),
            rc(
                2,
                "a.rs",
                None,
                Some(1),
                "early reply",
                "bob",
                "2026-01-01T00:00:00Z",
                Some(1),
            ),
            rc(
                1,
                "a.rs",
                None,
                Some(1),
                "root",
                "alice",
                "2026-02-01T00:00:00Z",
                None,
            ),
            rc(
                21,
                "stale.rs",
                None,
                None,
                "outdated reply",
                "bob",
                "2026-05-01T00:00:00Z",
                Some(20),
            ),
            rc(
                99,
                "a.rs",
                None,
                Some(1),
                "orphan",
                "bob",
                "2099-01-01T00:00:00Z",
                Some(999),
            ),
        ];
        // The persistence boundary sees the raw API collection before conversion.
        let snapshot = snapshot(&raw);
        assert_eq!(snapshot.repository, "acme/widgets");
        assert_eq!(snapshot.number, 7);
        assert_eq!(snapshot.head_sha, "head");
        assert_eq!(
            snapshot
                .threads
                .iter()
                .map(|t| (t.id.as_str(), t.updated_at.as_str(), t.resolved))
                .collect::<Vec<_>>(),
            vec![
                ("20", "2026-05-01T00:00:00Z", false),
                ("1", "2026-04-01T00:00:00Z", false),
            ]
        );
        let ui = assemble_threads(raw.into_iter().map(ReviewComment::from).collect());
        assert_eq!(
            ui.iter().map(|t| t.root.id).collect::<Vec<_>>(),
            vec![1, 20]
        );
        assert!(
            ui[0].replies.is_empty(),
            "UI drops the early parent and its descendant"
        );
        assert_eq!(ui[1].replies[0].id, 21);
    }

    #[test]
    fn pr_snapshot_preserves_its_parent_walk_depth_limit_and_raw_timestamps() {
        let mut raw = vec![rc(1, "stale.rs", None, None, "root", "alice", "a", None)];
        for id in 2..=35 {
            raw.push(rc(
                id,
                "stale.rs",
                None,
                None,
                "reply",
                "bob",
                match id {
                    34 => "y", // 33 edges to root: still reached.
                    35 => "z", // 34 edges: guard stops before reaching root.
                    _ => "b",
                },
                Some(id - 1),
            ));
        }
        raw.reverse(); // Snapshot parent walks do not depend on input order.
        let snapshot = snapshot(&raw);
        assert_eq!(snapshot.threads.len(), 1);
        assert_eq!(snapshot.threads[0].id, "1");
        assert_eq!(snapshot.threads[0].updated_at, "y");
        assert!(!snapshot.threads[0].resolved);
        let ui = assemble_threads(raw.into_iter().map(ReviewComment::from).collect());
        assert_eq!(
            ui[0].replies.last().unwrap().id,
            35,
            "UI assembly has no snapshot depth bound"
        );
    }

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
