//! `HybridSearchHandle` — public seam the daemon substitutes for the
//! lexical-only `SearchHandle` when hybrid search is enabled. The
//! method surface mirrors `SearchHandle` byte-for-byte so daemon
//! call sites compile against either type without conditionals.

use anyhow::Result;
use lixun_indexer::index_service::SearchHandle;
use std::sync::Arc;

#[derive(Clone)]
pub struct HybridSearchHandle {
    inner: SearchHandle,
    ann: Option<Arc<dyn crate::ann::AnnHandle>>,
    // Retained for API compatibility and future RRF mode toggle (see
    // fused_search): callers configure k through `new()`, and re-
    // enabling rrf::rrf_fuse can pull this back into the active path
    // without a constructor break.
    #[allow(dead_code)]
    rrf_k: f32,
    overfetch: usize,
}

impl HybridSearchHandle {
    pub fn new_lexical_only(inner: SearchHandle) -> Self {
        Self {
            inner,
            ann: None,
            rrf_k: 60.0,
            overfetch: 4,
        }
    }

    pub fn new(inner: SearchHandle, ann: Arc<dyn crate::ann::AnnHandle>, rrf_k: f32) -> Self {
        Self {
            inner,
            ann: Some(ann),
            rrf_k,
            overfetch: 4,
        }
    }

    pub async fn search(&self, query: &lixun_core::Query) -> Result<Vec<lixun_core::Hit>> {
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

    async fn fused_search(
        &self,
        query: &lixun_core::Query,
        ann: Arc<dyn crate::ann::AnnHandle>,
    ) -> Result<Vec<(lixun_core::Hit, lixun_core::ScoreBreakdown)>> {
        use std::collections::HashSet;

        let target_limit = query.limit.max(1) as usize;
        let ann_k = target_limit
            .saturating_mul(self.overfetch)
            .max(target_limit);

        let lex_fut = self.inner.search_with_breakdown(query);
        let ann_fut = ann.search_text(&query.text, ann_k);
        let (lex_pairs, ann_hits) = tokio::try_join!(lex_fut, ann_fut)?;

        tracing::debug!(
            target: "lixun_fusion",
            bm25 = lex_pairs.len(),
            ann = ann_hits.len(),
            "fusion: ranked input sizes (text-priority mode)"
        );
        // ANN=0 while BM25>0 on a non-empty query is the signature of
        // an unpopulated ANN handle (store or text-embedder OnceLock
        // empty); ann::search_text returns Ok(empty) in that case
        // and would otherwise hide a misconfigured semantic plugin
        // behind a green hybrid path.
        if ann_hits.is_empty() && !lex_pairs.is_empty() && !query.text.trim().is_empty() {
            tracing::warn!(
                target: "lixun_fusion",
                query_len = query.text.len(),
                bm25 = lex_pairs.len(),
                "fusion: ANN returned 0 hits while BM25 found matches; check ANN handle wiring"
            );
        }

        // Text-priority composition: lexical hits first in their own
        // BM25 order, then ANN-only hits as a semantic suffix. RRF is
        // intentionally NOT used here — for short symbolic queries
        // (e.g. ticket ids, filename codes like "AQL-HSSA") RRF
        // collapses BM25 and ANN scores into the same 1/(60+rank)
        // band, and noisy semantic neighbours rank alongside exact
        // text matches. Showing BM25 first preserves precision; the
        // ANN suffix preserves recall for natural-language queries
        // when BM25 returns few or no results. rrf::rrf_fuse is kept
        // in the crate for future modes selectable via config.
        let mut out: Vec<(lixun_core::Hit, lixun_core::ScoreBreakdown)> =
            Vec::with_capacity(target_limit);
        let mut seen: HashSet<String> = HashSet::with_capacity(lex_pairs.len() + ann_hits.len());

        for (hit, bd) in lex_pairs.into_iter().take(target_limit) {
            seen.insert(hit.id.0.clone());
            out.push((hit, bd));
        }

        if out.len() < target_limit {
            // Hydrate ANN-only docs through the lexical SearchHandle so
            // they get the same Hit/ScoreBreakdown shape as BM25 hits,
            // then mark them as semantic-suffix entries.
            for h in ann_hits.into_iter() {
                if out.len() >= target_limit {
                    break;
                }
                if seen.contains(&h.doc_id) {
                    continue;
                }
                seen.insert(h.doc_id.clone());
                let Some((mut hit, mut bd)) = self.inner.hydrate_doc(&h.doc_id).await? else {
                    continue;
                };
                // Semantic-suffix marker: zero out lexical score so the
                // text-first ordering is unambiguous for downstream
                // consumers, and stash the raw ANN distance in tantivy
                // so callers can still inspect the embedding signal.
                hit.score = 0.0;
                bd.tantivy = h.distance;
                bd.category_mult = 1.0;
                bd.prefix_mult = 1.0;
                bd.acronym_mult = 1.0;
                bd.recency_mult = 1.0;
                bd.coord_mult = 1.0;
                bd.frecency_mult = 1.0;
                bd.latch_mult = 1.0;
                bd.stage2_clamped = 1.0;
                bd.final_score = 0.0;
                out.push((hit, bd));
            }
        }

        Ok(out)
    }
}
