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
        
        let modality = ann.classify_query(&query.text).await.unwrap_or(lixun_mutation::Modality::Text);
        
        let (lex_pairs, text_hits, image_hits) = match modality {
            lixun_mutation::Modality::Text => {
                let ann_hits = ann.search_text(&query.text, ann_k).await?;
                let lex = lex_fut.await?;
                (lex, ann_hits, Vec::new())
            }
            lixun_mutation::Modality::Image => {
                let ann_hits = ann.search_image(&query.text, ann_k).await?;
                let lex = lex_fut.await?;
                (lex, Vec::new(), ann_hits)
            }
            lixun_mutation::Modality::Both => {
                let text_fut = ann.search_text(&query.text, ann_k);
                let image_fut = ann.search_image(&query.text, ann_k);
                let (lex, text, image) = tokio::try_join!(lex_fut, text_fut, image_fut)?;
                (lex, text, image)
            }
        };

        tracing::debug!(
            target: "lixun_fusion",
            bm25 = lex_pairs.len(),
            text_ann = text_hits.len(),
            image_ann = image_hits.len(),
            ?modality,
            "fusion: ranked input sizes"
        );
        if text_hits.is_empty() && image_hits.is_empty() && !lex_pairs.is_empty() && !query.text.trim().is_empty() {
            tracing::warn!(
                target: "lixun_fusion",
                query_len = query.text.len(),
                bm25 = lex_pairs.len(),
                "fusion: ANN returned 0 hits while BM25 found matches; check ANN handle wiring"
            );
        }

        let mut out: Vec<(lixun_core::Hit, lixun_core::ScoreBreakdown)> =
            Vec::with_capacity(target_limit);
        let mut seen: HashSet<String> = HashSet::with_capacity(lex_pairs.len() + text_hits.len() + image_hits.len());

        match modality {
            lixun_mutation::Modality::Image => {
                for h in image_hits.into_iter() {
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
                if out.len() < target_limit {
                    for (hit, bd) in lex_pairs.iter().take(target_limit - out.len()) {
                        if seen.contains(&hit.id.0) {
                            continue;
                        }
                        seen.insert(hit.id.0.clone());
                        out.push((hit.clone(), bd.clone()));
                    }
                }
            }
            lixun_mutation::Modality::Text => {
                for (hit, bd) in lex_pairs.iter().take(target_limit) {
                    seen.insert(hit.id.0.clone());
                    out.push((hit.clone(), bd.clone()));
                }
                if out.len() < target_limit {
                    for h in text_hits.into_iter() {
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
            }
            lixun_mutation::Modality::Both => {
                for h in image_hits.into_iter() {
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
                for (hit, bd) in lex_pairs.iter() {
                    if out.len() >= target_limit {
                        break;
                    }
                    if seen.contains(&hit.id.0) {
                        continue;
                    }
                    seen.insert(hit.id.0.clone());
                    out.push((hit.clone(), bd.clone()));
                }
                if out.len() < target_limit {
                    for h in text_hits.into_iter() {
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
            }
        }

        Ok(out)
    }
}
