//! `HybridSearchHandle` — public seam the daemon substitutes for the
//! lexical-only `SearchHandle` when hybrid search is enabled.
//!
//! WD-T0 scaffold: the type compiles, but the semantic path is empty.
//! WD-T1 mirrors `SearchHandle`'s method surface byte-for-byte; WD-T7
//! fills the fusion bodies.

use lixun_indexer::index_service::SearchHandle;

#[cfg(feature = "semantic")]
use std::sync::Arc;

#[derive(Clone)]
pub struct HybridSearchHandle {
    inner: SearchHandle,
    #[cfg(feature = "semantic")]
    ann: Option<Arc<dyn crate::ann::AnnHandle>>,
    #[cfg(feature = "semantic")]
    rrf_k: f32,
}

impl HybridSearchHandle {
    /// Lexical-only constructor — every search method delegates to the
    /// inner `SearchHandle`. Used when no plugin advertises an ANN
    /// channel, or when the `semantic` feature is off.
    pub fn new_lexical_only(inner: SearchHandle) -> Self {
        Self {
            inner,
            #[cfg(feature = "semantic")]
            ann: None,
            #[cfg(feature = "semantic")]
            rrf_k: 60.0,
        }
    }
}
