use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::Connection;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS progress (
    doc_id      TEXT PRIMARY KEY,
    channel     TEXT NOT NULL,
    embedded_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS backfill_meta (
    k TEXT PRIMARY KEY,
    v TEXT NOT NULL
);
";

pub struct BackfillJournal {
    conn: Connection,
}

impl BackfillJournal {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating journal dir {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening backfill journal at {}", path.display()))?;
        conn.execute_batch(SCHEMA)
            .context("initializing backfill journal schema")?;
        Ok(Self { conn })
    }

    pub fn record(&mut self, doc_id: &str, channel: &str, embedded_at: i64) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO progress (doc_id, channel, embedded_at) \
                 VALUES (?1, ?2, ?3)",
                rusqlite::params![doc_id, channel, embedded_at],
            )
            .context("backfill journal: record progress")?;
        Ok(())
    }

    pub fn forget(&mut self, doc_id: &str) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM progress WHERE doc_id = ?1",
                rusqlite::params![doc_id],
            )
            .context("backfill journal: forget doc")?;
        Ok(())
    }

    pub fn was_embedded(&self, doc_id: &str, channel: &str) -> Result<bool> {
        let n: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM progress WHERE doc_id = ?1 AND channel = ?2",
                rusqlite::params![doc_id, channel],
                |row| row.get(0),
            )
            .context("backfill journal: was_embedded query")?;
        Ok(n > 0)
    }

    pub fn meta_get(&self, key: &str) -> Result<Option<String>> {
        match self.conn.query_row(
            "SELECT v FROM backfill_meta WHERE k = ?1",
            rusqlite::params![key],
            |row| row.get::<_, String>(0),
        ) {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e).context("backfill journal: meta_get"),
        }
    }

    pub fn meta_set(&mut self, key: &str, value: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO backfill_meta (k, v) VALUES (?1, ?2)",
                rusqlite::params![key, value],
            )
            .context("backfill journal: meta_set")?;
        Ok(())
    }
}

/// Default location: `$XDG_STATE_HOME/lixun/semantic-backfill.sqlite`,
/// falling back to `$XDG_DATA_HOME/state/lixun/...` on platforms where
/// `dirs::state_dir()` returns `None` (notably non-Linux).
pub fn default_journal_path() -> Result<PathBuf> {
    let state = dirs::state_dir().or_else(|| dirs::data_local_dir().map(|p| p.join("state")));
    let base = state.context("XDG state directory unavailable")?;
    Ok(base.join("lixun").join("semantic-backfill.sqlite"))
}
