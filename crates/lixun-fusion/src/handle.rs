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
    // Retained for API compatibility and future RRF mode toggle (see
    // fused_search_streaming): callers configure k through `new()`, and re-
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

        // Phase 2: ANN in parallel (text + image), cancellable.
        let text_fut = ann.search_text(&query.text, ann_k);
        let image_fut = ann.search_image(&query.text, ann_k);

        let (text_res, image_res) = tokio::select! {
            _ = cancel.cancelled() => {
                tracing::debug!(target: "lixun_fusion", "ANN cancelled, skipping Final chunk");
                return Ok(());
            }
            res = async { tokio::join!(text_fut, image_fut) } => res,
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

        let _ = tx
            .send(FusionChunk {
                phase: Phase::Final,
                hits: out,
            })
            .await;

        Ok(())
    }
}
