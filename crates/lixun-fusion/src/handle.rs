//! `HybridSearchHandle` — public seam the daemon substitutes for the
//! lexical-only `SearchHandle` when hybrid search is enabled. The
//! method surface mirrors `SearchHandle` byte-for-byte so daemon
//! call sites compile against either type without conditionals.

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
    #[cfg(feature = "semantic")]
    overfetch: usize,
}

impl HybridSearchHandle {
    pub fn new_lexical_only(inner: SearchHandle) -> Self {
        Self {
            inner,
            #[cfg(feature = "semantic")]
            ann: None,
            #[cfg(feature = "semantic")]
            rrf_k: 60.0,
            #[cfg(feature = "semantic")]
            overfetch: 4,
        }
    }

    #[cfg(feature = "semantic")]
    pub fn new(inner: SearchHandle, ann: Arc<dyn crate::ann::AnnHandle>, rrf_k: f32) -> Self {
        Self {
            inner,
            ann: Some(ann),
            rrf_k,
            overfetch: 4,
        }
    }

    pub async fn search(&self, query: &lixun_core::Query) -> Result<Vec<lixun_core::Hit>> {
        #[cfg(feature = "semantic")]
        if self.ann.is_some() {
            let pairs = self.search_with_breakdown(query).await?;
            return Ok(pairs.into_iter().map(|(h, _)| h).collect());
        }
        self.inner.search(query).await
    }

    pub async fn search_with_breakdown(
        &self,
        query: &lixun_core::Query,
    ) -> Result<Vec<(lixun_core::Hit, lixun_core::ScoreBreakdown)>> {
        #[cfg(feature = "semantic")]
        if let Some(ann) = self.ann.clone() {
            return self.fused_search(query, ann).await;
        }
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

    #[cfg(feature = "semantic")]
    async fn fused_search(
        &self,
        query: &lixun_core::Query,
        ann: Arc<dyn crate::ann::AnnHandle>,
    ) -> Result<Vec<(lixun_core::Hit, lixun_core::ScoreBreakdown)>> {
        use crate::rrf::rrf_fuse;
        use std::collections::HashMap;

        let target_limit = query.limit.max(1) as usize;
        let ann_k = target_limit
            .saturating_mul(self.overfetch)
            .max(target_limit);

        let lex_fut = self.inner.search_with_breakdown(query);
        let ann_fut = ann.search_text(&query.text, ann_k);
        let (lex_pairs, ann_hits) = tokio::try_join!(lex_fut, ann_fut)?;

        let bm25_ranked: Vec<(String, f32)> = lex_pairs
            .iter()
            .map(|(h, _)| (h.id.0.clone(), h.score))
            .collect();
        let ann_ranked: Vec<(String, f32)> = ann_hits
            .iter()
            .map(|h| (h.doc_id.clone(), h.distance))
            .collect();

        let fused = rrf_fuse(&bm25_ranked, &ann_ranked, self.rrf_k);

        tracing::debug!(
            target: "lixun_fusion",
            bm25 = lex_pairs.len(),
            ann = ann_hits.len(),
            fused = fused.len(),
            "fusion: ranked input sizes"
        );
        // ANN=0 while BM25>0 on a non-empty query is the signature of
        // an unpopulated `LanceDbAnnHandle` (store or text-embedder
        // OnceLock empty); `ann::search_text` returns Ok(empty) in that
        // case and would otherwise hide a misconfigured semantic plugin
        // behind a green hybrid path.
        if ann_hits.is_empty() && !lex_pairs.is_empty() && !query.text.trim().is_empty() {
            tracing::warn!(
                target: "lixun_fusion",
                query_len = query.text.len(),
                bm25 = lex_pairs.len(),
                "fusion: ANN returned 0 hits while BM25 found matches; check ANN handle wiring"
            );
        }

        let mut by_id: HashMap<String, (lixun_core::Hit, lixun_core::ScoreBreakdown)> = lex_pairs
            .into_iter()
            .map(|(h, b)| (h.id.0.clone(), (h, b)))
            .collect();

        let mut out = Vec::with_capacity(fused.len().min(target_limit));
        for (doc_id, fused_score) in fused.into_iter().take(target_limit) {
            let pair = if let Some(p) = by_id.remove(&doc_id) {
                Some(p)
            } else {
                self.inner.hydrate_doc(&doc_id).await?
            };
            let Some((mut hit, mut bd)) = pair else {
                continue;
            };
            hit.score = fused_score;
            bd.tantivy = fused_score;
            bd.category_mult = 1.0;
            bd.prefix_mult = 1.0;
            bd.acronym_mult = 1.0;
            bd.recency_mult = 1.0;
            bd.coord_mult = 1.0;
            bd.frecency_mult = 1.0;
            bd.latch_mult = 1.0;
            bd.stage2_clamped = 1.0;
            bd.final_score = fused_score;
            out.push((hit, bd));
        }

        Ok(out)
    }
}
