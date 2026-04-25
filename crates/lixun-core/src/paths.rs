//! Path utilities shared across sources, the indexer, and preview
//! plugins.
//!
//! The `fs:` document id scheme (see [`crate::DocId`]) encodes an
//! absolute filesystem path. When the same physical file is reachable
//! through multiple paths that differ only by a symlinked ancestor
//! (for example `~/Documents` → `~/Nextcloud/Documents`), naïvely
//! building `fs:<raw-path>` yields duplicate index rows that each
//! point at the same bytes, each extract the same text, and each
//! run OCR independently. Canonicalising the path once, before the
//! id is constructed, collapses both views onto a single stable id
//! and lets the existing DB-16 `HasBody` short-circuit dedupe OCR
//! enqueue across the aliases.
//!
//! Canonicalisation is best-effort: `std::fs::canonicalize` can fail
//! on broken symlinks, permission errors, or paths that do not yet
//! exist (e.g. a watcher event for a just-deleted file). In those
//! cases we fall back to the raw path so the caller still produces
//! a doc id rather than panicking or dropping the document.

use std::path::Path;

/// Build a stable `fs:<abspath>` document id for `path`.
///
/// Canonicalises the input via [`std::fs::canonicalize`] so aliases
/// through symlinked ancestors collapse onto a single id. On any
/// canonicalisation error (broken symlink, missing file, permission
/// denied) falls back to the raw path — the caller gets a well-formed
/// id either way and the worst case is transient duplication until
/// the tree settles.
///
/// Returns the full id including the `fs:` prefix so every call site
/// produces the exact same string shape without rebuilding the
/// prefix inline.
pub fn canonical_fs_doc_id(path: &Path) -> String {
    format!("fs:{}", canonical_fs_path_str(path))
}

/// Build the canonical absolute path string for `path`, without the
/// `fs:` prefix.
///
/// Same canonicalisation contract as [`canonical_fs_doc_id`]: best
/// effort, falls back to the raw path string on any error. Used by
/// call sites that need the plain path text (manifest keys, the
/// `Document::path` display field) in addition to the doc id, so both
/// stay in lock-step without two independent canonicalisation
/// fallbacks drifting apart.
pub fn canonical_fs_path_str(path: &Path) -> String {
    let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    canon.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs as unix_fs;

    #[test]
    fn canonical_fs_doc_id_matches_raw_for_plain_file() {
        // For a file reached through a path with no symlinks in its
        // ancestry the canonical id must equal the raw id. This
        // guards the common case where canonicalisation is a no-op.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("plain.txt");
        fs::write(&path, b"hello").unwrap();
        let canon = std::fs::canonicalize(&path).unwrap();
        let id = canonical_fs_doc_id(&path);
        assert_eq!(id, format!("fs:{}", canon.to_string_lossy()));
    }

    #[test]
    fn canonical_fs_doc_id_collapses_symlinked_ancestor() {
        // Core invariant: two paths to the same file that differ
        // only by a symlinked ancestor directory must produce the
        // same doc id. This is the exact scenario that caused
        // duplicate OCR work in the v1.1 smoke (Finding 2).
        let tmp = tempfile::tempdir().unwrap();
        let real_dir = tmp.path().join("real");
        fs::create_dir(&real_dir).unwrap();
        let file = real_dir.join("doc.txt");
        fs::write(&file, b"content").unwrap();

        let link_dir = tmp.path().join("link");
        unix_fs::symlink(&real_dir, &link_dir).unwrap();
        let aliased = link_dir.join("doc.txt");

        let id_real = canonical_fs_doc_id(&file);
        let id_alias = canonical_fs_doc_id(&aliased);
        assert_eq!(id_real, id_alias);
    }

    #[test]
    fn canonical_fs_doc_id_falls_back_on_missing_path() {
        // Canonicalisation of a non-existent path returns Err; the
        // helper must still produce a well-formed id so callers do
        // not need to branch on path existence. The fallback uses
        // the raw path verbatim — downstream dedupe may be imperfect
        // for that one transient id but nothing crashes.
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist.txt");
        let id = canonical_fs_doc_id(&missing);
        assert_eq!(id, format!("fs:{}", missing.to_string_lossy()));
    }

    #[test]
    fn canonical_fs_doc_id_is_idempotent() {
        // Feeding the already-canonical path back into the helper
        // must yield the same id. Guards against accidental double-
        // prefixing or canonicalisation mutating stable inputs.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("idem.txt");
        fs::write(&path, b"x").unwrap();
        let canon = std::fs::canonicalize(&path).unwrap();
        let id_first = canonical_fs_doc_id(&path);
        let id_second = canonical_fs_doc_id(&canon);
        assert_eq!(id_first, id_second);
    }
}
