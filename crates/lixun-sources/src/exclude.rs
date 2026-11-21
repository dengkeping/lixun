use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub fn path_excluded(path: &Path, substrings: &[String], regexes: &[regex::Regex]) -> bool {
    let s = path.to_string_lossy();
    substrings.iter().any(|p| s.contains(p.as_str())) || regexes.iter().any(|r| r.is_match(&s))
}

/// Resolved once per walk. Both the configured spelling and its target
/// participate in exclusions, including when a configured root is a symlink.
pub struct PathPolicy {
    roots: Vec<(PathBuf, PathBuf)>,
    mounted_devices: Vec<(PathBuf, u64)>,
    substrings: Vec<String>,
    regexes: Vec<regex::Regex>,
}

impl PathPolicy {
    pub fn new(roots: &[PathBuf], substrings: &[String], regexes: &[regex::Regex]) -> Self {
        let roots: Vec<_> = roots
            .iter()
            .filter_map(|root| Some((std::path::absolute(root).ok()?, root.canonicalize().ok()?)))
            .collect();
        let targets: Vec<_> = roots.iter().map(|(_, t)| t.clone()).collect();
        let mounted_devices = crate::manifest::mounted_areas(&targets)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|p| Some((p.clone(), p.metadata().ok()?.dev())))
            .collect();
        Self {
            roots,
            mounted_devices,
            substrings: substrings.to_vec(),
            regexes: regexes.to_vec(),
        }
    }

    fn rule(&self, path: &Path) -> Option<String> {
        for ancestor in path.ancestors() {
            let name = ancestor.to_string_lossy();
            if let Some(rule) = self
                .substrings
                .iter()
                .find(|rule| name.contains(rule.as_str()))
            {
                return Some(format!("substring {rule}"));
            }
            if let Some(rule) = self.regexes.iter().find(|rule| rule.is_match(&name)) {
                return Some(format!("regex {rule}"));
            }
        }
        None
    }

    pub fn reason(&self, path: &Path) -> Option<String> {
        let Ok(absolute) = std::path::absolute(path) else {
            return Some("unresolvable path".into());
        };
        if let Some(rule) = self.rule(&absolute) {
            return Some(rule);
        }
        let mut observed = self
            .roots
            .iter()
            .filter(|(root, target)| absolute.starts_with(root) || absolute.starts_with(target));
        if observed.next().is_none() {
            return Some("outside configured roots".into());
        }
        let target = match absolute.canonicalize() {
            Ok(target) => target,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // For a removal or a diagnostic on a missing file, resolve
                // the nearest existing parent, including symlinked parents.
                let Some((base, resolved)) = absolute
                    .ancestors()
                    .skip(1)
                    .find_map(|base| Some((base, base.canonicalize().ok()?)))
                else {
                    return Some("unresolvable path".into());
                };
                let suffix = absolute.strip_prefix(base).expect("ancestor of this path");
                if suffix
                    .components()
                    .any(|c| c == std::path::Component::ParentDir)
                {
                    return Some("unresolvable path".into());
                }
                resolved.join(suffix)
            }
            Err(_) => return Some("unresolvable path".into()),
        };
        if !self.roots.iter().any(|(_, root)| target.starts_with(root)) {
            return Some("outside configured roots".into());
        }
        if target != absolute {
            return self.rule(&target);
        }
        None
    }

    pub fn root_available_for(&self, path: &Path) -> bool {
        if self.mounted_devices.iter().any(|(mount, device)| {
            path.starts_with(mount) && mount.metadata().map_or(true, |meta| meta.dev() != *device)
        }) {
            return false;
        }
        self.roots.iter().any(|(root, target)| {
            (path.starts_with(root) || path.starts_with(target))
                && root.canonicalize().is_ok_and(|current| current == *target)
                && target.is_dir()
        })
    }

    /// Recheck the disk at the writer boundary. Enumeration may have ended
    /// before a concurrent refresh recreated this file or its mount went away.
    pub fn should_remove(&self, path: &Path) -> bool {
        if !self
            .roots
            .iter()
            .any(|(root, target)| path.starts_with(root) || path.starts_with(target))
        {
            return true;
        }
        if !self.root_available_for(path) {
            return false;
        }
        match std::fs::symlink_metadata(path) {
            Ok(_) => self.matches(path),
            Err(error) => error.kind() == std::io::ErrorKind::NotFound,
        }
    }

    pub fn is_root(&self, path: &Path) -> bool {
        self.roots
            .iter()
            .any(|(root, target)| root == path || target == path)
    }

    pub fn matches(&self, path: &Path) -> bool {
        self.reason(path).is_some()
    }

    /// Descendant links are not followed during a walk. Each ancestor has
    /// already passed filter_entry, so no per-entry realpath is necessary.
    pub fn allows_walk_entry(&self, path: &Path, walk_root: &Path) -> bool {
        if path == walk_root {
            return self.reason(path).is_none();
        }
        let absolute;
        let path = if path.is_absolute() {
            path
        } else {
            let Ok(resolved) = std::path::absolute(path) else {
                return false;
            };
            absolute = resolved;
            &absolute
        };
        if path_excluded(path, &self.substrings, &self.regexes) {
            return false;
        }
        self.roots.iter().any(|(root, target)| {
            path.strip_prefix(root).is_ok_and(|suffix| {
                root == target
                    || !path_excluded(&target.join(suffix), &self.substrings, &self.regexes)
            })
        })
    }
}

/// Absolute paths of every directory lixun itself writes to, used as
/// substring excludes for the fs source. The set must cover XDG
/// data, state, cache and config so the indexer never sees its own
/// LanceDB rotations, SQLite WAL files or extract caches as user
/// content. `LIXUN_SEMANTIC_DATA_DIR` is honoured as well so a user
/// who relocates the worker storage doesn't reintroduce the loop.
pub fn lixun_self_excludes() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |p: PathBuf| {
        if let Some(s) = p.to_str()
            && !s.is_empty()
        {
            out.push(s.to_string());
        }
    };
    if let Some(p) = dirs::data_dir() {
        push(p.join("lixun"));
    }
    if let Some(p) = dirs::state_dir() {
        push(p.join("lixun"));
    } else {
        let home = std::env::var("HOME").unwrap_or_default();
        if !home.is_empty() {
            push(PathBuf::from(&home).join(".local/state/lixun"));
        }
    }
    if let Some(p) = dirs::cache_dir() {
        push(p.join("lixun"));
    }
    if let Some(p) = dirs::config_dir() {
        push(p.join("lixun"));
    }
    if let Ok(custom) = std::env::var("LIXUN_SEMANTIC_DATA_DIR")
        && !custom.is_empty()
    {
        push(PathBuf::from(custom));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    fn re(s: &str) -> regex::Regex {
        regex::Regex::new(s).unwrap()
    }

    #[test]
    fn direct_and_walk_paths_respect_parent_rules_and_root_aliases() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let alias = tmp.path().join("alias");
        let outside = tmp.path().join("external");
        std::fs::create_dir_all(root.join("blocked")).unwrap();
        std::fs::create_dir(&outside).unwrap();
        symlink(&root, &alias).unwrap();
        symlink(&outside, root.join("link")).unwrap();
        let rules = PathPolicy::new(std::slice::from_ref(&alias), &[], &[re("/blocked$")]);
        assert!(rules.matches(&alias.join("link/missing")));
        assert!(rules.matches(&alias.join("blocked/child")));
        assert!(!rules.allows_walk_entry(&alias.join("blocked"), &alias));
        assert!(rules.allows_walk_entry(&alias.join("allowed"), &alias));
        assert!(rules.root_available_for(&root.join("allowed")));
    }

    #[test]
    fn empty_lists_never_exclude() {
        assert!(!path_excluded(&p("/any/path.txt"), &[], &[]));
    }

    #[test]
    fn substring_matches_anywhere_in_path() {
        let subs = vec!["node_modules".to_string()];
        assert!(path_excluded(
            &p("/home/me/proj/node_modules/lib/x.js"),
            &subs,
            &[]
        ));
        assert!(path_excluded(&p("/tmp/node_modules"), &subs, &[]));
        assert!(!path_excluded(&p("/home/me/src/main.rs"), &subs, &[]));
    }

    #[test]
    fn regex_anchored_to_extension() {
        let res = vec![re(r"\.pyc$")];
        assert!(path_excluded(&p("/a/b/foo.pyc"), &[], &res));
        assert!(!path_excluded(&p("/a/b/foo.py"), &[], &res));
        assert!(!path_excluded(&p("/a/b/.pyc.txt"), &[], &res));
    }

    #[test]
    fn regex_libreoffice_lock_file() {
        let res = vec![re(r"\.~lock\..*#$")];
        assert!(path_excluded(
            &p("/home/u/Docs/.~lock.Report.xlsx#"),
            &[],
            &res
        ));
        assert!(!path_excluded(&p("/home/u/Docs/Report.xlsx"), &[], &res));
    }

    #[test]
    fn substring_or_regex_short_circuits() {
        let subs = vec![".git".to_string()];
        let res = vec![re(r"\.log$")];
        assert!(path_excluded(&p("/repo/.git/HEAD"), &subs, &res));
        assert!(path_excluded(&p("/var/app.log"), &subs, &res));
        assert!(!path_excluded(&p("/home/u/notes.md"), &subs, &res));
    }

    #[test]
    fn substring_case_sensitive() {
        let subs = vec!["CACHE".to_string()];
        assert!(!path_excluded(&p("/home/u/.cache/x"), &subs, &[]));
        assert!(path_excluded(&p("/home/u/CACHE/x"), &subs, &[]));
    }
}
