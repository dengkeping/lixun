# Performance Budget — Keystroke → Render Latency

## Target Table

| Workload | Target P99 | Current Baseline (P99) | Unit |
|----------|-----------|------------------------|------|
| `cold-3char` | ≤ 35 ms | 14.36 ms | per search |
| `warm-3char` | ≤ 16 ms | 23.41 ms | per search |
| `warm-30char` | ≤ 22 ms | 15.56 ms | per search |
| `bursty-10keystrokes-200ms` | ≤ 500 ms total | 471.26 ms | total burst |
| `ipc-receive-10hits` (GUI) | ≤ 0.10 ms | 0.03 ms | per update |
| `ipc-receive-30hits` (GUI) | ≤ 0.20 ms | 0.09 ms | per update |

## Workload Definitions (verbatim from plan)

### `cold-3char`
Connect to a running daemon with a fresh Unix socket connection, send one `Request::Search { q: "rep", limit: 30, epoch: 1 }`, and measure end-to-end from request write to final response read.

### `warm-3char`
Reuse a single long-lived daemon connection across all iterations. Send 50 warmup requests before measurement begins. Query string is `"rep"` (3 characters, limit 30).

### `warm-30char`
Identical to `warm-3char` but with a 30-character query string (`"abcdefghijklmnopqrstuvwxyzabcd"`) that exercises the full token pipeline.

### `bursty-10keystrokes-200ms`
Simulate 10 keystrokes over 200 ms with growing query strings (`"a"`, `"aa"`, …, `"aaaaaaaaaa"`). Measure total end-to-end latency of the burst from first request write to last response read. Delay between keystrokes is 20 ms.

### `ipc-receive-10hits` (GUI bench)
Mock a daemon-side `Response::SearchChunk { phase: Final, hits: 10, … }`, deserialize it, run `compute_render_plan`, and update a `gtk::StringList` model. Measurement stops at model insertion.

### `ipc-receive-30hits` (GUI bench)
Same as `ipc-receive-10hits` but with 30 hits.

## Regression Policy

Any change that causes a ≥ 10% regression on any of the above workloads blocks merge. The baseline numbers are locked in `.workspace-local/evidence/baseline/keystroke_latency/` and in the `w0.1-baseline` criterion baseline.

## Baseline Capture Date

2026-05-21. Benchmarks run on the reference machine against the running `lixund` daemon.

## GUI Process RSS Invariant (W0.3)

> GUI process RSS may not drift more than ±5% from W0.3 baseline across A1–A6.

| State | Mean RSS | Unit |
|-------|----------|------|
| idle | 117,528 | kB |
| active | 117,528 | kB |
| teardown | 117,528 | kB |

Baseline captured by `scripts/gui-rss-baseline.sh` on 2026-05-21.
Evidence: `.workspace-local/evidence/baseline/gui_rss/20260521_120428.json`.
Session type: Wayland (wtype unavailable on this compositor; active-phase measurement falls back to idle-level RSS).

## Commands

```bash
# Daemon-side latency
cargo bench -p lixun-daemon --bench keystroke_latency -- --save-baseline w0.1-baseline

# GUI-side render path
cargo bench -p lixun-gui --bench render_path -- --save-baseline w0.1-baseline
```
