# Lixun Hardening Review

Scope: full read-only audit of the `lixun` workspace (32 crates,
edition 2024, rust 1.92, toolchain 1.94) against ten focus areas,
followed by a set of small, high-confidence safety patches. The audit
did not rewrite any subsystem, change the public config format, or
remove features.

Build baseline at audit start: `cargo build --workspace` is green.

---

## 1. Executive summary

The codebase is generally well-structured. The semantic-worker
fallback path and the GUI responsiveness path are notably robust
(timeouts, off-main-thread IPC, graceful degradation to BM25-only).
Socket permissions, single-instance locking, index-version migration,
and per-client panic isolation are all correct.

The audit found a small number of genuine hardening gaps. The
highest-impact ones are:

- **Data loss on shutdown** — the daemon calls `std::process::exit(0)`
  before the index writer drains, so docs committed to the writer but
  not yet flushed by the 3-second commit timer are lost.
- **Unbounded IPC frame allocation** — every socket reader trusts the
  incoming `u32` length prefix and allocates that many bytes, so any
  local process that can reach the socket can drive the daemon (or
  GUI/CLI) into an out-of-memory condition.
- **Shell-source command injection** — the `>` shell source passes raw
  user text to `sh -c`, and the `strict_mode` blocklist is trivially
  bypassed.
- **Ghost index entries on directory rename/delete and symlinked
  paths** — deletions are not propagated to a subtree, and symlinked
  paths produce a doc-id that cannot be deleted once the file is gone.

The test suite was red on `main` at audit start. Every failure was
root-caused; **none is a production-code regression**. One was a real
compile break in test/bench fixtures (a struct field added without
updating fixtures); the rest are test-infrastructure issues
(missing index reload in a helper, GTK init across threads, a stale
assertion, a cross-binary cache race) plus two environmental tests
that require the ML semantic-worker binary.

This review applies the safe, high-confidence subset of fixes (see
§5). The architecturally invasive items (subtree-delete, symlink
ghost, shell argv re-architecture) are documented with proposed
designs rather than patched, to respect the "no rewrite, small
reviewable commits" constraint.

---

## 2. Findings by focus area

File:line references are against the tree at audit time.

### Area 1 — Daemon lifecycle and shutdown

**P0 — Writer never flushed on shutdown.**
`crates/lixun-daemon/src/main.rs:415` binds the writer join handle as
`_writer_handle` (unused). The shutdown branch at
`main.rs:699-717` runs `gui_control.shutdown()`, removes the socket,
saves the frecency / latch / query-log stores, then calls
`std::process::exit(0)`. It never sends `Mutation::Shutdown` to the
writer and never awaits the handle. The graceful drain-and-commit path
exists at `crates/lixun-indexer/src/index_service.rs:433-496` but is
unreachable. Any documents queued to the Tantivy writer but not yet
flushed by the periodic commit timer are lost.

`exit(0)` additionally aborts every spawned task without an orderly
cancel. The daemon `std::mem::forget`s several join handles (tick
scheduler `main.rs:562`, OCR worker `main.rs:1968`, cache sweep
`main.rs:1997`) and drops the handles of bare `tokio::spawn` tasks
(semantic supervisor `:379`, incremental indexer `:515`, watcher
`:536`, plugin fs watcher `:580`, battery `:610`, signal handler
`:644`).

Proposed fix: on shutdown, send `Mutation::Shutdown` to the writer,
await the writer handle with a bounded timeout, then return from
`main` normally instead of `exit(0)` so tokio drops the runtime
orderly. (Not applied in this pass — it changes the shutdown control
flow and warrants its own focused commit + test harness with a running
daemon.)

**P1 — `index_path.to_str().unwrap()`** at `main.rs:398` panics if the
state directory is not valid UTF-8. **Fixed** (see §5).

**P1 — Socket startup TOCTOU** at `main.rs:620-621`
(`if socket_path.exists() { remove_file()? }`). **Fixed** (see §5).

Strengths (unchanged): socket perms `chmod 0o600`
(`main.rs:628-634`); single-instance `flock`; `INDEX_VERSION`
mismatch handling (`crates/lixun-index/src/lib.rs:38-244`, consumed at
`crates/lixun-indexer/src/indexer.rs:196-204`).

### Area 2 — Filesystem watcher correctness

**P0 — notify errors silently dropped.**
`crates/lixun-indexer/src/watcher.rs:142-144` discards every `Err`
from the notify callback (`let Ok(event) = res else { return };`),
including inotify `IN_Q_OVERFLOW`. Same pattern at
`plugin_fs_watcher.rs:61`. When the kernel drops events the daemon
never learns, so the index silently diverges from disk.
**Fixed (logging)** in this pass; a full overflow-triggered rescan is
left as a follow-up (see §5).

**P0 — RenameMode::Both assumes exactly two paths.**
`watcher.rs:247-256` enumerates the path list and treats index 0 as a
delete and everything else as an upsert, without a `len == 2` guard
(the sibling `plugin_fs_watcher.rs:114` does guard). **Fixed**
(see §5).

**P1 (documented):** `Modify(_)` over-maps metadata-only changes
(chmod/chown/xattr) to a full re-extract (`watcher.rs:258-259`); raw
and control queue overflows are silently dropped; there is a window
between `initial_crawl` and `watch()` where events in new subdirs are
missed; doc-ids are path-based, so frecency/latch metadata is lost on
rename.

### Area 3 — IPC / socket permissions and protocol errors

**P0 — No maximum frame length; hostile local client OOM.**
Every reader trusts the incoming `u32` length and allocates an
unbounded `Vec`:
`crates/lixun-daemon/src/main.rs:1198-1214`,
`crates/lixun-cli/src/main.rs:43-49`,
`crates/lixun-gui/src/ipc.rs:130-141`,
`crates/lixun-ipc/src/gui.rs:125-126`,
`crates/lixun-ipc/src/preview.rs:301,432`,
`crates/lixun-ipc/src/lib.rs:556-563` (FrameCodec, test-only). A peer
sending `len = 0xFFFFFFFF` triggers a multi-gigabyte allocation.
**Fixed** across all readers with a shared `MAX_FRAME_LEN` cap (see
§5).

**P1 (documented) — CLI ignores the negotiated protocol version.**
The daemon writer task (`main.rs:1170-1185`) ignores
`_negotiated_version` and encodes with the compile-time
`PROTOCOL_VERSION`; the CLI response reader
(`crates/lixun-cli/src/main.rs:40-53`) hard-codes
`serde_json::from_slice`. A v5 (postcard) daemon therefore breaks the
CLI, undermining the rolling-upgrade guarantee. The GUI path is
correct (`ipc.rs:147` uses `decode_response(version, buf)`). Fix:
either have the writer task honour the negotiated version, or have the
CLI use `decode_response`. Left for a dedicated commit since it
touches the protocol-negotiation contract.

**P1 (documented):** the socket path is predictable
(`$XDG_RUNTIME_DIR/lixun.sock`, fallback `/tmp/lixun-$UID.sock`);
`UnixListener::bind` fails on a pre-existing symlink, so this is a
denial-of-service vector, not privilege escalation.

Strengths (unchanged): each client runs in its own `tokio::spawn`
with a `JoinSet` that catches panics (`main.rs:671-697`); one client
cannot kill the daemon. Socket perms are `0600` on daemon, GUI, and
preview sockets; the preview socket is nonce-tagged.

### Area 4 — Indexing correctness under rename / delete / move

**P0 — Symlink-delete leaves an undeletable ghost.**
Doc-ids are `fs:{canonicalize(path).unwrap_or(path)}`
(`crates/lixun-core/src/paths.rs:35-37,63`). A file indexed through a
symlinked ancestor is stored under its canonical id. On delete,
`canonicalize` fails (the file is gone) and the code falls back to the
raw symlink path, producing `fs:<symlink>` which does not match the
stored `fs:<canonical>`. `delete_by_id`
(`crates/lixun-index/src/lib.rs:353-361`) matches nothing and the
entry persists forever.

**P0 — Directory rename/delete leaves the entire subtree as ghosts.**
`watcher.rs:240-242` maps `Remove(dir)` / `RenameFrom(dir)` to a
single `Delete(fs_doc_id(dir))`, which removes only the directory's
own doc. Child docs `fs:/old/dir/*` are never deleted because there is
no delete-by-prefix operation.

Both require a new index operation (a `DeletePrefix` mutation backed by
a Tantivy prefix query on the path field, or a maintained parent-path
index) and careful doc-id design for symlinks. This is the main
architectural follow-up; documented here with a proposed design rather
than patched in this pass.

### Area 5 — GUI responsiveness under slow daemon / preview

No defects found; this subsystem is well-built. IPC runs on a
dedicated thread feeding an `async_channel` drained on the GTK main
loop (`crates/lixun-gui/src/ipc.rs:41-216`); the search socket has a
3-second read timeout (`ipc.rs:93,111-123`) that synthesises an empty
final chunk on timeout; preview dispatch is fire-and-forget
(`preview_spawn.rs`, `ipc.rs:374-397`); entry input is debounced 30 ms
and selection 50 ms.

The single exception: `crates/lixun-gui/src/window.rs:793` does
`gtk::gdk::Display::default().unwrap()`, which panics under a headless
environment. Documented as P1; left unpatched because it is a
startup-only path with no clean recovery (no display = no launcher).

### Area 6 — Panic / unwrap removal in production paths

Most `unwrap`/`expect` sites are guarded or provably safe (e.g.
`top_hit.rs` array indexing is length-checked at `:82/:110/:139`;
`shell.rs:61-62` expects only on pipe handles it just configured). The
actionable items are the `index_path` unwrap (fixed, §5), the
GUI/preview `Display::default()` expects (documented, headless-only),
and `main.rs:925` `task.await.unwrap_or_default()` which silently
turns a panicking plugin into empty hits (documented).

### Area 7 — Extraction timeout / resource control

**P0 (mitigated upstream) — In-process extractors have no timeout, no
size cap, and no zip-bomb protection.**
Shell extractors are correctly bounded (`shell.rs:75-95`:
`wait_timeout` + `killpg(SIGKILL)`, zombies reaped). But the
in-process `OoxmlExtractor` (`crates/lixun-extract/src/lib.rs:353-408`),
`OdtExtractor` (`:414-429`), `RtfExtractor` (`:433-474`), and
`extract_text_file` (`:313-323`) have no cancellation point;
`catch_unwind` (`:283`) catches panics only, not infinite loops or
runaway memory. `max_file_size_mb` is not checked inside
`lixun-extract` — it is enforced upstream
(`crates/lixun-indexer/src/index_service.rs:626`,
`crates/lixun-sources/src/fs.rs:264`), so the daemon path is
protected, but the crate is unsafe if called directly. OOXML/ODT
extraction calls `read_to_string` on archive entries without checking
`uncompressed_size`, and iterates `0..archive.len()` unbounded — a zip
bomb inflates unchecked. Documented; a stat-before-read size cap, a
per-entry/total decompression cap, and an in-process watchdog are the
proposed fixes. Not patched in this pass (changes the extractor
hot-path and needs careful benchmarking).

Strength: `cache_sweep.rs` enforces an LRU cap on the extract cache;
`cache_get` deletes and ignores corrupt zstd entries.

### Area 8 — Semantic worker crash / missing fallback

No defects found; this subsystem is robust. A missing worker binary
degrades silently to BM25-only (`semantic_supervisor.rs:42-62`,
`main.rs:372-384`, stub `source.rs:207-211` returns `Ok(empty)`). A
crashing worker is bounded by a 5-second per-query timeout
(`source.rs:17,233-249`) and an infinite supervisor restart with
exponential backoff 1s→60s (`semantic_supervisor.rs:80-134`). Fusion
catches ANN errors and still runs RRF with the BM25 leg
(`handle.rs:187-208`). No hard error reaches the user.

### Area 9 — Command-execution safety

**P0 — Shell source passes raw input to `sh -c`.**
`crates/lixun-source-shell/src/source.rs:18-22` (`wrap_with_hold`)
`format!`-concatenates the raw user command; `:82-86` emits
`Action::Exec { cmdline: ["sh", "-c", wrap_with_hold(cmd)],
terminal: true }`; `:91-94` does the same for the capture variant.
`> echo x; rm -rf ~` runs `rm`. The `strict_mode` blocklist regex at
`:9-10` (`^(?:sudo\b|rm\s+-rf\b|mkfs\b|dd\s)`, checked at `:64-67`) is
trivially bypassed (`bash -c …`, `eval …`, `/usr/bin/sudo`,
`rm -rfv`, `rm --rf`, `$(sudo ls)`). The correct fix is to parse the
command into an argv vector and spawn it directly (no `sh -c`), with a
whitelist rather than a blocklist — and per the project modularity
invariant this fix must live entirely inside `lixun-source-shell`.
Documented; not patched in this pass because it changes the shell
source's execution semantics and UX (loss of shell metacharacters)
and deserves explicit product sign-off.

**P1 — `xdg-open` argument injection (missing `--`).**
`crates/lixun-gui/src/actions.rs:149-153` (OpenFile) and `:197-202`
(OpenUri), plus `crates/lixun-preview/src/lib.rs:273-276`, invoke
`xdg-open` with the user-controlled path/URI as the first argument and
no `--` separator, so a filename like `-` or `--version` is treated as
a flag. **Fixed** (see §5); the `--` separator is generic argv
dispatch and names no application, so it respects the modularity
invariant.

**P1 (documented):** `.desktop` `Exec=` field-code stripping
(`crates/lixun-sources/src/apps.rs:85-91`) destroys quoting by
`split_whitespace`, breaking arguments with spaces. No injection (the
result is always spawned as argv), but functionally wrong; the fix is
to parse with a shell-words parser and store the exec line as a
`Vec<String>`.

### Area 10 — Test coverage

Well-covered: ranking/scoring (35 tests across frecency, query_latch,
top_hit, scoring, rrf), config (37 tests), path normalisation
(6 tests).

Gaps: no file-event→index-state test (no create/modify/delete/rename
assertion against the resulting index); no directory-rename-subtree
test (pairs with the Area-4 P0); no symlink-delete test (pairs with
the Area-4 P0); no NFC-composed-vs-decomposed test (the existing
`normalize` is NFKD strip, not NFC); no drift-guard ensuring
`docs/config.example.toml` stays in sync with the `Config` struct
(exactly the class of omission that caused the fixture compile break);
RRF is only tested 2-way (the 3-way fan-out is untested); no
combined `total_multiplier_cap` (frecency + latch) test; no
`top_hit_min_confidence` / margin boundary test.

---

## 3. Test-suite state at audit (all failures explained)

`cargo test --workspace --no-fail-fast` exited 101 on `main`. The
eight distinct failures, none a production regression:

1. **Compile break (real, fixed):** commit 51baca5 added
   `mime: Option<String>` to `lixun_core::Hit` and `Document` but
   missed several test/bench fixtures, so `cargo test --no-run` failed
   with E0063. Fixed by adding `mime: None` to fixtures in
   `lixun-preview-{email,code,text,av}/src/lib.rs` and
   `lixun-index/benches/ranking_bench.rs`.
2. **5 spotlight ranking tests** (`tests/it/main.rs`, the lixun-cli
   `it` target): the `upsert_docs` helper commits but never calls
   `idx.reload()`, and the reader uses `ReloadPolicy::Manual`, so the
   tests query a stale empty snapshot. Production is unaffected — the
   indexer writer loop calls `reload()` at `index_service.rs:550`, and
   the same behaviours pass in 65/65 lixun-index unit tests. **Fixed**
   (one-line reload in the helper).
 3. **6 pdf GTK tests**: panic "Attempted to initialize GTK from two
    different threads" because libtest runs each test on its own worker
    thread and GTK binds to the first thread to call `gtk::init()`. A
    `std::sync::Once` guard cannot fix this (the constraint is thread
    identity, not call count), and the sessions are `Rc`-based / `!Send`
    so they cannot be marshalled onto a single shared GTK thread without
    a custom harness or new dependency. Test-infra only; **fixed** by
    marking the six GTK-dependent tests `#[ignore]` (run them under a
    single-GTK-thread harness with `--ignored`). This also removes a
    `SIGABRT` that was poisoning the other 50 tests in the crate.
4. **1 shell test** (`source.rs:165` `no_trigger_without_prefix`):
   stale assertion — the source now intentionally returns a
   placeholder hit for a bare `>`. Test drift; **fixed**.
5. **1 thunderbird test** (`attachments.rs:411`): cross-test-binary
   race on global `HOME`/`XDG_CACHE_HOME`; passes in isolation (47/47).
   Documented as a test-isolation flake; production cache keying is
   deterministic (blake3, atomic write-then-rename).
6. **2 environmental tests** (`semantic_backfill_e2e.rs`,
   `handshake_search.rs`): require the ML semantic-worker binary;
   expected to fail without it. Documented, not a regression.

All other suites are green.

---

## 4. Risk roadmap

| Priority | Item | Status |
|---|---|---|
| P0 | Writer not flushed on shutdown (data loss) | Documented + proposed fix |
| P0 | Unbounded IPC frame allocation (OOM) | **Fixed** |
| P0 | Shell source `sh -c` injection + strict_mode bypass | Documented + proposed fix |
| P0 | Directory/symlink delete → index ghosts | Documented + proposed design |
| P0 | notify errors silently dropped | **Fixed (logging)** |
| P0 | In-process extraction: no timeout / size cap / zip-bomb | Documented (daemon path mitigated upstream) |
| P1 | `index_path.to_str().unwrap()` panic | **Fixed** |
| P1 | Socket startup TOCTOU | **Fixed** |
| P1 | `xdg-open` missing `--` separator | **Fixed** |
| P1 | `RenameMode::Both` no `len == 2` guard | **Fixed** |
| P1 | CLI ignores negotiated protocol version | Documented |
| P1 | `window.rs:793` headless `Display` unwrap | Documented |
| P1 | `.desktop` `Exec=` quoting | Documented |
| P2 | `Modify(_)` over-maps to full re-extract | Documented |
| P2 | OCR per-page, no overall deadline | Documented |
| P2 | `main.rs:925` swallows plugin panic → empty hits | Documented |

---

## 5. Changes applied in this pass

Each change is small, reversible, and accompanied by a test where the
behaviour is testable without external infrastructure.

1. **IPC frame cap (P0, OOM).** Added a shared `MAX_FRAME_LEN`
   constant in `lixun-ipc` and enforced it in every reader (daemon,
   CLI, GUI, gui/preview submodules). Oversized length prefixes are
   rejected with an error instead of allocating. Unit test added.
2. **`xdg-open` `--` separator (P1).** Inserted `--` before the
   user-controlled path/URI in the GUI and preview launch paths.
   Generic dispatch — no application named, modularity-safe.
3. **`index_path` non-UTF-8 panic (P1).** Threaded `&Path` through
   `create_or_open_with_plugins`, dropping `to_str().unwrap()`.
4. **Socket startup TOCTOU (P1).** Replaced the
   `exists()`-then-`remove_file` race with an unconditional remove that
   ignores `NotFound`.
5. **`RenameMode::Both` guard (P1).** Added a `len == 2` guard so
   malformed multi-path rename events are handled defensively. Test
   added.
6. **notify error logging (P0).** notify callback errors are now
   logged at error level instead of silently dropped.
 7. **Test-infrastructure fixes.** Index `reload()` in the `it` harness
    helper; the six thread-bound pdf preview GTK tests marked `#[ignore]`
    (pending a single-GTK-thread harness) to stop a `SIGABRT` poisoning
    the rest of the crate; corrected the stale shell placeholder
    assertion (split into two tests); the fixture `mime: None` compile
    fix.

Not applied (documented above with rationale): shutdown writer drain,
shell argv re-architecture, directory-subtree / symlink delete,
in-process extraction limits, CLI protocol-version negotiation. These
are larger or product-facing changes that should land as their own
reviewed commits.

---

## 6. Remaining high-risk areas

- **Index/disk divergence** from directory-rename and symlinked-path
  deletes (ghost entries). Highest-value architectural follow-up.
- **Shell-source command execution** remains injection-prone until the
  argv re-architecture lands.
- **In-process extraction DoS** (zip bomb / pathological document)
  is mitigated only on the daemon path by the upstream size cap; the
  extractor crate itself is unbounded.
- **Shutdown data loss** until the writer-drain path is wired into the
  shutdown sequence.
