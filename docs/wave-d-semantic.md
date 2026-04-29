# Wave D — Semantic search

> **See also:** [`docs/search-fusion.md`](search-fusion.md) — the Spotlight-style
> 3-way fan-out + RRF architecture (BM25 + text semantic + image semantic),
> shipped in v0.6.0. This document focuses on worker setup, model management,
> and operations; `search-fusion.md` covers the fusion layer and why we don't
> use a query classifier.

## Overview

Lixun's hybrid search combines three retrieval paths: lexical scoring
(BM25 over Tantivy, always on), dense-vector text semantic similarity
(opt-in, `bge-small-en-v1.5`), and dense-vector cross-modal image
semantic similarity (opt-in, CLIP `clip-vit-b-32`). When semantic is
enabled, all three paths run in parallel for every query and the daemon
fuses their results with Reciprocal Rank Fusion (RRF, default `k = 60`)
— same architecture Apple Spotlight and Microsoft Windows Search use.
A hit ranked well by any of the three backends surfaces in the merged
list. The heavy ML stack (ONNX Runtime, LanceDB, fastembed) lives in a
separate `lixun-semantic-worker` sidecar process, not in `lixund`
itself; the daemon talks to the worker over IPC. Semantic is gated by
two conditions: a reachable `lixun-semantic-worker` binary on disk
(daemon probes at startup) and `[semantic] enabled = true` in the
user config (the daemon-side stub defaults to disabled, matching
the legacy plugin's opt-in semantics). Without both, the daemon
behaves exactly like a pure-lexical build — no vector index is
opened, no models are downloaded, no worker is spawned.

## Requirements

- **GLIBC ≥ 2.27.** ONNX Runtime (loaded by the embedder on first
  vector op) refuses to initialise on older systems. Check with
  `ldd --version`.
- **Disk.** Two stores grow with use:
  - Model cache (~400 MB for default text + image models) at
    `$FASTEMBED_CACHE_DIR`. The shipped systemd unit pins this to
    `~/.cache/lixun/fastembed/`.
  - Vector index at `~/.local/share/lixun/semantic/vectors/`. Two LanceDB
    tables (`text_vectors`, `image_vectors`); size scales roughly
    linearly with corpus size.
- **The `lixun-semantic-worker` sidecar binary.** The daemon probes
  for it at startup in this order: the `LIXUN_SEMANTIC_WORKER` env
  var, then any `lixun-semantic-worker` on `$PATH`, then
  `/usr/lib/lixun/lixun-semantic-worker`. Arch users get this for
  free by installing the `lixun-bin` package. From source:
  `cargo build --release -p lixun-semantic-worker` then drop the
  resulting `target/release/lixun-semantic-worker` somewhere
  reachable.

## Enabling semantic search

Add the following block to your lixun config file (the same TOML
file the daemon already reads — see existing lixun config docs and
`lixun-cli --help` for its location; the daemon prints the resolved
path on startup):

```toml
[semantic]
enabled = true
text_model = "bge-small-en-v1.5"
image_model = "clip-vit-b-32"
batch_size = 32
flush_ms = 2000
min_image_side_px = 300
backfill_on_start = false
rrf_k = 60.0
cache_subdir = "fastembed"
```

Restart the daemon after editing:

```sh
systemctl --user restart lixund
```

On the next start, the semantic plugin opens the vector store under
`~/.local/share/lixun/semantic/vectors/`, downloads the configured models
into `$FASTEMBED_CACHE_DIR` if they are not yet cached, and starts
embedding new documents as the indexer commits them.

## First-run backfill

Documents indexed *after* `[semantic] enabled = true` are embedded
automatically as part of the normal commit path. Documents already
in the lexical index from earlier runs are not — they need an
explicit one-time backfill:

```sh
lixun-cli semantic backfill
```

The command streams every existing document through the embedder
and writes vectors into the LanceDB tables. Progress is journalled
to `~/.local/state/lixun/semantic-backfill.sqlite`, so the operation
is resume-safe: if interrupted (Ctrl-C, daemon restart, machine
reboot), re-running the same command picks up where it left off
instead of starting over.

Backfill is CPU-bound and can take a long time on large corpora.
It is safe to run in the background while the daemon serves
queries normally.

## Disk usage

- **Models** — `$FASTEMBED_CACHE_DIR` (the systemd unit pins this to
  `~/.cache/lixun/fastembed/`). Roughly 100–500 MB depending on the
  selected `text_model`; `bge-small-en-v1.5` is on the smaller end
  of that range, larger BGE variants approach the upper end.
- **Vector index** — `~/.local/share/lixun/vectors/`. Two LanceDB
  tables: `text_vectors` (dimension 384 for the default
  `bge-small-en-v1.5`; 1024 if you switch to `bge-m3`) and
  `image_vectors` (dimension 512 for `clip-vit-b-32`). Both use
  IVF_PQ compression. Size scales roughly linearly with document
  count.
- **Backfill journal** — `~/.local/state/lixun/semantic-backfill.sqlite`.
  A few MB even for large corpora.

## Disabling semantic search

Set `enabled = false` in `[semantic]` and restart the daemon:

```sh
systemctl --user restart lixund
```

The daemon will skip the semantic plugin entirely on the next
start; queries fall back to pure-lexical results. The vector index
and the model cache are **not** removed automatically. Reclaim the
disk manually if you want it back:

```sh
rm -rf ~/.local/share/lixun/vectors/
rm -rf ~/.cache/lixun/fastembed/
```

(The backfill journal at
`~/.local/state/lixun/semantic-backfill.sqlite` is small and can be
left in place; deleting it just means a future re-enable will
re-embed everything from scratch.)

## Troubleshooting

- **"GLIBC too old" / ONNX Runtime fails to load.** Verify
  `ldd --version` reports ≥ 2.27. Older distros need a newer
  glibc or a rebuild against your system's runtime.
- **Models won't download.** Confirm `$FASTEMBED_CACHE_DIR`
  (default `~/.cache/lixun/fastembed/` under the shipped unit) is
  writable, and that the daemon has working network access on
  first start. Subsequent starts work offline once the cache is
  populated.
- **Backfill is slow.** Expected — embedding is CPU-bound. If
  RAM allows, raise `batch_size` in `[semantic]` to feed the
  embedder more work per batch and amortise per-batch overhead.
- **Vector index keeps growing after deletes.** LanceDB tables
  retain rows for tombstoned docs until compaction; this is
  cosmetic, not a correctness issue. Size pressure on the index
  is bounded by your live corpus plus a small delta.
- **Some mail messages skipped during backfill (`body_present=false`
  in trace logs).** Expected. The Thunderbird source reads body
  text from `messagesText_content.c0body` in
  `global-messages-db.sqlite` (gloda). For messages where gloda
  did not produce a full-text row (HTML-only newsletters,
  attachment-only messages, content-type filtering, gloda
  queueing), the LEFT JOIN returns NULL and the message has no
  embeddable text. The semantic worker correctly skips such
  documents — there is nothing to embed. Compare
  `last_backfill_total` vs `last_backfill_submitted` in
  `~/.local/state/lixun/semantic-backfill.sqlite` to see how many
  documents fell into this bucket.
