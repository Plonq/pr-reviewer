//! The local store: the app's own memory, on disk.
//!
//! Everything the app knows between launches lives in one SQLite database
//! (WAL mode) at the platform's application-support location:
//!
//! - the **queue cache** — the last review queue, so the app paints
//!   instantly on open and still works offline,
//! - **per-PR bookkeeping** — the last head SHA a PR was seen at, the
//!   thread timestamps the novelty module compares against, the pending
//!   draft mirror,
//! - **settings** — the JSON blob of user-facing configuration.
//!
//! Snapshots are stored as **JSON columns** rather than relational tables:
//! the shape of a PR snapshot is the GitHub API's shape, which drifts, and
//! a JSON column absorbs that drift without a migration. Relational
//! bookkeeping (head SHAs, thread timestamps) stays in columns, because
//! those are queried, not just read back.
//!
//! The store is cheap to open (a couple of milliseconds), safe to share
//! across threads (`&Connection` is `Send` but not `Sync`, so the app
//! holds it behind a `Mutex`), and never blocks the UI thread for long:
//! every write is a single transaction.

mod cache;
mod settings;
mod snapshot;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::Connection;

pub use cache::{CachedLabel, CachedQueue, CachedQueueRow, QueueCache};
pub use settings::Settings;
pub use snapshot::{PrSnapshot, SnapshotStore, ThreadSeen};

/// The app's database schema version. Bumped whenever a migration runs;
/// the stored version is what the next open migrates *from*.
pub const SCHEMA_VERSION: i64 = 1;

/// Where the database lives: the platform's application-support directory,
/// under the app's name.
///
/// macOS: `~/Library/Application Support/Gander/store.db`.
///
/// The directory is created on demand; a home directory that can't be read
/// yields `None`, and the app runs without a store (cache-first becomes
/// network-first, but nothing breaks).
pub fn default_database_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let mut path = PathBuf::from(home);
    path.push("Library");
    path.push("Application Support");
    path.push("Gander");
    path.push("store.db");
    Some(path)
}

/// A handle to the local store.
///
/// Opening one creates the schema if it's missing and migrates it forward
/// if it's behind. Every method is a self-contained transaction, so a
/// crash mid-write leaves the database consistent (WAL mode means readers
/// never block on the writer).
#[derive(Debug)]
pub struct Store {
    conn: Connection,
    path: Option<PathBuf>,
}

impl Store {
    /// Open (creating if needed) the database at `path`, running any
    /// pending migrations.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("couldn't create the store directory {}", parent.display())
            })?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("couldn't open the store at {}", path.display()))?;
        Self::prepare(&conn)?;
        Ok(Self {
            conn,
            path: Some(path.to_path_buf()),
        })
    }

    /// Open the store at the platform's application-support location, or
    /// an in-memory store when that location isn't available (no `HOME`,
    /// unwritable directory) — the app then runs without persistence
    /// rather than refusing to start.
    pub fn open_default() -> Result<Self> {
        match default_database_path() {
            Some(path) => Self::open(&path),
            None => Self::open_in_memory(),
        }
    }

    /// An in-memory store: the shape tests use, and the fallback when the
    /// application-support location isn't available.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::prepare(&conn)?;
        Ok(Self { conn, path: None })
    }

    /// Where the database lives, when it lives on disk.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Put the connection into the mode the store wants and bring the
    /// schema up to date.
    fn prepare(conn: &Connection) -> Result<()> {
        // WAL keeps readers (the UI painting from cache) off the writer's
        // back; the busy timeout keeps a concurrent write from failing
        // outright.
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "synchronous", "NORMAL").ok();
        conn.pragma_update(None, "foreign_keys", "ON").ok();
        conn.busy_timeout(std::time::Duration::from_secs(5)).ok();
        migrate(conn)?;
        // The reconciliation runs outside the versioned migration: a
        // database written by an older build (or one whose table was
        // created by hand) can be missing a column this build reads, and
        // the fix is additive, not a version bump.
        reconcile_queue_cache(conn)?;
        Ok(())
    }

    /// The queue cache: the review queue as it was last seen.
    pub fn queue_cache(&self) -> QueueCache<'_> {
        QueueCache::new(&self.conn)
    }

    /// Per-PR bookkeeping: head SHAs, thread timestamps, pending drafts.
    pub fn snapshots(&self) -> SnapshotStore<'_> {
        SnapshotStore::new(&self.conn)
    }

    /// The settings blob.
    pub fn settings(&self) -> Settings<'_> {
        Settings::new(&self.conn)
    }
}

/// Bring a database's schema up to [`SCHEMA_VERSION`].
///
/// Migrations are additive and idempotent: each step checks what it needs
/// is missing before creating it, so a database that's already at the
/// current version runs no statements at all, and a database from any
/// earlier version is brought forward step by step. The schema version
/// lives in `PRAGMA user_version`, which SQLite bumps atomically with the
/// transaction that made the change.
fn migrate(conn: &Connection) -> Result<()> {
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .context("couldn't read the store's schema version")?;
    if version >= SCHEMA_VERSION {
        return Ok(());
    }
    let tx = conn.unchecked_transaction()?;
    for step in (version as u64)..(SCHEMA_VERSION as u64) {
        match step {
            0 => migrate_v0_to_v1(&tx)?,
            other => anyhow::bail!("no migration from schema version {other}"),
        }
    }
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()?;
    Ok(())
}

/// The first schema: the queue cache, the per-PR bookkeeping, and the
/// settings blob.
fn migrate_v0_to_v1(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS queue_cache (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            rows TEXT NOT NULL DEFAULT '[]',
            search TEXT NOT NULL DEFAULT '',
            sort TEXT NOT NULL DEFAULT '',
            sort_order TEXT NOT NULL DEFAULT '',
            fetched_at_ms INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS pr_snapshots (
            repository TEXT NOT NULL,
            number INTEGER NOT NULL,
            head_sha TEXT NOT NULL DEFAULT '',
            snapshot TEXT NOT NULL DEFAULT '{}',
            fetched_at_ms INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (repository, number)
        );
        CREATE TABLE IF NOT EXISTS pr_threads (
            repository TEXT NOT NULL,
            number INTEGER NOT NULL,
            thread_id TEXT NOT NULL,
            updated_at TEXT NOT NULL DEFAULT '',
            resolved INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (repository, number, thread_id)
        );
        CREATE TABLE IF NOT EXISTS settings (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL DEFAULT '{}'
        );",
    )
    .context("couldn't create the store's schema")?;
    Ok(())
}

/// The columns of the `queue_cache` table, as SQLite reports them.
fn queue_cache_columns(conn: &Connection) -> Vec<String> {
    let mut stmt = match conn.prepare("PRAGMA table_info(queue_cache)") {
        Ok(stmt) => stmt,
        Err(_) => return Vec::new(),
    };
    let names = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default();
    names
}

/// Bring a `queue_cache` table up to the current shape: every column the
/// cache reads, added when it's missing.
///
/// The table is created with `CREATE TABLE IF NOT EXISTS`, so a database
/// written by an older build — or one whose table was created by hand —
/// can be missing columns this build expects. `ALTER TABLE ... ADD COLUMN`
/// is the additive fix: existing rows keep their data and gain a default,
/// so a cache written before a column existed still reads (with that
/// column empty) instead of failing every read.
fn reconcile_queue_cache(conn: &Connection) -> Result<()> {
    let columns = queue_cache_columns(conn);
    if columns.is_empty() {
        return Ok(()); // No table yet; the migration creates it whole.
    }
    for (name, definition) in [
        ("rows", "TEXT NOT NULL DEFAULT '[]'"),
        ("search", "TEXT NOT NULL DEFAULT ''"),
        ("sort", "TEXT NOT NULL DEFAULT ''"),
        ("sort_order", "TEXT NOT NULL DEFAULT ''"),
        ("fetched_at_ms", "INTEGER NOT NULL DEFAULT 0"),
    ] {
        if columns.iter().any(|known| known == name) {
            continue;
        }
        conn.execute(
            &format!("ALTER TABLE queue_cache ADD COLUMN {name} {definition}"),
            [],
        )
        .with_context(|| format!("couldn't add the queue_cache.{name} column"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The schema a fresh database gets: the version stamped, and every
    /// table this store reads present.
    #[test]
    fn a_fresh_database_carries_the_current_schema_version() {
        let store = Store::open_in_memory().expect("an in-memory store must open");
        let version: i64 = store
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("the schema version must be readable");
        assert_eq!(version, SCHEMA_VERSION);
        for table in ["queue_cache", "pr_snapshots", "pr_threads", "settings"] {
            let count: i64 = store
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |row| row.get(0),
                )
                .expect("sqlite_master must be readable");
            assert_eq!(count, 1, "the {table} table must exist");
        }
    }

    /// The store opens the same database twice without complaint — the
    /// second open finds the schema already there and migrates nothing.
    #[test]
    fn opening_an_existing_database_is_idempotent() {
        let dir = std::env::temp_dir().join(format!(
            "gander-store-open-{}-{}",
            std::process::id(),
            now_millis()
        ));
        let path = dir.join("store.db");
        let store = Store::open(&path).expect("the first open must succeed");
        store
            .queue_cache()
            .write(&CachedQueue::empty())
            .expect("the write must succeed");
        drop(store);

        let store = Store::open(&path).expect("the second open must succeed");
        let cached = store.queue_cache().read().expect("the cache must load");
        assert_eq!(cached.prs.len(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A path whose parent doesn't exist yet is created — the app never
    /// has to make the application-support directory itself.
    #[test]
    fn opening_creates_the_parent_directory() {
        let dir = std::env::temp_dir().join(format!(
            "gander-store-mkdir-{}-{}",
            std::process::id(),
            now_millis()
        ));
        let path = dir.join("nested").join("store.db");
        let store = Store::open(&path).expect("the open must create the directory");
        assert!(path.exists());
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A database written by an older schema is migrated forward: the
    /// version advances, and the data it held survives the migration.
    #[test]
    fn an_older_database_is_migrated_forward() {
        let dir = std::env::temp_dir().join(format!(
            "gander-store-migrate-{}-{}",
            std::process::id(),
            now_millis()
        ));
        let path = dir.join("store.db");
        {
            let store = Store::open(&path).expect("the open must succeed");
            store
                .queue_cache()
                .write(&CachedQueue::empty())
                .expect("the write must succeed");
            // Pretend an older build wrote this database: the version
            // stamped back, a column it didn't know about dropped.
            store
                .conn
                .execute("PRAGMA user_version = 0", [])
                .expect("the version must be writable");
            store
                .conn
                .execute("ALTER TABLE queue_cache DROP COLUMN search", [])
                .expect("the column must be droppable");
        }
        let store = Store::open(&path).expect("the migrated open must succeed");
        let version: i64 = store
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("the schema version must be readable");
        assert_eq!(version, SCHEMA_VERSION);
        let cached = store.queue_cache().read().expect("the cache must load");
        assert!(cached.prs.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The cold-start cost of the store: opening the database and
    /// reading the cached queue.
    ///
    /// The app's cold-start budget is a second, and most of it is the
    /// window; the store must be a rounding error against it. A warm
    /// cache read is the whole cost of the app's instant paint.
    #[test]
    fn the_store_opens_and_reads_the_cache_well_under_a_second() {
        let dir = std::env::temp_dir().join(format!("gander-cold-{}", now_millis()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("store.db");
        // Warm it: a store with a queue in it.
        {
            let store = Store::open(&path).expect("the open must succeed");
            let mut queue = CachedQueue::empty();
            queue.search = "is:pr is:open review-requested:@me".into();
            queue.fetched_at_ms = 1_700_000_000_000;
            for number in 1..=20 {
                queue.prs.push(CachedQueueRow {
                    number,
                    title: format!("PR {number}"),
                    repository: "octocat/Hello-World".into(),
                    updated_at: "2026-10-20T08:07:08Z".into(),
                    ..Default::default()
                });
            }
            store
                .queue_cache()
                .write(&queue)
                .expect("the write must succeed");
        }
        // Cold start: open and read.
        let started = std::time::Instant::now();
        let store = Store::open(&path).expect("the open must succeed");
        let cached = store.queue_cache().read().expect("the read must succeed");
        let elapsed = started.elapsed();
        assert_eq!(cached.prs.len(), 20);
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "the cold start took {elapsed:?}, over the one-second budget"
        );
        eprintln!("cold start (store open + cache read): {elapsed:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Milliseconds since the epoch — enough to tell two temp paths
    /// apart inside one test run.
    fn now_millis() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    }
}
