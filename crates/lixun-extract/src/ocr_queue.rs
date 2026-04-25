//! Persistent SQLite-backed queue for deferred OCR jobs (OCR-T5).
//!
//! The queue carries work from the main indexing phase, which enqueues
//! scan PDFs and images whose text-layer extraction returned empty
//! (DB-13), to the idle-gated OCR worker (T6) that drains it serially
//! and upserts the OCR'd body back into the index (DB-11).
//!
//! Persistence is load-bearing: the user's explicit requirement is
//! that OCR survives daemon restarts. Each row captures the path's
//! mtime/size at enqueue time so the worker (or anyone watching this
//! queue from another tool) can detect an in-flight file change
//! before overwriting a freshly-extracted body.
//!
//! # Connection policy
//!
//! Production callers pass a filesystem path via [`OcrQueue::open`].
//! Every method opens a fresh [`rusqlite::Connection`] via
//! `open_with_flags` — no shared handle, no pool. This matches the
//! style of the existing gloda source and keeps the queue usable
//! from multiple threads without extra locking: SQLite's own WAL
//! mode plus a busy-retry wrapper handle concurrency.
//!
//! Tests open `:memory:` databases via [`OcrQueue::from_connection`],
//! which holds a single live `Connection` behind a `Mutex`. A
//! per-call reopen would give every method its own empty in-memory
//! DB (R16), which is why the test entry point is separate.
//!
//! # Corruption recovery
//!
//! If the on-disk file fails to open with `SQLITE_CORRUPT`,
//! [`OcrQueue::open`] renames the offending file to
//! `{path}.corrupt-<unix-secs>` and creates a fresh DB. Pending rows
//! are lost; the index's next pass will re-enqueue OCR candidates.

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OpenFlags};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// One row in the OCR queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OcrQueueRow {
    pub doc_id: String,
    pub path: String,
    pub mtime: i64,
    pub size: u64,
    pub ext: String,
    pub enqueued_at: i64,
    pub attempts: u32,
    pub last_error: Option<String>,
}

impl OcrQueueRow {
    /// Construct a fresh row with `attempts = 0`, `last_error = None`,
    /// and `enqueued_at` set to the current wall-clock second.
    /// Callers supplying historical timestamps (e.g. migration code)
    /// should set the public fields directly after construction.
    pub fn new(
        doc_id: impl Into<String>,
        path: impl Into<String>,
        mtime: i64,
        size: u64,
        ext: impl Into<String>,
    ) -> Self {
        Self {
            doc_id: doc_id.into(),
            path: path.into(),
            mtime,
            size,
            ext: ext.into(),
            enqueued_at: now_secs(),
            attempts: 0,
            last_error: None,
        }
    }
}

/// Persistent OCR job queue.
///
/// Instances are cheap to clone if needed (clone the path, not the
/// connection). See the module docs for the connection policy.
pub struct OcrQueue {
    /// Production path. `None` when the queue was constructed via
    /// [`OcrQueue::from_connection`] for testing.
    path: Option<PathBuf>,
    /// Test-only single connection (for `:memory:`). Guarded by a
    /// mutex because `Connection` is `!Sync`.
    test_conn: Option<Mutex<Connection>>,
}

impl OcrQueue {
    /// Open or create the on-disk queue at `path`. Applies WAL mode,
    /// `synchronous = NORMAL`, creates the schema on first use, and
    /// recovers from a corrupt existing file by renaming it aside
    /// (see module docs).
    pub fn open(path: PathBuf) -> Result<Self> {
        let conn = open_initialized(&path)?;
        drop(conn);
        Ok(Self {
            path: Some(path),
            test_conn: None,
        })
    }

    /// Test-only entry point. Takes ownership of an already-opened
    /// connection (typically `Connection::open_in_memory()`), applies
    /// pragmas, and creates the schema. The connection is held for
    /// the lifetime of the returned `OcrQueue` — required because
    /// `:memory:` reopens give empty databases (R16).
    pub fn from_connection(conn: Connection) -> Result<Self> {
        apply_pragmas(&conn)?;
        init_schema(&conn)?;
        Ok(Self {
            path: None,
            test_conn: Some(Mutex::new(conn)),
        })
    }

    /// Idempotent enqueue. `INSERT OR IGNORE` — if the `doc_id` is
    /// already queued, the existing row is preserved (attempts,
    /// last_error, enqueued_at untouched). This is what lets the
    /// DB-13 short-circuit logic work correctly: a repeated
    /// enqueue from a rescan doesn't reset retry state.
    pub fn enqueue(&self, row: OcrQueueRow) -> Result<()> {
        self.with_conn(|conn| {
            with_busy_retry(|| {
                conn.execute(
                    "INSERT OR IGNORE INTO ocr_queue
                        (doc_id, path, mtime, size, ext, enqueued_at, attempts, last_error)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        row.doc_id,
                        row.path,
                        row.mtime,
                        row.size as i64,
                        row.ext,
                        row.enqueued_at,
                        row.attempts as i64,
                        row.last_error,
                    ],
                )?;
                Ok(())
            })
        })
        .context("ocr queue: enqueue failed")
    }

    /// Return the next pending row whose `attempts < max_attempts`,
    /// ordered by `(attempts ASC, enqueued_at ASC)` so that freshly
    /// enqueued rows drain first and rows that failed a lot are
    /// considered only after nobody else is waiting. Returns
    /// `Ok(None)` if nothing is pending.
    pub fn peek_next(&self, max_attempts: u32) -> Result<Option<OcrQueueRow>> {
        self.with_conn(|conn| {
            with_busy_retry(|| {
                let mut stmt = conn.prepare(
                    "SELECT doc_id, path, mtime, size, ext, enqueued_at, attempts, last_error
                     FROM ocr_queue
                     WHERE attempts < ?1
                     ORDER BY attempts ASC, enqueued_at ASC
                     LIMIT 1",
                )?;
                let mut rows = stmt.query(params![max_attempts as i64])?;
                if let Some(r) = rows.next()? {
                    Ok(Some(row_from_sql(r)?))
                } else {
                    Ok(None)
                }
            })
        })
        .context("ocr queue: peek_next failed")
    }

    /// Delete the row with the given `doc_id`. No-op if nothing
    /// matches. The worker calls this after a successful OCR + upsert.
    pub fn remove(&self, doc_id: &str) -> Result<()> {
        self.with_conn(|conn| {
            with_busy_retry(|| {
                conn.execute("DELETE FROM ocr_queue WHERE doc_id = ?1", params![doc_id])?;
                Ok(())
            })
        })
        .context("ocr queue: remove failed")
    }

    /// Increment the attempts counter and record the last error
    /// message. Called by the worker when OCR fails or returns
    /// `Ok(None)` (below min-side, unreadable, etc). Rows are not
    /// auto-deleted on failure; T9 cache sweep reaps zombies that
    /// hit max_attempts more than 30 days ago.
    pub fn mark_failure(&self, doc_id: &str, err: &str) -> Result<()> {
        self.with_conn(|conn| {
            with_busy_retry(|| {
                conn.execute(
                    "UPDATE ocr_queue
                     SET attempts = attempts + 1, last_error = ?2
                     WHERE doc_id = ?1",
                    params![doc_id, err],
                )?;
                Ok(())
            })
        })
        .context("ocr queue: mark_failure failed")
    }

    /// Delete rows whose attempts counter has exhausted the worker's
    /// retry budget (`attempts >= max_attempts`) and whose
    /// `enqueued_at` is older than `older_than_secs` (Unix seconds).
    /// Returns the number of rows deleted.
    ///
    /// Called from the cache-sweep tick (T9) to bound queue growth
    /// under permanent-failure conditions while preserving a grace
    /// window during which operators can still inspect `last_error`
    /// via `sqlite3`. The worker treats a row with `attempts >=
    /// max_attempts` as invisible (see [`peek_next`]), so reaping
    /// does not race with in-flight OCR.
    pub fn reap_zombies(&self, max_attempts: u32, older_than_secs: i64) -> Result<u64> {
        self.with_conn(|conn| {
            with_busy_retry(|| {
                let affected = conn.execute(
                    "DELETE FROM ocr_queue
                     WHERE attempts >= ?1 AND enqueued_at < ?2",
                    params![max_attempts as i64, older_than_secs],
                )?;
                Ok(affected as u64)
            })
        })
        .context("ocr queue: reap_zombies failed")
    }

    /// `true` iff the queue is empty.
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Row count including rows whose attempts are exhausted.
    /// Useful for diagnostics and queue-depth metrics.
    pub fn len(&self) -> Result<u64> {
        self.with_conn(|conn| {
            with_busy_retry(|| {
                let n: i64 = conn.query_row("SELECT COUNT(*) FROM ocr_queue", [], |r| r.get(0))?;
                Ok(n as u64)
            })
        })
        .context("ocr queue: len failed")
    }

    /// Internal helper: pick between the persistent `path` (open a
    /// fresh connection) or the test-held connection.
    fn with_conn<T>(
        &self,
        f: impl FnOnce(&Connection) -> rusqlite::Result<T>,
    ) -> rusqlite::Result<T> {
        if let Some(ref m) = self.test_conn {
            let guard = m.lock().expect("ocr queue test mutex poisoned");
            f(&guard)
        } else {
            let path = self
                .path
                .as_ref()
                .expect("OcrQueue: no path and no test connection");
            let conn = open_with_flags(path)?;
            f(&conn)
        }
    }
}

fn open_with_flags(path: &Path) -> rusqlite::Result<Connection> {
    with_busy_retry(|| {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        // Busy timeout inside SQLite itself complements our
        // application-level retry by waiting out lock contention
        // during a single statement without returning BUSY.
        conn.busy_timeout(Duration::from_millis(250))?;
        Ok(conn)
    })
}

/// Open + prepare the on-disk queue with corruption recovery.
/// SQLite accepts garbage files at `open_with_flags` time — corruption
/// only surfaces once we touch the header via a PRAGMA or a DDL
/// statement. This helper runs the full init sequence
/// (`open_with_flags` → `apply_pragmas` → `init_schema`) and, on any
/// step failing with `NotADatabase` or `DatabaseCorrupt`, renames the
/// offending file aside and retries once on a fresh DB. Any other
/// failure propagates up with context.
fn open_initialized(path: &Path) -> Result<Connection> {
    match try_open_and_init(path) {
        Ok(conn) => Ok(conn),
        Err(e) if has_corrupt_cause(&e) => {
            let backup = corrupt_backup_path(path);
            tracing::warn!(
                target: "lixun_extract::ocr_queue",
                "ocr-queue file at {} is corrupt; renaming to {} and recreating",
                path.display(),
                backup.display()
            );
            std::fs::rename(path, &backup).with_context(|| {
                format!(
                    "ocr queue: renaming corrupt file {} aside failed",
                    path.display()
                )
            })?;
            try_open_and_init(path).with_context(|| {
                format!(
                    "ocr queue: reopening {} after corruption failed",
                    path.display()
                )
            })
        }
        Err(e) => Err(e).with_context(|| format!("ocr queue: opening {} failed", path.display())),
    }
}

fn try_open_and_init(path: &Path) -> Result<Connection> {
    let conn = open_with_flags(path).context("open_with_flags")?;
    apply_pragmas(&conn)?;
    init_schema(&conn)?;
    Ok(conn)
}

fn has_corrupt_cause(err: &anyhow::Error) -> bool {
    err.chain()
        .filter_map(|e| e.downcast_ref::<rusqlite::Error>())
        .any(is_corrupt)
}

fn corrupt_backup_path(path: &Path) -> PathBuf {
    let ts = now_secs();
    let mut s = path.as_os_str().to_os_string();
    s.push(format!(".corrupt-{ts}"));
    PathBuf::from(s)
}

fn is_corrupt(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(ffi, _)
            if ffi.code == rusqlite::ErrorCode::DatabaseCorrupt
                || ffi.code == rusqlite::ErrorCode::NotADatabase
    )
}

fn apply_pragmas(conn: &Connection) -> Result<()> {
    // WAL makes concurrent readers and a single writer coexist
    // without blocking each other; synchronous=NORMAL is the
    // documented safe choice with WAL.
    conn.pragma_update(None, "journal_mode", "WAL")
        .context("ocr queue: PRAGMA journal_mode=WAL failed")?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .context("ocr queue: PRAGMA synchronous=NORMAL failed")?;
    Ok(())
}

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS ocr_queue (
            doc_id      TEXT PRIMARY KEY,
            path        TEXT NOT NULL,
            mtime       INTEGER NOT NULL,
            size        INTEGER NOT NULL,
            ext         TEXT NOT NULL,
            enqueued_at INTEGER NOT NULL,
            attempts    INTEGER NOT NULL DEFAULT 0,
            last_error  TEXT
        );
        CREATE INDEX IF NOT EXISTS ocr_queue_attempts_idx
            ON ocr_queue(attempts, enqueued_at);",
    )
    .context("ocr queue: schema init failed")?;
    Ok(())
}

fn row_from_sql(r: &rusqlite::Row<'_>) -> rusqlite::Result<OcrQueueRow> {
    let size_i: i64 = r.get(3)?;
    let attempts_i: i64 = r.get(6)?;
    Ok(OcrQueueRow {
        doc_id: r.get(0)?,
        path: r.get(1)?,
        mtime: r.get(2)?,
        size: size_i.max(0) as u64,
        ext: r.get(4)?,
        enqueued_at: r.get(5)?,
        attempts: attempts_i.max(0) as u32,
        last_error: r.get(7)?,
    })
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Retry a fallible rusqlite op up to 10 times on BUSY/LOCKED with
/// bounded backoff. Worst-case wait ≈ 1.3 s, which is well inside
/// the daemon's per-op budget but long enough to absorb typical
/// writer-vs-reader contention from the CLI's status query and the
/// watcher thread re-enqueuing a file during an active drain.
fn with_busy_retry<T, F>(mut f: F) -> rusqlite::Result<T>
where
    F: FnMut() -> rusqlite::Result<T>,
{
    // 9 sleeps + the initial attempt = 10 tries. Hand-tuned ramp
    // starting short (25ms) to handle the common case of a writer
    // holding the lock for a few ms, capped at 250ms so we never
    // block a single op for more than ~250ms between tries.
    const BACKOFFS: [Duration; 9] = [
        Duration::from_millis(25),
        Duration::from_millis(50),
        Duration::from_millis(100),
        Duration::from_millis(150),
        Duration::from_millis(200),
        Duration::from_millis(250),
        Duration::from_millis(250),
        Duration::from_millis(250),
        Duration::from_millis(250),
    ];
    let mut last_err = None;
    let schedule = std::iter::once(None).chain(BACKOFFS.iter().map(Some));
    for sleep_after in schedule {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) => {
                if !is_busy(&e) {
                    return Err(e);
                }
                last_err = Some(e);
                if let Some(d) = sleep_after {
                    std::thread::sleep(*d);
                }
            }
        }
    }
    Err(last_err.unwrap())
}

fn is_busy(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(ffi, _)
            if ffi.code == rusqlite::ErrorCode::DatabaseBusy
                || ffi.code == rusqlite::ErrorCode::DatabaseLocked
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn mem_queue() -> OcrQueue {
        let conn = Connection::open_in_memory().expect("open_in_memory failed");
        OcrQueue::from_connection(conn).expect("from_connection failed")
    }

    fn sample_row(doc_id: &str) -> OcrQueueRow {
        OcrQueueRow::new(
            doc_id,
            format!("/tmp/{doc_id}.pdf"),
            1_700_000_000,
            4096,
            "pdf",
        )
    }

    #[test]
    fn enqueue_peek_remove_roundtrip() {
        let q = mem_queue();
        assert_eq!(q.len().unwrap(), 0);
        q.enqueue(sample_row("doc-a")).unwrap();
        assert_eq!(q.len().unwrap(), 1);
        let row = q.peek_next(3).unwrap().expect("row should be present");
        assert_eq!(row.doc_id, "doc-a");
        assert_eq!(row.path, "/tmp/doc-a.pdf");
        assert_eq!(row.mtime, 1_700_000_000);
        assert_eq!(row.size, 4096);
        assert_eq!(row.ext, "pdf");
        assert_eq!(row.attempts, 0);
        assert_eq!(row.last_error, None);
        q.remove("doc-a").unwrap();
        assert!(q.peek_next(3).unwrap().is_none());
        assert_eq!(q.len().unwrap(), 0);
    }

    #[test]
    fn enqueue_duplicate_doc_id_is_idempotent_and_preserves_attempts() {
        let q = mem_queue();
        q.enqueue(sample_row("dup")).unwrap();
        q.mark_failure("dup", "boom").unwrap();
        q.mark_failure("dup", "boom-again").unwrap();

        let mut second = sample_row("dup");
        second.attempts = 0;
        second.last_error = None;
        q.enqueue(second).unwrap();

        assert_eq!(q.len().unwrap(), 1);
        let row = q.peek_next(10).unwrap().expect("row should still be there");
        assert_eq!(row.attempts, 2);
        assert_eq!(row.last_error.as_deref(), Some("boom-again"));
    }

    #[test]
    fn mark_failure_increments_attempts_and_respects_max_attempts() {
        let q = mem_queue();
        q.enqueue(sample_row("flaky")).unwrap();
        q.mark_failure("flaky", "e1").unwrap();
        q.mark_failure("flaky", "e2").unwrap();
        q.mark_failure("flaky", "e3").unwrap();

        assert!(q.peek_next(3).unwrap().is_none());
        assert_eq!(q.len().unwrap(), 1);
        let row = q.peek_next(10).unwrap().expect("still present");
        assert_eq!(row.attempts, 3);
    }

    #[test]
    fn peek_next_orders_by_enqueued_at_asc() {
        let q = mem_queue();
        let mut a = sample_row("older");
        a.enqueued_at = 100;
        let mut b = sample_row("newer");
        b.enqueued_at = 200;
        q.enqueue(b).unwrap();
        q.enqueue(a).unwrap();
        let row = q.peek_next(3).unwrap().unwrap();
        assert_eq!(row.doc_id, "older");
    }

    #[test]
    fn survive_restart() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ocr-queue.db");

        let q1 = OcrQueue::open(path.clone()).unwrap();
        q1.enqueue(sample_row("persisted-a")).unwrap();
        q1.enqueue(sample_row("persisted-b")).unwrap();
        drop(q1);

        let q2 = OcrQueue::open(path).unwrap();
        assert_eq!(q2.len().unwrap(), 2);
        let row = q2.peek_next(3).unwrap().unwrap();
        assert!(row.doc_id.starts_with("persisted-"));
    }

    #[test]
    fn corrupt_file_renamed_and_fresh_db_created() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ocr-queue.db");
        fs::write(&path, b"this is not a sqlite database at all").unwrap();

        let q = OcrQueue::open(path.clone()).expect("open must recover from corrupt file");
        assert_eq!(q.len().unwrap(), 0);

        let entries: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            entries
                .iter()
                .any(|n| n.starts_with("ocr-queue.db.corrupt-")),
            "expected a .corrupt-<ts> sibling, got entries: {entries:?}"
        );

        q.enqueue(sample_row("fresh")).unwrap();
        assert_eq!(q.len().unwrap(), 1);
    }

    #[test]
    fn remove_is_noop_for_unknown_doc_id() {
        let q = mem_queue();
        q.remove("nope").unwrap();
        assert_eq!(q.len().unwrap(), 0);
    }

    #[test]
    fn mark_failure_is_noop_for_unknown_doc_id() {
        let q = mem_queue();
        q.mark_failure("nope", "ignored").unwrap();
        assert_eq!(q.len().unwrap(), 0);
    }

    #[test]
    fn reap_zombies_deletes_old_high_attempt_rows() {
        let q = mem_queue();
        let now = now_secs();
        let thirty_days = 30 * 86_400;

        let mut fresh_no_fail = sample_row("fresh-no-fail");
        fresh_no_fail.attempts = 0;
        fresh_no_fail.enqueued_at = now - 40 * 86_400;
        q.enqueue(fresh_no_fail).unwrap();

        let mut old_exhausted = sample_row("old-exhausted");
        old_exhausted.attempts = 3;
        old_exhausted.enqueued_at = now - 40 * 86_400;
        q.enqueue(old_exhausted).unwrap();

        let mut recent_exhausted = sample_row("recent-exhausted");
        recent_exhausted.attempts = 3;
        recent_exhausted.enqueued_at = now - 25 * 86_400;
        q.enqueue(recent_exhausted).unwrap();

        let cutoff = now - thirty_days;
        let deleted = q.reap_zombies(3, cutoff).unwrap();
        assert_eq!(deleted, 1, "only old-exhausted should be reaped");
        assert_eq!(q.len().unwrap(), 2);

        let row = q.peek_next(10).unwrap().expect("at least one row remains");
        assert_ne!(row.doc_id, "old-exhausted");
    }
}
