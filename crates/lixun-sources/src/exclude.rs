use std::path::{Path, PathBuf};

pub fn path_excluded(path: &Path, substrings: &[String], regexes: &[regex::Regex]) -> bool {
    let s = path.to_string_lossy();
    substrings.iter().any(|p| s.contains(p.as_str())) || regexes.iter().any(|r| r.is_match(&s))
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
        if let Some(s) = p.to_str() {
            if !s.is_empty() {
                out.push(s.to_string());
            }
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
    if let Ok(custom) = std::env::var("LIXUN_SEMANTIC_DATA_DIR") {
        if !custom.is_empty() {
            push(PathBuf::from(custom));
        }
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
