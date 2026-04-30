//! ListView factory, result model updater, and cached hit store.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gtk::gdk;
use gtk::gio;
use gtk::prelude::*;
use lixun_core::{Action, Category, Hit, RowMenuDef, RowMenuVerb, RowMenuVisibility};

/// Per-row state carried by every persistent controller created
/// in `connect_setup`. `doc_id` is the DocId of the Hit currently
/// bound to this pool-slot row widget; `None` between `unbind`
/// and the next `bind`. Callbacks fired on an unbound row (late
/// gestures after scroll recycle, stale drag-prepares) must
/// no-op by checking `doc_id.is_none()`. `menu_key` mirrors the
/// source_instance of the currently-bound hit so `on_item_notify`
/// can skip `set_menu_model` when it already matches (see
/// MENU_CACHE below — this is the leak fix for 59.77 MB of
/// popover-menu churn heaptrack attributed to per-bind
/// gtk_popover_menu_set_menu_model).
#[derive(Default)]
struct RowState {
    doc_id: Option<String>,
    menu_key: Option<String>,
}

use crate::actions::{copy_to_clipboard, execute_action, execute_secondary_action};
use crate::icons::{category_fallback, resolve_icon};
use crate::ipc::dispatch_click_pair;
use crate::ipc::send_preview_request;

pub(crate) const ICON_SIZE_NORMAL: i32 = 32;
pub(crate) const ICON_SIZE_TOP_HIT: i32 = 48;

pub(crate) fn add_css_class<W: gtk::prelude::WidgetExt>(widget: &W, class: &str) {
    widget.add_css_class(class);
}

fn category_kind_fallback(cat: &Category) -> &'static str {
    match cat {
        Category::App => "Application",
        Category::File => "File",
        Category::Mail => "Email",
        Category::Attachment => "Attachment",
        Category::Calculator => "Calculator",
        Category::Shell => "Shell",
    }
}

thread_local! {
    static CACHED_HITS: RefCell<Vec<Hit>> = const { RefCell::new(Vec::new()) };
    static TOP_HIT_DOC_ID: RefCell<Option<String>> = const { RefCell::new(None) };
}

pub(crate) fn cache_hits(hits: Vec<Hit>) {
    CACHED_HITS.with(|c| *c.borrow_mut() = hits);
}

pub(crate) fn with_cached_hits<R>(f: impl FnOnce(&[Hit]) -> R) -> R {
    CACHED_HITS.with(|c| f(&c.borrow()))
}

/// Look up a cached Hit by doc id and return a clone so callers
/// can execute side effects without holding the `CACHED_HITS`
/// borrow. Action callbacks that activate app actions (e.g.
/// `clear-and-hide-launcher`) trigger a chain that re-enters
/// `clear_cached_hits()` → `borrow_mut()`, which would panic on
/// a live read borrow. Clone-out avoids re-entrancy; the cost is
/// one `Hit::clone` per click event (a handful per second at
/// most) instead of per-bind (hundreds during scrolling).
pub(crate) fn cached_hit_by_id(doc_id: &str) -> Option<Hit> {
    CACHED_HITS.with(|c| c.borrow().iter().find(|h| h.id.0 == doc_id).cloned())
}

pub(crate) fn clear_cached_hits() {
    CACHED_HITS.with(|c| c.borrow_mut().clear());
    TOP_HIT_DOC_ID.with(|c| *c.borrow_mut() = None);
}

pub(crate) fn cache_top_hit_doc_id(id: Option<String>) {
    TOP_HIT_DOC_ID.with(|c| *c.borrow_mut() = id);
}

pub(crate) fn is_top_hit_doc(id: &str) -> bool {
    TOP_HIT_DOC_ID.with(|c| c.borrow().as_deref() == Some(id))
}

/// Replace every row in `model` with rows derived from `hits`.
/// Disables `selection.autoselect` for the duration of the churn so
/// SingleSelection's interpolation formula (gtksingleselection.c
/// line 253-296) cannot drift the cursor on the per-row
/// items-changed emissions; callers are expected to set the
/// desired selection index themselves after this function returns,
/// or to pin it via `set_selected(INVALID_LIST_POSITION)` if they
/// want a blank state.
pub(crate) fn update_results(
    model: &gtk::StringList,
    selection: &gtk::SingleSelection,
    hits: &[Hit],
    top_hit_doc_id: Option<String>,
) {
    let prev_autoselect = selection.is_autoselect();
    selection.set_autoselect(false);

    let n = model.n_items();
    for _ in 0..n {
        model.remove(0);
    }
    cache_hits(hits.to_vec());
    cache_top_hit_doc_id(top_hit_doc_id);
    for hit in hits {
        model.append(&hit.id.0);
    }

    selection.set_autoselect(prev_autoselect);
}

/// Merge `new_hits` into `model` keyed by stable Hit identity
/// `(source_instance, doc_id)`. Rows already in `model` whose key
/// is also in `new_hits` are kept in place. Rows in `model` whose
/// key is no longer in `new_hits` are removed. Rows in `new_hits`
/// not yet in `model` are appended. The CACHED_HITS store is
/// updated to reflect the merged set so row factories can render
/// against the latest snapshot. Selection is restored by doc_id
/// when possible.
pub(crate) fn update_results_merge(
    model: &gtk::StringList,
    selection: &gtk::SingleSelection,
    new_hits: &[Hit],
    top_hit_doc_id: Option<String>,
) {
    let prev_autoselect = selection.is_autoselect();
    selection.set_autoselect(false);

    let prior_selected_doc_id: Option<String> = {
        let idx = selection.selected();
        selection.item(idx).and_then(|obj| {
            obj.downcast::<gtk::StringObject>()
                .ok()
                .map(|s| s.string().to_string())
        })
    };

    let new_keys: std::collections::HashSet<(String, String)> = new_hits
        .iter()
        .map(|h| (h.source_instance.clone(), h.id.0.clone()))
        .collect();

    let prior_hits: Vec<Hit> = with_cached_hits(|h| h.to_vec());
    let prior_doc_to_key: std::collections::HashMap<String, (String, String)> = prior_hits
        .iter()
        .map(|h| (h.id.0.clone(), (h.source_instance.clone(), h.id.0.clone())))
        .collect();

    let mut existing_doc_ids: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    let mut i = 0;
    while i < model.n_items() {
        let Some(obj) = model.item(i) else {
            i += 1;
            continue;
        };
        let Some(str_obj) = obj.downcast::<gtk::StringObject>().ok() else {
            i += 1;
            continue;
        };
        let doc_id = str_obj.string().to_string();
        let key = prior_doc_to_key
            .get(&doc_id)
            .cloned()
            .unwrap_or_else(|| (String::new(), doc_id.clone()));
        if new_keys.contains(&key) {
            existing_doc_ids.insert(doc_id);
            i += 1;
        } else {
            model.remove(i);
        }
    }

    for hit in new_hits {
        if !existing_doc_ids.contains(&hit.id.0) {
            model.append(&hit.id.0);
        }
    }

    cache_hits(new_hits.to_vec());
    cache_top_hit_doc_id(top_hit_doc_id);

    if let Some(want) = prior_selected_doc_id {
        let n = model.n_items();
        for idx in 0..n {
            if let Some(obj) = model.item(idx)
                && let Some(str_obj) = obj.downcast::<gtk::StringObject>().ok()
                && str_obj.string() == want
            {
                selection.set_selected(idx);
                break;
            }
        }
    }

    selection.set_autoselect(prev_autoselect);
}

pub(crate) fn synthetic_history_hits(queries: &[String]) -> Vec<Hit> {
    use lixun_core::{Action, DocId};
    queries
        .iter()
        .enumerate()
        .map(|(i, q)| Hit {
            id: DocId(format!("history:{i}:{q}")),
            category: Category::File,
            title: q.clone(),
            subtitle: "Recent search".to_string(),
            icon_name: Some("document-open-recent".to_string()),
            kind_label: Some("Recent".to_string()),
            score: 0.0,
            action: Action::ReplaceQuery { q: q.clone() },
            extract_fail: false,
            sender: None,
            recipients: None,
            body: None,
            secondary_action: None,
            source_instance: String::new(),
            row_menu: lixun_core::RowMenuDef::empty(),
            mime: None,
        })
        .collect()
}

thread_local! {
    static MENU_CACHE: RefCell<HashMap<String, gio::Menu>> =
        RefCell::new(HashMap::new());
}

/// Translate a plugin-authored `RowMenuDef` into a GTK `gio::Menu`,
/// caching the result by `menu_key` (== hit.source_instance) so
/// subsequent row binds that belong to the same source reuse the
/// exact same menu object. This is the load-bearing half of the
/// leak fix: `gtk_popover_menu_set_menu_model` retains its argument
/// inside GTK's object graph; swapping it per-bind accumulates
/// orphan menu trees (heaptrack measured 59.77 MB leaked / 1.46M
/// g_malloc0 calls). With this cache the popover sees at most
/// ~7 unique menu objects per session (one per source_instance),
/// and `on_item_notify` only calls `set_menu_model` when the
/// row's menu_key actually changes.
///
/// The cache is never evicted: key space equals the small, bounded
/// set of registered source instances (current installs: 7). This
/// also keeps the function honouring AGENTS.md invariant #1 — the
/// host never names a plugin here; it iterates whatever verbs the
/// plugin's `row_menu()` declared and maps them to the fixed
/// action vocabulary below.
fn menu_for_key(key: &str, def: &RowMenuDef) -> gio::Menu {
    MENU_CACHE.with(|cache| {
        if let Some(existing) = cache.borrow().get(key) {
            return existing.clone();
        }
        let menu = gio::Menu::new();
        for item in &def.items {
            let action = match item.verb {
                RowMenuVerb::Open => "row.open",
                RowMenuVerb::Secondary => "row.secondary",
                RowMenuVerb::Copy => "row.copy",
                RowMenuVerb::QuickLook => "row.quicklook",
                RowMenuVerb::Info => "row.info",
            };
            menu.append(Some(&item.label), Some(action));
        }
        cache.borrow_mut().insert(key.to_string(), menu.clone());
        menu
    })
}

/// Does the source's menu expose a conditionally-enabled Secondary
/// item? Used in `on_item_notify` to decide whether `row.secondary`
/// should follow `hit.secondary_action.is_some()` or stay permanently
/// enabled (e.g. Fs.Reveal which is always valid).
fn menu_has_conditional_secondary(def: &RowMenuDef) -> bool {
    def.items.iter().any(|it| {
        it.verb == RowMenuVerb::Secondary
            && it.visibility == RowMenuVisibility::RequiresSecondaryAction
    })
}

fn hit_file_path(hit: &Hit) -> Option<std::path::PathBuf> {
    match &hit.action {
        Action::OpenFile { path } | Action::ShowInFileManager { path } => Some(path.clone()),
        _ => None,
    }
}

fn populate_info_popover_body(vbox: &gtk::Box, hit: &Hit) {
    while let Some(child) = vbox.first_child() {
        vbox.remove(&child);
    }
    let title = gtk::Label::new(Some(&hit.title));
    title.set_xalign(0.0);
    add_css_class(&title, "lixun-title");
    vbox.append(&title);

    let path_label = gtk::Label::new(Some(&hit.subtitle));
    path_label.set_xalign(0.0);
    path_label.set_selectable(true);
    path_label.set_wrap(true);
    add_css_class(&path_label, "lixun-subtitle");
    vbox.append(&path_label);

    if let Some(kind) = hit.kind_label.as_deref() {
        let kind_label = gtk::Label::new(Some(&format!("Kind: {}", kind)));
        kind_label.set_xalign(0.0);
        add_css_class(&kind_label, "lixun-subtitle");
        vbox.append(&kind_label);
    }
}

pub(crate) fn create_list_factory(entry: gtk::Entry) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    let setup_entry = entry.clone();

    factory.connect_setup(move |_, list_item| {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        row.set_widget_name("lixun-hit");
        add_css_class(&row, "lixun-hit");

        let icon = gtk::Image::new();
        icon.set_pixel_size(ICON_SIZE_NORMAL);
        icon.set_margin_start(4);
        row.append(&icon);

        let text_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
        text_box.set_hexpand(true);

        let title = gtk::Label::new(None);
        title.set_xalign(0.0);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        add_css_class(&title, "lixun-title");
        text_box.append(&title);

        let subtitle = gtk::Label::new(None);
        subtitle.set_xalign(0.0);
        subtitle.set_ellipsize(gtk::pango::EllipsizeMode::End);
        add_css_class(&subtitle, "lixun-subtitle");
        text_box.append(&subtitle);

        row.append(&text_box);

        let kind = gtk::Label::new(None);
        kind.set_xalign(1.0);
        add_css_class(&kind, "lixun-kind");
        row.append(&kind);

        let list_item = list_item
            .downcast_ref::<gtk::ListItem>()
            .expect("ListItem expected");
        list_item.set_child(Some(&row));

        // Per-row state shared by every persistent controller
        // installed below. `connect_bind` writes doc_id+category
        // into this cell; `connect_unbind` clears them. Every
        // callback guards on `doc_id.is_none()` and no-ops for
        // late/stale firings (post-recycle gesture, stale drag
        // prepare). No Hit is ever captured in closures — the
        // current Hit is resolved through `with_cached_hits` at
        // callback time, eliminating the bind-time Hit::clone
        // hotspot that previously leaked ~293 MB per session.
        let state = Rc::new(RefCell::new(RowState::default()));

        // ===== SimpleActionGroup with five "row.*" actions =====
        // Built once per pool slot, inserted once. GIO holds a
        // strong ref from the row; we never reinstall, so no
        // orphan ActionGroup can accumulate.
        let group = gio::SimpleActionGroup::new();

        let open_state = Rc::clone(&state);
        let open_entry = setup_entry.clone();
        let open = gio::SimpleAction::new("open", None);
        open.connect_activate(move |_, _| {
            let Some(doc_id) = open_state.borrow().doc_id.clone() else {
                tracing::debug!("row.open fired on unbound row");
                return;
            };
            if let Some(hit) = cached_hit_by_id(&doc_id) {
                dispatch_click_pair(&hit.id.0, open_entry.text().as_str());
                if let Err(e) = execute_action(&hit) {
                    tracing::error!("Action failed: {}", e);
                }
            }
        });
        group.add_action(&open);

        let secondary_state = Rc::clone(&state);
        let secondary = gio::SimpleAction::new("secondary", None);
        secondary.connect_activate(move |_, _| {
            let Some(doc_id) = secondary_state.borrow().doc_id.clone() else {
                tracing::debug!("row.secondary fired on unbound row");
                return;
            };
            if let Some(hit) = cached_hit_by_id(&doc_id)
                && let Err(e) = execute_secondary_action(&hit)
            {
                tracing::error!("Secondary action failed: {}", e);
            }
        });
        group.add_action(&secondary);

        let copy_state = Rc::clone(&state);
        let copy = gio::SimpleAction::new("copy", None);
        copy.connect_activate(move |_, _| {
            let Some(doc_id) = copy_state.borrow().doc_id.clone() else {
                tracing::debug!("row.copy fired on unbound row");
                return;
            };
            if let Some(hit) = cached_hit_by_id(&doc_id) {
                copy_to_clipboard(&hit);
            }
        });
        group.add_action(&copy);

        let quick_state = Rc::clone(&state);
        let quick_row = row.clone();
        let quick = gio::SimpleAction::new("quicklook", None);
        quick.connect_activate(move |_, _| {
            let Some(doc_id) = quick_state.borrow().doc_id.clone() else {
                tracing::debug!("row.quicklook fired on unbound row");
                return;
            };
            if let Some(hit) = cached_hit_by_id(&doc_id) {
                let monitor = quick_row
                    .root()
                    .and_then(|r| r.downcast::<gtk::ApplicationWindow>().ok())
                    .and_then(|w| crate::ipc::current_monitor_connector(&w));
                send_preview_request(&hit, monitor);
            }
        });
        group.add_action(&quick);

        // ===== Info popover (persistent child of the row) =====
        // Parented once via set_parent(&row). popdown() hides it;
        // we NEVER call unparent() — that would crash on the next
        // activate. Row widgets are pool-stable (never destroyed
        // during GUI lifetime), so this popover lives as long as
        // its row.
        let info_popover = gtk::Popover::new();
        info_popover.set_parent(&row);
        info_popover.set_has_arrow(true);
        let info_vbox = gtk::Box::new(gtk::Orientation::Vertical, 4);
        info_vbox.set_margin_top(8);
        info_vbox.set_margin_bottom(8);
        info_vbox.set_margin_start(12);
        info_vbox.set_margin_end(12);
        info_popover.set_child(Some(&info_vbox));

        let info_state = Rc::clone(&state);
        let info_popover_for_action = info_popover.clone();
        let info_vbox_for_action = info_vbox.clone();
        let info = gio::SimpleAction::new("info", None);
        info.connect_activate(move |_, _| {
            let Some(doc_id) = info_state.borrow().doc_id.clone() else {
                tracing::debug!("row.info fired on unbound row");
                return;
            };
            if let Some(hit) = cached_hit_by_id(&doc_id) {
                populate_info_popover_body(&info_vbox_for_action, &hit);
                info_popover_for_action.popup();
            }
        });
        group.add_action(&info);

        row.insert_action_group("row", Some(&group));

        // ===== Right-click popover (persistent, menu model swapped on bind) =====
        // PopoverMenu is built once with an empty menu; `on_item_notify`
        // later calls `set_menu_model` only when the bound hit's
        // `source_instance` differs from the previously-bound one
        // (see MENU_CACHE). Parented once; never rebuilt.
        let right_click_popover = gtk::PopoverMenu::from_model(Some(&gio::Menu::new()));
        right_click_popover.set_parent(&row);
        right_click_popover.set_has_arrow(false);

        let right_click_gesture = gtk::GestureClick::new();
        right_click_gesture.set_button(gdk::BUTTON_SECONDARY);
        let right_click_popover_for_gesture = right_click_popover.clone();
        right_click_gesture.connect_pressed(move |_g, _n_press, x, y| {
            let rect = gdk::Rectangle::new(x as i32, y as i32, 1, 1);
            right_click_popover_for_gesture.set_pointing_to(Some(&rect));
            right_click_popover_for_gesture.popup();
        });
        row.add_controller(right_click_gesture);

        // ===== Double-click primary = launch + clear-and-hide =====
        let dblclick_state = Rc::clone(&state);
        let dblclick_entry = setup_entry.clone();
        let dblclick_gesture = gtk::GestureClick::new();
        dblclick_gesture.set_button(gdk::BUTTON_PRIMARY);
        dblclick_gesture.connect_pressed(move |_g, n_press, _x, _y| {
            if n_press != 2 {
                return;
            }
            let Some(doc_id) = dblclick_state.borrow().doc_id.clone() else {
                tracing::debug!("double-click fired on unbound row");
                return;
            };
            if let Some(hit) = cached_hit_by_id(&doc_id) {
                dispatch_click_pair(&hit.id.0, dblclick_entry.text().as_str());
                if let Err(e) = execute_action(&hit) {
                    tracing::error!("double-click open failed: {}", e);
                    return;
                }
                // Double-click = launch-completing action;
                // drop the launcher session cache via the
                // "clear-and-hide-launcher" app action. Safe
                // now because we're no longer inside a
                // CACHED_HITS borrow (cached_hit_by_id drops
                // it before returning).
                if let Some(app) = gio::Application::default() {
                    app.activate_action("clear-and-hide-launcher", None);
                }
            }
        });
        row.add_controller(dblclick_gesture);

        // ===== Drag source (permanent row controller) =====
        // For non-file rows, connect_prepare returns None, which
        // GTK4 silently aborts before any drag cursor or visual
        // feedback — same UX as the old "install only for
        // File/Attachment" code path. The key difference is no
        // per-bind add_controller churn.
        //
        // For file rows we hand GTK4 a GdkFileList wrapped in a
        // ContentProvider. GdkFileList registers the provider under
        // both `text/uri-list` and `application/vnd.portal.filetransfer`,
        // which is what Nautilus / Dolphin / Thunar / Files accept.
        // Passing a bare GString URI instead (as we used to) resolves
        // to `text/plain`, which every file manager silently rejects.
        let drag_state = Rc::clone(&state);
        let drag = gtk::DragSource::new();
        drag.set_actions(gdk::DragAction::COPY);
        drag.connect_prepare(move |source, _x, _y| {
            let doc_id = drag_state.borrow().doc_id.clone()?;
            let hit = cached_hit_by_id(&doc_id)?;
            let path = hit_file_path(&hit)?;
            let file = gio::File::for_path(&path);
            let file_list = gdk::FileList::from_array(&[file]);
            let content = gdk::ContentProvider::for_value(&file_list.to_value());
            if let Some(paintable) = resolve_icon(&hit, ICON_SIZE_NORMAL) {
                source.set_icon(Some(&paintable), 0, 0);
            }
            Some(content)
        });
        row.add_controller(drag);

        // Per-setup bind/unbind handlers via item-property notify.
        // Each pool-slot list_item carries a unique closure that
        // captures ITS OWN `state` Rc (so the controllers above
        // see state mutations) and ITS OWN right-click popover
        // (so the menu model swap targets the right widget).
        // This avoids the need for `unsafe { set_data }` plumbing
        // a shared factory-level connect_bind handler would have
        // required, at the cost of one extra closure per row.
        let notify_state = Rc::clone(&state);
        let notify_right_click_popover = right_click_popover.clone();
        let notify_secondary = secondary.clone();
        list_item.connect_notify_local(Some("item"), move |list_item, _| {
            on_item_notify(
                list_item,
                &notify_state,
                &notify_right_click_popover,
                &notify_secondary,
            );
        });

        // Re-apply hero styling (large icon + card frame) whenever
        // this item's selected state flips. GTK4 ListView recycles
        // child widgets across list_items as the user scrolls; the
        // notify handler is attached to the concrete ListItem, so it
        // fires correctly for whichever item currently owns this
        // row widget. Combined with the connect_unbind reset below,
        // this prevents stale `.lixun-top-hit` classes from carrying
        // over to rows that are no longer selected.
        list_item.connect_selected_notify(|list_item| {
            apply_selected_styling(list_item);
        });
    });

    // Reset both selection-cursor and top-hit-hero styling when a
    // row widget is returned to the pool for recycling. Without
    // this a row that was decorated at unbind time would retain
    // its CSS classes on reuse for a different item, producing a
    // ghost highlight.
    factory.connect_unbind(|_, list_item| {
        let list_item = list_item
            .downcast_ref::<gtk::ListItem>()
            .expect("ListItem expected");
        if let Some(row) = list_item.child().and_downcast::<gtk::Box>() {
            row.remove_css_class("lixun-top-hit");
            row.remove_css_class("lixun-top-hit-hero");
            if let Some(icon) = row.first_child().and_downcast::<gtk::Image>() {
                icon.set_pixel_size(ICON_SIZE_NORMAL);
            }
        }
        // Row state is cleared via the `item` notify below when
        // item becomes None. Nothing to do here for row state.
    });

    factory.connect_bind(move |_, list_item| {
        let list_item = list_item
            .downcast_ref::<gtk::ListItem>()
            .expect("ListItem expected");
        apply_selected_styling(list_item);
        apply_top_hit_styling(list_item);
    });

    factory
}

/// Called from `connect_notify_local("item", ...)` on each list
/// item: fires when the item is bound (item becomes Some) and
/// unbound (item becomes None). Updates the row's labels, icon
/// hint, shared RowState, and the right-click popover's menu
/// model to match the newly-bound Hit. On unbind (item is None)
/// clears the RowState so subsequent callbacks no-op safely.
fn on_item_notify(
    list_item: &gtk::ListItem,
    state: &Rc<RefCell<RowState>>,
    right_click_popover: &gtk::PopoverMenu,
    secondary: &gio::SimpleAction,
) {
    let Some(row) = list_item.child().and_downcast::<gtk::Box>() else {
        return;
    };

    let Some(str_obj) = list_item
        .item()
        .and_then(|i| i.downcast::<gtk::StringObject>().ok())
    else {
        // Item cleared — row is unbound. Reset RowState so stale
        // callbacks no-op, and disable the secondary action so a
        // recycled row does not show stale "Open parent mail"
        // availability before its next bind writes the correct
        // state.
        let mut s = state.borrow_mut();
        s.doc_id = None;
        s.menu_key = None;
        secondary.set_enabled(false);
        return;
    };

    let doc_id = str_obj.string().to_string();
    with_cached_hits(|hits| {
        if let Some(hit) = hits.iter().find(|h| h.id.0 == doc_id) {
            let text_box = row
                .first_child()
                .and_then(|c| c.next_sibling())
                .and_downcast::<gtk::Box>()
                .expect("text_box");
            let title = text_box
                .first_child()
                .and_downcast::<gtk::Label>()
                .expect("title");
            title.set_text(&hit.title);

            let subtitle = title
                .next_sibling()
                .and_downcast::<gtk::Label>()
                .expect("subtitle");
            subtitle.set_text(&hit.subtitle);

            let kind = text_box
                .next_sibling()
                .and_downcast::<gtk::Label>()
                .expect("kind");
            let kind_text = hit
                .kind_label
                .clone()
                .unwrap_or_else(|| category_kind_fallback(&hit.category).to_string());
            kind.set_text(&kind_text);

            // Cache-aware menu swap: only call `set_menu_model`
            // when this row's menu_key differs from the new hit's
            // source_instance. Combined with the per-source
            // `MENU_CACHE`, this bounds `set_menu_model` calls to
            // O(unique source_instances) per process instead of
            // O(binds), which fixes the PopoverMenu retention
            // leak measured by heaptrack on the old code path.
            let current_key = state.borrow().menu_key.clone();
            let new_key = Some(hit.source_instance.clone());
            if current_key != new_key {
                let menu = menu_for_key(&hit.source_instance, &hit.row_menu);
                right_click_popover.set_menu_model(Some(&menu));
            }

            // Conditional-secondary items live in a shared menu
            // model cached per source_instance; visibility of
            // "Open parent mail" (and any future
            // `RequiresSecondaryAction` item) is expressed via
            // GAction enabled state, not by rebuilding the menu.
            if menu_has_conditional_secondary(&hit.row_menu) {
                secondary.set_enabled(hit.secondary_action.is_some());
            } else {
                // Source does not use conditional secondary; keep
                // the action enabled so sources that expose an
                // unconditional "Secondary" verb (e.g. fs reveal)
                // still work.
                secondary.set_enabled(true);
            }

            let mut s = state.borrow_mut();
            s.doc_id = Some(doc_id);
            s.menu_key = new_key;
        }
    });
}

/// Apply the stateful selection-cursor class `.lixun-top-hit` to
/// the row iff the list item is currently selected. Called on
/// initial bind and on every selection-change so the cursor
/// highlight follows the user's arrow-key input. Icon size and
/// paintable are owned by `apply_top_hit_styling`, not this
/// function.
fn apply_selected_styling(list_item: &gtk::ListItem) {
    let Some(row) = list_item.child().and_downcast::<gtk::Box>() else {
        return;
    };
    if list_item.is_selected() {
        row.add_css_class("lixun-top-hit");
    } else {
        row.remove_css_class("lixun-top-hit");
    }
}

/// Apply the structural hero class `.lixun-top-hit-hero` to the
/// row iff its DocId matches the top-hit id nominated by the
/// daemon for the current response. Owns icon size and paintable
/// (large icon for top hit, normal for the rest). Independent of
/// selection state, so the hero decoration stays on row 0 even
/// when the user moves the cursor with arrow keys.
fn apply_top_hit_styling(list_item: &gtk::ListItem) {
    let Some(row) = list_item.child().and_downcast::<gtk::Box>() else {
        return;
    };
    let Some(icon) = row.first_child().and_downcast::<gtk::Image>() else {
        return;
    };
    let Some(str_obj) = list_item
        .item()
        .and_then(|i| i.downcast::<gtk::StringObject>().ok())
    else {
        return;
    };
    let doc_id = str_obj.string().to_string();
    let is_top_hit = is_top_hit_doc(&doc_id);
    if is_top_hit {
        row.add_css_class("lixun-top-hit-hero");
    } else {
        row.remove_css_class("lixun-top-hit-hero");
    }
    let icon_size = if is_top_hit {
        ICON_SIZE_TOP_HIT
    } else {
        ICON_SIZE_NORMAL
    };
    icon.set_pixel_size(icon_size);
    with_cached_hits(|hits| {
        if let Some(hit) = hits.iter().find(|h| h.id.0 == doc_id) {
            if let Some(paintable) = resolve_icon(hit, icon_size) {
                icon.set_paintable(Some(&paintable));
            } else {
                icon.set_icon_name(Some(category_fallback(&hit.category)));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    use lixun_core::{RowMenuItem, RowMenuVerb};

    fn sample_def_single(verb: RowMenuVerb, label: &str) -> RowMenuDef {
        RowMenuDef {
            items: vec![RowMenuItem {
                label: label.to_string(),
                verb,
                visibility: Default::default(),
            }],
        }
    }

    #[test]
    fn row_state_default_is_unbound() {
        let s = RowState::default();
        assert!(s.doc_id.is_none());
        assert!(s.menu_key.is_none());
    }

    #[test]
    fn menu_for_key_caches_by_key() {
        gtk::init().ok();
        MENU_CACHE.with(|c| c.borrow_mut().clear());
        let key = "test.key.cache";
        let def = sample_def_single(RowMenuVerb::Open, "Open");
        let a = menu_for_key(key, &def);
        let b = menu_for_key(key, &def);
        assert_eq!(a.n_items(), 1);
        assert_eq!(b.n_items(), 1);
        assert!(MENU_CACHE.with(|c| c.borrow().contains_key(key)));
    }

    #[test]
    fn menu_for_key_separate_keys_separate_entries() {
        gtk::init().ok();
        MENU_CACHE.with(|c| c.borrow_mut().clear());
        let def = sample_def_single(RowMenuVerb::Copy, "Copy");
        let _ = menu_for_key("a.key", &def);
        let _ = menu_for_key("b.key", &def);
        let len = MENU_CACHE.with(|c| c.borrow().len());
        assert!(len >= 2);
    }

    #[test]
    fn menu_has_conditional_secondary_detects_flag() {
        let def = RowMenuDef {
            items: vec![
                RowMenuItem {
                    label: "Open".into(),
                    verb: RowMenuVerb::Open,
                    visibility: Default::default(),
                },
                RowMenuItem {
                    label: "Open parent mail".into(),
                    verb: RowMenuVerb::Secondary,
                    visibility: RowMenuVisibility::RequiresSecondaryAction,
                },
            ],
        };
        assert!(menu_has_conditional_secondary(&def));
    }

    #[test]
    fn menu_has_conditional_secondary_false_without_flag() {
        let def = RowMenuDef {
            items: vec![RowMenuItem {
                label: "Reveal".into(),
                verb: RowMenuVerb::Secondary,
                visibility: Default::default(),
            }],
        };
        assert!(!menu_has_conditional_secondary(&def));
    }

    #[test]
    fn top_hit_doc_id_roundtrip() {
        cache_top_hit_doc_id(Some("app:firefox".into()));
        assert!(is_top_hit_doc("app:firefox"));
        assert!(!is_top_hit_doc("app:chromium"));
        assert!(!is_top_hit_doc(""));
        cache_top_hit_doc_id(None);
        assert!(!is_top_hit_doc("app:firefox"));
    }
}
