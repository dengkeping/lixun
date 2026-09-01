//! Main window construction: layer-shell setup, entry + list, keyboard
//! bindings, animations, Toggle ping to the daemon.
//!
//! Service mode (G1.6): the window is built once per process and toggled
//! via `LauncherController::{show, hide, toggle, quit}` driven by
//! `gui_server`. `animate_hide` no longer calls `app.quit()`; only the
//! daemon's explicit `GuiCommand::Quit` triggers process exit, via
//! `LauncherController::quit`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use gtk::gio;
use gtk::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, LayerShell};
use lixun_core::Category;

use crate::factory::{
    add_css_class, clear_cached_hits, create_list_factory, update_results, with_cached_hits,
};
use crate::ipc::{IpcClient, fetch_claimed_prefixes, start_ipc_thread};
use crate::status::StatusBar;
use lixun_core::{DocId, Hit};

/// Decision record for one response cycle: the list of hits to
/// render in order, and optionally the index of the top-hit row
/// that should receive hero styling. Kept pure and widget-free so
/// the reorder logic can be unit-tested headlessly (see
/// window::tests).
///
/// Invariant: when `top_hit_index` is `Some(i)`, `i == 0`. The
/// reorder in `compute_render_plan` floats the top hit to the
/// front; callers rely on this to key hero styling by "row 0 iff
/// top_hit_index.is_some()" without scanning.
#[derive(Debug)]
pub struct RenderPlan {
    pub hits: Vec<Hit>,
    pub top_hit_index: Option<usize>,
}

/// Compose render order from daemon's `hits` and optional
/// `top_hit` nomination. Matches Spotlight + every surveyed
/// open-source launcher: the top hit is NOT a separate structural
/// widget; it IS the first row of the unified list, styled
/// prominently.
///
/// - `top_hit = Some(id)` and a hit with that id exists at
///   position N in `hits` → move that hit to index 0, shift
///   0..N down by one (stable for the rest); return
///   `top_hit_index = Some(0)`.
/// - `top_hit = None`, `top_hit = Some` but not present in hits,
///   or `hits` is empty → leave order untouched; return
///   `top_hit_index = None`.
pub fn compute_render_plan(hits: &[Hit], top_hit: Option<&DocId>) -> RenderPlan {
    let Some(want) = top_hit else {
        return RenderPlan {
            hits: hits.to_vec(),
            top_hit_index: None,
        };
    };
    let Some(pos) = hits.iter().position(|h| h.id == *want) else {
        return RenderPlan {
            hits: hits.to_vec(),
            top_hit_index: None,
        };
    };
    let mut reordered = Vec::with_capacity(hits.len());
    reordered.push(hits[pos].clone());
    for (i, h) in hits.iter().enumerate() {
        if i != pos {
            reordered.push(h.clone());
        }
    }
    RenderPlan {
        hits: reordered,
        top_hit_index: Some(0),
    }
}

pub(crate) type CategoryFilter = std::rc::Rc<std::cell::Cell<Option<Category>>>;

/// Frozen snapshot of a user search session, captured on hide and
/// restored on show. Mirrors Spotlight's UX: if the user dismisses
/// the launcher without launching anything (Escape, focus-loss,
/// toggle-off, preview), their query, results, and selection
/// survive so the next show picks up exactly where they left off.
/// The restored query is presented fully selected, so typing
/// replaces it wholesale (the Spotlight contract) while Enter and
/// the arrows still act on the restored results.
///
/// Only a launch action (Enter, double-click, calculator copy)
/// clears this cache — everything else keeps it. A silent
/// background re-search is issued on restore so the displayed rows
/// reflect any fs-watcher or gloda updates that happened while the
/// launcher was hidden.
#[derive(Clone)]
pub(crate) struct SessionSnapshot {
    pub(crate) query: String,
    pub(crate) hits: Vec<lixun_core::Hit>,
    /// DocId of the selected hit at hide time. Restored by DocId
    /// rather than by index because the silent refresh may reorder
    /// the list; matching on identity keeps the cursor on the same
    /// logical item (or falls back to index 0 if it's gone).
    pub(crate) selected_doc_id: Option<String>,
    /// Which category chip was active. `None` = "All".
    pub(crate) category: Option<Category>,
    /// Which chip button index was active (0..4). Saved separately
    /// from `category` because chip 0 is All (category=None) but
    /// distinct from a future explicit "uncategorized" filter.
    pub(crate) chip_index: usize,
    /// Vertical scroll position of the results list at hide time.
    /// Restored by writing into the scrolled window's vadjustment
    /// after the model is repopulated; without this the list jumps
    /// back to the top on every reopen even when the cursor is far
    /// down the results.
    pub(crate) scroll_position: f64,
}

pub(crate) const DEFAULT_TOP_MARGIN: i32 = 140;

/// Entry logo variants, embedded so installed binaries don't depend
/// on the source checkout's `packaging/` directory (the previous
/// `CARGO_MANIFEST_DIR` path only exists on the build machine). The
/// light logo serves the (default) dark skin; the dark logo serves
/// the light skin.
const LOGO_LIGHT_SVG: &[u8] = include_bytes!("../../../packaging/icons/lixun-logo-light.svg");
const LOGO_DARK_SVG: &[u8] = include_bytes!("../../../packaging/icons/lixun-logo-dark.svg");

/// Transition latch duration. `connect_leave` fires spuriously during the
/// show transition on some compositors (Hyprland, sway); ignoring leave
/// events for this window after each show prevents a show-leave-hide
/// flicker cycle. 150 ms covers the 120 ms fade-slide-in animation plus
/// a small compositor focus-settle margin.
const JUST_SHOWED_GUARD_MS: u64 = 150;

/// Lives for the whole GUI process lifetime. Owns every widget the
/// service-mode command handlers (`show`, `hide`, `toggle`, `quit`,
/// `clear_session`) need to mutate, plus the `session_epoch` that the
/// IPC thread checks before committing search replies. All methods
/// assume they are called on the GTK main thread; the `gui_server`
/// module funnels commands here via `glib::spawn_future_local`.
pub(crate) struct LauncherController {
    window: gtk::ApplicationWindow,
    entry: gtk::Entry,
    chips: std::rc::Rc<CategoryChips>,
    selection: gtk::SingleSelection,
    list_view: gtk::ListView,
    scrolled: gtk::ScrolledWindow,
    status: std::rc::Rc<StatusBar>,
    model: gtk::StringList,
    current_category: CategoryFilter,
    pending_debounce: std::rc::Rc<std::cell::RefCell<Option<glib::SourceId>>>,
    last_query: std::rc::Rc<std::cell::RefCell<String>>,
    session_epoch: Arc<AtomicU64>,
    just_showed_until: std::rc::Rc<std::cell::Cell<Instant>>,
    filter: gtk::CustomFilter,
    /// Snapshot of the last dismissed-but-not-launched session.
    /// Populated by `persist_session` on soft hide, consumed by
    /// `restore_session` on next show, emptied by `clear_session`
    /// on any launch action.
    cached_session: std::rc::Rc<std::cell::RefCell<Option<SessionSnapshot>>>,
    #[allow(dead_code)]
    ipc: IpcClient,
    /// Latch set by `restore_session` so the entry's
    /// connect_changed handler can short-circuit its debounced
    /// search. Critical for selection preservation — see
    /// `restore_session` docstring.
    is_restoring: std::rc::Rc<std::cell::Cell<bool>>,
    /// True when the user has explicitly moved the cursor off the
    /// top row (↑ / ↓ / click / restored via cached session). The
    /// response poller only preserves selection by DocId when this
    /// is true; a fresh keystroke, which clears the flag, makes the
    /// poller always snap to row 0 so ranking order wins (Spotlight
    /// semantic). Without this, the preserve-by-DocId path — useful
    /// during silent refresh — would also chase the previous row's
    /// DocId across every keystroke, warping the cursor to wherever
    /// the new ranking happens to place it (reported as "ends up in
    /// middle of list after second query").
    user_selected_override: std::rc::Rc<std::cell::Cell<bool>>,
    #[allow(dead_code)]
    searching_indicator: std::rc::Rc<std::cell::Cell<bool>>,
    /// True between Space-to-preview and Escape/launch. While true,
    /// arrow-key selection changes fan out to the preview daemon
    /// (debounced) so the user sees the highlighted row rendered
    /// live, Spotlight-style. Also gates `focus_ctrl.connect_leave`:
    /// clicking into the preview window yields launcher focus, and
    /// without this gate the leave handler would auto-hide the
    /// launcher and kill the preview session. Reset by Escape
    /// (keymap), hide() (soft), clear_and_hide() (launch), and
    /// quit(). Oracle invariant: preview-mode-active belongs to the
    /// launcher, not the daemon — the daemon is source of truth for
    /// the preview process lifecycle, the launcher is source of
    /// truth for "should we still be live-previewing at all".
    preview_mode_active: std::rc::Rc<std::cell::Cell<bool>>,
    /// `[gui] show_recents`: render the daemon's frecency hits when
    /// the launcher opens on an empty query (O4).
    show_recents_enabled: bool,
    /// Pre-rendered key-hint line for the idle/results footer (O3),
    /// built once from the resolved keybindings.
    hint_line: String,
    /// Pending debounce for selection-driven preview updates. 50ms
    /// per Oracle compromise: short enough that the user sees the
    /// preview track their arrows, long enough that holding an
    /// arrow key does not fire one preview per row traversed
    /// (which would thrash the preview plugin's build/update path
    /// and defeat the Ready-state reuse in `preview_spawn.rs`). The
    /// preview-side epoch-drop from lixun-ipc::preview::PROTOCOL_VERSION
    /// protects against races we don't debounce out — this is a
    /// belt-and-suspenders design (Oracle #10).
    preview_debounce: std::rc::Rc<std::cell::RefCell<Option<glib::SourceId>>>,
    /// Pre-slide layer-shell position, saved while preview mode holds
    /// the launcher out of the preview's way (left-edge tuck in
    /// window-managed placement, centered in the left column in
    /// overlay placement — see `slide_for_preview`). `None` when not
    /// slid.
    /// Restored verbatim on preview exit and NEVER written to the
    /// persistent per-monitor position store — the slide is a
    /// transient layout, not a user preference. (A Super+drag during
    /// preview mode still persists its own position for future cold
    /// starts, but exit restores the pre-slide layout for this
    /// session.)
    preview_slide_saved: std::cell::RefCell<Option<crate::preview_layout::SavedSlidePosition>>,
    /// `[gui] preview_width_percent` from the shared daemon config,
    /// used with `preview_max_width_px` to predict the width the
    /// preview surface will claim (the preview process derives its
    /// width from the same config fields — see `apply_monitor_and_cap`
    /// in `lixun-preview-bin`).
    preview_width_percent: u8,
    /// `[gui] preview_max_width_px` companion cap for the prediction.
    preview_max_width_px: i32,
    /// Slide target resolved once at build from
    /// `[gui] preview_placement` and runtime layer-shell support:
    /// `WindowManaged` tucks the launcher against the left edge,
    /// `OverlayColumn` centers it in the column beside the
    /// right-anchored overlay preview.
    slide_mode: crate::preview_layout::SlideMode,
}

impl LauncherController {
    pub(crate) fn is_visible(&self) -> bool {
        self.window.is_visible()
    }

    /// Make the window visible. Returns the resulting visibility
    /// (`true` on success). Recomputes the monitor so re-shows track
    /// the pointer across multi-monitor setups.
    ///
    /// If a session snapshot was cached by the previous soft-hide,
    /// restore it before presenting: the user sees their prior
    /// query, results, and selection immediately (no flash of empty
    /// launcher), with a silent background refresh catching the
    /// results up to any index changes that happened in between.
    pub(crate) fn show(&self) -> bool {
        // Already visible: nothing to restore. Avoids snapshot consumption
        // when the daemon dispatches Show after a preview close while the
        // launcher stayed visible (Phase 1 xdg-toplevel preview UX).
        // Without this guard, take() drains cached_session and a stale
        // empty snapshot wipes the live results.
        if self.window.is_visible() {
            return true;
        }
        self.recompute_monitor();

        let snapshot = self.cached_session.borrow_mut().take();
        tracing::info!(
            "gui: show() snapshot_present={} entry_text_before={:?}",
            snapshot.is_some(),
            self.entry.text().to_string()
        );
        if let Some(snapshot) = snapshot {
            // If the live UI already matches the snapshot (same query
            // and the model is non-empty), the previous hide() left
            // everything in place — just present the window. Skipping
            // restore_session avoids a full model rebuild on every
            // ESC→toggle cycle: model rows, selection, scroll position
            // and entry text are already correct, and re-populating
            // resets vadjustment to 0 which we then have to chase with
            // a retry-poll. Falls through to restore_session only on
            // the cold path (model was scrubbed, or this is a fresh
            // GUI process started by the daemon).
            let live_query = self.entry.text().to_string();
            let model_populated = self.model.n_items() > 0;
            if model_populated && live_query == snapshot.query {
                tracing::info!("gui: show() reusing live state, skipping restore_session");
            } else {
                tracing::info!(
                    "gui: show() restoring snapshot query={:?} hits={}",
                    snapshot.query,
                    snapshot.hits.len()
                );
                self.restore_session(&snapshot);
            }
        }

        // Empty launcher: no session to restore — offer the user's
        // recent hits instead of a dead pane (O4). Spotlight shows
        // Recents; the window may open tall when they exist.
        if self.entry.text().is_empty() && self.model.n_items() == 0 {
            self.maybe_show_recents();
        }

        self.window.remove_css_class("lixun-hiding");
        // Reduced motion: skip the CSS motion class (and its removal
        // timer below) entirely when the desktop disables animations.
        let animate = animations_enabled();
        if animate {
            self.window.add_css_class("lixun-showing");
        }
        self.window.set_visible(true);
        {
            let w = self.window.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(100), move || {
                report_launcher_geometry(&w);
            });
        }
        self.arm_layer_shell_focus();
        tracing::info!(
            "gui: show() called arm_layer_shell_focus; entry has_focus={}",
            self.entry.has_focus()
        );
        // Select the (possibly restored) query wholesale: typing on
        // reopen replaces it instead of appending ("firefoxchrome").
        self.entry.select_region(0, -1);
        // Armed on BOTH paths: the guard papers over compositor
        // focus-settle races on show (see JUST_SHOWED_GUARD_MS), not
        // just the animation window.
        self.just_showed_until
            .set(Instant::now() + Duration::from_millis(JUST_SHOWED_GUARD_MS));

        if animate {
            let window_weak = self.window.downgrade();
            glib::timeout_add_local_once(Duration::from_millis(120), move || {
                if let Some(w) = window_weak.upgrade() {
                    w.remove_css_class("lixun-showing");
                }
            });
        }
        true
    }

    /// Soft-hide: make the window invisible but keep the current
    /// session (query + results + selection) in `cached_session` so
    /// the next `show()` restores it. This is the Spotlight-style
    /// default for every dismiss that is NOT a launch action
    /// (Escape, focus-loss, toggle-off, preview-open, preview-close).
    /// Does NOT exit the process; only `quit()` does.
    pub(crate) fn hide(&self) -> bool {
        // Reset the flag WITHOUT dismissing the preview: sending
        // PreviewHide on this path would tear down a preview that is
        // still wanted (user-driven dismissals go through
        // `toggle`/`clear_and_hide`, which call
        // `dismiss_active_preview` first). Routed through the
        // `set_preview_mode_active` funnel so the preview-mode slide
        // is restored (and the preview debounce cancelled) exactly
        // like every other exit path.
        self.set_preview_mode_active(false);
        self.persist_session();
        self.animate_hide();
        false
    }

    /// Hard-hide: clear the session completely, then hide. Used by
    /// every launch-completing action (Enter, primary/secondary,
    /// double-click, calculator copy) where the user has finished
    /// the task and expects a fresh launcher next time.
    pub(crate) fn clear_and_hide(&self) -> bool {
        self.cancel_preview_debounce();
        // A completed launch dismisses a live preview too — leaving
        // its toplevel behind after the launcher vanished orphans it.
        self.dismiss_active_preview();
        self.clear_session();
        self.animate_hide();
        false
    }

    /// Send the preview-hide request iff a preview session is live,
    /// and drop the local flag. Same request the keymap Escape path
    /// sends; the epoch machinery on the preview side makes a
    /// duplicate harmless.
    fn dismiss_active_preview(&self) {
        if self.preview_mode_active.get() {
            crate::ipc::send_preview_hide_request();
            // Funnel through the setter so the preview-mode slide is
            // restored alongside the flag reset.
            self.set_preview_mode_active(false);
        }
    }

    fn animate_hide(&self) {
        self.window.remove_css_class("lixun-showing");
        if !animations_enabled() {
            // Reduced motion: no .lixun-hiding keyframe, no 120 ms
            // defer — unmap immediately.
            self.window.set_visible(false);
            return;
        }
        self.window.add_css_class("lixun-hiding");

        let window_weak = self.window.downgrade();
        glib::timeout_add_local_once(Duration::from_millis(120), move || {
            if let Some(w) = window_weak.upgrade() {
                w.set_visible(false);
                w.remove_css_class("lixun-hiding");
            }
        });
    }

    /// Idle "Recent" section (O4): when the launcher shows with an
    /// empty query (fresh open, or the user cleared it), ask the
    /// daemon for its frecency-ranked hits and render them through
    /// the normal results path. Epoch-guarded: any keystroke bumps
    /// the session epoch and the reply is dropped. The rows carry a
    /// "Recent" kind label; ranking (top_hit) is not applied — the
    /// frecency order IS the ranking.
    pub(crate) fn maybe_show_recents(&self) {
        const RECENTS_LIMIT: u32 = 8;
        if !self.show_recents_enabled || !self.entry.text().is_empty() {
            return;
        }
        let epoch = self.session_epoch.load(Ordering::SeqCst);
        let session_epoch = Arc::clone(&self.session_epoch);
        let entry = self.entry.clone();
        let model = self.model.clone();
        let selection = self.selection.clone();
        let filter = self.filter.clone();
        let scrolled = self.scrolled.clone();
        let status = std::rc::Rc::clone(&self.status);
        let hints = self.hint_line.clone();
        crate::ipc::fetch_recents_async(RECENTS_LIMIT, move |mut hits| {
            if session_epoch.load(Ordering::SeqCst) != epoch || !entry.text().is_empty() {
                return;
            }
            if hits.is_empty() {
                return;
            }
            for h in &mut hits {
                h.kind_label = Some("Recent".into());
            }
            update_results(&model, &selection, &hits, None);
            filter.changed(gtk::FilterChange::Different);
            if selection.n_items() > 0 {
                selection.set_selected(0);
            }
            scrolled.set_visible(true);
            scrolled.set_vexpand(false);
            status.show_hints(&hints);
        });
    }

    /// Soft-hide WITHOUT leaving preview mode (`GuiCommand::SoftHide`,
    /// P1): unmap the launcher while a preview window genuinely needs
    /// its screen area, but keep `preview_mode_active` — and with it
    /// the selection→preview pipeline — alive so `PreviewNav` relays
    /// keep scrubbing results. The regular `hide()` remains the
    /// dismissal path and continues to reset preview mode.
    pub(crate) fn soft_hide(&self) -> bool {
        self.persist_session();
        self.animate_hide();
        false
    }

    /// Apply a relayed preview navigation key (`GuiCommand::PreviewNav`,
    /// P1 keyboard continuity): move the result selection by `delta`
    /// rows through the same selection model as local arrow keys, so
    /// the debounced selection→preview fan-out re-fires. Clamped to
    /// the filtered list bounds; no-op without results.
    pub(crate) fn preview_nav(&self, delta: i32) {
        let n = self.selection.n_items();
        if n == 0 {
            return;
        }
        let current = self.selection.selected();
        let base = if current == gtk::INVALID_LIST_POSITION {
            0
        } else {
            current as i64 + i64::from(delta)
        };
        let target = base.clamp(0, i64::from(n) - 1) as u32;
        if target != current {
            self.selection.set_selected(target);
            self.user_selected_override.set(true);
            self.list_view
                .scroll_to(target, gtk::ListScrollFlags::NONE, None);
        }
    }

    /// Flip visibility. Single source of truth for service-mode toggle:
    /// daemon just sends `GuiCommand::Toggle`, the GUI inspects
    /// `window.is_visible()` and picks show or hide.
    pub(crate) fn toggle(&self) -> bool {
        if self.window.is_visible() {
            // Super+Space with a preview open dismisses both:
            // hiding only the launcher would orphan the preview
            // toplevel with no keyboard path back to it.
            self.dismiss_active_preview();
            self.hide()
        } else {
            self.show()
        }
    }

    /// Exit the GTK application. Called only from the daemon's
    /// `GuiCommand::Quit` path (graceful shutdown).
    pub(crate) fn quit(&self) {
        self.window.close();
        if let Some(app) = self.window.application() {
            app.quit();
        }
    }

    /// Drop only the cached session snapshot without touching the
    /// live UI state. Called by `GuiCommand::ClearSession` from the
    /// daemon after a preview process exits with the "launched"
    /// sentinel — the launcher is already hidden (persist_session
    /// fired during the Space → preview handoff), and we only need
    /// to invalidate the cache so the next show opens blank.
    ///
    /// The UI still needs scrubbing despite being invisible:
    /// persist_session deliberately leaves entry/model/selection
    /// populated so restore_session can flash them back instantly
    /// on the next show. Without a scrub here, that stale state
    /// becomes visible the moment the user hits Super+Space.
    pub(crate) fn drop_cached_session(&self) {
        self.scrub_ui();
    }

    /// Mark the selection as user-chosen so the response poller's
    /// preserve-by-DocId path activates on the next reply. Called
    /// by keymap navigation (↑/↓/Ctrl variants) and factory
    /// click/tap handlers. Cleared automatically on fresh keystroke
    /// in the entry handler.
    pub(crate) fn mark_user_selected(&self) {
        self.user_selected_override.set(true);
    }

    /// Single funnel for preview-mode transitions. Every enter path
    /// (keymap `quick_look` / `quick_look_alt`) and every exit path
    /// (keymap Escape/typing/launch, `dismiss_active_preview`,
    /// `hide`, `GuiCommand::ExitPreviewMode`, the daemon's
    /// preview-error path) goes through here, which is what lets the
    /// slide-beside-preview layout stay balanced: enter slides the
    /// launcher into the left column, exit restores the exact saved
    /// position. Both legs are idempotent — the saved-position slot
    /// guards re-entry, so a repeated `true` cannot re-save a slid
    /// position and a repeated `false` cannot double-restore.
    pub(crate) fn set_preview_mode_active(&self, active: bool) {
        self.preview_mode_active.set(active);
        if active {
            self.slide_for_preview();
        } else {
            self.cancel_preview_debounce();
            self.restore_preview_slide();
        }
    }

    /// Slide the launcher out of the incoming preview's way. Saves
    /// the current anchors/margins first so `restore_preview_slide`
    /// can put them back exactly. The target depends on `slide_mode`
    /// ([`crate::preview_layout::slide_left_margin`]):
    ///
    /// * `WindowManaged` (`[gui] preview_placement = "window"`, the
    ///   default): tuck against the left screen edge. The WM owns
    ///   the preview's placement, so with typical centered placement
    ///   this minimises the initial overlap; any residue is
    ///   user-recoverable by dragging the preview.
    /// * `OverlayColumn` (`"overlay"` with layer-shell): center in
    ///   the left column beside the right-anchored overlay preview.
    ///   When the column is too narrow the slide is skipped
    ///   entirely: the preview process applies the same fits
    ///   predicate to the geometry we report and soft-hides the
    ///   launcher instead (`PreviewEvent::SetLauncherVisible(false)`
    ///   → `GuiCommand::SoftHide`), which preserves preview mode.
    fn slide_for_preview(&self) {
        use gtk4_layer_shell::{Edge, LayerShell};
        if self.preview_slide_saved.borrow().is_some() {
            return; // already slid
        }
        // Use the monitor the launcher surface actually occupies —
        // the same one `current_monitor_connector` reports to the
        // daemon for the preview request, so the slide math and the
        // preview surface agree on the output. Pointer-monitor
        // fallback only for the unrealized-surface edge case.
        let monitor = self
            .window
            .surface()
            .and_then(|s| gtk::prelude::WidgetExt::display(&self.window).monitor_at_surface(&s))
            .or_else(pick_current_monitor);
        let Some(monitor) = monitor else {
            return;
        };
        let mon_w = monitor.geometry().width();
        let (launcher_w, _) = window_size(&self.window);
        let preview_w = crate::preview_layout::predicted_preview_width(
            mon_w,
            self.preview_width_percent,
            self.preview_max_width_px,
        );
        let Some(target_left) =
            crate::preview_layout::slide_left_margin(self.slide_mode, mon_w, launcher_w, preview_w)
        else {
            tracing::info!(
                "gui: preview slide skipped (mode={:?} mon_w={} launcher_w={} preview_w={})",
                self.slide_mode,
                mon_w,
                launcher_w,
                preview_w
            );
            return;
        };
        *self.preview_slide_saved.borrow_mut() =
            Some(crate::preview_layout::SavedSlidePosition {
                left_anchored: self.window.is_anchor(Edge::Left),
                left: self.window.margin(Edge::Left),
                top: self.window.margin(Edge::Top),
            });
        self.window.set_anchor(Edge::Left, true);
        self.window.set_margin(Edge::Left, target_left);
        tracing::info!(
            "gui: preview slide → left={} (mode={:?} mon_w={} launcher_w={} preview_w={})",
            target_left,
            self.slide_mode,
            mon_w,
            launcher_w,
            preview_w
        );
        // Tell the daemon (and through it the preview process) where
        // the launcher now sits, so the preview's fits-beside check
        // runs against the slid rect.
        report_launcher_geometry(&self.window);
    }

    /// Undo `slide_for_preview`: restore the exact anchors/margins
    /// saved at slide time. No-op when the launcher never slid (the
    /// column was too narrow, or preview mode never activated).
    /// Deliberately does not touch the persistent position store.
    fn restore_preview_slide(&self) {
        use gtk4_layer_shell::{Edge, LayerShell};
        let Some(saved) = self.preview_slide_saved.borrow_mut().take() else {
            return;
        };
        self.window.set_anchor(Edge::Left, saved.left_anchored);
        self.window.set_margin(Edge::Left, saved.left);
        self.window.set_margin(Edge::Top, saved.top);
        tracing::info!(
            "gui: preview slide restored (left_anchored={} left={} top={})",
            saved.left_anchored,
            saved.left,
            saved.top
        );
        report_launcher_geometry(&self.window);
    }

    pub(crate) fn preview_mode_active(&self) -> bool {
        self.preview_mode_active.get()
    }

    /// Re-arm layer-shell keyboard interactivity. Toggles the
    /// keyboard mode `None → OnDemand` to force the compositor
    /// (e.g. KWin) to re-evaluate `keyboard_interactivity` for the
    /// layer surface, then routes focus to the entry.
    ///
    /// Why this exists. `zwlr_layer_surface_v1` keyboard focus is
    /// compositor-controlled via `keyboard_interactivity`. Once
    /// the surface has been mapped and unmapped at least once,
    /// KWin does not regrant `OnDemand` keyboard focus on a
    /// subsequent `set_visible(true)` — the surface still "exists"
    /// with the prior mode, but the compositor sees no reason to
    /// grant the seat keyboard back. Toggling the mode through
    /// `None` triggers a re-evaluation that recovers focus.
    ///
    /// `xdg-toplevel`'s `set_startup_id` + `present` is NOT
    /// applicable here — that is xdg-shell activation, semantically
    /// distinct from layer-shell keyboard interactivity, and KWin
    /// does not honour xdg-shell startup IDs on layer surfaces.
    fn arm_layer_shell_focus(&self) {
        self.window.set_keyboard_mode(KeyboardMode::None);
        self.window.set_keyboard_mode(KeyboardMode::OnDemand);
        self.entry.grab_focus();
    }

    /// React to `GuiCommand::ExitPreviewMode` from the daemon: the
    /// warm preview process reported that the user dismissed its
    /// window (Escape/Space inside preview), so we must leave
    /// preview mode and pull keyboard focus back to the launcher.
    ///
    /// `grab_focus` alone is insufficient: it only chooses which
    /// widget is focused *within* a surface that already owns the
    /// Wayland seat keyboard. When the preview toplevel took the
    /// seat and then hid, the compositor does not hand the seat
    /// back to the launcher's layer surface on its own, so arrow
    /// keys would go nowhere. We rearm the layer-shell keyboard
    /// interactivity (`arm_layer_shell_focus`) which forces the
    /// compositor to re-evaluate `keyboard_interactivity` for the
    /// surface, after which `grab_focus` routes input to the entry.
    ///
    /// `activation_token` is preserved on the wire (the preview
    /// process still mints it as part of its dismissal handshake)
    /// but unused by this body: layer surfaces are not toplevels
    /// and do not honour xdg-shell startup IDs.
    pub(crate) fn exit_preview_mode(&self, activation_token: Option<String>) {
        tracing::info!(
            "gui: exit_preview_mode token_present={} window_visible={} entry_focus={} preview_active={}",
            activation_token.is_some(),
            self.window.is_visible(),
            self.entry.has_focus(),
            self.preview_mode_active.get()
        );
        let _ = activation_token;
        self.set_preview_mode_active(false);
        self.arm_layer_shell_focus();
    }

    fn cancel_preview_debounce(&self) {
        if let Some(id) = self.preview_debounce.borrow_mut().take() {
            id.remove();
        }
    }

    /// Reset every piece of session state so the next show is clean.
    /// Called by launch-completing actions via `clear_and_hide`.
    /// Bumps `session_epoch` first so any in-flight search replies
    /// land in a new epoch and get discarded by the IPC poller;
    /// the remainder of the work is delegated to `scrub_ui`.
    pub(crate) fn clear_session(&self) {
        self.session_epoch.fetch_add(1, Ordering::SeqCst);
        self.scrub_ui();
    }

    /// Return every piece of transient UI to the "blank launcher"
    /// state: drop the cached snapshot, clear the user-selected
    /// override, cancel any debounced search, empty the entry,
    /// collapse chips+status, wipe the results model, and restore
    /// the selection to INVALID. The `autoselect` toggle around
    /// the model drain prevents SingleSelection's interpolation
    /// formula (gtksingleselection.c:253-296) from drifting the
    /// cursor on the per-row items-changed emissions during the
    /// remove loop. Callers wrap this with whatever pre-state work
    /// they need (epoch bump, visibility change, etc.).
    fn scrub_ui(&self) {
        self.cached_session.borrow_mut().take();
        self.user_selected_override.set(false);

        if let Some(id) = self.pending_debounce.borrow_mut().take() {
            id.remove();
        }

        self.entry.set_text("");
        self.chips.activate_index(0);
        self.current_category.set(None);
        self.filter.changed(gtk::FilterChange::Different);

        self.selection.set_autoselect(false);
        let n = self.model.n_items();
        for _ in 0..n {
            self.model.remove(0);
        }
        self.selection.set_selected(gtk::INVALID_LIST_POSITION);
        self.selection.set_autoselect(true);
        clear_cached_hits();

        self.scrolled.set_visible(false);
        self.scrolled.set_vexpand(false);
        self.chips.container.set_visible(false);
        self.status.hide();

        self.last_query.borrow_mut().clear();
    }

    /// Capture the current session into `cached_session` and then
    /// quiesce in-flight IPC + debounce without touching the UI
    /// state (entry text, model items, selection remain intact in
    /// case of abort — though nothing currently aborts a hide).
    /// The UI itself gets hidden by `animate_hide`; this method
    /// only deals with state management.
    ///
    /// Called by soft-hide paths: Escape, focus-loss, toggle-off,
    /// preview invocation. If the current query is empty there is
    /// no session worth saving — clear the cache instead, so a
    /// "blank launcher → Escape → Super+Space" cycle doesn't
    /// restore a ghost of some previous non-empty session.
    fn persist_session(&self) {
        self.session_epoch.fetch_add(1, Ordering::SeqCst);
        if let Some(id) = self.pending_debounce.borrow_mut().take() {
            id.remove();
        }

        let query = self.entry.text().to_string();
        tracing::info!("gui: persist_session() query={:?}", query);
        if query.is_empty() {
            tracing::info!("gui: persist_session() empty query → CLEARING cached_session");
            self.cached_session.borrow_mut().take();
            return;
        }

        let selected_doc_id = {
            let idx = self.selection.selected();
            self.selection.item(idx).and_then(|obj| {
                obj.downcast::<gtk::StringObject>()
                    .ok()
                    .map(|s| s.string().to_string())
            })
        };

        let hits = with_cached_hits(|h| h.to_vec());
        let snapshot = SessionSnapshot {
            query,
            hits,
            selected_doc_id,
            category: self.current_category.get(),
            chip_index: self.chips.active_index().unwrap_or(0),
            scroll_position: self.scrolled.vadjustment().value(),
        };
        *self.cached_session.borrow_mut() = Some(snapshot);
    }

    /// Restore a `SessionSnapshot` captured by `persist_session`
    /// into the UI and fire a silent background re-search so the
    /// displayed rows catch up with any index updates that
    /// happened while the launcher was hidden.
    ///
    /// Two non-obvious gotchas:
    ///
    /// - `is_restoring` latch neutralises the entry's
    ///   connect_changed handler while we call `entry.set_text`.
    ///   Without it the handler schedules a duplicate debounced
    ///   search that races our silent refresh; whichever reply
    ///   wins hits the response poller, which then recomputes
    ///   the cursor from its own `prior_selected` snapshot and
    ///   can land on the wrong row.
    ///
    /// - `filter.changed` must be called AFTER `update_results`,
    ///   not only before. FilterListModel does not recompute
    ///   `n_items` eagerly on child-model append; without the
    ///   second invalidation the DocId lookup below sees an empty
    ///   filtered view and falls back to index 0 — which is the
    ///   exact bug report ("selection always 1st row") this
    ///   method exists to fix.
    fn restore_session(&self, snapshot: &SessionSnapshot) {
        self.is_restoring.set(true);

        self.chips.activate_index(snapshot.chip_index);
        self.current_category.set(snapshot.category);
        self.filter.changed(gtk::FilterChange::Different);

        *self.last_query.borrow_mut() = snapshot.query.clone();

        update_results(&self.model, &self.selection, &snapshot.hits, None);
        self.filter.changed(gtk::FilterChange::Different);

        let selected_idx = snapshot
            .selected_doc_id
            .as_deref()
            .and_then(|want| {
                (0..self.selection.n_items()).find(|&i| {
                    self.selection
                        .item(i)
                        .and_then(|o| o.downcast::<gtk::StringObject>().ok())
                        .map(|s| s.string() == want)
                        .unwrap_or(false)
                })
            })
            .unwrap_or(0);
        if self.selection.n_items() > 0 {
            self.selection.set_selected(selected_idx);
        }

        self.chips.container.set_visible(true);
        if !snapshot.hits.is_empty() {
            self.scrolled.set_visible(true);
            self.scrolled.set_vexpand(false);
        }

        self.entry.set_text(&snapshot.query);
        // Restored query arrives fully selected (see the
        // `SessionSnapshot` docs): first keystroke replaces it.
        self.entry.select_region(0, -1);

        // Defer scroll restore until after GTK lays out the new rows;
        // vadjustment.upper() is only valid post-allocate, so calling
        // set_value() inline here clamps to the current (still-zero)
        // upper bound and the list lands at the top. ListView is
        // virtualised and computes upper across several frames as it
        // measures rows, so retry until upper covers target or we've
        // burned the budget. 16 ms × 30 ≈ half a second cap.
        let scrolled = self.scrolled.clone();
        let target = snapshot.scroll_position;
        let attempts = std::rc::Rc::new(std::cell::Cell::new(0u32));
        glib::timeout_add_local(std::time::Duration::from_millis(16), move || {
            let adj = scrolled.vadjustment();
            let upper = adj.upper() - adj.page_size();
            if upper >= target || attempts.get() >= 30 {
                adj.set_value(target.min(upper.max(0.0)));
                glib::ControlFlow::Break
            } else {
                attempts.set(attempts.get() + 1);
                glib::ControlFlow::Continue
            }
        });

        // The restored cursor is user intent from the prior session;
        // arm the override so the silent refresh's poller run
        // preserves the DocId we just selected rather than snapping
        // the cursor to row 0.
        self.user_selected_override.set(true);

        self.is_restoring.set(false);
    }

    fn recompute_monitor(&self) {
        let monitor = pick_current_monitor();
        // Assign the surface to its output first: in layer-shell the
        // top/left margins are interpreted relative to the monitor the
        // surface is bound to, so the binding must happen before we
        // apply any margin so the offsets land on the right output.
        self.window.set_monitor(monitor.as_ref());
        if let Some(monitor) = monitor {
            apply_validated_placement(&self.window, &monitor);
        }
    }
}

/// Honour the desktop's reduced-motion preference: when
/// `gtk-enable-animations` is off, `show`/`animate_hide` skip the CSS
/// motion classes and their fixed 120 ms timers. The
/// JUST_SHOWED_GUARD latch is independent of this — it papers over
/// compositor focus races, not animation timing — and stays armed
/// either way.
fn animations_enabled() -> bool {
    gtk::Settings::default()
        .map(|s| s.is_gtk_enable_animations())
        .unwrap_or(true)
}

/// Pick the monitor the launcher should appear on.
///
/// Prefers the output under the pointer (the user's active screen),
/// then falls back to the first connected monitor. Returns `None`
/// only when no display/monitor is available at all.
fn pick_current_monitor() -> Option<gtk::gdk::Monitor> {
    let display = gtk::gdk::Display::default()?;
    if let Some(seat) = display.default_seat()
        && let Some(pointer) = seat.pointer()
    {
        let (surface, _, _) = pointer.surface_at_position();
        if let Some(surface) = surface
            && let Some(monitor) = display.monitor_at_surface(&surface) {
                return Some(monitor);
            }
    }
    display
        .monitors()
        .item(0)
        .and_downcast::<gtk::gdk::Monitor>()
}

/// Where the launcher surface should sit on a given monitor.
enum Placement {
    /// Pin the surface at an exact (top, left) offset (Left anchored).
    Pinned { top: i32, left: i32 },
    /// Drop the Left anchor and let the compositor centre the surface
    /// horizontally; pin only the vertical offset.
    Centered { top: i32 },
}

/// Decide where to place the launcher on `mon_geom` given a window size
/// and an optional saved position.
///
/// A saved position is honoured only when it actually fits the current
/// monitor. A wildly out-of-range offset (e.g. an x saved on a wider
/// external monitor that is no longer connected) is a stale layout
/// leak, not a user preference, so we centre instead of clamping it to
/// the edge. A minor overflow (the window spilling a little past the
/// bottom/right edge) is clamped back into range.
fn decide_placement(
    mon_w: i32,
    mon_h: i32,
    win_w: i32,
    win_h: i32,
    saved: Option<crate::launcher_position::SavedPosition>,
) -> Placement {
    let Some(saved) = saved else {
        return Placement::Centered {
            top: DEFAULT_TOP_MARGIN,
        };
    };

    // Reject positions whose origin is off the monitor entirely, or
    // negative — these come from a different display layout.
    let origin_off_screen = saved.left < 0
        || saved.top < 0
        || (mon_w > 0 && saved.left >= mon_w)
        || (mon_h > 0 && saved.top >= mon_h);
    if origin_off_screen {
        return Placement::Centered {
            top: DEFAULT_TOP_MARGIN,
        };
    }

    // Origin is on-screen; clamp minor overflow so the whole window
    // stays visible.
    let mut left = saved.left;
    let mut top = saved.top;
    if mon_w > 0 && win_w > 0 {
        left = left.min((mon_w - win_w).max(0));
    }
    if mon_h > 0 && win_h > 0 {
        top = top.min((mon_h - win_h).max(0));
    }
    Placement::Pinned { top, left }
}

/// Read the current window size, falling back to the configured
/// default width/height before the surface has been measured.
fn window_size(window: &gtk::ApplicationWindow) -> (i32, i32) {
    let mut w = window.width();
    let mut h = window.height();
    if w <= 0 {
        w = window.default_width();
    }
    if h <= 0 {
        h = window.default_height();
    }
    (w, h)
}

/// Validate and apply the saved (or default) placement for `monitor`,
/// rewriting the anchor and margins on `window`.
///
/// This is the single source of truth for launcher positioning. It runs
/// every time the launcher is shown (via `recompute_monitor`) and at
/// startup, so a position saved on a now-disconnected monitor can never
/// pin the surface off the currently connected screen.
fn apply_validated_placement(window: &gtk::ApplicationWindow, monitor: &gtk::gdk::Monitor) {
    use gtk4_layer_shell::{Edge, LayerShell};
    let geom = monitor.geometry();
    let (win_w, win_h) = window_size(window);
    let connector = monitor.connector().map(|s| s.to_string());
    let saved = crate::launcher_position::load(connector.as_deref());
    match decide_placement(geom.width(), geom.height(), win_w, win_h, saved) {
        Placement::Pinned { top, left } => {
            window.set_anchor(Edge::Left, true);
            window.set_margin(Edge::Top, top);
            window.set_margin(Edge::Left, left);
        }
        Placement::Centered { top } => {
            window.set_anchor(Edge::Left, false);
            window.set_margin(Edge::Left, 0);
            window.set_margin(Edge::Top, top);
        }
    }
}

/// Live semantic availability as last reported by the daemon (F6):
/// `(config_enabled, human state label, worker_ready)`. Shared
/// between the startup status fetch, the zero-hit refresh, and the
/// settings menu so the menu label reflects worker health, not
/// config-section presence.
pub(crate) type SemanticUiState = std::rc::Rc<std::cell::RefCell<Option<(bool, String, bool)>>>;

/// Human display for an accel string, via GTK's own formatter so the
/// footer/tooltip text matches user rebinds ("<Ctrl>c" → "Ctrl+C").
/// Falls back to the raw string when unparseable.
fn accel_display(accel: &str) -> String {
    match gtk::accelerator_parse(accel) {
        Some((key, mods)) => gtk::accelerator_get_label(key, mods).to_string(),
        None => accel.to_string(),
    }
}

/// Persistent dimmed key-hint footer (O3), built from the LIVE
/// resolved keybindings so user rebinds display truthfully.
fn build_hint_line(kb: &lixun_config::Keybindings) -> String {
    format!(
        "{} Open · {} Reveal · {} Preview · {} Copy · ? Shortcuts",
        accel_display(&kb.primary_action),
        accel_display(&kb.secondary_action),
        accel_display(&kb.quick_look),
        accel_display(&kb.copy),
    )
}

/// Rotating idle placeholder hints (O3). Built from the daemon's
/// claimed prefixes so plugin-owned prefixes appear without the GUI
/// naming any plugin (hard-modularity: the strings come from the
/// generic ClaimedPrefixes fetch).
fn build_placeholder_hints(claimed_prefixes: &[String]) -> Vec<String> {
    let mut hints = vec![
        "Search\u{2026}".to_string(),
        "Space previews · Ctrl+1\u{2026}4 filters".to_string(),
        "? shows shortcuts".to_string(),
    ];
    for p in claimed_prefixes {
        hints.push(format!("Type {p} for instant answers"));
    }
    hints
}

/// Shortcuts overlay (O3): a popover listing every RESOLVED
/// keybinding so user rebinds display truthfully. Opened from ? (on
/// an empty entry) and F1 via the keymap; calling it again while the
/// popover is up toggles it closed.
///
/// CRITICAL: the popover must NOT take the default autohide grab. An
/// xdg_popup grab moves keyboard focus to the popup surface, which
/// fires the launcher's `focus_ctrl.connect_leave` → `hide()`, and
/// the popover dies with its unmapped parent — "cheatsheet flashes,
/// then both windows disappear". With `autohide(false)` there is no
/// grab: focus never leaves the entry, typing keeps working, and the
/// keymap owns dismissal (any key closes it; Escape/?/F1 close-only).
/// Real focus loss (clicking another app) still hides the launcher
/// normally, unmapping the popover with it — the Spotlight contract.
pub(crate) fn show_shortcuts_overlay(
    entry: &gtk::Entry,
    kb: &lixun_config::Keybindings,
    slot: &std::rc::Rc<std::cell::RefCell<Option<gtk::Popover>>>,
) {
    // Toggle: a second ?/F1 while the overlay is up closes it.
    let existing = slot.borrow().clone();
    if let Some(p) = existing {
        p.popdown();
        return;
    }
    let rows: [(&str, String); 13] = [
        ("Open", accel_display(&kb.primary_action)),
        ("Secondary action", accel_display(&kb.secondary_action)),
        ("Quick Look", accel_display(&kb.quick_look)),
        ("Quick Look (from entry)", accel_display(&kb.quick_look_alt)),
        ("Copy", accel_display(&kb.copy)),
        ("Next result", accel_display(&kb.next_result)),
        ("Previous result", accel_display(&kb.previous_result)),
        ("Next category", accel_display(&kb.next_category)),
        ("Previous category", accel_display(&kb.previous_category)),
        ("Filter: all", accel_display(&kb.filter_all)),
        ("History", accel_display(&kb.history_up)),
        ("Reset position", accel_display(&kb.reset_gui_position)),
        ("Close", accel_display(&kb.close)),
    ];
    let grid = gtk::Grid::new();
    grid.set_row_spacing(4);
    grid.set_column_spacing(24);
    grid.set_margin_top(10);
    grid.set_margin_bottom(10);
    grid.set_margin_start(14);
    grid.set_margin_end(14);
    for (i, (name, accel)) in rows.iter().enumerate() {
        let name_label = gtk::Label::new(Some(name));
        name_label.set_halign(gtk::Align::Start);
        add_css_class(&name_label, "lixun-subtitle");
        let accel_label = gtk::Label::new(Some(accel));
        accel_label.set_halign(gtk::Align::End);
        add_css_class(&accel_label, "lixun-shortcut-accel");
        grid.attach(&name_label, 0, i as i32, 1, 1);
        grid.attach(&accel_label, 1, i as i32, 1, 1);
    }
    let popover = gtk::Popover::new();
    popover.set_child(Some(&grid));
    popover.set_parent(entry);
    // No grab — see the docstring. The keymap dismisses on any key.
    popover.set_autohide(false);
    add_css_class(&popover, "lixun-shortcuts");
    let rect = gtk::gdk::Rectangle::new(0, entry.height(), 1, 1);
    popover.set_pointing_to(Some(&rect));
    // On close: release the keymap's slot, re-assert entry focus, and
    // unparent on idle so repeated openings do not accumulate
    // orphaned popovers under the entry.
    let slot_for_closed = std::rc::Rc::clone(slot);
    let entry_for_closed = entry.clone();
    popover.connect_closed(move |p| {
        slot_for_closed.borrow_mut().take();
        entry_for_closed.grab_focus();
        let p = p.clone();
        glib::idle_add_local_once(move || p.unparent());
    });
    *slot.borrow_mut() = Some(popover.clone());
    popover.popup();
}

/// Resolved path of the user config the settings-menu toggles edit.
fn user_config_path() -> std::path::PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("~/.config"))
        .join("lixun/config.toml")
}

/// Read + parse the user config for the settings-menu toggles. A
/// missing file means "all defaults" and yields an empty document —
/// the old code silently no-oped there, so first-run users could
/// never toggle anything. Read/parse failures surface on the status
/// bar and return `None`.
fn load_config_document(status_bar: &StatusBar) -> Option<toml_edit::DocumentMut> {
    let config_path = user_config_path();
    match std::fs::read_to_string(&config_path) {
        Ok(content) => match content.parse::<toml_edit::DocumentMut>() {
            Ok(doc) => Some(doc),
            Err(e) => {
                status_bar.show_error(&format!("Couldn't parse config.toml: {}", e));
                None
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Some(toml_edit::DocumentMut::new())
        }
        Err(e) => {
            status_bar.show_error(&format!("Couldn't read config.toml: {}", e));
            None
        }
    }
}

/// Write the edited config back; failures surface on the status bar
/// (a silent `let _ =` made the toggle look successful while the
/// daemon restarted into the old config). Returns success.
fn write_config_document(doc: &toml_edit::DocumentMut, status_bar: &StatusBar) -> bool {
    let config_path = user_config_path();
    if let Some(parent) = config_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&config_path, doc.to_string()) {
        status_bar.show_error(&format!("Couldn't write config.toml: {}", e));
        return false;
    }
    true
}

/// Build and pop the settings menu under the entry's logo. Shared by
/// the logo-area right-click gesture and the logo icon press so both
/// affordances show the identical menu. Toggle state is spelled out
/// as text ("On"/"Off") — a colour-only indicator is invisible to
/// colourblind users.
fn popup_settings_menu(
    entry: &gtk::Entry,
    semantic_live: &SemanticUiState,
    semantic_configured: bool,
    ocr_enabled: bool,
) {
    let menu = gtk::PopoverMenu::from_model(None::<&gtk::gio::MenuModel>);
    let menu_model = gtk::gio::Menu::new();

    menu_model.append(Some("Relaunch Daemon"), Some("app.relaunch"));

    // Label from LIVE daemon status when available (F6): config-
    // section presence used to claim "On" even when the operator
    // wrote `enabled = false` or the worker crashed. Fall back to
    // the config-derived guess only while no Status reply has
    // arrived yet.
    let semantic_label = match &*semantic_live.borrow() {
        Some((true, _, true)) => "Semantic Search: On".to_string(),
        Some((true, state, false)) => format!("Semantic Search: On ({state})"),
        Some((false, _, _)) => "Semantic Search: Off".to_string(),
        None if semantic_configured => "Semantic Search: On".to_string(),
        None => "Semantic Search: Off".to_string(),
    };
    menu_model.append(Some(&semantic_label), Some("app.toggle-semantic"));

    let ocr_label = if ocr_enabled { "OCR: On" } else { "OCR: Off" };
    menu_model.append(Some(ocr_label), Some("app.toggle-ocr"));

    menu_model.append(Some("Open Config"), Some("app.open-config"));

    menu.set_menu_model(Some(&menu_model));
    menu.set_parent(entry);
    let rect = gtk::gdk::Rectangle::new(0, entry.height(), 1, 1);
    menu.set_pointing_to(Some(&rect));
    menu.popup();
}

pub(crate) fn build_window(app: &gtk::Application) -> Result<()> {
    let session_epoch = Arc::new(AtomicU64::new(0));
    let (ipc, ipc_event_rx) = start_ipc_thread(Arc::clone(&session_epoch));
    let daemon_config = lixun_config::Config::load()?;

    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .default_width(720)
        .decorated(false)
        .build();
    window.set_widget_name("lixun-root");

    window.init_layer_shell();
    // Stable layer-shell namespace so compositors can target the
    // launcher surface with layer rules (e.g. Hyprland
    //   layerrule = blur, ^(lixun-gui)$
    // ). Must be set after init_layer_shell and before the surface
    // is realized; gtk4-layer-shell 0.8 takes Option<&str>.
    window.set_namespace(Some("lixun-gui"));
    // Overlay keeps the launcher above ordinary toplevels; fcitx5
    // popups resolve above by the standard above-overlay rule. NOTE:
    // the overlay layer also stacks above EVERY xdg-toplevel — so a
    // WM-managed preview window (`[gui] preview_placement =
    // "window"`, the default) renders UNDER this surface wherever
    // the two overlap. That is the documented tradeoff of the
    // draggable preview mode; the launcher slides toward the left
    // edge during preview mode to minimise the overlap, and
    // `preview_placement = "overlay"` puts the preview on this same
    // layer (anchored to the RIGHT edge) for a deterministic
    // side-by-side layout instead (see `slide_for_preview`).
    //
    // xdg-foreign-v2 transient parenting is intentionally NOT wired:
    // the protocol restricts zxdg_exporter_v2.export_toplevel to
    // xdg_toplevel surfaces, and wlroots/Mutter/KWin reject layer_surface
    // with invalid_surface. Parenting is cosmetic (window-switcher
    // grouping) and moot now that both surfaces share the Overlay layer.
    // Request::PreviewSetParent remains in the IPC for a future
    // xdg_toplevel launcher mode.
    window.set_layer(gtk4_layer_shell::Layer::Overlay);
    // Anchor only Top. Leaving Left and Right unanchored lets the
    // layer-shell compositor center the window horizontally on the
    // monitor — anchoring both edges would stretch the surface to
    // the full screen width, which we explicitly do not want here.
    // Vertical position is pinned by the top margin.
    window.set_anchor(Edge::Top, true);
    window.set_keyboard_mode(gtk4_layer_shell::KeyboardMode::OnDemand);

    // Restore the per-monitor saved position through the same
    // validation path used on every show(), so a position saved on a
    // now-disconnected monitor can never pin the surface off the
    // currently connected screen at cold start.
    if let Some(monitor) = pick_current_monitor() {
        window.set_monitor(Some(&monitor));
        apply_validated_placement(&window, &monitor);
    } else {
        window.set_margin(Edge::Top, DEFAULT_TOP_MARGIN);
    }
    add_css_class(&window, "lixun-window");

    // Apply the user's system-wide colour-scheme preference before the
    // window is mapped, so the first frame already carries the right
    // skin. Portal absence or a missing key is non-fatal: we keep the
    // historical dark skin in that case. The listener that tracks
    // live scheme changes is installed further down, once the entry
    // exists — it also swaps the entry's logo variant.
    let initial_scheme_is_light = crate::color_scheme::read_initial().is_light();
    if initial_scheme_is_light {
        window.add_css_class("lixun-light");
    }
    let color_scheme_rx = crate::color_scheme::spawn_listener();

    let blur = crate::kde_blur::BlurController::new(&window, daemon_config.gui.blur);

    let display = gtk::gdk::Display::default()
        .ok_or_else(|| anyhow::anyhow!("No GDK display available; running headless?"))?;

    // Resolve window WIDTH as a percentage of the primary monitor.
    // Height is deliberately NOT pinned here: layer-shell surface
    // anchored only on Top sizes to content, which is the whole
    // point of the Spotlight-style empty-query collapse (G0.2 in
    // gui-ux-v1). Any set_size_request / set_default_size with a
    // height arg forces a minimum surface height and defeats the
    // collapse — exactly the regression fixed here after commit
    // 708cb69 had introduced it via monitor-relative sizing.
    //
    // The config's height_percent / max_height_px are instead
    // applied to the inner ScrolledWindow as
    // max_content_height (see below), so the results list has a
    // vertical cap without pinning the outer surface.
    let gui_max_content_height: i32;
    if let Some(monitor) = display
        .monitors()
        .item(0)
        .and_downcast::<gtk::gdk::Monitor>()
    {
        let geom = monitor.geometry();
        // Compute in f64 and round: `i32 * percent / 100` truncates,
        // which can undershoot the requested fraction by a pixel.
        // Result is clamped to the configured max_*_px caps as before.
        let percent_of = |dim: i32, percent: u8| -> i32 {
            (f64::from(dim) * f64::from(percent) / 100.0).round() as i32
        };
        let w = percent_of(geom.width(), daemon_config.gui.width_percent)
            .min(daemon_config.gui.max_width_px);
        let h = percent_of(geom.height(), daemon_config.gui.height_percent)
            .min(daemon_config.gui.max_height_px);
        window.set_default_size(w, -1);
        gui_max_content_height = h;
    } else {
        gui_max_content_height = daemon_config.gui.max_height_px;
    }

    let style_manager = crate::style_manager::StyleManager::install(
        &display,
        daemon_config.gui.theme.as_deref(),
        daemon_config.gui.matugen.enabled,
        daemon_config.gui.matugen.colors_path.clone(),
        daemon_config.gui.opacity,
    );

    // Spawn the live-reload pipeline: a notify watcher posts
    // ConfigChanged / UserCssChanged / ThemeCssChanged events to a
    // glib-friendly async_channel. The pump below reapplies the
    // appropriate provider (or toggles the blur controller) on the
    // GTK main thread. When the active theme path changes the
    // watcher is rebuilt so the new theme directory is observed.
    let config_path = dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join("lixun/config.toml");
    let (style_tx, style_rx) = async_channel::unbounded::<crate::style_watcher::StyleEvent>();
    let initial_theme_css = style_manager
        .resolver
        .active_css_path(daemon_config.gui.theme.as_deref());
    let initial_colors_css = daemon_config.gui.matugen.colors_path.clone();
    let initial_watcher = crate::style_watcher::spawn(
        config_path.clone(),
        style_manager.resolver.user_override(),
        initial_colors_css.clone(),
        initial_theme_css.clone(),
        style_tx.clone(),
    );
    match initial_watcher {
        Ok(watcher) => {
            let mut current_theme_css = initial_theme_css;
            let mut current_colors_css = initial_colors_css;
            let mut current_watcher = watcher;
            let style_manager = style_manager;
            let blur = blur;
            let style_tx_pump = style_tx.clone();
            let config_path_pump = config_path.clone();
            glib::MainContext::default().spawn_local(async move {
                while let Ok(event) = style_rx.recv().await {
                    use crate::style_watcher::StyleEvent;
                    match event {
                        StyleEvent::ConfigChanged => match lixun_config::Config::load() {
                            Ok(cfg) => {
                                let theme = cfg.gui.theme.as_deref();
                                style_manager.apply_theme(theme);
                                style_manager.set_surface_opacity(cfg.gui.opacity);
                                blur.set_mode(cfg.gui.blur);
                                let new_theme_css = style_manager.resolver.active_css_path(theme);
                                let new_colors_css = cfg.gui.matugen.colors_path.clone();
                                if new_theme_css != current_theme_css
                                    || new_colors_css != current_colors_css
                                {
                                    match crate::style_watcher::spawn(
                                        config_path_pump.clone(),
                                        style_manager.resolver.user_override(),
                                        new_colors_css.clone(),
                                        new_theme_css.clone(),
                                        style_tx_pump.clone(),
                                    ) {
                                        Ok(w) => {
                                            current_watcher = w;
                                            current_theme_css = new_theme_css;
                                            current_colors_css = new_colors_css;
                                        }
                                        Err(e) => tracing::warn!(
                                            error = %e,
                                            "failed to rebuild style watcher after path change",
                                        ),
                                    }
                                }
                            }
                            Err(e) => tracing::warn!(
                                error = %e,
                                "failed to reload config after change",
                            ),
                        },
                        StyleEvent::ColorsCssChanged => {
                            style_manager.reload_colors_css();
                        }
                        StyleEvent::ThemeCssChanged => {
                            // Reload the active theme by re-resolving from current config.
                            if let Ok(cfg) = lixun_config::Config::load() {
                                style_manager.apply_theme(cfg.gui.theme.as_deref());
                            }
                        }
                        StyleEvent::UserCssChanged => {
                            style_manager.reload_user_css();
                        }
                    }
                }
                // Receiver closed: drop the watcher explicitly so the
                // backing notify thread tears down before this task ends.
                drop(current_watcher);
            });
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to start style watcher; live reload disabled");
            // Without a watcher the StyleManager and BlurController still
            // need to stay alive for the lifetime of the window. Leak them
            // into the application's MainContext by holding them in a
            // never-completing task.
            let style_manager = style_manager;
            let blur = blur;
            glib::MainContext::default().spawn_local(async move {
                std::future::pending::<()>().await;
                drop(style_manager);
                drop(blur);
            });
        }
    }

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 6);
    vbox.set_margin_start(16);
    vbox.set_margin_end(16);
    vbox.set_margin_top(12);
    vbox.set_margin_bottom(2);

    let entry = gtk::Entry::builder()
        .placeholder_text("Search\u{2026}")
        .hexpand(true)
        .build();
    entry.set_widget_name("lixun-entry");
    add_css_class(&entry, "lixun-entry");
    // A1: name the search field for assistive tech — the composite
    // GtkEntry exposes no label of its own.
    entry.update_property(&[gtk::accessible::Property::Label("Search")]);

    let logo_light = gtk::gdk::Texture::from_bytes(&glib::Bytes::from_static(LOGO_LIGHT_SVG))
        .inspect_err(|e| tracing::warn!("failed to decode embedded light logo: {}", e))
        .ok();
    let logo_dark = gtk::gdk::Texture::from_bytes(&glib::Bytes::from_static(LOGO_DARK_SVG))
        .inspect_err(|e| tracing::warn!("failed to decode embedded dark logo: {}", e))
        .ok();
    let initial_logo = if initial_scheme_is_light {
        &logo_dark
    } else {
        &logo_light
    };
    if let Some(icon) = initial_logo {
        entry.set_icon_from_paintable(gtk::EntryIconPosition::Primary, Some(icon));
        entry.set_icon_activatable(gtk::EntryIconPosition::Primary, true);
    }

    // Track live scheme changes: skin class on the window, matching
    // logo variant on the entry (light logo on the dark skin and
    // vice versa).
    {
        let window_for_scheme = window.clone();
        let entry_for_scheme = entry.clone();
        let logo_light = logo_light.clone();
        let logo_dark = logo_dark.clone();
        glib::MainContext::default().spawn_local(async move {
            while let Ok(scheme) = color_scheme_rx.recv().await {
                if scheme.is_light() {
                    window_for_scheme.add_css_class("lixun-light");
                    if let Some(logo) = logo_dark.as_ref() {
                        entry_for_scheme
                            .set_icon_from_paintable(gtk::EntryIconPosition::Primary, Some(logo));
                    }
                } else {
                    window_for_scheme.remove_css_class("lixun-light");
                    if let Some(logo) = logo_light.as_ref() {
                        entry_for_scheme
                            .set_icon_from_paintable(gtk::EntryIconPosition::Primary, Some(logo));
                    }
                }
            }
        });
    }

    // "Configured" is a config-file guess used only until the first
    // live Status reply lands in `semantic_ui` (F6): section presence
    // alone claimed On even for `enabled = false` or a dead worker.
    let semantic_configured = daemon_config
        .plugin_sections
        .get("semantic")
        .and_then(|v| v.get("enabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let semantic_ui: SemanticUiState = std::rc::Rc::new(std::cell::RefCell::new(None));
    let ocr_enabled = daemon_config.ocr.enabled;
    let max_results = daemon_config.max_results;

    // The settings menu opens from two affordances: right-click over
    // the logo area (x < 60) and a plain click on the logo icon
    // itself (the icon is activatable above).
    let entry_for_menu = entry.clone();
    let semantic_ui_for_gesture = std::rc::Rc::clone(&semantic_ui);
    let gesture = gtk::GestureClick::new();
    gesture.set_button(3);
    gesture.connect_pressed(move |_gesture, _n_press, x, _y| {
        if x < 60.0 {
            popup_settings_menu(
                &entry_for_menu,
                &semantic_ui_for_gesture,
                semantic_configured,
                ocr_enabled,
            );
        }
    });
    entry.add_controller(gesture);

    let semantic_ui_for_icon = std::rc::Rc::clone(&semantic_ui);
    entry.connect_icon_press(move |entry, pos| {
        if pos == gtk::EntryIconPosition::Primary {
            popup_settings_menu(entry, &semantic_ui_for_icon, semantic_configured, ocr_enabled);
        }
    });

    vbox.append(&entry);

    let current_category: CategoryFilter = std::rc::Rc::new(std::cell::Cell::new(None));
    let chips = build_category_chips(&current_category, &daemon_config.keybindings);
    chips.container.set_visible(false);
    vbox.append(&chips.container);

    // ScrolledWindow size policy.
    //
    // This is the canonical GTK4 recipe for a Spotlight-style
    // collapsing list (verified against Walker, Sherlock, Ironbar
    // launchers and confirmed from gtkscrolledwindow.c measure
    // impl):
    //
    //   propagate_natural_height(true) — child's natural height
    //       feeds the ScrolledWindow's natural request (without
    //       this, max_content_height is silently ignored; see
    //       CLAMP in gtkscrolledwindow.c vfunc_measure).
    //
    //   min_content_height(0) — natural/min height can collapse
    //       all the way to 0 when the ListView has no rows, so a
    //       `set_visible(false)` on the empty-query state actually
    //       zeroes the surface height instead of leaving a
    //       scrollbar-sized gap.
    //
    //   max_content_height(gui_max_content_height) — caps the
    //       surface from above using the value we used to pass to
    //       the window's set_size_request. vexpand + this cap
    //       compose as min(available, cap, natural), so the list
    //       grows with hits up to the cap, then starts scrolling.
    //
    //   vexpand(true) — works correctly now because the cap above
    //       bounds the request. (Earlier builds had vexpand(false)
    //       as a workaround because max-cap was missing, which in
    //       turn masked a separate set_size_request bug.)
    //
    // Without all four, either the collapse breaks (no propagate
    // or non-zero min) or the window stretches past max_height_px
    // (no cap).
    let scrolled = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .propagate_natural_height(true)
        .min_content_height(0)
        .max_content_height(gui_max_content_height)
        .build();
    scrolled.set_widget_name("lixun-results-scroll");
    add_css_class(&scrolled, "lixun-results");
    scrolled.set_visible(false);
    vbox.append(&scrolled);

    let model = gtk::StringList::new(&[]);

    let filter = gtk::CustomFilter::new({
        let current = std::rc::Rc::clone(&current_category);
        move |obj| {
            let Some(filter_cat) = current.get() else {
                return true;
            };
            let Some(str_obj) = obj.downcast_ref::<gtk::StringObject>() else {
                return true;
            };
            let doc_id = str_obj.string().to_string();
            with_cached_hits(|hits| {
                hits.iter()
                    .find(|h| h.id.0 == doc_id)
                    .map(|h| h.category == filter_cat)
                    .unwrap_or(true)
            })
        }
    });

    let filter_model = gtk::FilterListModel::new(Some(model.clone()), Some(filter.clone()));

    let selection = gtk::SingleSelection::builder()
        .model(&filter_model)
        .autoselect(true)
        .build();

    // Built before the factory: the row action handlers surface
    // launch failures through the status bar (they used to vanish
    // into the log).
    let status_bar = std::rc::Rc::new(StatusBar::new());

    let list_view = gtk::ListView::builder()
        .model(&selection)
        .factory(&create_list_factory(
            entry.clone(),
            std::rc::Rc::clone(&status_bar),
        ))
        .build();
    list_view.set_widget_name("lixun-results");
    // A1: name the results surface for assistive tech.
    list_view.update_property(&[gtk::accessible::Property::Label("Search results")]);
    scrolled.set_child(Some(&list_view));

    chips.wire_toggle({
        let filter = filter.clone();
        move || filter.changed(gtk::FilterChange::Different)
    });

    vbox.append(status_bar.widget());

    window.set_child(Some(&vbox));

    let chips_rc = std::rc::Rc::new(chips);
    let pending_debounce: std::rc::Rc<std::cell::RefCell<Option<glib::SourceId>>> =
        std::rc::Rc::new(std::cell::RefCell::new(None));
    let loading_timer: std::rc::Rc<std::cell::RefCell<Option<glib::SourceId>>> =
        std::rc::Rc::new(std::cell::RefCell::new(None));
    let claimed_prefixes: std::rc::Rc<Vec<String>> = std::rc::Rc::new(fetch_claimed_prefixes());
    tracing::info!("gui: fetched claimed_prefixes={:?}", claimed_prefixes);

    // Startup daemon-status fetch (off the main thread): primes the
    // live semantic state for the settings menu (F6) and surfaces a
    // daemon-side config parse error that would otherwise stay
    // journal-only (O2 — the daemon now survives a bad config on
    // built-in defaults and reports the error via Status).
    {
        let semantic_ui = std::rc::Rc::clone(&semantic_ui);
        let status_for_startup = std::rc::Rc::clone(&status_bar);
        crate::ipc::fetch_daemon_status_async(move |snap| {
            if let Some(snap) = snap {
                *semantic_ui.borrow_mut() = snap.semantic.clone();
                if let Some(err) = snap.config_error {
                    status_for_startup
                        .show_error(&format!("Config error \u{2014} using defaults: {err}"));
                }
            }
        });
    }

    // Rotating idle placeholder (O3): cycle discoverability hints
    // through the entry's placeholder while the query is empty. The
    // placeholder only paints on an empty entry, so rotation while
    // text is present is invisible by construction; we still skip
    // the churn in that case.
    {
        let hints = build_placeholder_hints(&claimed_prefixes);
        let entry_for_hints = entry.clone();
        let idx = std::rc::Rc::new(std::cell::Cell::new(0usize));
        glib::timeout_add_seconds_local(4, move || {
            if entry_for_hints.text().is_empty() {
                idx.set((idx.get() + 1) % hints.len());
                entry_for_hints.set_placeholder_text(Some(&hints[idx.get()]));
            }
            glib::ControlFlow::Continue
        });
    }

    // A1: announce the SETTLED selection to screen readers. Focus
    // stays parked on the entry while arrows move the selection, so
    // Orca never hears the cursor move; announce() (v4_16 API) fills
    // that gap. Debounced 150 ms so holding an arrow key announces
    // the landing row, not every intermediate one.
    {
        let announce_debounce: std::rc::Rc<std::cell::RefCell<Option<glib::SourceId>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let list_view_for_announce = list_view.clone();
        selection.connect_selected_notify(move |sel| {
            let idx = sel.selected();
            if idx == gtk::INVALID_LIST_POSITION {
                return;
            }
            let Some(doc_id) = sel
                .item(idx)
                .and_then(|o| o.downcast::<gtk::StringObject>().ok())
                .map(|s| s.string().to_string())
            else {
                return;
            };
            if let Some(id) = announce_debounce.borrow_mut().take() {
                id.remove();
            }
            let list_view = list_view_for_announce.clone();
            let slot = std::rc::Rc::clone(&announce_debounce);
            let id = glib::timeout_add_local_once(Duration::from_millis(150), move || {
                *slot.borrow_mut() = None;
                crate::factory::with_cached_hits(|hits| {
                    if let Some(hit) = hits.iter().find(|h| h.id.0 == doc_id) {
                        gtk::prelude::AccessibleExt::announce(
                            &list_view,
                            &hit.title,
                            gtk::AccessibleAnnouncementPriority::Medium,
                        );
                    }
                });
            });
            *announce_debounce.borrow_mut() = Some(id);
        });
    }
    let last_query: std::rc::Rc<std::cell::RefCell<String>> =
        std::rc::Rc::new(std::cell::RefCell::new(String::new()));
    let just_showed_until: std::rc::Rc<std::cell::Cell<Instant>> =
        std::rc::Rc::new(std::cell::Cell::new(Instant::now()));

    let cached_session: std::rc::Rc<std::cell::RefCell<Option<SessionSnapshot>>> =
        std::rc::Rc::new(std::cell::RefCell::new(None));
    let is_restoring: std::rc::Rc<std::cell::Cell<bool>> =
        std::rc::Rc::new(std::cell::Cell::new(false));
    let user_selected_override: std::rc::Rc<std::cell::Cell<bool>> =
        std::rc::Rc::new(std::cell::Cell::new(false));
    let searching_indicator: std::rc::Rc<std::cell::Cell<bool>> =
        std::rc::Rc::new(std::cell::Cell::new(false));
    let preview_mode_active: std::rc::Rc<std::cell::Cell<bool>> =
        std::rc::Rc::new(std::cell::Cell::new(false));
    let preview_debounce: std::rc::Rc<std::cell::RefCell<Option<glib::SourceId>>> =
        std::rc::Rc::new(std::cell::RefCell::new(None));

    // Resolve the preview-mode slide target once: overlay placement
    // needs runtime layer-shell support (the preview process makes
    // the same `is_supported` check in its own build path, so the
    // two agree); everything else — including a configured
    // "overlay" on a compositor without layer-shell — behaves as
    // the WM-managed default.
    let slide_mode = if daemon_config.gui.preview_placement
        == lixun_config::PreviewPlacement::Overlay
        && gtk4_layer_shell::is_supported()
    {
        crate::preview_layout::SlideMode::OverlayColumn
    } else {
        crate::preview_layout::SlideMode::WindowManaged
    };

    let controller = std::rc::Rc::new(LauncherController {
        window: window.clone(),
        entry: entry.clone(),
        chips: std::rc::Rc::clone(&chips_rc),
        selection: selection.clone(),
        list_view: list_view.clone(),
        scrolled: scrolled.clone(),
        status: std::rc::Rc::clone(&status_bar),
        model: model.clone(),
        current_category: std::rc::Rc::clone(&current_category),
        pending_debounce: std::rc::Rc::clone(&pending_debounce),
        last_query: std::rc::Rc::clone(&last_query),
        session_epoch: Arc::clone(&session_epoch),
        just_showed_until: std::rc::Rc::clone(&just_showed_until),
        filter: filter.clone(),
        cached_session: std::rc::Rc::clone(&cached_session),
        show_recents_enabled: daemon_config.gui.show_recents,
        hint_line: build_hint_line(&daemon_config.keybindings),
        ipc: ipc.clone(),
        is_restoring: std::rc::Rc::clone(&is_restoring),
        user_selected_override: std::rc::Rc::clone(&user_selected_override),
        searching_indicator: std::rc::Rc::clone(&searching_indicator),
        preview_mode_active: std::rc::Rc::clone(&preview_mode_active),
        preview_debounce: std::rc::Rc::clone(&preview_debounce),
        preview_slide_saved: std::cell::RefCell::new(None),
        preview_width_percent: daemon_config.gui.preview_width_percent,
        preview_max_width_px: daemon_config.gui.preview_max_width_px,
        slide_mode,
    });

    let close_action = gio::SimpleAction::new("close-launcher", None);
    let controller_for_close = std::rc::Rc::clone(&controller);
    close_action.connect_activate(move |_, _| {
        controller_for_close.hide();
    });
    app.add_action(&close_action);

    {
        let controller_for_sel = std::rc::Rc::clone(&controller);
        let window_for_sel = window.clone();
        selection.connect_selected_notify(move |sel| {
            if !controller_for_sel.preview_mode_active() {
                return;
            }
            let idx = sel.selected();
            if idx == gtk::INVALID_LIST_POSITION {
                return;
            }
            let Some(obj) = sel.item(idx) else { return };
            let Some(str_obj) = obj.downcast_ref::<gtk::StringObject>() else {
                return;
            };
            let doc_id = str_obj.string().to_string();

            controller_for_sel.cancel_preview_debounce();
            let controller_inner = std::rc::Rc::clone(&controller_for_sel);
            let window_inner = window_for_sel.clone();
            let id = glib::timeout_add_local_once(Duration::from_millis(50), move || {
                *controller_inner.preview_debounce.borrow_mut() = None;
                if !controller_inner.preview_mode_active() {
                    return;
                }
                let monitor = crate::ipc::current_monitor_connector(&window_inner);
                crate::factory::with_cached_hits(|hits| {
                    if let Some(hit) = hits.iter().find(|h| h.id.0 == doc_id) {
                        crate::ipc::send_preview_request(hit, monitor.clone());
                    }
                });
                // Keep keyboard focus in the search entry so the
                // next arrow press is delivered to the launcher,
                // not stolen by the preview surface the compositor
                // may have just focused.
                controller_inner.entry.grab_focus();
            });
            *controller_for_sel.preview_debounce.borrow_mut() = Some(id);
        });
    }

    let clear_action = gio::SimpleAction::new("clear-and-hide-launcher", None);
    let controller_for_clear = std::rc::Rc::clone(&controller);
    clear_action.connect_activate(move |_, _| {
        controller_for_clear.clear_and_hide();
    });
    app.add_action(&clear_action);

    let relaunch_action = gio::SimpleAction::new("relaunch", None);
    relaunch_action.connect_activate(move |_, _| {
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "restart", "lixund.service"])
            .spawn();
    });
    app.add_action(&relaunch_action);

    let toggle_semantic_action = gio::SimpleAction::new("toggle-semantic", None);
    let semantic_current = semantic_configured;
    let status_for_semantic = std::rc::Rc::clone(&status_bar);
    toggle_semantic_action.connect_activate(move |_, _| {
        let Some(mut doc) = load_config_document(&status_for_semantic) else {
            return;
        };
        if semantic_current {
            doc.remove("semantic");
        } else {
            let mut table = toml_edit::Table::new();
            table.insert("enabled", toml_edit::value(true));
            doc.insert("semantic", toml_edit::Item::Table(table));
        }
        if !write_config_document(&doc, &status_for_semantic) {
            return;
        }

        let _ = std::process::Command::new("systemctl")
            .args(["--user", "restart", "lixund.service"])
            .spawn();
    });
    app.add_action(&toggle_semantic_action);

    let toggle_ocr_action = gio::SimpleAction::new("toggle-ocr", None);
    let ocr_current = daemon_config.ocr.enabled;
    let status_for_ocr = std::rc::Rc::clone(&status_bar);
    toggle_ocr_action.connect_activate(move |_, _| {
        let Some(mut doc) = load_config_document(&status_for_ocr) else {
            return;
        };
        if let Some(ocr_table) = doc.get_mut("ocr").and_then(|v| v.as_table_mut()) {
            ocr_table.insert("enabled", toml_edit::value(!ocr_current));
        } else {
            let mut table = toml_edit::Table::new();
            table.insert("enabled", toml_edit::value(!ocr_current));
            doc.insert("ocr", toml_edit::Item::Table(table));
        }
        if !write_config_document(&doc, &status_for_ocr) {
            return;
        }

        let _ = std::process::Command::new("systemctl")
            .args(["--user", "restart", "lixund.service"])
            .spawn();
    });
    app.add_action(&toggle_ocr_action);

    let open_config_action = gio::SimpleAction::new("open-config", None);
    open_config_action.connect_activate(move |_, _| {
        let config_path = user_config_path();
        // xdg-utils' xdg-open rejects any argv that begins with '-' including
        // the conventional GNU end-of-options separator. See the docstring on
        // crates/lixun-gui/src/actions.rs::xdg_open_command for the protocol
        // detail. Pass the target path directly.
        let _ = std::process::Command::new("xdg-open")
            .arg(&config_path)
            .spawn();
    });
    app.add_action(&open_config_action);

    install_response_handler(
        ipc_event_rx,
        Arc::clone(&session_epoch),
        model.clone(),
        filter.clone(),
        selection.clone(),
        list_view.clone(),
        chips_rc.container.clone(),
        scrolled.clone(),
        std::rc::Rc::clone(&status_bar),
        std::rc::Rc::clone(&last_query),
        std::rc::Rc::clone(&user_selected_override),
        std::rc::Rc::clone(&searching_indicator),
        std::rc::Rc::clone(&loading_timer),
        build_hint_line(&daemon_config.keybindings),
        std::rc::Rc::clone(&semantic_ui),
    );

    // Drain the icon-loader's ready channel on the GTK main loop.
    // When the off-thread worker finishes loading an absolute-path
    // texture, it emits the IconKey here; we provoke a filter
    // re-evaluation so the ListView re-binds visible rows and
    // picks up the freshly-cached texture without requiring the
    // user to scroll or re-type. Same async_channel boundary as
    // start_ipc_thread; no tokio.
    {
        let icon_ready_rx = crate::icons::icon_ready_rx();
        let filter_for_icons = filter.clone();
        glib::spawn_future_local(async move {
            while let Ok(_key) = icon_ready_rx.recv().await {
                filter_for_icons.changed(gtk::FilterChange::Different);
            }
        });
    }

    // Drain the reaper's launch-failure channel on the GTK main loop.
    // A helper process that execs fine and dies later fails after the
    // launcher has hidden, so the status bar cannot carry the
    // message — raise a desktop notification instead.
    {
        let failure_rx = crate::reaper::failure_rx();
        let app_for_notify = app.clone();
        glib::spawn_future_local(async move {
            while let Ok(failure) = failure_rx.recv().await {
                let body = match failure.code {
                    Some(code) => format!(
                        "Couldn't open {} — exited with code {}",
                        failure.program, code
                    ),
                    None => format!("Couldn't open {} — killed by a signal", failure.program),
                };
                let notification = gio::Notification::new("Lixun");
                notification.set_body(Some(&body));
                app_for_notify.send_notification(None, &notification);
            }
        });
    }

    let controller_for_empty = std::rc::Rc::clone(&controller);
    install_entry_handler(
        &entry,
        ipc.clone(),
        model.clone(),
        selection.clone(),
        chips_rc.container.clone(),
        scrolled.clone(),
        std::rc::Rc::clone(&status_bar),
        std::rc::Rc::clone(&last_query),
        std::rc::Rc::clone(&pending_debounce),
        Arc::clone(&session_epoch),
        std::rc::Rc::clone(&is_restoring),
        std::rc::Rc::clone(&user_selected_override),
        std::rc::Rc::clone(&searching_indicator),
        std::rc::Rc::clone(&loading_timer),
        std::rc::Rc::clone(&claimed_prefixes),
        max_results,
        std::rc::Rc::new(move || controller_for_empty.maybe_show_recents()),
    );

    crate::keymap::install_keyboard_handler(
        &window,
        &list_view,
        &entry,
        &selection,
        &filter_model,
        &model,
        std::rc::Rc::clone(&chips_rc),
        std::rc::Rc::clone(&status_bar),
        &scrolled,
        &chips_rc.container,
        ipc.clone(),
        daemon_config.keybindings.clone(),
        std::rc::Rc::clone(&controller),
    );

    let focus_ctrl = gtk::EventControllerFocus::new();
    let entry_for_focus_enter = entry.clone();
    focus_ctrl.connect_enter(move |_| {
        tracing::info!("gui: focus_ctrl ENTER, calling entry.grab_focus()");
        entry_for_focus_enter.grab_focus();
    });
    let controller_for_leave = std::rc::Rc::clone(&controller);
    let just_showed_for_leave = std::rc::Rc::clone(&just_showed_until);
    focus_ctrl.connect_leave(move |_| {
        tracing::info!("gui: focus_ctrl LEAVE fired, just_showed_until check");
        if Instant::now() < just_showed_for_leave.get() {
            tracing::info!("gui: spurious leave during show transition, ignored");
            return;
        }
        // While the preview window is open, the compositor will hand
        // keyboard focus to it (the preview is now a regular
        // xdg-toplevel, not a layer-shell surface, so it can take
        // focus). That LEAVE event is expected and benign — the
        // launcher must stay visible so the user can keep navigating
        // results and see the preview update live. Preview dismissal
        // is driven exclusively by explicit user input handled in
        // keymap.rs (Space / Escape) and by the preview window's own
        // close controllers (X button, launch action). Auto-closing
        // here would defeat the "launcher + preview side-by-side"
        // workflow.
        if controller_for_leave.preview_mode_active() {
            tracing::info!(
                "gui: focus_ctrl LEAVE in preview mode → ignored (preview drives dismissal)"
            );
            return;
        }
        tracing::info!("gui: focus_ctrl LEAVE → controller.hide()");
        controller_for_leave.hide();
    });
    window.add_controller(focus_ctrl);

    install_drag_gesture(&window);

    // In daemon-spawned (service) mode the GUI must start hidden
    // and wait for the daemon's first command. Calling show() here
    // unconditionally would race the post-spawn Toggle that the
    // daemon sends as soon as wait_for_ready resolves: the window
    // is already visible by that point, Toggle inspects
    // is_visible()=true and hides it, and the user has to press
    // Super+Space a second time to get the launcher up. The
    // daemon flags this mode via LIXUN_GUI_SERVICE_SPAWN=1 (see
    // lixun-daemon/src/gui_control.rs spawn()).
    //
    // Standalone launches (`lixun-gui` from a terminal for CSS
    // inspection or dev work, no daemon) have the variable unset,
    // so the old "show myself immediately" behaviour is
    // preserved. README documents GTK_DEBUG=interactive lixun-gui,
    // that still works.
    if std::env::var_os("LIXUN_GUI_SERVICE_SPAWN").is_none() {
        controller.show();
    }

    crate::gui_server::start(std::rc::Rc::clone(&controller))?;

    tracing::info!("Lixun GUI window built");
    Ok(())
}

fn install_drag_gesture(window: &gtk::ApplicationWindow) {
    use gtk::prelude::*;
    use std::cell::Cell;
    use std::rc::Rc;

    let gesture = gtk::GestureDrag::new();
    let base_top = Rc::new(Cell::new(0i32));
    let base_left = Rc::new(Cell::new(0i32));
    let pending_save: Rc<std::cell::RefCell<Option<glib::SourceId>>> =
        Rc::new(std::cell::RefCell::new(None));
    let drag_accepted: Rc<Cell<bool>> = Rc::new(Cell::new(false));

    let window_for_begin = window.clone();
    let base_top_for_begin = Rc::clone(&base_top);
    let base_left_for_begin = Rc::clone(&base_left);
    let drag_accepted_for_begin = Rc::clone(&drag_accepted);
    gesture.connect_drag_begin(move |gesture, _x, _y| {
        // Super-hold gates the drag so plain clicks fall through to
        // the entry, list, and chips. Without this guard, drags on
        // the chrome between widgets would move the launcher and
        // confuse users who expected the click to land on a widget.
        if !gesture
            .current_event_state()
            .contains(gtk::gdk::ModifierType::SUPER_MASK)
        {
            gesture.set_state(gtk::EventSequenceState::Denied);
            return;
        }

        use gtk4_layer_shell::LayerShell;

        // First-drag bootstrap: when no saved position has been
        // restored, Left is unanchored and the compositor centers
        // the window. Anchoring Left mid-drag would make the
        // window snap to left=0 then back — visible jitter. So we
        // freeze the current centered x by computing it from the
        // monitor and window allocation, anchor Left, and seed
        // base_left with that value.
        if !window_for_begin.is_anchor(gtk4_layer_shell::Edge::Left) {
            let alloc_width = window_for_begin.width();
            let monitor_width = gtk::gdk::Display::default()
                .and_then(|d| d.monitors().item(0).and_downcast::<gtk::gdk::Monitor>())
                .map(|m| m.geometry().width())
                .unwrap_or(0);
            let centered_left = ((monitor_width - alloc_width) / 2).max(0);
            window_for_begin.set_margin(gtk4_layer_shell::Edge::Left, centered_left);
            window_for_begin.set_anchor(gtk4_layer_shell::Edge::Left, true);
            base_left_for_begin.set(centered_left);
        } else {
            base_left_for_begin.set(window_for_begin.margin(gtk4_layer_shell::Edge::Left));
        }
        base_top_for_begin.set(window_for_begin.margin(gtk4_layer_shell::Edge::Top));

        // Switch cursor to "grabbing" so the user gets visual
        // feedback that the launcher is being dragged.
        if let Some(cursor) = gtk::gdk::Cursor::from_name("grabbing", None) {
            window_for_begin.set_cursor(Some(&cursor));
        }

        drag_accepted_for_begin.set(true);
    });

    // Jump-on-release: don't move window during drag (eliminates jitter
    // and lag on high-refresh monitors). Just track offset; apply once
    // on drag_end. Standard pattern for Wayland layer-shell drag.
    let drag_offset: Rc<Cell<(f64, f64)>> = Rc::new(Cell::new((0.0, 0.0)));
    let drag_accepted_for_update = Rc::clone(&drag_accepted);
    gesture.connect_drag_update(move |_gesture, offset_x, offset_y| {
        if !drag_accepted_for_update.get() {
            return;
        }
        drag_offset.set((offset_x, offset_y));
    });

    let window_for_end = window.clone();
    let base_top_for_end = Rc::clone(&base_top);
    let base_left_for_end = Rc::clone(&base_left);
    let drag_accepted_for_end = Rc::clone(&drag_accepted);
    gesture.connect_drag_end(move |gesture, offset_x, offset_y| {
        if !drag_accepted_for_end.get() {
            return;
        }
        use gtk4_layer_shell::LayerShell;

        // If Super is still held when the drag ends, restore the
        // "grab" (open hand) affordance so the user knows another
        // drag is available. Otherwise reset to the default cursor.
        let still_super = gesture
            .current_event_state()
            .contains(gtk::gdk::ModifierType::SUPER_MASK);
        let hover_cursor = still_super
            .then(|| gtk::gdk::Cursor::from_name("grab", None))
            .flatten();
        window_for_end.set_cursor(hover_cursor.as_ref());

        let new_top = (base_top_for_end.get() + offset_y as i32).max(0);
        let new_left = (base_left_for_end.get() + offset_x as i32).max(0);
        window_for_end.set_margin(gtk4_layer_shell::Edge::Top, new_top);
        window_for_end.set_margin(gtk4_layer_shell::Edge::Left, new_left);

        if let Some(prev_id) = pending_save.borrow_mut().take() {
            prev_id.remove();
        }

        let window_clone = window_for_end.clone();
        let pending_clone = Rc::clone(&pending_save);
        let source_id =
            glib::timeout_add_local_once(std::time::Duration::from_millis(250), move || {
                use gtk4_layer_shell::LayerShell;
                let top = window_clone.margin(gtk4_layer_shell::Edge::Top);
                let left = window_clone.margin(gtk4_layer_shell::Edge::Left);

                let connector = gtk::gdk::Display::default()
                    .and_then(|d| d.monitors().item(0).and_downcast::<gtk::gdk::Monitor>())
                    .and_then(|m| m.connector())
                    .map(|gs| gs.to_string());

                crate::launcher_position::save(connector.as_deref(), top, left);
                report_launcher_geometry(&window_clone);
                pending_clone.borrow_mut().take();
            });
        *pending_save.borrow_mut() = Some(source_id);

        drag_accepted_for_end.set(false);
    });

    window.add_controller(gesture);

    install_super_drag_cursor(window, drag_accepted);
}

/// Track Super-key press/release so the cursor shows the "grab" (open
/// hand) affordance whenever the modifier is held, even before the
/// user starts dragging. Without this the affordance only appears
/// after the GestureDrag claims the sequence — too late to discover.
///
/// Cursor is left alone while a drag is in progress (drag_accepted is
/// true): connect_drag_begin/end own the cursor for that window of
/// time.
fn install_super_drag_cursor(
    window: &gtk::ApplicationWindow,
    drag_accepted: std::rc::Rc<std::cell::Cell<bool>>,
) {
    use gtk::prelude::*;

    let key_ctrl = gtk::EventControllerKey::new();

    let window_for_press = window.clone();
    let drag_for_press = std::rc::Rc::clone(&drag_accepted);
    key_ctrl.connect_key_pressed(move |_ctrl, key, _code, _state| {
        if matches!(key, gtk::gdk::Key::Super_L | gtk::gdk::Key::Super_R)
            && !drag_for_press.get()
            && let Some(cursor) = gtk::gdk::Cursor::from_name("grab", None)
        {
            window_for_press.set_cursor(Some(&cursor));
        }
        glib::Propagation::Proceed
    });

    let window_for_release = window.clone();
    let drag_for_release = std::rc::Rc::clone(&drag_accepted);
    key_ctrl.connect_key_released(move |_ctrl, key, _code, _state| {
        if matches!(key, gtk::gdk::Key::Super_L | gtk::gdk::Key::Super_R) && !drag_for_release.get()
        {
            window_for_release.set_cursor(None);
        }
    });

    window.add_controller(key_ctrl);
}

/// Read launcher rect (monitor-local logical pixels) and send to daemon.
///
/// Used to inform preview-bin where the launcher sits so it can ask the
/// daemon to unmap us when the preview window's rect overlaps ours on
/// the same monitor (layer-shell Overlay always paints above any
/// xdg-toplevel, so visual stacking can't solve this).
///
/// When the Left edge isn't anchored (default centered launcher), we
/// compute the centered x ourselves from monitor geometry so the rect
/// we report matches the surface the compositor actually places.
pub(crate) fn report_launcher_geometry(window: &gtk::ApplicationWindow) {
    tracing::debug!("gui: report_launcher_geometry called");
    use gtk4_layer_shell::LayerShell;
    let monitor = match pick_current_monitor() {
        Some(m) => m,
        None => {
            tracing::debug!("gui: report_launcher_geometry: no monitor");
            return;
        }
    };
    let connector = match monitor.connector() {
        Some(s) => s.to_string(),
        None => {
            tracing::debug!("gui: report_launcher_geometry: no connector");
            return;
        }
    };
    let top = window.margin(gtk4_layer_shell::Edge::Top);
    let mut w = window.width();
    let mut h = window.height();
    tracing::debug!("gui: report_launcher_geometry: initial w={} h={}", w, h);
    if w <= 0 {
        w = window.default_width();
    }
    if h <= 0 {
        h = window.default_height();
    }
    tracing::debug!("gui: report_launcher_geometry: final w={} h={}", w, h);
    if w <= 0 || h <= 0 {
        tracing::debug!("gui: report_launcher_geometry: zero size, skipping");
        return;
    }
    let x = if window.is_anchor(gtk4_layer_shell::Edge::Left) {
        window.margin(gtk4_layer_shell::Edge::Left)
    } else {
        let mon_w = monitor.geometry().width();
        ((mon_w - w) / 2).max(0)
    };
    tracing::debug!(
        "gui: report_launcher_geometry: sending connector={} x={} top={} w={} h={}",
        connector,
        x,
        top,
        w,
        h
    );
    crate::ipc::send_launcher_geometry(connector, x, top, w, h);
}

#[allow(clippy::too_many_arguments)]
fn install_response_handler(
    event_rx: async_channel::Receiver<crate::ipc::IpcMessage>,
    session_epoch: Arc<AtomicU64>,
    model: gtk::StringList,
    filter: gtk::CustomFilter,
    selection: gtk::SingleSelection,
    list_view: gtk::ListView,
    chips_container: gtk::Box,
    scrolled: gtk::ScrolledWindow,
    status: std::rc::Rc<StatusBar>,
    last_query: std::rc::Rc<std::cell::RefCell<String>>,
    user_selected_override: std::rc::Rc<std::cell::Cell<bool>>,
    searching_indicator: std::rc::Rc<std::cell::Cell<bool>>,
    loading_timer: std::rc::Rc<std::cell::RefCell<Option<glib::SourceId>>>,
    results_hint_line: String,
    semantic_ui: SemanticUiState,
) {
    let pending_hits = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let last_epoch = std::rc::Rc::new(std::cell::Cell::new(0u64));
    // F3: pending render timer for the Initial (BM25-only) chunk. In
    // semantic mode the Final waits on embedding + ANN + fusion;
    // deliberately buffering Initial made keystroke→results latency
    // equal embedding time. If the Final hasn't landed ~100 ms after
    // Initial, render the provisional hits (epoch-guarded); the
    // Final re-renders with the fused ranking when it arrives. Fast
    // lexical queries beat the timer and keep their single rebuild.
    let initial_timer: std::rc::Rc<std::cell::RefCell<Option<glib::SourceId>>> =
        std::rc::Rc::new(std::cell::RefCell::new(None));
    // Zero-hit path: cached daemon Status sample plus a latch that
    // stops fetch threads from stacking up while one round-trip is
    // still in flight (see `present_zero_hit_status`).
    let index_status_cache: std::rc::Rc<std::cell::RefCell<Option<IndexStatusSnapshot>>> =
        std::rc::Rc::new(std::cell::RefCell::new(None));
    let index_status_inflight = std::rc::Rc::new(std::cell::Cell::new(false));

    glib::spawn_future_local(async move {
        while let Ok(msg) = event_rx.recv().await {
            match msg {
                crate::ipc::IpcMessage::SearchChunk {
                    epoch,
                    phase,
                    hits,
                    top_hit,
                    claimed,
                } => {
                    let current_session_epoch = session_epoch.load(Ordering::SeqCst);
                    if epoch < current_session_epoch {
                        tracing::debug!(
                            "gui: dropping stale chunk epoch={} < session_epoch={}",
                            epoch,
                            current_session_epoch
                        );
                        continue;
                    }

                    if epoch > last_epoch.get() {
                        tracing::debug!(
                            "gui: new epoch {} > {}, clearing pending",
                            epoch,
                            last_epoch.get()
                        );
                        pending_hits.borrow_mut().clear();
                        last_epoch.set(epoch);
                    }

                    match phase {
                        lixun_ipc::Phase::Initial => {
                            tracing::debug!(
                                "gui: buffering Initial chunk epoch={} hits={}",
                                epoch,
                                hits.len()
                            );
                            *pending_hits.borrow_mut() = hits;
                            searching_indicator.set(true);

                            if let Some(id) = initial_timer.borrow_mut().take() {
                                id.remove();
                            }
                            let pending = std::rc::Rc::clone(&pending_hits);
                            let session_epoch_t = Arc::clone(&session_epoch);
                            let model_t = model.clone();
                            let filter_t = filter.clone();
                            let selection_t = selection.clone();
                            let list_view_t = list_view.clone();
                            let chips_t = chips_container.clone();
                            let scrolled_t = scrolled.clone();
                            let override_t = std::rc::Rc::clone(&user_selected_override);
                            let timer_slot = std::rc::Rc::clone(&initial_timer);
                            let id = glib::timeout_add_local_once(
                                Duration::from_millis(100),
                                move || {
                                    *timer_slot.borrow_mut() = None;
                                    if session_epoch_t.load(Ordering::SeqCst) != epoch {
                                        return;
                                    }
                                    let hits = pending.borrow().clone();
                                    if hits.is_empty() {
                                        return;
                                    }
                                    tracing::debug!(
                                        "gui: provisional render of {} Initial hits epoch={}",
                                        hits.len(),
                                        epoch
                                    );
                                    let prior = override_t.get().then(|| {
                                        let idx = selection_t.selected();
                                        selection_t.item(idx).and_then(|obj| {
                                            obj.downcast::<gtk::StringObject>()
                                                .ok()
                                                .map(|s| s.string().to_string())
                                        })
                                    }).flatten();
                                    // No top-hit nomination on Initial —
                                    // hero styling waits for the Final.
                                    let plan = compute_render_plan(&hits, None);
                                    update_results(&model_t, &selection_t, &plan.hits, None);
                                    filter_t.changed(gtk::FilterChange::Different);
                                    let new_idx = prior
                                        .as_deref()
                                        .and_then(|want| {
                                            (0..selection_t.n_items()).find(|&i| {
                                                selection_t
                                                    .item(i)
                                                    .and_then(|o| {
                                                        o.downcast::<gtk::StringObject>().ok()
                                                    })
                                                    .map(|s| s.string() == want)
                                                    .unwrap_or(false)
                                            })
                                        })
                                        .unwrap_or(0);
                                    if selection_t.n_items() > 0 {
                                        selection_t.set_selected(new_idx);
                                        list_view_t.scroll_to(
                                            new_idx,
                                            gtk::ListScrollFlags::NONE,
                                            None,
                                        );
                                    }
                                    chips_t.set_visible(true);
                                    scrolled_t.set_visible(true);
                                    scrolled_t.set_vexpand(false);
                                },
                            );
                            *initial_timer.borrow_mut() = Some(id);
                        }
                        lixun_ipc::Phase::Final => {
                            tracing::debug!(
                                "gui: rendering Final chunk epoch={} hits={} claimed={}",
                                epoch,
                                hits.len(),
                                claimed
                            );
                            // Final wins: never let the provisional
                            // Initial render fire after it.
                            if let Some(id) = initial_timer.borrow_mut().take() {
                                id.remove();
                            }
                            if let Some(id) = loading_timer.borrow_mut().take() {
                                id.remove();
                            }
                            let all_hits = if hits.is_empty() {
                                let pending = pending_hits.borrow().clone();
                                if !pending.is_empty() {
                                    tracing::debug!(
                                        "gui: Final empty, falling back to {} buffered Initial hits",
                                        pending.len()
                                    );
                                }
                                pending
                            } else {
                                hits
                            };
                            for (i, h) in all_hits.iter().enumerate().take(10) {
                                tracing::debug!(
                                    "gui: final hit[{}] id={} title={:?} score={:.4}",
                                    i,
                                    h.id.0,
                                    h.title,
                                    h.score
                                );
                            }
                            pending_hits.borrow_mut().clear();

                            let preserve_doc_id = user_selected_override.get();
                            let prior_selected = if preserve_doc_id {
                                let idx = selection.selected();
                                selection.item(idx).and_then(|obj| {
                                    obj.downcast::<gtk::StringObject>()
                                        .ok()
                                        .map(|s| s.string().to_string())
                                })
                            } else {
                                None
                            };

                            let plan = compute_render_plan(&all_hits, top_hit.as_ref());
                            let top_hit_doc_id = plan
                                .top_hit_index
                                .and_then(|i| plan.hits.get(i))
                                .map(|h| h.id.0.clone());

                            update_results(&model, &selection, &plan.hits, top_hit_doc_id);
                            searching_indicator.set(false);

                            filter.changed(gtk::FilterChange::Different);
                            if !plan.hits.is_empty() {
                                let wanted_doc = prior_selected
                                    .clone()
                                    .or_else(|| plan.hits.first().map(|h| h.id.0.clone()));
                                let new_idx = wanted_doc
                                    .as_deref()
                                    .and_then(|want| {
                                        (0..selection.n_items()).find(|&i| {
                                            selection
                                                .item(i)
                                                .and_then(|o| {
                                                    o.downcast::<gtk::StringObject>().ok()
                                                })
                                                .map(|s| s.string() == want)
                                                .unwrap_or(false)
                                        })
                                    })
                                    .unwrap_or(0);
                                if selection.n_items() > 0 {
                                    selection.set_selected(new_idx);
                                    list_view.scroll_to(new_idx, gtk::ListScrollFlags::NONE, None);
                                }
                            }

                            let has_anything = !plan.hits.is_empty();
                            // Calculator results present as a normal hit row
                            // (the calculator source emits the value as the
                            // hit title); the old status-bar calculation
                            // path was dead code and has been removed.
                            if !has_anything {
                                let q = last_query.borrow().clone();
                                if !q.is_empty() {
                                    chips_container.set_visible(true);
                                    scrolled.set_visible(false);
                                    scrolled.set_vexpand(false);
                                    present_zero_hit_status(
                                        &q,
                                        claimed,
                                        epoch,
                                        &status,
                                        &session_epoch,
                                        &index_status_cache,
                                        &index_status_inflight,
                                        &semantic_ui,
                                    );
                                    selection.set_selected(gtk::INVALID_LIST_POSITION);
                                } else {
                                    chips_container.set_visible(false);
                                    scrolled.set_visible(false);
                                    scrolled.set_vexpand(false);
                                    status.hide();
                                }
                            } else {
                                chips_container.set_visible(true);
                                let list_has_rows = !plan.hits.is_empty();
                                scrolled.set_visible(list_has_rows);
                                scrolled.set_vexpand(false);
                                // O3: replace the collapsed footer with
                                // the persistent dimmed key hints while
                                // results are on screen.
                                status.show_hints(&results_hint_line);
                            }
                        }
                    }
                }
                crate::ipc::IpcMessage::TransportFailed { epoch } => {
                    if epoch < session_epoch.load(Ordering::SeqCst) {
                        tracing::debug!("gui: dropping stale TransportFailed epoch={}", epoch);
                        continue;
                    }
                    if let Some(id) = loading_timer.borrow_mut().take() {
                        id.remove();
                    }
                    if let Some(id) = initial_timer.borrow_mut().take() {
                        id.remove();
                    }
                    pending_hits.borrow_mut().clear();
                    searching_indicator.set(false);
                    // Keep the query and any rendered results as they
                    // are: an actionable error plus the user's context
                    // beats a scrubbed launcher claiming "No results".
                    status.show_daemon_unresponsive();
                }
                crate::ipc::IpcMessage::SearchTimeout { epoch } => {
                    if epoch < session_epoch.load(Ordering::SeqCst) {
                        tracing::debug!("gui: dropping stale SearchTimeout epoch={}", epoch);
                        continue;
                    }
                    // The IPC reader keeps its connection open, so a
                    // slow Final can still land and replace this. Keep
                    // the spinner up; just explain the wait.
                    if let Some(id) = loading_timer.borrow_mut().take() {
                        id.remove();
                    }
                    status.show_still_searching();
                }
            }
        }
    });
}

/// Cached daemon Status sample for the zero-hit path: when it was
/// fetched and what it said (`None` payload = fetch failed).
type IndexStatusSnapshot = (Instant, Option<crate::ipc::DaemonStatusSnapshot>);

/// The empty-state semantic note (F6): shown when the operator has
/// `[semantic] enabled = true` but the worker is not `Ready`, so a
/// zero-hit result is honestly labelled as degraded rather than
/// authoritative.
fn semantic_down_note(semantic: &Option<(bool, String, bool)>) -> Option<String> {
    match semantic {
        Some((true, state, false)) => {
            Some(format!("Semantic search unavailable ({state})"))
        }
        _ => None,
    }
}

/// Decide the status-bar presentation for a genuine zero-hit Final.
///
/// During the minutes-long first index (and any full reindex) the
/// definitive "No results" is a lie — the documents just are not in
/// the index yet. For non-claimed queries, ask the daemon once
/// whether a reindex is running (sampled off the main thread over a
/// one-shot socket with a 500 ms read bound, cached for ~5 s) and
/// show indexing progress instead of the empty state.
#[allow(clippy::too_many_arguments)]
fn present_zero_hit_status(
    q: &str,
    claimed: bool,
    epoch: u64,
    status: &std::rc::Rc<StatusBar>,
    session_epoch: &Arc<AtomicU64>,
    cache: &std::rc::Rc<std::cell::RefCell<Option<IndexStatusSnapshot>>>,
    fetch_inflight: &std::rc::Rc<std::cell::Cell<bool>>,
    semantic_ui: &SemanticUiState,
) {
    // Claimed queries (shell `>`, calculator `=`) are answered by
    // their plugin, not the index — a reindex is irrelevant to them.
    if claimed {
        status.show_empty(q);
        return;
    }

    const INDEX_STATUS_TTL: Duration = Duration::from_secs(5);
    let cached = cache
        .borrow()
        .as_ref()
        .and_then(|(at, st)| (at.elapsed() < INDEX_STATUS_TTL).then_some(st.clone()));
    match cached {
        Some(Some(st)) if st.reindex_in_progress => status.show_indexing(st.indexed_docs),
        Some(st) => {
            // Definitive empty state; append the semantic-degraded
            // note when the worker is configured but down (F6).
            let note = st.as_ref().and_then(|s| semantic_down_note(&s.semantic));
            status.show_empty_with_note(q, note.as_deref());
        }
        None => {
            // Show the empty state immediately; upgrade to the
            // indexing state (or annotate semantic degradation) when
            // the daemon replies and the session hasn't moved on.
            status.show_empty(q);
            if fetch_inflight.get() {
                return;
            }
            fetch_inflight.set(true);
            let (tx, rx) = async_channel::bounded::<Option<crate::ipc::DaemonStatusSnapshot>>(1);
            std::thread::spawn(move || {
                let _ = tx.send_blocking(crate::ipc::request_daemon_status());
            });
            let cache = std::rc::Rc::clone(cache);
            let fetch_inflight = std::rc::Rc::clone(fetch_inflight);
            let status = std::rc::Rc::clone(status);
            let session_epoch = Arc::clone(session_epoch);
            let semantic_ui = std::rc::Rc::clone(semantic_ui);
            let q = q.to_string();
            glib::spawn_future_local(async move {
                let st = rx.recv().await.ok().flatten();
                fetch_inflight.set(false);
                *cache.borrow_mut() = Some((Instant::now(), st.clone()));
                if let Some(st) = &st {
                    // Keep the settings menu's semantic label fresh.
                    *semantic_ui.borrow_mut() = st.semantic.clone();
                }
                if epoch != session_epoch.load(Ordering::SeqCst) {
                    return;
                }
                if let Some(st) = st {
                    if st.reindex_in_progress {
                        status.show_indexing(st.indexed_docs);
                    } else if let Some(note) = semantic_down_note(&st.semantic) {
                        status.show_empty_with_note(&q, Some(&note));
                    }
                }
            });
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn install_entry_handler(
    entry: &gtk::Entry,
    ipc: IpcClient,
    model: gtk::StringList,
    selection: gtk::SingleSelection,
    chips_container: gtk::Box,
    scrolled: gtk::ScrolledWindow,
    status: std::rc::Rc<StatusBar>,
    last_query: std::rc::Rc<std::cell::RefCell<String>>,
    pending_debounce: std::rc::Rc<std::cell::RefCell<Option<glib::SourceId>>>,
    session_epoch: Arc<AtomicU64>,
    is_restoring: std::rc::Rc<std::cell::Cell<bool>>,
    user_selected_override: std::rc::Rc<std::cell::Cell<bool>>,
    _searching_indicator: std::rc::Rc<std::cell::Cell<bool>>,
    loading_timer: std::rc::Rc<std::cell::RefCell<Option<glib::SourceId>>>,
    claimed_prefixes: std::rc::Rc<Vec<String>>,
    max_results: u32,
    on_empty_query: std::rc::Rc<dyn Fn()>,
) {
    tracing::info!("gui: install_entry_handler called, registering connect_changed");
    entry.connect_changed(move |e| {
        if is_restoring.get() {
            tracing::debug!("gui: entry changed but is_restoring=true, skipping");
            return;
        }
        let text = e.text().to_string();
        tracing::debug!("gui: entry changed, text={:?}", text);

        if let Some(id) = pending_debounce.borrow_mut().take() {
            id.remove();
        }

        if text.is_empty() {
            // Bump the session epoch so any in-flight IPC reply
            // for the just-erased query lands in a stale epoch
            // and gets dropped by the response poller. Without
            // this, two regressions reappear the moment the
            // user clears the query via Backspace:
            //
            //   * stale hits from the previous non-empty query
            //     arrive after the clear, the poller runs the
            //     `hits_snapshot.is_empty()` check against the
            //     new empty state, falls through to the else-
            //     branch, and repopulates the list with random-
            //     looking rows (they are the old query's hits).
            //   * last_query still carries the prior text, so
            //     the poller's `q.is_empty()` check goes false
            //     and it pops `status.show_empty("firefox")`
            //     below the entry — that status bar is the
            //     phantom bottom margin that grew after each
            //     clear cycle.
            //
            // Clearing last_query + bumping epoch + dropping
            // the cached hits is exactly the subset of
            // scrub_ui() that matters here; the rest (entry,
            // chips, categories) we deliberately do NOT touch:
            // the user controls the entry via Backspace itself,
            // and the category filter staying on the user's
            // last choice across clears matches the current UX
            // contract for this bug fix.
            session_epoch.fetch_add(1, Ordering::SeqCst);
            if let Some(id) = pending_debounce.borrow_mut().take() {
                id.remove();
            }
            if let Some(id) = loading_timer.borrow_mut().take() {
                id.remove();
            }
            last_query.borrow_mut().clear();
            clear_cached_hits();
            // Stale IPC events with epoch < session_epoch are dropped
            // by install_response_handler (push-based, no shared
            // mutex to drain).

            // Disable autoselect around the bulk clear so
            // SingleSelection's interpolation formula
            // (gtksingleselection.c:253-296) does not drift the
            // selected index toward the end of the list on every
            // per-row items-changed emission. Re-enable after the
            // clear and pin selection to INVALID explicitly.
            selection.set_autoselect(false);
            let n = model.n_items();
            for _ in 0..n {
                model.remove(0);
            }
            selection.set_selected(gtk::INVALID_LIST_POSITION);
            selection.set_autoselect(true);
            user_selected_override.set(false);
            chips_container.set_visible(false);
            scrolled.set_visible(false);
            scrolled.set_vexpand(false);
            status.hide();
            // O4: a cleared query returns to the idle state, which
            // now offers the frecency "Recent" section (epoch-guarded
            // inside; a keystroke supersedes the fetch).
            on_empty_query();
            return;
        }

        // Fresh keystroke => fresh ranking, row 0 wins. Clear the
        // override so the poller snaps to row 0 on this response.
        user_selected_override.set(false);

        // Bump session epoch on every non-empty keystroke (not just
        // empty). Without this, in-flight IPC replies for the
        // PREVIOUS query land in the shared `ipc.responses` mutex
        // with a matching epoch and the poller renders them as if
        // they were results for the CURRENT query — visible as
        // "type AQL-HSSA, backspace to AQ, see AQL-HSSA results
        // back in the list". Bumping here invalidates every
        // outstanding chunk: the IPC reader breaks its read loop
        // (ipc.rs epoch_at_send check) and any chunk that did
        // commit before the bump gets dropped on epoch mismatch
        // (ipc.rs resp_epoch check).
        //
        // Also drain the shared response slots so the poller does
        // not pick up stale leftovers between the bump and the
        // next chunk arrival.
        session_epoch.fetch_add(1, Ordering::SeqCst);
        // Stale IPC events with epoch < session_epoch are dropped
        // by install_response_handler (push-based, no shared
        // mutex to drain).

        chips_container.set_visible(true);

        let ipc = ipc.clone();
        let status_for_debounce = std::rc::Rc::clone(&status);
        let q = text.clone();
        let last_q = std::rc::Rc::clone(&last_query);
        let pending_self = std::rc::Rc::clone(&pending_debounce);
        let epoch = Arc::clone(&session_epoch);
        let prefixes_for_debounce = std::rc::Rc::clone(&claimed_prefixes);
        let loading_timer_for_debounce = std::rc::Rc::clone(&loading_timer);
        // 30 ms keystroke debounce. A6 (8a309de) introduced
        // cooperative cancellation in the daemon's collector, so
        // superseded queries are aborted server-side and the GUI
        // no longer needs the 80 ms guard that previously absorbed
        // bursts. The 30 ms residual is a single-frame-budget
        // safety margin so we don't fire a fresh IPC round-trip on
        // every individual keystroke during rapid typing.
        let id = glib::timeout_add_local_once(Duration::from_millis(30), move || {
            *last_q.borrow_mut() = q.clone();
            let epoch_snapshot = epoch.load(Ordering::SeqCst);
            tracing::debug!(
                "gui: debounce fired, sending search query={:?} limit={} epoch={}",
                q,
                max_results,
                epoch_snapshot
            );
            // Skip "Searching…" spinner for queries claimed by an
            // instant plugin (shell `>`, calculator `=`). These
            // respond in <10ms so the spinner would only flash.
            // Claimed prefixes are fetched from the daemon on
            // startup, so no plugin-specific strings live in GUI code.
            let trimmed = q.trim_start();
            let is_claimed = prefixes_for_debounce
                .iter()
                .any(|p| trimmed.starts_with(p.as_str()));
            // Delayed spinner: cancel whatever the previous keystroke
            // armed, then re-arm. The spinner only appears if this
            // search is still unanswered after 120 ms — warm queries
            // Final in ~20 ms, so they never flash the launcher's
            // bottom edge. The Final handler cancels the pending
            // timer (as do the empty-query and transport-error
            // paths).
            if let Some(id) = loading_timer_for_debounce.borrow_mut().take() {
                id.remove();
            }
            if !is_claimed {
                let timer_slot = std::rc::Rc::clone(&loading_timer_for_debounce);
                let timer_id =
                    glib::timeout_add_local_once(Duration::from_millis(120), move || {
                        *timer_slot.borrow_mut() = None;
                        status_for_debounce.show_loading();
                    });
                *loading_timer_for_debounce.borrow_mut() = Some(timer_id);
            }
            let _ = ipc.request_tx.send((q, max_results, epoch_snapshot));
            *pending_self.borrow_mut() = None;
        });
        *pending_debounce.borrow_mut() = Some(id);
    });
}

pub(crate) struct CategoryChips {
    pub(crate) container: gtk::Box,
    pub(crate) buttons: [gtk::ToggleButton; 5],
}

impl CategoryChips {
    pub(crate) fn wire_toggle<F>(&self, on_change: F)
    where
        F: Fn() + 'static + Clone,
    {
        for button in &self.buttons {
            let cb = on_change.clone();
            button.connect_toggled(move |_| {
                cb();
            });
        }
    }

    pub(crate) fn activate_index(&self, index: usize) {
        if let Some(btn) = self.buttons.get(index) {
            btn.set_active(true);
        }
    }

    pub(crate) fn active_index(&self) -> Option<usize> {
        self.buttons.iter().position(|b| b.is_active())
    }
}

fn build_category_chips(
    current: &CategoryFilter,
    keybindings: &lixun_config::Keybindings,
) -> CategoryChips {
    let container = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    container.set_widget_name("lixun-chips");
    container.set_margin_top(4);
    container.set_margin_bottom(2);
    add_css_class(&container, "lixun-chips");

    let labels = [
        ("All", None),
        ("Apps", Some(Category::App)),
        ("Files", Some(Category::File)),
        ("Mail", Some(Category::Mail)),
        ("Attachments", Some(Category::Attachment)),
    ];

    // Tooltip shows each chip's *resolved* accelerator — read from the
    // parsed keybindings, never hardcoded, so user rebinds stay truthful.
    let accels = [
        keybindings.filter_all.as_str(),
        keybindings.filter_apps.as_str(),
        keybindings.filter_files.as_str(),
        keybindings.filter_mail.as_str(),
        keybindings.filter_attachments.as_str(),
    ];

    let mut buttons: Vec<gtk::ToggleButton> = Vec::with_capacity(5);
    let group_anchor: Option<gtk::ToggleButton> = None;
    let mut group_anchor = group_anchor;

    for ((label, _cat), accel) in labels.iter().zip(accels) {
        let b = gtk::ToggleButton::with_label(label);
        add_css_class(&b, "lixun-chip");
        if let Some((key, mods)) = gtk::accelerator_parse(accel) {
            b.set_tooltip_text(Some(&gtk::accelerator_get_label(key, mods)));
        }
        if let Some(anchor) = group_anchor.as_ref() {
            b.set_group(Some(anchor));
        } else {
            group_anchor = Some(b.clone());
        }
        container.append(&b);
        buttons.push(b);
    }

    buttons[0].set_active(true);

    for (button, (_, cat)) in buttons.iter().zip(labels.iter()) {
        let current_clone = std::rc::Rc::clone(current);
        let cat = *cat;
        button.connect_toggled(move |b| {
            if b.is_active() {
                current_clone.set(cat);
            }
        });
    }

    let buttons_arr: [gtk::ToggleButton; 5] = buttons
        .try_into()
        .expect("exactly 5 chip buttons constructed");

    CategoryChips {
        container,
        buttons: buttons_arr,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lixun_core::{Action, Category, DocId};

    fn mk_hit(id: &str, title: &str) -> Hit {
        Hit {
            id: DocId(id.into()),
            category: Category::App,
            title: title.into(),
            subtitle: String::new(),
            icon_name: None,
            kind_label: None,
            score: 0.0,
            action: Action::Launch {
                exec: vec!["true".into()],
                terminal: false,
                desktop_id: None,
                desktop_file: None,
                working_dir: None,
            },
            extract_fail: false,
            sender: None,
            recipients: None,
            body: None,
            secondary_action: None,
            source_instance: String::new(),
            row_menu: lixun_core::RowMenuDef::empty(),
            mime: None,
            timestamp: None,
            size: None,
        }
    }

    #[test]
    fn response_routing_renders_hero() {
        let hits = vec![
            mk_hit("app:editor-a", "Editor A"),
            mk_hit("app:browser-b", "Browser B"),
            mk_hit("app:mailer-c", "Mailer C"),
        ];
        let top = DocId("app:editor-a".into());
        let plan = compute_render_plan(&hits, Some(&top));
        assert_eq!(plan.top_hit_index, Some(0));
        assert_eq!(plan.hits.len(), 3);
        assert_eq!(plan.hits[0].id.0, "app:editor-a");
        assert_eq!(plan.hits[1].id.0, "app:browser-b");
        assert_eq!(plan.hits[2].id.0, "app:mailer-c");
    }

    #[test]
    fn hero_hidden_without_top_hit() {
        let hits = vec![mk_hit("app:a", "A"), mk_hit("app:b", "B")];
        let plan = compute_render_plan(&hits, None);
        assert!(plan.top_hit_index.is_none());
        assert_eq!(plan.hits.len(), 2);
        assert_eq!(plan.hits[0].id.0, "app:a");
        assert_eq!(plan.hits[1].id.0, "app:b");
    }

    #[test]
    fn top_hit_moves_to_front() {
        let hits = vec![
            mk_hit("app:a", "A"),
            mk_hit("app:b", "B"),
            mk_hit("app:c", "C"),
        ];
        let top = DocId("app:b".into());
        let plan = compute_render_plan(&hits, Some(&top));
        assert_eq!(plan.top_hit_index, Some(0));
        let ids: Vec<&str> = plan.hits.iter().map(|h| h.id.0.as_str()).collect();
        assert_eq!(ids, vec!["app:b", "app:a", "app:c"]);
    }

    #[test]
    fn unknown_top_hit_id_degrades_to_no_hero() {
        let hits = vec![mk_hit("app:a", "A"), mk_hit("app:b", "B")];
        let top = DocId("app:missing".into());
        let plan = compute_render_plan(&hits, Some(&top));
        assert!(plan.top_hit_index.is_none());
        assert_eq!(plan.hits.len(), 2);
        assert_eq!(plan.hits[0].id.0, "app:a");
        assert_eq!(plan.hits[1].id.0, "app:b");
    }

    #[test]
    fn empty_hits_with_some_top_hit() {
        let hits: Vec<Hit> = vec![];
        let top = DocId("app:x".into());
        let plan = compute_render_plan(&hits, Some(&top));
        assert!(plan.top_hit_index.is_none());
        assert!(plan.hits.is_empty());
    }

    #[test]
    fn top_hit_already_at_front() {
        let hits = vec![
            mk_hit("app:x", "X"),
            mk_hit("app:y", "Y"),
            mk_hit("app:z", "Z"),
        ];
        let top = DocId("app:x".into());
        let plan = compute_render_plan(&hits, Some(&top));
        assert_eq!(plan.top_hit_index, Some(0));
        let ids: Vec<&str> = plan.hits.iter().map(|h| h.id.0.as_str()).collect();
        assert_eq!(ids, vec!["app:x", "app:y", "app:z"]);
    }

    #[test]
    fn compute_render_plan_is_deterministic() {
        let hits = vec![
            mk_hit("app:a", "A"),
            mk_hit("app:b", "B"),
            mk_hit("app:c", "C"),
        ];
        let top = DocId("app:b".into());
        let p1 = compute_render_plan(&hits, Some(&top));
        let p2 = compute_render_plan(&hits, Some(&top));
        assert_eq!(p1.top_hit_index, p2.top_hit_index);
        let ids1: Vec<&str> = p1.hits.iter().map(|h| h.id.0.as_str()).collect();
        let ids2: Vec<&str> = p2.hits.iter().map(|h| h.id.0.as_str()).collect();
        assert_eq!(ids1, ids2);
    }
}
