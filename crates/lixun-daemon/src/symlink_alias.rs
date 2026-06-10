//! Persistent observed-path → canonical-doc-id map (sidecar JSON).
//!
//! Mirrors the on-disk shape of `frecency.json`: corrupt-tolerant
//! load that downgrades to an empty map with a warn, atomic save via
//! `<name>.json.tmp` + `rename`. Implements
//! [`lixun_sources::SymlinkAliasNoter`] so the watcher can call
//! through a trait object and the indexer never names this crate.
//!
//! Key convention: the map is keyed on the **raw observed path
//! string** (no `fs:` prefix), the exact value the watcher's Gone
//! arm has on hand when `std::fs::canonicalize` fails on a vanished
//! path. The value is the full canonical doc-id, including the `fs:`
//! prefix, so the Gone arm can hand it straight to
//! `Mutation::DeleteSubtree` without further wrangling.

use anyhow::Result;
use lixun_sources::SymlinkAliasNoter;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};

#[derive(Debug, Default, Serialize, Deserialize)]
struct OnDisk {
    aliases: HashMap<String, String>,
}

#[derive(Debug, Default)]
pub struct SymlinkAliases {
    map: Arc<RwLock<HashMap<String, String>>>,
}

impl SymlinkAliases {
    /// Load `symlink_aliases.json` from `state_dir`. Missing →
    /// empty map. Corrupt or unreadable → warn + empty map (never
    /// errors), matching the frecency-store contract.
    pub fn load(state_dir: &Path) -> Self {
        let path = state_dir.join("symlink_aliases.json");
        if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(content) => match serde_json::from_str::<OnDisk>(&content) {
                    Ok(disk) => {
                        return Self {
                            map: Arc::new(RwLock::new(disk.aliases)),
                        };
                    }
                    Err(e) => {
                        tracing::warn!(
                            "symlink_alias: failed to parse {:?}: {}; starting empty",
                            path,
                            e
                        );
                    }
                },
                Err(e) => {
                    tracing::warn!(
                        "symlink_alias: failed to read {:?}: {}; starting empty",
                        path,
                        e
                    );
                }
            }
        }
        Self::default()
    }

    /// Atomic write: serialise to `symlink_aliases.json.tmp`, then
    /// rename over `symlink_aliases.json`. A concurrent loader sees
    /// either the pre-existing file or the complete new file —
    /// never a partially-written tmp.
    pub fn save(&self, state_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(state_dir)?;
        let final_path = state_dir.join("symlink_aliases.json");
        let tmp_path = state_dir.join("symlink_aliases.json.tmp");
        let snapshot = OnDisk {
            aliases: self
                .map
                .read()
                .expect("symlink_alias map poisoned")
                .clone(),
        };
        let content = serde_json::to_string_pretty(&snapshot)?;
        std::fs::write(&tmp_path, content)?;
        std::fs::rename(&tmp_path, &final_path)?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.map.read().expect("symlink_alias map poisoned").len()
    }
}

impl SymlinkAliasNoter for SymlinkAliases {
    fn note(&self, observed: &str, canonical_id: &str) {
        let mut guard = self
            .map
            .write()
            .expect("symlink_alias map poisoned");
        guard.insert(observed.to_string(), canonical_id.to_string());
    }

    fn resolve(&self, observed: &str) -> Option<String> {
        self.map
            .read()
            .expect("symlink_alias map poisoned")
            .get(observed)
            .cloned()
    }

    fn forget(&self, canonical_id: &str) {
        let mut guard = self
            .map
            .write()
            .expect("symlink_alias map poisoned");
        guard.retain(|_, v| v != canonical_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn aliases_load_save_roundtrip() {
        let dir = tempdir().expect("tempdir");

        let store = SymlinkAliases::load(dir.path());
        assert_eq!(store.len(), 0, "fresh state_dir → empty store");

        store.note("/home/u/Documents/a.txt", "fs:/data/real/a.txt");
        store.note("/home/u/Documents/b.txt", "fs:/data/real/b.txt");
        store.note("/srv/link/x", "fs:/srv/real/x");

        store.save(dir.path()).expect("save");
        drop(store);

        let loaded = SymlinkAliases::load(dir.path());
        assert_eq!(loaded.len(), 3);
        assert_eq!(
            loaded.resolve("/home/u/Documents/a.txt").as_deref(),
            Some("fs:/data/real/a.txt")
        );
        assert_eq!(
            loaded.resolve("/home/u/Documents/b.txt").as_deref(),
            Some("fs:/data/real/b.txt")
        );
        assert_eq!(
            loaded.resolve("/srv/link/x").as_deref(),
            Some("fs:/srv/real/x")
        );
        assert_eq!(loaded.resolve("/no/such/path").as_deref(), None);

        loaded.forget("fs:/data/real/a.txt");
        assert_eq!(loaded.resolve("/home/u/Documents/a.txt"), None);
        assert_eq!(loaded.len(), 2);
    }

    #[test]
    fn aliases_atomic_save_survives_concurrent_load() {
        // Pin the atomic-rename invariant: after save() returns, the
        // on-disk JSON parses cleanly, contains the full snapshot,
        // and no `.tmp` file is left behind. A loader running
        // concurrently with further in-memory note() calls reads the
        // saved snapshot, not garbage and not the live in-memory
        // state — the on-disk file is decoupled from the live map.
        let dir = tempdir().expect("tempdir");

        let store = SymlinkAliases::default();
        for i in 0..256 {
            store.note(
                &format!("/observed/path/{i}.txt"),
                &format!("fs:/canonical/{i}.txt"),
            );
        }
        store.save(dir.path()).expect("save");

        let final_path = dir.path().join("symlink_aliases.json");
        let tmp_path = dir.path().join("symlink_aliases.json.tmp");
        assert!(final_path.exists(), "final file must exist after save");
        assert!(
            !tmp_path.exists(),
            "tmp must have been renamed away (atomic rename invariant)"
        );

        let raw = std::fs::read_to_string(&final_path).expect("read");
        let _: OnDisk = serde_json::from_str(&raw).expect("parses cleanly");

        // Drive concurrent writers against the live map while a fresh
        // load() pulls the on-disk snapshot. The Arc<RwLock<...>> is
        // shared through a clone of the inner handle.
        let writer_store = SymlinkAliases {
            map: Arc::clone(&store.map),
        };
        let writer_handle = std::thread::spawn(move || {
            for i in 0..256 {
                writer_store.note(
                    &format!("/extra/{i}.txt"),
                    &format!("fs:/extra-canon/{i}.txt"),
                );
            }
        });

        let loaded = SymlinkAliases::load(dir.path());
        writer_handle.join().expect("writer thread joined");

        assert_eq!(loaded.len(), 256, "loaded snapshot is the saved 256");
        assert_eq!(
            loaded.resolve("/observed/path/0.txt").as_deref(),
            Some("fs:/canonical/0.txt")
        );
        assert_eq!(
            loaded.resolve("/observed/path/255.txt").as_deref(),
            Some("fs:/canonical/255.txt")
        );
        // The concurrent writer's notes did NOT bleed into the
        // on-disk snapshot (it was already serialised).
        assert!(loaded.resolve("/extra/0.txt").is_none());
    }
}
