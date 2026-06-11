//! Folder listing preview plugin.
//!
//! Renders the immediate contents of a directory hit: a header
//! strip showing the folder name and a summary ("N files, M
//! folders · total size"), then a scrollable [`gtk::ListBox`] of
//! children — directories first, then files, alphabetical within
//! each group. Subdirectories are NOT recursed into; `total_size`
//! covers immediate regular files only so the listing stays O(n)
//! in immediate children even on deep trees.
//!
//! The widget owns its own scrolling
//! ([`SizingPreference::OwnsScroll`]): the header strip stays
//! pinned while the entry list scrolls beneath it. Enumeration
//! runs off the UI thread via `gio::spawn_blocking`, with a
//! placeholder shown until results land.
//!
//! Producer contract: a directory is indexed as
//! `Hit { kind_label: Some("Folder"), action: Action::OpenFile { path }, .. }`.
//! `match_score` keys off `kind_label` alone (no filesystem access)
//! and yields 80; the archive plugin declines folder hits so
//! ownership is uncontested. Launch falls through to the trait's
//! default implementation, which dispatches `Action::OpenFile`
//! through `xdg-open` without naming any concrete file manager.

mod listing;

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;
use lixun_core::{Action, Hit};
use lixun_preview::{PreviewPlugin, PreviewPluginCfg, PreviewPluginEntry, SizingPreference};

use listing::{DirError, DirListing, Entry, EntryKind, list_dir};

/// Upper bound on entries rendered for a single folder. Larger
/// directories list up to this point and surface a "truncated"
/// status row so the UI stays responsive on pathological inputs.
const ENTRY_CAP: usize = 5000;

pub struct FolderPreview;

impl PreviewPlugin for FolderPreview {
    fn id(&self) -> &'static str {
        "folder"
    }

    fn match_score(&self, hit: &Hit) -> u32 {
        // The indexer flags directories via `kind_label`, not via a
        // dedicated `Category` variant. Stay cheap — no filesystem
        // access here.
        if hit.kind_label.as_deref() == Some("Folder") {
            80
        } else {
            0
        }
    }

    fn sizing(&self) -> SizingPreference {
        SizingPreference::OwnsScroll
    }

    fn build(&self, hit: &Hit, _cfg: &PreviewPluginCfg<'_>) -> anyhow::Result<gtk::Widget> {
        let path = match &hit.action {
            Action::OpenFile { path } | Action::ShowInFileManager { path } => path.clone(),
            _ => anyhow::bail!(
                "folder plugin: hit category={:?} has no renderable source",
                hit.category
            ),
        };

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("lixun-preview-folder");

        // The subtitle starts blank — it fills in once the off-
        // thread listing returns. We keep a clone of the label so
        // the spawned future can mutate it after build() has
        // already returned to the host.
        let (header, subtitle) = build_header(&path);
        root.append(&header);

        let scroll = gtk::ScrolledWindow::new();
        scroll.set_hscrollbar_policy(gtk::PolicyType::Automatic);
        scroll.set_vscrollbar_policy(gtk::PolicyType::Automatic);
        scroll.set_min_content_width(640);
        scroll.set_min_content_height(360);
        scroll.set_vexpand(true);

        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::None);
        list.add_css_class("lixun-preview-folder-entries");
        list.append(&status_row("Reading folder…"));
        scroll.set_child(Some(&list));
        root.append(&scroll);

        // Listing-generation counter owned by this widget (same
        // epoch idiom as the pdf plugin's render pipeline): every
        // spawn_listing call bumps it and stamps the in-flight
        // future, so a slow listing that completes after a newer
        // one was spawned for the same widget is discarded instead
        // of overwriting the fresh rows. build() is currently the
        // only spawner; any future in-place update path must reuse
        // this same counter when it re-lists.
        let epoch = Rc::new(Cell::new(0u64));
        spawn_listing(path, list, subtitle, epoch);

        Ok(root.upcast())
    }
}

/// Build the pinned header strip and return both the header widget
/// (to attach to the root) and a clone of the subtitle `Label` (so
/// the off-thread listing can fill it in once results land).
///
/// GTK widget handles are refcounted — the clone returned here and
/// the one appended into the header point at the same underlying
/// widget.
fn build_header(path: &std::path::Path) -> (gtk::Widget, gtk::Label) {
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    header.set_margin_top(12);
    header.set_margin_bottom(12);
    header.set_margin_start(16);
    header.set_margin_end(16);
    header.add_css_class("lixun-preview-folder-header");

    let icon = gtk::Image::from_icon_name("folder");
    icon.set_pixel_size(32);
    header.append(&icon);

    let titles = gtk::Box::new(gtk::Orientation::Vertical, 2);
    titles.set_hexpand(true);

    let title = gtk::Label::new(Some(
        &path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned()),
    ));
    title.set_xalign(0.0);
    title.set_hexpand(true);
    title.add_css_class("title");
    titles.append(&title);

    let subtitle = gtk::Label::new(None);
    subtitle.set_xalign(0.0);
    subtitle.set_hexpand(true);
    subtitle.add_css_class("dim-label");
    titles.append(&subtitle);

    header.append(&titles);

    (header.upcast(), subtitle)
}

/// Run the (blocking) enumeration off the UI thread and populate
/// the `ListBox` when it completes, keeping `build` well under its
/// budget. The `subtitle` clone receives the "N files, M folders ·
/// size" summary once results land.
///
/// `epoch` guards against out-of-order completion: each call bumps
/// the widget-owned counter and captures the new value; if another
/// listing was spawned for the same widget before this one
/// finished, the captured value no longer matches and the stale
/// result is dropped without touching the rows.
fn spawn_listing(path: PathBuf, list: gtk::ListBox, subtitle: gtk::Label, epoch: Rc<Cell<u64>>) {
    let this_epoch = epoch.get().wrapping_add(1);
    epoch.set(this_epoch);
    let work_path = path.clone();
    glib::spawn_future_local(async move {
        let outcome = gtk::gio::spawn_blocking(move || list_dir(&work_path, ENTRY_CAP))
            .await
            .unwrap_or_else(|_| Err(DirError::Io("listing task cancelled".to_string())));

        if epoch.get() != this_epoch {
            tracing::info!(
                "folder: discarding stale listing for {:?} (epoch {} superseded by {})",
                path,
                this_epoch,
                epoch.get()
            );
            return;
        }

        clear_rows(&list);

        match outcome {
            Ok(result) => {
                subtitle.set_text(&summary_line(&result));
                populate(&list, result);
            }
            Err(err) => {
                subtitle.set_text("");
                list.append(&status_row(&err.to_string()));
            }
        }
    });
}

fn summary_line(result: &DirListing) -> String {
    format!(
        "{} files, {} folders · {}",
        result.files,
        result.dirs,
        human_bytes(result.total_size)
    )
}

fn populate(list: &gtk::ListBox, result: DirListing) {
    if result.entries.is_empty() {
        list.append(&status_row("Folder is empty"));
        return;
    }

    for entry in &result.entries {
        list.append(&entry_row(entry));
    }

    if result.truncated {
        list.append(&status_row(&format!(
            "… and more — listing truncated at {ENTRY_CAP} entries"
        )));
    }
}

fn entry_row(entry: &Entry) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::new();
    let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    hbox.set_margin_top(4);
    hbox.set_margin_bottom(4);
    hbox.set_margin_start(12);
    hbox.set_margin_end(12);

    let icon_name = match entry.kind {
        EntryKind::Dir => "folder",
        EntryKind::Symlink => "emblem-symbolic-link",
        EntryKind::File | EntryKind::Other => "text-x-generic",
    };
    let icon = gtk::Image::from_icon_name(icon_name);
    hbox.append(&icon);

    let name = gtk::Label::new(Some(&entry.name));
    name.set_xalign(0.0);
    name.set_hexpand(true);
    name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    hbox.append(&name);

    if let Some(bytes) = entry.size {
        let size = gtk::Label::new(Some(&human_bytes(bytes)));
        size.set_xalign(1.0);
        size.add_css_class("dim-label");
        hbox.append(&size);
    }

    row.set_child(Some(&hbox));
    row
}

fn status_row(message: &str) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::new();
    row.set_selectable(false);
    let label = gtk::Label::new(Some(message));
    label.set_xalign(0.0);
    label.set_margin_top(8);
    label.set_margin_bottom(8);
    label.set_margin_start(12);
    label.set_margin_end(12);
    label.add_css_class("dim-label");
    row.set_child(Some(&label));
    row
}

fn clear_rows(list: &gtk::ListBox) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
}

fn human_bytes(n: u64) -> String {
    const UNITS: &[(&str, u64)] = &[
        ("GiB", 1024 * 1024 * 1024),
        ("MiB", 1024 * 1024),
        ("KiB", 1024),
    ];
    for (unit, factor) in UNITS {
        if n >= *factor {
            return format!("{:.1} {}", n as f64 / *factor as f64, unit);
        }
    }
    format!("{n} B")
}

inventory::submit! {
    PreviewPluginEntry {
        factory: || Box::new(FolderPreview),
    }
}
