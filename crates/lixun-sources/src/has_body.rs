//! Abstract body-presence check so `FsSource` can skip OCR enqueue
//! for documents whose body was already recovered in a prior pass,
//! without pulling in the concrete `lixun-indexer::SearchHandle` (and
//! its tokio `Mutex<LixunIndex>`). The daemon supplies an adapter
//! that bridges this trait to the real `SearchHandle`; tests supply
//! a simple mock.
//!
//! AGENTS.md modularity rule: domain-specific infrastructure (the
//! Tantivy reader, the tokio runtime that wraps it) must not leak
//! into the neutral sources layer, and this trait is the minimal
//! seam that keeps the boundary clean.
//!
//! The trait is intentionally **synchronous** because
//! `maybe_enqueue_ocr` runs on rayon worker threads — which are not
//! tokio workers — so the daemon adapter can safely drive the async
//! `SearchHandle::has_body` via `tokio::runtime::Handle::block_on`.

use anyhow::Result;

/// Source of truth for "is this doc already indexed with a body?".
pub trait HasBody: Send + Sync {
    fn has_body(&self, doc_id: &str) -> Result<bool>;
}
