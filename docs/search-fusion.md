# Search fusion architecture

## Overview

Lixun's search uses a **Spotlight-style fan-out-and-merge** architecture:
every query runs in parallel against three retrieval backends (BM25 lexical,
text semantic, image semantic), then merges the results via Reciprocal Rank
Fusion (RRF). This design eliminates the need for query classification —
instead of trying to guess whether a short keyword is a text query or an
image query, we let all three backends compete and let RRF pick the best
results from each.

This architecture matches Apple Spotlight (WWDC24 "Support semantic search
with Core Spotlight") and Microsoft Windows Search/Recall. The key insight:
**fan-out is cheaper than classification**, especially when classification
is unreliable (short queries, code identifiers, ambiguous natural language).

## Three retrieval paths

### 1. BM25 lexical (always on)

Full-text search over Tantivy. Tokenizes the query, scores documents by
term frequency × inverse document frequency, applies category boosts (apps
get +100, email +50). Fast (~10-20ms), interpretable, excellent for exact
matches and keyword queries.

**Strengths:** Exact filename matches, code identifiers, short tokens,
keyword queries.

**Weaknesses:** Synonyms, paraphrases, natural language intent ("photos of
my dog" won't match `IMG_1234.jpg`).

### 2. Text semantic (opt-in, requires lixun-semantic-worker)

Dense vector retrieval via fastembed's `bge-small-en-v1.5` (384-dim) over
LanceDB. Embeds the query and searches the `text_vectors` table for nearest
neighbors by cosine similarity.

**Strengths:** Semantic similarity, paraphrases, natural language queries
("authentication code" matches "login logic").

**Weaknesses:** Short tokens, exact matches (embedding space is fuzzy),
computational cost (~20-30ms per query).

### 3. Image semantic (opt-in, requires lixun-semantic-worker)

Cross-modal text→image retrieval via CLIP (`clip-vit-b-32`, 512-dim). Embeds
the query with CLIP's text encoder and searches the `image_vectors` table
(populated by CLIP's vision encoder) for nearest neighbors.

**Strengths:** Natural language image queries ("photos of dogs", "screenshot
of terminal", "scanned invoice"), content-based retrieval (matches image
content, not just filename).

**Weaknesses:** CLIP was trained on (image, caption) pairs from the web, so
it works best for queries that sound like image captions. Abstract concepts
or non-visual queries return noise.

## Reciprocal Rank Fusion (RRF)

RRF merges three ranked lists into one. For each document, compute:

```
rrf_score = sum over all lists where doc appears: 1 / (k + rank_in_that_list)
```

Default `k = 60` (Microsoft Azure AI Search standard). Higher `k` flattens
the curve (more democratic), lower `k` amplifies top-ranked hits.

**Why RRF?**

- **Robust to score distribution mismatches.** BM25 scores are in [0, 500+],
  ANN distances are in [0, 2]. Naive score fusion (weighted sum) requires
  careful normalization and per-backend weight tuning. RRF only cares about
  rank, not raw scores.
- **No query classification needed.** A query for an application name gets
  BM25 hits (matching .desktop entries, source files, icons) ranked high,
  plus some image noise from CLIP ranked low. RRF naturally promotes the
  BM25 hits because they appear in top positions. No need to guess "is this
  a text query or image query?" upfront.
- **Graceful degradation.** If one backend returns garbage (e.g., CLIP
  returns 80 random JPGs for a short keyword query), those hits get low RRF
  scores because they only appear in one list at low ranks. BM25's exact
  match dominates.

## Hydration and score preservation

After RRF produces a merged ranking, we hydrate the top-N documents:

1. **BM25 hits:** Use the full `ScoreBreakdown` from Tantivy (includes
   category boost, term contributions). Preserve the original BM25
   `final_score` in the `Hit` struct so the GUI can display interpretable
   scores (e.g., a high three-digit BM25 score for an exact app match).
2. **ANN-only hits:** Hydrate via `index_service.hydrate_doc()`, set
   `score = 0.0` as a marker (semantic-suffix convention from earlier
   text-priority fusion), store raw ANN distance in `bd.tantivy` for
   debugging.

This preserves the user-facing score semantics: high scores = strong BM25
match, low scores = semantic fallback.

## Configuration

All three backends run in parallel by default when semantic is enabled. To
disable image search (e.g., on a headless server with no photos), set:

```toml
[semantic]
enabled = true
image_search = false  # Only text semantic, no CLIP
```

To tune RRF:

```toml
[semantic]
rrf_k = 60.0  # Default; higher = more democratic, lower = top-heavy
```

## Performance

Typical latency (p50/p95) on a mid-range laptop (4-core, NVMe SSD):

- **BM25 only** (semantic disabled): 10-20ms
- **BM25 + text semantic**: 30-50ms
- **BM25 + text + image (full 3-way)**: 40-60ms

The three backends run in parallel via `tokio::try_join!`, so total latency
is `max(bm25_time, text_ann_time, image_ann_time) + rrf_merge_time`. RRF
merge is ~1-2ms for typical result set sizes (20 BM25 + 80 text ANN + 80
image ANN).

## Why not query classification?

We tried it (commit 790d2df, reverted in 2f283f3). The approach: embed 10
image anchor phrases ("a photograph", "a screenshot", "a scanned document")
and 10 text anchor phrases ("source code", "an email message", "a
configuration file") at worker startup, then classify each query by
comparing its CLIP text embedding to both anchor groups.

**Why it failed:**

1. **CLIP text space is asymmetric for short queries.** CLIP was trained on
   (image, caption) pairs where captions are short like "a photo of a cat
   on a sofa". So CLIP's text encoder maps short tokens (single-word app
   names, file extensions, ticket codes) roughly equidistant from both image
   anchors and text anchors. The classifier returned `Modality::Both` for
   nearly every short query.
2. **Image ANN returns k=80 hits regardless of relevance.** When the
   classifier said "Both", fusion put image hits first (most selective),
   which filled the 10-slot output with random JPGs before BM25's exact
   matches got considered. Plain keyword queries returned only camera-roll
   photos.
3. **Classification is a single point of failure.** If the classifier
   mispredicts, the entire result set is wrong. Fan-out-and-merge is robust:
   even if one backend returns garbage, RRF demotes it because it only
   appears in one list at low ranks.

The anchor classifier remains in the codebase (worker's `query_router.rs`,
`AnnHandle::classify_query()` trait method) but is unused. Fusion calls all
three backends unconditionally.

## Comparison to other systems

| System | Architecture | Merge strategy |
|--------|--------------|----------------|
| **Lixun** | 3-way fan-out (BM25 + text ANN + image ANN) | RRF k=60 |
| **Apple Spotlight** | Parallel lexical + semantic (CSUserQuery) | Proprietary ML ranker |
| **Microsoft Windows Search** | Parallel full-text + vector | RRF (Azure AI Search) |
| **Microsoft Windows Recall** | Pure semantic (40+ local SLMs) | No lexical fallback |
| **Perplexity/You.com** | LLM reranking over web results | Cross-encoder on top-K |

Lixun is closest to Windows Search: fan-out to multiple backends, RRF merge,
graceful degradation when semantic is disabled.

## Future work

- **Cross-encoder reranking:** Run a lightweight cross-encoder (e.g.,
  `ms-marco-MiniLM-L6-v2`) over the top-20 RRF results for a final precision
  pass. Cost: ~50-100ms, but only on the already-filtered top-20.
- **Temporal queries:** "photos from new year's eve 2025" → extract date
  entities via NER, add tantivy range filters.
- **Multimodal queries:** "screenshot of an app showing a chat window" →
  combine CLIP image search with text filters.
- **Learned fusion weights:** Replace fixed RRF with a small learned ranker
  (LambdaMART, LightGBM) trained on click logs. Requires user opt-in for
  telemetry.

## See also

- [docs/architecture.md](architecture.md) — full block diagram from user interface to result delivery

## References

- Apple WWDC24: "Support semantic search with Core Spotlight"
  (https://developer.apple.com/videos/play/wwdc2024/10131/)
- Microsoft Learn: "Hybrid search using Reciprocal Rank Fusion (RRF)"
  (https://learn.microsoft.com/en-us/azure/search/hybrid-search-ranking)
- Windows Recall architecture blog (2024-05-20)
- Cormack et al. (2009): "Reciprocal Rank Fusion outperforms Condorcet and
  individual systems" (SIGIR)
