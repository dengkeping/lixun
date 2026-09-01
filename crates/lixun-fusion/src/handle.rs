//! `HybridSearchHandle` — public seam the daemon substitutes for the
//! lexical-only `SearchHandle` when hybrid search is enabled. The
//! method surface mirrors `SearchHandle` byte-for-byte so daemon
//! call sites compile against either type without conditionals.

use anyhow::Result;
use lixun_indexer::index_service::SearchHandle;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Phase of a streaming search result chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Initial chunk: BM25-only results, sent immediately. Provisional
    /// (no calculation/top_hit/explanations). GUI renders or skips if
    /// empty in hybrid mode (keeps old rows + spinner).
    Initial,
    /// Final chunk: full RRF-merged results after ANN completes.
    /// Authoritative (includes calculation/top_hit/explanations after
    /// daemon applies stage-2 ranking + plugin fan-out). GUI merges
    /// by stable Hit identity or full-replaces if no Initial.
    Final,
}

/// A chunk of search results from the streaming search API.
#[derive(Debug, Clone)]
pub struct FusionChunk {
    pub phase: Phase,
    pub hits: Vec<(lixun_core::Hit, lixun_core::ScoreBreakdown)>,
}

#[derive(Clone)]
pub struct HybridSearchHandle {
    inner: SearchHandle,
    ann: Option<Arc<dyn crate::ann::AnnHandle>>,
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

    /// Streaming search API. Returns a channel receiver that yields
    /// two chunks: Initial (BM25-only, immediate) and Final (full RRF
    /// after ANN completes). Lexical-only mode sends single Final chunk.
    ///
    /// Caller must provide a CancellationToken; when cancelled, ANN
    /// futures abort and only Initial chunk is sent (if not yet sent).
    ///
    /// Channel buffer size is 2 (Initial + Final). Caller must consume
    /// chunks or risk blocking the search task.
    pub async fn search_streaming(
        &self,
        query: &lixun_core::Query,
        cancel: CancellationToken,
    ) -> Result<mpsc::Receiver<FusionChunk>> {
        let (tx, rx) = mpsc::channel(2);

        if self.ann.is_none() {
            // Lexical-only mode: single Final chunk.
            let pairs = self.inner.search_with_breakdown(query).await?;
            let _ = tx
                .send(FusionChunk {
                    phase: Phase::Final,
                    hits: pairs,
                })
                .await;
            return Ok(rx);
        }

        // Hybrid mode: spawn task that sends Initial (BM25) immediately,
        // then Final (RRF) after ANN completes or cancellation.
        let query = query.clone();
        let handle = self.clone();
        tokio::spawn(async move {
            if let Err(e) = handle.fused_search_streaming(&query, tx, cancel).await {
                tracing::warn!(target: "lixun_fusion", error = %e, "fused_search_streaming failed");
            }
        });

        Ok(rx)
    }

    /// Backward-compat wrapper: collects Final chunk from streaming API.
    pub async fn search(&self, query: &lixun_core::Query) -> Result<Vec<lixun_core::Hit>> {
        let cancel = CancellationToken::new();
        let mut rx = self.search_streaming(query, cancel).await?;
        let mut final_hits = Vec::new();
        while let Some(chunk) = rx.recv().await {
            if chunk.phase == Phase::Final {
                final_hits = chunk.hits.into_iter().map(|(h, _)| h).collect();
                break;
            }
        }
        Ok(final_hits)
    }

    /// Backward-compat wrapper: collects Final chunk from streaming API.
    pub async fn search_with_breakdown(
        &self,
        query: &lixun_core::Query,
    ) -> Result<Vec<(lixun_core::Hit, lixun_core::ScoreBreakdown)>> {
        let cancel = CancellationToken::new();
        let mut rx = self.search_streaming(query, cancel).await?;
        let mut final_pairs = Vec::new();
        while let Some(chunk) = rx.recv().await {
            if chunk.phase == Phase::Final {
                final_pairs = chunk.hits;
                break;
            }
        }
        Ok(final_pairs)
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

    /// Batch hydration passthrough: one searcher, input order
    /// preserved, dead ids skipped. Used by the daemon's `Recents`
    /// handler to resolve frecency doc ids into presentable hits.
    pub async fn hydrate_docs(
        &self,
        doc_ids: Vec<String>,
    ) -> Result<Vec<(lixun_core::Hit, lixun_core::ScoreBreakdown)>> {
        self.inner.hydrate_docs(doc_ids).await
    }

    async fn fused_search_streaming(
        &self,
        query: &lixun_core::Query,
        tx: mpsc::Sender<FusionChunk>,
        cancel: CancellationToken,
    ) -> Result<()> {
        use std::collections::HashMap;

        let target_limit = query.limit.max(1) as usize;
        let ann_k = target_limit
            .saturating_mul(self.overfetch)
            .max(target_limit);

        let ann = self
            .ann
            .clone()
            .expect("fused_search_streaming called without ANN");

        // Phase 1: BM25-only, send Initial chunk immediately.
        let lex_pairs = self.inner.search_with_breakdown(query).await?;
        let _ = tx
            .send(FusionChunk {
                phase: Phase::Initial,
                hits: lex_pairs.clone(),
            })
            .await;

        // Phase 2: ANN in parallel (text + image) plus query-modality
        // classification, cancellable. Classification shares the CLIP
        // query embedding with the image search (worker-side cache), so
        // it adds no extra inference.
        let text_fut = ann.search_text(&query.text, ann_k);
        let image_fut = ann.search_image(&query.text, ann_k);
        let classify_fut = ann.classify_query(&query.text);

        let (text_res, image_res, modality_res) = tokio::select! {
            _ = cancel.cancelled() => {
                tracing::debug!(target: "lixun_fusion", "ANN cancelled, skipping Final chunk");
                return Ok(());
            }
            res = async { tokio::join!(text_fut, image_fut, classify_fut) } => res,
        };

        let text_hits = match text_res {
            Ok(hits) => hits,
            Err(e) => {
                tracing::debug!(target: "lixun_fusion", error = %e, "text ANN errored, falling back to BM25-only");
                Vec::new()
            }
        };
        let image_hits = match image_res {
            Ok(hits) => hits,
            Err(e) => {
                tracing::debug!(target: "lixun_fusion", error = %e, "image ANN errored, falling back to BM25-only");
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

        // Only FULL lexical matches carry RRF standing. Fallback-tier
        // hits (disjunctive refill — partial matches) are page filler:
        // letting them into the ranked list would hand half the fused
        // page to docs that merely contain one query word, crowding out
        // the semantic hits that actually answer the query.
        let bm25_ranked: Vec<(String, f32)> = lex_pairs
            .iter()
            .filter(|(_, bd)| !bd.lexical_fallback)
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

        // Modality-aware channel weights. Text (also the fallback when
        // the router is unavailable or errors) stays strictly neutral —
        // canonical RRF — so a degraded worker never changes ranking.
        // Image-intent queries lean the fusion toward CLIP image hits;
        // Both leans mildly. No channel is ever zeroed.
        let modality = modality_res.unwrap_or(lixun_mutation::Modality::Text);
        let weights = match modality {
            lixun_mutation::Modality::Image => (1.0, 0.8, 1.8),
            lixun_mutation::Modality::Both => (1.0, 1.0, 1.3),
            lixun_mutation::Modality::Text => (1.0, 1.0, 1.0),
        };
        tracing::debug!(
            target: "lixun_fusion",
            ?modality,
            "fusion: query modality weights (bm25, text, image) = {:?}",
            weights
        );

        let fused = crate::rrf::rrf_fuse_3way_weighted(
            &bm25_ranked,
            &text_ranked,
            &image_ranked,
            self.rrf_k,
            weights,
        );

        // The RRF order is authoritative for the Final chunk, but downstream
        // stage-2 (frecency/latch) multiplies `hit.score` and re-sorts, and
        // plugin hits compete on the same scale. Raw RRF scores have a very
        // flat dynamic range (~1/(k+rank)), which would let stage-2 dominate.
        // Instead, map fused rank positions onto the BM25 final-score ladder:
        // the hit RRF ranks #i gets the #i-th highest lexical score. Ordering
        // follows RRF exactly while the score distribution stage-2 sees is
        // identical to lexical-only mode.
        // Fallback-tier scores are excluded: a partial match's inflated
        // BM25 value must not become a rank slot's score, or the
        // daemon's post-stage-2 re-sort would promote whatever lands on
        // that slot right back up the page.
        let mut ladder: Vec<f32> = lex_pairs
            .iter()
            .filter(|(_, bd)| !bd.lexical_fallback)
            .map(|(_, bd)| bd.final_score)
            .collect();
        ladder.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
        let score_for_rank = |pos: usize, rrf_score: f32| -> f32 {
            if let Some(s) = ladder.get(pos) {
                *s
            } else if let Some(last) = ladder.last() {
                // Past the lexical list: decay below the weakest BM25 score so
                // appended ANN-only hits keep their relative RRF order.
                last * 0.95_f32.powi((pos + 1 - ladder.len()) as i32)
            } else {
                // Pure-ANN result set: only relative order matters.
                rrf_score
            }
        };

        let mut out: Vec<(lixun_core::Hit, lixun_core::ScoreBreakdown)> =
            Vec::with_capacity(target_limit);

        // Hydrate every ANN-only doc in ONE blocking hop up front —
        // the per-doc hydrate_doc loop was N sequential spawn_blocking
        // round trips.
        let missing_ids: Vec<String> = fused
            .iter()
            .take(target_limit)
            .filter(|(id, _)| !bm25_by_id.contains_key(id))
            .map(|(id, _)| id.clone())
            .collect();
        let hydrated_by_id: HashMap<String, (lixun_core::Hit, lixun_core::ScoreBreakdown)> =
            if missing_ids.is_empty() {
                HashMap::new()
            } else {
                self.inner
                    .hydrate_docs(missing_ids)
                    .await?
                    .into_iter()
                    .map(|(h, bd)| (h.id.0.clone(), (h, bd)))
                    .collect()
            };

        for (pos, (doc_id, rrf_score)) in fused.into_iter().take(target_limit).enumerate() {
            let in_bm25 = bm25_by_id.contains_key(&doc_id);
            let in_text_ann = text_hits.iter().any(|h| h.doc_id == doc_id);
            let in_image_ann = image_hits.iter().any(|h| h.doc_id == doc_id);
            let source = match (in_bm25, in_text_ann, in_image_ann) {
                (true, true, true) => "BM25+TEXT_ANN+IMAGE_ANN",
                (true, true, false) => "BM25+TEXT_ANN",
                (true, false, true) => "BM25+IMAGE_ANN",
                (true, false, false) => "BM25",
                (false, true, true) => "TEXT_ANN+IMAGE_ANN",
                (false, true, false) => "TEXT_ANN",
                (false, false, true) => "IMAGE_ANN",
                (false, false, false) => "HYDRATE",
            };
            tracing::debug!(
                target: "lixun_fusion",
                "fusion: hit doc_id={} source={} rrf_score={:.4}",
                doc_id,
                source,
                rrf_score
            );
            let (mut hit, mut bd) = if let Some(pair) = bm25_by_id.get(&doc_id) {
                pair.clone()
            } else if let Some(pair) = hydrated_by_id.get(&doc_id) {
                pair.clone()
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
            }
            let assigned = score_for_rank(pos, rrf_score);
            hit.score = assigned;
            bd.final_score = assigned;
            out.push((hit, bd));
        }

        // Pad the page with fallback-tier lexical hits only when fusion
        // (strong lexical + ANN) could not fill it. They keep their own
        // ladder positions after every fused hit, so they always render
        // below the relevant results.
        if out.len() < target_limit {
            let in_out: std::collections::HashSet<&str> =
                out.iter().map(|(h, _)| h.id.0.as_str()).collect();
            let filler: Vec<(lixun_core::Hit, lixun_core::ScoreBreakdown)> = lex_pairs
                .iter()
                .filter(|(h, bd)| bd.lexical_fallback && !in_out.contains(h.id.0.as_str()))
                .take(target_limit - out.len())
                .cloned()
                .collect();
            // Decay below the weakest fused hit so filler stays at the
            // bottom through the daemon's re-sort.
            let floor = out.last().map(|(h, _)| h.score).unwrap_or(1.0);
            for (i, (mut hit, mut bd)) in filler.into_iter().enumerate() {
                let assigned = floor * 0.95_f32.powi(i as i32 + 1);
                hit.score = assigned;
                bd.final_score = assigned;
                out.push((hit, bd));
            }
        }

        let _ = tx
            .send(FusionChunk {
                phase: Phase::Final,
                hits: out,
            })
            .await;

        Ok(())
    }
}
