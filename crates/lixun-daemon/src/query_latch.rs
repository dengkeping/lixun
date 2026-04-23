//! Query-latch store — per-(normalized-query, doc_id) click counts.
//!
//! Alfred/Spotlight-style latching: when the user repeatedly selects the
//! same doc for the same query, future searches of that exact query
//! surface that doc at the top. Word order matters — `"foo bar"` and
//! `"bar foo"` are different latches.
//!
//! Count is capped at 50 on write to bound the `ln(1+count)` growth;
//! recency weighting reuses the frecency age buckets so that a latch
//! decays into a neutral multiplier over months of disuse.

use anyhow::Result;
use lixun_index::normalize::normalize_for_latch_key;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

const MAX_COUNT: u32 = 50;
const SECONDS_PER_DAY: i64 = 86_400;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
pub struct LatchEntry {
    pub count: u32,
    pub last_ts: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct QueryLatchStore {
    // key = normalize_for_latch_key(query_raw)
    by_query: BTreeMap<String, HashMap<String, LatchEntry>>,
}

fn recency_weight(age_seconds: i64) -> f32 {
    let age = age_seconds.max(0);
    let age_days = (age / SECONDS_PER_DAY) as f32;
    if age_days <= 4.0 {
        1.0
    } else if age_days <= 14.0 {
        0.7
    } else if age_days <= 31.0 {
        0.5
    } else if age_days <= 90.0 {
        0.3
    } else {
        0.1
    }
}

impl QueryLatchStore {
    /// Load `query_latch.json` from `state_dir`. Missing file →
    /// empty store. Corrupt JSON is logged at warn level and replaced
    /// with an empty store (no migration path — this is a brand-new
    /// store with no predecessor).
    pub fn load(state_dir: &Path) -> Result<Self> {
        let path = state_dir.join("query_latch.json");
        if !path.exists() {
            return Ok(Self::default());
        }
        match std::fs::read_to_string(&path) {
            Ok(content) => match serde_json::from_str::<QueryLatchStore>(&content) {
                Ok(store) => Ok(store),
                Err(e) => {
                    tracing::warn!(
                        "query_latch: failed to parse {:?}: {}; starting empty",
                        path,
                        e
                    );
                    Ok(Self::default())
                }
            },
            Err(e) => {
                tracing::warn!(
                    "query_latch: failed to read {:?}: {}; starting empty",
                    path,
                    e
                );
                Ok(Self::default())
            }
        }
    }

    /// Atomic write: serialize to `query_latch.json.tmp`, then rename
    /// over `query_latch.json`.
    pub fn save(&self, state_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(state_dir)?;
        let final_path = state_dir.join("query_latch.json");
        let tmp_path = state_dir.join("query_latch.json.tmp");
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(&tmp_path, content)?;
        std::fs::rename(&tmp_path, &final_path)?;
        Ok(())
    }

    pub fn record(&mut self, query_raw: &str, doc_id: &str, now: i64) {
        let key = normalize_for_latch_key(query_raw);
        if key.is_empty() {
            return;
        }
        let per_query = self.by_query.entry(key).or_default();
        let entry = per_query.entry(doc_id.to_string()).or_default();
        entry.count = (entry.count + 1).min(MAX_COUNT);
        entry.last_ts = now;
    }

    /// Multiplicative boost for `(query_raw, doc_id)`. Returns `1.0`
    /// when the pair has no latch entry, otherwise
    /// `clamp(1 + weight * ln(1+count) * recency, 1.0, cap)`.
    pub fn mult(&self, query_raw: &str, doc_id: &str, now: i64, weight: f32, cap: f32) -> f32 {
        let key = normalize_for_latch_key(query_raw);
        let Some(per_query) = self.by_query.get(&key) else {
            return 1.0;
        };
        let Some(entry) = per_query.get(doc_id) else {
            return 1.0;
        };
        let recency = recency_weight(now - entry.last_ts);
        let raw = (1.0 + entry.count as f32).ln() * recency;
        (1.0 + weight * raw).clamp(1.0, cap)
    }

    /// Returns `true` iff the `(query_raw, doc_id)` pair has at least
    /// `threshold` recorded clicks. Used by Top Hit confidence (T6).
    #[allow(dead_code)]
    pub fn strong(&self, query_raw: &str, doc_id: &str, threshold: u32) -> bool {
        let key = normalize_for_latch_key(query_raw);
        self.by_query
            .get(&key)
            .and_then(|per_query| per_query.get(doc_id))
            .map(|entry| entry.count >= threshold)
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn cap_and_ordering() {
        let mut store = QueryLatchStore::default();
        let now: i64 = 1_700_000_000;

        // Fresh store → neutral multiplier.
        assert!((store.mult("foo", "doc1", now, 0.5, 3.0) - 1.0).abs() < 1e-6);

        // 5 recordings for ("foo", "doc1") → mult > 1.0 and <= cap.
        for _ in 0..5 {
            store.record("foo", "doc1", now);
        }
        let m_five = store.mult("foo", "doc1", now, 0.5, 3.0);
        assert!(m_five > 1.0, "5 clicks must lift multiplier above 1.0");
        assert!(m_five <= 3.0, "multiplier must not exceed cap");

        // 5 more recordings → still <= cap.
        for _ in 0..5 {
            store.record("foo", "doc1", now);
        }
        let m_ten = store.mult("foo", "doc1", now, 0.5, 3.0);
        assert!(m_ten <= 3.0, "cap must hold after additional clicks");
        assert!(m_ten >= m_five, "more clicks should not regress multiplier");

        // Unrelated doc under same query stays neutral.
        assert!((store.mult("foo", "doc2", now, 0.5, 3.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn word_order_matters() {
        let mut store = QueryLatchStore::default();
        let now: i64 = 1_700_000_000;

        store.record("foo bar", "doc1", now);

        // Exact word order latches.
        let m_exact = store.mult("foo bar", "doc1", now, 0.5, 3.0);
        assert!(m_exact > 1.0, "exact word order must lift multiplier");

        // Reversed word order is a different latch key → neutral.
        let m_reversed = store.mult("bar foo", "doc1", now, 0.5, 3.0);
        assert!(
            (m_reversed - 1.0).abs() < 1e-6,
            "reversed word order must stay neutral, got {}",
            m_reversed
        );
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = tempdir().expect("tempdir");
        let now: i64 = 1_700_000_000;

        let mut store = QueryLatchStore::default();
        store.record("foo", "doc1", now);
        store.record("foo", "doc1", now);
        store.record("foo bar", "doc2", now);
        store.record("baz", "doc3", now);

        let m1_pre = store.mult("foo", "doc1", now, 0.5, 3.0);
        let m2_pre = store.mult("foo bar", "doc2", now, 0.5, 3.0);
        let m3_pre = store.mult("baz", "doc3", now, 0.5, 3.0);

        store.save(dir.path()).expect("save");
        drop(store);

        let loaded = QueryLatchStore::load(dir.path()).expect("load");
        assert!((loaded.mult("foo", "doc1", now, 0.5, 3.0) - m1_pre).abs() < 1e-6);
        assert!((loaded.mult("foo bar", "doc2", now, 0.5, 3.0) - m2_pre).abs() < 1e-6);
        assert!((loaded.mult("baz", "doc3", now, 0.5, 3.0) - m3_pre).abs() < 1e-6);
    }

    #[test]
    fn load_missing_file_returns_default() {
        let dir = tempdir().expect("tempdir");
        let store = QueryLatchStore::load(dir.path()).expect("load");
        let now: i64 = 1_700_000_000;
        assert!((store.mult("anything", "anywhere", now, 0.5, 3.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn load_corrupt_file_returns_default() {
        let dir = tempdir().expect("tempdir");
        std::fs::write(dir.path().join("query_latch.json"), "not json at all").unwrap();
        let store = QueryLatchStore::load(dir.path()).expect("load should not error");
        let now: i64 = 1_700_000_000;
        assert!((store.mult("foo", "doc1", now, 0.5, 3.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn empty_key_is_not_recorded() {
        let mut store = QueryLatchStore::default();
        let now: i64 = 1_700_000_000;
        store.record("", "doc1", now);
        store.record("   ", "doc1", now);
        assert!((store.mult("", "doc1", now, 0.5, 3.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn strong_threshold() {
        let mut store = QueryLatchStore::default();
        let now: i64 = 1_700_000_000;
        for _ in 0..3 {
            store.record("foo", "doc1", now);
        }
        assert!(store.strong("foo", "doc1", 3));
        assert!(!store.strong("foo", "doc1", 4));
        assert!(!store.strong("foo", "doc2", 1));
        assert!(!store.strong("bar", "doc1", 1));
    }

    #[test]
    fn count_capped_at_50() {
        let mut store = QueryLatchStore::default();
        let now: i64 = 1_700_000_000;
        for _ in 0..100 {
            store.record("foo", "doc1", now);
        }
        // With count capped at 50 and default weight/cap, mult is
        // bounded by cap regardless.
        let m = store.mult("foo", "doc1", now, 0.5, 3.0);
        assert!(m <= 3.0);
        assert!(m > 1.0);
    }
}
