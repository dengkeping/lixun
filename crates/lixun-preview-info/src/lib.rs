//! Catch-all "Get Info" preview plugin.
//!
//! Every other preview plugin declines hits outside its domain, so
//! before this plugin existed a Space press on an app, calculator,
//! or shell row produced *nothing*: the host bailed before
//! rebuilding the header, the daemon saw only a log-line error,
//! and the launcher was left wedged in phantom preview mode (P2).
//! This plugin scores 1 — the lowest positive score — for EVERY
//! hit, so `select_plugin` always resolves and Space always shows
//! at least a Spotlight-style info card: large icon, title, kind,
//! size, modified date, and location, all read straight off the
//! generic `Hit` (no filesystem access, trivially within the
//! ≤50 ms build contract).
//!
//! Specialized plugins outbid it by construction: the lowest
//! domain score in the tree is 50 (text's extension match), so a
//! score of 1 never shadows a real renderer.

use gtk::prelude::*;
use lixun_core::{Category, Hit};
use lixun_preview::{PreviewPlugin, PreviewPluginCfg, PreviewPluginEntry, SizingPreference};

pub struct InfoPreview;

impl PreviewPlugin for InfoPreview {
    fn id(&self) -> &'static str {
        "info"
    }

    fn match_score(&self, _hit: &Hit) -> u32 {
        // Unconditional floor score: every hit previews as an info
        // card when nothing more specific claims it.
        1
    }

    fn sizing(&self) -> SizingPreference {
        SizingPreference::FitToContent
    }

    fn build(&self, hit: &Hit, _cfg: &PreviewPluginCfg<'_>) -> anyhow::Result<gtk::Widget> {
        let card = gtk::Box::new(gtk::Orientation::Vertical, 8);
        card.set_halign(gtk::Align::Center);
        card.set_valign(gtk::Align::Center);
        card.set_hexpand(true);
        card.set_vexpand(true);
        card.set_margin_top(32);
        card.set_margin_bottom(32);
        card.set_margin_start(48);
        card.set_margin_end(48);
        card.add_css_class("lixun-preview-info");

        let icon = gtk::Image::new();
        icon.set_pixel_size(128);
        icon.set_margin_bottom(8);
        icon.add_css_class("lixun-preview-info-icon");
        icon.set_icon_name(Some(resolve_icon_name(hit)));
        card.append(&icon);

        let title = gtk::Label::new(Some(&hit.title));
        title.set_wrap(true);
        title.set_justify(gtk::Justification::Center);
        title.set_max_width_chars(40);
        title.add_css_class("lixun-preview-info-title");
        card.append(&title);

        let kind_text = hit
            .kind_label
            .clone()
            .unwrap_or_else(|| category_kind(hit.category).to_string());
        let kind = gtk::Label::new(Some(&kind_text));
        kind.add_css_class("lixun-preview-info-kind");
        card.append(&kind);

        // Detail rows, Spotlight Get-Info style. Only present
        // fields render — an app has no size, a calculator result
        // no path.
        let details = gtk::Box::new(gtk::Orientation::Vertical, 4);
        details.set_margin_top(16);
        details.set_halign(gtk::Align::Center);
        details.add_css_class("lixun-preview-info-details");

        if let Some(size) = hit.size {
            details.append(&detail_row("Size", &human_bytes(size)));
        }
        if let Some(ts) = hit.timestamp {
            details.append(&detail_row("Modified", &format_timestamp(ts)));
        }
        if !hit.subtitle.is_empty() {
            details.append(&detail_row("Where", &hit.subtitle));
        }
        card.append(&details);

        tracing::info!(
            "info: rendered card for {} (category={:?})",
            hit.id.0,
            hit.category
        );
        Ok(card.upcast())
    }
}

/// One "Label   value" detail row.
fn detail_row(label: &str, value: &str) -> gtk::Widget {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.set_halign(gtk::Align::Center);

    let key = gtk::Label::new(Some(label));
    key.set_xalign(1.0);
    key.set_width_chars(9);
    key.add_css_class("lixun-preview-info-detail-key");
    row.append(&key);

    let val = gtk::Label::new(Some(value));
    val.set_xalign(0.0);
    val.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    val.set_max_width_chars(48);
    val.set_selectable(true);
    val.set_tooltip_text(Some(value));
    val.add_css_class("lixun-preview-info-detail-value");
    row.append(&val);

    row.upcast()
}

/// Icon for the card: the hit's own `icon_name` when it is a theme
/// name (absolute paths need the launcher's texture loader, which
/// this plugin deliberately avoids — no I/O in build), otherwise a
/// category-generic fallback. The category → icon mapping is
/// plugin-local domain knowledge; hosts never carry it.
fn resolve_icon_name(hit: &Hit) -> &str {
    match hit.icon_name.as_deref() {
        Some(name) if !name.starts_with('/') => name,
        _ => category_icon(hit.category),
    }
}

fn category_icon(cat: Category) -> &'static str {
    match cat {
        Category::App => "application-x-executable",
        Category::File => "text-x-generic",
        Category::Mail => "mail-message",
        Category::Attachment => "mail-attachment",
        Category::Calculator => "accessories-calculator",
        Category::Shell => "utilities-terminal",
    }
}

fn category_kind(cat: Category) -> &'static str {
    match cat {
        Category::App => "Application",
        Category::File => "File",
        Category::Mail => "Email",
        Category::Attachment => "Attachment",
        Category::Calculator => "Calculator",
        Category::Shell => "Shell command",
    }
}

/// Humanize a byte count: `42 B`, `1.2 KB`, `3.5 MB`, `2.0 GB`.
/// Decimal units to match what file managers show users.
fn human_bytes(n: u64) -> String {
    const UNITS: &[(&str, u64)] = &[
        ("GB", 1_000_000_000),
        ("MB", 1_000_000),
        ("KB", 1_000),
    ];
    for (unit, factor) in UNITS {
        if n >= *factor {
            return format!("{:.1} {}", n as f64 / *factor as f64, unit);
        }
    }
    format!("{} B", n)
}

/// Absolute local date-time for a Unix timestamp, e.g.
/// `12 Mar 2026 14:05`. Out-of-range values render as a dash
/// rather than panicking.
fn format_timestamp(ts: i64) -> String {
    match chrono::DateTime::from_timestamp(ts, 0) {
        Some(utc) => utc
            .with_timezone(&chrono::Local)
            .format("%-d %b %Y %H:%M")
            .to_string(),
        None => "—".to_string(),
    }
}

inventory::submit! {
    PreviewPluginEntry {
        factory: || Box::new(InfoPreview),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lixun_core::{Action, DocId};
    use std::path::PathBuf;

    fn hit(category: Category, action: Action) -> Hit {
        Hit {
            id: DocId("test:x".into()),
            category,
            title: "X".into(),
            subtitle: String::new(),
            icon_name: None,
            kind_label: None,
            score: 1.0,
            action,
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
    fn scores_one_for_every_hit() {
        // Floor score: 1 for a file hit AND for hits no other plugin
        // takes (apps, calculator, shell). The lowest specialized
        // score in the tree is 50 (text's extension match), so 1 can
        // never shadow a real renderer.
        let file = hit(
            Category::File,
            Action::OpenFile {
                path: PathBuf::from("/tmp/x.txt"),
            },
        );
        assert_eq!(InfoPreview.match_score(&file), 1);

        let app = hit(
            Category::App,
            Action::Launch {
                exec: vec!["true".into()],
                terminal: false,
                desktop_id: None,
                desktop_file: None,
                working_dir: None,
            },
        );
        assert_eq!(InfoPreview.match_score(&app), 1);
        assert!(InfoPreview.match_score(&app) < 50);

        let calc = hit(
            Category::Calculator,
            Action::CopyText { text: "4".into() },
        );
        assert_eq!(InfoPreview.match_score(&calc), 1);
    }

    #[test]
    fn human_bytes_decimal_units() {
        assert_eq!(human_bytes(42), "42 B");
        assert_eq!(human_bytes(1_200), "1.2 KB");
        assert_eq!(human_bytes(3_500_000), "3.5 MB");
        assert_eq!(human_bytes(2_000_000_000), "2.0 GB");
    }

    #[test]
    fn format_timestamp_renders_and_survives_extremes() {
        let s = format_timestamp(1_726_000_000);
        assert!(s.contains("2024"), "unexpected render: {s}");
        assert_eq!(format_timestamp(i64::MAX), "—");
    }

    #[test]
    fn icon_name_prefers_theme_names_and_rejects_paths() {
        let mut h = hit(
            Category::App,
            Action::Launch {
                exec: vec!["true".into()],
                terminal: false,
                desktop_id: None,
                desktop_file: None,
                working_dir: None,
            },
        );
        h.icon_name = Some("firefox".into());
        assert_eq!(resolve_icon_name(&h), "firefox");
        h.icon_name = Some("/usr/share/icons/x.png".into());
        assert_eq!(resolve_icon_name(&h), "application-x-executable");
        h.icon_name = None;
        assert_eq!(resolve_icon_name(&h), "application-x-executable");
    }
}
