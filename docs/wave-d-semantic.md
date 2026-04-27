# Wave D — Semantic search

## Overview

Lixun's hybrid search combines two retrieval paths: lexical scoring
(BM25 over Tantivy, always on) and dense-vector semantic similarity
(opt-in). When both paths are active the daemon fuses their results
with Reciprocal Rank Fusion (RRF, default `k = 60`) so a hit ranked
well by either side surfaces in the merged list. The semantic plugin
runs in-process inside `lixund` and is gated twice: once at compile
time behind the `semantic` Cargo feature, and once at runtime behind
`[semantic] enabled = true` in the user config. Without both gates
the daemon behaves exactly like a pure-lexical build — no vector
index is opened, no models are downloaded.

## Requirements

- **GLIBC ≥ 2.27.** ONNX Runtime (loaded by the embedder on first
  vector op) refuses to initialise on older systems. Check with
  `ldd --version`.
- **Disk.** Two stores grow with use:
  - Model cache (~100–500 MB depending on `text_model`) at
    `$FASTEMBED_CACHE_DIR`. The shipped systemd unit pins this to
    `~/.cache/lixun/fastembed/`.
  - Vector index at `~/.local/share/lixun/vectors/`. Two LanceDB
    tables (`text_vectors`, `image_vectors`); size scales roughly
    linearly with corpus size.
- **A `lixund` binary built with `--features semantic`.** Arch
  users get this for free by installing the `lixun-bin` package,
  which ships a daemon compiled with `_features="semantic"`. From
  source: `cargo build -p lixun-daemon --bin lixund --features semantic`.

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
`~/.local/share/lixun/vectors/`, downloads the configured models
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
