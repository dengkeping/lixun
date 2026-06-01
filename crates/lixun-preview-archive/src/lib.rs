//! Archive listing preview plugin.
//!
//! Shows the contents of `zip`, `tar` (plain / gzip / zstd / bzip2)
//! and `7z` containers as a flat, read-only entry list. Nothing is
//! ever extracted to disk: enumeration is header-only (see
//! [`listing`]). Encrypted, malformed and oversized archives degrade
//! to an inline message rather than panicking or blocking.
//!
//! The widget owns its own scrolling ([`SizingPreference::OwnsScroll`]):
//! a fixed header strip stays pinned while the entry `ListBox` scrolls
//! beneath it. Listing runs off the UI thread via
//! `gio::spawn_blocking`, with a placeholder shown until results land.

mod listing;

use std::path::PathBuf;

use gtk::glib;
use gtk::prelude::*;
use lixun_core::{Action, Hit};
use lixun_preview::{PreviewPlugin, PreviewPluginCfg, PreviewPluginEntry, SizingPreference};

use listing::{ArchiveError, ArchiveListing, Entry};

/// Upper bound on entries rendered for a single archive. Archives
/// with more members are listed up to this point and flagged as
/// truncated so the UI stays responsive on pathological inputs.
const ENTRY_CAP: usize = 5000;

pub struct ArchivePreview;

impl PreviewPlugin for ArchivePreview {
    fn id(&self) -> &'static str {
        "archive"
    }

    fn match_score(&self, hit: &Hit) -> u32 {
        // A directory may legitimately be named `foo.zip`; decline it
        // so the folder plugin keeps ownership of directory hits.
        if hit.kind_label.as_deref() == Some("Folder") {
            return 0;
        }

        let path = match &hit.action {
            Action::OpenFile { path } | Action::ShowInFileManager { path } => path,
            _ => return 0,
        };

        if listing::detect_format(path).is_some() {
            80
        } else {
            0
        }
    }

    fn sizing(&self) -> SizingPreference {
        SizingPreference::OwnsScroll
    }

    fn build(&self, hit: &Hit, cfg: &PreviewPluginCfg<'_>) -> anyhow::Result<gtk::Widget> {
        let path = match &hit.action {
            Action::OpenFile { path } | Action::ShowInFileManager { path } => path.clone(),
            _ => anyhow::bail!(
                "archive plugin: hit category={:?} has no renderable source",
                hit.category
            ),
        };

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("lixun-preview-archive");

        let header = build_header(&path);
        root.append(&header);

        let scroll = gtk::ScrolledWindow::new();
        scroll.set_hscrollbar_policy(gtk::PolicyType::Automatic);
        scroll.set_vscrollbar_policy(gtk::PolicyType::Automatic);
        scroll.set_min_content_width(640);
        scroll.set_min_content_height(360);
        scroll.set_vexpand(true);

        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::None);
        list.add_css_class("lixun-preview-archive-entries");
        list.append(&status_row("Reading archive…"));
        scroll.set_child(Some(&list));
        root.append(&scroll);

        let max_bytes = cfg.max_file_size_mb.saturating_mul(1024 * 1024);
        spawn_listing(path, max_bytes, list);

        Ok(root.upcast())
    }
}

fn build_header(path: &std::path::Path) -> gtk::Widget {
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    header.set_margin_top(12);
    header.set_margin_bottom(12);
    header.set_margin_start(16);
    header.set_margin_end(16);
    header.add_css_class("lixun-preview-archive-header");

    let icon = gtk::Image::from_icon_name("package-x-generic");
    icon.set_pixel_size(32);
    header.append(&icon);

    let title = gtk::Label::new(Some(
        &path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned()),
    ));
    title.set_xalign(0.0);
    title.set_hexpand(true);
    title.add_css_class("title");
    header.append(&title);

    header.upcast()
}

/// Run the (blocking) enumeration off the UI thread and populate the
/// `ListBox` when it completes, keeping `build` well under its budget.
fn spawn_listing(path: PathBuf, max_bytes: u64, list: gtk::ListBox) {
    let work_path = path.clone();
    glib::spawn_future_local(async move {
        let outcome = gtk::gio::spawn_blocking(move || enumerate(&work_path, max_bytes))
            .await
            .unwrap_or_else(|_| Err(ArchiveError::Io("listing task cancelled".to_string())));

        clear_rows(&list);

        match outcome {
            Ok(result) => populate(&list, result),
            Err(err) => list.append(&status_row(&err.to_string())),
        }
    });
}

/// Size-gate then enumerate. Kept separate so the gate runs on the
/// blocking thread alongside the read, never on the UI thread.
fn enumerate(path: &std::path::Path, max_bytes: u64) -> Result<ArchiveListing, ArchiveError> {
    if max_bytes > 0
        && let Ok(meta) = std::fs::metadata(path)
        && meta.len() > max_bytes
    {
        return Err(ArchiveError::Io(format!(
            "archive exceeds the {} MiB preview limit",
            max_bytes / (1024 * 1024)
        )));
    }
    listing::list_entries(path, ENTRY_CAP)
}

fn populate(list: &gtk::ListBox, result: ArchiveListing) {
    if result.entries.is_empty() {
        list.append(&status_row("Archive is empty"));
        return;
    }

    for entry in &result.entries {
        list.append(&entry_row(entry));
    }

    if result.truncated {
        list.append(&status_row(&format!(
            "… listing truncated at {ENTRY_CAP} entries"
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

    let icon_name = if entry.is_dir {
        "folder"
    } else {
        "text-x-generic"
    };
    let icon = gtk::Image::from_icon_name(icon_name);
    hbox.append(&icon);

    let name = gtk::Label::new(Some(&entry.name));
    name.set_xalign(0.0);
    name.set_hexpand(true);
    name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    hbox.append(&name);

    if !entry.is_dir {
        let size = gtk::Label::new(Some(&human_bytes(entry.size)));
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
        factory: || Box::new(ArchivePreview),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lixun_core::{Category, DocId};
    use std::path::PathBuf;

    fn archive_hit(path: impl Into<PathBuf>, kind_label: Option<&str>) -> Hit {
        let path = path.into();
        Hit {
            id: DocId("fs:test".into()),
            category: Category::File,
            title: path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            subtitle: path.display().to_string(),
            icon_name: None,
            kind_label: kind_label.map(|s| s.to_string()),
            score: 1.0,
            action: Action::OpenFile { path },
            extract_fail: false,
            sender: None,
            recipients: None,
            body: None,
            secondary_action: None,
            source_instance: String::new(),
            row_menu: lixun_core::RowMenuDef::empty(),
            mime: None,
        }
    }

    #[test]
    fn zip_extension_scores_eighty() {
        let hit = archive_hit("/tmp/photos.zip", None);
        assert_eq!(ArchivePreview.match_score(&hit), 80);
    }

    #[test]
    fn double_extension_tarball_scores_eighty() {
        let hit = archive_hit("/tmp/backup.tar.gz", None);
        assert_eq!(ArchivePreview.match_score(&hit), 80);
    }

    #[test]
    fn sevenz_extension_scores_eighty() {
        let hit = archive_hit("/tmp/data.7z", None);
        assert_eq!(ArchivePreview.match_score(&hit), 80);
    }

    #[test]
    fn directory_named_like_archive_is_declined() {
        let hit = archive_hit("/tmp/foo.zip", Some("Folder"));
        assert_eq!(ArchivePreview.match_score(&hit), 0);
    }

    #[test]
    fn non_archive_file_is_declined() {
        let hit = archive_hit("/tmp/notes.txt", None);
        assert_eq!(ArchivePreview.match_score(&hit), 0);
    }

    #[test]
    fn non_path_action_is_declined() {
        let hit = Hit {
            id: DocId("calc:1".into()),
            category: Category::Calculator,
            title: "42".into(),
            subtitle: String::new(),
            icon_name: None,
            kind_label: None,
            score: 1.0,
            action: Action::ReplaceQuery { q: "42".into() },
            extract_fail: false,
            sender: None,
            recipients: None,
            body: None,
            secondary_action: None,
            source_instance: String::new(),
            row_menu: lixun_core::RowMenuDef::empty(),
            mime: None,
        };
        assert_eq!(ArchivePreview.match_score(&hit), 0);
    }
}
