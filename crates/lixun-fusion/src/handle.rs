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
        use std::collections::HashMap;
        use std::time::Duration;

        let target_limit = query.limit.max(1) as usize;
        let ann_k = target_limit
            .saturating_mul(self.overfetch)
            .max(target_limit);

        // Hot-path budget for ANN sidecar IPC. The semantic worker
        // shares CPU with the embedding loop and tantivy/OCR storms
        // during backfill; if it cannot reply within ANN_BUDGET we
        // ship BM25-only rather than make the user wait. 150 ms is
        // tuned so an idle worker (~30-50 ms p50 per the architecture
        // doc) almost always makes the cut, while a saturated worker
        // never delays interactive search past the GUI's typing
        // animation. Backfill / batch paths use the longer
        // `SEARCH_TIMEOUT_BACKFILL` constant in lixun-source-semantic-stub.
        const ANN_BUDGET: Duration = Duration::from_millis(150);

        // Spotlight-style fan-out: run BM25 + text-ANN + image-ANN in
        // parallel and let RRF (Cormack 2009, k=60) merge the three
        // ranked lists. No pre-classification: the modality classifier
        // approach (CLIP-text anchors) was abandoned because CLIP text
        // space is asymmetric for short tokens — codes and identifiers
        // map equidistant from image and text anchors, producing
        // Modality::Both. RRF is robust to score distribution
        // mismatches between BM25 (unbounded) and ANN (cosine in [0,1])
        // because it uses ranks, not scores.
        //
        // BM25 always runs to completion (the index is local + lock-free
        // since the SearchHandle Mutex was removed). ANN futures are
        // bounded by ANN_BUDGET so a saturated semantic worker cannot
        // stall interactive search; on timeout we degrade to BM25-only.
        let lex_fut = self.inner.search_with_breakdown(query);
        let text_fut = tokio::time::timeout(ANN_BUDGET, ann.search_text(&query.text, ann_k));
        let image_fut = tokio::time::timeout(ANN_BUDGET, ann.search_image(&query.text, ann_k));
        let (lex_res, text_res, image_res) = tokio::join!(lex_fut, text_fut, image_fut);

        let lex_pairs = lex_res?;
        let text_hits = match text_res {
            Ok(Ok(hits)) => hits,
            Ok(Err(e)) => {
                tracing::debug!(target: "lixun_fusion", error = %e, "text ANN errored, falling back to BM25-only");
                Vec::new()
            }
            Err(_elapsed) => {
                tracing::debug!(
                    target: "lixun_fusion",
                    budget_ms = ANN_BUDGET.as_millis() as u64,
                    "text ANN exceeded budget, falling back to BM25-only"
                );
                Vec::new()
            }
        };
        let image_hits = match image_res {
            Ok(Ok(hits)) => hits,
            Ok(Err(e)) => {
                tracing::debug!(target: "lixun_fusion", error = %e, "image ANN errored, falling back to BM25-only");
                Vec::new()
            }
            Err(_elapsed) => {
                tracing::debug!(
                    target: "lixun_fusion",
                    budget_ms = ANN_BUDGET.as_millis() as u64,
                    "image ANN exceeded budget, falling back to BM25-only"
                );
                Vec::new()
            }
        };

        tracing::debug!(
            target: "lixun_fusion",
            bm25 = lex_pairs.len(),
            text_ann = text_hits.len(),
            image_ann = image_hits.len(),
            "fusion: ranked input sizes"
        );

        // Build doc_id-keyed lookup tables for hydration. BM25 already
        // gives full Hit+ScoreBreakdown; ANN hits give only doc_id
        // and distance, requiring hydrate_doc.
        let bm25_by_id: HashMap<String, (lixun_core::Hit, lixun_core::ScoreBreakdown)> = lex_pairs
            .iter()
            .map(|(h, bd)| (h.id.0.clone(), (h.clone(), bd.clone())))
            .collect();
        let ann_distance_by_id: HashMap<String, f32> = text_hits
            .iter()
            .chain(image_hits.iter())
            .map(|h| (h.doc_id.clone(), h.distance))
            .collect();

        let bm25_ranked: Vec<(String, f32)> = lex_pairs
            .iter()
            .map(|(h, bd)| (h.id.0.clone(), bd.final_score))
            .collect();
        let text_ranked: Vec<(String, f32)> = text_hits
            .iter()
            .map(|h| (h.doc_id.clone(), h.distance))
            .collect();
        let image_ranked: Vec<(String, f32)> = image_hits
            .iter()
            .map(|h| (h.doc_id.clone(), h.distance))
            .collect();

        let fused =
            crate::rrf::rrf_fuse_3way(&bm25_ranked, &text_ranked, &image_ranked, self.rrf_k);

        let mut out: Vec<(lixun_core::Hit, lixun_core::ScoreBreakdown)> =
            Vec::with_capacity(target_limit);

        for (doc_id, _rrf_score) in fused.into_iter().take(target_limit) {
            let (mut hit, mut bd) = if let Some(pair) = bm25_by_id.get(&doc_id) {
                pair.clone()
            } else if let Some((hit, bd)) = self.inner.hydrate_doc(&doc_id).await? {
                (hit, bd)
            } else {
                continue;
            };
            if !bm25_by_id.contains_key(&doc_id) {
                if let Some(distance) = ann_distance_by_id.get(&doc_id) {
                    bd.tantivy = *distance;
                }
                bd.category_mult = 1.0;
                bd.exact_title_mult = 1.0;
                bd.prefix_mult = 1.0;
                bd.acronym_mult = 1.0;
                bd.recency_mult = 1.0;
                bd.coord_mult = 1.0;
                bd.frecency_mult = 1.0;
                bd.latch_mult = 1.0;
                bd.stage2_clamped = 1.0;
                hit.score = 0.0;
                bd.final_score = 0.0;
            }
            out.push((hit, bd));
        }

        Ok(out)
    }
}
