//! Keyboard bindings for the launcher window.
//!
//! Centralizes all key handling so window.rs stays small. Extended in later
//! waves for category filters (Ctrl+1..4), category jumps (Ctrl+Down/Up),
//! Quick Look (Space), and history navigation (↑ in empty entry).

use glib::clone;
use gtk::prelude::*;
use lixun_core::Action;
use lixun_config::Keybindings;

use crate::actions::{
    copy_to_clipboard, execute_action, execute_secondary_action, run_and_capture_async,
};
use crate::factory::{cached_hit_by_id, synthetic_history_hits, update_results, with_cached_hits};
use crate::ipc::{
    IpcClient, current_monitor_connector, dispatch_click_pair, fetch_search_history_async,
    send_preview_request, send_preview_scroll,
};
use crate::status::StatusBar;
use crate::window::{CategoryChips, LauncherController};

fn selected_hit_in<F: FnOnce(&lixun_core::Hit)>(
    selection: &gtk::SingleSelection,
    filter_model: &gtk::FilterListModel,
    f: F,
) {
    let idx = selection.selected();
    if let Some(item) = filter_model.item(idx)
        && let Some(str_obj) = item.downcast_ref::<gtk::StringObject>()
    {
        let doc_id = str_obj.string().to_string();
        // Clone-out before invoking callback: callbacks for actions like
        // ReplaceQuery call entry.set_text() which synchronously fires
        // connect_changed → clear_cached_hits() → borrow_mut() panic
        // (already-borrowed) if we held a live CACHED_HITS read borrow.
        if let Some(hit) = cached_hit_by_id(&doc_id) {
            f(&hit);
        }
    }
}

fn jump_to_next_category(
    selection: &gtk::SingleSelection,
    filter_model: &gtk::FilterListModel,
    direction: i32,
) {
    let n = filter_model.n_items();
    if n == 0 {
        return;
    }
    let current_idx = selection.selected();

    let current_cat = filter_model
        .item(current_idx)
        .and_then(|o| o.downcast::<gtk::StringObject>().ok())
        .and_then(|s| {
            let id = s.string().to_string();
            with_cached_hits(|hits| hits.iter().find(|h| h.id.0 == id).map(|h| h.category))
        });

    let range: Box<dyn Iterator<Item = u32>> = if direction > 0 {
        Box::new((current_idx + 1)..n)
    } else {
        Box::new((0..current_idx).rev())
    };

    for i in range {
        let cat = filter_model
            .item(i)
            .and_then(|o| o.downcast::<gtk::StringObject>().ok())
            .and_then(|s| {
                let id = s.string().to_string();
                with_cached_hits(|hits| hits.iter().find(|h| h.id.0 == id).map(|h| h.category))
            });
        if cat != current_cat {
            selection.set_selected(i);
            return;
        }
    }
}

/// The modifier bits dispatch distinguishes on. Lock/IM state bits
/// (CapsLock, NumLock, Button1…) are ignored on both sides.
fn dispatch_modifier_mask() -> gtk::gdk::ModifierType {
    gtk::gdk::ModifierType::CONTROL_MASK
        | gtk::gdk::ModifierType::SHIFT_MASK
        | gtk::gdk::ModifierType::ALT_MASK
        | gtk::gdk::ModifierType::SUPER_MASK
}

/// Exact equality on the masked modifier set — NOT `contains`. With
/// `contains`, bare "Down" would also fire on Ctrl+Down, shadowing
/// `next_category`, and bare "space" `quick_look` would swallow
/// `quick_look_alt` (<Ctrl>space). Extra held modifiers now make an
/// accel NOT match, which is what lets Ctrl-chord rebinds coexist
/// with plain bindings on the same base key.
fn mods_match_exact(expected: gtk::gdk::ModifierType, state: gtk::gdk::ModifierType) -> bool {
    let mask = dispatch_modifier_mask();
    (state & mask) == (expected & mask)
}

fn accel_matches(accel: &str, key: gtk::gdk::Key, state: gtk::gdk::ModifierType) -> bool {
    let Some((expected_key, expected_mods)) = gtk::accelerator_parse(accel) else {
        return false;
    };
    key == expected_key && mods_match_exact(expected_mods, state)
}

/// True when the Entry or any of its descendants (the internal GtkText
/// delegate that actually owns keyboard focus) is the focused widget.
/// `gtk::Entry::is_focus()` returns false in that case because Entry is a
/// composite: GtkText is a child widget, not the Entry itself. Without
/// this helper our key dispatch would treat a typing Entry as unfocused
/// and swallow Space into `quick_look`.
fn entry_has_focus(entry: &gtk::Entry, window: &gtk::ApplicationWindow) -> bool {
    let Some(focus) = gtk::prelude::RootExt::focus(window) else {
        return false;
    };
    let entry_widget = entry.upcast_ref::<gtk::Widget>();
    &focus == entry_widget || focus.is_ancestor(entry)
}

/// Key should be forwarded to a focused text input instead of intercepted
/// by window-level shortcut dispatch. Shift alone is allowed (capitals);
/// Ctrl/Alt/Super/Meta/Hyper disqualify. Keys without a non-control
/// Unicode mapping (Escape, Return, arrows, Tab, F-keys) also disqualify.
fn is_printable_key(key: gtk::gdk::Key, state: gtk::gdk::ModifierType) -> bool {
    let non_shift_mods = gtk::gdk::ModifierType::CONTROL_MASK
        | gtk::gdk::ModifierType::ALT_MASK
        | gtk::gdk::ModifierType::SUPER_MASK
        | gtk::gdk::ModifierType::META_MASK
        | gtk::gdk::ModifierType::HYPER_MASK;
    if state.intersects(non_shift_mods) {
        return false;
    }
    matches!(key.to_unicode(), Some(c) if !c.is_control())
}

/// Scroll the list so `target` and a few rows past it (in the
/// direction of movement) stay visible. Solves the 'selected row
/// sits at the bottom edge with no context below' problem by
/// pinning the scroll anchor at `target + margin` — GTK ensures
/// that anchor is in the viewport, which means `target` itself
/// lands higher up. `delta` is the scroll direction: positive for
/// Down navigation (look-ahead below), negative for Up (look-ahead
/// above). Margin of 3 gives a usable amount of context without
/// scrolling too eagerly on every keypress.
fn scroll_with_margin(
    list_view: &gtk::ListView,
    selection: &gtk::SingleSelection,
    target: u32,
    delta: i32,
) {
    const MARGIN: i32 = 3;
    let n = selection.n_items();
    if n == 0 {
        return;
    }
    let anchor_signed = target as i32 + delta.signum() * MARGIN;
    let anchor = anchor_signed.clamp(0, n as i32 - 1) as u32;
    let info = gtk::ScrollInfo::new();
    info.set_enable_vertical(true);
    // NONE: do not steal focus to the anchor row, do not change
    // selection — just make sure it is on-screen. The caller
    // already updated `selection.set_selected(target)` before this.
    list_view.scroll_to(anchor, gtk::ListScrollFlags::NONE, Some(info));
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn install_keyboard_handler(
    window: &gtk::ApplicationWindow,
    list_view: &gtk::ListView,
    entry: &gtk::Entry,
    selection: &gtk::SingleSelection,
    filter_model: &gtk::FilterListModel,
    model: &gtk::StringList,
    chips: std::rc::Rc<CategoryChips>,
    status_bar: std::rc::Rc<StatusBar>,
    scrolled: &gtk::ScrolledWindow,
    chips_container: &gtk::Box,
    _ipc: IpcClient,
    keybindings: Keybindings,
    controller: std::rc::Rc<LauncherController>,
) {
    // Main key controller attached to window with Capture phase.
    // Capture is required so bare-Space `quick_look` fires when focus is on
    // GtkListView — in Bubble phase the list view consumes Space (default
    // row-activate handler) before the accel dispatcher sees it, breaking
    // preview. The entry_has_focus + is_printable_key short-circuit below
    // returns Proceed so GtkText IM still receives printable text input.
    let key_controller = gtk::EventControllerKey::new();
    key_controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    // Live handle to the O3 shortcuts overlay while it is up. Owned
    // here because the keymap is its only opener and dismisser; the
    // popover's connect_closed clears it (window.rs).
    let shortcuts_overlay: std::rc::Rc<std::cell::RefCell<Option<gtk::Popover>>> =
        std::rc::Rc::new(std::cell::RefCell::new(None));
    key_controller.connect_key_pressed(clone!(
        #[strong]
        selection,
        #[strong]
        filter_model,
        #[strong]
        list_view,
        #[strong]
        window,
        #[strong]
        entry,
        #[strong]
        chips,
        #[strong]
        keybindings,
        #[strong]
        controller,
        #[strong]
        status_bar,
        #[strong]
        shortcuts_overlay,
        move |_, key, _keycode, state| {
            let entry_focus = entry_has_focus(&entry, &window);
            let printable = is_printable_key(key, state);
            tracing::info!(
                "gui: window key_controller fired key={:?} entry_focus={} printable={}",
                key.name(),
                entry_focus,
                printable
            );
            // A focused button (category chip ToggleButton — a Button
            // subclass — or the status bar's "Search the web") must stay
            // activatable from the keyboard: without this, the capture-
            // phase dispatch below swallows Return into `primary_action`
            // and Space into `quick_look`, leaving the focus ring on a
            // widget that keys can never activate. Proceed hands the key
            // to GTK's default button activation. Printable keys other
            // than Space still fall through to the warp-back-to-entry
            // logic below.
            if let Some(focused) = gtk::prelude::RootExt::focus(&window)
                && focused.downcast_ref::<gtk::Button>().is_some()
                && matches!(
                    key,
                    gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter | gtk::gdk::Key::space
                )
            {
                return glib::signal::Propagation::Proceed;
            }
            // O3 modality: while the shortcuts overlay is visible,
            // the next key closes it. Escape/?/F1 are consumed as a
            // pure "close" (Escape must NOT fall through to the
            // launcher-hide branch below); every other key falls
            // through to its normal meaning after the close, so Down
            // navigates and letters type into the entry. The popover
            // holds no grab (see show_shortcuts_overlay), so all keys
            // arrive here. Clone out of the RefCell before popdown():
            // popdown fires connect_closed, which mutably borrows the
            // slot to clear it.
            let overlay = shortcuts_overlay.borrow().clone();
            if let Some(p) = overlay {
                if p.is_visible() {
                    p.popdown();
                    if matches!(
                        key,
                        gtk::gdk::Key::Escape | gtk::gdk::Key::question | gtk::gdk::Key::F1
                    ) {
                        return glib::signal::Propagation::Stop;
                    }
                } else {
                    // Stale handle (e.g. the launcher was hidden with
                    // the overlay up): just release it.
                    shortcuts_overlay.borrow_mut().take();
                }
            }
            // P11: while a preview is on screen, PgUp/PgDn (and
            // Shift+Space page-back, mirroring Quick Look) page the
            // preview content without moving focus into its window.
            // Dispatched before every other branch so Shift+Space is
            // not mistaken for a printable key (which would dismiss
            // the preview below). Keys are fixed, not rebindable —
            // they only exist inside preview mode.
            if controller.preview_mode_active() {
                let page = match key {
                    gtk::gdk::Key::Page_Down | gtk::gdk::Key::KP_Page_Down => Some(true),
                    gtk::gdk::Key::Page_Up | gtk::gdk::Key::KP_Page_Up => Some(false),
                    gtk::gdk::Key::space
                        if mods_match_exact(gtk::gdk::ModifierType::SHIFT_MASK, state) =>
                    {
                        Some(false)
                    }
                    _ => None,
                };
                if let Some(down) = page {
                    send_preview_scroll(down, 1);
                    return glib::signal::Propagation::Stop;
                }
            }
            // O3: shortcuts overlay on F1 (always) and ? (only while
            // the query is empty — with text present, "?" stays
            // typeable). Lists the RESOLVED keybindings so user
            // rebinds display truthfully.
            if key == gtk::gdk::Key::F1
                || (key == gtk::gdk::Key::question && entry.text().is_empty())
            {
                crate::window::show_shortcuts_overlay(&entry, &keybindings, &shortcuts_overlay);
                return glib::signal::Propagation::Stop;
            }
            // quick_look_alt (default <Ctrl>space) opens Quick Look on
            // the selected row even while the entry owns focus. The
            // bare-Space `quick_look` below requires focus to be OFF the
            // entry (Space must stay typeable), which made previewing
            // the Top Hit cost Down, Up, Space; the Ctrl chord carries
            // no text so it can fire from typing position. Dispatched
            // before the printable-key short-circuit on purpose.
            if accel_matches(&keybindings.quick_look_alt, key, state)
                && filter_model.n_items() > 0
            {
                controller.set_preview_mode_active(true);
                let monitor = current_monitor_connector(&window);
                selected_hit_in(&selection, &filter_model, |hit| {
                    send_preview_request(hit, monitor.clone());
                });
                // Same focus re-assert as the quick_look branch below.
                entry.grab_focus();
                return glib::signal::Propagation::Stop;
            }
            // Hard rule: printable unmodified keys belong to the focused
            // Entry. Forward them before any accel dispatch can swallow
            // them (e.g. bare-Space `quick_look` binding).
            // Exception: while preview is active, fall through so the
            // preview-dismissal block below closes the preview first.
            // Otherwise Space in preview mode would type ' ' into the
            // query instead of closing the preview.
            if entry_has_focus(&entry, &window)
                && is_printable_key(key, state)
                && !controller.preview_mode_active()
            {
                return glib::signal::Propagation::Proceed;
            }
            // BUG-5: focus has left the entry (list_view grabbed it on
            // Down, or user clicked a result row). A printable key that
            // is NOT a registered accel (quick_look=Space stays a
            // quick_look trigger) should warp back to the entry and
            // continue typing — that is the Raycast/Alfred contract.
            // Without this, once focus moves to the list the launcher
            // becomes a dead keyboard surface until Escape/Enter/click.
            // Typing while preview is active dismisses the preview and
            // forwards the key to the Entry. Mirrors Spotlight/Alfred:
            // any printable key or Backspace ends the QuickLook session
            // and resumes search-as-you-type. Returning Proceed lets
            // GtkEntry's IM context (fcitx5 preedit/composition) own
            // the keystroke — bypassing IM here is what produced the
            // reversed-character / select-all chaos with Cyrillic input.
            //
            // Space is special: it is the quick_look accel that opened
            // the preview, so a second Space must CLOSE preview without
            // typing ' ' into the query. Stop propagation for Space;
            // Proceed for every other printable key and Backspace.
            // The quick_look accel only OPENS preview when entry lacks
            // focus (line 428), so suppressing it here while preview is
            // already active is safe.
            if controller.preview_mode_active()
                && (is_printable_key(key, state) || key == gtk::gdk::Key::BackSpace)
                && !accel_matches(&keybindings.close, key, state)
                && !accel_matches(&keybindings.primary_action, key, state)
                && !accel_matches(&keybindings.secondary_action, key, state)
            {
                crate::ipc::send_preview_hide_request();
                controller.set_preview_mode_active(false);
                entry.grab_focus();
                if accel_matches(&keybindings.quick_look, key, state) {
                    return glib::signal::Propagation::Stop;
                }
                return glib::signal::Propagation::Proceed;
            }
            // Synthetic printable-key warp is disabled while preview is
            // active. Under the passive-preview model the launcher Entry
            // always retains focus, so this path is unreachable in
            // preview mode; running it anyway would bypass the IM
            // context (fcitx5 preedit/composition) and produce reversed
            // characters or stray select-all on language switch.
            if !controller.preview_mode_active()
                && !entry_has_focus(&entry, &window)
                && is_printable_key(key, state)
                && !accel_matches(&keybindings.quick_look, key, state)
                && let Some(ch) = key.to_unicode()
            {
                let cur = entry.text().to_string();
                let mut appended = cur;
                appended.push(ch);
                entry.set_text(&appended);
                entry.set_position(-1);
                entry.grab_focus();
                entry.set_position(-1);
                return glib::signal::Propagation::Stop;
            }
            // Same contract for BACKSPACE. `is_printable_key` rejects
            // it (Unicode U+0008 is a control char), so the block
            // above never fires; without this branch Backspace while
            // the list has focus hits GtkListView, which ignores it,
            // and the user's query becomes read-only whenever the
            // cursor is on a row. Spotlight lets you keep editing in
            // that state; match its behaviour by deleting the last
            // character of the query and warping focus back to the
            // entry. Bare Backspace only — Ctrl/Alt/Super variants
            // fall through so future word-delete accels stay
            // available.
            if !controller.preview_mode_active()
                && !entry_has_focus(&entry, &window)
                && key == gtk::gdk::Key::BackSpace
                && !state.intersects(
                    gtk::gdk::ModifierType::CONTROL_MASK
                        | gtk::gdk::ModifierType::ALT_MASK
                        | gtk::gdk::ModifierType::SUPER_MASK
                        | gtk::gdk::ModifierType::META_MASK
                        | gtk::gdk::ModifierType::HYPER_MASK,
                )
            {
                let mut text = entry.text().to_string();
                // Rust's String::pop is char-aware, so a cyrillic or
                // other multibyte trailing character is removed as a
                // single unit — we never leave the string on a non-
                // char-boundary byte index.
                text.pop();
                entry.set_text(&text);
                entry.grab_focus();
                entry.set_position(-1);
                return glib::signal::Propagation::Stop;
            }
            // Category jumps are dispatched BEFORE result navigation on
            // their own configured accels (`next_category` /
            // `previous_category`, default <Ctrl>Down/<Ctrl>Up). The old
            // code hardcoded a Ctrl flag inside the result branches,
            // which only worked because accel matching was inexact —
            // rebinding `next_result` to any Ctrl chord made plain
            // navigation unreachable.
            if accel_matches(&keybindings.previous_category, key, state)
                || accel_matches(&keybindings.next_category, key, state)
            {
                // BUG-5 regression guard: same defensive pin as Up/Down.
                if filter_model.n_items() == 0 {
                    entry.grab_focus();
                    return glib::signal::Propagation::Stop;
                }
                let direction = if accel_matches(&keybindings.next_category, key, state) {
                    1
                } else {
                    -1
                };
                let entry_had_focus = entry_has_focus(&entry, &window);
                jump_to_next_category(&selection, &filter_model, direction);
                let target = selection.selected();
                if target != gtk::INVALID_LIST_POSITION {
                    scroll_with_margin(&list_view, &selection, target, direction);
                    controller.mark_user_selected();
                }
                if entry_had_focus && !controller.preview_mode_active() {
                    list_view.grab_focus();
                }
                return glib::signal::Propagation::Stop;
            }
            if accel_matches(&keybindings.previous_result, key, state) {
                // Up on empty entry = let entry_key_controller handle history
                if entry.text().is_empty() && entry_has_focus(&entry, &window) {
                    return glib::signal::Propagation::Proceed;
                }
                // Defensive: no results, no list to navigate. Pin focus
                // in the entry so GTK's default Up/Down focus-chain
                // cannot warp focus to a sibling widget (the list is
                // hidden, chip row, etc.) leaving the user stuck in
                // a widget that doesn't accept printable keys.
                // BUG-5 regression guard.
                if filter_model.n_items() == 0 {
                    entry.grab_focus();
                    return glib::signal::Propagation::Stop;
                }
                let entry_had_focus = entry_has_focus(&entry, &window);
                let current = selection.selected();
                if current > 0 {
                    let target = current - 1;
                    selection.set_selected(target);
                    controller.mark_user_selected();
                    scroll_with_margin(&list_view, &selection, target, -1);
                    if entry_had_focus && !controller.preview_mode_active() {
                        list_view.grab_focus();
                    }
                } else {
                    // Already at the top row; Up returns focus to the Entry.
                    entry.grab_focus();
                }
                glib::signal::Propagation::Stop
            } else if accel_matches(&keybindings.next_result, key, state) {
                // BUG-5 regression guard: same defensive pin as Up.
                if filter_model.n_items() == 0 {
                    entry.grab_focus();
                    return glib::signal::Propagation::Stop;
                }
                let entry_had_focus = entry_has_focus(&entry, &window);
                let current = selection.selected();
                let n = selection.n_items();
                if current + 1 < n {
                    let target = current + 1;
                    selection.set_selected(target);
                    controller.mark_user_selected();
                    scroll_with_margin(&list_view, &selection, target, 1);
                }
                if entry_had_focus && n > 0 && !controller.preview_mode_active() {
                    list_view.grab_focus();
                }
                glib::signal::Propagation::Stop
            } else if accel_matches(&keybindings.close, key, state) {
                if controller.preview_mode_active() {
                    // Two-stage Escape: first press dismisses
                    // the preview only (warm process stays
                    // running), second press (preview_mode_active
                    // now false) hides the launcher. Spotlight
                    // QuickLook semantics. The IPC roundtrips
                    // through the daemon which then dispatches
                    // ExitPreviewMode back — but we reset the
                    // local flag synchronously here too so the
                    // next arrow keypress doesn't race the IPC.
                    crate::ipc::send_preview_hide_request();
                    controller.set_preview_mode_active(false);
                } else {
                    controller.hide();
                }
                glib::signal::Propagation::Stop
            } else if accel_matches(&keybindings.primary_action, key, state)
                || accel_matches(&keybindings.secondary_action, key, state)
            {
                // Zero results: Enter falls back to the web search the
                // status bar advertises (which was mouse-only before)
                // instead of silently closing the launcher and wiping
                // the session. Empty query = nothing to search, stay up.
                if filter_model.n_items() == 0 {
                    let q = entry.text().to_string();
                    if !q.is_empty() {
                        crate::status::open_web_search(&q);
                        controller.clear_and_hide();
                    }
                    return glib::signal::Propagation::Stop;
                }
                let mut should_hide = true;
                let query_at_click = entry.text().to_string();
                let is_secondary = accel_matches(&keybindings.secondary_action, key, state);
                selected_hit_in(&selection, &filter_model, |hit| {
                    // Secondary ReplaceQuery chains the hit's value back
                    // into the entry (e.g. Shift+Enter on a calculator
                    // result continues the computation) — mid-session,
                    // never a launch.
                    if is_secondary
                        && let Some(sec) = &hit.secondary_action
                        && let Action::ReplaceQuery { q } = sec.as_ref()
                    {
                        entry.set_text(q);
                        entry.set_position(-1);
                        entry.grab_focus();
                        should_hide = false;
                        return;
                    }
                    if let Action::ReplaceQuery { q } = &hit.action {
                        entry.set_text(q);
                        entry.set_position(-1);
                        entry.grab_focus();
                        should_hide = false;
                        return;
                    }
                    // Primary CopyText: put the value on the clipboard,
                    // confirm via toast, and KEEP the launcher open so
                    // the user can keep chaining (calculator results).
                    if !is_secondary
                        && let Action::CopyText { text } = &hit.action
                    {
                        if let Some(display) = gtk::gdk::Display::default() {
                            display.clipboard().set_text(text);
                        }
                        status_bar.show_toast(&format!("Copied: {}", text));
                        should_hide = false;
                        return;
                    }
                    // Shift+Enter on a hit whose secondary action is
                    // ExecCapture: run the command headless, capture
                    // stdout, and replace the query text. Used by
                    // shell plugin to pipe command output into the
                    // launcher instead of opening a terminal. The
                    // capture waits on a worker thread; the query
                    // replacement callback runs back on the main
                    // thread once output (or a timeout) arrives.
                    if is_secondary
                        && let Some(sec) = &hit.secondary_action
                        && let Action::ExecCapture {
                            cmdline,
                            working_dir,
                        } = sec.as_ref()
                    {
                        let entry = entry.clone();
                        run_and_capture_async(cmdline, working_dir.as_deref(), move |output| {
                            if let Some(output) = output {
                                entry.set_text(output.trim_end());
                                entry.set_position(-1);
                                entry.grab_focus();
                            }
                        });
                        should_hide = false;
                        return;
                    }
                    dispatch_click_pair(&hit.id.0, &query_at_click);
                    let result = if is_secondary {
                        execute_secondary_action(hit)
                    } else {
                        execute_action(hit)
                    };
                    if let Err(e) = result {
                        tracing::error!("Action failed: {}", e);
                        // Keep the launcher up and say what went wrong.
                        // Hiding on a failed launch makes the failure
                        // indistinguishable from success — the user sees
                        // the window vanish and no application appear,
                        // with nothing to act on. The double-click path
                        // already declines to hide (factory.rs); mirror
                        // it here.
                        status_bar.show_error(&format!("Couldn't open “{}”: {}", hit.title, e));
                        should_hide = false;
                    }
                });
                // Launch-completing action: drop the session cache
                // so the next show is a fresh launcher.
                // ReplaceQuery keeps the launcher visible and is
                // mid-session, so it must NOT clear.
                if should_hide {
                    if controller.preview_mode_active() {
                        crate::ipc::send_preview_hide_request();
                        controller.set_preview_mode_active(false);
                    }
                    controller.clear_and_hide();
                }
                glib::signal::Propagation::Stop
            } else if accel_matches(&keybindings.copy, key, state) {
                // Selected text in the entry wins: Ctrl+C with a live
                // selection is native text copy, not hit copy. Without
                // this Proceed, copying selected query text was
                // impossible — the capture-phase dispatch always stole
                // the chord.
                if entry_has_focus(&entry, &window) && entry.selection_bounds().is_some() {
                    return glib::signal::Propagation::Proceed;
                }
                // Copy is treated as a completed action (the user
                // got what they wanted — a clipboard value), so
                // clear the session on hide. But copy itself does
                // not close the launcher today; wait for user's
                // next Escape/focus-loss, which will hit hide()
                // and persist the session again. That's the
                // current UX and this commit does not change it.
                selected_hit_in(&selection, &filter_model, |hit| {
                    let copied = copy_to_clipboard(hit);
                    // Make the invisible action visible.
                    status_bar.show_toast(&format!("Copied: {}", copied));
                });
                glib::signal::Propagation::Stop
            } else if accel_matches(&keybindings.quick_look, key, state)
                && !entry_has_focus(&entry, &window)
            {
                // Only trigger when focus is NOT in entry (i.e. list is focused)
                // so space can still be typed into the search field.
                controller.set_preview_mode_active(true);
                let monitor = current_monitor_connector(&window);
                selected_hit_in(&selection, &filter_model, |hit| {
                    send_preview_request(hit, monitor.clone());
                });
                // Re-assert keyboard focus on the search entry.
                // The preview window presents itself on a layer-
                // shell surface and some compositors hand it
                // keyboard focus even with KeyboardMode::OnDemand.
                // Grabbing focus here ensures arrow-key scrub is
                // delivered to the launcher, not the preview.
                entry.grab_focus();
                glib::signal::Propagation::Stop
            } else if accel_matches(&keybindings.reset_gui_position, key, state) {
                // Clears the saved per-monitor launcher position and
                // re-centers the window. Configurable via the
                // `reset_gui_position` keybinding (default `<Ctrl>0`).
                // Caveat for users picking a shifted-digit accel
                // (`<Ctrl><Shift>0`, etc): `gtk::accelerator_parse`
                // always returns the base keysym, but most keyboard
                // layouts emit a different keysym for shifted digits
                // (US: `Shift+0` → `parenright`), so such accels will
                // silently never fire. Prefer unshifted or alpha keys.
                use gtk4_layer_shell::LayerShell;
                let connector = gtk::gdk::Display::default()
                    .and_then(|d| d.monitors().item(0).and_downcast::<gtk::gdk::Monitor>())
                    .and_then(|m| m.connector())
                    .map(|gs| gs.to_string());
                crate::launcher_position::clear(connector.as_deref());
                window.set_anchor(gtk4_layer_shell::Edge::Left, false);
                window.set_margin(
                    gtk4_layer_shell::Edge::Top,
                    crate::window::DEFAULT_TOP_MARGIN,
                );
                window.set_margin(gtk4_layer_shell::Edge::Left, 0);
                {
                    let w = window.clone();
                    glib::idle_add_local_once(move || {
                        crate::window::report_launcher_geometry(&w);
                    });
                }
                glib::signal::Propagation::Stop
            } else if accel_matches(&keybindings.filter_all, key, state) {
                chips.activate_index(0);
                glib::signal::Propagation::Stop
            } else if accel_matches(&keybindings.filter_apps, key, state) {
                chips.activate_index(1);
                glib::signal::Propagation::Stop
            } else if accel_matches(&keybindings.filter_files, key, state) {
                chips.activate_index(2);
                glib::signal::Propagation::Stop
            } else if accel_matches(&keybindings.filter_mail, key, state) {
                chips.activate_index(3);
                glib::signal::Propagation::Stop
            } else if accel_matches(&keybindings.filter_attachments, key, state) {
                chips.activate_index(4);
                glib::signal::Propagation::Stop
            } else {
                glib::signal::Propagation::Proceed
            }
        }
    ));
    window.add_controller(key_controller);

    // Entry-level key controller for history navigation when entry is focused and empty.
    // Capture phase so we run BEFORE GtkText's built-in Up/Down handler (which
    // in a single-line GtkEntry is a no-op but still stops propagation,
    // preventing a default Bubble-phase controller from ever seeing the key).
    let entry_key_controller = gtk::EventControllerKey::new();
    entry_key_controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    entry_key_controller.connect_key_pressed(clone!(
        #[strong]
        entry,
        #[strong]
        selection,
        #[strong]
        model,
        #[strong]
        list_view,
        #[strong]
        status_bar,
        #[strong]
        keybindings,
        #[strong]
        scrolled,
        #[strong]
        chips_container,
        move |_, key, _keycode, state| {
            tracing::info!("gui: ENTRY key_controller fired key={:?}", key.name());
            if accel_matches(&keybindings.history_up, key, state) && entry.text().is_empty() {
                // K8b: the history round-trip used to block the GTK
                // main thread inside this key handler for up to its
                // 500 ms socket timeout. Route it through the same
                // worker-thread + main-loop-callback pattern as the
                // search path; the callback re-checks that the entry
                // is still empty so a late reply never clobbers a
                // query the user started typing meanwhile.
                let entry = entry.clone();
                let model = model.clone();
                let selection = selection.clone();
                let list_view = list_view.clone();
                let status_bar = std::rc::Rc::clone(&status_bar);
                let chips_container = chips_container.clone();
                let scrolled = scrolled.clone();
                fetch_search_history_async(10, move |queries| {
                    if queries.is_empty() || !entry.text().is_empty() {
                        return;
                    }
                    let hits = synthetic_history_hits(&queries);
                    update_results(&model, &selection, &hits, None);
                    selection.set_selected(0);
                    list_view.scroll_to(0, gtk::ListScrollFlags::NONE, None);
                    status_bar.hide();
                    // The list_view/scrolled are hidden by default on
                    // empty-entry state (window.rs:455). Force them
                    // visible here, otherwise the synthetic history
                    // hits are loaded into the model silently and the
                    // user sees nothing.
                    chips_container.set_visible(true);
                    scrolled.set_visible(true);
                    scrolled.set_vexpand(false);
                });
                glib::signal::Propagation::Stop
            } else {
                glib::signal::Propagation::Proceed
            }
        }
    ));
    entry.add_controller(entry_key_controller);
}

#[cfg(test)]
mod tests {
    use super::{accel_matches, mods_match_exact};
    use gtk::gdk::{Key, ModifierType};

    #[test]
    fn mods_match_exact_requires_equality_not_containment() {
        // A bare binding must NOT match while Ctrl is held.
        assert!(!mods_match_exact(
            ModifierType::empty(),
            ModifierType::CONTROL_MASK
        ));
        // The exact chord matches.
        assert!(mods_match_exact(
            ModifierType::CONTROL_MASK,
            ModifierType::CONTROL_MASK
        ));
        // An extra Shift on a Ctrl chord must not match.
        assert!(!mods_match_exact(
            ModifierType::CONTROL_MASK,
            ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK
        ));
        // Lock/pointer bits outside the dispatch mask are ignored.
        assert!(mods_match_exact(
            ModifierType::empty(),
            ModifierType::LOCK_MASK
        ));
    }

    /// All `gtk::accelerator_parse`-backed assertions live in ONE
    /// test: `gtk::init()` pins "the GTK main thread" to whichever
    /// thread runs it first and the libtest harness gives every test
    /// its own thread, so two such tests in parallel would trip the
    /// gtk4-rs main-thread assertion. Headless environments (no
    /// display) skip silently.
    #[test]
    fn accel_matches_is_exact_on_modifiers() {
        if gtk::init().is_err() {
            eprintln!("skipping accel_matches_is_exact_on_modifiers: no display");
            return;
        }
        // Bare "Down" must no longer match Ctrl+Down — that chord
        // belongs to `next_category`.
        assert!(accel_matches("Down", Key::Down, ModifierType::empty()));
        assert!(!accel_matches("Down", Key::Down, ModifierType::CONTROL_MASK));
        assert!(accel_matches(
            "<Ctrl>Down",
            Key::Down,
            ModifierType::CONTROL_MASK
        ));
        // Shift+Enter secondary must still match "<Shift>Return".
        assert!(accel_matches(
            "<Shift>Return",
            Key::Return,
            ModifierType::SHIFT_MASK
        ));
        assert!(!accel_matches("<Shift>Return", Key::Return, ModifierType::empty()));
        // Bare "space" quick_look must not fire on Ctrl+space so it
        // cannot shadow quick_look_alt.
        assert!(accel_matches("space", Key::space, ModifierType::empty()));
        assert!(!accel_matches("space", Key::space, ModifierType::CONTROL_MASK));
        assert!(accel_matches(
            "<Ctrl>space",
            Key::space,
            ModifierType::CONTROL_MASK
        ));
        // CapsLock must not break plain bindings.
        assert!(accel_matches("Return", Key::Return, ModifierType::LOCK_MASK));
        // Unparseable accels never match.
        assert!(!accel_matches("<Bogus>x", Key::x, ModifierType::empty()));
    }
}
