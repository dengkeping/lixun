//! Plain-text preview plugin.
//!
//! Matches a hit when it points at a known text-ish extension,
//! a `text/*` MIME type, or a file whose first 4 KiB parse as
//! mostly-printable UTF-8. Renders the first 50 KiB of the file
//! in a read-only monospace `TextView`.
//!
//! 50 KiB is a hard cap (not a configurable one): previews must
//! open within the plan's 500 ms budget, and showing megabytes of
//! plain text produces a ScrolledWindow nobody scrolls. The
//! `[preview].max_file_size_mb` config instead controls whether
//! we attempt to preview at all (see `match_score`).

use std::fs::File;
use std::io::Read;
use std::path::Path;

use gtk::prelude::*;
use lixun_core::{Action, Hit};
use lixun_preview::{PreviewPlugin, PreviewPluginCfg, PreviewPluginEntry, SizingPreference};

const DISPLAY_CAP_BYTES: usize = 50 * 1024;
const SNIFF_BYTES: usize = 4 * 1024;
const SNIFF_CTRL_THRESHOLD_PERCENT: usize = 5;

const STRONG_EXTENSIONS: &[&str] = &[
    "txt",
    "log",
    "md",
    "markdown",
    "rst",
    "rs",
    "py",
    "js",
    "ts",
    "jsx",
    "tsx",
    "go",
    "c",
    "cpp",
    "cc",
    "cxx",
    "h",
    "hpp",
    "hh",
    "java",
    "kt",
    "scala",
    "rb",
    "sh",
    "bash",
    "zsh",
    "fish",
    "toml",
    "yaml",
    "yml",
    "json",
    "xml",
    "html",
    "htm",
    "css",
    "scss",
    "sass",
    "less",
    "sql",
    "lua",
    "pl",
    "pm",
    "r",
    "R",
    "swift",
    "dart",
    "zig",
    "nim",
    "ex",
    "exs",
    "erl",
    "hs",
    "ml",
    "mli",
    "cs",
    "fs",
    "fsx",
    "php",
    "ini",
    "conf",
    "cfg",
    "properties",
    "env",
    "dockerfile",
    "gitignore",
    "gitattributes",
    "editorconfig",
    "tex",
    "bib",
    "vim",
];

pub struct TextPreview;

impl PreviewPlugin for TextPreview {
    fn id(&self) -> &'static str {
        "text"
    }

    fn match_score(&self, hit: &Hit) -> u32 {
        let path = match &hit.action {
            Action::OpenFile { path } | Action::ShowInFileManager { path } => path,
            _ => return 0,
        };

        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            let lower = ext.to_ascii_lowercase();
            if STRONG_EXTENSIONS.iter().any(|&e| e == lower) {
                return 50;
            }
        }

        if let Some(mime) = hit.kind_label.as_deref().filter(|m| m.starts_with("text/")) {
            tracing::trace!("text: mime match {}", mime);
            return 30;
        }

        if sniff_looks_like_text(path) {
            return 10;
        }

        0
    }

    fn sizing(&self) -> SizingPreference {
        SizingPreference::FitToContent
    }

    fn build(&self, hit: &Hit, _cfg: &PreviewPluginCfg<'_>) -> anyhow::Result<gtk::Widget> {
        let path = match &hit.action {
            Action::OpenFile { path } | Action::ShowInFileManager { path } => path.clone(),
            _ => anyhow::bail!("text plugin: hit has no openable path"),
        };

        let mut file = File::open(&path)?;
        let mut buf = vec![0u8; DISPLAY_CAP_BYTES];
        let n = file.read(&mut buf)?;
        buf.truncate(n);
        let lossy = String::from_utf8_lossy(&buf);

        let truncated = n >= DISPLAY_CAP_BYTES;
        let mut body = lossy.into_owned();
        if truncated {
            body.push_str("\n\n\u{2026} [truncated at 50 KiB for preview]\n");
        }

        let buffer = gtk::TextBuffer::new(None);
        buffer.set_text(&body);
        if let Some(start) = buffer.iter_at_offset(0).into() {
            buffer.place_cursor(&start);
        }

        let view = gtk::TextView::with_buffer(&buffer);
        view.set_editable(false);
        view.set_cursor_visible(false);
        view.set_monospace(true);
        view.set_wrap_mode(gtk::WrapMode::WordChar);
        view.set_left_margin(16);
        view.set_right_margin(16);
        view.set_top_margin(12);
        view.set_bottom_margin(12);
        view.add_css_class("lixun-preview-text");

        let scroll = gtk::ScrolledWindow::new();
        scroll.set_hscrollbar_policy(gtk::PolicyType::Automatic);
        scroll.set_vscrollbar_policy(gtk::PolicyType::Automatic);
        // TextView with word-wrap reports natural width = 0 because
        // it is content to wrap arbitrarily narrow. Without a floor
        // the shrink-to-fit host would open the window at MIN_WIDTH
        // regardless of content. Ask for a comfortable reading width
        // (~80 monospace chars) as the lower bound; the content can
        // still push upward via propagate_natural_width on the host.
        scroll.set_min_content_width(640);
        scroll.set_min_content_height(240);
        scroll.set_child(Some(&view));
        scroll.add_css_class("lixun-preview-text-scroll");

        tracing::info!(
            "text: rendered {:?} bytes={} truncated={}",
            path,
            n,
            truncated
        );

        Ok(scroll.upcast())
    }
}

fn sniff_looks_like_text(path: &Path) -> bool {
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut buf = vec![0u8; SNIFF_BYTES];
    let Ok(n) = file.read(&mut buf) else {
        return false;
    };
    if n == 0 {
        return false;
    }
    buf.truncate(n);

    if std::str::from_utf8(&buf).is_err() {
        return false;
    }

    let ctrl = buf
        .iter()
        .filter(|&&b| b < 0x20 && b != b'\n' && b != b'\r' && b != b'\t')
        .count();
    ctrl * 100 / n.max(1) <= SNIFF_CTRL_THRESHOLD_PERCENT
}

inventory::submit! {
    PreviewPluginEntry {
        factory: || Box::new(TextPreview),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lixun_core::{Category, DocId};
    use std::path::PathBuf;

    fn file_hit(path: impl Into<PathBuf>, kind: Option<&str>) -> Hit {
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
            kind_label: kind.map(str::to_string),
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
    fn extension_txt_scores_high() {
        let hit = file_hit("/tmp/foo.txt", None);
        assert_eq!(TextPreview.match_score(&hit), 50);
    }

    #[test]
    fn extension_rs_scores_high() {
        let hit = file_hit("/tmp/lib.rs", None);
        assert_eq!(TextPreview.match_score(&hit), 50);
    }

    #[test]
    fn extension_uppercase_is_folded() {
        let hit = file_hit("/tmp/README.MD", None);
        assert_eq!(
            TextPreview.match_score(&hit),
            50,
            "extension match must be case-insensitive"
        );
    }

    #[test]
    fn mime_text_plain_scores_mid() {
        let hit = file_hit("/tmp/no-extension", Some("text/plain"));
        assert_eq!(TextPreview.match_score(&hit), 30);
    }

    #[test]
    fn unknown_extension_no_mime_scores_zero_without_content() {
        let hit = file_hit("/tmp/nonexistent.unknown-ext", None);
        assert_eq!(
            TextPreview.match_score(&hit),
            0,
            "absent file + unknown extension + no MIME must not match"
        );
    }

    #[test]
    fn non_file_action_scores_zero() {
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
        assert_eq!(TextPreview.match_score(&hit), 0);
    }

    #[test]
    fn content_sniff_picks_up_extensionless_text_file() {
        let tmp = std::env::temp_dir().join(format!(
            "lixun-preview-text-sniff-{}.bin",
            std::process::id()
        ));
        std::fs::write(
            &tmp,
            b"This is plain ASCII without an extension.\nSecond line.\n",
        )
        .unwrap();
        let hit = file_hit(&tmp, None);
        let score = TextPreview.match_score(&hit);
        std::fs::remove_file(&tmp).ok();
        assert_eq!(score, 10, "UTF-8 ASCII body must trigger content sniff");
    }

    #[test]
    fn content_sniff_rejects_binary_file() {
        let tmp = std::env::temp_dir().join(format!(
            "lixun-preview-text-sniff-bin-{}.bin",
            std::process::id()
        ));
        let binary: Vec<u8> = (0..512).map(|i| (i * 17) as u8).collect();
        std::fs::write(&tmp, &binary).unwrap();
        let hit = file_hit(&tmp, None);
        let score = TextPreview.match_score(&hit);
        std::fs::remove_file(&tmp).ok();
        assert_eq!(score, 0, "binary bytes must not sniff as text");
    }
}
