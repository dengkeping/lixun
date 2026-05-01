# lixun research findings — 2026-05

Companion to `.workspace-local/plans/lixun-roadmap-2026.md`. Captures the raw
Oracle / librarian / explore output from the 2026-05-01 research session,
so the plan stays short and actionable while no evidence is lost.

Sections map 1:1 to plan phases. Each finding cites the agent session
ID; resume any session via `task(session_id="...")` for follow-up.

## 1. PR1 retrospective — what landed and why

Commit `969dab2` on `main`, pushed 2026-05-01. Single commit by user
choice ("Оставить локально, не пушить" — later overridden with "пуш").

Files (13 + Cargo.lock):

- `crates/lixun-ipc/src/{lib.rs,preview.rs,Cargo.toml}`
- `crates/lixun-preview/src/lib.rs`
- `crates/lixun-preview-bin/{Cargo.toml,src/main.rs}`
- `crates/lixun-daemon/src/{preview_spawn.rs,main.rs}`
- `crates/lixun-gui/src/{window.rs,keymap.rs,ipc.rs,gui_server.rs}`

Stats: +2078 / −406. Branch `main`, 1 commit ahead of origin at the
time of compress (45678e4..969dab2 on push).

### 1.1 Architecture (Oracle ses_220b14482ffegxhPm25itDeLZE)

Original verdict NEEDS CHANGES → user accepted all 10 fixes:

1. **IPC isolation**: per-process socket
   `/run/user/{uid}/lixun-preview-{pid}.sock` (NOT well-known). Reuse
   the existing `FrameCodec` wire format (u32 BE len + u16 ver + JSON),
   not JSON-lines. Messages: `PreviewCommand::ShowOrUpdate{epoch,hit,
   monitor}`, `Close{epoch}`, `Ping`; `PreviewEvent::Ready`,
   `Closed{epoch}`, `Error{epoch,msg}`. Latest-wins backpressure, NOT
   FIFO. EOF on either side → other treats as gone.
2. **Cancel race**: BOTH launcher-side debounce (35–50 ms) AND preview-
   side epoch-drop. Every async load/render path captures current
   epoch and re-checks before mutating widget tree.
3. **Plugin migration**: default impl returns `bail!(UpdateUnsupported)`
   sentinel; host falls back to rebuild + `set_child` on the sentinel.
   No plugin edits required for PR1.
4. **Idle timer**: in preview-bin via `glib::timeout_add_local_once`
   (NOT tokio — GTK is single-threaded). Daemon does not run a second
   idle timer; trust preview to self-quit.
5. **Daemon state machine**: `enum PreviewLifecycle { Dead,
   Starting{pid,socket_path,latest_desired}, Ready{pid,socket_path,
   latest_desired,visible,last_used} }`. Latest-desired primary.
6. **Layer-shell focus**: REMOVE focus-loss-close from
   `install_close_controllers`. Gate launcher's `focus_ctrl.connect_
   leave` on `preview_mode_active`.
7. **Bisection**: daemon decides spawn-vs-reuse, launcher is source of
   truth for desired selection + monitor, preview-bin is source of
   truth for currently rendered widget.
8. **Migration path**: 5 PRs (PR1=IPC + lifecycle + rebuild fallback;
   PR2=GUI preview-mode + selection-driven updates; PR3=text/image/
   code real update; PR4=email/bundle/pdf; PR5=AV/office). User
   override: ship PR1 as ONE commit.
9. **AGENTS.md modularity**: idle timeout + KeyboardMode = HOST policy
   not trait/plugin policy. Host never branches on plugin id.
10. **Misc**: launcher needs `preview_mode_active` state; heavy plugins
    must never block GTK thread; socket 0600 + user-private runtime
    dir; cold-start readiness race — daemon buffers latest desired
    until Ready arrives; preview stays on launcher monitor.

Effort: Medium (1–2 d) for architecture + scaffolding. Top 3 risks:
stale rendering under rapid arrows, focus/compositor regressions, weak
daemon state machine. All mitigated.

### 1.2 Runtime bug rounds (7 total)

| # | Bug | Root cause | Fix |
|---|-----|------------|-----|
| 1 | preview window flickers and dies on every Space | GTK auto-quits when `connect_activate` returns with no window; warm process builds window lazily after first `ShowOrUpdate` arrives | `app.hold()` guard stored on `PreviewState` |
| 2 | preview steals keyboard focus from launcher under KWin | `KeyboardMode::OnDemand` is "may participate", not "never focus me"; KWin gives focus to newly mapped Layer::Overlay | `KeyboardMode::None` (Oracle ses_21db7e1cbffeVIaITAVDJXkOxU) |
| 3 | preview escape doesn't reset `preview_mode_active`; arrows respawn preview | `close_via_keyboard` did NOT send `PreviewEvent::Closed`; daemon never dispatched `ExitPreviewMode` | new `PreviewCommand::Hide` IPC; launcher-side two-stage Escape sends Hide first |
| 4 | Enter inside preview did nothing | `install_close_controllers` had no access to `outbound_tx`; Return/KP_Enter degenerated to `close_via_keyboard` | thread `outbound_tx` through `build_window_skeleton` |
| 5 | Meta+Space keybind died after rebuild | `xdg-desktop-portal-kde` flake on KDE Plasma 6: `GlobalShortcuts.CreateSession` failed at daemon startup | `systemctl --user restart xdg-desktop-portal && systemctl --user restart lixund` (NOT a regression) |
| 6a | preview window appeared **under** launcher | both surfaces on Layer::Overlay; same-layer ordering protocol-undefined | launcher promoted to Layer::Top (Oracle ses_21d1294f3ffem2O6bwqHDEfcsh) |
| 6b | typing in launcher with preview open caused chaos (wrong-side characters, accidental select-all, query cleared) | keymap.rs synthetic-printable-key fallback (lines 207–222) + synthetic-Backspace (244–253) bypassed GTK IM context; Up/Down arrow handlers (282/292/315/327) called `list_view.grab_focus()` moving focus out of entry; next keypress hit synthetic path → fcitx5 broken | typing-while-preview-open exits preview (Variant 4); synthetic-input gates wrapped in `!preview_mode_active`; `list_view.grab_focus()` gated on `!preview_mode_active` |
| 6c | fcitx5 candidate popup also appeared under launcher | same Layer::Overlay z-order issue as 6a | fixed by 6a |

User m0361 confirmed "победа" after round 7.

### 1.3 Coupling audit (explore ses_21c2405efffeerL47qMgFRaHdl)

133 direct GTK type references across 8 plugins. PreviewPlugin trait
has 3 GTK-typed methods: `build()->gtk::Widget` (line 104),
`update(_,&gtk::Widget)` (line 132), `install_user_css(&gtk::gdk::
Display)` (line 274).

Per-plugin GTK matches: text 8, image 16, pdf 19, code 7, email 40,
office 25, av 18, bundle registry-only.

Heavy native deps that survive any toolkit migration: poppler, syntect,
mail-parser, html2text, sha2, libc.

Migration casualties: all GTK widget code, Cairo direct draw (pdf),
MediaFile/Video (av).

Layer-shell-specific code in preview-bin/src/main.rs lines 536–556:
`init_layer_shell()`, `set_layer(Layer::Overlay)`, 4× `set_anchor(Edge
::*, false)`, `set_keyboard_mode(KeyboardMode::None)` — all
xdg-toplevel-incompatible.

Pre-existing tech debt: pdf has 24-line Cairo ABI workaround docstring
(poppler-rs 0.26 links cairo 0.22, gtk4-rs links cairo 0.20, types not
interchangeable, never let cairo cross crate boundary). av has BUG-2
workaround for `gtk::Video` aborting on audio-only files. av has
unsafe `vbox.set_data::<gtk::MediaFile>(...)` at line 135.

Plugin registration via `inventory` crate (`PreviewPluginEntry {
factory: fn() -> Box<dyn PreviewPlugin> }`). NO stable ABI — plain
Rust trait + dynamic dispatch, every gtk4-rs version bump = recompile.

`PreviewPluginCfg = {section: Option<&toml::Value>, max_file_size_mb:
u64}` — only code plugin reads `cfg.section` for theme.
`SizingPreference = {FitToContent, FixedCap}`.

No advanced gestures (no GestureZoom/GestureDrag/DrawingArea custom
snapshot/IM context/clipboard ops/drag-drop/popovers).

## 2. Round 1 toolkit verdict (Oracle ses_21c24cae9ffe7BVf9wwUn5czjE)

**Toolkit**: Qt6/QML for **preview only** (launcher stays GTK4 +
`gtk4-layer-shell`).

**Window kind**: preview as undecorated `xdg-toplevel`, NOT layer-shell
overlay.

**Plugin ABI**: out-of-process semantic preview protocol via IPC where
plugins return data payloads (Text / RichText / Image / Pdf / Media /
Bundle), NOT toolkit widgets.

**Migration cost**: 6–9 person-weeks production, 10–12 polished.

**Per-plugin migration table**:

| Plugin | Effort |
|--------|--------|
| text / image / code / bundle | LOW (2–4 days each) |
| email | MEDIUM (4–7 days) |
| pdf / office / av | HIGH (1–2.5 weeks each) |

**Top-3 QuickLook gaps** in current implementation:

- Unified interaction model across plugin types.
- Selection semantics across formats (text, image region, PDF text).
- Progressive instant preview pipeline (skeleton → low-res → full).

**Top-5 risks of staying GTK4 + layer-shell** for preview:

1. Selection / IME / clipboard fragility under Wayland.
2. Gesture routing minefield (GestureZoom, drag-region select).
3. Plugin ABI brittle for years (every gtk4-rs bump = recompile).
4. PDF / code interaction architecture suffers (no first-class text
   selection model in GTK plugin trait).
5. Every compositor edge case becomes your problem.

**Bottom line**: GTK4-layer-shell is the wrong role for a preview that
wants QuickLook UX.

## 3. Round 2 single-toolkit verdict (Oracle ses_21c1193f4ffevRGuzoBcAujH1z)

User asked: why not port the launcher to Qt6 too, and can we be
toolkit-neutral?

**Answer**: Keep launcher on GTK4 + `gtk4-layer-shell`, move ONLY
preview to Qt6/QML on `xdg-toplevel`.

Split is the best engineering trade. "Compositor-neutral" is mostly
NOT a GTK-vs-Qt question; it is a Wayland protocol coverage question,
and the protocol that makes the launcher special is the one GNOME
still does not support: `wlr-layer-shell`. NO toolkit choice changes
that.

**Action plan**:

1. Freeze launcher GTK4 + layer-shell.
2. Move preview to Qt6/QML + `xdg-toplevel` + `xdg-foreign-v2`
   transient parenting.
3. Do NOT port launcher to Qt6 unless GTK becomes painful.
4. Do NOT chase "toolkit-neutral" — Slint / iced / egui have no better
   Wayland protocol story.
5. Define protocol targets: base `xdg-shell`, launcher overlay
   `wlr-layer-shell`, preview parenting `xdg-foreign-v2`, IM /
   clipboard / DnD / a11y via toolkit.

**Effort**: Medium for preview alone, Large if also port launcher.

**Risks of all-Qt**:

- LayerShellQt is a KDE wrapper, not upstream Qt.
- Rust binding story thinner than GTK GObject Rust (`cxx-qt` + manual
  CXX bridge needed; zero OSS precedents).
- GNOME still no layer-shell so Qt port buys no compositor parity.

**Risks of toolkit-neutral**:

- Accessibility is the biggest trap.
- IME quality drops first.
- Plugin ecosystem weaker.

**Risks of two toolkits**:

- Packaging grows (~150–280 MB combined RSS).
- UI consistency drifts.
- Engineering specialization splits.

**Effort to port launcher**:

- GTK4 → Qt6/QML: 4–7 person-weeks.
- GTK4 → Slint / iced / egui: 6–10 person-weeks.
- Entire UI on `smithay-client-toolkit` DIY: 12–24+ person-weeks.

**Escalation triggers**: revisit all-Qt if launcher grows complex;
re-evaluate launcher model if GNOME parity becomes a hard requirement
(core issue is loss of `wlr-layer-shell`, not GTK).

## 4. Round 1 librarian — QuickLook precedents (ses_21c2454e4ffeQmPwe6wCIhxSEi)

**Production QuickLook precedents on Linux Wayland 2025–2026**:

| Project | Toolkit | Window |
|---------|---------|--------|
| GNOME Sushi | GTK4 + JS | `xdg-toplevel` via DBus — NOT layer-shell |
| Walker | GTK4 + Rust + `gtk4-layer-shell` | layer-shell, only Spotlight clone with live preview, has same z-order / fcitx5 bugs we hit (confirms our diagnosis is universal) |
| KDE Kiview / Dolphin Quick Look | Qt6 | `xdg-toplevel` |
| Zathura / Evince / Okular / Sioyek / Foliate / Papers | mixed | all `xdg-toplevel` |

**Spotlight clones** (Albert, Anyrun, Ulauncher, Rofi, Fuzzel) — NONE
have built-in preview except Walker.

**Toolkit Wayland maturity**:

- GTK4 + layer-shell: production-ready.
- Qt6: excellent; Plasma fractional-scale issues fixed in 6.7–6.8.2.
- Slint / iced / egui: no production layer-shell.

**Cross-process parenting**: `xdg-foreign-v2` supported on KWin, Mutter,
Hyprland (March 2026), sway, labwc, niri.

**PDF**: Poppler standard everywhere.

**OSS components catalog**:

| Need | Component | License |
|------|-----------|---------|
| PDF rendering | Poppler (`poppler-rs`, `poppler-qt6`) | GPL-2 |
| Syntax highlighting | `syntect` | MIT |
| Email parse | `mail-parser`, `html2text` | MIT |
| Image decode | `image-rs`, `resvg` | MIT/MPL |
| Video / audio | GStreamer | LGPL |
| Office docs | LibreOffice headless | MPL |
| HTML render | QtWebEngine | various |
| PDF viewer (QML) | QtPdf 6.5+ (`PdfMultiPageView`, `PdfScrollablePageView`, `PdfPageView`, `PdfSelection`, `PdfSearchModel`) | LGPL |

**Plugin-ABI prior art**:

- Walker Elephant — gRPC / Protobuf, language-agnostic.
- Anyrun — `abi_stable` Rust dylib.
- Albert — Qt + Python hybrid.
- Apple QuickLook Generator — sandboxed CFPlugIn bundles.

## 5. Round 2 librarian — Qt6 + toolkit-neutral (ses_21c11162affevlKVVVA1adEsI9)

**Qt6 + LayerShellQt** (KDE official) v6.6.80 / 6.6.3-2 production-
ready, full feature parity with `gtk4-layer-shell`.

Production users: KRunner Plasma 6.2+, Quickshell (2269 stars C++ /
QtQuick), Husky-Panel — all C++ / QML, ZERO production Rust apps using
QtQuick + LayerShellQt.

**Rust + Qt6**: `cxx-qt` by KDAB v0.8.1 (Feb 2026) production-ready
as Qt6 binding, but NO built-in LayerShellQt bindings — manual CXX
bridge required.

**Toolkit-neutral Rust 2026**:

| Toolkit | Layer-shell support | Notes |
|---------|---------------------|-------|
| Slint | ❌ | winit issue #2582 PR #4044 unmerged; `layer-shika` early dev workaround |
| iced | ✅ via `iced-layershell` v0.17.1 | Trebuchet, Icelauncher production launchers; uses smithay-client-toolkit not winit; fcitx5 quirks (issue #3258 patched #3259) |
| egui | ❌ | `sctk_egui` early WIP missing clipboard, fractional, IME, DnD |
| smithay-client-toolkit raw | ✅ | you become the toolkit author |
| xilem / floem / makepad / gpui / freya | ❌ | all winit-based, no layer-shell |

**Compositor matrix 2026**:

| Compositor | wlr-layer-shell-v1 v5 |
|------------|------------------------|
| KWin / Hyprland / Niri 25.11 / sway 1.12 / labwc 0.9.6 | ✅ ALL support |
| Mutter (GNOME 47/48/50) | ❌ does NOT support; GNOME mutter#973 open since 2019; GNOME 50 alpha + Mutter 50.1 added no layer-shell |

`xdg-foreign-v2` supported everywhere including Mutter. `text-input-v3`
everywhere.

**Memory**: GTK4 hello-world 50–200 MB private (~50% GPU/driver,
`GSK_RENDERER=cairo` halves), Qt6 40–80 MB, dual-runtime ~150–280 MB
combined.

NO major Linux app intentionally ships dual GTK + Qt as deliberate
architecture (only file-dialog adapters).

**Spotlight-clone toolkit survey 2026**:

| Project | Toolkit | Status |
|---------|---------|--------|
| Walker | GTK4 + Rust + `gtk4-layer-shell` | production v2.16.0 |
| Albert | Qt6 + C++ | native production v34.0.10 |
| Anyrun | GTK4 + Rust + `abi_stable` | production v25.12.0 |
| Ulauncher | GTK3 | dying |
| KRunner | Qt6 + QML + LayerShellQt | production |
| Onagre | iced | NO layer-shell, maintenance mode |
| Trebuchet | iced + `iced-layershell` | production (March 2026) |
| Icelauncher | iced + `iced-layershell` | early beta |
| Hamr | GTK4 + Rust + `gtk4-layer-shell` | production |
| Launchpad | GTK4 + relm4 + `gtk4-layer-shell` | production |

**Qt6 preview components**:

- QtPdf 6.5+ EXCELLENT.
- QtWebEngine production but heavy 300–600 MB Chromium 140 in Qt 6.11.
- `poppler-qt6` v26.0.90 stable, used by Okular, but NO Rust bindings.
- QSyntaxHighlighter built-in.
- KFileMetaData / KIO::PreviewJob KF6 C++ only, no Rust bindings.

## 6. Cosmic Desktop — Round 3 verdict

### 6.1 Oracle (ses_21c039d9effe0zFBaE1fqiiaO1)

COSMIC in 2026 does NOT change the architecture and is NOT a reason
to either drop GTK4 launcher or stall the Qt6/QML preview migration.

COSMIC is not a GNOME-style exception — it is another layer-shell-
capable Wayland compositor. `wlr-layer-shell` works; System76 use it
themselves; custom protocols (`cosmic-overlap-notify`) are optional
shell extensions.

Treating COSMIC as a "special favorite that justifies moving to iced"
is wrong — System76 use iced because they are building their own shell
and the libcosmic ecosystem.

**Verdict**: (b) actively smoke-test on COSMIC now, but (a) treat it
as architecturally non-special; (c) do NOT move to iced.

**Effort**: Short.

### 6.2 Librarian (ses_21c03d231ffe8fDC4l9y53KQfP)

- **Status**: STABLE — Epoch 1.0 released 2025-12-11, current 1.0.11
  (April 2026), default in Pop!_OS 24.04 LTS.
- **wlr-layer-shell-v1**: production-ready (cosmic-launcher uses it).
  Focus handling fixed in cosmic-comp#770. Strict initial configure
  ack required (cosmic-epoch#2913).
- **xdg-foreign-v2**: PARTIAL (panel applet bug cosmic-panel#250).
- **text-input-v3**: SUPPORTED, but Pop!_OS 24.04 ships fcitx5 5.1.7
  broken; needs ≥5.1.12 (cosmic-session#185).
- **wp-fractional-scale-v1**: ✅
- **wp-cursor-shape-v1**: ✅
- **GTK4 + gtk4-layer-shell on Cosmic**: strict layer-surface teardown
  ordering (gtk4-layer-shell#119 — null-buffer commit before destroy).
- **Qt6 + LayerShellQt on Cosmic**: intermittent platform plugin init
  bug cosmic-comp#659; XWayland sub-window ghost windows
  cosmic-comp#1683.
- **Cosmic-specific protocols** (OPTIONAL for basic, REQUIRED for
  native feel): `cosmic-overlap-notify-v1`, `cosmic-toplevel-info-v1`,
  `cosmic-toplevel-management-v1`, `cosmic-workspace-v1`.
- **Smithay relationship**: cosmic-comp uses Smithay (community lib);
  iced apps get NO first-class status, equal treatment.

**Practical consequence**: add Pop!_OS 24.04 + fcitx5 5.1.12+ to the
smoke-test matrix; nothing else changes.

## 7. Plugin sandboxing — research

### 7.1 Oracle architecture verdict (ses_21be92f16ffeJlQ1cy0J4WcnXT)

User originally framed: untrusted third-party plugins, Rust-only via
stable ABI, full FS / network / process isolation, declarative UI tree.

Oracle: **the chosen combination (untrusted .so + sandbox) is
internally contradictory**. A `.so` in the host process shares memory,
FDs, syscall capabilities with the host. Untrusted means subprocess —
no negotiation.

**Recommendations**:

1. Drop `abi_stable` + Rust-only as foundational choices — protocol
   becomes the compatibility contract.
2. Closed widget vocabulary in v1: `Text` / `Image` / `Row` / `Column`
   / `Scroll` / `Divider` / `Badge` / `KeyValue` / `MarkdownLite` /
   `ActionBar` / `ErrorView`. No plugin-defined widgets.
3. Sandbox MVP = `bubblewrap` + `seccomp` + `systemd-run` scope
   (cgroups) + capability drop. Landlock optional defense-in-depth.
4. IPC = JSON v1 over Unix socket / stdio. Methods: `initialize` /
   `search` / `preview` / `perform_action` / `shutdown`.
5. Resource limits = cgroups `memory.max` + `pids.max` + IPC
   deadlines (search 50–150 ms / preview 500 ms – 5 s).
6. Distribution = manual install + signed manifest, NOT registry
   initially.
7. Two explicit classes: **Extensions** (trusted in-process .so, NOT
   marketed as sandboxed) vs **Plugins** (untrusted subprocess).

**Effort**: 6–10 person-weeks minimal MVP, 10–16 polished with
signatures + permissions UX + crash handling + dev tooling.

### 7.2 Librarian Linux sandboxing 2026 (ses_21be9fd6fffeMrNAOUoc842RHH)

**Sandbox technology ranking** (by isolation strength):

| Rank | Tech | Verdict |
|------|------|---------|
| 1 | Wasmtime | true sandbox **by construction** vs sandbox by configuration |
| 2 | bubblewrap + Landlock + seccomp | battle-tested in Flatpak, Steam Linux Runtime |

**Production users**:

- Wasmtime: Fastly Compute, AWS Lambda, Azure, Fermyon Spin, Shopify
  Functions, Envoy / Istio.

**Critical answer**: NO, Landlock + seccomp + capabilities applied to
the host process CANNOT meaningfully isolate an in-process .so.
OpenSSH 9.5+ (gold standard) is fork → child → sandbox → exec, no
magic.

**Rust crates 2026**:

| Crate | Downloads | Notes |
|-------|-----------|-------|
| `landlock` | 8M+ | path-based access control |
| `seccompiler` | — | Cloudflare-grade |
| `extrasafe` | — | high-level wrapper |
| `wasmtime` | 25k stars | first-class WASI Preview 2 |
| `wasmer` | 28k stars | alternative |

**CVE reality** (April 2026):

- CVE-2026-34078 — Flatpak <1.16.4 sandbox escape. No one is perfect.
- Wasmtime: 2 critical CVSS 9.0 in aarch64 Cranelift + Winch backends;
  mitigation = Cranelift x86_64 + Spectre + AOT.

### 7.3 Librarian declarative UI tree precedents (ses_21be9a4f4ffehJtm415gknKecQ)

**Closest match to the proposed design**: Android RemoteViews.

| Precedent | Pattern | Evidence |
|-----------|---------|----------|
| Android RemoteViews | layout reference XML id + sequence of mutation Action whitelist (~40 ops: SetText / SetImage / SetVisibility / SetOnClickPendingIntent). Closed View whitelist via @RemoteView annotation: FrameLayout / LinearLayout / RelativeLayout / GridLayout + AnalogClock / Button / Chronometer / ImageButton / ImageView / ProgressBar / TextClock / TextView. Process isolation via UID. | 15+ years production, billions of devices |
| Android RemoteViews CVE history | confused deputy bugs (CVE-2025-22441 / CVE-2023-21286 / CVE-2022-20470 / CVE-2021-0567). Format itself solid. | — |
| Android RemoteViews performance | cached widget views, `reapply()` diff actions, ashmem shared-memory bitmaps, < 50 ms typical update | — |
| xdg-desktop-portal File Chooser | request system dialog, NOT describe UI | partial pattern match |
| Figma plugins | dual-sandbox (QuickJS code + UI iframe), API calls instead of JSON tree, host materializes | dual-sandbox model |
| VS Code Webview | iframe-based, less constrained vocabulary | partial |
| WidgetKit / Glance | typed widget hierarchy, host renders | partial |

**Comparison summary**: RemoteViews = YES closest. Figma = YES
dual-sandbox. VS Code Webview = partially. xdg-portal = partially.

### 7.4 WASM verdict (synthesized from m0429–m0430)

User asked: "do you recommend WASM based on research?"

**Yes, seriously recommended.**

**Pros (7)**:

1. Sandbox by construction (single-pass typed bytecode, no syscalls
   without explicit imports) vs sandbox by configuration.
2. Cross-language free — Rust / C / Go / Zig / AssemblyScript /
   Python (Pyodide-style) targets.
3. Cold start ~5 ms vs subprocess 20–50 ms.
4. Fuel metering for hard-cap CPU budgets per call.
5. Crash isolation — WASM trap doesn't kill the host.
6. Cross-platform binary distribution (one `.wasm` for all hosts).
7. `.wasm` is easier to verify (sign + hash + reproducible build).

**Cons (7)**:

1. Plugins must compile to WASM (not all crates do).
2. Wasmtime CVEs April 2026 (mitigation: x86_64 Cranelift + Spectre +
   AOT).
3. Heavy native libs (`poppler`, GStreamer, LibreOffice) do NOT work
   in WASM.
4. WASI Preview 2 still maturing.
5. No GTK / Qt calls directly — but this aligns with the declarative
   UI tree anyway.
6. Performance 1–1.5× slower than native (acceptable for plugins).
7. No launcher has shipped this (lixun would be first).

### 7.5 Three-tier architecture (proposed in m0430)

| Tier | Trust | Heavy native deps | Cold start |
|------|-------|-------------------|-----------|
| T1 built-in Rust .so | trusted, in-process | OK | warm (single instance) |
| T2 WASM | untrusted, third-party | NO | ~5 ms |
| T3 (FUTURE) subprocess | trusted-or-untrusted depending on config | OK | 20–50 ms |

**Recommended for v1**: T1 + T2. T3 only if a third-party plugin
genuinely needs heavy native libs and cannot be rewritten in WASM.

User m0431 dismissed the open follow-up questions and pivoted to the
plugin-ideas research; sandbox decisions remain open (plan §7.2).

## 8. Plugin ideas research

### 8.1 Oracle strategic verdict (ses_21bd8cd0cffeWJiqHm9GLkRoQi)

**8 sections** of analysis:

1. **Identity** = "queryable personal workspace browser, local-first,
   inspect-before-open". NOT "Spotlight clone" (commodity race), NOT
   "AI-augmented" (gimmick on Linux), NOT "privacy-first headline"
   (trust property, not a category). Wedge already in the
   architecture: warm preview + plugin-owned rendering + bias to
   inspect-before-open.
2. **Shape**: 6 table-stakes + 2 differentiator plugins. Solo author
   ⇒ narrow breadth + one deep wedge.
3. **v1.0 must-haves** (without these users say "incomplete"):
   files + folders, browser tabs / bookmarks / history, clipboard
   history, system control surfaces (PipeWire / NetworkManager /
   systemd / Bluetooth via D-Bus), calculator + units + currency,
   apps + .desktop actions submenu. Email already has Thunderbird
   foundation.
4. **Killer differentiators**: inspectable local knowledge via rich
   preview (Markdown / Jupyter / SQLite / archives / fonts), compound
   local workspace recall, threaded local communications (extend
   Thunderbird), bundles / archives as first-class searchable
   objects.
5. **Sequencing**: generic local indexing substrate → permission /
   secret boundaries (BEFORE network plugins) → files plugin
   (stresses all common abstractions) → browser / clipboard / system
   → email-deepening → workspace-recall.
6. **NOT to build in v1.x**: SaaS integrations (GitHub / Jira /
   Slack / Notion / Linear / Google Drive / OpenAI maintenance
   treadmill), password manager integrations (security nightmare
   pre-trust-model; if must — `pass` titles-only safest), smart
   home / cloud admin / chat services, AI assistant as a plugin.
7. **Network APIs**: v1.0 strictly local; reserve network for
   permissioned tier later. Define capability boundaries now
   (network / secrets / fs separately) for the future WASM tier.
   Rule: if a plugin's value disappears offline, not v1.
8. **First-mover opportunity** = compound local workspace recall
   (NOT local AI). Surface "what you were just working on" as
   coherent object joined from files + mail threads + browser pages
   + downloads + attachments + code folders, with live preview and
   one-step reopening. Deterministic signals only (temporal
   proximity, shared paths, attachment relationships, sender / domain
   repetition, project folder, browser host, repo) — no ML required.

**Next 6 plugins ranked**: see plan §4 (single source of truth for
sequencing).

### 8.2 Librarian launcher catalog 2026 (ses_21bd9e3a2ffeYwUa5FmhTtOgLs)

**Raycast top extensions** (downloads):

| Extension | Downloads |
|-----------|-----------|
| Kill Process | 561K |
| Color Picker | 418K |
| Google Chrome tabs | 413K |
| Google Translate | 381K |
| Spotify | 375K |
| VS Code | 308K |
| Slack | 247K |
| ChatGPT | 225K |
| Brew | 228K |
| Notion | 218K |

**Universal-demand patterns** (5+ launchers): browser tabs / bookmarks
/ history, calculator, color picker, Spotify / MPRIS, password
manager, translation, system monitor, clipboard history, emoji,
window switcher, kill process, Docker, VS Code projects, GitHub,
timer.

**Linux-specific gaps** (high opportunity):

- Browser tab switcher — HIGH (no Linux launcher does Firefox /
  Chromium tabs well).
- Full email search like Apple-Mail-in-Spotlight — HIGH.
- Recent files via XDG — HIGH.
- Office docs preview — HIGH (GNOME Sushi gap).
- Font preview with character set — MEDIUM.

**Linux-native opportunities** (no macOS equivalent):

| Source | Effort | Demand |
|--------|--------|--------|
| Flatpak / Snap search | M | HIGH |
| systemd service control | S | MEDIUM |
| i3 / Sway WM control | M | HIGH |
| Wayland screen capture | M | MEDIUM |
| PipeWire audio | M | HIGH |

### 8.3 Librarian Linux-native data sources (ses_21bd93f7cffezvjLXBkiSqpgtD)

**Top 15 ranked** (S=small, M=medium, L=large effort; ★=Linux-only
superpower):

| # | Source | Effort | Notes |
|---|--------|--------|-------|
| 1 | Desktop Apps `.desktop` | S | ★★★ Linux-only Actions submenus |
| 2 | systemd Units | S | ★★★ start / stop / restart / status |
| 3 | Package Managers (apt / dnf / pacman / flatpak) | M | ★★★ |
| 4 | Firefox / Chromium History | S | ✓ cross-platform but essential |
| 5 | Git Repositories via `git2` crate | S | recent repos |
| 6 | Password Managers (`pass` / `rbw`) | M | titles-only |
| 7 | Zeal Docsets | S | ★★★ Linux Dash equivalent |
| 8 | Ollama Models | S | local LLM model picker |
| 9 | systemd Journal | M | ★★★ search logs |
| 10 | Podman / Docker | M | container control |
| 11 | Obsidian / Joplin | M | note search |
| 12 | IDE Recent Projects | S | VS Code, JetBrains, Zed |
| 13 | Signal / Telegram | L | message search |
| 14 | Fontconfig | S | ★★★ font preview |
| 15 | NetworkManager / BlueZ | M | ★★★ wifi / bluetooth control |

**Key Rust crates available**:

| Crate | Downloads |
|-------|-----------|
| `freedesktop-desktop-entry` | 514k+ |
| `zbus` | 49M+ |
| `zbus_systemd` | 384k+ |
| `rusqlite` | 50M+ |
| `notify` | 20M+ |
| `git2` | 40M+ |
| `journald-query` | 10k+ |
| `bollard` (Docker) | 1M+ |

## 9. Conversational appendix — preserved user intent

User messages preserved verbatim from this session, in chronological
order, so future sessions can reconstruct exact framing without
relying on summary drift:

- m0356 (toolkit + window kind): "отбой по superinit. У меня вопрос.
  Я планирую расширять дальше preview — добавить pan/zoom/paginate,
  выделение текста, выделение части изображения и т.д. + plugin API
  для пользовательских плагинов. Сейчас preview выглядит как ебучий
  костыль. Хочется большего соответствия apple quicklook. 1. точно ли
  GTK4 лучший фреймворк для наших целей? 2. Точно ли стоит
  использовать shell overlay, вместо нормального окна".
- m0359 (OSS components): "доп вопрос: есть ли какие-то готовые
  opensource компоненты, которые мы можем использовать".
- m0395 (single-toolkit + cross-compositor): "еще вопросы: 1. если уж
  переезжать на qt6, почему сам лончер тоже не перенести? на хуя нам
  тянуть и gtk4 и qt6 в зависимости? 2. мы можем вообще ни к какому
  из этих фреймворков не привязываться? программа должна работать и
  в KDE и GNOME и Hyprland и в Niri и тд".
- m0407 (Cosmic): "еще вопрос: что у нас в Cosmic Desktop?".
- m0417 (sandboxing pivot): "давай подумаем над sandboxed generators
  для linux. хочу быть первым".
- m0418 (sandboxing framing): threat model = untrusted third-party
  plugins (full FS + network + process isolation); plugin language =
  Rust only via stable ABI (Anyrun-style); plugin output = DOM-like
  UI tree (host renders); timeline = research only.
- m0429 (WASM pivot): "давай вернемся к WASM. Рекоммендуешь на
  основании исследований его рассмотреть?".
- m0431 (plugin ideas pivot): "подожди с вопросами. еще один
  глобальный вопрос для исследования: подкинь идеи — какие плагины
  ты мне рекомендуешь разработать для моего продукта?".
- m0442 (identity confirmed): "именно такой фрейм. ты все правильно
  сделал. запиши себе обновить README в этом ключе".
- m0450 (this artifact): "Все результаты исследований в этой сессии
  запиши в блокнот. Или лучше чтобы ничего не потерялось давай
  наметаем план сразу?".
