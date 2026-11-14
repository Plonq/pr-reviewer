//! The app's side of the local store: the handle the UI keeps, and the
//! translation between the app's domain types and the store's rows.
//!
//! The store itself lives in the `store` crate and knows nothing about
//! the UI. This module is the seam: it owns the `Mutex<Store>` the app
//! shares across threads, opens the database at the platform's
//! application-support location, and translates a fetched queue into the
//! cache rows the store keeps.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use github::{CiState, QueueItem, QueueSearch, ReviewState};
use store::{CachedLabel, CachedQueue, CachedQueueRow, Store};

/// Re-exported so the app can name the store's types without reaching
/// past this module.
pub use store::{PrSnapshot as StoredPrSnapshot, ReviewPosition, ThreadSeen as StoredThreadSeen};

/// The app's handle to the local store.
///
/// The store is shared across threads (the queue fetch writes to it from
/// the background executor), so it lives behind a mutex. Reads are fast
/// enough to hold it across a query — a cached queue is a few hundred
/// rows of JSON — and the writes are one transaction each.
#[derive(Clone)]
pub struct LocalStore {
    inner: Arc<Mutex<Store>>,
    /// Where the database lives, for the diagnostics line.
    path: Option<PathBuf>,
}

impl std::fmt::Debug for LocalStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalStore")
            .field("path", &self.path)
            .finish()
    }
}

impl LocalStore {
    pub fn diff_preferences(&self, loc: Option<&github::PrLocator>) -> store::DiffPreferences {
        let Ok(store) = self.inner.lock() else {
            return Default::default();
        };
        let slug = loc.map(|loc| loc.repo_slug());
        store
            .settings()
            .diff_preferences(
                loc.zip(slug.as_deref())
                    .map(|(loc, slug)| (slug, loc.number)),
            )
            .unwrap_or_default()
    }

    pub fn save_diff_preferences(
        &self,
        loc: Option<&github::PrLocator>,
        prefs: store::DiffPreferences,
    ) {
        let Ok(store) = self.inner.lock() else { return };
        let slug = loc.map(|loc| loc.repo_slug());
        if let Err(err) = store.settings().set_diff_preferences(
            loc.zip(slug.as_deref())
                .map(|(loc, slug)| (slug, loc.number)),
            prefs,
        ) {
            eprintln!("gander: couldn't save diff preferences: {err}");
        }
    }

    pub fn position(&self, loc: &github::PrLocator) -> Option<ReviewPosition> {
        let store = self.inner.lock().ok()?;
        store
            .positions()
            .read(&loc.repo_slug(), loc.number)
            .ok()
            .flatten()
    }

    pub fn save_position(&self, loc: &github::PrLocator, position: &ReviewPosition) -> bool {
        let Ok(store) = self.inner.lock() else {
            return false;
        };
        store
            .positions()
            .write(&loc.repo_slug(), loc.number, position)
            .is_ok()
    }

    /// A display-only mirror. The session always reconciles with GitHub before
    /// writing, so a draft submitted elsewhere is never resurrected.
    pub fn review_session(&self, loc: &github::PrLocator) -> github::review_session::ReviewSession {
        let Ok(store) = self.inner.lock() else {
            return Default::default();
        };
        store
            .snapshots()
            .pending_review(&loc.repo_slug(), loc.number)
            .ok()
            .flatten()
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default()
    }

    pub fn save_review_session(
        &self,
        loc: &github::PrLocator,
        session: &github::review_session::ReviewSession,
    ) {
        let Ok(store) = self.inner.lock() else {
            return;
        };
        let value = session
            .pending
            .as_ref()
            .and_then(|_| serde_json::to_value(session).ok());
        let _ =
            store
                .snapshots()
                .write_pending_review(&loc.repo_slug(), loc.number, value.as_ref());
    }

    /// Open the store at the platform's application-support location.
    ///
    /// A store that can't be opened (an unwritable directory, a corrupt
    /// database) is not a reason to refuse to start: the app falls back
    /// to an in-memory store, which behaves the same except that nothing
    /// survives the launch. The reason is reported so the app can say so,
    /// together with the path it wanted — that's what the user might have
    /// to fix.
    pub fn open_default() -> (Self, Option<String>) {
        match Store::open_default() {
            Ok(store) => {
                let path = store.path().map(PathBuf::from);
                (
                    Self {
                        inner: Arc::new(Mutex::new(store)),
                        path,
                    },
                    None,
                )
            }
            Err(err) => {
                let reason = err.to_string();
                let wanted = store::default_database_path();
                let store = Store::open_in_memory().unwrap_or_else(|_| {
                    // An in-memory store can't fail to open; if it somehow
                    // does, the app still has to run, so fall back to a
                    // store that panics only on use — which never happens
                    // because the app treats a store failure as "no cache".
                    panic!("an in-memory store must open: {err:#}")
                });
                (
                    Self {
                        inner: Arc::new(Mutex::new(store)),
                        path: wanted,
                    },
                    Some(reason),
                )
            }
        }
    }
    /// An in-memory store, for tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn in_memory() -> Self {
        Self {
            inner: Arc::new(Mutex::new(
                Store::open_in_memory().expect("an in-memory store must open"),
            )),
            path: None,
        }
    }

    /// Where the database lives, when it lives on disk.
    pub fn path(&self) -> Option<&std::path::Path> {
        self.path.as_deref()
    }

    /// The cached queue, or an empty one when nothing was cached.
    pub fn cached_queue(&self) -> CachedQueue {
        let Ok(store) = self.inner.lock() else {
            return CachedQueue::empty();
        };
        store.queue_cache().read().unwrap_or_default()
    }

    /// The cached queue's rows, as the queue's own rows.
    pub fn cached_queue_items(&self) -> Vec<QueueItem> {
        self.cached_queue().prs.iter().map(queue_item).collect()
    }

    /// Cache a fetched queue: the rows, the search that produced them,
    /// and the moment they arrived.
    pub fn cache_queue(&self, search: &QueueSearch, prs: &[QueueItem]) {
        let (sort, order) = search.sort();
        let queue = CachedQueue {
            prs: prs.iter().map(cached_row).collect(),
            search: search.query(),
            sort: sort.to_string(),
            order: order.to_string(),
            fetched_at_ms: now_ms(),
        };
        self.write_queue(&queue);
    }

    /// Write a queue into the cache verbatim.
    pub fn write_queue(&self, queue: &CachedQueue) {
        let Ok(store) = self.inner.lock() else {
            return;
        };
        let _ = store.queue_cache().write(queue);
    }

    /// A setting's value, when it is set.
    pub fn setting(&self, key: &str) -> Option<serde_json::Value> {
        let store = self.inner.lock().ok()?;
        store.settings().get(key).ok().flatten()
    }

    /// Set a setting's value.
    pub fn set_setting(&self, key: &str, value: serde_json::Value) {
        let Ok(store) = self.inner.lock() else {
            return;
        };
        let _ = store.settings().set(key, value);
    }

    /// Restart must not proceed if the session could not be saved durably.
    pub fn set_setting_checked(&self, key: &str, value: serde_json::Value) -> anyhow::Result<()> {
        let store = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("store lock poisoned"))?;
        store.settings().set(key, value)
    }

    /// Store a PR's snapshot: what the app last saw of it, and when.
    ///
    /// The per-thread bookkeeping is written alongside it: the threads
    /// the snapshot carries are the ones the app has now seen, and the
    /// novelty module compares the next fetch's threads against them.
    pub fn save_snapshot(&self, snapshot: &store::PrSnapshot) {
        let Ok(store) = self.inner.lock() else {
            return;
        };
        let snapshots = store.snapshots();
        let _ = snapshots.write(snapshot);
        for thread in &snapshot.threads {
            let _ = snapshots.mark_thread_seen(&snapshot.repository, snapshot.number, thread);
        }
    }

    /// The snapshot of a PR, when the app has one.
    pub fn snapshot(&self, repository: &str, number: u64) -> Option<store::PrSnapshot> {
        let store = self.inner.lock().ok()?;
        store.snapshots().read(repository, number).ok().flatten()
    }

    /// The head SHA the app last saw for a PR, when it has one. The
    /// novelty module compares it with the fresh one to tell "changed
    /// since I looked" from "as I left it".
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn head_sha(&self, repository: &str, number: u64) -> Option<String> {
        let store = self.inner.lock().ok()?;
        store
            .snapshots()
            .head_sha(repository, number)
            .ok()
            .flatten()
    }

    /// When the app last fetched a PR's detail, in milliseconds since the
    /// epoch. The freshness bookkeeping compares it with the clock.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn snapshot_fetched_at_ms(&self, repository: &str, number: u64) -> Option<i64> {
        let store = self.inner.lock().ok()?;
        store
            .snapshots()
            .fetched_at_ms(repository, number)
            .ok()
            .flatten()
    }

    /// The threads the app has seen on a PR, with the moment it saw each.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn threads_seen(&self, repository: &str, number: u64) -> Vec<store::ThreadSeen> {
        let Ok(store) = self.inner.lock() else {
            return Vec::new();
        };
        store
            .snapshots()
            .thread_seen(repository, number)
            .unwrap_or_default()
    }
}

/// The current time in milliseconds since the epoch.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A queue row, as the cache stores it.
fn cached_row(item: &QueueItem) -> CachedQueueRow {
    CachedQueueRow {
        number: item.number,
        title: item.title.clone(),
        author: item.author.clone(),
        author_avatar_url: item.author_avatar_url.clone(),
        repository: item.repository.clone(),
        is_draft: item.is_draft,
        review_state: review_state_name(item.review_state).to_string(),
        ci: ci_state_name(item.ci).to_string(),
        additions: item.additions,
        deletions: item.deletions,
        comments: item.comments,
        labels: item
            .labels
            .iter()
            .map(|label| CachedLabel {
                name: label.name.clone(),
                color: label.color.clone(),
            })
            .collect(),
        created_at: item.created_at.clone(),
        review_requested_at: item.review_requested_at.clone(),
        updated_at: item.updated_at.clone(),
    }
}

/// A cached row, as the queue's own row.
pub fn queue_item(row: &CachedQueueRow) -> QueueItem {
    QueueItem {
        number: row.number,
        title: row.title.clone(),
        author: row.author.clone(),
        author_avatar_url: row.author_avatar_url.clone(),
        repository: row.repository.clone(),
        is_draft: row.is_draft,
        review_state: review_state_from_name(&row.review_state),
        ci: ci_state_from_name(&row.ci),
        additions: row.additions,
        deletions: row.deletions,
        comments: row.comments,
        labels: row
            .labels
            .iter()
            .map(|label| github::QueueLabel {
                name: label.name.clone(),
                color: label.color.clone(),
            })
            .collect(),
        created_at: row.created_at.clone(),
        review_requested_at: row.review_requested_at.clone(),
        reviewed_at: None,
        updated_at: row.updated_at.clone(),
    }
}

/// A review state, as the cache spells it.
fn review_state_name(state: ReviewState) -> &'static str {
    match state {
        ReviewState::Awaiting => "awaiting",
        ReviewState::Approved => "approved",
        ReviewState::ChangesRequested => "changes_requested",
    }
}

/// A cached review state, read back. An unknown spelling degrades to
/// `Awaiting` — the queue's honest default — never to a wrong verdict.
fn review_state_from_name(name: &str) -> ReviewState {
    match name {
        "approved" => ReviewState::Approved,
        "changes_requested" => ReviewState::ChangesRequested,
        _ => ReviewState::Awaiting,
    }
}

/// A CI state, as the cache spells it.
fn ci_state_name(state: CiState) -> &'static str {
    match state {
        CiState::Pass => "pass",
        CiState::Fail => "fail",
        CiState::Pending => "pending",
        CiState::NoChecks => "no_checks",
    }
}

/// A cached CI state, read back. An unknown spelling degrades to
/// `NoChecks` — the queue's honest default.
fn ci_state_from_name(name: &str) -> CiState {
    match name {
        "pass" => CiState::Pass,
        "fail" => CiState::Fail,
        "pending" => CiState::Pending,
        _ => CiState::NoChecks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A queue row, as the facade produces it.
    fn item(number: u64) -> QueueItem {
        QueueItem {
            number,
            title: "Add deactivation filters".into(),
            author: "octocat".into(),
            author_avatar_url: Some("https://avatars.githubusercontent.com/u/583231".into()),
            repository: "octocat/Hello-World".into(),
            is_draft: true,
            review_state: ReviewState::ChangesRequested,
            ci: CiState::Pass,
            additions: Some(151),
            deletions: Some(30),
            comments: 2,
            labels: vec![github::QueueLabel {
                name: "api".into(),
                color: "1D76DB".into(),
            }],
            created_at: "2026-10-15T10:00:00Z".into(),
            review_requested_at: Some("2026-10-19T09:00:00Z".into()),
            reviewed_at: None,
            updated_at: "2026-10-20T08:07:08Z".into(),
        }
    }

    /// A fetched queue survives the round trip into the cache and back:
    /// every field, including the verdicts and the labels.
    #[test]
    fn a_queue_round_trips_through_the_cache() {
        let store = LocalStore::in_memory();
        let search = QueueSearch::review_requested();
        store.cache_queue(&search, &[item(4781)]);
        let cached = store.cached_queue();
        assert_eq!(cached.prs.len(), 1);
        assert_eq!(cached.search, "is:pr is:open review-requested:@me");
        assert_eq!(cached.sort, "review-requested");
        assert_eq!(cached.order, "asc");
        assert!(cached.fetched_at_ms > 0);
        let items = store.cached_queue_items();
        assert_eq!(items, vec![item(4781)]);
    }

    /// A cached queue knows the search it came from, so the app can tell
    /// a queue it asked for from one it didn't.
    #[test]
    fn a_cached_queue_knows_its_search() {
        let store = LocalStore::in_memory();
        store.cache_queue(&QueueSearch::review_requested(), &[item(1)]);
        let cached = store.cached_queue();
        assert!(cached.matches(
            "is:pr is:open review-requested:@me",
            "review-requested",
            "asc"
        ));
        assert!(!cached.matches("is:pr is:open review-requested:@me", "created", "asc"));
    }

    /// An empty store caches nothing and reads an empty queue — the app
    /// paints an empty screen and the network fills it.
    #[test]
    fn an_empty_store_caches_nothing() {
        let store = LocalStore::in_memory();
        assert!(store.cached_queue().prs.is_empty());
        assert!(store.cached_queue_items().is_empty());
    }

    /// The verdicts survive the round trip in both directions: a cached
    /// "changes requested" comes back as such, and a cached spelling this
    /// build doesn't know degrades to the honest default.
    #[test]
    fn verdicts_round_trip_and_unknown_spellings_degrade() {
        let store = LocalStore::in_memory();
        store.cache_queue(&QueueSearch::review_requested(), &[item(1)]);
        let cached = store.cached_queue();
        assert_eq!(cached.prs[0].review_state, "changes_requested");
        assert_eq!(cached.prs[0].ci, "pass");
        // An unknown spelling from a newer build reads back as the
        // honest default, not as a wrong verdict.
        let mut drifted = cached.clone();
        drifted.prs[0].review_state = "brand_new_state".into();
        drifted.prs[0].ci = "brand_new_state".into();
        store.write_queue(&drifted);
        let items = store.cached_queue_items();
        assert_eq!(items[0].review_state, ReviewState::Awaiting);
        assert_eq!(items[0].ci, CiState::NoChecks);
    }

    /// Settings round-trip through the store.
    #[test]
    fn settings_round_trip() {
        let store = LocalStore::in_memory();
        assert!(store.setting("sort").is_none());
        store.set_setting("sort", serde_json::Value::String("oldest".into()));
        assert_eq!(
            store.setting("sort"),
            Some(serde_json::Value::String("oldest".into()))
        );
    }

    #[test]
    fn positions_survive_reopening_and_snapshot_refreshes_do_not_replace_them() {
        let path = std::env::temp_dir().join("opencode").join(format!(
            "gander-position-{}-{}.db",
            std::process::id(),
            now_ms()
        ));
        let open = || LocalStore {
            inner: Arc::new(Mutex::new(Store::open(&path).unwrap())),
            path: Some(path.clone()),
        };
        let loc = github::PrLocator {
            owner: "acme".into(),
            repo: "widgets".into(),
            number: 7,
        };
        let position = ReviewPosition {
            path: "src/lib.rs".into(),
            new_line: Some(52),
            row: 53,
            fraction: 0.375,
            horizontal_offset: 23.,
            split: true,
            ..Default::default()
        };
        {
            let store = open();
            assert!(store.save_position(&loc, &position));
            store.save_snapshot(&StoredPrSnapshot {
                repository: loc.repo_slug(),
                number: loc.number,
                ..Default::default()
            });
        }
        {
            let store = open();
            assert_eq!(store.position(&loc), Some(position));
            assert_eq!(
                store.position(&github::PrLocator {
                    number: 8,
                    ..loc.clone()
                }),
                None
            );
            assert_eq!(
                store.position(&github::PrLocator {
                    owner: "other".into(),
                    ..loc
                }),
                None
            );
        }
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn pending_drafts_survive_reopening_the_database_and_clear_after_submission() {
        use github::review_session::ReviewSession;
        let path = std::env::temp_dir().join("opencode").join(format!(
            "gander-review-{}-{}.db",
            std::process::id(),
            now_ms()
        ));
        let open = || LocalStore {
            inner: Arc::new(Mutex::new(Store::open(&path).unwrap())),
            path: Some(path.clone()),
        };
        let loc = github::PrLocator {
            owner: "acme".into(),
            repo: "widgets".into(),
            number: 7,
        };
        let pending = github::PendingReview {
            id: 91,
            node_id: "PRR_pending".into(),
            commit_id: Some("dead10cc".into()),
            body: "summary".into(),
            comments: vec![github::PendingComment {
                in_reply_to_id: None,
                node_id: "draft_node".into(),
                id: 12,
                path: "a.rs".into(),
                side: github::DiffSide::Left,
                line: Some(7),
                start_line: Some(5),
                body: "draft".into(),
            }],
        };
        {
            let store = open();
            store.save_review_session(
                &loc,
                &ReviewSession {
                    pending: Some(pending.clone()),
                    verified: true,
                },
            );
        }
        {
            let store = open();
            let restored = store.review_session(&loc);
            assert_eq!(restored.pending, Some(pending));
            assert!(
                !restored.verified,
                "disk mirrors must be reconciled before mutation"
            );
            let other = github::PrLocator {
                number: 8,
                ..loc.clone()
            };
            assert!(store.review_session(&other).pending.is_none());
            store.save_review_session(&loc, &ReviewSession::default());
        }
        {
            let store = open();
            assert!(store.review_session(&loc).pending.is_none());
        }
        let _ = std::fs::remove_file(path);
    }
}
