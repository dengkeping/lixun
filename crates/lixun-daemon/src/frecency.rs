//! Firefox-style frecency store — local JSON state.
//!
//! Per-doc ring-buffer of visit timestamps, each weighted by an
//! age-bucket function at read time. See the `Frecency model (D3)`
//! section of `.local-plans/plans/spotlight-wave-a-ranking.md` for the
//! authoritative spec. Weights are pure functions of `(record, now)`
//! with no mutation; `record_click` is the sole write path.
//!
//! The store replaces the old additive `ClickHistory`. On first daemon
//! start after the upgrade, the legacy `history.json` is deleted and a
//! cold-start empty frecency store is returned — per plan decision D7.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::Path;

const MAX_VISITS_PER_DOC: usize = 10;
const SECONDS_PER_DAY: i64 = 86_400;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum VisitKind {
    Click,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct VisitEntry {
    pub ts: i64,
    pub kind: VisitKind,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct FrecencyRecord {
    visits: VecDeque<VisitEntry>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct FrecencyStore {
    records: HashMap<String, FrecencyRecord>,
}

/// Firefox-style age bucket weights (plan D3). Future-dated timestamps
/// (negative age) are treated as age=0 and get the freshest weight.
pub fn bucket_weight(age_seconds: i64) -> f32 {
    let age = age_seconds.max(0);
    let age_days = age / SECONDS_PER_DAY;
    if age_days <= 4 {
        1.00
    } else if age_days <= 14 {
        0.70
    } else if age_days <= 31 {
        0.50
    } else if age_days <= 90 {
        0.30
    } else {
        0.10
    }
}

impl FrecencyStore {
    /// Load `frecency.json` from `state_dir`. On the first run after
    /// upgrade the legacy `history.json` is deleted and an empty store
    /// is returned (cold-start migration, plan D7). A corrupt
    /// `frecency.json` is logged and replaced with an empty store
    /// rather than propagating the parse error.
    pub fn load(state_dir: &Path) -> Result<Self> {
        let path = state_dir.join("frecency.json");
        if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(content) => match serde_json::from_str::<FrecencyStore>(&content) {
                    Ok(store) => return Ok(store),
                    Err(e) => {
                        tracing::warn!(
                            "frecency: failed to parse {:?}: {}; starting empty",
                            path,
                            e
                        );
                        return Ok(Self::default());
                    }
                },
                Err(e) => {
                    tracing::warn!(
                        "frecency: failed to read {:?}: {}; starting empty",
                        path,
                        e
                    );
                    return Ok(Self::default());
                }
            }
        }

        let legacy = state_dir.join("history.json");
        if legacy.exists() {
            std::fs::remove_file(&legacy)?;
            tracing::info!("frecency: migrated from ClickHistory; cold start, frecency empty");
        }
        Ok(Self::default())
    }

    /// Atomic write: serialize to `frecency.json.tmp`, then rename over
    /// `frecency.json`.
    pub fn save(&self, state_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(state_dir)?;
        let final_path = state_dir.join("frecency.json");
        let tmp_path = state_dir.join("frecency.json.tmp");
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(&tmp_path, content)?;
        std::fs::rename(&tmp_path, &final_path)?;
        Ok(())
    }

    pub fn record_click(&mut self, doc_id: &str, now: i64) {
        let record = self.records.entry(doc_id.to_string()).or_default();
        record.visits.push_back(VisitEntry {
            ts: now,
            kind: VisitKind::Click,
        });
        while record.visits.len() > MAX_VISITS_PER_DOC {
            record.visits.pop_front();
        }
    }

    /// Raw frecency score: sum of bucket weights over all stored
    /// visits for `doc_id`. Bounded by `MAX_VISITS_PER_DOC` (each
    /// bucket weight is `<= 1.0`).
    pub fn raw(&self, doc_id: &str, now: i64) -> f32 {
        let Some(record) = self.records.get(doc_id) else {
            return 0.0;
        };
        record
            .visits
            .iter()
            .map(|v| bucket_weight(now - v.ts))
            .sum()
    }

    /// Frecency multiplier: `1 + alpha * raw`. Returns `1.0` for
    /// unknown docs. `alpha` is expected in `[0, 1]`.
    pub fn mult(&self, doc_id: &str, now: i64, alpha: f32) -> f32 {
        1.0 + alpha * self.raw(doc_id, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const DAY: i64 = SECONDS_PER_DAY;

    #[test]
    fn bucket_weights() {
        assert!((bucket_weight(DAY) - 1.00).abs() < 1e-6);
        assert!((bucket_weight(10 * DAY) - 0.70).abs() < 1e-6);
        assert!((bucket_weight(25 * DAY) - 0.50).abs() < 1e-6);
        assert!((bucket_weight(60 * DAY) - 0.30).abs() < 1e-6);
        assert!((bucket_weight(365 * DAY) - 0.10).abs() < 1e-6);
        // Future-dated timestamps count as age = 0 → freshest bucket.
        assert!((bucket_weight(-100) - 1.00).abs() < 1e-6);
    }

    #[test]
    fn mult_semantics() {
        let mut store = FrecencyStore::default();
        let now: i64 = 1_700_000_000;

        // Empty store → neutral.
        assert!((store.mult("foo", now, 0.1) - 1.0).abs() < 1e-5);

        // Three clicks at `now` → raw=3.0, alpha=0.1, mult=1.3.
        store.record_click("foo", now);
        store.record_click("foo", now);
        store.record_click("foo", now);
        assert!((store.mult("foo", now, 0.1) - 1.3).abs() < 1e-5);

        // Unknown doc stays neutral.
        assert!((store.mult("bar", now, 0.1) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = tempdir().expect("tempdir");
        let now: i64 = 1_700_000_000;

        let mut store = FrecencyStore::default();
        // 5 clicks across 3 doc ids, ages spread across buckets.
        store.record_click("a", now);
        store.record_click("a", now - 10 * DAY);
        store.record_click("b", now - 25 * DAY);
        store.record_click("b", now - 60 * DAY);
        store.record_click("c", now);

        let ma_pre = store.mult("a", now, 0.1);
        let mb_pre = store.mult("b", now, 0.1);
        let mc_pre = store.mult("c", now, 0.1);
        let md_pre = store.mult("d", now, 0.1);

        store.save(dir.path()).expect("save");
        drop(store);

        let loaded = FrecencyStore::load(dir.path()).expect("load");
        assert!((loaded.mult("a", now, 0.1) - ma_pre).abs() < 1e-6);
        assert!((loaded.mult("b", now, 0.1) - mb_pre).abs() < 1e-6);
        assert!((loaded.mult("c", now, 0.1) - mc_pre).abs() < 1e-6);
        assert!((loaded.mult("d", now, 0.1) - md_pre).abs() < 1e-6);
    }

    #[test]
    fn cold_start_migration() {
        let dir = tempdir().expect("tempdir");
        let legacy_path = dir.path().join("history.json");
        std::fs::write(&legacy_path, r#"{"counts":{"x":5}}"#).expect("write legacy");
        assert!(legacy_path.exists());

        let store = FrecencyStore::load(dir.path()).expect("load migrates");
        let now: i64 = 1_700_000_000;
        // Counts are NOT ported — cold start.
        assert!((store.mult("x", now, 0.1) - 1.0).abs() < 1e-6);
        // Legacy file is gone.
        assert!(!legacy_path.exists());
    }

    #[test]
    fn visits_capped_per_doc() {
        let mut store = FrecencyStore::default();
        let now: i64 = 1_700_000_000;
        for i in 0..20 {
            store.record_click("doc", now - i);
        }
        // With cap = 10 and alpha = 0.1, raw <= 10 → mult <= 2.0.
        assert!(store.mult("doc", now, 0.1) <= 2.0 + 1e-5);
    }
}
