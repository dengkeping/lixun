//! Pure, GTK-free directory enumeration.
//!
//! Walks `std::fs::read_dir` for the given path and classifies each
//! immediate child as a directory, regular file, symlink, or other
//! special node (fifo, socket, …). Symlinks are intentionally
//! reported as such — never followed — so a directory containing a
//! symlink to a multi-gigabyte tree still lists in O(immediate
//! children).
//!
//! Errors on individual children (permission denied on a single
//! entry, racing unlink between `read_dir` and `symlink_metadata`)
//! are absorbed: the entry is still surfaced by name with kind
//! `Other` and unknown size. Only a failure to open the directory
//! itself (or being handed a non-directory path) produces an
//! `Err(DirError::Io)`.
//!
//! Sorting is deterministic: directories first, then everything
//! else, alphabetical by lowercase name within each group. The
//! `cap` parameter truncates the rendered slice while the
//! `files`/`dirs`/`total_size` counters reflect the FULL directory,
//! so the header summary stays accurate even when the list is cut
//! short.

use std::fmt;
use std::fs;
use std::path::Path;

/// A single immediate child of the listed directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub kind: EntryKind,
    /// Byte length for regular files; `None` for directories,
    /// symlinks and special nodes (where "size" has no useful
    /// meaning for the preview header).
    pub size: Option<u64>,
}

/// Classification of an immediate child node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// Regular file.
    File,
    /// Directory.
    Dir,
    /// Symbolic link (never followed by this lister).
    Symlink,
    /// Anything else: FIFO, socket, block/char device, or an entry
    /// whose metadata could not be read.
    Other,
}

/// Result of enumerating a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirListing {
    /// Rendered entries after sort + truncation.
    pub entries: Vec<Entry>,
    /// Total number of immediate regular files seen (NOT just the
    /// rendered slice). Used for the header summary.
    pub files: usize,
    /// Total number of immediate directories seen (NOT just the
    /// rendered slice).
    pub dirs: usize,
    /// Sum of `size` over immediate regular files only (never
    /// recurses into subdirectories, never follows symlinks).
    pub total_size: u64,
    /// `true` when the rendered slice was cut short at `cap`
    /// entries; `entries.len() == cap` in that case.
    pub truncated: bool,
}

/// Why a directory could not be listed.
#[derive(Debug)]
pub enum DirError {
    /// Underlying I/O failure when opening the directory itself.
    /// Per-entry failures are absorbed into `EntryKind::Other`
    /// rather than producing this variant.
    Io(String),
}

impl fmt::Display for DirError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DirError::Io(why) => write!(f, "cannot read folder: {why}"),
        }
    }
}

impl std::error::Error for DirError {}

/// Enumerate immediate children of `path`.
///
/// `cap` bounds the rendered slice; counts on the returned listing
/// still reflect the full directory.
pub fn list_dir(path: &Path, cap: usize) -> Result<DirListing, DirError> {
    let read = fs::read_dir(path).map_err(|e| DirError::Io(e.to_string()))?;

    let mut entries: Vec<Entry> = Vec::new();
    let mut files: usize = 0;
    let mut dirs: usize = 0;
    let mut total_size: u64 = 0;

    for item in read {
        // Graceful per-entry handling: a DirEntry error is surfaced
        // with a synthetic name rather than abandoning the listing.
        let dir_entry = match item {
            Ok(e) => e,
            Err(_) => {
                entries.push(Entry {
                    name: "<unreadable entry>".to_string(),
                    kind: EntryKind::Other,
                    size: None,
                });
                continue;
            }
        };

        let name = dir_entry.file_name().to_string_lossy().into_owned();

        // `symlink_metadata` so links are NOT followed.
        let meta = match fs::symlink_metadata(dir_entry.path()) {
            Ok(m) => m,
            Err(_) => {
                entries.push(Entry {
                    name,
                    kind: EntryKind::Other,
                    size: None,
                });
                continue;
            }
        };

        let file_type = meta.file_type();
        let (kind, size) = if file_type.is_symlink() {
            (EntryKind::Symlink, None)
        } else if file_type.is_dir() {
            dirs += 1;
            (EntryKind::Dir, None)
        } else if file_type.is_file() {
            files += 1;
            total_size = total_size.saturating_add(meta.len());
            (EntryKind::File, Some(meta.len()))
        } else {
            (EntryKind::Other, None)
        };

        entries.push(Entry { name, kind, size });
    }

    // Sort: directories first, then everything else, alphabetical
    // (case-insensitive) within each group. Stable sort keeps
    // identical lowercase-keys in `read_dir` order, which is fine
    // since they would only differ in case.
    entries.sort_by(|a, b| {
        let group_a = group_key(a.kind);
        let group_b = group_key(b.kind);
        group_a
            .cmp(&group_b)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    let truncated = entries.len() > cap;
    if truncated {
        entries.truncate(cap);
    }

    Ok(DirListing {
        entries,
        files,
        dirs,
        total_size,
        truncated,
    })
}

/// Lower value sorts first. Directories lead, everything else
/// (files, symlinks, special) follows.
fn group_key(kind: EntryKind) -> u8 {
    match kind {
        EntryKind::Dir => 0,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use std::path::PathBuf;

    /// Build a unique temp path under `std::env::temp_dir()` so
    /// parallel test runs do not collide. The caller is responsible
    /// for cleanup (best-effort `remove_dir_all`).
    fn tmp(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "lixun-folder-test-{}-{}",
            std::process::id(),
            name
        ));
        p
    }

    fn cleanup(p: &Path) {
        let _ = fs::remove_dir_all(p);
    }

    fn write_bytes(p: &Path, bytes: &[u8]) {
        let mut f = File::create(p).expect("create file");
        f.write_all(bytes).expect("write bytes");
    }

    #[test]
    fn counts_files_dirs_and_immediate_total_size() {
        let root = tmp("counts");
        cleanup(&root);
        fs::create_dir_all(&root).unwrap();

        // Two immediate files of known size.
        write_bytes(&root.join("a.txt"), b"hello"); // 5 bytes
        write_bytes(&root.join("b.bin"), b"01234567"); // 8 bytes

        // One immediate subdirectory holding a 100-byte file. That
        // nested file must NOT be counted in total_size.
        let nested = root.join("inner");
        fs::create_dir(&nested).unwrap();
        write_bytes(&nested.join("deep.dat"), &vec![0u8; 100]);

        let result = list_dir(&root, 100).expect("list ok");

        assert_eq!(result.files, 2, "two immediate regular files");
        assert_eq!(result.dirs, 1, "one immediate subdirectory");
        assert_eq!(
            result.total_size, 13,
            "only immediate file bytes counted (5 + 8)"
        );
        assert!(!result.truncated);
        cleanup(&root);
    }

    #[test]
    fn sort_dirs_before_files_alphabetically() {
        let root = tmp("sort");
        cleanup(&root);
        fs::create_dir_all(&root).unwrap();

        write_bytes(&root.join("z_file.txt"), b"x");
        write_bytes(&root.join("a_file.txt"), b"x");
        fs::create_dir(root.join("b_dir")).unwrap();
        fs::create_dir(root.join("A_dir")).unwrap();

        let result = list_dir(&root, 100).expect("list ok");

        let names: Vec<&str> = result.entries.iter().map(|e| e.name.as_str()).collect();
        // Dirs first (case-insensitive alpha: A_dir, b_dir), then
        // files (a_file.txt, z_file.txt).
        assert_eq!(names, vec!["A_dir", "b_dir", "a_file.txt", "z_file.txt"]);
        cleanup(&root);
    }

    #[test]
    fn symlink_is_classified_and_not_followed() {
        use std::os::unix::fs::symlink;

        let root = tmp("symlink");
        cleanup(&root);
        fs::create_dir_all(&root).unwrap();

        // Target: a 999-byte file we DO NOT want counted via the
        // link's metadata.
        let target = root.join("target.bin");
        write_bytes(&target, &vec![0u8; 999]);

        // Symlink pointing at the target.
        symlink(&target, root.join("alias")).expect("create symlink");

        let result = list_dir(&root, 100).expect("list ok");

        let alias = result
            .entries
            .iter()
            .find(|e| e.name == "alias")
            .expect("alias entry present");
        assert_eq!(alias.kind, EntryKind::Symlink, "alias is a symlink");
        assert_eq!(alias.size, None, "symlink size not reported");

        // total_size should reflect ONLY the regular target file
        // (999 bytes), never doubled by the symlink.
        assert_eq!(result.files, 1, "only the regular file counts");
        assert_eq!(result.total_size, 999);
        cleanup(&root);
    }

    #[test]
    fn truncates_when_over_cap() {
        let root = tmp("truncate");
        cleanup(&root);
        fs::create_dir_all(&root).unwrap();

        for i in 0..10 {
            write_bytes(&root.join(format!("f{i:02}.txt")), b"x");
        }

        let result = list_dir(&root, 3).expect("list ok");

        assert!(result.truncated, "truncated flag set");
        assert_eq!(result.entries.len(), 3, "rendered slice capped");
        // Counts still reflect the full directory.
        assert_eq!(result.files, 10);
        assert_eq!(result.dirs, 0);
        cleanup(&root);
    }

    #[test]
    fn non_directory_path_returns_io_error() {
        let root = tmp("not-a-dir");
        cleanup(&root);
        // Ensure parent exists; create a plain file at `root`.
        if let Some(parent) = root.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        write_bytes(&root, b"i am a file");

        let result = list_dir(&root, 100);
        assert!(
            matches!(result, Err(DirError::Io(_))),
            "expected Io error for non-directory path, got {result:?}"
        );

        // And the function does not panic.
        let _ = fs::remove_file(&root);
    }

    #[test]
    fn empty_directory_lists_cleanly() {
        let root = tmp("empty");
        cleanup(&root);
        fs::create_dir_all(&root).unwrap();

        let result = list_dir(&root, 100).expect("list ok");
        assert!(result.entries.is_empty());
        assert_eq!(result.files, 0);
        assert_eq!(result.dirs, 0);
        assert_eq!(result.total_size, 0);
        assert!(!result.truncated);
        cleanup(&root);
    }
}
