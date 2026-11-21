# Lixun 利寻

[![version](https://img.shields.io/github/v/release/dengkeping/lixun)](https://github.com/dengkeping/lixun/releases/latest)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-green)](#license)
![platform](https://img.shields.io/badge/platform-Linux-lightgrey)
[![rust](https://img.shields.io/badge/rust-2024-orange)](https://www.rust-lang.org/)
![status](https://img.shields.io/badge/status-active%20development-yellow)
[![SonarQube Quality Gate](https://sonarcloud.io/api/project_badges/measure?project=dengkeping_lixun&metric=alert_status)](https://sonarcloud.io/project/overview?id=dengkeping_lixun)

> **Status — active development.** Lixun is under heavy development. Expect
> rough edges, occasional regressions, and breaking changes between minor
> versions. Bug reports, feature requests, and patches are very welcome —
> open an issue on the project tracker, or send patches/proposals to
> Denis Kopin <deng@lixun.app>. Please include the output of
> `lixun-cli status` and the relevant section of `~/.config/lixun/config.toml`
> when reporting bugs.

Lixun is a local search launcher for Linux Wayland desktops. Press **Super+Space**
to find an application, file, folder or email. Search by filename or by words
inside a document. Press **Space** to preview a result and **Enter** to open it.
The optional semantic worker can also find documents by meaning and photos by
what they depict.

---

## Features

- **Preview pane** — inspect before you open. Hit Space on any result to see
  text, images, PDFs, code, email, office documents, or audio/video content in
  an overlay. Lixun keeps the preview process available between queries.
- **Semantic search** — combines full-text results with document and image
  embeddings. Photo queries can use English, Russian or Chinese descriptions.
  Install the optional worker and enable it in the configuration. See
  [Semantic search](#semantic-search) for setup and measured results.
- **Apps** — all `.desktop` entries, launched with `GDK_BACKEND` respected.
- **Files** — search names and contents under the configured roots. The default
  root is your home directory, subject to exclusions. Supported formats include
  plain text, PDF, DOCX, XLSX, PPTX, ODT, RTF and legacy Office files.
- **Email (Thunderbird)** — reads the running profile's
  `global-messages-db.sqlite` directly; subjects, bodies, senders,
  recipients all searchable. Optional attachment indexing from mbox files.
- **Email (Maildir)** — any number of maildir roots (mutt/neomutt/offlineimap
  /isync/fdm-style layouts).
- **Calculator** — type `= sqrt(16) + pi` and get `7.1415…` at the top.
  Also unit, base, and percent conversions: `5km in mi`, `72F in C`,
  `1 GiB in MB`, `0xff`, `255 in hex`, `15% of 80`.
- **Shell** — type `> ls -la` to spawn a terminal via `xdg-terminal-exec`.
- **Keyword web searches** — declare `[[search.keyword]]` shortcuts
  (`gh rust lifetimes` → GitHub search) and pick your `web_engine_url`
  for the zero-results fallback. A browser opens only when you
  explicitly activate the result.
- **Command sources** — extend the launcher without recompiling: a
  `[[command_source]]` entry maps a query prefix (e.g. `pass `) to a
  local script whose JSON stdout becomes result rows (the
  Ulauncher/rofi-script model). Argv is spawned directly — no shell
  interpolation — with a timeout and bounded output.
- **Ranking you can steer** — frecency and query-latch learning with
  per-result controls: "Hide from Results" and "Reset Ranking" in the
  row menu, `lixun-cli hidden list|unhide` to inspect and undo hides.
- **Automatic OCR** — index scanned PDFs and images via Tesseract.
- **Theming** — full GTK4 CSS customization via `~/.config/lixun/style.css`.
- **System impact preset** — four resource levels (unlimited/high/medium/low)
  controlling thread pools, heap sizes, and scheduling hints.

Indexing, queries and model inference run locally. The semantic worker downloads models on first use. Activating a web-search shortcut sends the query to the configured search engine.

Deliberately out of scope for now: **window switching** (listing and
focusing open windows, as KRunner does). It needs per-compositor
protocols — wlr-foreign-toplevel on wlroots, the KWin D-Bus interface
on Plasma — with no portable fallback; revisit when a common protocol
settles. Users coming from KRunner should keep their compositor's
native switcher bound.

---

## Install

### Release packages (x86_64)

Download the core package for your distribution from the
[latest release](https://github.com/dengkeping/lixun/releases/latest).
The release workflow builds and tests three packages per distribution.

| Package | Purpose |
| --- | --- |
| `lixun` | Daemon, launcher, CLI and preview |
| `lixun-semantic` | Optional worker for document and image search |
| `lixun-ocr` | Optional dependencies for recognizing text in scans and images |

From the download directory, run the command for your distribution. The globs
below select the core package. Add optional package filenames to the same
command if needed.

```sh
# Arch Linux or CachyOS
sudo pacman -U ./lixun-[0-9]*-x86_64.pkg.tar.zst

# Debian 13
sudo apt install ./lixun_*+deb13_amd64.deb

# Ubuntu 26.04
sudo apt install ./lixun_*+ubuntu26.04_amd64.deb
```

Start the daemon in your desktop session and check its status.

```sh
systemctl --user enable --now lixund.service
lixun-cli status
lixun-cli toggle
```

### From source

From a checkout with Rust and the native build dependencies installed, build
the four core binaries. The [release build script](packaging/release/build.sh)
lists the native packages used on each supported distribution.

```sh
cargo build --locked --release -p lixun-daemon -p lixun-cli -p lixun-gui -p lixun-preview-bin
# Optional semantic worker
cargo build --locked --release -p lixun-semantic-worker
```

To run this build directly in a Wayland session, put the binaries on the
daemon's search path and start it in the foreground. Use this method when
there is no other Lixun daemon running for your user.

```sh
export PATH="$PWD/target/release:$PATH"
lixund
```

**Runtime dependencies:** `gtk4`, `gtk4-layer-shell`, `poppler` (PDF),
`zstd`, `sqlite`.

**Text extraction:** DOCX, XLSX, PPTX and ODT are read directly. For PDF
text install `pdftotext` (Poppler). Legacy DOC, XLS and PPT extraction
uses `antiword`, `catdoc` and LibreOffice respectively.

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

**Optional (semantic search):** the worker uses ONNX Runtime and caches
its models locally. See [Semantic search](#semantic-search).

---

## Getting started

### Bind the global hotkey

On KDE Plasma the daemon uses KGlobalAccel when available. On other desktops
it tries the XDG GlobalShortcuts portal. Check the **Hotkey** line in
`lixun-cli status` to see the active backend. The launcher requires a Wayland
compositor with layer-shell support. GNOME/Mutter lacks this protocol.

The default toggle can be set in the user configuration.

```toml
# ~/.config/lixun/config.toml
[keybindings]
global_toggle = "Super+space"
```

If the portal rejects the binding, write it in spec form: `LOGO+space`.

On compositors without the GlobalShortcuts portal (sway, niri, older
wlroots builds), bind `lixun-cli toggle` manually in your compositor
config. sway example:

```
bindsym $mod+space exec lixun-cli toggle
```

### Command-line usage

```sh
lixun-cli toggle        # show/hide the GUI
lixun-cli search "meeting notes"    # search without GUI (prints to stdout)
lixun-cli reindex       # start a full reindex in the background
lixun-cli status        # daemon health, index stats, reindex progress
lixun-cli impact get    # show current system-impact level
lixun-cli impact set medium --persist  # change level (hot+cold knobs)
lixun-cli impact explain                # show the current resource profile
lixun-cli dashboard                     # inspect the daemon interactively
lixun-cli check-exclude ~/Documents     # explain the indexing policy for a path
```

### Working with search results

- When the filename contains the whole query, the row shows its parent folder, even if the same words appear in a spreadsheet formula or document body. A content match can show the surrounding passage.
- File type labels use readable names, including for Office files already in the index.
- Press **Space** on a matching PDF to open its preview on the first occurrence with the quote highlighted. Enter opens the usual file handler.
- Open the launcher menu and choose **Saved searches…** to name the current query and category. Choosing a saved name searches the current index again.
- Run `lixun-cli preview-root /path/to/folder` before adding an indexing root. The command checks installed extractors, size limits, OCR eligibility and exclusion rules. Excluded directories appear once with the matching rule. OCR candidates depend on successful recognition when indexed.
- When an ordinary query returns zero results, the launcher can offer the same keystrokes in the other Russian/English layout. Select the suggestion and press **Enter** to search again.

See [Preview pane](#preview-pane) for supported formats.

---

## Configuration

From a source checkout, copy the example to start a new configuration. Release
packages also install the example under `/usr/share/doc/lixun/`.

```sh
mkdir -p ~/.config/lixun
cp -n docs/config.example.toml ~/.config/lixun/config.toml
```

A short taste of the schema — indexer knobs live under `[core]`:

```toml
[core]
max_results = 30               # Result cap shared by GUI and CLI
max_file_size_mb = 50          # Larger files are indexed by name only
extractor_timeout_secs = 15    # Per-extractor timeout (pdftotext, etc.)
exclude = [".thunderbird"]     # Substring excludes (added to defaults)
exclude_regex = ['\.sqlite-wal$', '/target/(debug|release)/']

[keybindings]
global_toggle = "Super+space"

[semantic]
enabled = false                # Opt-in dense-vector sidecar
```

Every supported section — `[ranking]`, `[keybindings]`, `[preview]`,
`[gui]` (sizing, blur, theme, matugen), the source plugins
(`[[maildir]]`, `[thunderbird]`, `[calculator]`, `[shell]`,
`[semantic]`), `[extract]`, `[ocr]`, and `[impact]` — is documented
with defaults and commentary in
[`docs/config.example.toml`](docs/config.example.toml), the single
reference for the config schema.

---

## Preview pane

Press **Space** on a focused result to open the preview overlay. Closes on
**Escape**, **Space**, or focus-loss. The preview process (`lixun-preview`)
is spawned on-demand via Unix socket from the daemon; it selects a bundled preview plugin based on MIME type and file extension:

- **text** — plain text files with syntax-aware line numbers
- **image** — PNG, JPEG, WebP, GIF (static and animated), TIFF, BMP, SVG
- **pdf** — rendered via Poppler
- **code** — syntax-highlighted source files via syntect
- **email** — RFC 822 messages and Thunderbird mbox entries
- **office** — OOXML and legacy Office documents (DOCX, XLSX, PPTX, DOC, XLS, PPT)
- **av** — audio/video files via GStreamer (MP3, MP4, WebM, etc.)
- **archive** — archive contents
- **folder** — directory contents
- **info** — file metadata as a fallback

**GStreamer** is required for the av plugin and for animated image rendering.
Without it, those formats show a "cannot preview" placeholder; all other
plugins work normally.

---

## Semantic search

Install `lixun-semantic` and enable the worker in
`~/.config/lixun/config.toml`. These are the default models.

```toml
[semantic]
enabled = true
text_model = "bge-small-en-v1.5"
image_model = "clip-vit-b-32-multilingual"
```

Restart the daemon after enabling the worker. Check its status, then backfill
existing documents if needed.

```sh
systemctl --user restart lixund.service
lixun-cli status
lixun-cli semantic backfill
```

Hybrid search can show a lexical page immediately and refine it using
reciprocal rank fusion. Explicit photo queries give more weight to visual
matches. Quoted phrases and field constraints retain their lexical meaning.

The default document model produces 384-dimensional vectors. The multilingual
image-query encoder searches the same 512-dimensional image vectors as the
earlier English CLIP encoder. Switching between these two query encoders
preserves existing image vectors. For multilingual document retrieval,
`text_model = "bge-m3"` uses more memory and requires a separate backfill.

On the project's held-out photo benchmark, the default returned four relevant
photos in the first five results in each of English, Russian and Chinese.
The same benchmark records false positives and peak worker memory.
[Search quality report](docs/search-quality.md) · [Search architecture](docs/search-fusion.md)

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
lixun-cli impact explain                # Show the current resource profile
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

The launcher ships with Tokyo Night, including a light palette that follows
the desktop colour preference. It works on a fresh installation with no
theme files or configuration. Place CSS overrides in
`~/.config/lixun/style.css`, or select a named theme under `[gui]`.
The launcher reloads edits while it runs.

Start with [the example stylesheet](docs/style.example.css) and copy the overrides
you need from [the theming guide](docs/theming.md#custom-stylesheet-recipes).

Inspect the live widget tree with:

```sh
GTK_DEBUG=interactive lixun-gui
```

Every themeable widget carries both a stable CSS id (`#lixun-root`,
`#lixun-entry`, `#lixun-results`, …) and a class (`.lixun-hit`,
`.lixun-top-hit`, …) so you can target either the specific widget or
all widgets of a kind.

### Compositor blur (liquid-glass look)

The default panel is 70% opaque so that a
blurred backdrop can shine through. The launcher window asks the
compositor to blur whatever is behind it on supported compositors:

- **KDE Plasma 6 (KWin)** — the GUI auto-binds the
  `org_kde_kwin_blur` protocol on every `map`. No user configuration
  required; if the protocol is unavailable (non-KDE compositor, X11
  session) the call is a silent no-op.
- **Hyprland** — add to `~/.config/hypr/hyprland.conf`:

  ```
  layerrule = blur, lixun-gui
  layerrule = ignorealpha 0.5, lixun-gui
  ```

- **sway, niri** — no blur protocol available; the window
  uses a denser, 94% opaque tint when blur is unavailable.

Set `[gui] opacity` to adjust the tint while blur is active.

---

## Architecture

Lixun is a **workspace browser with plugin-owned rendering**. The daemon
(`lixund`) owns the index and serves IPC; the GUI is a thin GTK4 + layer-shell
window spawned on demand; results come from source plugins discovered through
`inventory::submit!`; previews come from format plugins spawned in a separate
warm process. Nothing in the host binary names any concrete plugin — the
domain logic for "what counts as a Thunderbird message", "how to render a PDF",
"how to launch a `mid:` URI" all lives inside the plugin that owns that domain.

```
┌─────────────────┐         ┌─────────────────────────────────┐
│   lixun-gui     │◄───────►│          lixund (daemon)        │
│  GTK4 +         │ IPC     │  • Tantivy writer               │
│  layer-shell    │         │  • tick scheduler               │
│  main window    │         │  • filesystem watcher (notify)  │
└─────────────────┘         │  • IPC server (unix socket)     │
                            └─────────────────────────────────┘
┌─────────────────┐                    │        │
│   lixun-cli     │◄───── IPC ─────────┘        │
│  thin client    │                             │
└─────────────────┘                             ▼
                                    ┌───────────────────────┐
                                    │    lixun-indexer      │
                                    │  writer task, ticks   │
                                    └───────────────────────┘
                                                │
                                                ▼
                                    ┌───────────────────────┐
                                    │     lixun-index       │
                                    │    (Tantivy store)    │
                                    └───────────────────────┘
                                                ▲
                                                │ inventory::submit!
                                                │
                        ┌───────────────────────┴──────────────┐
                        │  Source plugins (config-gated):      │
                        │  • lixun-source-thunderbird          │
                        │  • lixun-source-maildir              │
                        │  • lixun-source-calculator (= prefix)│
                        │  • lixun-source-shell      (> prefix)│
                        │  • lixun-source-semantic-stub        │
                        │  • … (add your own — see below)      │
                        └──────────────────────────────────────┘

Preview path (spawned on first Space, kept warm between queries):

    lixund ──unix socket──► lixun-preview  ──►  Format plugins
                            layer-shell          (inventory::submit!)
                            overlay
                                                 • text   • image
                                                 • pdf    • code
                                                 • email  • office
                                                 • av     • archive
                                                 • folder • info
```

- **Daemon (`lixund`)** — owns the Tantivy writer, runs tick scheduler,
  watches filesystem via `notify`, serves IPC.
- **GUI (`lixun-gui`)** — GTK4 window with `gtk4-layer-shell`, spawned on
  demand by the daemon. Fully stateless; re-spawnable.
- **CLI (`lixun-cli`)** — thin IPC client. Distributions may also install
  it as `lixun` for convenience.
- **Preview (`lixun-preview`)** — reusable companion process spawned
  by the daemon when the user presses Space on a focused result row.
  Renders the hit in a second layer-shell overlay using a format plugin.
  Closes on Escape, Space, or focus-loss; main window remains alive underneath.
- **Plugin registration** — `inventory::submit!` + anchor crate pattern
  used twice: once for source plugins (`lixun-plugin-bundle`) and once
  for preview format plugins (`lixun-preview-bundle`). Adding either:
  new crate → feature in the bundle → nothing else in the daemon.

### Plugin lifecycle

Source factories use their configuration and defaults to decide which instances to create:

- `[thunderbird]` → `lixun-source-thunderbird` registered
- `[[maildir]]` → `lixun-source-maildir` registered (one instance per block)
- Calculator and shell are enabled by default. Set `enabled = false` under
  `[calculator]` or `[shell]` to disable the corresponding source.
- `[semantic]` + reachable `lixun-semantic-worker` sidecar → `lixun-source-semantic-stub` registered
- `[[search.keyword]]` defines web shortcuts and `[[command_source]]` registers local commands.

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

**Core crates:**

| Crate | Role |
|---|---|
| `lixun-config` | Configuration types and defaults |
| `lixun-core` | Shared types (`Document`, `Query`, `Hit`, …) |
| `lixun-ipc` | Unix-socket codec + request/response schema |
| `lixun-index` | Tantivy schema + writer wrapper |
| `lixun-extract` | File-format → text extractors |
| `lixun-sources` | `PluginFactory` trait + built-in `fs`/`apps` sources |
| `lixun-indexer` | Writer task, tick scheduler, fs watcher glue |
| `lixun-daemon` | IPC server, hotkey, startup, systemd glue |
| `lixun-gui` | GTK4 main window |
| `lixun-cli` | `lixun-cli` command (may be installed as `lixun`) |
| `lixun-mutation` | Config mutation (TOML edit) for `impact set --persist` |
| `lixun-fusion` | Reciprocal Rank Fusion + semantic search integration |
| `lixun-plugin-bundle` | Linker anchor for source plugins |

**Source plugins:**

| Crate | Role |
|---|---|
| `lixun-source-maildir` | Maildir plugin |
| `lixun-source-command` | External command sources |
| `lixun-source-keyword` | Keyword web searches |
| `lixun-source-thunderbird` | Gloda + mbox attachments |
| `lixun-source-calculator` | Calculator plugin (`=` prefix) |
| `lixun-source-shell` | Shell-command plugin (`>` prefix) |
| `lixun-source-semantic-stub` | Semantic search IPC client (talks to `lixun-semantic-worker` sidecar) |

**Preview crates:**

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
| `lixun-preview-archive` | Archive listing |
| `lixun-preview-folder` | Directory listing |
| `lixun-preview-info` | File information |

The semantic subsystem also includes `lixun-semantic-worker`, `lixun-semantic-proto` and the shared `lixun-image-decode` crate.

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

- **Semantic worker memory:** on the mixed-corpus benchmark the default worker
  peaked at 1,301 MiB. With BGE-M3 it peaked at 2,623 MiB on the same corpus.
  See the [benchmark conditions](docs/search-quality.md#resources-and-model-choice).

- **System impact preset:** Reduces CPU threads, heap sizes, and applies
  nice/ionice/SCHED_IDLE scheduling hints. Best-effort only — does not cap
  RAM. For hard limits, use systemd slice settings (see `docs/system-impact.md`).

- **Background reindex:** `lixun-cli reindex` starts the job and returns. The daemon
  does the work in the background. Watch progress via `lixun-cli status`.

- **gloda batch size:** tune `[thunderbird].gloda_batch_size` to trade off
   catch-up latency vs peak memory. Default 2500 is balanced for a 255k-message
   gloda database.

---

## Roadmap

Honest view of what is **not** shipped yet. Order reflects rough priority,
not a schedule. Anything here can slip or be dropped based on what the
existing codebase teaches us.

### Preview parity (Phase 1)

The PDF plugin supports page navigation, zoom, text selection and copying, in-document search, and highlighted search passages. The image plugin supports panning, zoom and copying a selected region. A stable API for third-party preview plugins remains open.

Open architectural question: whether the preview window stays on GTK4 +
`wlr-layer-shell`, moves to GTK4 + `xdg-toplevel` (~1 week, keeps the
toolkit), or is rewritten in Qt6 / QML on `xdg-toplevel` (6–9 weeks,
higher ceiling for pan/zoom and selection). The main window stays on
GTK4 + `gtk4-layer-shell` regardless. Compositor matrix: KWin, Hyprland,
sway, Niri are Tier 1; Cosmic is Tier 2 (smoke-test only); GNOME / Mutter
does not support `wlr-layer-shell` and is out of scope until it does.

### Next source plugins (Phase 2)

Ranked roughly by how much each one stresses the indexing substrate and
by how often people actually ask for it:

1. **Files / folders** — extend previews for Markdown, Jupyter notebooks,
   SQLite databases and fonts. Folder and archive listings already ship.

2. **Clipboard history** — `wl-clipboard` watcher, ring buffer, opt-in
   capture, searchable.
3. **Browser history / bookmarks / tabs** — Firefox `places.sqlite`,
   Chromium `History` SQLite, MozLZ4 decompression for session tabs;
   live-tabs via native-messaging host.
4. **System control surfaces** — one-keystroke control over systemd units,
   NetworkManager connections, BlueZ devices, PipeWire sinks / sources,
   brightness, via D-Bus.
5. **Thunderbird deepening** — surface mail threads and attachments as
   first-class objects, not just single messages.
6. **Resume-workspace** — compound local workspace recall: "what was I
   working on yesterday afternoon" returns a coherent object joined from
   files + mail threads + browser pages + downloads + attachments + code
   folders, with live preview and one-step reopen. Deterministic signals
   only (temporal proximity, shared paths, attachment relationships,
   sender / domain repetition, project folder, browser host, repo). No ML.

Explicitly **not** planned for v1.x: SaaS integrations (GitHub, Jira,
Slack, Notion, Linear, Google Drive, cloud LLMs); password manager
integrations beyond `pass` titles-only; smart home and cloud admin; a
conversational model plugin. If a plugin's value disappears offline, it is
not v1.

### Plugin trust model (Phase 3)

Today every source and preview plugin is a trusted in-process Rust
crate registered through `inventory` — fine for plugins shipped with
lixun, not fine for third-party plugins. The plan is two explicit
classes:

- **Extensions** — trusted, in-process, heavy native deps welcome
  (`poppler`, `gstreamer`, `libreoffice` headless, `syntect`,
  `mail-parser`). Today's plugins stay here. Not marketed as sandboxed.
- **Plugins** — untrusted third-party, out-of-process, declarative UI
  tree (closed widget vocabulary modeled on Android RemoteViews — Text,
  Image, Row, Column, Scroll, Divider, Badge, KeyValue, MarkdownLite,
  ActionBar, ErrorView). Host materializes; plugins never touch GTK or
  Qt directly.

Sandbox layer is still open: WASM (Wasmtime, WASI Preview 2, fuel
metering, ~5 ms cold start, cross-language, but no heavy native deps)
versus subprocess + bwrap + seccomp + cgroups + capability drop
(battle-tested, heavy deps welcome, ~20–50 ms cold start). A hybrid
(WASM first-class, subprocess escape hatch for plugins that need native
libraries) is the current working assumption.

No existing Linux launcher ships a sandboxed third-party plugin tier.
This is the longest-tail bet and the one that decides whether lixun
stays a personal tool or grows an ecosystem.

---

## Upgrade notes

A change to `INDEX_VERSION` or the plugin schema fingerprint starts a replacement Tantivy generation under the index directory. The previous directory stays in place. If its field layout is readable by this build, it continues to answer lexical searches while the sources are indexed. An incompatible layout delays search startup until the replacement is ready.

After the source walk and writer commit, the daemon checks that every filesystem manifest entry is present. If an indexed volume is unavailable, reconnect it and restart the daemon to resume. The daemon then opens a reader, checks the segment checksums and atomically publishes the `CURRENT` pointer. A restart before publication can resume the matching `PENDING` generation. Legacy installations with no pointer use the original directory as their previous generation. Allow disk space for both generations. Old generations are retained.

The [search quality report](docs/search-quality.md) records the semantic model changes, measurements, and vector backfill requirements.

**v0.7.8** shows the parent folder for filename matches and readable file types
for Office documents and attachments. Existing vendor-type labels are resolved
when records are read. These display changes do not require a full reindex.
[Release packages](https://github.com/dengkeping/lixun/releases/tag/v0.7.8)

Earlier milestones:

- **v0.7.2** moves filesystem operations off the async runtime threads during
  daemon startup, preview and semantic worker socket cleanup, vector store
  recovery, and OCR result validation.

- **v0.7.1** fixes Unicode search and applies category filters before filling
  the results page. Apps get priority for name matches. The daemon also
  enforces indexing exclusions during updates and protects private query
  history.

- **v0.7.0** (UX hardening pass). Keyboard-flow fixes: the top-hit preview
  chord, safe Enter on empty results, and session text selection. Honest
  daemon/indexing states in the status surfaces, calculator and shell
  plugins enabled by default (no config section required), visible
  selection styling, and accessibility fixes. No INDEX_VERSION bump.

- **v0.6.0** (cross-modal image search + RRF fusion). Added CLIP-based
  text→image search: query "photos of dogs" returns photos ranked by visual
  content, not filename. Fusion refactored to 3-way parallel fan-out (BM25 +
  text ANN + image ANN) merged via Reciprocal Rank Fusion (k=60). Removed the
  experimental anchor-based query classifier (CLIP text space is asymmetric for
  short queries — fan-out-and-merge is more robust). **INDEX_VERSION bump:**
  Tantivy schema gained a `mime` field needed for routing image documents into
  the CLIP image embedder. Daemon detects the mismatch on startup and reindexes
  from source. See the generation compatibility notes above and
  `docs/search-fusion.md` for architecture details.

- **v0.5.1** (packaging hardening). Fixed Arch makepkg link failure (LTO
  was dropping `cargo:rustc-link-lib=static` metadata from
  libsqlite3-sys/tikv-jemalloc-sys/zstd-sys). PKGBUILDs now set
  `options=(!lto)` plus a `cargo clean` and pinned linker. Also fixed the
  semantic worker self-indexing storm (LanceDB transaction/manifest files
  under `~/.local/share/lixun/` were being indexed by the fs source,
  saturating the worker's Delete channel). No INDEX_VERSION bump.

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
