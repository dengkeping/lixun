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
        self.search_streaming_in_category(query, None, cancel).await
    }

    pub async fn search_streaming_in_category(
        &self,
        query: &lixun_core::Query,
        category: Option<lixun_core::Category>,
        cancel: CancellationToken,
    ) -> Result<mpsc::Receiver<FusionChunk>> {
        let (tx, rx) = mpsc::channel(2);

        if self.ann.is_none() {
            // Lexical-only mode: single Final chunk.
            let pairs = self.inner.search_in_category(query, category).await?;
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
            if let Err(e) = handle
                .fused_search_streaming(&query, category, tx, cancel)
                .await
            {
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

    pub fn document_count(&self) -> u64 {
        self.inner.document_count()
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
        category: Option<lixun_core::Category>,
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
        let lex_pairs = self.inner.search_in_category(query, category).await?;
        let _ = tx
            .send(FusionChunk {
                phase: Phase::Initial,
                hits: lex_pairs.clone(),
            })
            .await;

        // Only the lexical parser can enforce scoped terms, phrases and exclusions.
        if lixun_core::query_uses_operators(&query.text) {
            let _ = tx
                .send(FusionChunk {
                    phase: Phase::Final,
                    hits: lex_pairs,
                })
                .await;
            return Ok(());
        }

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

        let mut text_hits = match text_res {
            Ok(hits) => hits,
            Err(e) => {
                tracing::debug!(target: "lixun_fusion", error = %e, "text ANN errored, falling back to BM25-only");
                Vec::new()
            }
        };
        let mut image_hits = match image_res {
            Ok(hits) => hits,
            Err(e) => {
                tracing::debug!(target: "lixun_fusion", error = %e, "image ANN errored, falling back to BM25-only");
                Vec::new()
            }
        };

        if let Some(category) = category
            && !(text_hits.is_empty() && image_hits.is_empty())
        {
            let ids = text_hits
                .iter()
                .chain(&image_hits)
                .map(|hit| hit.doc_id.clone())
                .collect();
            let allowed = self.inner.ids_in_category(ids, category).await?;
            text_hits.retain(|hit| allowed.contains(&hit.doc_id));
            image_hits.retain(|hit| allowed.contains(&hit.doc_id));
        }

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
        //
        // Navigational pin: a filename-shaped query ("golf-cart-vvp",
        // "IMG_1234", "report.pdf") names a document — the CLIP router
        // only sees the WORDS ("golf cart" reads as image-intent) and
        // would tilt the fusion toward photos of the thing instead of
        // the thing itself. Pin such queries to neutral Text weights;
        // bare words ("sunset") still classify freely.
        let modality = if looks_navigational(&query.text) {
            tracing::debug!(
                target: "lixun_fusion",
                "fusion: navigational query — modality pinned to Text"
            );
            lixun_mutation::Modality::Text
        } else {
            modality_res.unwrap_or(lixun_mutation::Modality::Text)
        };
        let weights = channel_weights(&query.text, modality, &text_hits, &image_hits);
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

        // Precision-first invariant: when the query precision-matches a
        // document lexically (title prefix / exact-title boost), docs
        // with lexical standing outrank ANN-only docs wholesale — a
        // precisely-named doc must not need its own semantic leg to
        // beat generic CLIP neighbours (see rrf::precision_first).
        let precision_ids: std::collections::HashSet<String> = lex_pairs
            .iter()
            .filter(|(_, bd)| {
                !bd.lexical_fallback && (bd.prefix_mult > 1.0 || bd.exact_title_mult > 1.0)
            })
            .map(|(h, _)| h.id.0.clone())
            .collect();
        let lexical_ids: std::collections::HashSet<String> =
            bm25_ranked.iter().map(|(id, _)| id.clone()).collect();
        let mut fused = crate::rrf::precision_first(fused, &precision_ids, &lexical_ids);
        prioritize_filename_words(
            &mut fused,
            &query.text,
            lex_pairs
                .iter()
                .filter(|(hit, bd)| {
                    hit.category == lixun_core::Category::File && !bd.lexical_fallback
                })
                .map(|(hit, _)| (hit.id.0.as_str(), hit.title.as_str())),
            &precision_ids,
        );

        // Preserve category priority before the fused page is truncated.
        // Only full lexical matches carry priority: a semantic neighbour
        // alone does not establish that the query names a searchable item.
        let priorities: HashMap<&str, i8> = lex_pairs
            .iter()
            .filter(|(_, bd)| !bd.lexical_fallback)
            .map(|(hit, bd)| (hit.id.0.as_str(), bd.rank_tier(hit.category)))
            .collect();
        prioritize_fused(&mut fused, &priorities);

        // The tiered fusion order defines the Final chunk, but downstream
        // stage-2 (frecency/latch) multiplies `hit.score` and re-sorts, and
        // plugin hits compete on the same scale. Raw RRF scores have a very
        // flat dynamic range (~1/(k+rank)), which would let stage-2 dominate.
        // Instead, map fused rank positions onto the BM25 final-score ladder:
        // the hit at position #i gets the #i-th highest lexical score. Ordering
        // follows category priority then RRF; stage-2 sees the same score
        // distribution as lexical-only mode.
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
        // Equal lexical scores are common in repetitive corpora. Preserve the
        // fused order through the final score sort, including tied BM25 slots.
        for i in 1..ladder.len() {
            ladder[i] = ladder[i].min(ladder[i - 1].next_down());
        }

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
            tracing::debug!(target: "lixun_fusion", source, rrf_score, "fusion result");
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
            // A lexical refill candidate can independently qualify through ANN.
            // Only the filler appended below remains in the partial-match tier.
            bd.lexical_fallback = false;
            let assigned = score_for_rank(&ladder, pos, rrf_score);
            hit.score = assigned;
            bd.final_score = assigned;
            out.push((hit, bd));
        }

        append_lexical_fallbacks(&mut out, &lex_pairs, target_limit);

        if tracing::enabled!(target: "lixun_fusion::ranks", tracing::Level::TRACE) {
            let ranks: Vec<_> = out
                .iter()
                .map(|(hit, _)| {
                    let rank = |channel: &[(String, f32)]| {
                        channel
                            .iter()
                            .position(|(id, _)| id == &hit.id.0)
                            .map_or(0, |i| i + 1)
                    };
                    [rank(&bm25_ranked), rank(&text_ranked), rank(&image_ranked)]
                })
                .collect();
            tracing::trace!(target: "lixun_fusion::ranks", ?ranks, "post-fusion positions");
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

/// A single word in a filename can identify the file even when its semantic
/// embedding is missing. Earlier words take precedence over later ones.
fn prioritize_filename_words<'a>(
    fused: &mut [(String, f32)],
    query: &str,
    filenames: impl Iterator<Item = (&'a str, &'a str)>,
    precision_ids: &std::collections::HashSet<String>,
) {
    use lixun_index::normalize::normalize_for_match;
    let word = normalize_for_match(query);
    if word.is_empty() || !word.chars().all(char::is_alphanumeric) {
        return;
    }
    let positions: std::collections::HashMap<_, _> = filenames
        .filter_map(|(id, title)| {
            let stem = std::path::Path::new(title).file_stem()?.to_str()?;
            let position = normalize_for_match(stem)
                .split(|ch: char| !ch.is_alphanumeric())
                .filter(|token| !token.is_empty())
                .position(|token| token == word)?;
            Some((id, position))
        })
        .collect();
    if !positions.is_empty() {
        fused.sort_by_key(|(id, _)| {
            if precision_ids.contains(id) {
                (0, 0)
            } else if let Some(&position) = positions.get(id.as_str()) {
                (1, position)
            } else {
                (2, 0)
            }
        });
    }
}

/// Stable tier ordering leaves fusion relevance untouched within each tier.
fn prioritize_fused(fused: &mut [(String, f32)], priorities: &std::collections::HashMap<&str, i8>) {
    fused.sort_by_key(|(id, _)| {
        std::cmp::Reverse(priorities.get(id.as_str()).copied().unwrap_or(0))
    });
}

fn score_for_rank(ladder: &[f32], pos: usize, rrf_score: f32) -> f32 {
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
}

fn append_lexical_fallbacks(
    out: &mut Vec<(lixun_core::Hit, lixun_core::ScoreBreakdown)>,
    lex_pairs: &[(lixun_core::Hit, lixun_core::ScoreBreakdown)],
    target_limit: usize,
) {
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
}

fn lead_separation(hits: &[lixun_mutation::AnnHit]) -> f32 {
    match hits {
        // A singleton has no observed runner-up. A weak prior preserves
        // image retrieval in small collections while letting well-separated
        // text neighbours win ambiguous queries.
        [only] if only.distance.is_finite() => 0.18,
        [first, second, ..]
            if first.distance.is_finite()
                && second.distance.is_finite()
                && second.distance > 0.0 =>
        {
            ((second.distance - first.distance) / second.distance).clamp(0.0, 1.0)
        }
        _ => 0.0,
    }
}

/// Filename-shaped query heuristic for the navigational modality pin:
/// a single token joined by separator characters (`-`, `_`, `.`, `/`)
/// reads as an identifier the user is naming, not a scene they are
/// describing. Multi-word queries and bare words are NOT navigational —
/// those legitimately belong to the modality router.
fn looks_navigational(query: &str) -> bool {
    let t = query.trim();
    !t.is_empty() && !t.contains(char::is_whitespace) && t.contains(['-', '_', '.', '/'])
}

fn channel_weights(
    query: &str,
    modality: lixun_mutation::Modality,
    text_hits: &[lixun_mutation::AnnHit],
    image_hits: &[lixun_mutation::AnnHit],
) -> (f32, f32, f32) {
    match explicit_media_intent(query).unwrap_or(modality) {
        lixun_mutation::Modality::Image => (1.0, 0.8, 2.0),
        // A rank-one neighbour exists even for a channel with no clear
        // match. For ambiguous intent use the separation of the leading
        // neighbours within each channel, not raw cross-model distances.
        lixun_mutation::Modality::Both => (
            1.0,
            1.0 + lead_separation(text_hits),
            1.0 + 1.3 * lead_separation(image_hits),
        ),
        lixun_mutation::Modality::Text => (1.0, 1.0, 1.0),
    }
}

fn explicit_media_intent(query: &str) -> Option<lixun_mutation::Modality> {
    let text = query.trim().to_lowercase();
    if looks_navigational(&text) {
        return None;
    }
    let photos = [
        "photo of ",
        "photos of ",
        "photograph of ",
        "photographs of ",
        "pictures of ",
        "picture of ",
        "фото ",
        "фотография ",
        "фотографии ",
    ];
    if photos.iter().any(|prefix| text.starts_with(prefix)) || text.ends_with("的照片") {
        Some(lixun_mutation::Modality::Image)
    } else if [
        "article about ",
        "articles about ",
        "статья о ",
        "статья об ",
    ]
    .iter()
    .any(|prefix| text.starts_with(prefix))
        || text.ends_with("的文章")
    {
        Some(lixun_mutation::Modality::Text)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{looks_navigational, prioritize_fused};

    #[test]
    fn single_word_in_filename_survives_semantic_fusion() {
        let files = [
            ("suffix", "outline.маяк"),
            ("substring", "У_маяка.txt"),
            ("copy", "2.Карта-маяк_севера.pdf"),
            ("original", "Карта-маяк_севера.pdf"),
            ("prefix", "Маяк севера.pdf"),
        ];
        let initial: Vec<_> = [
            "semantic",
            "suffix",
            "substring",
            "copy",
            "original",
            "prefix",
        ]
        .into_iter()
        .map(|id| (id.to_owned(), 1.0))
        .collect();
        let precision = ["prefix".to_owned()].into();
        let mut fused = initial.clone();
        super::prioritize_filename_words(&mut fused, " МАЯК ", files.into_iter(), &precision);
        let ids: Vec<_> = fused.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "prefix",
                "original",
                "copy",
                "semantic",
                "suffix",
                "substring"
            ]
        );
        for query in ["фото маяка", "pdf", "мая"] {
            let mut fused = initial.clone();
            super::prioritize_filename_words(&mut fused, query, files.into_iter(), &precision);
            assert_eq!(fused, initial);
        }
    }

    #[test]
    fn explicit_photo_intent_beats_duplicate_text_votes() {
        use lixun_mutation::Modality;
        for q in ["Photos of birds", "фото птиц", "鸟的照片"] {
            let weights = super::channel_weights(q, Modality::Both, &[], &[]);
            let text = vec![(String::from("article"), 1.0)];
            let images: Vec<_> = (0..5).map(|i| (format!("photo-{i}"), 1.0)).collect();
            let ordered = crate::rrf::rrf_fuse_3way_weighted(&text, &text, &images, 60.0, weights);
            assert!(
                ordered[..5].iter().all(|(id, _)| id.starts_with("photo-")),
                "{q}"
            );
        }
        assert_eq!(super::explicit_media_intent("photo.jpg"), None);
        assert_eq!(
            super::explicit_media_intent("article about photographs"),
            Some(Modality::Text)
        );
    }

    #[test]
    fn priority_match_survives_semantic_page_cutoff() {
        let lexical = vec![("app:viewer".into(), 1.0)];
        let semantic = vec![("fs:photo".into(), 0.1), ("mail:note".into(), 0.2)];
        let mut fused = crate::rrf::rrf_fuse_3way_weighted(
            &lexical,
            &semantic,
            &semantic,
            60.0,
            (1.0, 1.0, 1.8),
        );
        assert_ne!(
            fused[0].0, "app:viewer",
            "semantic votes reproduce the page-cutoff failure"
        );
        let original_scores: std::collections::HashMap<_, _> = fused.iter().cloned().collect();
        prioritize_fused(&mut fused, &[("app:viewer", 1)].into());
        assert_eq!(
            fused[0].0, "app:viewer",
            "priority must apply before taking the page"
        );
        assert_eq!(fused[1].0, "fs:photo");
        assert_eq!(fused[2].0, "mail:note");
        for (id, score) in fused {
            assert_eq!(
                score, original_scores[&id],
                "tiering must not alter relevance scores"
            );
        }
    }

    #[test]
    fn fusion_order_is_unchanged_without_priority_matches() {
        let mut fused = vec![("mail:note".into(), 0.5), ("fs:photo".into(), 0.3)];
        let expected = fused.clone();
        prioritize_fused(&mut fused, &Default::default());
        assert_eq!(fused, expected);
    }

    #[test]
    fn navigational_detects_separator_joined_tokens() {
        assert!(looks_navigational("golf-cart-vvp"));
        assert!(looks_navigational("IMG_1234"));
        assert!(looks_navigational("report.pdf"));
        assert!(looks_navigational("src/main.rs"));
        assert!(looks_navigational("  golf-cart-vvp  "));
    }

    #[test]
    fn navigational_rejects_natural_language_and_bare_words() {
        assert!(!looks_navigational("photos of dogs"));
        assert!(!looks_navigational("sunset"));
        assert!(!looks_navigational("golf cart vvp"));
        assert!(!looks_navigational(""));
        assert!(!looks_navigational("   "));
        assert!(!looks_navigational("my-file with spaces"));
    }
}
