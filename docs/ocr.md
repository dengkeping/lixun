# OCR — operator guide

Optical character recognition runs as an opt-in, idle-gated companion to the
main indexer. When enabled it extracts text from image files and image-only
PDF pages via `tesseract`, persists the result in the on-disk extract cache,
and upserts the body into Tantivy so the file becomes searchable by content
instead of by filename alone.

This document covers enabling OCR, the runtime model, and the known
behavioural edges surfaced by the v1.1 smoke run on 2026-04-25.

## Enabling OCR

OCR is off by default. Add the following to
`~/.config/lixun/config.toml` (see `docs/config.example.toml` for the full
reference):

```toml
[ocr]
enabled = true
languages = ["eng"]       # or ["eng", "rus", "chi_sim"], …
min_image_side_px = 256   # images smaller than this on BOTH axes are skipped
psi_avg10_threshold = 20  # worker ticks only while /proc/pressure/cpu some avg10 < this
```

Install `tesseract` plus the language packs you need. Distro package names
are listed in the top-level `README.md`. Without the requested language
packs the daemon auto-disables OCR with a single warning — you do not need
to gate the config section by hostname.

Restart the daemon after editing the config:

```sh
systemctl --user restart lixund
```

## Runtime model

```
┌───────────────┐   extract returns Ok(None)   ┌───────────────┐
│  lixun-sources│ ──────────────────────────►  │  ocr_queue.db │
│  (fs, tbird)  │   on OCR-candidate ext       │  (SQLite WAL) │
└───────────────┘                              └───────┬───────┘
                                                       │ peek_next (1 at a time)
                                                       ▼
                                               ┌───────────────┐
                                               │  ocr_tick     │
                                               │  PSI-gated    │
                                               │  worker       │
                                               └───────┬───────┘
                                                       │ tesseract subprocess
                                                       │ (OMP_THREAD_LIMIT=1,
                                                       │  nice, ioprio idle)
                                                       ▼
                                               ┌───────────────┐
                                               │ extract cache │
                                               │ + UpsertBody  │
                                               └───────────────┘
```

Key properties:

- **Persistent queue.** State lives in
  `~/.local/state/lixun/ocr-queue.db`. Progress survives daemon restarts
  and SIGKILL. Schema: `ocr_queue(doc_id PK, path, mtime, size, ext,
  enqueued_at, attempts, last_error)`.
- **Serial drain.** One tesseract subprocess at a time by default. The
  worker only runs while CPU pressure is low (via `/proc/pressure/cpu`
  `some avg10`) and while the main indexer is not actively draining a
  batch.
- **Child process discipline.** Every subprocess spawned by the extract
  layer gets `OMP_THREAD_LIMIT=1`, `nice -n 19`, and `ioprio idle` via
  `pre_exec`, so OCR cannot starve an interactive foreground workload.
- **Short-circuit re-enqueue (DB-16).** Before enqueueing a file, the
  fs source checks the live index via a `HasBody` adapter. Files that
  already have a non-empty body skip enqueue entirely, so reindex runs
  do not redo OCR for files already indexed through a previous OCR
  pass.
- **Extract cache key.** Path-keyed for fs, content-hashed for
  Thunderbird attachments. Cache lives under
  `~/.cache/lixun/extract/`; a periodic sweep reaps entries older than
  30 days.

## Monitoring

```sh
# Queue depth (instant count)
sqlite3 ~/.local/state/lixun/ocr-queue.db 'SELECT COUNT(*) FROM ocr_queue;'

# Breakdown by extension
sqlite3 ~/.local/state/lixun/ocr-queue.db \
  'SELECT ext, COUNT(*), ROUND(AVG(size)/1024.0, 1) AS avg_kb
   FROM ocr_queue GROUP BY ext ORDER BY COUNT(*) DESC LIMIT 10;'

# Failed rows
sqlite3 ~/.local/state/lixun/ocr-queue.db \
  'SELECT path, attempts, last_error FROM ocr_queue
   WHERE last_error IS NOT NULL LIMIT 20;'

# Drain activity in the daemon log
journalctl --user -u lixund.service -f | grep -E 'ocr ok|ocr fail|ocr tick'
```

A dedicated `lixun status --ocr` subcommand is tracked as a v1.2 item
(see below).

## v1.1 smoke findings (2026-04-25)

The following behaviours were observed during a full-home
`lixun reindex` smoke on a ~311k-file tree with photo and icon-heavy
subdirectories. The daemon at `6c77e6b` (tip of main) was correct —
jobs were persisted, drained idle-gated, and completed — but three
design edges cause disproportionate queue growth and disproportionate
repeat work. Fixes are deferred to v1.2.

### Finding 1 — Body clobber on reindex with empty cache HIT

**Symptom.** After an OCR pass populated `body` for an image-only PDF,
running `lixun reindex` a second time re-enqueued the same file and
re-ran tesseract.

**Root cause.** `reindex_full` wipes the fs manifest and treats every
file as changed. For an image-only PDF, `extract_content(path)` returns
`Ok(None)` (the cached path-keyed extract produced by `pdftotext` is
empty — the original, pre-OCR content). The fs source upserts a
`Document` with `body: None`, which replaces the OCR-recovered body in
Tantivy. The enqueue-time `HasBody` gate (DB-16) then correctly sees
no body and re-enqueues.

**Impact.** Every `lixun reindex` on a directory with previously
OCR-recovered files redoes all of that OCR work.

**Fix direction (v1.2, Option A).** At fs upsert time, when the
cache returns `Ok(None)` for a file that already has a non-empty body
in the live index, preserve the existing body instead of clobbering
it. Requires extending the existing `HasBody` trait (or adding a
sibling `GetBody` trait) with `fn get_body(&self, doc_id) ->
Option<String>`. `LixunIndex::get_body_by_id` already exists;
`SearchHandle` gets a thin adapter. Estimated ~150 LOC + 3 tests.

**Option B (rejected for now).** Partial-upsert support at the
Tantivy mutation level would allow a metadata-only update. Tantivy's
replace-by-id semantics force a full-doc rebuild, so partial upsert
would require an intermediate "merge on read" layer. Out of scope
for v1.2.

### Finding 2 — Symlink duplication

**Symptom.** `/home/$USER/Documents/lixun-ocr-smoke/ocr-smoke-42.pdf`
and `/home/$USER/Nextcloud/Documents/lixun-ocr-smoke/ocr-smoke-42.pdf`
produced two separate `fs:<raw-path>` doc_ids, two separate
`ocr_queue` rows, and two tesseract invocations for the same physical
file. `~/Documents` is a symlink to `~/Nextcloud/Documents`.

**Evidence (from `/tmp/lixund-v1.1-smoke.log`):**

```
ocr ok: /home/cryptobroiler/Nextcloud/Documents/lixun-ocr-smoke/ocr-smoke-42.pdf (74 chars)
ocr ok: /home/cryptobroiler/Documents/lixun-ocr-smoke/ocr-smoke-42.pdf         (74 chars)
```

**Root cause.** The fs source constructs `doc_id = fs:<path>` from
the raw path as discovered by directory traversal. A symlinked
ancestor directory produces a second path that resolves to the same
inode, but the doc_id derivation never canonicalises.

**Fix direction (v1.2).** Canonicalise via `std::fs::canonicalize`
before constructing the doc_id, inside `FsSource`. The fs watcher
must also translate symlinked event paths to their canonical form
before enqueue. A one-time migration sweep deduplicates existing
`fs:` doc_ids that share an inode. Estimated ~100–150 LOC + tests.

The canonicalisation must stay inside `lixun-sources` to preserve the
AGENTS.md modularity rule — no plugin-specific canonicalisation logic
leaks into the host or the trait crate.

### Finding 3 — No dimension/size pre-filter at enqueue

**Symptom.** A full-home reindex produced a queue that grew past
68 000 rows. The queue contained thousands of 32×32 and 64×64
icon PNGs (`steam_icon_*.png`, `chrome-*.png`) and small
throwaway images (`.oh-my-zsh/plugins/z/img/demo.gif`). These files
fail the dimension check inside `ocr_image` and are marked
`last_error = "too small"`, but only after the worker has:

1. Popped the row (SQLite write under `with_busy_retry`).
2. Probed capabilities for the job.
3. Attempted a tesseract spawn (or the image decode that precedes it).
4. Recorded the failure (another SQLite write).

**Queue composition at peak (2026-04-25, `~/.local/state/lixun/ocr-queue.db`):**

| ext   | count   | avg size | notes                              |
|-------|---------|----------|------------------------------------|
| jpg   | 59 077  | ~960 KB  | photo collections + some thumbs    |
| png   |  6 505  | ~380 KB  | many 32×32 / 64×64 app icons       |
| pdf   |  1 541  | ~2.0 MB  | legitimate scan-only documents     |
| jpeg  |    826  | ~1.5 MB  | phone photos                       |
| gif   |    188  | small    | plugin demo gifs, animations       |
| bmp   |    184  | small    | mostly spurious                    |
| webp  |     78  | small    | icons and web captures             |
| tif   |      9  | large    | document scans                     |
| tiff  |      5  | large    | document scans                     |

Tiny-file indicator: 2 107 entries under 4 KB total; 2 480
`png/ico/gif/bmp/webp` under 10 KB. These are almost entirely
icons that will fail the dimension check.

**Impact.** Every tiny image costs three queue writes and one
subprocess attempt per retry cycle. With thousands of icons the
aggregate drain time is hours of wall-clock for work that cannot
produce any text.

**Fix direction (v1.2).** Move the dimension probe from the tesseract
worker to `FsSource::maybe_enqueue_ocr`. `image::ImageReader::
with_guessed_format()?.into_dimensions()` reads only the PNG/JPEG
header, so the probe is cheap. Files below `min_image_side_px` on
either axis are skipped at enqueue time and never touch the SQLite
queue.

A simpler filesize heuristic (skip `png/ico/gif/bmp/webp` under
10 KB) can ship first as a fast mitigation, with the dimension probe
layered on top in a second commit. Estimated ~50 LOC + tests.

### Finding 4 — Serial drain rate vs bulk discovery

At 1 job per ~10 s on the smoke machine, a 68 000-row backlog drains
in roughly 190 hours. The strict-serial policy (DB-12) is correct for
laptop-idle behaviour but leaves the user with a long tail after the
first full-home reindex on a photo-heavy home directory.

**Fix direction (v1.2, discussion-only).** Options:

- Expose `[ocr].max_concurrent_jobs` (default 1, cap at `num_cpus`).
- Adaptive: bump concurrency to 2–3 when the queue is deep AND PSI
  stays below threshold for N consecutive ticks.
- Rely on Finding 3 (pre-filter) to shrink the backlog by an order of
  magnitude, keep serial drain.

The third option is attractive because Finding 3 is a pure win
regardless of concurrency policy. Prefer it first, revisit
concurrency only if the residual queue is still impractical.

### Finding 5 — Sparse observability

Current worker logs are per-job INFO lines. There is no aggregate
progress line, no queue-depth field in `lixun status`, and no CLI
surface for failed rows.

**Fix direction (v1.2).**

- Add `queue_depth: usize`, `queue_failed: usize`,
  `last_drain_at: SystemTime` fields to the IPC `IndexStats` message.
- Emit `tracing::info!` every N ticks: `"OCR progress: 900/1106
  drained, 23 failed, avg 4.2 s/job"`.
- Add `lixun status --ocr` to print the summary plus recent failures.

All of the above stays inside `lixun-indexer` and `lixun-cli`; the
trait crates and plugins are unaffected.

## v1.2 priority order

1. **Body-preservation on empty cache HIT** (Finding 1) — unblocks
   repeated `lixun reindex` without wasted OCR.
2. **Symlink canonicalisation** (Finding 2) — cuts duplicate work for
   every user with Nextcloud / cloud-mount symlinks.
3. **Pre-filter at enqueue** (Finding 3) — largest queue-size reduction
   per LOC of any v1.2 item.
4. **Observability** (Finding 5) — prerequisite for reasoning about
   v1.2+ drain-rate tuning on real user systems.
5. **Drain rate / concurrency** (Finding 4) — revisit after 1–3 land
   and we have real post-fix queue sizes.

## Plugin boundary

OCR machinery lives in `lixun-extract` behind plain functions
(`ocr_image`, `ocr_pdf_pages`, `run_ocr_job`) and in `lixun-indexer`
behind the `ocr_tick` worker module. The fs source uses the neutral
`OcrEnqueue` / `HasBody` traits to talk to the queue and to the
index; no plugin-specific knowledge leaks into the host.

A future `OcrBackend` trait that would make `tesseract` one of
several backends (alongside hypothetical `surya` / `paddleocr`) is
intentionally deferred. The cost is ~500 LOC of trait wrapping with
no current user benefit. Revisit when a second backend lands.
