//! Query-history log — persists recent search queries for the GUI's
//! "Up-arrow in empty entry" history dropdown.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::Path;

const DEFAULT_MAX: usize = 100;
const FILE_NAME: &str = "queries.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryLog {
    entries: VecDeque<String>,
    max: usize,
}

impl Default for QueryLog {
    fn default() -> Self {
        Self {
            entries: VecDeque::with_capacity(DEFAULT_MAX),
            max: DEFAULT_MAX,
        }
    }
}

impl QueryLog {
    pub fn load(state_dir: &Path) -> Result<Self> {
        let path = state_dir.join(FILE_NAME);
        if !path.exists() {
            return Ok(Self::default());
        }
        let body = std::fs::read_to_string(&path)?;
        let mut log: QueryLog = serde_json::from_str(&body).unwrap_or_default();
        if log.max == 0 {
            log.max = DEFAULT_MAX;
        }
        Ok(log)
    }

    pub fn save(&self, state_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(state_dir)?;
        let path = state_dir.join(FILE_NAME);
        let temporary = state_dir.join("queries.json.tmp");
        std::fs::write(&temporary, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(temporary, path)?;
        Ok(())
    }

    pub fn remove_excluded(&mut self, excludes: impl Fn(&str) -> bool) -> usize {
        let before = self.entries.len();
        self.entries.retain(|query| !excludes(query));
        before - self.entries.len()
    }

    pub fn record_query(&mut self, q: &str) {
        let q = q.trim();
        if q.is_empty() {
            return;
        }
        self.entries.retain(|e| e != q);
        self.entries.push_front(q.to_string());
        while self.entries.len() > self.max {
            self.entries.pop_back();
        }
    }

    pub fn recent(&self, n: usize) -> Vec<String> {
        self.entries.iter().take(n).cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_and_recent() {
        let mut log = QueryLog::default();
        log.record_query("foo");
        log.record_query("bar");
        log.record_query("baz");
        let recent = log.recent(10);
        assert_eq!(recent, vec!["baz", "bar", "foo"]);
    }

    #[test]
    fn deduplicates_move_to_front() {
        let mut log = QueryLog::default();
        log.record_query("foo");
        log.record_query("bar");
        log.record_query("foo");
        let recent = log.recent(10);
        assert_eq!(recent, vec!["foo", "bar"]);
    }

    #[test]
    fn truncates_to_max() {
        let mut log = QueryLog {
            entries: VecDeque::new(),
            max: 3,
        };
        for i in 0..10 {
            log.record_query(&format!("q{i}"));
        }
        let recent = log.recent(100);
        assert_eq!(recent.len(), 3);
        assert_eq!(recent, vec!["q9", "q8", "q7"]);
    }

    #[test]
    fn skips_empty_and_whitespace() {
        let mut log = QueryLog::default();
        log.record_query("");
        log.record_query("   ");
        log.record_query("\t\n");
        assert_eq!(log.recent(10).len(), 0);
    }

    #[test]
    fn trims_whitespace() {
        let mut log = QueryLog::default();
        log.record_query("  foo  ");
        assert_eq!(log.recent(10), vec!["foo"]);
    }

    #[test]
    fn recent_respects_n() {
        let mut log = QueryLog::default();
        for i in 0..5 {
            log.record_query(&format!("q{i}"));
        }
        let recent = log.recent(3);
        assert_eq!(recent, vec!["q4", "q3", "q2"]);
    }

    #[test]
    fn save_and_load_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log = QueryLog::default();
        log.record_query("alpha");
        log.record_query("beta");
        log.save(tmp.path()).unwrap();

        let loaded = QueryLog::load(tmp.path()).unwrap();
        assert_eq!(loaded.recent(10), vec!["beta", "alpha"]);
    }

    #[test]
    fn load_missing_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let log = QueryLog::load(tmp.path()).unwrap();
        assert_eq!(log.recent(10).len(), 0);
    }
}
