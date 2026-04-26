//! `HybridSearchHandle` — public seam the daemon substitutes for the
//! lexical-only `SearchHandle` when hybrid search is enabled.
//!
//! WD-T1 mirrors `SearchHandle`'s method surface byte-for-byte; WD-T7
//! fills the fusion bodies (RRF over BM25 ranks + ANN ranks).

use anyhow::Result;
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

    pub async fn search(
        &self,
        query: &lixun_core::Query,
    ) -> Result<Vec<lixun_core::Hit>> {
        self.inner.search(query).await
    }

    pub async fn search_with_breakdown(
        &self,
        query: &lixun_core::Query,
    ) -> Result<Vec<(lixun_core::Hit, lixun_core::ScoreBreakdown)>> {
        self.inner.search_with_breakdown(query).await
    }

    pub async fn all_doc_ids(&self) -> Result<std::collections::HashSet<String>> {
        self.inner.all_doc_ids().await
    }

    pub async fn has_body(&self, doc_id: &str) -> Result<bool> {
        self.inner.has_body(doc_id).await
    }

    pub async fn get_body(&self, doc_id: &str) -> Result<Option<String>> {
        self.inner.get_body(doc_id).await
    }

    pub async fn hydrate_doc(
        &self,
        doc_id: &str,
    ) -> Result<Option<(lixun_core::Hit, lixun_core::ScoreBreakdown)>> {
        self.inner.hydrate_doc(doc_id).await
    }
}
