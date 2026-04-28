# Lixun 利寻

[![version](https://img.shields.io/badge/version-0.5.0-blue)](https://repo.dkp.hk/denis/lixun/releases)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-green)](#license)
[![platform](https://img.shields.io/badge/platform-Linux-lightgrey)]()
[![rust](https://img.shields.io/badge/rust-2024-orange)](https://www.rust-lang.org/)
[![status](https://img.shields.io/badge/status-active%20development-yellow)]()

> **Status — active development.** Lixun is under heavy development. Expect
> rough edges, occasional regressions, and breaking changes between minor
> versions. Bug reports, feature requests, and patches are very welcome —
> open an issue on the project tracker, or send patches/proposals to
> Denis Kopin <deng@lixun.app>. Please include the output of
> `lixun-cli status` and the relevant section of `~/.config/lixun/config.toml`
> when reporting bugs.

A **Spotlight-style launcher for Linux**. Press `Super + Space`, start typing,
get instant results across applications, files, and email. Backed by a
local [Tantivy](https://github.com/quickwit-oss/tantivy) full-text index
and a long-lived daemon that keeps everything fresh via filesystem watchers.
Now with semantic search (Wave D), preview pane, and system-impact presets
(Wave E).

---

## Features

- **Apps** — all `.desktop` entries, launched with `GDK_BACKEND` respected.
- **Files** — everything under your home (minus sensible excludes) with
  full-text body extraction from PDF/DOCX/XLSX/PPTX/RTF/DOC/XLS/PPT/plain text.
- **Email (Thunderbird)** — reads the running profile's
  `global-messages-db.sqlite` directly; subjects, bodies, senders,
  recipients all searchable. Optional attachment indexing from mbox files.
- **Email (Maildir)** — any number of maildir roots (mutt/neomutt/offlineimap
  /isync/fdm-style layouts).
- **Preview pane** — press Space on any result to open a Quick Look-style
  overlay showing text, images, PDFs, code, email, office documents, or
  audio/video content.
- **Semantic search** — hybrid lexical + dense-vector retrieval via
  Reciprocal Rank Fusion. Runs as a sidecar process: install the
  `lixun-semantic-worker` binary on `$PATH` (or set
  `LIXUN_SEMANTIC_WORKER` to its path) and add `[semantic] enabled = true`
  to the config to activate it at runtime (default is off).
- **Calculator** — type `= sqrt(16) + pi` and get `7.1415…` at the top.
- **Shell** — type `> ls -la` to spawn a terminal via `xdg-terminal-exec`.
- **Automatic OCR** — index scanned PDFs and images via Tesseract.
- **Theming** — full GTK4 CSS customization via `~/.config/lixun/style.css`.
- **System impact preset** — four resource levels (unlimited/high/medium/low)
  controlling thread pools, heap sizes, and scheduling hints.

Everything local. No telemetry, no cloud, no vendor.

---

## Install

### Arch Linux (binary package)

```sh
git clone https://repo.dkp.hk/denis/lixun.git
cd lixun
cargo build --workspace --release
cp target/release/{lixun-cli,lixund,lixun-gui,lixun-preview} /tmp/lixun-arch-tarball/
tar -C /tmp/lixun-arch-tarball -czf packaging/arch/lixun-0.5.0-x86_64.tar.gz .
cd packaging/arch && makepkg -f
sudo pacman -U lixun-bin-0.5.0-1-x86_64.pkg.tar.zst
systemctl --user enable --now lixund.service
```

### From source (any Linux)

```sh
# Daemon, GUI, CLI, preview (lexical search only)
cargo build --workspace --release

# Semantic search worker (separate sidecar binary; ONNX Runtime, ~400 MB model cache)
cargo build --release -p lixun-semantic-worker

install -Dm755 target/release/lixund        /usr/local/bin/lixund
install -Dm755 target/release/lixun-cli     /usr/local/bin/lixun-cli
install -Dm755 target/release/lixun-gui     /usr/local/bin/lixun-gui
install -Dm755 target/release/lixun-preview /usr/local/bin/lixun-preview
# Optional: install the semantic worker if you built it above
install -Dm755 target/release/lixun-semantic-worker /usr/local/bin/lixun-semantic-worker
install -Dm644 packaging/systemd/lixund.service \
  ~/.config/systemd/user/lixund.service
systemctl --user enable --now lixund.service
```

**Runtime dependencies:** `gtk4`, `gtk4-layer-shell`, `poppler` (PDF),
`zstd`, `sqlite`.

**Optional (extraction):** `libreoffice-fresh` (docx/xlsx/pptx),
`catdoc` or `antiword` (legacy .doc).

**Optional (OCR for images and scan PDFs):** `tesseract` plus the
language packs you need — `tesseract-data-eng`, `tesseract-data-rus`,
`tesseract-data-chi_sim` (Arch); `tesseract-ocr` + `tesseract-ocr-eng`
/ `tesseract-ocr-rus` / `tesseract-ocr-chi-sim` (Debian/Ubuntu);
`tesseract` + `tesseract-langpack-eng` / `-rus` / `-chi_sim` (Fedora).
Off by default; see [Automatic OCR](#automatic-ocr) below for
configuration and behaviour.

**Optional (preview):** `gst-plugins-base`, `gst-plugins-good`,
`gst-libav` — required for the preview pane's audio/video plugin
and for animated GIF/WebP rendering in the image plugin. On Arch
all three are in `extra/`. Without them, preview still works for
text/code/pdf/email/office/static-image content, but the av plugin
cannot decode MP3/MP4/etc. and animated images silently render as
blank paintables.

**Optional (shell plugin):** `xdg-terminal-exec` — required by the
`> cmd` shell trigger to spawn the user's terminal emulator. Reads
`~/.config/xdg-terminals.list` (and `$TERMINAL`) to pick the right
program. Without it, lixun falls back to `$TERMINAL` then `xterm`;
if none of those resolve, the shell hit's spawn silently fails.

**Optional (semantic search):** ONNX Runtime (pulled via `fastembed`),
~400 MB model cache (`bge-small-en-v1.5` default), ~1.5–1.7 GB
Lance/Arrow staging during backfill. See [Semantic search](#semantic-search).

---

## Quick start

### Bind the global hotkey

Lixun uses the **XDG GlobalShortcuts portal** (works on KDE Plasma 6+, GNOME,
Hyprland, etc). On first run it registers a request for `Super+space`;
accept it in the portal dialog, or configure your compositor directly:

```toml
# ~/.config/lixun/config.toml
[keybindings]
global_toggle = "Super+space"
```

If the portal rejects the binding, write it in spec form: `LOGO+space`.

### Command-line usage

```sh
lixun-cli toggle        # show/hide the GUI
lixun-cli search <q>    # search without GUI (prints to stdout)
lixun-cli reindex       # fire-and-forget full reindex (returns in ms)
lixun-cli status        # daemon health, index stats, reindex progress
lixun-cli impact get    # show current system-impact level
lixun-cli impact set medium [--persist]  # change level (hot+cold knobs)
lixun-cli impact explain                 # preview changes without applying
```

### Preview pane

Press **Space** on any focused result to open the preview overlay.
Press **Space** or **Escape** to close it. The preview renders in a
separate layer-shell window so the launcher stays visible underneath.

---

## Configuration

Copy the example config and edit:

```sh
mkdir -p ~/.config/lixun
cp docs/config.example.toml ~/.config/lixun/config.toml
```

Annotated config showing all supported sections:

```toml
# Top-level settings
max_file_size_mb = 50          # Files larger than this indexed by name only
extractor_timeout_secs = 15    # Per-extractor timeout (pdftotext, etc.)

exclude = [".thunderbird", "target", "node_modules"]   # substring excludes
exclude_regex = ['\.sqlite-wal$', '/target/(debug|release)/']

[ranking]
apps = 1.3                     # Category score multipliers
files = 1.2
mail = 1.0
attachments = 0.9
prefix_boost = 1.4             # Title prefix match boost
acronym_boost = 1.25           # D4 acronym/initials boost
recency_weight = 0.2           # Recency bonus weight
recency_tau_days = 30.0        # Recency decay horizon
frecency_alpha = 0.1           # Frecency multiplier weight
latch_weight = 0.5             # Query-latch weight
latch_cap = 3.0                # Latch multiplier cap
total_multiplier_cap = 6.0     # Stage-2 multiplier ceiling
top_hit_min_confidence = 0.6   # Hero row confidence threshold
top_hit_min_margin = 1.3       # Hero row margin threshold
strong_latch_threshold = 3     # "Strong" latch click count

[keybindings]
global_toggle = "Super+space"
close = "Escape"
primary_action = "Return"
secondary_action = "<Shift>Return"
copy = "<Ctrl>c"
quick_look = "space"

[gui]
width_percent = 40             # Launcher width (% of monitor)
height_percent = 60            # Launcher height (% of monitor)
max_width_px = 900             # Absolute pixel caps
max_height_px = 800
preview_width_percent = 80     # Preview pane dimensions
preview_height_percent = 80
preview_max_width_px = 2000
preview_max_height_px = 1400

[preview]
enabled = true
default_format = "auto"        # "auto" or force a plugin id
max_file_size_mb = 200         # Preview limit (separate from extraction)

# ─── Source plugins (presence of section = plugin loaded) ─────────────

[[maildir]]                    # One instance per [[maildir]] block
id = "personal"
paths = ["~/Mail/INBOX", "~/Mail/Archive"]
open_cmd = ["neomutt", "-f", "{folder}"]

[thunderbird]
enabled = true
gloda_batch_size = 2500        # Tick batch: smaller = lower memory, slower catch-up
attachments = true             # Index mbox attachments (reindex-on-demand only)
# profile = "/path/to/thunderbird/XXX.profile"   # override auto-detect

[calculator]                   # Empty section enables the plugin

[shell]
# working_dir = "~"            # Working directory for shell commands
# strict_mode = false          # Block risky commands (sudo, rm -rf, etc.)

[extract]
cache_max_mb = 500             # Extraction cache LRU cap
cache_sweep_interval_secs = 600

[ocr]
enabled = false
# languages = ["eng", "rus"]   # Auto-derived from $LANG when omitted
# max_pages_per_pdf = 20
# min_image_side_px = 200
# timeout_secs = 30
# worker_interval_secs = 60
# jobs_per_tick = 10
# adaptive_throttle = false
# max_cpu_pressure_avg10 = 10.0
# nice_level = 19
# io_class_idle = false

[semantic]
enabled = false                # Requires the lixun-semantic-worker sidecar binary
text_model = "bge-small-en-v1.5"
image_model = "clip-vit-b-32"
batch_size = 32
flush_ms = 2000
min_image_side_px = 300
backfill_on_start = false
rrf_k = 60.0
cache_subdir = "fastembed"

[impact]
level = "high"                 # unlimited | high | medium | low
follow_battery = false         # Auto-switch to low on battery
on_battery_level = "low"       # Level to use when on battery
```

See [`docs/config.example.toml`](docs/config.example.toml) for the full reference.

---

## Preview pane

Press **Space** on a focused result to open the preview overlay. Closes on
**Escape**, **Space**, or focus-loss. The preview process (`lixun-preview`)
is spawned on-demand via Unix socket from the daemon; it selects one of
seven format plugins based on MIME type and file extension:

- **text** — plain text files with syntax-aware line numbers
- **image** — PNG, JPEG, WebP, GIF (static and animated), TIFF, BMP, SVG
- **pdf** — rendered via Poppler
- **code** — syntax-highlighted source files via syntect
- **email** — RFC 822 messages and Thunderbird mbox entries
- **office** — OOXML and legacy Office documents (DOCX, XLSX, PPTX, DOC, XLS, PPT)
- **av** — audio/video files via GStreamer (MP3, MP4, WebM, etc.)

**GStreamer** is required for the av plugin and for animated image rendering.
Without it, those formats show a "cannot preview" placeholder; all other
plugins work normally.

---

## Semantic search

Semantic search adds dense-vector retrieval to the lexical BM25 index.
Results from both paths are fused with Reciprocal Rank Fusion (RRF).

**Requirements:**
- Install the `lixun-semantic-worker` sidecar binary (the daemon
  discovers it via `LIXUN_SEMANTIC_WORKER`, then `$PATH`, then
  `/usr/lib/lixun/lixun-semantic-worker`)
- Add `[semantic] enabled = true` to config
- GLIBC >= 2.27 (ONNX Runtime requirement)

**Configuration:**
- Embeddings produced by `fastembed` (default text model: `bge-small-en-v1.5`)
- Model cache: `~/.cache/lixun/fastembed/` (~100–500 MB depending on model)
- Vector store: `~/.local/share/lixun/vectors/` (LanceDB tables)

**Memory footprint:**
- ONNX heap: ~400 MB
- Lance/Arrow staging during backfill: ~1.5–1.7 GB
- Backfill command: `lixun-cli semantic backfill` (resume-safe)

See [`docs/wave-d-semantic.md`](docs/wave-d-semantic.md) for full details.

---

## System impact preset

The `[impact]` preset tunes resource usage across CPU, memory, and I/O
with a single dial. Four levels available: `unlimited | high | medium | low`.
Default is `high` (matches v0.4.0 behaviour).

**CLI control:**

```sh
lixun-cli impact get                    # Show current level
lixun-cli impact set medium             # Apply level (hot knobs immediate)
lixun-cli impact set low --persist      # Persist to config.toml
lixun-cli impact explain                # Preview changes
```

**Hot reload** (takes effect within 2 seconds):
- `daemon_nice`, `ocr_jobs_per_tick`, `ocr_adaptive_throttle`,
  `ocr_nice_level`, `ocr_io_class_idle`, `ocr_worker_interval`

**Cold reload** (requires daemon restart):
- Thread pools (tokio, ONNX, rayon, Tantivy), heap sizes,
  semantic batch/concurrency, `daemon_sched_idle`

**Battery follow:**

```toml
[impact]
level = "high"
follow_battery = true
on_battery_level = "low"
```

See [`docs/system-impact.md`](docs/system-impact.md) for the full knob table
and systemd hard-cap recipes.

---

## Automatic OCR

Scanned PDFs and bitmap images (`.png`, `.jpg`, `.tiff`, `.webp`, …)
can be indexed by their text content via Tesseract. Off by default;
enable under `[ocr]` in `~/.config/lixun/config.toml`:

```toml
[ocr]
enabled = true
# languages: auto-derived from $LANG/$LC_ALL when omitted; fallback ["eng"]
# languages = ["eng", "rus", "chi_sim"]
# max_pages_per_pdf = 20         # omit = unlimited (per-page timeout still applies)
# min_image_side_px = 200        # pre-filter: skip anything smaller
# timeout_secs = 30
# worker_interval_secs = 60
# jobs_per_tick = 10             # batch drain size per worker tick

# Adaptive CPU throttle (Linux PSI, off by default):
# adaptive_throttle    = true
# max_cpu_pressure_avg10 = 10.0  # skip tick when /proc/pressure/cpu some avg10 exceeds this
# nice_level           = 19
# io_class_idle        = true    # ionice IDLE class on tesseract children
```

How it behaves:

- **Deferred, queued.** Extraction of a file returns immediately
  without text content; an OCR job is enqueued into a persistent
  SQLite queue at `~/.local/state/lixun/ocr-queue.db`. Queue survives
  daemon restarts, retries with capped attempts, and exposes progress
  via `lixun-cli status --ocr`.
- **Idle-gated.** The worker only runs while the main indexer is
  idle, draining up to `jobs_per_tick` jobs per wake-up. User-facing
  search never stalls behind OCR.
- **Small-image pre-filter.** Images below `min_image_side_px` are
  skipped before enqueue — typical icons and thumbnails don't
  consume queue slots.
- **Short-circuit on existing body.** If a document already has
  body text indexed from a prior extraction, re-indexing won't
  re-enqueue it; body is preserved across reindex.
- **Adaptive CPU throttle (Linux).** With `adaptive_throttle =
  true` the worker skips its tick while `/proc/pressure/cpu` shows
  sustained load, and spawns `tesseract` with `nice = nice_level`
  (plus `ionice --idle` if `io_class_idle = true`). Safe to leave
  running during full builds.
- **Shared extraction cache.** OCR results land in the same
  `~/.cache/lixun/extract/` store as pdftotext/OOXML output, keyed
  by `(path, mtime, size, engine version)`. Invalidates on file
  change; an LRU sweep caps total size (see `[extract]`).
- **Graceful degradation.** Without `tesseract` or language packs
  installed, the daemon auto-disables OCR with a single warning and
  keeps running.

Check progress at any time:

```sh
lixun-cli status --ocr
```

See `docs/config.example.toml` for the full `[ocr]` and `[extract]`
reference.

---

## Theming

Lixun is fully themable via GTK4 CSS. Drop a `style.css` at
`~/.config/lixun/style.css`; it loads at `APPLICATION+1` priority on top
of the built-in theme, so every declaration you write overrides the
default. Restart the daemon (or the GUI) to apply:

```sh
systemctl --user restart lixund
```

See [`docs/style.example.css`](docs/style.example.css) for the full
selector reference (window, entry, result rows, status bar, …) plus
ready-made recipes for a light theme, an alternate accent colour,
bigger text, and denser rows. The file doubles as a copy of the
default stylesheet, so you can fork it and tweak.

Inspect the live widget tree with:

```sh
GTK_DEBUG=interactive lixun-gui
```

Every themeable widget carries both a stable CSS id (`#lixun-root`,
`#lixun-entry`, `#lixun-results`, …) and a class (`.lixun-hit`,
`.lixun-top-hit`, …) so you can target either the specific widget or
all widgets of a kind.

---

## Architecture

```
┌────────────────────────────────────────────────────────────────────┐
│  lixun-gui (GTK4 + layer-shell)   ◄──── unix socket ────►  lixund  │
│                                                                    │
│  lixun-cli                        ◄──── unix socket ────►          │
└────────────────────────────────────────────────────────────────────┘
                                                               │
                                                               ▼
    ┌─────────────────┐    ┌──────────────────┐    ┌──────────────────────┐
    │  lixun-sources  │───►│  lixun-indexer   │───►│   lixun-index         │
    │  (fs, apps,     │    │  (writer task,   │    │   (Tantivy wrapper)   │
    │   plugin trait) │    │   tick scheduler)│    │                       │
    └─────────────────┘    └──────────────────┘    └──────────────────────┘
            ▲
            │ inventory::submit!
            │
    ┌───────┴────────────────────────────┐
    │  lixun-source-thunderbird  (gloda + attachments)
    │  lixun-source-maildir
    │  lixun-source-calculator  (= prefix)
    │  lixun-source-shell        (> prefix)
    │  lixun-source-semantic-stub (IPC client to lixun-semantic-worker sidecar)
    │  … (add your own: see below)
    └────────────────────────────────────┘

    ┌─────────────────────────────────────────────────────────────────┐
    │  Preview pane (spawned on Space)                                 │
    │                                                                  │
    │  lixund ──unix socket──► lixun-preview (layer-shell overlay)    │
    │                               │                                  │
    │                               ▼                                  │
    │                    ┌─────────────────────┐                       │
    │                    │  Format plugins     │                       │
    │                    │  (text/image/pdf/   │                       │
    │                    │   code/email/office/│                       │
    │                    │   av)               │                       │
    │                    └─────────────────────┘                       │
    └─────────────────────────────────────────────────────────────────┘
```

- **Daemon (`lixund`)** — owns the Tantivy writer, runs tick scheduler,
  watches filesystem via `notify`, serves IPC.
- **GUI (`lixun-gui`)** — GTK4 window with `gtk4-layer-shell`, spawned on
  demand by the daemon. Fully stateless; re-spawnable.
- **CLI (`lixun-cli`)** — thin IPC client. Distributions may also install
  it as `lixun` for convenience.
- **Preview (`lixun-preview`)** — short-lived companion process spawned
  by the daemon when the user presses Space on a focused result row.
  Renders the hit in a second layer-shell overlay using a format plugin.
  Closes on Escape, Space, or focus-loss; launcher remains alive underneath.
- **Plugin registration** — `inventory::submit!` + anchor crate pattern
  used twice: once for source plugins (`lixun-plugin-bundle`) and once
  for preview format plugins (`lixun-preview-bundle`). Adding either:
  new crate → feature in the bundle → nothing else in the daemon.

### Plugin lifecycle

A source plugin is loaded **if and only if** its config section is present:

- `[thunderbird]` → `lixun-source-thunderbird` registered
- `[[maildir]]` → `lixun-source-maildir` registered (one instance per block)
- `[calculator]` → `lixun-source-calculator` registered (singleton)
- `[shell]` → `lixun-source-shell` registered (singleton)
- `[semantic]` + reachable `lixun-semantic-worker` sidecar → `lixun-source-semantic-stub` registered
- Nothing → plugin stays dormant, zero runtime cost

No code in `lixund` names any plugin. The daemon iterates
`inventory::iter::<PluginFactoryEntry>` and hands each factory its
matching TOML table.

The system-impact preset is plumbed plugin-agnostically as
`Arc<ImpactProfile>` in the `PluginBuildContext`. No host code knows
plugin identity; each plugin reads the profile and adjusts its own
behaviour (batch sizes, concurrency, throttling) accordingly.

---

## Development

```sh
cargo build  --workspace
cargo test   --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

### Layout

**Core (12 crates):**

| Crate | Role |
|---|---|
| `lixun-core` | Shared types (`Document`, `Query`, `Hit`, …) |
| `lixun-ipc` | Unix-socket codec + request/response schema |
| `lixun-index` | Tantivy schema + writer wrapper |
| `lixun-extract` | File-format → text extractors |
| `lixun-sources` | `PluginFactory` trait + built-in `fs`/`apps` sources |
| `lixun-indexer` | Writer task, tick scheduler, fs watcher glue |
| `lixun-daemon` | IPC server, hotkey, startup, systemd glue |
| `lixun-gui` | GTK4 launcher window |
| `lixun-cli` | `lixun-cli` command (may be installed as `lixun`) |
| `lixun-mutation` | Config mutation (TOML edit) for `impact set --persist` |
| `lixun-fusion` | Reciprocal Rank Fusion + semantic search integration |
| `lixun-plugin-bundle` | Linker anchor for source plugins |

**Source plugins (5 crates):**

| Crate | Role |
|---|---|
| `lixun-source-maildir` | Maildir plugin |
| `lixun-source-thunderbird` | Gloda + mbox attachments |
| `lixun-source-calculator` | Calculator plugin (`=` prefix) |
| `lixun-source-shell` | Shell-command plugin (`>` prefix) |
| `lixun-source-semantic-stub` | Semantic search IPC client (talks to `lixun-semantic-worker` sidecar) |

**Preview pane (10 crates):**

| Crate | Role |
|---|---|
| `lixun-preview` | `PreviewPlugin` trait + selector + shared CSS helper |
| `lixun-preview-bundle` | Linker anchor for preview format plugins |
| `lixun-preview-bin` | `lixun-preview` binary (layer-shell overlay + plugin host) |
| `lixun-preview-text` | Plain text preview |
| `lixun-preview-image` | Image preview (PNG, JPEG, WebP, GIF, TIFF, BMP, SVG) |
| `lixun-preview-pdf` | PDF preview via Poppler |
| `lixun-preview-code` | Syntax-highlighted code preview |
| `lixun-preview-email` | Email/mbox preview |
| `lixun-preview-office` | Office document preview (OOXML + legacy) |
| `lixun-preview-av` | Audio/video preview via GStreamer |

### Writing a new source plugin

1. New crate `lixun-source-xxx` depending on `lixun-core` + `lixun-sources`.
2. Implement `PluginFactory` (parses its TOML section → returns one or more
   `PluginInstance`s).
3. `inventory::submit!` at crate root with a `PluginFactoryEntry`.
4. Add it as an optional dep in `lixun-plugin-bundle/Cargo.toml` under a feature.

Zero changes to `lixund`. Config-driven, auto-registered.

---

## Performance notes

- **Lexical-only daemon RSS:** 200–400 MiB for typical home corpora
  (~500k documents). `systemd MemoryPeak` shows higher — that's page cache
  attributed to the cgroup, not daemon heap. Check `/proc/$(pgrep lixund)/status`
  for the real number.

- **With semantic search enabled:** Add ~400 MB ONNX heap + 1.5–1.7 GB
  Lance/Arrow staging during backfill. After backfill completes, staging
  drops; steady-state is roughly ONNX heap + vector index working set.

- **System impact preset:** Reduces CPU threads, heap sizes, and applies
  nice/ionice/SCHED_IDLE scheduling hints. Best-effort only — does not cap
  RAM. For hard limits, use systemd slice settings (see `docs/system-impact.md`).

- **Fire-and-forget reindex:** `lixun-cli reindex` returns in ~6 ms; daemon
  does the work in the background. Watch progress via `lixun-cli status`.

- **gloda batch size:** tune `[thunderbird].gloda_batch_size` to trade off
  catch-up latency vs peak memory. Default 2500 is balanced for a 255k-message
  gloda database.

---

## Upgrade notes

The on-disk search index is versioned by `INDEX_VERSION` in
`crates/lixun-index/src/lib.rs`. When this version bumps, the daemon
detects the mismatch on startup and re-indexes from scratch; the old
index stays queryable until the rebuild finishes, so there is no
user-visible search downtime. Expect transient CPU and I/O for the
duration of the reindex (minutes on typical home corpora).

Recent version bumps:

- **v0.4.0** (Wave D, semantic search). Added the semantic search
  feature. As of the sidecar refactor, semantic runs in a separate
  `lixun-semantic-worker` process discovered by the daemon at startup;
  the heavy ML stack (ONNX Runtime, LanceDB) no longer links into
  `lixund` itself. Requires explicit `[semantic]` config section to
  enable. No INDEX_VERSION bump (semantic is additive).

- **v0.5.0** (Wave E, system-impact preset). Added `[impact]` config block
  and `lixun-cli impact` commands. No INDEX_VERSION bump. New config knobs
  are hot-reloadable (OCR fields, daemon nice) or cold (thread pools, heaps).
  See `docs/system-impact.md` for the full knob table.

- **8 → 9** (Wave B, ranking overhaul). Tantivy upgraded to 0.26.1,
  the spotlight tokenizer gained a Porter English stemmer as its final
  stage (so e.g. `running` and `runs` both reach the index as `run`),
  and query-time proximity + coordination boosts were added. First
  daemon start after pulling this release will re-index once.

---

## Trademarks and attributions

Lixun (利寻) is the author's product name. This codebase references many
third-party products and trademarks; they are used for identification
purposes only.

**Operating systems and UI environments:**

- Linux is a registered trademark of Linus Torvalds.
- KDE Plasma is a trademark of KDE e.V.
- GNOME is a trademark of the GNOME Foundation.
- Hyprland is a trademark of its respective owners.
- GTK and GStreamer are trademarks of the GNOME Project / freedesktop.org.

**Third-party software referenced as runtime or optional dependencies:**

- Spotlight is a trademark of Apple Inc.; Lixun is not affiliated with,
  endorsed by, or sponsored by Apple.
- Thunderbird is a trademark of the Mozilla Foundation.
- Tantivy is a trademark of Quickwit, Inc.
- Tesseract OCR is Apache 2.0 licensed (originally developed by HP, now
  maintained by Google).
- Poppler is maintained by freedesktop.org.
- LibreOffice is a trademark of The Document Foundation.
- SQLite is in the public domain (by D. Richard Hipp).
- fastembed is by Qdrant (Apache 2.0).
- LanceDB is by Lance (Apache 2.0).
- ONNX Runtime is a project of the Linux Foundation.
- The `bge-small-en-v1.5` embedding model is by BAAI (MIT license).
- xdg-terminal-exec is maintained by freedesktop.org.
- catdoc and antiword are legacy document converters (GPL).

All product names, logos, and brands are property of their respective
owners. All company, product and service names used in this documentation
are for identification purposes only. Use of these names, logos, and brands
does not imply endorsement.

---

## License

Lixun is dual-licensed under either of:

- **MIT License** — see [`LICENSE-MIT`](LICENSE-MIT)
- **Apache License, Version 2.0** — see [`LICENSE-APACHE`](LICENSE-APACHE)

at your option. SPDX-License-Identifier: `MIT OR Apache-2.0`.

This is the standard dual-license used across the Rust ecosystem
(tokio, serde, ripgrep, fd, bat). Pick whichever fits your downstream
context. Apache-2.0 carries an explicit patent grant and contributor
patent-retaliation clause that bare MIT lacks; MIT is shorter and
matches what some legacy distros and corporate review pipelines expect.

Copyright (c) 2024-2026 Denis Kopin <deng@lixun.app>.

### Contributions

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in Lixun by you, as defined in the Apache-2.0
license, shall be dual-licensed as above, without any additional terms
or conditions.

### Trademark and bundled dependencies

The license grant above covers the Lixun source code only. The names
"Lixun" and "利寻" are not licensed under MIT or Apache-2.0; see the
**Trademarks and attributions** section above for the policy on the
project name and on third-party trademarks referenced in this README.

Bundled and linked third-party software keeps its own license terms;
downstream packagers are responsible for honouring the licenses of all
linked dependencies. Tools such as `cargo deny check licenses` or
`cargo about` are recommended for license compliance auditing.
