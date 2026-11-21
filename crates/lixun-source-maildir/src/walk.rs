use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub fn walk_messages(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !is_tmp_dir(e.path()))
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        if !is_message_path(entry.path()) {
            continue;
        }
        out.push(entry.path().to_path_buf());
    }
    out
}

pub fn is_message_path(path: &Path) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    matches!(
        parent.file_name().and_then(|s| s.to_str()),
        Some("cur") | Some("new")
    )
}

fn is_tmp_dir(path: &Path) -> bool {
    path.file_name().and_then(std::ffi::OsStr::to_str) == Some("tmp")
}

pub fn derive_folder(root: &Path, message_path: &Path) -> String {
    let rel = message_path.strip_prefix(root).unwrap_or(message_path);
    let mut components: Vec<String> = Vec::new();
    for c in rel.components() {
        let s = c.as_os_str().to_string_lossy().to_string();
        if s == "cur" || s == "new" || s == "tmp" {
            break;
        }
        components.push(s);
    }
    if components.is_empty() {
        "INBOX".into()
    } else {
        components.join("/")
    }
}

pub fn parse_flags(filename: &str) -> Vec<&'static str> {
    let Some(idx) = filename.find(":2,") else {
        return Vec::new();
    };
    let flag_chars = &filename[idx + 3..];
    let mut flags = Vec::new();
    for c in flag_chars.chars() {
        let mapped = match c {
            'S' => "seen",
            'R' => "replied",
            'F' => "flagged",
            'D' => "draft",
            'P' => "passed",
            'T' => "trashed",
            _ => continue,
        };
        if !flags.contains(&mapped) {
            flags.push(mapped);
        }
    }
    flags
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_folder_from_inbox_path() {
        let root = Path::new("/home/me/Mail");
        let p = Path::new("/home/me/Mail/INBOX/cur/12345.M.host:2,S");
        assert_eq!(derive_folder(root, p), "INBOX");
    }

    #[test]
    fn derive_folder_from_nested_path() {
        let root = Path::new("/home/me/Mail");
        let p = Path::new("/home/me/Mail/Work/Projects/cur/abc.eml");
        assert_eq!(derive_folder(root, p), "Work/Projects");
    }

    #[test]
    fn derive_folder_root_maildir() {
        let root = Path::new("/home/me/Mail");
        let p = Path::new("/home/me/Mail/cur/12345:2,");
        assert_eq!(derive_folder(root, p), "INBOX");
    }

    #[test]
    fn is_message_path_only_in_cur_or_new() {
        assert!(is_message_path(Path::new("/m/INBOX/cur/1:2,S")));
        assert!(is_message_path(Path::new("/m/INBOX/new/1")));
        assert!(!is_message_path(Path::new("/m/INBOX/tmp/1")));
        assert!(!is_message_path(Path::new("/m/INBOX/.msf")));
    }

    #[test]
    fn parse_flags_seen_and_replied() {
        let flags = parse_flags("12345.M.host:2,RS");
        assert!(flags.contains(&"replied"));
        assert!(flags.contains(&"seen"));
        assert_eq!(flags.len(), 2);
    }

    #[test]
    fn parse_flags_none() {
        assert_eq!(parse_flags("12345.M.host"), Vec::<&str>::new());
        assert_eq!(parse_flags("12345.M.host:2,"), Vec::<&str>::new());
    }

    #[test]
    fn parse_flags_ignores_unknown() {
        let flags = parse_flags("1:2,SXYZF");
        assert!(flags.contains(&"seen"));
        assert!(flags.contains(&"flagged"));
        assert_eq!(flags.len(), 2);
    }
}
