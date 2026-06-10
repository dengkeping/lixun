//! Sink for observed-path → canonical-doc-id alias notes.
//!
//! The fs source canonicalises paths via `canonical_fs_doc_id` so a
//! file reachable through several symlinked ancestors collapses onto
//! a single doc id. The price is paid at delete time:
//! `std::fs::canonicalize` cannot resolve a vanished path and the
//! caller falls back to the raw observed path — a non-canonical id
//! that does not match the row stored in the index, so the delete
//! silently leaves a ghost behind.
//!
//! The watcher records every observed→canonical mapping it sees at
//! index time through this trait, then consults it at delete time to
//! recover the canonical id and issue the delete that actually
//! matches the stored row. The daemon supplies a JSON-sidecar
//! adapter; tests supply a simple in-memory mock.
//!
//! AGENTS.md modularity rule: the persistent JSON store is daemon
//! infrastructure and must not leak into the neutral sources layer
//! or into the indexer that calls the trait, so the seam stays a
//! plain trait object.

pub trait SymlinkAliasNoter: Send + Sync {
    /// Record that `observed` (a path that traversed a symlink) maps
    /// to the canonical doc-id `canonical_id` stored in the index.
    fn note(&self, observed: &str, canonical_id: &str);

    /// Look up the canonical doc-id previously recorded for `observed`.
    fn resolve(&self, observed: &str) -> Option<String>;

    /// Drop any alias entries pointing at `canonical_id` (cleanup
    /// after delete).
    fn forget(&self, canonical_id: &str);
}
