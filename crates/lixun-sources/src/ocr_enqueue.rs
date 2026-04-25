//! Abstract enqueue sink so `FsSource` can request deferred OCR
//! without pulling in `lixun-extract::ocr_queue` (the concrete
//! SQLite-backed queue). The daemon supplies an adapter that bridges
//! this trait to the real `OcrQueue`; tests supply a simple collector.
//!
//! AGENTS.md modularity rule: domain-specific infrastructure (SQLite
//! queue persistence, OCR engine knobs) must not leak into the
//! neutral sources layer, and this trait is the minimal seam that
//! keeps the boundary clean.

use anyhow::Result;
use std::path::Path;

/// Sink for OCR enqueue requests emitted during file extraction.
pub trait OcrEnqueue: Send + Sync {
    fn enqueue(
        &self,
        doc_id: &str,
        path: &Path,
        mtime: i64,
        size: u64,
        ext: &str,
    ) -> Result<()>;
}
