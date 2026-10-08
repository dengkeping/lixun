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
    /// MIME key the row's "Open With" submenu was last populated
    /// for; `None` when the submenu is empty/absent. Lets bind skip
    /// the repopulation when consecutive hits share a type, which is
    /// the common case while scrolling a homogeneous list.
    openwith_mime: Option<String>,
}

/// Everything `create_list_factory` needs beyond the widgets it
/// builds itself. Bundled so the call site stays readable as the
/// row machinery grows.
pub(crate) struct RowFactoryCtx {
    pub(crate) entry: gtk::Entry,
    pub(crate) status: Rc<StatusBar>,
    /// Resolved keybindings, used for the menu items' accelerator
    /// labels (K11) — the popover doubles as shortcut documentation.
    pub(crate) keybindings: Rc<lixun_config::Keybindings>,
    /// K10: clicking a row is explicit user selection; late Final
    /// chunks must preserve it instead of snapping to row 0.
    pub(crate) user_selected: Rc<std::cell::Cell<bool>>,
    /// Count of currently-mapped row popovers (context menu / Get
    /// Info). While non-zero the launcher's focus-leave handler must
    /// not hide the window: an autohide popover takes an xdg_popup
    /// grab that moves keyboard focus to the popup surface, which
    /// fires `connect_leave` — without this gate the launcher hides
    /// and the popover dies with it (the trap that bit the ?/F1
    /// overlay twice). Map/unmap (not popup/closed) keep the count
    /// balanced even when a popover is torn down without a `closed`
    /// emission.
    pub(crate) popover_open: Rc<std::cell::Cell<u32>>,
    /// The unfiltered results model, so row verbs that remove a row
    /// (C2 "Hide from Results") can update the visible list without
    /// waiting for the next search round-trip.
    pub(crate) model: gtk::StringList,
}

use crate::actions::{copy_to_clipboard, execute_action, execute_secondary_action};
use crate::icons::{category_fallback, resolve_icon};
use crate::ipc::dispatch_click_pair;
use crate::ipc::send_preview_request;
use crate::status::StatusBar;

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
    /// doc_id → row widget currently bound to it (K11). Weak refs:
    /// the pool owns the widgets; entries are pruned on unbind, so
    /// the map never outgrows the visible row pool.
    static ROW_WIDGETS: RefCell<HashMap<String, glib::WeakRef<gtk::Box>>> =
        RefCell::new(HashMap::new());
    /// doc_id → one-line ranking explanation from the daemon (R9).
    /// Refreshed on every Final chunk; read by the Get Info popover.
    static EXPLANATIONS: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
    /// mime → (app name, desktop id) pairs for the "Open With"
    /// submenu (C1). AppInfo enumeration walks the desktop database;
    /// caching per MIME keeps row binds cheap. Bounded by the number
    /// of distinct MIME types the user's results surface.
    static OPENWITH_CACHE: RefCell<HashMap<String, OpenWithApps>> =
        RefCell::new(HashMap::new());
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
    EXPLANATIONS.with(|c| c.borrow_mut().clear());
}

/// Replace the ranking-explanation map (R9). Called on every Final
/// chunk with entries zipped from the daemon's index-aligned
/// `explanations`.
pub(crate) fn cache_explanations(map: HashMap<String, String>) {
    EXPLANATIONS.with(|c| *c.borrow_mut() = map);
}

/// The row widget currently bound to `doc_id`, if that row is
/// realized in the visible pool (K11).
pub(crate) fn row_widget_for(doc_id: &str) -> Option<gtk::Box> {
    ROW_WIDGETS.with(|m| m.borrow().get(doc_id).and_then(|w| w.upgrade()))
}

/// Pop the actions menu of the row bound to `doc_id` (K11 keyboard
/// path). Returns false when the row is not realized.
pub(crate) fn open_row_menu(doc_id: &str) -> bool {
    let Some(row) = row_widget_for(doc_id) else {
        return false;
    };
    gtk::prelude::WidgetExt::activate_action(&row, "row.menu", None).is_ok()
}

/// Pop the Get Info popover of the row bound to `doc_id` (K11).
pub(crate) fn open_row_info(doc_id: &str) -> bool {
    let Some(row) = row_widget_for(doc_id) else {
        return false;
    };
    gtk::prelude::WidgetExt::activate_action(&row, "row.info", None).is_ok()
}

/// Doc-id namespace for rows the GUI synthesizes locally (recent
/// searches, the web-search fallback, "Show more results"). These
/// never exist in the index, so click recording skips them.
pub(crate) fn is_synthetic_doc_id(doc_id: &str) -> bool {
    doc_id.starts_with("history:")
        || doc_id.starts_with("gui-web:")
        || doc_id.starts_with("gui-more:")
}

/// Zero-results fallback row (C3c): a selectable, Enter-able
/// "Search the web" hit mirroring `synthetic_history_hits`, so the
/// fallback is keyboard-reachable instead of button-only.
pub(crate) fn synthetic_web_search_hit(query: &str, engine_template: &str) -> Hit {
    use lixun_core::DocId;
    let uri = crate::status::web_search_url_for(engine_template, query);
    Hit {
        id: DocId(format!("gui-web:{query}")),
        category: Category::File,
        title: format!("Search the web for \u{201c}{query}\u{201d}"),
        subtitle: uri.clone(),
        icon_name: Some("web-browser".to_string()),
        kind_label: Some("Web Search".to_string()),
        score: 0.0,
        action: Action::OpenUri { uri },
        extract_fail: false,
        sender: None,
        recipients: None,
        body: None,
        secondary_action: None,
        source_instance: String::new(),
        row_menu: RowMenuDef::empty(),
        mime: None,
        timestamp: None,
        size: None,
    }
}

/// Terminal "Show more results" row (R7), appended when a page came
/// back full. Activation is intercepted by id prefix (see
/// `is_more_results_doc_id`) and re-issues the query with a larger
/// limit; the stored action is an inert same-query ReplaceQuery so
/// unknown dispatch paths degrade harmlessly.
pub(crate) fn synthetic_more_results_hit(query: &str, next_limit: u32) -> Hit {
    use lixun_core::DocId;
    Hit {
        id: DocId(format!("gui-more:{next_limit}:{query}")),
        category: Category::File,
        title: "Show more results".to_string(),
        subtitle: format!("Fetch up to {next_limit} results"),
        icon_name: Some("go-down-symbolic".to_string()),
        kind_label: Some("More".to_string()),
        score: 0.0,
        action: Action::ReplaceQuery { q: query.to_string() },
        extract_fail: false,
        sender: None,
        recipients: None,
        body: None,
        secondary_action: None,
        source_instance: String::new(),
        row_menu: RowMenuDef::empty(),
        mime: None,
        timestamp: None,
        size: None,
    }
}

pub(crate) fn is_more_results_doc_id(doc_id: &str) -> bool {
    doc_id.starts_with("gui-more:")
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
pub fn update_results(
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
            secondary_action: None,
            source_instance: String::new(),
            row_menu: lixun_core::RowMenuDef::empty(),
            mime: None,
            timestamp: None,
            size: None,
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
fn menu_for_key(key: &str, def: &RowMenuDef, kb: &lixun_config::Keybindings) -> gio::Menu {
    MENU_CACHE.with(|cache| {
        if let Some(existing) = cache.borrow().get(key) {
            return existing.clone();
        }
        let menu = gio::Menu::new();
        for item in &def.items {
            // Verb → (row action, accelerator shown as the item's
            // shortcut label). The accel attribute is documentation:
            // GTK renders it beside the item, so the panel doubles
            // as a per-row cheat sheet (K11). Dispatch itself stays
            // in the keymap.
            let (action, accel): (&str, Option<&str>) = match item.verb {
                RowMenuVerb::Open => ("row.open", Some(kb.primary_action.as_str())),
                RowMenuVerb::Secondary => ("row.secondary", Some(kb.secondary_action.as_str())),
                RowMenuVerb::Copy => ("row.copy", Some(kb.copy.as_str())),
                RowMenuVerb::QuickLook => ("row.quicklook", Some(kb.quick_look_alt.as_str())),
                RowMenuVerb::Info => ("row.info", Some(kb.info.as_str())),
                // Rendered as a per-hit submenu by the bind path —
                // never into this shared, per-source cached model
                // (C1; the cache is the popover-leak fix).
                RowMenuVerb::OpenWith => continue,
                RowMenuVerb::HideFromResults => ("row.hide", None),
                RowMenuVerb::ResetRanking => ("row.reset-rank", None),
            };
            let menu_item = gio::MenuItem::new(Some(&item.label), Some(action));
            // No parse-validation here: GTK ignores unparseable
            // accel attributes at render time, and validating would
            // drag gtk::accelerator_parse (main-thread-only) into
            // this otherwise gio-level, headless-testable function.
            if let Some(accel) = accel {
                menu_item.set_attribute_value("accel", Some(&accel.to_variant()));
            }
            menu.append_item(&menu_item);
        }
        // Host-owned ranking-control section (C2). Frecency and the
        // query latch are daemon/host state, not plugin domain, so
        // the host appends these verbs to every non-empty row menu
        // uniformly rather than asking each source to declare them.
        if !def.is_empty() {
            let ranking = gio::Menu::new();
            ranking.append(Some("Hide from Results"), Some("row.hide"));
            ranking.append(Some("Reset Ranking"), Some("row.reset-rank"));
            menu.append_section(None, &ranking);
        }
        cache.borrow_mut().insert(key.to_string(), menu.clone());
        menu
    })
}

/// The declared "Open With" item's label, when the source opted into
/// the verb (C1). Presence drives whether the bind path links the
/// per-row submenu into the popover shell.
fn menu_openwith_label(def: &RowMenuDef) -> Option<String> {
    def.items
        .iter()
        .find(|it| it.verb == RowMenuVerb::OpenWith)
        .map(|it| it.label.clone())
}

/// (display name, desktop id) pairs registered for one MIME type.
type OpenWithApps = Rc<Vec<(String, String)>>;

/// Applications registered for `mime`, as (display name, desktop id)
/// pairs, cached per MIME type. Enumeration order is GIO's
/// recommendation order; capped so a type with dozens of handlers
/// does not produce an unusable submenu.
fn openwith_apps_for_mime(mime: &str) -> OpenWithApps {
    const MAX_OPENWITH_APPS: usize = 8;
    OPENWITH_CACHE.with(|cache| {
        if let Some(existing) = cache.borrow().get(mime) {
            return Rc::clone(existing);
        }
        let apps: Vec<(String, String)> = gio::AppInfo::all_for_type(mime)
            .into_iter()
            .filter_map(|app| {
                let id = app.id()?;
                Some((app.name().to_string(), id.to_string()))
            })
            .take(MAX_OPENWITH_APPS)
            .collect();
        let apps = Rc::new(apps);
        cache
            .borrow_mut()
            .insert(mime.to_string(), Rc::clone(&apps));
        apps
    })
}

/// MIME key for a hit's "Open With" submenu: the stored MIME when
/// the source recorded one, otherwise a content-type guess from the
/// file extension. `None` for hits without a file path — the
/// submenu is omitted for those.
fn openwith_mime_for_hit(hit: &Hit) -> Option<String> {
    if let Some(mime) = hit.mime.as_deref().filter(|m| !m.is_empty()) {
        return Some(mime.to_string());
    }
    let path = hit_file_path(hit)?;
    let (guess, _uncertain) =
        gio::functions::content_type_guess(Some(&path), None::<&[u8]>);
    Some(guess.to_string())
}

/// Rebuild `submenu` with the applications for `mime`. Item targets
/// carry the desktop id; the shared `row.openwith` action launches
/// by id, so the submenu content is pure data.
fn populate_openwith_menu(submenu: &gio::Menu, mime: &str) {
    submenu.remove_all();
    let apps = openwith_apps_for_mime(mime);
    if apps.is_empty() {
        // Item with an unregistered action renders insensitive —
        // an honest "nothing installed" placeholder.
        submenu.append(Some("No applications found"), Some("row.none"));
        return;
    }
    for (name, id) in apps.iter() {
        let item = gio::MenuItem::new(Some(name), None);
        item.set_action_and_target_value(Some("row.openwith"), Some(&id.to_variant()));
        submenu.append_item(&item);
    }
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

/// Display-only subtitle for absolute-path rows: the parent
/// directory with `home` contracted to `~`. The trailing filename is
/// dropped because it duplicates the row title; the deep directories
/// are the disambiguating part, so callers pair this with
/// `EllipsizeMode::Middle` and put the full path on the row tooltip.
/// Never touches the `Hit` — index semantics stay unchanged.
fn display_path_subtitle(subtitle: &str, home: Option<&std::path::Path>) -> String {
    let path = std::path::Path::new(subtitle);
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(path);
    if let Some(home) = home
        && let Ok(rest) = dir.strip_prefix(home)
    {
        return if rest.as_os_str().is_empty() {
            "~".to_string()
        } else {
            format!("~/{}", rest.display())
        };
    }
    dir.display().to_string()
}

pub(crate) fn hit_file_path(hit: &Hit) -> Option<std::path::PathBuf> {
    match &hit.action {
        Action::OpenFile { path } | Action::ShowInFileManager { path } => Some(path.clone()),
        _ => None,
    }
}

/// Humanized byte size for the Get Info popover and mail rows (R4).
/// Decimal units, one fractional digit above KB.
pub(crate) fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Compact relative age ("2d ago") for the row's right-hand column
/// (R4). Coarse on purpose — the exact timestamp lives in the row
/// tooltip and the Get Info popover.
pub(crate) fn relative_age(ts: i64, now: i64) -> String {
    let secs = (now - ts).max(0);
    if secs < 60 {
        "now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else if secs < 30 * 86_400 {
        format!("{}d ago", secs / 86_400)
    } else if secs < 365 * 86_400 {
        format!("{}mo ago", secs / (30 * 86_400))
    } else {
        format!("{}y ago", secs / (365 * 86_400))
    }
}

/// Absolute local timestamp for tooltips / Get Info.
fn absolute_date(ts: i64) -> Option<String> {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
}

/// Fold a string the way the index tokenizer does (NFKD + strip
/// U+0300..=U+036F combining marks + lowercase), keeping a map from
/// each folded char back to the ORIGINAL byte range it came from, so
/// highlight spans land on the right bytes of the displayed text
/// even across diacritics and multi-char lowercasing (R2).
fn fold_with_map(text: &str) -> (String, Vec<(u32, u32)>) {
    use unicode_normalization::UnicodeNormalization;
    let mut folded = String::with_capacity(text.len());
    let mut map: Vec<(u32, u32)> = Vec::with_capacity(text.len());
    for (start, ch) in text.char_indices() {
        let end = start + ch.len_utf8();
        for dc in ch.nfkd() {
            if matches!(dc, '\u{0300}'..='\u{036F}') {
                continue;
            }
            for lc in dc.to_lowercase() {
                folded.push(lc);
                map.push((start as u32, end as u32));
            }
        }
    }
    (folded, map)
}

/// Byte ranges of `text` (in ORIGINAL bytes) matching any
/// whitespace-separated token of `query`, case- and
/// diacritic-insensitively. Overlapping/duplicate ranges may occur;
/// Pango tolerates overlapping attributes, so no merge pass.
pub(crate) fn highlight_ranges(text: &str, query: &str) -> Vec<(u32, u32)> {
    let (folded, map) = fold_with_map(text);
    let mut out = Vec::new();
    for token in query.split_whitespace() {
        let (needle, _) = fold_with_map(token);
        if needle.is_empty() {
            continue;
        }
        let mut from = 0usize;
        while let Some(pos) = folded[from..].find(&needle) {
            let start = from + pos;
            let end = start + needle.len();
            // Translate folded BYTE offsets to folded CHAR indices to
            // read the map.
            let start_char = folded[..start].chars().count();
            let end_char = start_char + folded[start..end].chars().count();
            if let (Some(&(s, _)), Some(&(_, e))) =
                (map.get(start_char), map.get(end_char.saturating_sub(1)))
            {
                out.push((s, e));
            }
            from = end;
        }
    }
    out
}

/// Apply (or clear) bold match-highlight attributes on a row label
/// for the current query (R2). Always sets attributes so recycled
/// rows never carry a stale highlight.
fn apply_highlight(label: &gtk::Label, displayed_text: &str, query: &str) {
    let ranges = if query.trim().is_empty() {
        Vec::new()
    } else {
        highlight_ranges(displayed_text, query)
    };
    if ranges.is_empty() {
        label.set_attributes(None);
        return;
    }
    let attrs = gtk::pango::AttrList::new();
    for (start, end) in ranges {
        let mut attr = gtk::pango::AttrInt::new_weight(gtk::pango::Weight::Bold);
        attr.set_start_index(start);
        attr.set_end_index(end);
        attrs.insert(attr);
    }
    label.set_attributes(Some(&attrs));
}

/// Parse the daemon's one-line score breakdown
/// (`score=… = tantivy(…) × cat(1.300) × prefix(1.400) × …`) into
/// named multiplier values. `score`/`tantivy` are the base, not
/// multipliers, and are skipped. Pure so it unit-tests headlessly.
fn parse_score_multipliers(explanation: &str) -> Vec<(String, f32)> {
    let mut out = Vec::new();
    let bytes = explanation.as_bytes();
    let mut i = 0;
    while let Some(open_rel) = explanation[i..].find('(') {
        let open = i + open_rel;
        // Walk back over the multiplier name (ascii alnum + '_').
        let mut start = open;
        while start > 0
            && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_')
        {
            start -= 1;
        }
        let Some(close_rel) = explanation[open..].find(')') else {
            break;
        };
        let close = open + close_rel;
        let name = &explanation[start..open];
        if let Ok(value) = explanation[open + 1..close].parse::<f32>()
            && !name.is_empty()
            && name != "score"
            && name != "tantivy"
        {
            out.push((name.to_string(), value));
        }
        i = close + 1;
    }
    out
}

/// Human labels for the breakdown's multiplier tokens. Unknown
/// tokens fall back to themselves so a future daemon field still
/// renders something meaningful.
fn multiplier_label(name: &str) -> &str {
    match name {
        "cat" => "category weight",
        "exact" => "exact title match",
        "prefix" => "title starts with query",
        "acronym" => "acronym match",
        "recency" => "recently modified",
        "coord" => "all words in title",
        "stage2" => "opened often",
        other => other,
    }
}

/// The top ranking factors as a short human line for the Get Info
/// popover (R9): non-neutral multipliers, strongest first, capped at
/// three. `None` when everything is ~1.0 (neutral ranking).
fn ranking_summary(explanation: &str) -> Option<String> {
    const NEUTRAL_EPSILON: f32 = 0.02;
    let mut mults: Vec<(String, f32)> = parse_score_multipliers(explanation)
        .into_iter()
        .filter(|(_, v)| (v - 1.0).abs() > NEUTRAL_EPSILON)
        .collect();
    if mults.is_empty() {
        return None;
    }
    mults.sort_by(|a, b| {
        (b.1 - 1.0)
            .abs()
            .partial_cmp(&(a.1 - 1.0).abs())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    mults.truncate(3);
    let ups: Vec<String> = mults
        .iter()
        .filter(|(_, v)| *v > 1.0)
        .map(|(n, v)| format!("{} \u{d7}{:.2}", multiplier_label(n), v))
        .collect();
    let downs: Vec<String> = mults
        .iter()
        .filter(|(_, v)| *v < 1.0)
        .map(|(n, v)| format!("{} \u{d7}{:.2}", multiplier_label(n), v))
        .collect();
    let mut parts = Vec::new();
    if !ups.is_empty() {
        parts.push(format!("Ranked up: {}", ups.join(", ")));
    }
    if !downs.is_empty() {
        parts.push(format!("Ranked down: {}", downs.join(", ")));
    }
    Some(parts.join(" \u{b7} "))
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

    // R4: metadata rows from the hit's hydrated index fields.
    let meta_row = |text: String| {
        let label = gtk::Label::new(Some(&text));
        label.set_xalign(0.0);
        add_css_class(&label, "lixun-subtitle");
        vbox.append(&label);
    };
    if let Some(ts) = hit.timestamp
        && let Some(date) = absolute_date(ts)
    {
        meta_row(format!("Modified: {date}"));
    }
    if let Some(size) = hit.size {
        meta_row(format!("Size: {}", human_size(size)));
    }
    if let Some(path) = hit_file_path(hit) {
        meta_row(format!("Where: {}", path.display()));
    }

    // R9: why this hit ranked where it did — top non-neutral
    // multipliers from the daemon's explain pipeline.
    let summary = EXPLANATIONS.with(|m| {
        m.borrow()
            .get(&hit.id.0)
            .and_then(|expl| ranking_summary(expl))
    });
    if let Some(summary) = summary {
        meta_row(summary);
    }
}

pub(crate) fn create_list_factory(ctx: RowFactoryCtx) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    let setup_entry = ctx.entry.clone();
    let setup_status = ctx.status.clone();
    let setup_kb = Rc::clone(&ctx.keybindings);
    let setup_user_selected = Rc::clone(&ctx.user_selected);
    let setup_popover_open = Rc::clone(&ctx.popover_open);
    let setup_model = ctx.model.clone();

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

        // Direction-aware alignment (halign, not xalign) so RTL
        // locales mirror the row. Ellipsizing still works with
        // halign Start: a non-Fill label's allocation is capped at
        // min(natural, available), so the ellipsis kicks in whenever
        // the text outgrows the row.
        let title = gtk::Label::new(None);
        title.set_halign(gtk::Align::Start);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        add_css_class(&title, "lixun-title");
        text_box.append(&title);

        let subtitle = gtk::Label::new(None);
        subtitle.set_halign(gtk::Align::Start);
        subtitle.set_ellipsize(gtk::pango::EllipsizeMode::End);
        add_css_class(&subtitle, "lixun-subtitle");
        text_box.append(&subtitle);

        row.append(&text_box);

        let kind = gtk::Label::new(None);
        kind.set_halign(gtk::Align::End);
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
        let open_status = setup_status.clone();
        let open = gio::SimpleAction::new("open", None);
        open.connect_activate(move |_, _| {
            let Some(doc_id) = open_state.borrow().doc_id.clone() else {
                tracing::debug!("row.open fired on unbound row");
                return;
            };
            if is_more_results_doc_id(&doc_id) {
                if let Some(app) = gio::Application::default() {
                    app.activate_action("show-more-results", None);
                }
                return;
            }
            if let Some(hit) = cached_hit_by_id(&doc_id) {
                dispatch_click_pair(&hit.id.0, open_entry.text().as_str());
                if let Err(e) = execute_action(&hit) {
                    tracing::error!("Action failed: {}", e);
                    // Same visibility rule as the Enter path (keymap):
                    // a silent failure is indistinguishable from
                    // success.
                    open_status.show_error(&format!("Couldn't open “{}”: {}", hit.title, e));
                }
            }
        });
        group.add_action(&open);

        let secondary_state = Rc::clone(&state);
        let secondary_status = setup_status.clone();
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
                secondary_status.show_error(&format!("Couldn't open “{}”: {}", hit.title, e));
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

        // ===== "Open With <app>" (C1) =====
        // Shared action taking the desktop id as its target; the
        // per-hit submenu items carry the id as pure data. Launch is
        // mime-registry driven — the host names no application.
        let openwith_state = Rc::clone(&state);
        let openwith_entry = setup_entry.clone();
        let openwith_status = setup_status.clone();
        let openwith =
            gio::SimpleAction::new("openwith", Some(glib::VariantTy::STRING));
        openwith.connect_activate(move |_, param| {
            let Some(doc_id) = openwith_state.borrow().doc_id.clone() else {
                tracing::debug!("row.openwith fired on unbound row");
                return;
            };
            let Some(app_id) = param.and_then(|v| v.get::<String>()) else {
                return;
            };
            if let Some(hit) = cached_hit_by_id(&doc_id) {
                let Some(path) = hit_file_path(&hit) else {
                    return;
                };
                dispatch_click_pair(&hit.id.0, openwith_entry.text().as_str());
                if let Err(e) = crate::actions::launch_with_app_id(&app_id, &path) {
                    tracing::error!("open-with failed: {}", e);
                    openwith_status
                        .show_error(&format!("Couldn't open \u{201c}{}\u{201d}: {}", hit.title, e));
                }
            }
        });
        group.add_action(&openwith);

        // ===== Ranking controls (C2) =====
        let hide_state = Rc::clone(&state);
        let hide_status = setup_status.clone();
        let hide_model = setup_model.clone();
        let hide = gio::SimpleAction::new("hide", None);
        hide.connect_activate(move |_, _| {
            let Some(doc_id) = hide_state.borrow().doc_id.clone() else {
                tracing::debug!("row.hide fired on unbound row");
                return;
            };
            if is_synthetic_doc_id(&doc_id) {
                return;
            }
            crate::ipc::send_set_doc_hidden(&doc_id, true);
            // Remove the row locally so the hide is visible without
            // a search round-trip.
            let n = hide_model.n_items();
            for i in 0..n {
                if hide_model.string(i).is_some_and(|s| s == doc_id) {
                    hide_model.remove(i);
                    break;
                }
            }
            CACHED_HITS.with(|c| c.borrow_mut().retain(|h| h.id.0 != doc_id));
            hide_status.show_toast(
                "Hidden from results \u{2014} undo with `lixun-cli hidden`",
            );
        });
        group.add_action(&hide);

        let reset_state = Rc::clone(&state);
        let reset_status = setup_status.clone();
        let reset = gio::SimpleAction::new("reset-rank", None);
        reset.connect_activate(move |_, _| {
            let Some(doc_id) = reset_state.borrow().doc_id.clone() else {
                tracing::debug!("row.reset-rank fired on unbound row");
                return;
            };
            if is_synthetic_doc_id(&doc_id) {
                return;
            }
            crate::ipc::send_reset_doc_ranking(&doc_id);
            let title = cached_hit_by_id(&doc_id)
                .map(|h| h.title)
                .unwrap_or_else(|| doc_id.clone());
            reset_status.show_toast(&format!("Ranking reset for \u{201c}{}\u{201d}", title));
        });
        group.add_action(&reset);

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

        // ===== Right-click popover (persistent; shell mutated on bind) =====
        // Each pool slot owns ONE PopoverMenu whose model is a
        // per-slot `shell` menu, set exactly once here. On bind,
        // `on_item_notify` mutates the shell only when the hit's
        // `source_instance` changes: it links in the per-source
        // cached section (see MENU_CACHE — the popover-leak fix:
        // `set_menu_model` is never called again) plus, for sources
        // declaring the OpenWith verb, the per-slot `openwith_menu`
        // submenu whose items are per-hit data (C1).
        let shell_menu = gio::Menu::new();
        let openwith_menu = gio::Menu::new();
        let right_click_popover = gtk::PopoverMenu::from_model(Some(&shell_menu));
        right_click_popover.set_parent(&row);
        right_click_popover.set_has_arrow(false);

        // Focus-trap guard (K11): count mapped row popovers so the
        // launcher's focus-leave handler ignores the focus loss an
        // autohide popover's grab produces. Map/unmap stay balanced
        // on every teardown path, unlike popup/closed.
        for popover in [
            right_click_popover.clone().upcast::<gtk::Widget>(),
            info_popover.clone().upcast::<gtk::Widget>(),
        ] {
            let opened = Rc::clone(&setup_popover_open);
            popover.connect_map(move |_| {
                opened.set(opened.get() + 1);
            });
            let closed_count = Rc::clone(&setup_popover_open);
            popover.connect_unmap(move |_| {
                closed_count.set(closed_count.get().saturating_sub(1));
            });
        }

        let right_click_gesture = gtk::GestureClick::new();
        right_click_gesture.set_button(gdk::BUTTON_SECONDARY);
        let right_click_popover_for_gesture = right_click_popover.clone();
        right_click_gesture.connect_pressed(move |_g, _n_press, x, y| {
            let rect = gdk::Rectangle::new(x as i32, y as i32, 1, 1);
            right_click_popover_for_gesture.set_pointing_to(Some(&rect));
            right_click_popover_for_gesture.popup();
        });
        row.add_controller(right_click_gesture);

        // K11 keyboard path: `row.menu` pops the same popover
        // pointing at the row itself (no pointer position to anchor
        // to). Reached via the keymap's actions_menu / Menu /
        // Shift+F10 dispatch through `open_row_menu`.
        let menu_popover = right_click_popover.clone();
        let menu_action = gio::SimpleAction::new("menu", None);
        menu_action.connect_activate(move |_, _| {
            menu_popover.set_pointing_to(None);
            menu_popover.popup();
        });
        group.add_action(&menu_action);

        // ===== Double-click primary = launch + clear-and-hide =====
        let dblclick_state = Rc::clone(&state);
        let dblclick_entry = setup_entry.clone();
        let dblclick_status = setup_status.clone();
        let dblclick_user_selected = Rc::clone(&setup_user_selected);
        let dblclick_gesture = gtk::GestureClick::new();
        dblclick_gesture.set_button(gdk::BUTTON_PRIMARY);
        dblclick_gesture.connect_pressed(move |_g, n_press, _x, _y| {
            if n_press == 1 {
                // K10: a plain click is explicit user selection —
                // protect it from a late Final chunk snapping the
                // cursor back to row 0 (and swapping the preview
                // under the user in preview mode).
                dblclick_user_selected.set(true);
                return;
            }
            if n_press != 2 {
                return;
            }
            let Some(doc_id) = dblclick_state.borrow().doc_id.clone() else {
                tracing::debug!("double-click fired on unbound row");
                return;
            };
            // R7: the terminal "Show more results" row re-issues the
            // query with a larger limit instead of launching.
            if is_more_results_doc_id(&doc_id) {
                if let Some(app) = gio::Application::default() {
                    app.activate_action("show-more-results", None);
                }
                return;
            }
            if let Some(hit) = cached_hit_by_id(&doc_id) {
                dispatch_click_pair(&hit.id.0, dblclick_entry.text().as_str());
                if let Err(e) = execute_action(&hit) {
                    tracing::error!("double-click open failed: {}", e);
                    // The launcher stays up (no clear-and-hide below),
                    // so the status bar can say what went wrong.
                    dblclick_status.show_error(&format!("Couldn't open “{}”: {}", hit.title, e));
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
        let notify_shell = shell_menu.clone();
        let notify_openwith = openwith_menu.clone();
        let notify_secondary = secondary.clone();
        let notify_entry = setup_entry.clone();
        let notify_kb = Rc::clone(&setup_kb);
        list_item.connect_notify_local(Some("item"), move |list_item, _| {
            on_item_notify(
                list_item,
                &notify_state,
                &notify_shell,
                &notify_openwith,
                &notify_secondary,
                &notify_entry,
                &notify_kb,
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
#[allow(clippy::too_many_arguments)]
fn on_item_notify(
    list_item: &gtk::ListItem,
    state: &Rc<RefCell<RowState>>,
    shell_menu: &gio::Menu,
    openwith_menu: &gio::Menu,
    secondary: &gio::SimpleAction,
    entry: &gtk::Entry,
    kb: &Rc<lixun_config::Keybindings>,
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
        // state. The path tooltip is display state too: drop it so
        // a recycled row never shows the previous hit's path.
        row.set_tooltip_text(None);
        let mut s = state.borrow_mut();
        if let Some(old_id) = s.doc_id.take() {
            // K11 registry: this row no longer answers for that doc.
            ROW_WIDGETS.with(|m| {
                m.borrow_mut().remove(&old_id);
            });
        }
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
            let query = entry.text().to_string();
            let title = text_box
                .first_child()
                .and_downcast::<gtk::Label>()
                .expect("title");
            title.set_text(&hit.title);
            // R2: embolden the query's matches so body-only hits stop
            // looking unrelated. Folding mirrors the index tokenizer
            // (NFKD + strip combining marks + lowercase).
            apply_highlight(&title, &hit.title, &query);

            let subtitle = title
                .next_sibling()
                .and_downcast::<gtk::Label>()
                .expect("subtitle");
            // Display-only path contraction (the Hit itself is
            // untouched): "~" for $HOME, trailing filename dropped
            // (it duplicates the title), middle-ellipsis so the
            // disambiguating deep directories survive, full path on
            // the row tooltip. Non-path subtitles (mail authors,
            // "Recent search") keep the plain end-ellipsized text.
            if hit.subtitle.starts_with('/') {
                let displayed =
                    display_path_subtitle(&hit.subtitle, dirs::home_dir().as_deref());
                subtitle.set_text(&displayed);
                apply_highlight(&subtitle, &displayed, &query);
                subtitle.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
                row.set_tooltip_text(Some(&hit.subtitle));
            } else {
                subtitle.set_text(&hit.subtitle);
                apply_highlight(&subtitle, &hit.subtitle, &query);
                subtitle.set_ellipsize(gtk::pango::EllipsizeMode::End);
                row.set_tooltip_text(None);
            }

            let kind = text_box
                .next_sibling()
                .and_downcast::<gtk::Label>()
                .expect("kind");
            let kind_text = hit
                .kind_label
                .clone()
                .unwrap_or_else(|| category_kind_fallback(&hit.category).to_string());
            // R4: mail and attachment rows show WHEN over WHAT — the
            // relative date replaces the kind label (which moves to
            // the label's tooltip; the full metadata lives in Get
            // Info). Generic: keyed on category + timestamp presence.
            let mail_like = matches!(hit.category, Category::Mail | Category::Attachment);
            // R9: the hero row carries a "Top Hit" caption in the
            // existing kind-label slot (no box-model change — BUG-6);
            // the displaced kind text moves to the tooltip.
            if is_top_hit_doc(&doc_id) {
                kind.set_text("Top Hit");
                kind.set_tooltip_text(Some(&kind_text));
                kind.add_css_class("lixun-top-hit-caption");
            } else {
                kind.remove_css_class("lixun-top-hit-caption");
                match hit.timestamp {
                    Some(ts) if mail_like => {
                        let now = chrono::Utc::now().timestamp();
                        kind.set_text(&relative_age(ts, now));
                        kind.set_tooltip_text(Some(&kind_text));
                    }
                    _ => {
                        kind.set_text(&kind_text);
                        kind.set_tooltip_text(None);
                    }
                }
            }

            // A1: expose the whole row to assistive tech. Selection
            // moves while focus stays on the entry, so the label is
            // the only thing Orca can read per row.
            let mut accessible_label =
                format!("{}, {}, {}", hit.title, hit.subtitle, kind_text);
            if is_top_hit_doc(&doc_id) {
                accessible_label.push_str(", Top Hit");
            }
            row.update_property(&[gtk::accessible::Property::Label(&accessible_label)]);

            // Cache-aware menu swap: only rebuild the per-slot shell
            // when this row's menu_key differs from the new hit's
            // source_instance. Combined with the per-source
            // `MENU_CACHE`, this bounds shell rebuilds to
            // O(unique source_instances) per slot instead of
            // O(binds) — `set_menu_model` itself is never called
            // after setup, which preserves the PopoverMenu
            // retention-leak fix measured by heaptrack on the old
            // per-bind code path.
            let current_key = state.borrow().menu_key.clone();
            let new_key = Some(hit.source_instance.clone());
            let openwith_declared = menu_openwith_label(&hit.row_menu);
            if current_key != new_key {
                shell_menu.remove_all();
                let cached = menu_for_key(&hit.source_instance, &hit.row_menu, kb);
                shell_menu.append_section(None, &cached);
                if let Some(label) = openwith_declared.as_deref() {
                    // Per-HIT content lives in the per-slot submenu,
                    // never in the shared cached model (C1).
                    shell_menu.append_submenu(Some(label), openwith_menu);
                }
                state.borrow_mut().openwith_mime = None;
            }

            // C1: (re)populate the per-slot Open With submenu when
            // the hit's MIME key changed since the last bind of this
            // slot. Homogeneous lists (all one type) repopulate once.
            if openwith_declared.is_some() {
                let mime = openwith_mime_for_hit(hit);
                let prev = state.borrow().openwith_mime.clone();
                if mime != prev {
                    match mime.as_deref() {
                        Some(m) => populate_openwith_menu(openwith_menu, m),
                        None => openwith_menu.remove_all(),
                    }
                    state.borrow_mut().openwith_mime = mime;
                }
            }

            // K11 registry: this row now answers for the bound doc.
            ROW_WIDGETS.with(|m| {
                let mut map = m.borrow_mut();
                if let Some(old_id) = state.borrow().doc_id.as_deref()
                    && old_id != doc_id
                {
                    map.remove(old_id);
                }
                map.insert(doc_id.clone(), row.downgrade());
            });

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
        assert!(s.openwith_mime.is_none());
    }

    // The three menu_for_key tests touch GTK (gio::Menu + accel
    // labels), so their bodies run on the shared `test_gtk` worker —
    // initializing GTK per-test raced the gtk4-rs cross-thread init
    // assertion against keymap's accel test. MENU_CACHE is a
    // thread_local; routing every toucher through the single worker
    // also keeps the cache the tests inspect on one thread.
    #[test]
    fn menu_for_key_caches_by_key() {
        let ran = crate::test_gtk::run_gtk(|| {
            MENU_CACHE.with(|c| c.borrow_mut().clear());
            let key = "test.key.cache";
            let def = sample_def_single(RowMenuVerb::Open, "Open");
            let kb = lixun_config::Keybindings::default();
            let a = menu_for_key(key, &def, &kb);
            let b = menu_for_key(key, &def, &kb);
            // One plugin item plus the host-appended ranking section.
            assert_eq!(a.n_items(), 2);
            assert_eq!(b.n_items(), 2);
            assert!(MENU_CACHE.with(|c| c.borrow().contains_key(key)));
        });
        if !ran {
            eprintln!("skipping menu_for_key_caches_by_key: no display");
        }
    }

    #[test]
    fn menu_for_key_separate_keys_separate_entries() {
        let ran = crate::test_gtk::run_gtk(|| {
            MENU_CACHE.with(|c| c.borrow_mut().clear());
            let def = sample_def_single(RowMenuVerb::Copy, "Copy");
            let kb = lixun_config::Keybindings::default();
            let _ = menu_for_key("a.key", &def, &kb);
            let _ = menu_for_key("b.key", &def, &kb);
            let len = MENU_CACHE.with(|c| c.borrow().len());
            assert!(len >= 2);
        });
        if !ran {
            eprintln!("skipping menu_for_key_separate_keys_separate_entries: no display");
        }
    }

    #[test]
    fn menu_for_key_skips_openwith_items_in_shared_model() {
        let ran = crate::test_gtk::run_gtk(menu_for_key_skips_openwith_assertions);
        if !ran {
            eprintln!("skipping menu_for_key_skips_openwith_items_in_shared_model: no display");
        }
    }

    fn menu_for_key_skips_openwith_assertions() {
        MENU_CACHE.with(|c| c.borrow_mut().clear());
        let def = RowMenuDef {
            items: vec![
                RowMenuItem {
                    label: "Open".into(),
                    verb: RowMenuVerb::Open,
                    visibility: Default::default(),
                },
                RowMenuItem {
                    label: "Open With".into(),
                    verb: RowMenuVerb::OpenWith,
                    visibility: Default::default(),
                },
            ],
        };
        let kb = lixun_config::Keybindings::default();
        let menu = menu_for_key("openwith.key", &def, &kb);
        // Open + ranking section only — OpenWith is per-hit and must
        // never enter the shared cached model.
        assert_eq!(menu.n_items(), 2);
        assert_eq!(
            menu_openwith_label(&def).as_deref(),
            Some("Open With")
        );
        assert!(menu_openwith_label(&sample_def_single(RowMenuVerb::Open, "Open")).is_none());
    }

    #[test]
    fn synthetic_doc_id_namespace() {
        assert!(is_synthetic_doc_id("history:0:foo"));
        assert!(is_synthetic_doc_id("gui-web:foo"));
        assert!(is_synthetic_doc_id("gui-more:120:foo"));
        assert!(is_more_results_doc_id("gui-more:120:foo"));
        assert!(!is_synthetic_doc_id("fs:/tmp/foo"));
        assert!(!is_more_results_doc_id("fs:/tmp/foo"));
    }

    #[test]
    fn synthetic_web_search_hit_builds_engine_url() {
        let hit = synthetic_web_search_hit("rust gtk", "https://duckduckgo.com/?q={query}");
        assert_eq!(hit.id.0, "gui-web:rust gtk");
        match &hit.action {
            Action::OpenUri { uri } => {
                assert_eq!(uri, "https://duckduckgo.com/?q=rust+gtk");
            }
            other => panic!("expected OpenUri, got {other:?}"),
        }
        assert_eq!(hit.kind_label.as_deref(), Some("Web Search"));
    }

    #[test]
    fn synthetic_more_results_hit_shape() {
        let hit = synthetic_more_results_hit("foo", 120);
        assert_eq!(hit.id.0, "gui-more:120:foo");
        assert_eq!(hit.title, "Show more results");
        assert!(matches!(&hit.action, Action::ReplaceQuery { q } if q == "foo"));
    }

    #[test]
    fn parse_score_multipliers_extracts_named_values() {
        let expl = "score=2.1000 = tantivy(1.2000) \u{d7} cat(1.300) \u{d7} exact(1.000) \
                    \u{d7} prefix(1.400) \u{d7} acronym(1.000) \u{d7} recency(1.050) \
                    \u{d7} coord(1.000) \u{d7} stage2(1.350)";
        let mults = parse_score_multipliers(expl);
        let get = |n: &str| mults.iter().find(|(k, _)| k == n).map(|(_, v)| *v);
        assert_eq!(get("cat"), Some(1.3));
        assert_eq!(get("stage2"), Some(1.35));
        assert_eq!(get("tantivy"), None, "base score is not a multiplier");
        assert_eq!(get("score"), None);
    }

    #[test]
    fn ranking_summary_picks_top_non_neutral() {
        let expl = "score=2.1 = tantivy(1.2) \u{d7} cat(1.300) \u{d7} exact(1.000) \
                    \u{d7} prefix(1.400) \u{d7} acronym(1.000) \u{d7} recency(1.000) \
                    \u{d7} coord(1.000) \u{d7} stage2(1.350)";
        let summary = ranking_summary(expl).expect("non-neutral multipliers present");
        assert!(summary.starts_with("Ranked up:"), "got: {summary}");
        assert!(summary.contains("title starts with query"), "got: {summary}");
        assert!(summary.contains("opened often"), "got: {summary}");
        // Neutral breakdown → no summary line at all.
        let neutral = "score=1.0 = tantivy(1.0) \u{d7} cat(1.000) \u{d7} exact(1.000) \
                       \u{d7} prefix(1.000) \u{d7} acronym(1.000) \u{d7} recency(1.000) \
                       \u{d7} coord(1.000) \u{d7} stage2(1.000)";
        assert!(ranking_summary(neutral).is_none());
        // Below-1.0 multipliers read as ranked down.
        let down = "score=0.9 = tantivy(1.0) \u{d7} cat(0.900) \u{d7} exact(1.000) \
                    \u{d7} prefix(1.000) \u{d7} acronym(1.000) \u{d7} recency(1.000) \
                    \u{d7} coord(1.000) \u{d7} stage2(1.000)";
        let summary = ranking_summary(down).expect("down multiplier present");
        assert!(summary.contains("Ranked down:"), "got: {summary}");
        assert!(summary.contains("category weight"), "got: {summary}");
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
    fn display_path_subtitle_contracts_home_and_strips_filename() {
        let home = std::path::Path::new("/home/user");
        assert_eq!(
            display_path_subtitle("/home/user/docs/deep/report.pdf", Some(home)),
            "~/docs/deep"
        );
        assert_eq!(display_path_subtitle("/home/user/report.pdf", Some(home)), "~");
    }

    #[test]
    fn display_path_subtitle_outside_home_keeps_absolute_dir() {
        let home = std::path::Path::new("/home/user");
        assert_eq!(
            display_path_subtitle("/etc/systemd/system/foo.service", Some(home)),
            "/etc/systemd/system"
        );
        assert_eq!(display_path_subtitle("/foo.txt", Some(home)), "/");
    }

    #[test]
    fn display_path_subtitle_root_and_no_home() {
        assert_eq!(display_path_subtitle("/", None), "/");
        assert_eq!(display_path_subtitle("/home/user/a.txt", None), "/home/user");
    }

    #[test]
    fn human_size_formats_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(999), "999 B");
        assert_eq!(human_size(1000), "1.0 KB");
        assert_eq!(human_size(1_234_000), "1.2 MB");
        assert_eq!(human_size(5_000_000_000), "5.0 GB");
    }

    #[test]
    fn relative_age_buckets() {
        let now = 1_000_000_000i64;
        assert_eq!(relative_age(now - 30, now), "now");
        assert_eq!(relative_age(now - 120, now), "2m ago");
        assert_eq!(relative_age(now - 7_200, now), "2h ago");
        assert_eq!(relative_age(now - 2 * 86_400, now), "2d ago");
        assert_eq!(relative_age(now - 70 * 86_400, now), "2mo ago");
        assert_eq!(relative_age(now - 800 * 86_400, now), "2y ago");
        // Future timestamps clamp to "now", never negative ages.
        assert_eq!(relative_age(now + 500, now), "now");
    }

    #[test]
    fn highlight_ranges_case_insensitive() {
        let ranges = highlight_ranges("Quarterly Report", "rep");
        assert_eq!(ranges, vec![(10, 13)]);
    }

    #[test]
    fn highlight_ranges_diacritic_folded() {
        // Query without accents must land on the accented bytes,
        // mirroring the index tokenizer's folding.
        let text = "R\u{e9}sum\u{e9}.pdf";
        let ranges = highlight_ranges(text, "resume");
        assert_eq!(ranges.len(), 1);
        let (s, e) = ranges[0];
        assert_eq!(&text.as_bytes()[s as usize..e as usize], "R\u{e9}sum\u{e9}".as_bytes());
    }

    #[test]
    fn highlight_ranges_multiple_tokens_and_misses() {
        let ranges = highlight_ranges("alpha beta gamma", "beta zzz");
        assert_eq!(ranges, vec![(6, 10)]);
        assert!(highlight_ranges("anything", "").is_empty());
    }

    #[test]
    fn top_hit_doc_id_roundtrip() {
        cache_top_hit_doc_id(Some("app:editor-a".into()));
        assert!(is_top_hit_doc("app:editor-a"));
        assert!(!is_top_hit_doc("app:browser-b"));
        assert!(!is_top_hit_doc(""));
        cache_top_hit_doc_id(None);
        assert!(!is_top_hit_doc("app:editor-a"));
    }
}
