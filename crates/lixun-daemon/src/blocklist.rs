//! Ranking blocklist — doc ids the user asked to hide from results.
//!
//! Persisted as `blocklist.json` beside the frecency store in
//! `$XDG_STATE_HOME/lixun`. The search path drops blocklisted ids
//! before ranking; `Request::Recents` filters them too. Unlike the
//! frecency/latch stores (saved on shutdown), the blocklist is saved
//! on every mutation: hides are rare, explicit user actions and must
//! survive a crash.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Blocklist {
    // BTreeSet so `list()` is stable-sorted for the CLI without an
    // extra sort pass.
    docs: BTreeSet<String>,
}

impl Blocklist {
    /// Load `blocklist.json` from `state_dir`. Missing file → empty.
    /// Corrupt JSON is logged and replaced with an empty list rather
    /// than propagating the parse error (matches the frecency store's
    /// degradation contract).
    pub fn load(state_dir: &Path) -> Self {
        let path = state_dir.join("blocklist.json");
        if !path.exists() {
            return Self::default();
        }
        match std::fs::read_to_string(&path) {
            Ok(content) => match serde_json::from_str::<Blocklist>(&content) {
                Ok(list) => list,
                Err(e) => {
                    tracing::warn!("blocklist: failed to parse {:?}: {}; starting empty", path, e);
                    Self::default()
                }
            },
            Err(e) => {
                tracing::warn!("blocklist: failed to read {:?}: {}; starting empty", path, e);
                Self::default()
            }
        }
    }

    /// Atomic write: serialize to `blocklist.json.tmp`, then rename
    /// over `blocklist.json`.
    pub fn save(&self, state_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(state_dir)?;
        let final_path = state_dir.join("blocklist.json");
        let tmp_path = state_dir.join("blocklist.json.tmp");
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(&tmp_path, content)?;
        std::fs::rename(&tmp_path, &final_path)?;
        Ok(())
    }

    /// Add or remove one doc id. Returns `true` when the set changed.
    pub fn set_hidden(&mut self, doc_id: &str, hidden: bool) -> bool {
        if hidden {
            self.docs.insert(doc_id.to_string())
        } else {
            self.docs.remove(doc_id)
        }
    }

    pub fn contains(&self, doc_id: &str) -> bool {
        self.docs.contains(doc_id)
    }

    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    /// All blocklisted ids, sorted.
    pub fn list(&self) -> Vec<String> {
        self.docs.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn set_hidden_and_contains() {
        let mut b = Blocklist::default();
        assert!(b.is_empty());
        assert!(b.set_hidden("fs:/tmp/a", true));
        assert!(!b.set_hidden("fs:/tmp/a", true), "second insert is a no-op");
        assert!(b.contains("fs:/tmp/a"));
        assert!(!b.contains("fs:/tmp/b"));
        assert!(b.set_hidden("fs:/tmp/a", false));
        assert!(!b.set_hidden("fs:/tmp/a", false), "second remove is a no-op");
        assert!(!b.contains("fs:/tmp/a"));
    }

    #[test]
    fn list_is_sorted() {
        let mut b = Blocklist::default();
        b.set_hidden("fs:/z", true);
        b.set_hidden("fs:/a", true);
        b.set_hidden("app:m", true);
        assert_eq!(
            b.list(),
            vec!["app:m".to_string(), "fs:/a".to_string(), "fs:/z".to_string()]
        );
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = tempdir().expect("tempdir");
        let mut b = Blocklist::default();
        b.set_hidden("fs:/tmp/a", true);
        b.set_hidden("mail:42", true);
        b.save(dir.path()).expect("save");

        let loaded = Blocklist::load(dir.path());
        assert!(loaded.contains("fs:/tmp/a"));
        assert!(loaded.contains("mail:42"));
        assert_eq!(loaded.list().len(), 2);
    }

    #[test]
    fn load_missing_file_is_empty() {
        let dir = tempdir().expect("tempdir");
        assert!(Blocklist::load(dir.path()).is_empty());
    }

    #[test]
    fn load_corrupt_file_is_empty() {
        let dir = tempdir().expect("tempdir");
        std::fs::write(dir.path().join("blocklist.json"), "not json").unwrap();
        assert!(Blocklist::load(dir.path()).is_empty());
    }
}
