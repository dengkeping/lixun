//! Email message preview plugin.
//!
//! Parses `.eml` (rfc822) messages and Maildir items via
//! `mail-parser`. Renders:
//!
//! - A header grid (From / To / Cc / Date / Subject) at the top.
//! - A body area preferring `text/plain`; `text/html` parts are
//!   flattened through `html2text` because a full HTML renderer
//!   is out of scope for a preview.
//! - An attachment list (filename + size + content-type) as a
//!   read-only `ListBox`. No recursive preview of attachments in
//!   v1 — that would spawn a second preview window and break the
//!   single-preview invariant in `preview_spawn`.
//!
//! Malformed input never panics: `mail-parser` returns `None` for
//! unparseable bytes and we surface an inline error label
//! containing the string `"unparseable"` so the QA scenario can
//! grep for it.

use std::fs;

use gtk::prelude::*;
use lixun_core::{Action, Category, Hit};
use lixun_preview::{PreviewPlugin, PreviewPluginCfg, PreviewPluginEntry, SizingPreference};
use mail_parser::{Address, MessageParser, MimeHeaders};

const MAX_EMAIL_BYTES: usize = 8 * 1024 * 1024;

pub struct EmailPreview;

impl PreviewPlugin for EmailPreview {
    fn id(&self) -> &'static str {
        "email"
    }

    fn match_score(&self, hit: &Hit) -> u32 {
        let path = match &hit.action {
            Action::OpenFile { path } | Action::ShowInFileManager { path } => path,
            _ => {
                if matches!(hit.category, Category::Mail) {
                    return 60;
                }
                return 0;
            }
        };

        if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("eml"))
        {
            return 85;
        }

        let path_str = path.to_string_lossy();
        if (path_str.contains("/cur/") || path_str.contains("/new/"))
            && file_looks_like_rfc822(path)
        {
            return 70;
        }

        if matches!(hit.category, Category::Mail) {
            return 60;
        }

        0
    }

    fn sizing(&self) -> SizingPreference {
        SizingPreference::FitToContent
    }

    fn build(&self, hit: &Hit, _cfg: &PreviewPluginCfg<'_>) -> anyhow::Result<gtk::Widget> {
        let path = match &hit.action {
            Action::OpenFile { path } | Action::ShowInFileManager { path } => path.clone(),
            _ if hit.category == Category::Mail => {
                return Ok(build_from_hit_fields(hit));
            }
            _ => anyhow::bail!(
                "email plugin: hit category={:?} has no renderable source",
                hit.category
            ),
        };

        let raw = fs::read(&path)?;
        let capped: &[u8] = if raw.len() > MAX_EMAIL_BYTES {
            &raw[..MAX_EMAIL_BYTES]
        } else {
            &raw
        };

        let parser = MessageParser::default();
        let message = match parser.parse(capped) {
            Some(m) => m,
            None => return Ok(error_widget("unparseable: mail-parser rejected the bytes")),
        };

        let vbox = gtk::Box::new(gtk::Orientation::Vertical, 8);
        vbox.set_margin_top(12);
        vbox.set_margin_bottom(12);
        vbox.set_margin_start(16);
        vbox.set_margin_end(16);
        vbox.add_css_class("lixun-preview-email");

        vbox.append(&build_header_grid(&message));

        let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
        separator.set_margin_top(8);
        separator.set_margin_bottom(8);
        vbox.append(&separator);

        vbox.append(&build_body(&message));

        if let Some(list) = build_attachment_list(&message) {
            let heading = gtk::Label::new(Some("Attachments"));
            heading.set_xalign(0.0);
            heading.set_margin_top(12);
            heading.add_css_class("lixun-preview-email-attachments-heading");
            vbox.append(&heading);
            vbox.append(&list);
        }

        let scroll = gtk::ScrolledWindow::new();
        scroll.set_child(Some(&vbox));
        scroll.set_hscrollbar_policy(gtk::PolicyType::Automatic);
        scroll.set_vscrollbar_policy(gtk::PolicyType::Automatic);
        // See preview-text for the natural-width-zero rationale.
        // Email needs more horizontal room than a plain text file
        // because the header grid wants ~two columns of readable
        // width plus the body paragraph wraps beside it.
        scroll.set_min_content_width(720);
        scroll.set_min_content_height(320);

        tracing::info!("email: rendered {:?} bytes={}", path, capped.len());
        Ok(scroll.upcast())
    }
}

fn build_from_hit_fields(hit: &Hit) -> gtk::Widget {
    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 8);
    vbox.set_margin_top(12);
    vbox.set_margin_bottom(12);
    vbox.set_margin_start(16);
    vbox.set_margin_end(16);
    vbox.add_css_class("lixun-preview-email");

    // Skip Subject (already in preview-bin's main header as `title`)
    // and From (already as `subtitle`). Showing them again here is
    // visual duplication that triggered the BUG-4 followup. Only
    // surface headers that the main preview header does NOT carry —
    // today that's just the recipients list.
    if let Some(recipients) = hit.recipients.as_deref().filter(|s| !s.is_empty()) {
        let grid = gtk::Grid::new();
        grid.set_row_spacing(4);
        grid.set_column_spacing(12);
        grid.add_css_class("lixun-preview-email-headers");

        let key = gtk::Label::new(Some("To"));
        key.set_xalign(1.0);
        key.add_css_class("lixun-preview-email-header-key");
        grid.attach(&key, 0, 0, 1, 1);

        let val = gtk::Label::new(Some(recipients));
        val.set_xalign(0.0);
        val.set_selectable(true);
        val.set_wrap(true);
        val.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        val.add_css_class("lixun-preview-email-header-value");
        grid.attach(&val, 1, 0, 1, 1);

        vbox.append(&grid);

        let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
        separator.set_margin_top(8);
        separator.set_margin_bottom(8);
        vbox.append(&separator);
    }

    let body_text = hit
        .body
        .clone()
        .unwrap_or_else(|| "(body not available — gloda index stores a snippet only; open the message in Thunderbird for the full body)".into());
    let buffer = gtk::TextBuffer::new(None);
    buffer.set_text(&body_text);
    let view = gtk::TextView::with_buffer(&buffer);
    view.set_editable(false);
    view.set_cursor_visible(false);
    view.set_wrap_mode(gtk::WrapMode::WordChar);
    view.set_left_margin(4);
    view.set_right_margin(4);
    view.add_css_class("lixun-preview-email-body");
    vbox.append(&view);

    let scroll = gtk::ScrolledWindow::new();
    scroll.set_child(Some(&vbox));
    scroll.set_hscrollbar_policy(gtk::PolicyType::Automatic);
    scroll.set_vscrollbar_policy(gtk::PolicyType::Automatic);
    // Match the .eml path's floor so gloda-path and file-path email
    // previews open at the same visual size for the same message.
    scroll.set_min_content_width(720);
    scroll.set_min_content_height(320);

    tracing::info!(
        "email: rendered from Hit fields (gloda path) hit_id={} body_len={}",
        hit.id.0,
        hit.body.as_deref().map(|s| s.len()).unwrap_or(0)
    );

    scroll.upcast()
}

fn build_header_grid(msg: &mail_parser::Message<'_>) -> gtk::Grid {
    let grid = gtk::Grid::new();
    grid.set_row_spacing(4);
    grid.set_column_spacing(12);
    grid.add_css_class("lixun-preview-email-headers");

    let rows = [
        ("From", format_address(msg.from())),
        ("To", format_address(msg.to())),
        ("Cc", format_address(msg.cc())),
        (
            "Date",
            msg.date()
                .map(|d| d.to_rfc3339())
                .unwrap_or_else(|| "(no date)".into()),
        ),
        (
            "Subject",
            msg.subject().unwrap_or("(no subject)").to_string(),
        ),
    ];

    for (row, (label, value)) in rows.iter().enumerate() {
        let key = gtk::Label::new(Some(label));
        key.set_xalign(1.0);
        key.add_css_class("lixun-preview-email-header-key");
        grid.attach(&key, 0, row as i32, 1, 1);

        let val = gtk::Label::new(Some(value));
        val.set_xalign(0.0);
        val.set_selectable(true);
        val.set_wrap(true);
        val.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        val.add_css_class("lixun-preview-email-header-value");
        grid.attach(&val, 1, row as i32, 1, 1);
    }

    grid
}

fn build_body(msg: &mail_parser::Message<'_>) -> gtk::Widget {
    let text = pick_body_text(msg);

    let buffer = gtk::TextBuffer::new(None);
    buffer.set_text(&text);

    let view = gtk::TextView::with_buffer(&buffer);
    view.set_editable(false);
    view.set_cursor_visible(false);
    view.set_monospace(false);
    view.set_wrap_mode(gtk::WrapMode::WordChar);
    view.set_left_margin(4);
    view.set_right_margin(4);
    view.add_css_class("lixun-preview-email-body");
    view.upcast()
}

fn pick_body_text(msg: &mail_parser::Message<'_>) -> String {
    if let Some(plain) = msg.body_text(0) {
        return plain.into_owned();
    }
    if let Some(html) = msg.body_html(0) {
        return html2text::from_read(html.as_bytes(), 80);
    }
    String::from("(no body)")
}

fn build_attachment_list(msg: &mail_parser::Message<'_>) -> Option<gtk::ListBox> {
    let mut rows: Vec<(String, String, usize)> = Vec::new();
    for att in msg.attachments() {
        let name = att.attachment_name().unwrap_or("(unnamed)").to_string();
        let ct = att
            .content_type()
            .map(|ct| {
                let mut s = ct.ctype().to_string();
                if let Some(sub) = ct.subtype() {
                    s.push('/');
                    s.push_str(sub);
                }
                s
            })
            .unwrap_or_else(|| "application/octet-stream".into());
        let len = att.contents().len();
        rows.push((name, ct, len));
    }
    if rows.is_empty() {
        return None;
    }

    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    list.add_css_class("lixun-preview-email-attachments");

    for (name, ct, len) in rows {
        let row = gtk::ListBoxRow::new();
        let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        hbox.set_margin_top(4);
        hbox.set_margin_bottom(4);
        hbox.set_margin_start(8);
        hbox.set_margin_end(8);

        let name_label = gtk::Label::new(Some(&name));
        name_label.set_xalign(0.0);
        name_label.set_hexpand(true);
        name_label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        hbox.append(&name_label);

        let meta_label = gtk::Label::new(Some(&format!("{} · {}", human_bytes(len as u64), ct)));
        meta_label.add_css_class("lixun-preview-email-attachment-meta");
        hbox.append(&meta_label);

        row.set_child(Some(&hbox));
        list.append(&row);
    }

    Some(list)
}

fn format_address(hv: Option<&Address<'_>>) -> String {
    let Some(addr) = hv else {
        return "(none)".into();
    };
    match addr {
        Address::List(list) => list
            .iter()
            .map(|a| {
                let name = a.name().unwrap_or("");
                let email = a.address().unwrap_or("");
                if name.is_empty() {
                    email.to_string()
                } else {
                    format!("{} <{}>", name, email)
                }
            })
            .collect::<Vec<_>>()
            .join(", "),
        Address::Group(groups) => groups
            .iter()
            .map(|g| g.name.as_deref().unwrap_or("(group)"))
            .collect::<Vec<_>>()
            .join(", "),
    }
}

fn error_widget(msg: &str) -> gtk::Widget {
    let label = gtk::Label::new(Some(msg));
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.set_margin_top(24);
    label.set_margin_bottom(24);
    label.set_margin_start(24);
    label.set_margin_end(24);
    label.add_css_class("lixun-preview-email-error");
    label.upcast()
}

fn file_looks_like_rfc822(path: &std::path::Path) -> bool {
    use std::io::BufRead;
    let Ok(f) = std::fs::File::open(path) else {
        return false;
    };
    let mut reader = std::io::BufReader::new(f);
    let mut first = String::new();
    if reader.read_line(&mut first).is_err() {
        return false;
    }
    first.starts_with("Return-Path:")
        || first.starts_with("Received:")
        || first.starts_with("From ")
        || first.starts_with("Delivered-To:")
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
    format!("{} B", n)
}

inventory::submit! {
    PreviewPluginEntry {
        factory: || Box::new(EmailPreview),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lixun_core::DocId;
    use std::path::PathBuf;

    fn eml_hit(path: impl Into<PathBuf>) -> Hit {
        let path = path.into();
        Hit {
            id: DocId(format!("fs:{}", path.display())),
            category: Category::File,
            title: path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            subtitle: path.display().to_string(),
            icon_name: None,
            kind_label: None,
            score: 1.0,
            action: Action::OpenFile { path },
            extract_fail: false,
            sender: None,
            recipients: None,
            body: None,
            secondary_action: None,
        }
    }

    #[test]
    fn eml_extension_scores_eightyfive() {
        let hit = eml_hit("/tmp/x.eml");
        assert_eq!(EmailPreview.match_score(&hit), 85);
    }

    #[test]
    fn mail_category_without_file_scores_sixty() {
        let hit = Hit {
            id: DocId("mail:msg-1".into()),
            category: Category::Mail,
            title: "Important".into(),
            subtitle: "alice@example.com".into(),
            icon_name: None,
            kind_label: None,
            score: 1.0,
            action: Action::OpenMail {
                message_id: "msg-1".into(),
            },
            extract_fail: false,
            sender: None,
            recipients: None,
            body: None,
            secondary_action: None,
        };
        assert_eq!(EmailPreview.match_score(&hit), 60);
    }

    #[test]
    fn non_email_file_scores_zero() {
        let hit = eml_hit("/tmp/x.txt");
        assert_eq!(EmailPreview.match_score(&hit), 0);
    }

    #[test]
    fn launch_action_scores_zero() {
        let hit = Hit {
            id: DocId("app:firefox".into()),
            category: Category::App,
            title: "Firefox".into(),
            subtitle: String::new(),
            icon_name: None,
            kind_label: None,
            score: 1.0,
            action: Action::Launch {
                exec: "firefox".into(),
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
        };
        assert_eq!(EmailPreview.match_score(&hit), 0);
    }

    #[test]
    fn maildir_path_with_rfc822_content_scores_seventy() {
        let dir = std::env::temp_dir().join(format!("lixun-mail-{}", std::process::id()));
        let cur = dir.join("cur");
        std::fs::create_dir_all(&cur).unwrap();
        let path = cur.join("12345.msg");
        std::fs::write(
            &path,
            b"Return-Path: <alice@example.com>\nFrom: alice\nSubject: hi\n\nbody",
        )
        .unwrap();
        let hit = eml_hit(&path);
        let score = EmailPreview.match_score(&hit);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(score, 70);
    }

    #[test]
    fn maildir_path_without_rfc822_header_scores_zero() {
        let dir = std::env::temp_dir().join(format!("lixun-notmail-{}", std::process::id()));
        let cur = dir.join("cur");
        std::fs::create_dir_all(&cur).unwrap();
        let path = cur.join("12345.msg");
        std::fs::write(&path, b"random content not an email").unwrap();
        let hit = eml_hit(&path);
        let score = EmailPreview.match_score(&hit);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(score, 0);
    }

    #[test]
    fn human_bytes_sensible() {
        assert_eq!(human_bytes(100), "100 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MiB");
    }

    #[test]
    fn gloda_hit_openmail_has_no_path_but_still_matches_email_plugin() {
        // Regression for BUG-4 (gloda preview). An OpenMail hit must
        // score > 0 so `select_plugin` returns email-plugin, and the
        // plugin's build() must accept this branch instead of bailing
        // on 'no file path'. We don't exercise build() here (GTK
        // runtime not available in unit tests) but confirm the
        // score-side contract.
        let hit = Hit {
            id: DocId("mail:42".into()),
            category: Category::Mail,
            title: "Re: invoice #1234".into(),
            subtitle: "alice@example.com".into(),
            icon_name: None,
            kind_label: Some("Email".into()),
            score: 1.0,
            action: Action::OpenMail {
                message_id: "20160426103745.E36A4340033@cron001.example.com".into(),
            },
            extract_fail: false,
            sender: Some("alice@example.com".into()),
            recipients: Some("bob@example.com".into()),
            body: Some("gloda-stored body snippet".into()),
            secondary_action: None,
        };
        assert!(
            EmailPreview.match_score(&hit) >= 60,
            "gloda Mail hit must score >= 60 so email plugin wins"
        );
    }
}
