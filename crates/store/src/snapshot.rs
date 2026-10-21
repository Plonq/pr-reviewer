//! Per-PR bookkeeping: what the app remembers about each PR between
//! launches.
//!
//! Three things live here, and they are the "last seen" state the novelty
//! module compares fresh data against:
//!
//! - the **PR snapshot** — the last detail fetch of a PR, as a JSON blob,
//!   so a PR can be opened and read offline,
//! - the **head SHA** the snapshot was taken at — the PR's head commit at
//!   the time, so "what changed since I last looked" is a comparison
//!   rather than a guess,
//! - the **per-thread timestamps** — when each review thread was last
//!   seen, so a thread with new comments can be dotted.
//!
//! The snapshot is JSON (GitHub's shape drifts; a blob absorbs it). The
//! head SHA and the thread timestamps are columns, because they're
//! compared, not just read back.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

/// The last detail fetch of one PR, as the app stores it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrSnapshot {
    /// `owner/name` of the PR's repository.
    pub repository: String,
    /// The PR's number.
    pub number: u64,
    /// The PR's title.
    #[serde(default)]
    pub title: String,
    /// The PR's body (markdown), as GitHub served it.
    #[serde(default)]
    pub body: String,
    /// The author's login.
    #[serde(default)]
    pub author: String,
    /// The PR's head commit SHA, when the fetch carried one.
    #[serde(default)]
    pub head_sha: String,
    /// The PR's base ref name.
    #[serde(default)]
    pub base_ref: String,
    /// The PR's head ref name.
    #[serde(default)]
    pub head_ref: String,
    /// The review thread ids the snapshot carries, with the moment each
    /// was last seen — the novelty module's input.
    #[serde(default)]
    pub threads: Vec<ThreadSeen>,
    /// Whether the PR was a draft when the snapshot was taken.
    #[serde(default)]
    pub is_draft: bool,
    /// The moment the snapshot was taken, in milliseconds since the epoch.
    #[serde(default)]
    pub fetched_at_ms: i64,
}

/// One review thread the app has seen, and when.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadSeen {
    /// GitHub's thread id.
    pub id: String,
    /// The thread's last comment's timestamp, as GitHub served it.
    #[serde(default)]
    pub updated_at: String,
    /// Whether the thread was resolved when the app last saw it.
    #[serde(default)]
    pub resolved: bool,
}

/// The per-PR bookkeeping, over a connection.
pub struct SnapshotStore<'a> {
    conn: &'a Connection,
}

impl<'a> SnapshotStore<'a> {
    pub(crate) fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    /// The snapshot of one PR, when the app has one.
    pub fn read(&self, repository: &str, number: u64) -> Result<Option<PrSnapshot>> {
        let row: Option<String> = self
            .conn
            .query_row(
                "SELECT snapshot FROM pr_snapshots WHERE repository = ?1 AND number = ?2",
                rusqlite::params![repository, number as i64],
                |row| row.get(0),
            )
            .optional()
            .context("couldn't read the PR snapshot")?;
        Ok(row.and_then(|json| serde_json::from_str(&json).ok()))
    }

    /// Store the snapshot of one PR, replacing any earlier one. The head
    /// SHA is kept in its own column, so "has the head moved?" is a
    /// cheap comparison.
    pub fn write(&self, snapshot: &PrSnapshot) -> Result<()> {
        let json = serde_json::to_string(snapshot).context("couldn't encode the PR snapshot")?;
        self.conn
            .execute(
                "INSERT INTO pr_snapshots (repository, number, head_sha, snapshot, fetched_at_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(repository, number) DO UPDATE SET head_sha = ?3, snapshot = ?4, \
                 fetched_at_ms = ?5",
                rusqlite::params![
                    snapshot.repository,
                    snapshot.number as i64,
                    snapshot.head_sha,
                    json,
                    snapshot.fetched_at_ms
                ],
            )
            .context("couldn't write the PR snapshot")?;
        Ok(())
    }

    /// The head SHA the app last saw for a PR, when it has one.
    pub fn head_sha(&self, repository: &str, number: u64) -> Result<Option<String>> {
        let row: Option<String> = self
            .conn
            .query_row(
                "SELECT head_sha FROM pr_snapshots WHERE repository = ?1 AND number = ?2",
                rusqlite::params![repository, number as i64],
                |row| row.get(0),
            )
            .optional()
            .context("couldn't read the PR head SHA")?;
        Ok(row.filter(|sha| !sha.is_empty()))
    }

    /// When the app last fetched a PR's detail, in milliseconds since the
    /// epoch. `None` when it never did.
    pub fn fetched_at_ms(&self, repository: &str, number: u64) -> Result<Option<i64>> {
        let row: Option<i64> = self
            .conn
            .query_row(
                "SELECT fetched_at_ms FROM pr_snapshots WHERE repository = ?1 AND number = ?2",
                rusqlite::params![repository, number as i64],
                |row| row.get(0),
            )
            .optional()
            .context("couldn't read the PR snapshot time")?;
        Ok(row)
    }

    /// The moment each of a PR's threads was last seen, keyed by thread
    /// id. Threads the app has never seen are absent — the novelty module
    /// reads that as "new".
    pub fn thread_seen(&self, repository: &str, number: u64) -> Result<Vec<ThreadSeen>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT thread_id, updated_at, resolved FROM pr_threads \
                 WHERE repository = ?1 AND number = ?2 ORDER BY thread_id",
            )
            .context("couldn't prepare the thread query")?;
        let rows = stmt
            .query_map(rusqlite::params![repository, number as i64], |row| {
                Ok(ThreadSeen {
                    id: row.get(0)?,
                    updated_at: row.get(1)?,
                    resolved: row.get::<_, i64>(2)? != 0,
                })
            })
            .context("couldn't read the thread bookkeeping")?;
        let mut seen = Vec::new();
        for row in rows {
            seen.push(row.context("couldn't read a thread row")?);
        }
        Ok(seen)
    }

    /// Record the moment a thread was seen, together with whether it was
    /// resolved. A thread the app hasn't seen before is simply added.
    pub fn mark_thread_seen(
        &self,
        repository: &str,
        number: u64,
        thread: &ThreadSeen,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO pr_threads (repository, number, thread_id, updated_at, resolved) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(repository, number, thread_id) DO UPDATE SET \
                 updated_at = ?4, resolved = ?5",
                rusqlite::params![
                    repository,
                    number as i64,
                    thread.id,
                    thread.updated_at,
                    if thread.resolved { 1i64 } else { 0i64 }
                ],
            )
            .context("couldn't write the thread bookkeeping")?;
        Ok(())
    }

    /// Forget a PR's bookkeeping — the PR left the queue, or the app is
    /// starting over.
    pub fn forget(&self, repository: &str, number: u64) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM pr_snapshots WHERE repository = ?1 AND number = ?2",
                rusqlite::params![repository, number as i64],
            )
            .context("couldn't delete the PR snapshot")?;
        self.conn
            .execute(
                "DELETE FROM pr_threads WHERE repository = ?1 AND number = ?2",
                rusqlite::params![repository, number as i64],
            )
            .context("couldn't delete the thread bookkeeping")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A snapshot, as a detail fetch would produce it.
    fn sample() -> PrSnapshot {
        PrSnapshot {
            repository: "octocat/Hello-World".into(),
            number: 4781,
            title: "Add deactivation filters".into(),
            body: "Some **markdown**.".into(),
            author: "octocat".into(),
            head_sha: "abc123".into(),
            base_ref: "main".into(),
            head_ref: "feature/deactivation".into(),
            threads: vec![ThreadSeen {
                id: "T_1".into(),
                updated_at: "2026-10-20T08:07:08Z".into(),
                resolved: false,
            }],
            is_draft: false,
            fetched_at_ms: 1_700_000_000_000,
        }
    }

    /// A snapshot round-trips through the store: every field survives,
    /// including the head SHA and the thread list.
    #[test]
    fn a_snapshot_round_trips() {
        let store = crate::Store::open_in_memory().unwrap();
        let snapshots = store.snapshots();
        let snapshot = sample();
        snapshots.write(&snapshot).unwrap();
        let read = snapshots.read("octocat/Hello-World", 4781).unwrap();
        assert_eq!(read, Some(snapshot.clone()));
        assert_eq!(
            snapshots
                .head_sha("octocat/Hello-World", 4781)
                .unwrap()
                .as_deref(),
            Some("abc123")
        );
        assert_eq!(
            snapshots
                .fetched_at_ms("octocat/Hello-World", 4781)
                .unwrap(),
            Some(1_700_000_000_000)
        );
    }

    /// A PR the app has never seen reads as no snapshot — not an error.
    #[test]
    fn an_unseen_pr_has_no_snapshot() {
        let store = crate::Store::open_in_memory().unwrap();
        let snapshots = store.snapshots();
        assert_eq!(snapshots.read("octocat/Hello-World", 1).unwrap(), None);
        assert_eq!(snapshots.head_sha("octocat/Hello-World", 1).unwrap(), None);
        assert_eq!(
            snapshots.fetched_at_ms("octocat/Hello-World", 1).unwrap(),
            None
        );
    }

    /// Writing a second snapshot replaces the first: the store holds the
    /// *last* state, not a history.
    #[test]
    fn writing_again_replaces_the_snapshot() {
        let store = crate::Store::open_in_memory().unwrap();
        let snapshots = store.snapshots();
        snapshots.write(&sample()).unwrap();
        let mut second = sample();
        second.head_sha = "def456".into();
        second.title = "Renamed".into();
        snapshots.write(&second).unwrap();
        let read = snapshots
            .read("octocat/Hello-World", 4781)
            .unwrap()
            .unwrap();
        assert_eq!(read.head_sha, "def456");
        assert_eq!(read.title, "Renamed");
    }

    /// Thread bookkeeping: a thread the app has seen is remembered with
    /// its timestamp, and a thread it hasn't is absent — the difference
    /// the novelty module turns into a dot.
    #[test]
    fn thread_bookkeeping_remembers_what_it_saw() {
        let store = crate::Store::open_in_memory().unwrap();
        let snapshots = store.snapshots();
        assert!(snapshots
            .thread_seen("octocat/Hello-World", 4781)
            .unwrap()
            .is_empty());
        let thread = ThreadSeen {
            id: "T_1".into(),
            updated_at: "2026-10-20T08:07:08Z".into(),
            resolved: false,
        };
        snapshots
            .mark_thread_seen("octocat/Hello-World", 4781, &thread)
            .unwrap();
        let seen = snapshots.thread_seen("octocat/Hello-World", 4781).unwrap();
        assert_eq!(seen, vec![thread.clone()]);
        // Marking it again with a later timestamp updates it in place.
        let later = ThreadSeen {
            id: "T_1".into(),
            updated_at: "2026-10-21T08:07:08Z".into(),
            resolved: true,
        };
        snapshots
            .mark_thread_seen("octocat/Hello-World", 4781, &later)
            .unwrap();
        let seen = snapshots.thread_seen("octocat/Hello-World", 4781).unwrap();
        assert_eq!(seen, vec![later]);
    }

    /// The bookkeeping is per PR: two PRs in the same repo never see each
    /// other's threads.
    #[test]
    fn thread_bookkeeping_is_per_pr() {
        let store = crate::Store::open_in_memory().unwrap();
        let snapshots = store.snapshots();
        let thread = ThreadSeen {
            id: "T_1".into(),
            updated_at: "2026-10-20T08:07:08Z".into(),
            resolved: false,
        };
        snapshots
            .mark_thread_seen("octocat/Hello-World", 1, &thread)
            .unwrap();
        assert!(snapshots
            .thread_seen("octocat/Hello-World", 2)
            .unwrap()
            .is_empty());
        assert!(snapshots.thread_seen("other/repo", 1).unwrap().is_empty());
    }

    /// Forgetting a PR takes its snapshot and its threads with it.
    #[test]
    fn forgetting_a_pr_takes_its_bookkeeping() {
        let store = crate::Store::open_in_memory().unwrap();
        let snapshots = store.snapshots();
        snapshots.write(&sample()).unwrap();
        snapshots
            .mark_thread_seen(
                "octocat/Hello-World",
                4781,
                &ThreadSeen {
                    id: "T_1".into(),
                    updated_at: "2026-10-20T08:07:08Z".into(),
                    resolved: false,
                },
            )
            .unwrap();
        snapshots.forget("octocat/Hello-World", 4781).unwrap();
        assert_eq!(snapshots.read("octocat/Hello-World", 4781).unwrap(), None);
        assert!(snapshots
            .thread_seen("octocat/Hello-World", 4781)
            .unwrap()
            .is_empty());
    }
}
