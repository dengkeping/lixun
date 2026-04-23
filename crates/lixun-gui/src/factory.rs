//! ListView factory, result model updater, and cached hit store.

use std::cell::RefCell;

use gtk::gdk;
use gtk::gio;
use gtk::prelude::*;
use lixun_core::{Action, Category, Hit};

/// Per-row state carried by every persistent controller created
/// in `connect_setup`. `doc_id` is the DocId of the Hit currently
/// bound to this pool-slot row widget; `None` between `unbind`
/// and the next `bind`. Callbacks fired on an unbound row (late
/// gestures after scroll recycle, stale drag-prepares) must
/// no-op by checking `doc_id.is_none()`. `category` is kept
/// alongside so the right-click popover can swap its menu model
/// without a second cache lookup on each click.
#[derive(Default)]
#[allow(dead_code)] // Wired up in T2 of .local-plans/plans/gui-memory-leak-fix.md
struct RowState {
    doc_id: Option<String>,
    category: Option<Category>,
}

use crate::actions::{copy_to_clipboard, execute_action, execute_secondary_action};
use crate::icons::resolve_icon;
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
        })
        .collect()
}

fn build_menu_for(category: &Category) -> gio::Menu {
    let menu = gio::Menu::new();
    match category {
        Category::App => {
            menu.append(Some("Launch"), Some("row.open"));
            menu.append(Some("Copy name"), Some("row.copy"));
        }
        Category::File => {
            menu.append(Some("Open"), Some("row.open"));
            menu.append(Some("Reveal in File Manager"), Some("row.reveal"));
            menu.append(Some("Copy path"), Some("row.copy"));
            menu.append(Some("Quick Look"), Some("row.quicklook"));
            menu.append(Some("Get Info"), Some("row.info"));
        }
        Category::Mail => {
            menu.append(Some("Open in Thunderbird"), Some("row.open"));
            menu.append(Some("Copy subject"), Some("row.copy"));
        }
        Category::Attachment => {
            menu.append(Some("Open"), Some("row.open"));
            menu.append(Some("Quick Look"), Some("row.quicklook"));
            menu.append(Some("Copy filename"), Some("row.copy"));
            menu.append(Some("Open parent mail"), Some("row.reveal"));
        }
    }
    menu
}

fn install_action_group(row: &gtk::Box, hit: &Hit, entry: &gtk::Entry) {
    let group = gio::SimpleActionGroup::new();

    let open_hit = hit.clone();
    let open_entry = entry.clone();
    let open = gio::SimpleAction::new("open", None);
    open.connect_activate(move |_, _| {
        dispatch_click_pair(&open_hit.id.0, open_entry.text().as_str());
        if let Err(e) = execute_action(&open_hit) {
            tracing::error!("Action failed: {}", e);
        }
    });
    group.add_action(&open);

    let reveal_hit = hit.clone();
    let reveal = gio::SimpleAction::new("reveal", None);
    reveal.connect_activate(move |_, _| {
        if let Err(e) = execute_secondary_action(&reveal_hit) {
            tracing::error!("Reveal failed: {}", e);
        }
    });
    group.add_action(&reveal);

    let copy_hit = hit.clone();
    let copy = gio::SimpleAction::new("copy", None);
    copy.connect_activate(move |_, _| {
        copy_to_clipboard(&copy_hit);
    });
    group.add_action(&copy);

    let quick_hit = hit.clone();
    let quick_row = row.clone();
    let quick = gio::SimpleAction::new("quicklook", None);
    quick.connect_activate(move |_, _| {
        let monitor = quick_row
            .root()
            .and_then(|r| r.downcast::<gtk::ApplicationWindow>().ok())
            .and_then(|w| crate::ipc::current_monitor_connector(&w));
        send_preview_request(&quick_hit, monitor);
    });
    group.add_action(&quick);

    let info_hit = hit.clone();
    let info_row = row.clone();
    let info = gio::SimpleAction::new("info", None);
    info.connect_activate(move |_, _| {
        show_get_info_popover(&info_row, &info_hit);
    });
    group.add_action(&info);

    row.insert_action_group("row", Some(&group));
}

fn show_get_info_popover(anchor: &gtk::Box, hit: &Hit) {
    let popover = gtk::Popover::new();
    popover.set_parent(anchor);
    popover.set_has_arrow(true);

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 4);
    vbox.set_margin_top(8);
    vbox.set_margin_bottom(8);
    vbox.set_margin_start(12);
    vbox.set_margin_end(12);

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

    popover.set_child(Some(&vbox));
    popover.popup();
}

fn hit_file_path(hit: &Hit) -> Option<std::path::PathBuf> {
    match &hit.action {
        Action::OpenFile { path } | Action::ShowInFileManager { path } => Some(path.clone()),
        _ => None,
    }
}

fn clear_row_controllers(row: &gtk::Box) {
    let controllers = row.observe_controllers();
    let n = controllers.n_items();
    let mut to_remove: Vec<gtk::EventController> = Vec::new();
    for i in 0..n {
        if let Some(ctrl) = controllers.item(i).and_downcast::<gtk::EventController>() {
            to_remove.push(ctrl);
        }
    }
    for c in to_remove {
        row.remove_controller(&c);
    }
}

fn install_right_click_popover(row: &gtk::Box, category: &Category) {
    let menu = build_menu_for(category);
    let popover = gtk::PopoverMenu::from_model(Some(&menu));
    popover.set_parent(row);
    popover.set_has_arrow(false);

    let gesture = gtk::GestureClick::new();
    gesture.set_button(gdk::BUTTON_SECONDARY);
    let popover_for_gesture = popover.clone();
    gesture.connect_pressed(move |_g, _n_press, x, y| {
        let rect = gdk::Rectangle::new(x as i32, y as i32, 1, 1);
        popover_for_gesture.set_pointing_to(Some(&rect));
        popover_for_gesture.popup();
    });
    row.add_controller(gesture);
}

fn install_double_click_open(row: &gtk::Box, hit: &Hit, entry: &gtk::Entry) {
    let gesture = gtk::GestureClick::new();
    gesture.set_button(gdk::BUTTON_PRIMARY);
    let hit = hit.clone();
    let entry = entry.clone();
    gesture.connect_pressed(move |_g, n_press, _x, _y| {
        if n_press != 2 {
            return;
        }
        dispatch_click_pair(&hit.id.0, entry.text().as_str());
        if let Err(e) = execute_action(&hit) {
            tracing::error!("double-click open failed: {}", e);
            return;
        }
        // Double-click = launch-completing action, so tell the
        // launcher to drop its session cache via the
        // "clear-and-hide-launcher" app action. The plain "close-
        // launcher" action does a soft hide that persists state —
        // wrong for a launch.
        if let Some(app) = gio::Application::default() {
            app.activate_action("clear-and-hide-launcher", None);
        }
    });
    row.add_controller(gesture);
}

fn install_drag_source(row: &gtk::Box, hit: &Hit) {
    let Some(path) = hit_file_path(hit) else {
        return;
    };

    let drag = gtk::DragSource::new();
    drag.set_actions(gdk::DragAction::COPY);

    let hit_for_icon = hit.clone();
    drag.connect_prepare(move |source, _x, _y| {
        let file = gio::File::for_path(&path);
        let uri = file.uri();
        let content = gdk::ContentProvider::for_value(&uri.to_value());
        if let Some(paintable) = resolve_icon(&hit_for_icon, ICON_SIZE_NORMAL) {
            source.set_icon(Some(&paintable), 0, 0);
        }
        Some(content)
    });

    row.add_controller(drag);
}

pub(crate) fn create_list_factory(entry: gtk::Entry) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();

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
    });

    let bind_entry = entry.clone();
    factory.connect_bind(move |_, list_item| {
        let list_item = list_item
            .downcast_ref::<gtk::ListItem>()
            .expect("ListItem expected");

        if let Some(str_obj) = list_item
            .item()
            .and_then(|i| i.downcast::<gtk::StringObject>().ok())
        {
            let doc_id = str_obj.string().to_string();
            with_cached_hits(|hits| {
                if let Some(hit) = hits.iter().find(|h| h.id.0 == doc_id) {
                    let row = list_item.child().and_downcast::<gtk::Box>().unwrap();

                    let text_box = row
                        .first_child()
                        .and_then(|c| c.next_sibling())
                        .and_downcast::<gtk::Box>()
                        .unwrap();
                    let title = text_box.first_child().and_downcast::<gtk::Label>().unwrap();
                    title.set_text(&hit.title);

                    let subtitle = title.next_sibling().and_downcast::<gtk::Label>().unwrap();
                    subtitle.set_text(&hit.subtitle);

                    let kind = text_box
                        .next_sibling()
                        .and_downcast::<gtk::Label>()
                        .unwrap();
                    let kind_text = hit
                        .kind_label
                        .clone()
                        .unwrap_or_else(|| category_kind_fallback(&hit.category).to_string());
                    kind.set_text(&kind_text);

                    clear_row_controllers(&row);
                    install_action_group(&row, hit, &bind_entry);
                    install_right_click_popover(&row, &hit.category);
                    install_double_click_open(&row, hit, &bind_entry);
                    if matches!(hit.category, Category::File | Category::Attachment) {
                        install_drag_source(&row, hit);
                    }
                }
            });
        }

        apply_selected_styling(list_item);
        apply_top_hit_styling(list_item);
    });

    factory
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
                icon.set_icon_name(Some(match hit.category {
                    Category::App => "application-x-executable",
                    Category::File => "text-x-generic",
                    Category::Mail => "mail-message",
                    Category::Attachment => "mail-attachment",
                }));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_for_file_has_expected_items() {
        let menu = build_menu_for(&Category::File);
        assert_eq!(menu.n_items(), 5);
    }

    #[test]
    fn menu_for_app_has_expected_items() {
        let menu = build_menu_for(&Category::App);
        assert_eq!(menu.n_items(), 2);
    }

    #[test]
    fn menu_for_mail_has_expected_items() {
        let menu = build_menu_for(&Category::Mail);
        assert_eq!(menu.n_items(), 2);
    }

    #[test]
    fn menu_for_attachment_has_expected_items() {
        let menu = build_menu_for(&Category::Attachment);
        assert_eq!(menu.n_items(), 4);
    }

    #[test]
    fn row_state_default_is_unbound() {
        let s = RowState::default();
        assert!(s.doc_id.is_none());
        assert!(s.category.is_none());
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
