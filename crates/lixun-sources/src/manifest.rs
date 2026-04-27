use lixun_core::paths::canonical_fs_path_str;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Manifest {
    files: HashMap<String, i64>,
}

impl Manifest {
    pub fn load(state_dir: &Path) -> Self {
        let path = state_dir.join("manifest.json");
        let raw: Self = match std::fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => return Self::default(),
        };
        Self::canonicalize_keys(raw)
    }

    fn canonicalize_keys(raw: Self) -> Self {
        let mut migrated: HashMap<String, i64> = HashMap::with_capacity(raw.files.len());
        let mut rewritten: usize = 0;
        for (key, mtime) in raw.files {
            let canon = canonical_fs_path_str(Path::new(&key));
            if canon != key {
                rewritten += 1;
            }
            migrated
                .entry(canon)
                .and_modify(|existing| {
                    if mtime > *existing {
                        *existing = mtime;
                    }
                })
                .or_insert(mtime);
        }
        if rewritten > 0 {
            tracing::info!(
                "Manifest: canonicalised {} entries ({} unique after merge)",
                rewritten,
                migrated.len(),
            );
        }
        Self { files: migrated }
    }

    pub fn save(&self, state_dir: &Path) {
        let path = state_dir.join("manifest.json");
        if let Ok(content) = serde_json::to_string(self) {
            let _ = std::fs::write(&path, content);
        }
    }

    pub fn is_unchanged(&self, path: &str, mtime: i64) -> bool {
        self.files.get(path).copied() == Some(mtime)
    }

    pub fn update(&mut self, path: String, mtime: i64) {
        self.files.insert(path, mtime);
    }

    pub fn remove(&mut self, path: &str) {
        self.files.remove(path);
    }

    pub fn known_paths(&self) -> impl Iterator<Item = &String> {
        self.files.keys()
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_default_manifest_is_empty() {
        let m = Manifest::default();
        assert_eq!(m.len(), 0);
    }

    #[test]
    fn test_update_and_is_unchanged() {
        let mut m = Manifest::default();
        m.update("/tmp/test.txt".to_string(), 12345);
        assert!(m.is_unchanged("/tmp/test.txt", 12345));
        assert!(!m.is_unchanged("/tmp/test.txt", 99999));
        assert!(!m.is_unchanged("/tmp/other.txt", 12345));
    }

    #[test]
    fn test_remove() {
        let mut m = Manifest::default();
        m.update("/tmp/test.txt".to_string(), 12345);
        assert_eq!(m.len(), 1);
        m.remove("/tmp/test.txt");
        assert_eq!(m.len(), 0);
        assert!(!m.is_unchanged("/tmp/test.txt", 12345));
    }

    #[test]
    fn test_known_paths() {
        let mut m = Manifest::default();
        m.update("/a.txt".to_string(), 1);
        m.update("/b.txt".to_string(), 2);
        m.update("/c.txt".to_string(), 3);
        let paths: Vec<_> = m.known_paths().collect();
        assert_eq!(paths.len(), 3);
    }

    #[test]
    fn test_save_and_load_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = Manifest::default();
        m.update("/tmp/a.txt".to_string(), 100);
        m.update("/tmp/b.txt".to_string(), 200);
        m.save(tmp.path());

        let loaded = Manifest::load(tmp.path());
        assert!(loaded.is_unchanged("/tmp/a.txt", 100));
        assert!(loaded.is_unchanged("/tmp/b.txt", 200));
        assert_eq!(loaded.len(), 2);
    }

    #[test]
    fn test_load_nonexistent_returns_default() {
        let tmp = tempfile::tempdir().unwrap();
        let m = Manifest::load(tmp.path());
        assert_eq!(m.len(), 0);
    }

    #[test]
    fn test_load_corrupt_json_returns_default() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("manifest.json"), "not json").unwrap();
        let m = Manifest::load(tmp.path());
        assert_eq!(m.len(), 0);
    }

    #[test]
    fn test_update_overwrites_mtime() {
        let mut m = Manifest::default();
        m.update("/tmp/test.txt".to_string(), 100);
        m.update("/tmp/test.txt".to_string(), 200);
        assert!(m.is_unchanged("/tmp/test.txt", 200));
        assert!(!m.is_unchanged("/tmp/test.txt", 100));
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn load_canonicalises_symlinked_keys_and_merges_duplicates() {
        use std::os::unix::fs as unix_fs;

        let tmp = tempfile::tempdir().unwrap();

        let real_dir = tmp.path().join("real");
        fs::create_dir(&real_dir).unwrap();
        let real_file = real_dir.join("doc.txt");
        fs::write(&real_file, b"x").unwrap();

        let link_dir = tmp.path().join("link");
        unix_fs::symlink(&real_dir, &link_dir).unwrap();
        let aliased = link_dir.join("doc.txt");

        let canonical = canonical_fs_path_str(&real_file);
        let aliased_str = aliased.to_string_lossy().into_owned();
        let raw_stored = fs::canonicalize(&real_file)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_ne!(
            aliased_str, canonical,
            "symlink alias must differ from canonical"
        );

        let mut pre = Manifest::default();
        pre.update(aliased_str.clone(), 100);
        pre.update(raw_stored.clone(), 200);
        pre.save(tmp.path());

        let loaded = Manifest::load(tmp.path());
        assert_eq!(
            loaded.len(),
            1,
            "alias + canonical keys must collapse to one"
        );
        assert!(
            loaded.is_unchanged(&canonical, 200),
            "merged mtime must be the newest of the two aliases"
        );
    }

    #[test]
    fn load_is_idempotent_on_canonical_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.txt");
        fs::write(&file, b"x").unwrap();
        let canonical = canonical_fs_path_str(&file);

        let mut pre = Manifest::default();
        pre.update(canonical.clone(), 42);
        pre.save(tmp.path());

        let first = Manifest::load(tmp.path());
        first.save(tmp.path());
        let second = Manifest::load(tmp.path());

        assert_eq!(second.len(), 1);
        assert!(second.is_unchanged(&canonical, 42));
    }
}
