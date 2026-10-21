//! The queue cache: the review queue as it was last seen, on disk.
//!
//! The cache exists so the app can paint a queue *before* the network
//! answers — and so it can paint one at all with no network. It holds one
//! row: the last queue that was fetched, with the search that produced it
//! and the moment it was fetched.
//!
//! The rows are stored as a JSON column: a queue row's shape is GitHub's
//! shape, which drifts, and a JSON column absorbs that drift without a
//! migration. The *search* that produced the queue is stored beside it, in
//! its own column, because the app compares it against the search it is
//! about to run — a cache from a different search is a different queue.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

/// The queue as it was cached: the rows, the search that produced them,
/// and when it was fetched.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedQueue {
    /// The queue rows, in the order the search returned them.
    #[serde(default)]
    pub prs: Vec<CachedQueueRow>,
    /// The search that produced this queue: its compiled query and its
    /// sort spec. Empty for a queue that was never fetched.
    #[serde(default)]
    pub search: String,
    /// The sort spec, `field` and `order`, of the search that produced
    /// this queue.
    #[serde(default)]
    pub sort: String,
    /// The order of the sort spec, kept beside it so both halves of the
    /// spec survive a round trip.
    #[serde(default)]
    pub order: String,
    /// When the queue was fetched, in milliseconds since the epoch.
    #[serde(default)]
    pub fetched_at_ms: i64,
}

impl CachedQueue {
    /// An empty cache: no rows, no search, never fetched.
    pub fn empty() -> Self {
        Self::default()
    }

    /// The moment the cache was fetched, as milliseconds since the epoch.
    pub fn fetched_at_ms(&self) -> i64 {
        self.fetched_at_ms
    }

    /// How old the cache is, in seconds, relative to `now_ms`. Zero for a
    /// cache that was never fetched.
    pub fn age_seconds(&self, now_ms: i64) -> i64 {
        if self.fetched_at_ms <= 0 {
            return 0;
        }
        ((now_ms - self.fetched_at_ms) / 1000).max(0)
    }

    /// Whether the cache was written by a search with this query and sort.
    /// A cache from a different search is a different queue — the app
    /// shows it anyway (better stale rows than an empty screen) but knows
    /// the two aren't the same.
    pub fn matches(&self, query: &str, sort: &str, order: &str) -> bool {
        self.search == query && self.sort == sort && self.order == order
    }
}

/// One cached queue row.
///
/// A queue row is a flat, self-contained summary of a PR — title, author,
/// repo, sizes, CI verdict, labels — so it is cached as its own JSON
/// object rather than as part of one big queue blob: a queue that changes
/// only slightly then rewrites only the rows that changed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedQueueRow {
    pub number: u64,
    pub title: String,
    pub author: String,
    #[serde(default)]
    pub author_avatar_url: Option<String>,
    pub repository: String,
    #[serde(default)]
    pub is_draft: bool,
    /// The review state: `awaiting`, `approved`, or `changes_requested`.
    #[serde(default)]
    pub review_state: String,
    /// The CI verdict: `pass`, `fail`, `pending`, or `no_checks`.
    #[serde(default)]
    pub ci: String,
    #[serde(default)]
    pub additions: Option<u64>,
    #[serde(default)]
    pub deletions: Option<u64>,
    #[serde(default)]
    pub comments: u64,
    #[serde(default)]
    pub labels: Vec<CachedLabel>,
    pub updated_at: String,
}

/// One cached label: GitHub paints labels by name and a six-hex-digit
/// color, so both are cached verbatim.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedLabel {
    pub name: String,
    pub color: String,
}

/// The queue cache, over a connection.
pub struct QueueCache<'a> {
    conn: &'a Connection,
}

impl<'a> QueueCache<'a> {
    pub(crate) fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    /// The cached queue, or an empty one when nothing was ever cached.
    /// A cache row that can't be parsed is treated as no cache at all —
    /// a corrupt blob must never keep the app from starting.
    pub fn read(&self) -> Result<CachedQueue> {
        let row: Option<(String, String, String, String, i64)> = self
            .conn
            .query_row(
                "SELECT rows, search, sort, sort_order, fetched_at_ms FROM queue_cache WHERE id = 1",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()
            .context("couldn't read the queue cache")?;
        let Some((rows, search, sort, order, fetched_at_ms)) = row else {
            return Ok(CachedQueue::empty());
        };
        let prs = serde_json::from_str(&rows).unwrap_or_default();
        Ok(CachedQueue {
            prs,
            search,
            sort,
            order,
            fetched_at_ms,
        })
    }

    /// Replace the cached queue with `queue`.
    pub fn write(&self, queue: &CachedQueue) -> Result<()> {
        let rows = serde_json::to_string(&queue.prs).context("couldn't encode the queue cache")?;
        self.conn
            .execute(
                "INSERT INTO queue_cache (id, rows, search, sort, sort_order, fetched_at_ms) \
                 VALUES (1, ?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(id) DO UPDATE SET rows = ?1, search = ?2, sort = ?3, \
                 sort_order = ?4, fetched_at_ms = ?5",
                rusqlite::params![
                    rows,
                    queue.search,
                    queue.sort,
                    queue.order,
                    queue.fetched_at_ms
                ],
            )
            .context("couldn't write the queue cache")?;
        Ok(())
    }

    /// Forget the cached queue — the rows and the moment it was fetched.
    pub fn clear(&self) -> Result<()> {
        self.conn
            .execute("DELETE FROM queue_cache WHERE id = 1", [])
            .context("couldn't clear the queue cache")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cached queue, with rows, as the app would write it.
    fn sample() -> CachedQueue {
        CachedQueue {
            prs: vec![CachedQueueRow {
                number: 4781,
                title: "Add deactivation filters".into(),
                author: "octocat".into(),
                author_avatar_url: Some("https://avatars.githubusercontent.com/u/583231".into()),
                repository: "octocat/Hello-World".into(),
                is_draft: true,
                review_state: "awaiting".into(),
                ci: "pass".into(),
                additions: Some(151),
                deletions: Some(30),
                comments: 2,
                labels: vec![CachedLabel {
                    name: "api".into(),
                    color: "1D76DB".into(),
                }],
                updated_at: "2026-10-20T08:07:08Z".into(),
            }],
            search: "is:pr is:open review-requested:@me".into(),
            sort: "updated".into(),
            order: "desc".into(),
            fetched_at_ms: 1_700_000_000_000,
        }
    }

    /// The cache round-trips: what was written reads back byte for byte,
    /// every field included.
    #[test]
    fn a_cached_queue_round_trips() {
        let store = crate::Store::open_in_memory().unwrap();
        let cache = store.queue_cache();
        let queue = sample();
        cache.write(&queue).expect("the write must succeed");
        let read = cache.read().expect("the read must succeed");
        assert_eq!(read, queue);
    }

    /// A store that was never written to reads an empty cache, not an
    /// error — the app paints an empty queue, then the network fills it.
    #[test]
    fn an_empty_cache_reads_as_empty() {
        let store = crate::Store::open_in_memory().unwrap();
        let read = store.queue_cache().read().unwrap();
        assert_eq!(read, CachedQueue::empty());
        assert!(read.prs.is_empty());
        assert_eq!(read.fetched_at_ms(), 0);
    }

    /// A second write replaces the first: the cache holds the *last* queue,
    /// never an accumulation of queues.
    #[test]
    fn writing_again_replaces_the_cached_queue() {
        let store = crate::Store::open_in_memory().unwrap();
        let cache = store.queue_cache();
        cache.write(&sample()).unwrap();
        let mut second = sample();
        second.prs.clear();
        second.search = "is:pr is:open review-requested:@me author:bob".into();
        cache.write(&second).unwrap();
        let read = cache.read().unwrap();
        assert!(read.prs.is_empty());
        assert_eq!(read.search, "is:pr is:open review-requested:@me author:bob");
    }

    /// A corrupt cache blob degrades to an empty cache: a broken row must
    /// never keep the app from starting.
    #[test]
    fn a_corrupt_cache_degrades_to_empty() {
        let store = crate::Store::open_in_memory().unwrap();
        store
            .conn
            .execute(
                "INSERT INTO queue_cache (id, rows) VALUES (1, 'not json')",
                [],
            )
            .unwrap();
        let read = store.queue_cache().read().unwrap();
        assert!(read.prs.is_empty());
    }

    /// A cache knows which search produced it — the app compares the two
    /// to know whether the rows it paints are the rows it asked for.
    #[test]
    fn a_cache_knows_its_search() {
        let store = crate::Store::open_in_memory().unwrap();
        let cache = store.queue_cache();
        cache.write(&sample()).unwrap();
        let read = cache.read().unwrap();
        assert!(read.matches("is:pr is:open review-requested:@me", "updated", "desc"));
        assert!(!read.matches("is:pr is:open review-requested:@me", "updated", "asc"));
        assert!(!read.matches("is:pr is:open", "updated", "desc"));
    }

    /// Age is measured against the clock the caller passes in, so the
    /// freshness bookkeeping is testable without sleeping.
    #[test]
    fn a_cache_reports_its_age() {
        let store = crate::Store::open_in_memory().unwrap();
        let cache = store.queue_cache();
        let mut queue = sample();
        queue.fetched_at_ms = 1_000_000;
        cache.write(&queue).unwrap();
        let read = cache.read().unwrap();
        assert_eq!(read.age_seconds(1_000_000), 0);
        assert_eq!(read.age_seconds(1_030_000), 30);
        // A clock that went backwards (or a cache from the future) reads as
        // fresh rather than negative.
        assert_eq!(read.age_seconds(900_000), 0);
    }

    /// Clearing the cache forgets the rows and the moment — the app asks
    /// for this when the queue is rebuilt from scratch.
    #[test]
    fn clearing_forgets_the_cached_queue() {
        let store = crate::Store::open_in_memory().unwrap();
        let cache = store.queue_cache();
        cache.write(&sample()).unwrap();
        cache.clear().unwrap();
        let read = cache.read().unwrap();
        assert!(read.prs.is_empty());
        assert_eq!(read.fetched_at_ms(), 0);
    }
}
