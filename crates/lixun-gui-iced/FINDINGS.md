# Iced + iced_layershell PoC — Research Findings

**Status**: ABANDONED (2026-05-02)  
**Verdict**: Oracle recommendation — stay on GTK4, archive this branch as research artifact.

## Executive Summary

Iced 0.14 + iced_layershell 0.18 port of lixun launcher was technically feasible but failed the primary test: **it does not feel better than GTK4**. After ~12-15 hours of work and 570 LOC, three fundamental UX bugs emerged in Phase 2 (icons + status bar + category chips):

1. **Enter key does not execute selected hit** (keyboard event routing broken)
2. **Laggy/jerky UI on typing** (icon resolution + handle recreation + widget tree rebuild per keystroke)
3. **Backspace hold does not clear entry** (auto-repeat broken, must tap per char)

For a Spotlight-style launcher, these are product-defining bugs, not edge polish. Remaining gap to GTK4 parity: ~3975 LOC (session persistence, gui_server Unix socket, reaper SIGCHLD, preview mode, history popup, Ctrl+1..4 categories, Cmd+C copy, animations, attachment chips).

**Recommendation**: Keep branch as benchmark artifact, document learnings, return to GTK4.

---

## What Worked

### Phase 1: Basic Launcher (b23-b24)

- **IPC via tokio + mpsc**: Async Unix socket client in `ipc.rs` (164 LOC), clean separation from GUI
- **Final-only batching**: `pending_hits` buffer, only `Phase::Final` commits via `mem::take` → single rebuild per query (vs GTK4's Initial+Final double rebuild)
- **Scrollable Id**: `scrollable::Id::new("results")` preserves scroll offset across rebuilds (GTK4 ListView does this automatically)
- **Selection state**: `selected_idx: Option<usize>` + ↑↓ keyboard nav + visual highlight via `top_hit_container_style`
- **Action execution**: Minimal `execute_action` dispatch (OpenUri via xdg-open, Exec via xterm -e, OpenFile via xdg-open)
- **wgpu backend switch**: After tiny-skia + transparent + shadow issues (#2712, #2917), wgpu solved flicker completely

User quote after Phase 1: **"работает все"** — selection, Enter, Esc all worked.

### Phase 2: Visual Parity Attempt (b26-b29)

- **Icons via freedesktop-icons 0.4**: `icons.rs` (30 LOC) with category fallback, svg/image widget rendering
- **Status bar**: "Searching…" / "No results for X" + web search button
- **Category chips**: All/Apps/Files/Mail/Attachments filter buttons with active state styling

Build succeeded, GUI launched, features rendered. **But performance regressed.**

---

## What Broke (Root Causes)

### Bug 1: Lag on Typing (Performance Regression)

**Symptom**: User quote: "все лагает" after Phase 2, despite wgpu + Final-only batching + scrollable Id.

**Root causes** (3 compounding issues):

#### A. Icon resolution on every `view()` call

```rust
// main.rs:364 — called for EVERY hit on EVERY view() rebuild
let icon_widget = match icons::resolve_icon(hit, icon_size) {
    Some(path) => svg(svg::Handle::from_path(path)).into(),
    // ...
}
```

`icons::resolve_icon` → `freedesktop_icons::lookup(name).with_size(s).with_cache().find()` walks filesystem icon theme hierarchy (even with internal cache, this is hash lookup + path validation). **30 hits × N view() rebuilds per keystroke = heavy I/O.**

**GTK4 does differently**: `gtk::IconTheme::for_display().lookup_icon()` returns `IconPaintable`, GTK caches Paintable inside widget tree, ListView factory reuses widgets via bind/unbind (not recreate from scratch).

#### B. `image::Handle::from_path()` created in `view()` every frame

```rust
// main.rs:371 — WRONG per Iced docs #3160
image(image::Handle::from_path(path))
    .width(Length::Fixed(icon_size as f32))
    .into()
```

Iced docs (#3160) explicitly state: **"image::Handles are meant to be stored in the state. Use a clone in the view fn to produce the image."** Creating new handle every view() → renderer treats it as new resource → texture reupload → flicker + lag.

**Fix**: Store `HashMap<DocId, image::Handle>` in `Launcher` state, resolve icons once on IPC Final commit, clone handles in view().

#### C. Full widget tree rebuild every `view()`, no `lazy`

Before Phase 2: 3 widgets/hit (text title + text subtitle + container).  
After Phase 2: 6+ widgets/hit (icon svg/image + row + column + text + text + container).

**Widget creation doubled.** 30 hits × 6 widgets = 180 widget allocations per view(). No virtualization (Iced has no ListView equivalent), no `iced::widget::lazy` caching.

GTK4 ListView: factory pattern, widgets reused via bind/unbind, only visible rows rendered.

---

### Bug 2: Enter Key Does Not Execute

**Symptom**: ↑↓ and Esc work, but Enter does nothing. User must click or use external keybind.

**Root cause hypothesis**: Global keyboard listener (`iced::event::listen_with`) receives events **after** widget propagation. In Phase 2, added `button` widget (Web Search button with `on_press`) → button may capture Enter if in focus chain.

```rust
// main.rs:179-184 — global listener
iced::event::listen_with(|event, _status, _id| match event {
    iced::Event::Keyboard(iced::keyboard::Event::KeyPressed { key, modifiers, .. }) => 
        Some(Message::KeyPressed(key, modifiers)),
    _ => None,
})
```

Iced 0.14 + iced_layershell `KeyboardInteractivity::Exclusive` may route keys differently than expected. text_input doesn't consume Enter (no `on_submit` wired), but layer-shell focus semantics unclear.

**GTK4 does differently**: `keymap.rs` uses **Capture phase** EventControllerKey, explicitly returns `Proceed`/`Stop` to control propagation. Iced has no capture phase equivalent.

---

### Bug 3: Backspace Hold Does Not Clear Entry

**Symptom**: Auto-repeat broken. User must tap Backspace once per char.

**Root cause**: Same as Bug 2. Global `listen_with` returns `Some(Message::KeyPressed(...))` for **every** key including Backspace → Iced may mark event as captured → text_input doesn't receive auto-repeat events.

GTK4 keymap.rs: Backspace warp pattern explicitly checks `entry_has_focus` and returns `Proceed` to let entry handle it. Iced listener is unconditional.

**Possible fix**: Filter `listen_with` to only return `Some(Message)` for navigation keys (ArrowUp/Down/Enter/Escape), return `None` for printable keys and Backspace to let text_input handle them.

---

## Performance Comparison: Iced vs GTK4

### Cold Start (from benchmarks in b13)

| Framework | Cold Start | Notes |
|-----------|-----------|-------|
| GTK4 | 81-122ms | C version, Rust bindings add overhead |
| Iced wgpu | 217-280ms | Vulkan instance creation |
| Iced glow | 110-229ms | OpenGL, comparable to GTK4 |
| Iced tiny-skia | ~200ms | Software renderer |

**Iced wgpu is 2-3x slower to cold start than GTK4.**

### Memory (idle, simple UI)

| Framework | Memory | Notes |
|-----------|--------|-------|
| GTK4 | 50-200 MB | Real-world with GL buffers (not 7.5MB benchmark) |
| Iced wgpu | 47-100 MB | GPU buffers + wgpu overhead |
| Iced glow | 27-76 MB | Lower than wgpu |
| Iced tiny-skia | 30-40 MB | Software renderer, lowest |

**Iced glow/tiny-skia use less memory than GTK4 with GL renderer.**

### Input Latency

| Framework | Latency | Notes |
|-----------|---------|-------|
| Iced | ~3 frames (48-50ms @ 60Hz) | Reactive rendering (0.14 default) |
| GTK4 | ~3 frames | Retained-mode, similar |

**Comparable input latency.**

### Real-World Feel (Subjective)

- **Before Phase 2** (b24): User confirmed "заебись сейчас все" — wgpu solved flicker, felt smooth
- **After Phase 2** (b29): User reported "все лагает" — icon resolution + handle recreation killed performance

**Verdict**: Iced CAN be fast (Phase 1 proved it), but easy to regress with naive view() patterns. GTK4 is more forgiving (automatic caching, widget reuse).

---

## Iced Ecosystem Risks

### Known Upstream Issues Hit

1. **#2712**: tiny-skia + transparent + shadow → full redraw needed (forced switch to wgpu)
2. **#2917**: softbuffer transparency bug (shows black instead of transparent)
3. **#3160**: image handle recreation flicker (we hit this in Phase 2)
4. **IME**: PARTIAL on fcitx5 (#3258 candidate window bug, fix in #3259), better on ibus

### API Stability

- Iced 1.0 not yet released (still 0.14 in May 2026)
- `#[to_layer_message]` macro is iced_layershell-specific (orphaned if upstream changes)
- iced_layershell 0.18 had BREAKING API change from 0.5 (Application trait → build-pattern)

### Compositor Compatibility

- `KeyboardInteractivity::OnDemand` focus is compositor-defined, not guaranteed
- Tested on niri (wlroots-based), but Hyprland/Sway/River have known quirks:
  - niri #3615: keyboard focus corruption on destroy with exclusive focus → 100ms delay workaround
  - Hyprland Exclusive bug: captures pointer globally → brief Exclusive then switch to OnDemand
  - exwlshelleventloop #121: IME doesn't work in popups on Sway/River
- GNOME incompatible (no wlr-layer-shell support)

---

## Transferable Learnings for GTK4

### 1. Final-Only Batching (High Value)

**Iced approach** (ipc.rs + main.rs):
- `pending_hits: Vec<Hit>` buffer in state
- IPC `Phase::Initial` → extend `pending_hits`, don't render
- IPC `Phase::Final` → `hits = mem::take(&mut pending_hits)`, single atomic commit

**GTK4 current** (window.rs response poller):
- `Phase::Initial` → `update_results_merge` (incremental append to model)
- `Phase::Final` → `update_results_merge` again
- **Result**: 2 full model updates per query

**Recommendation**: Port Final-only batching to GTK4. Change response poller (window.rs:1016-1038) to buffer Initial chunks in `Vec<Hit>`, only call `update_results` on Final. Expected win: 50% fewer ListView rebuilds.

### 2. Icon Handle Caching (High Value)

**Problem**: GTK4 `icons.rs:resolve_icon` called on every factory bind (lines in factory.rs). IconTheme lookup is cached by GTK internally, but still hash lookup overhead.

**Recommendation**: Add `HashMap<DocId, gtk::IconPaintable>` to window state, resolve icons once on IPC Final commit (when hits arrive), store paintables, reuse in factory bind. Expected win: eliminate icon theme lookups during scroll/selection.

### 3. Scrollable Id Pattern (Low Value for GTK4)

Iced needed explicit `scrollable::Id` to preserve scroll offset across widget tree rebuilds. GTK4 ListView automatically preserves scroll via `Adjustment` + model position tracking. **No action needed.**

### 4. Async IPC (Medium Value)

Iced used pure async tokio (no blocking thread). GTK4 uses blocking thread + mpsc (ipc.rs:57 `std::thread::spawn`). Async is cleaner but GTK4's approach works fine. **No action needed unless refactoring IPC.**

---

## Why Abandon?

### Oracle's Assessment (b29)

> "The Iced port has already failed the most important test: it does not currently feel better than GTK4. Your current bugs are not edge polish bugs; they are launcher-core UX bugs: Enter, key repeat, and typing responsiveness. For a Spotlight-style app, those are product-defining."

### Effort vs Reward

- **Invested**: ~12-15 hours, 570 LOC
- **Remaining to GTK4 parity**: ~3975 LOC (session persistence, gui_server, reaper, preview, history, Ctrl+1..4, Cmd+C, animations, attachments)
- **Bug rate**: 3 fundamental UX bugs after Phase 2
- **User frustration**: "ненавижу GTK" is not sufficient reason to rewrite if GTK4 already works

### Strategic Decision

GTK4 is:
- Battle-tested (4545 LOC, 12 modules, all bugs fixed)
- Fast enough (81-122ms cold start, ~3 frame input latency)
- Stable API (GTK 4.12, mature ecosystem)
- Forgiving (automatic caching, widget reuse, ListView virtualization)

Iced is:
- Technically impressive (pure Rust, wgpu, layer-shell)
- Fragile (easy to regress with naive view() patterns)
- Immature (0.14, evolving API, iced_layershell coupling)
- Requires expertise (must understand handle caching, lazy widgets, subscription ordering)

**Verdict**: Stay on GTK4. Attack specific pain points (startup time, portal timeouts) in existing app, not rewrite the stack.

---

## Clean Exit Strategy

1. **Keep branch** as `experimental/iced` (do NOT delete)
2. **Add this FINDINGS.md** to document learnings
3. **Commit current state** with message: "research: Iced + iced_layershell PoC (ABANDONED — see FINDINGS.md)"
4. **Tag** as `research/iced-poc-2026-05-02`
5. **Switch to main** and continue GTK4 work
6. **Port Final-only batching** to GTK4 (high-value learning)
7. **Port icon handle caching** to GTK4 (high-value learning)

---

## If Continuing (Not Recommended)

If ignoring Oracle and continuing Iced port, fix bugs in this order:

### Priority 1: Icon Handle Caching (Fixes Lag)

```rust
// In Launcher struct
icon_cache: HashMap<DocId, iced::widget::image::Handle>,

// In IpcEvent Phase::Final handler
for hit in &launcher.hits {
    if let Some(path) = icons::resolve_icon(hit, 28) {
        let handle = if path.extension() == Some("svg") {
            // Store svg handle
        } else {
            iced::widget::image::Handle::from_path(path)
        };
        launcher.icon_cache.insert(hit.id.clone(), handle);
    }
}

// In view()
let icon_widget = launcher.icon_cache.get(&hit.id)
    .map(|h| image(h.clone()).width(...).into())
    .unwrap_or_else(|| container(text("")).into());
```

### Priority 2: Filter Keyboard Listener (Fixes Enter + Backspace)

```rust
// In subscription keyboard listener
iced::event::listen_with(|event, _status, _id| match event {
    iced::Event::Keyboard(iced::keyboard::Event::KeyPressed { key, modifiers, .. }) => {
        use iced::keyboard::key::Named;
        // ONLY capture navigation keys, let text_input handle the rest
        if modifiers.is_empty() {
            match key.as_ref() {
                iced::keyboard::Key::Named(Named::ArrowDown | Named::ArrowUp | Named::Enter | Named::Escape) => 
                    Some(Message::KeyPressed(key, modifiers)),
                _ => None, // Let text_input handle printable keys + Backspace
            }
        } else {
            None
        }
    }
    _ => None,
})
```

### Priority 3: Lazy Widget for Results (Reduces Rebuilds)

```rust
use iced::widget::lazy;

// In view(), wrap results_col in lazy
let results_widget = lazy(
    (launcher.hits.len(), launcher.selected_idx, launcher.category_filter),
    move |_| build_results_column(launcher).into()
);
```

**Estimated effort**: 5-10 hours without guarantees. Oracle recommends against.

---

## Conclusion

Iced + iced_layershell is a **viable but immature** toolkit for Wayland launchers. It CAN be fast (Phase 1 proved it), but requires deep understanding of Iced's reactive rendering model, handle caching, and subscription ordering. Easy to regress with naive patterns.

For lixun, **GTK4 is the pragmatic choice**. It's battle-tested, forgiving, and already works. Port the two high-value learnings (Final-only batching, icon handle caching) to GTK4 and move on.

**Branch status**: Archived as research artifact. Tag: `research/iced-poc-2026-05-02`.

---

**Author**: Sisyphus (autonomous execution, user asleep)  
**Date**: 2026-05-02  
**Oracle session**: ses_2198f0591ffekvfWwvDF4gL7iK
