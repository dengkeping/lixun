# Lixun Architecture

End-to-end block diagram showing how a search query flows through the system, from user interface to result delivery.

## Overview

Lixun uses a **3-way parallel fan-out** architecture inspired by Apple Spotlight and Microsoft Windows Search:
- **BM25** (Tantivy full-text index) for exact keyword matches
- **Text semantic** (bge-small-en-v1.5 embeddings) for natural language queries
- **Image semantic** (CLIP cross-modal embeddings) for visual content search

Results are merged via **Reciprocal Rank Fusion (RRF)** with k=60, the industry standard used by Apple Spotlight, Microsoft Azure AI Search, and Windows Search.

## Block Diagram

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                          USER INTERFACE LAYER                                │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                               │
│  ┌──────────────┐                              ┌──────────────┐             │
│  │  lixun-cli   │                              │  lixun-gui   │             │
│  │              │                              │              │             │
│  │  • search    │                              │  • GTK4 UI   │             │
│  │  • reindex   │                              │  • Layer     │             │
│  │  • status    │                              │    Shell     │             │
│  └──────┬───────┘                              └──────┬───────┘             │
│         │                                             │                      │
│         └─────────────────┬───────────────────────────┘                      │
│                           │                                                  │
│                           ▼                                                  │
│              Unix Domain Socket (protocol v3)                                │
│              /run/user/1000/lixun.sock                                       │
│              Frame: u32 BE length + JSON                                     │
│              Request::Search { q, limit, explain }                           │
│                                                                               │
└───────────────────────────────────────┬───────────────────────────────────────┘
                                        │
                                        ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                            DAEMON LAYER                                      │
│                         lixun-daemon (lixund)                                │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                               │
│  IPC Handler (main.rs:591-618)                                               │
│  ├─ listener.accept() loop                                                   │
│  ├─ tokio::spawn per client                                                  │
│  └─ deserialize Request → dispatch                                           │
│                                                                               │
│  Search Handler (main.rs:828-986)                                            │
│  ├─ search.search_with_breakdown(q, limit, explain)                          │
│  ├─ Stage-2 multipliers:                                                     │
│  │  • Frecency boost (recent queries get higher scores)                      │
│  │  • Query latch (last query's top hit gets 1.5× boost)                     │
│  ├─ Plugin fan-out (main.rs:879-907):                                        │
│  │  • Calculator: "2+2" → synthetic Hit                                      │
│  │  • Shell: "ls -la" → synthetic Hit                                        │
│  │  • Apps: query against .desktop entries                                   │
│  │  • Maildir/Thunderbird: query against email index                         │
│  └─ top_hit::select_top_hit (main.rs:942-967)                                │
│                                                                               │
│                           ▼                                                  │
│              HybridSearchHandle::fused_search()                              │
│              (lixun-fusion/src/handle.rs:79-177)                             │
│                                                                               │
└───────────────────────────────────────┬───────────────────────────────────────┘
                                        │
                                        ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                           FUSION LAYER                                       │
│                    3-Way Parallel Fan-Out                                    │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                               │
│  tokio::try_join!(lex_fut, text_fut, image_fut)                              │
│  (handle.rs:100-104)                                                         │
│                                                                               │
│  ┌─────────────────┐  ┌─────────────────┐  ┌─────────────────┐             │
│  │   BM25 Backend  │  │ Text ANN Backend│  │ Image ANN Backend│            │
│  │   fetch 10      │  │   fetch 40      │  │   fetch 40       │            │
│  │   (exact limit) │  │   (4× overfetch)│  │   (4× overfetch) │            │
│  └────────┬────────┘  └────────┬────────┘  └────────┬─────────┘             │
│           │                    │                     │                       │
│           └────────────────────┼─────────────────────┘                       │
│                                │                                             │
│                                ▼                                             │
│                    RRF Merge (k=60)                                          │
│                    (lixun-fusion/src/rrf.rs:33-63)                           │
│                    score = 1 / (k + rank)                                    │
│                                                                               │
│                                ▼                                             │
│                    Hydration Layer                                           │
│                    (handle.rs:150-174)                                       │
│                    doc_ids → Hit objects                                     │
│                                                                               │
│  ScoreBreakdown preservation:                                                │
│  • BM25 hits: full breakdown (category, prefix, acronym, recency, coord)     │
│  • ANN-only hits: score=0.0 marker, raw distance in bd.tantivy,             │
│                   degenerate 1.0 multipliers                                 │
│                                                                               │
└───────────────────────────────────────┬───────────────────────────────────────┘
                                        │
                                        ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                          BM25 BACKEND                                        │
│                    Tantivy Full-Text Index                                   │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                               │
│  LixunIndex::search_with_breakdown()                                         │
│  (lixun-index/src/lib.rs:323-478)                                            │
│                                                                               │
│  Query Parsing (lib.rs:848-929):                                             │
│  ├─ Tokenize via tantivy::tokenizer                                          │
│  ├─ Prefix detection: "firef" → "firef*"                                     │
│  ├─ Acronym detection: "ff" → "firefox"                                      │
│  └─ Build tantivy::query::BooleanQuery                                       │
│                                                                               │
│  Scoring Pipeline:                                                           │
│  ├─ BM25 base score (tantivy default)                                        │
│  ├─ Category boost:                                                          │
│  │  • Apps: 10×                                                              │
│  │  • Mail: 5×                                                               │
│  │  • Files: 1×                                                              │
│  ├─ Prefix match: 1.5×                                                       │
│  ├─ Acronym match: 2.0×                                                      │
│  ├─ Recency decay: exp(-age_days / 90)  (90-day half-life)                  │
│  └─ Coordination: (matched_terms / total_terms)                              │
│                                                                               │
│  ScoreBreakdown:                                                             │
│    final_score = base × category × prefix × acronym × recency × coord       │
│                                                                               │
│  Storage:                                                                    │
│  • Path: ~/.local/state/lixun/index/                                         │
│  • Size: 300-700 MB for 350k documents                                       │
│  • INDEX_VERSION: 10 (enforces rebuild on schema change)                     │
│                                                                               │
└───────────────────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────────────────┐
│                      SEMANTIC ANN BACKEND                                    │
│                    Sidecar Process Architecture                              │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                               │
│  lixun-semantic-worker (separate process)                                    │
│  ├─ Spawned by daemon at startup                                             │
│  ├─ IPC: Unix socket /run/user/1000/lixun/semantic-{pid}-{hash}.sock        │
│  ├─ Protocol v2: Cmd::SearchText / Cmd::SearchImage                          │
│  └─ Supervised with exponential backoff (1s → 60s max)                       │
│                                                                               │
│  ┌─────────────────────────────────────────────────────────────────────┐    │
│  │                    LanceDbAnnHandle                                  │    │
│  │              (lixun-semantic-worker/src/ann.rs)                      │    │
│  ├─────────────────────────────────────────────────────────────────────┤    │
│  │                                                                       │    │
│  │  search_text(query, k) → Vec<AnnHit>                                 │    │
│  │  ├─ Embed query via TextEmbedder (bge-small-en-v1.5, 384-dim)       │    │
│  │  └─ store.search_text(&vector, k) → LanceDB knn                      │    │
│  │                                                                       │    │
│  │  search_image(query, k) → Vec<AnnHit>                                │    │
│  │  ├─ Embed query via ClipTextEmbedder (CLIP text tower, 512-dim)     │    │
│  │  └─ store.search_image(&vector, k) → LanceDB knn                     │    │
│  │                                                                       │    │
│  └─────────────────────────────────────────────────────────────────────┘    │
│                                                                               │
│  ┌─────────────────────────────────────────────────────────────────────┐    │
│  │                      VectorStore                                     │    │
│  │           (lixun-semantic-worker/src/store.rs)                       │    │
│  ├─────────────────────────────────────────────────────────────────────┤    │
│  │                                                                       │    │
│  │  Storage: ~/.local/share/lixun/semantic/vectors/                     │    │
│  │  ├─ text_vectors.lance/  (bge-small-en-v1.5 embeddings)             │    │
│  │  └─ image_vectors.lance/ (CLIP vision embeddings)                    │    │
│  │                                                                       │    │
│  │  Operations:                                                          │    │
│  │  ├─ upsert_text_batch(docs, embeddings)                              │    │
│  │  ├─ upsert_image_batch(docs, embeddings)                             │    │
│  │  ├─ search_text(vector, k) → knn via LanceDB                         │    │
│  │  ├─ search_image(vector, k) → knn via LanceDB                        │    │
│  │  └─ delete(doc_ids)                                                  │    │
│  │                                                                       │    │
│  └─────────────────────────────────────────────────────────────────────┘    │
│                                                                               │
│  ┌─────────────────────────────────────────────────────────────────────┐    │
│  │                       Embedders                                      │    │
│  │          (lixun-semantic-worker/src/embedder.rs)                     │    │
│  ├─────────────────────────────────────────────────────────────────────┤    │
│  │                                                                       │    │
│  │  TextEmbedder                                                         │    │
│  │  ├─ Model: Xenova/bge-small-en-v1.5                                  │    │
│  │  ├─ Dim: 384                                                         │    │
│  │  └─ Use: text document embeddings                                    │    │
│  │                                                                       │    │
│  │  ImageEmbedder                                                        │    │
│  │  ├─ Model: Qdrant/clip-ViT-B-32-vision                               │    │
│  │  ├─ Dim: 512                                                         │    │
│  │  └─ Use: image file embeddings                                       │    │
│  │                                                                       │    │
│  │  ClipTextEmbedder                                                     │    │
│  │  ├─ Model: Qdrant/clip-ViT-B-32-text                                 │    │
│  │  ├─ Dim: 512 (same space as ImageEmbedder)                           │    │
│  │  └─ Use: cross-modal text→image search                               │    │
│  │                                                                       │    │
│  │  Model Cache: ~/.cache/lixun/fastembed/ (~400 MB)                    │    │
│  │                                                                       │    │
│  └─────────────────────────────────────────────────────────────────────┘    │
│                                                                               │
│  ┌─────────────────────────────────────────────────────────────────────┐    │
│  │                  Document Classification                             │    │
│  │            (lixun-semantic-worker/src/worker.rs:375-390)             │    │
│  ├─────────────────────────────────────────────────────────────────────┤    │
│  │                                                                       │    │
│  │  fn classify(doc: &UpsertedDoc) -> Channel {                         │    │
│  │      if doc.mime.starts_with("image/") {                             │    │
│  │          return Channel::Image;                                      │    │
│  │      }                                                                │    │
│  │      match doc.body {                                                │    │
│  │          Some(body) if !body.trim().is_empty() => Channel::Text,    │    │
│  │          _ => Channel::Skip,                                         │    │
│  │      }                                                                │    │
│  │  }                                                                    │    │
│  │                                                                       │    │
│  │  Channels:                                                            │    │
│  │  • Image: standalone image files (jpg/png/heic/etc)                  │    │
│  │  • Text: documents with body text (including OCR'd PDFs)             │    │
│  │  • Skip: empty/binary files                                          │    │
│  │                                                                       │    │
│  └─────────────────────────────────────────────────────────────────────┘    │
│                                                                               │
│  Batching & Flush:                                                            │
│  ├─ pending_text: Vec<UpsertedDoc>                                           │
│  ├─ pending_images: Vec<UpsertedDoc>                                         │
│  ├─ pending_deletes: Vec<String>                                             │
│  ├─ Flush triggers:                                                           │
│  │  • batch_size reached (default: 64)                                       │
│  │  • flush_period elapsed (default: 5s)                                     │
│  │  • Channel closed (daemon shutdown)                                       │
│  └─ Deduplication: pending_deletes sorted+deduped before flush               │
│                                                                               │
│  Supervisor (lixun-daemon/src/semantic_supervisor.rs):                       │
│  ├─ Crash detection via process exit                                         │
│  ├─ Exponential backoff: 1s → 2s → 4s → 8s → ... → 60s max                  │
│  └─ Restart with same socket path                                            │
│                                                                               │
└───────────────────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────────────────┐
│                          RESPONSE PATH                                       │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                               │
│  Daemon Serialization (main.rs:969-982):                                     │
│  ├─ HitsWithExtrasV3 {                                                       │
│  │    hits: Vec<Hit>,                                                        │
│  │    top_hit: Option<Hit>,                                                  │
│  │    query_time_ms: u64,                                                    │
│  │  }                                                                        │
│  │                                                                            │
│  │  Hit {                                                                    │
│  │    title: String,                                                         │
│  │    path: String,                                                          │
│  │    category: String,                                                      │
│  │    mime: Option<String>,                                                  │
│  │    mtime: Option<i64>,                                                    │
│  │    size: Option<u64>,                                                     │
│  │    body: Option<String>,  // snippet                                      │
│  │    score: f32,                                                            │
│  │    bd: ScoreBreakdown,                                                    │
│  │  }                                                                        │
│  │                                                                            │
│  └─ Frame as protocol v3: u32 BE length + JSON                               │
│                                                                               │
│  Unix Socket Write → lixun-cli / lixun-gui                                   │
│  ├─ lixun-cli: print hits to stdout                                          │
│  └─ lixun-gui: render in GTK4 list                                           │
│                                                                               │
└───────────────────────────────────────────────────────────────────────────────┘
```

## Key Metrics

| Metric | Value |
|--------|-------|
| **Query Latency (p50/p95)** | |
| BM25 only | 10-20 ms |
| BM25 + text semantic | 30-50 ms |
| Full 3-way (BM25 + text + image) | 40-60 ms |
| **Overfetch** | |
| BM25 | Exact limit (10) |
| Text ANN | 4× limit (40) |
| Image ANN | 4× limit (40) |
| **RRF Parameters** | |
| k constant | 60 (Apple Spotlight / MS Windows Search standard) |
| Formula | score = 1 / (k + rank) |
| **Index Size** | |
| Tantivy (350k docs) | 300-700 MB |
| LanceDB text vectors | Varies by corpus |
| LanceDB image vectors | Varies by image count |
| Model cache | ~400 MB |
| **Memory Usage** | |
| Daemon (idle) | 250 MB RSS |
| Daemon (indexing) | 1 GB RSS |
| Semantic worker | 1.7 GB RSS (ONNX Runtime + models) |

## Why No Query Classification?

Earlier versions (commit 790d2df, reverted in 2f283f3) attempted to classify queries as "text" vs "image" using CLIP anchor embeddings. This approach **failed** because:

1. **CLIP text space is asymmetric** for short queries — tokens like "firefox" or "README" map roughly equidistant from both image anchors ("a logo", "a screenshot") and text anchors ("source code", "a document").
2. The classifier returned `Modality::Both` for nearly all queries, causing image ANN's k=80 overfetch to drown BM25 exact matches.
3. Plain keyword queries like "firefox" returned only camera-roll photos instead of the Firefox application.

**Solution**: Fan out to all backends in parallel, let RRF merge decide. This matches Apple Spotlight and Microsoft Windows Search architecture — no upfront classification, just parallel execution + robust fusion.

The anchor classifier code remains in the codebase (`lixun-semantic-worker/src/query_router.rs`, `AnnHandle::classify_query` trait method) but is **unused** in the current fusion pipeline.

## References

- Apple WWDC24: "Support semantic search with Core Spotlight"
- Microsoft Learn: "Hybrid Search Overview"
- Cormack et al. (2009): "Reciprocal Rank Fusion outperforms Condorcet and individual rank learning methods" (SIGIR)
- [docs/search-fusion.md](search-fusion.md) — detailed fusion algorithm walkthrough
- [docs/wave-d-semantic.md](wave-d-semantic.md) — semantic worker setup and operations
