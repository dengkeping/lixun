# Spotlight-Parity QA Checklist

This document lists the Success Criteria from `.local/plans/spotlight-parity.md` and how to verify each. Automated criteria are covered by `cargo test --workspace` and `cargo test --test it`; manual criteria require a live compositor.

## Automated (cargo test)

| SC | Criterion | Covered by |
|----|-----------|------------|
| SC-2 | "résumé" matches "resume" | `tests/it: spotlight_diacritic_insensitive` |
| SC-3 | "chrm" matches "Chrome" (fuzzy) | `tests/it: spotlight_fuzzy_single_edit_typo` (firefox/firfox) + `lixun-index: test_search_fuzzy_typo` |
| SC-4 | "my report" AND-semantics | `tests/it: spotlight_and_semantics_default` + `lixun-index: test_search_and_default` |
| SC-5 | "-draft" excludes | `tests/it: spotlight_not_operator_excludes` + `lixun-index: test_search_not_operator` |
| SC-6 | "2+2" → "4" | `tests/it: calculator_detects_arithmetic` + 25 tests in `lixun-index::calculator` |
| SC-7 | "sqrt(16)+pi" | `lixun-index::calculator::tests::detect_evaluates_function_and_constant` |
| SC-20 | Typing feels instant | event-driven debounce (80 ms single cancelable timeout) replaces 40 ms polling |
| SC-21 | 110 + 25 new tests green | `cargo test --workspace` |
| SC-22 | clippy -D warnings clean | `cargo clippy --workspace -- -D warnings` |
| 23 | Prefix boost: "fire" ranks Firefox above unrelated | `lixun-index scoring::tests::prefix_and_unicode` |
| 24 | Acronym match: "vsc" ranks Visual Studio Code | `lixun-index scoring::tests::acronym_fixtures` |
| 25 | Recency: newer mtime wins for tied files | `lixun-index scoring::tests::recency_orders_ties` |
| 26 | Frecency replaces additive bonus (mult semantics) | `lixun-daemon frecency::tests::mult_semantics` |
| 27 | Query latching pins doc for its query | `lixun-daemon query_latch::tests::cap_and_ordering` |
| 28 | Top Hit present iff confidence + margin satisfied | `lixun-daemon top_hit::tests::prefix_match_sets_top_hit` + `ambiguous_returns_none` |
| 29 | Protocol v2 clients still work | `lixun-ipc test_codec_accepts_protocol_v2_frame` + `lixun-daemon top_hit::tests::v2_response_shape_preserved` |
| 30 | `[ranking]` config values apply (guards D6 fix) | `lixun-index test_ranking_config_category_multiplier` |
| 31 | Index version bump forces reindex | `lixun-index test_index_version_triggers_rebuild` |
| 32 | Acronym retrieval: "jp" retrieves JSONParser-like files | `lixun-index test_acronym_retrieval_jsonparser` |
| 33 | Short prefix retrieval: "fire" retrieves Firefox | `lixun-index test_prefix_retrieval_firefox` |
| 34 | Prefix ranks above body: title prefix outranks body-only match | `lixun-index test_prefix_ranks_above_body_match` |

## Manual (requires Wayland compositor)

Run:
```
cargo build --workspace --release
./target/release/lixund &
./target/release/lixun toggle
```

Walk through:

- [ ] **SC-1** (hotkey) — configure compositor to bind Super+Space → `lixun toggle`; press → window appears
- [ ] **SC-8** (drag-out) — select a File row, drag onto Desktop/Files; file copies
- [ ] **SC-9** (right-click menu) — right-click File row; popover shows Open/Reveal/Copy path/Quick Look/Get Info
- [ ] **SC-10** (Space Quick Look) — select File row, press Space; gnome-sushi or xdg-open launches
- [ ] **SC-11** (Ctrl+1..4) — press Ctrl+2 (Apps chip); list narrows to apps
- [ ] **SC-12** (Ctrl+↓) — in mixed list, Ctrl+↓ jumps to first row of next category
- [ ] **SC-13** (↑ history) — with empty entry, press ↑; last queries appear; Enter on one replaces entry text
- [ ] **SC-14** (active monitor) — with two monitors, move pointer to secondary; toggle → window on secondary
- [ ] **SC-15** (draggable) — drag top of window; moves; close + reopen → remembered position
- [ ] **SC-16** (visible selection + focus ring) — arrow up/down; row highlights with blue bar; Tab → focus ring
- [ ] **SC-17** (loading spinner) — type slowly; brief spinner in status bar during fetch
- [ ] **SC-18** (empty state) — type "zzzzzzzz"; status shows "No results for 'zzzzzzzz' — Search the web"
- [ ] **SC-GUI-HERO** (Top Hit renders as row 0, activates with Enter by default) —
      restart daemon fresh (`systemctl --user restart lixund`).
      1. Open launcher (Super+Space). Type `firefox`. Expected: row 0 shows Firefox with a
         visually-distinct hero style (larger icon, card frame, subtle outline);
         `GTK_DEBUG=interactive lixun-gui` confirms the row carries both `.lixun-hit` and
         `.lixun-top-hit-hero` CSS classes; the selection indicator is on row 0.
      2. Press Enter. Expected: Firefox launches.
      3. Close Firefox, re-open the launcher, type `firefox` again. Expected: identical behaviour
         to step 1 — hero styling on row 0, selection on row 0, Enter launches Firefox. This is
         the regression guard for the Wave A bug where the second invocation activated the wrong
         row.
      4. Type `firefox`, then press Down once. Expected: selection cursor moves to row 1;
         row 0 keeps `.lixun-top-hit-hero` styling (hero visual stays) while row 1 gains
         `.lixun-top-hit` (selection cursor).
      5. Edit query to `zzzzz` (no matches). Expected: results list empty, status bar shows
         "No results"; no row has hero styling.
      6. Edit query back to `firefox`. Expected: row 0 is Firefox with hero styling; selection
         snaps back to row 0 (override cleared by query-text-change).
      7. Type `fo`, select doc X, press Enter; re-open launcher and type `fo` again. Expected:
         doc X ranks higher than on the first search (latch learned via
         `Request::RecordQueryClick`). Repeat 3 times to saturate; doc X becomes the hero Top Hit
         for `fo`.

      Known follow-ups (not covered by this SC):
      - Top Hit not appearing for short-prefix queries like `firef` (Bug #2 from Wave A QA).

      Bug #2 is tracked for Wave A.1b Phase 3 — it requires retuning `top_hit_min_margin`
      against the Phase-2 retrieval distribution (live measurement) and is out of scope for
      this GUI-side SC. Bugs #3 and #4 are fixed at the retrieval layer in Wave A.1b Phase 2
      (see SC-32 and SC-33 above).
- [ ] **SC-19** (Top Hit) — type "firefox"; first row has larger (48px) icon and subtle border

## Backwards compatibility

- [ ] **Protocol v1 client** — old `lixun-cli` binary against new `lixund`: `Request::Search` → `Response::Hits`, not `HitsWithExtras` (daemon negotiates per-frame)
- [ ] **Index rebuild on version mismatch** — delete `~/.local/state/lixun/index/index_version.txt`, restart daemon; logs show "rebuilding index"

## Evidence collected on approval

- All automated tests green: `cargo test --workspace` ran at each wave commit.
- Commits on branch `feat/spotlight-parity`:
  - `f9c9ce0` plan
  - `9379a03` Wave 1.1 (icons + kinds + migration)
  - `81936b2` Wave 2.1 (tokenizer + QueryParser)
  - `87d7e04` Wave 2.2 (calculator + protocol v2)
  - `36dbe96` Wave 3.1 (module split)
  - `64ed380` Wave 3.2 (real icons + Top Hit + CSS + status bar)
  - `8483477` Wave 3.3 (event debounce + draggable + keymap)
  - `e074e05` Wave 4.1 (QueryLog + IPC)
  - `ab3ca93` Wave 4.3 (drag-out + popover + Get Info + quick_look helper)
  - `b8bddc1` Wave 4.2 (chips + Ctrl+1..4 + Space)
  - `006cdcd` Wave 4.4 (history UI wire)
