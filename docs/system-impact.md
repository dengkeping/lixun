# System Impact Preset

The system impact preset is a single dial that tunes Lixun's resource footprint across CPU, memory, and I/O. It exists so you can run Lixun aggressively on a workstation or gently on a laptop without editing a dozen individual knobs.

The preset is **soft and best-effort**. It adjusts thread pools, heap sizes, and scheduling hints that the daemon and its workers respect, but it does not enforce hard limits. For true resource caps, use systemd slice settings (see below).

---

## Levels

Four levels are available. Values below are for an 8-core host; thread counts scale with `num_cpus` where noted.

| Knob | Unlimited | High | Medium | Low |
|------|-----------|------|--------|-----|
| `tokio_worker_threads` | `num_cpus` | `num_cpus` | `max(num_cpus/2, 2)` | 2 |
| `onnx_intra_threads` | `num_cpus` | 4 | 2 | 1 |
| `onnx_inter_threads` | `num_cpus` | 2 | 1 | 1 |
| `rayon_threads` | `num_cpus` | `min(num_cpus, 4)` | 2 | 1 |
| `tantivy_heap_bytes` | 200 MB | 100 MB | 64 MB | 32 MB |
| `tantivy_num_threads` | `num_cpus` | 4 | 2 | 1 |
| `embed_batch_hint` | 64 | 32 | 16 | 8 |
| `embed_concurrency_hint` | None | None | Some(1) | Some(1) |
| `ocr_jobs_per_tick` | 200 | 100 | 20 | 5 |
| `ocr_adaptive_throttle` | false | false | true | true |
| `ocr_nice_level` | 0 | 5 | 15 | 19 |
| `ocr_io_class_idle` | false | false | true | true |
| `ocr_worker_interval` | 1s | 1s | 5s | 30s |
| `extract_cache_max_bytes` | 2000 MB | 500 MB | 200 MB | 100 MB |
| `max_file_size_bytes` | 500 MB | 50 MB | 20 MB | 5 MB |
| `gloda_batch_size` | 5000 | 2500 | 1000 | 200 |
| `daemon_nice` | 0 | 0 | 5 | 10 |
| `daemon_sched_idle` | false | false | false | true |

---

## CLI

Use `lixun-cli impact` to inspect or change the active profile. The binary ships as `lixun-cli` in the source tree; distributions may also install it as `lixun` for convenience.

```sh
# Show current level and resolved knob values
lixun-cli impact get

# Change level (applies hot knobs immediately, warns about cold knobs)
lixun-cli impact set medium

# Change level and persist to config (survives daemon restart)
lixun-cli impact set low --persist

# Show what would change without applying
lixun-cli impact explain
```

The `--persist` flag writes the level to `~/.config/lixun/config.toml` under `[impact]` so the daemon starts with it on the next launch.

---

## Hot vs Cold Reload

Some knobs apply immediately when you run `lixun-cli impact set`. Others require a daemon restart because they size thread pools or heaps at startup.

**Hot (applied live):**
- `daemon_nice`
- `ocr_jobs_per_tick`
- `ocr_adaptive_throttle`
- `ocr_nice_level`
- `ocr_io_class_idle`
- `ocr_worker_interval`

**Cold (require restart):**
- `tokio_worker_threads`
- `onnx_intra_threads`
- `onnx_inter_threads`
- `rayon_threads`
- `tantivy_heap_bytes`
- `tantivy_num_threads`
- `embed_batch_hint`
- `embed_concurrency_hint`
- `extract_cache_max_bytes`
- `max_file_size_bytes`
- `gloda_batch_size`
- `daemon_sched_idle`

When you change the level, the CLI reports which knobs were applied hot and which require a restart.

---

## Battery Follow

Enable `follow_battery` to automatically switch to the Low preset when on battery power and restore the previous level when AC is connected. This is useful for laptops where you want full performance at a desk but quiet operation on the go.

```toml
[impact]
level = "high"
follow_battery = true
```

The daemon checks power state periodically. When battery is detected, it switches to Low; when AC returns, it reverts to the configured level. If you manually set a level while follow_battery is enabled, the manual setting takes precedence until the next power state change.

---

## Hard Caps via systemd

The impact preset shrinks thread pools, heap sizes, and scheduling priority,
but it does not bound resident memory. The dominant memory consumers — the
ONNX embedding model (~400 MB on glibc heap, loaded once) and the Lance/Arrow
record-batch staging buffers used during semantic backfill (~1.5–2 GB of
anonymous mmap regions) — are sized by their own libraries, not by impact
knobs. To put a real ceiling on the daemon, pair the preset with cgroups v2
limits via systemd.

### What `MemoryMax=` actually does

`MemoryMax=` writes the cgroup's `memory.max` knob. When the cgroup hits it,
the kernel does **not** gracefully push pages out — it invokes the cgroup
OOM killer on a process inside the cgroup, which on a single-process unit
means `lixund` gets `SIGKILL`'d. With `Restart=on-failure` the daemon
restarts, the embedding model reloads from scratch, and you can end up in a
crash loop if the cap is too low.

### What about swap?

The kernel will prefer reclaiming page cache and pushing anonymous pages to
swap **before** OOM-killing, but only if swap is available and `MemorySwapMax=`
allows it. Two pitfalls:

- **No swap or zram-only systems** (common on modern Linux installs) skip
  straight from "over budget" to OOM-kill with no graceful degradation.
- **The ONNX model is hot, anonymous, and touched on every embedding call.**
  If the kernel evicts it to swap, every query triggers thousands of page
  faults reading from disk; embedding latency goes from tens of milliseconds
  to seconds per document. The daemon technically lives but search is
  unusable. The Lance/Arrow staging buffers are colder (touched only during
  backfill flushes) and tolerate swap better — they get one latency hit per
  flush rather than per query.

### Recommended recipe: `MemoryHigh` + `MemoryMax` + `MemorySwapMax`

`MemoryHigh=` is the **soft** cgroup knob: above it the kernel throttles
the cgroup's allocation rate and aggressively reclaims, but does not kill.
This gives graceful slowdown under pressure, with `MemoryMax=` as a backstop
only for runaway behaviour.

```sh
mkdir -p ~/.config/systemd/user/lixund.service.d
```

`~/.config/systemd/user/lixund.service.d/override.conf`:

```ini
[Service]
# Soft pressure: above 1.5G, kernel reclaims aggressively. ONNX model
# weights resist eviction (touched every query); cold Lance buffers go
# to swap or zram first.
MemoryHigh=1500M

# Hard ceiling with headroom. OOM-kill only on genuine runaway. Sized
# above the typical RSS for a `low`-profile daemon (~3G observed when
# semantic backfill is active; settles lower once backfill completes).
MemoryMax=2200M

# Allow swap so reclaim has somewhere to go before the kill. Set to 0
# only if you have no swap and prefer fast crash to slow swapping.
MemorySwapMax=1G

# CPU cap (independent of impact preset's nice/SCHED_IDLE).
CPUQuota=50%
```

Reload and restart:

```sh
systemctl --user daemon-reload
systemctl --user restart lixund
```

### Choosing values

- **Server / always-on workstation, plenty of RAM**: skip `MemoryHigh`/`Max`
  entirely; rely on the soft preset alone. The kernel's global reclaim is
  enough.
- **Laptop on battery, want politeness**: pair `[impact] level = "low"` with
  `MemoryHigh=1G`, `MemoryMax=1500M`, `MemorySwapMax=512M`. Expect search
  to keep working; backfill becomes molasses (intentional).
- **Containers / strict multi-tenant hosts**: set `MemoryMax=` only, accept
  that the daemon may OOM-restart under sustained pressure, and let the
  init system bring it back. Add `RestartSec=30` to avoid hot-loop restarts.

### What this does *not* fix

`setrlimit(RLIMIT_AS)` set from inside the daemon would also cap virtual
memory but turns any allocation overshoot into instant `SIGKILL` with no
swap fallback — strictly worse than systemd cgroup limits. There is no
in-process knob that can both cap RAM *and* keep semantic search responsive;
the model has a fixed working set.

---

## Precedence

Configuration follows this order, later sources overriding earlier:

1. Built-in defaults (Unlimited level)
2. Profile seeds from the selected level (Unlimited, High, Medium, Low)
3. Explicit per-knob keys in `[impact]` or elsewhere in config.toml

For example, if you select `level = "medium"` but also set `ocr_nice_level = 10`, the explicit 10 wins over the Medium preset's default of 15.

---

## Example Configuration

A complete `[impact]` section in `~/.config/lixun/config.toml`:

```toml
[impact]
# Select a preset level: unlimited, high, medium, low
level = "medium"

# Automatically drop to Low when on battery (optional)
follow_battery = true

# Override specific knobs from the preset (optional)
# ocr_nice_level = 10
# extract_cache_max_bytes = 104857600
```

The level seeds defaults for all 18 knobs. Explicit keys under `[impact]` or elsewhere in the config override the seeded values. Use `lixun-cli impact explain` to see the final resolved values.
