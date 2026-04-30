//! Keyboard bindings for the launcher window.
//!
//! Centralizes all key handling so window.rs stays small. Extended in later
//! waves for category filters (Ctrl+1..4), category jumps (Ctrl+Down/Up),
//! Quick Look (Space), and history navigation (↑ in empty entry).

use glib::clone;
use gtk::prelude::*;
use lixun_core::Action;
use lixun_daemon::config::Keybindings;

use crate::actions::{copy_to_clipboard, execute_action, execute_secondary_action};
use crate::factory::{synthetic_history_hits, update_results, with_cached_hits};
use crate::ipc::{
    IpcClient, current_monitor_connector, dispatch_click_pair, request_search_history,
    send_preview_request,
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
        with_cached_hits(|hits| {
            if let Some(hit) = hits.iter().find(|h| h.id.0 == doc_id) {
                f(hit);
            }
        });
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

fn accel_matches(accel: &str, key: gtk::gdk::Key, state: gtk::gdk::ModifierType) -> bool {
    let Some((expected_key, expected_mods)) = gtk::accelerator_parse(accel) else {
        return false;
    };
    key == expected_key && state.contains(expected_mods)
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
        move |_, key, _keycode, state| {
            let entry_focus = entry_has_focus(&entry, &window);
            let printable = is_printable_key(key, state);
            tracing::info!(
                "gui: window key_controller fired key={:?} entry_focus={} printable={}",
                key.name(), entry_focus, printable
            );
            // Hard rule: printable unmodified keys belong to the focused
            // Entry. Forward them before any accel dispatch can swallow
            // them (e.g. bare-Space `quick_look` binding).
            if entry_has_focus(&entry, &window) && is_printable_key(key, state) {
                return glib::signal::Propagation::Proceed;
            }
            // BUG-5: focus has left the entry (list_view grabbed it on
            // Down, or user clicked a result row). A printable key that
            // is NOT a registered accel (quick_look=Space stays a
            // quick_look trigger) should warp back to the entry and
            // continue typing — that is the Raycast/Alfred contract.
            // Without this, once focus moves to the list the launcher
            // becomes a dead keyboard surface until Escape/Enter/click.
            if !entry_has_focus(&entry, &window)
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
                // Move the caret to the end after grab_focus (which by
                // default selects all text).
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
            if !entry_has_focus(&entry, &window)
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
            let ctrl = state.contains(gtk::gdk::ModifierType::CONTROL_MASK);
            let shift = state.contains(gtk::gdk::ModifierType::SHIFT_MASK);
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
                if ctrl {
                    jump_to_next_category(&selection, &filter_model, -1);
                    let target = selection.selected();
                    if target != gtk::INVALID_LIST_POSITION {
                        scroll_with_margin(&list_view, &selection, target, -1);
                        controller.mark_user_selected();
                    }
                    if entry_had_focus {
                        list_view.grab_focus();
                    }
                    return glib::signal::Propagation::Stop;
                }
                let current = selection.selected();
                if current > 0 {
                    let target = current - 1;
                    selection.set_selected(target);
                    controller.mark_user_selected();
                    scroll_with_margin(&list_view, &selection, target, -1);
                    if entry_had_focus {
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
                if ctrl {
                    jump_to_next_category(&selection, &filter_model, 1);
                    let target = selection.selected();
                    if target != gtk::INVALID_LIST_POSITION {
                        scroll_with_margin(&list_view, &selection, target, 1);
                        controller.mark_user_selected();
                    }
                    if entry_had_focus {
                        list_view.grab_focus();
                    }
                    return glib::signal::Propagation::Stop;
                }
                let current = selection.selected();
                let n = selection.n_items();
                if current + 1 < n {
                    let target = current + 1;
                    selection.set_selected(target);
                    controller.mark_user_selected();
                    scroll_with_margin(&list_view, &selection, target, 1);
                }
                if entry_had_focus && n > 0 {
                    list_view.grab_focus();
                }
                glib::signal::Propagation::Stop
            } else if accel_matches(&keybindings.close, key, state) {
                controller.hide();
                glib::signal::Propagation::Stop
            } else if accel_matches(&keybindings.primary_action, key, state)
                || accel_matches(&keybindings.secondary_action, key, state)
            {
                let mut should_hide = true;
                let query_at_click = entry.text().to_string();
                selected_hit_in(&selection, &filter_model, |hit| {
                    if let Action::ReplaceQuery { q } = &hit.action {
                        entry.set_text(q);
                        entry.set_position(-1);
                        entry.grab_focus();
                        should_hide = false;
                        return;
                    }
                    dispatch_click_pair(&hit.id.0, &query_at_click);
                    let result = if accel_matches(&keybindings.secondary_action, key, state)
                        || shift
                        || ctrl
                    {
                        execute_secondary_action(hit)
                    } else {
                        execute_action(hit)
                    };
                    if let Err(e) = result {
                        tracing::error!("Action failed: {}", e);
                    }
                });
                // Launch-completing action: drop the session cache
                // so the next show is a fresh launcher.
                // ReplaceQuery keeps the launcher visible and is
                // mid-session, so it must NOT clear.
                if should_hide {
                    controller.clear_and_hide();
                }
                glib::signal::Propagation::Stop
            } else if accel_matches(&keybindings.copy, key, state) {
                // Copy is treated as a completed action (the user
                // got what they wanted — a clipboard value), so
                // clear the session on hide. But copy itself does
                // not close the launcher today; wait for user's
                // next Escape/focus-loss, which will hit hide()
                // and persist the session again. That's the
                // current UX and this commit does not change it.
                selected_hit_in(&selection, &filter_model, copy_to_clipboard);
                glib::signal::Propagation::Stop
            } else if accel_matches(&keybindings.quick_look, key, state)
                && !entry_has_focus(&entry, &window)
            {
                // Only trigger when focus is NOT in entry (i.e. list is focused)
                // so space can still be typed into the search field.
                let monitor = current_monitor_connector(&window);
                selected_hit_in(&selection, &filter_model, |hit| {
                    send_preview_request(hit, monitor.clone());
                });
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
                let queries = request_search_history(10);
                if queries.is_empty() {
                    return glib::signal::Propagation::Stop;
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
                scrolled.set_vexpand(true);
                glib::signal::Propagation::Stop
            } else {
                glib::signal::Propagation::Proceed
            }
        }
    ));
    entry.add_controller(entry_key_controller);
}
