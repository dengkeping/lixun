//! Syntax-highlighted code preview plugin.
//!
//! Extends the plain-text plugin with syntax-aware highlighting
//! via syntect. Matches a narrow list of source-code extensions
//! at score 70 so it beats `lixun-preview-text` (50) and loses
//! to `lixun-preview-image` (80) for edge cases like `.svg` that
//! are both "text" and "image".
//!
//! The per-plugin `[preview.code]` config table is the first real
//! consumer of the bag we preserved in G1.7:
//!
//! ```toml
//! [preview.code]
//! theme = "Solarized (light)"
//! ```
//!
//! Defaults to `"Solarized (dark)"`. Unknown theme names fall
//! back to the default and log a warning.
//!
//! Rendering path: syntect → Pango markup → `gtk::Label`. We use
//! `Label` rather than `TextView` because Label natively consumes
//! Pango `<span foreground="#rrggbb">` markup; TextView would
//! require building a TextTagTable by hand. The code is loaded
//! with a 50 KiB hard cap (same as the text plugin); beyond that
//! we append a truncation notice.
//!
//! On parse failure (malformed input, missing syntax) the plugin
//! falls back to the plain-text rendering path with escaped
//! content so the user at least sees the raw bytes.

use std::fs::File;
use std::io::{BufRead, BufReader};

use gtk::prelude::*;
use lixun_core::{Action, Hit};
use lixun_preview::{PreviewPlugin, PreviewPluginCfg, PreviewPluginEntry, SizingPreference};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style, Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

const DISPLAY_CAP_BYTES: usize = 50 * 1024;
const DEFAULT_THEME: &str = "Solarized (dark)";

const STRONG_EXTENSIONS: &[&str] = &[
    "rs", "py", "js", "ts", "jsx", "tsx", "go", "c", "cc", "cpp", "cxx", "h", "hh", "hpp", "hxx",
    "java", "kt", "rb", "sh", "bash", "zsh", "fish", "toml", "yaml", "yml", "json", "xml", "html",
    "htm", "css", "scss", "sass", "sql", "lua", "pl", "pm", "swift", "zig", "nim", "hs", "ml",
    "mli", "scala", "clj", "cljs", "ex", "exs", "erl", "dart", "md", "markdown", "vim", "r",
    "php", "cs", "fs", "fsx",
];

pub struct CodePreview;

impl PreviewPlugin for CodePreview {
    fn id(&self) -> &'static str {
        "code"
    }

    fn match_score(&self, hit: &Hit) -> u32 {
        let path = match &hit.action {
            Action::OpenFile { path } | Action::ShowInFileManager { path } => path,
            _ => return 0,
        };
        if let Some(ext) = path.extension().and_then(|e| e.to_str())
            && STRONG_EXTENSIONS
                .iter()
                .any(|&e| e.eq_ignore_ascii_case(ext))
        {
            return 70;
        }
        0
    }

    fn sizing(&self) -> SizingPreference {
        SizingPreference::FitToContent
    }

    fn build(&self, hit: &Hit, cfg: &PreviewPluginCfg<'_>) -> anyhow::Result<gtk::Widget> {
        let path = match &hit.action {
            Action::OpenFile { path } | Action::ShowInFileManager { path } => path.clone(),
            _ => anyhow::bail!("code plugin: hit has no openable path"),
        };

        let (body, truncated) = read_capped(&path)?;

        let syntax_set = SyntaxSet::load_defaults_newlines();
        let theme_set = ThemeSet::load_defaults();
        let theme_name = resolve_theme_name(cfg, &theme_set);
        let theme = &theme_set.themes[theme_name];

        let syntax = pick_syntax(&syntax_set, &path, &body);

        let markup = match render_pango_markup(&body, syntax, theme, &syntax_set) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    "code: syntect highlight failed for {:?}: {} — falling back to plain",
                    path,
                    e
                );
                escape_pango(&body)
            }
        };

        let mut full_markup = markup;
        if truncated {
            full_markup.push_str(
                "\n\n<i>… [truncated at 50 KiB for preview]</i>\n",
            );
        }

        let label = gtk::Label::new(None);
        label.set_markup(&full_markup);
        label.set_selectable(true);
        label.set_xalign(0.0);
        label.set_yalign(0.0);
        // Code stays unwrapped — wrapping at arbitrary points breaks
        // indentation-based reading (the whole reason to highlight).
        // But an unwrapped Label reports natural width = widest line,
        // which for typical source files (long comments, wide table
        // formatters) blows past any reasonable preview cap. Pin the
        // requested width to 100 monospace chars so the FitToContent
        // host opens at a sensible ~800 px wide and lets horizontal
        // scroll handle longer lines, rather than expanding the
        // window to fit the widest line in the file.
        label.set_wrap(false);
        label.set_width_chars(100);
        label.set_margin_top(12);
        label.set_margin_bottom(12);
        label.set_margin_start(16);
        label.set_margin_end(16);
        label.add_css_class("lixun-preview-code");

        let scroll = gtk::ScrolledWindow::new();
        scroll.set_hscrollbar_policy(gtk::PolicyType::Automatic);
        scroll.set_vscrollbar_policy(gtk::PolicyType::Automatic);
        scroll.set_child(Some(&label));
        scroll.add_css_class("lixun-preview-code-scroll");
        // See preview-text for the natural-width rationale. Code
        // floor is the same as text because Pango's width_chars
        // already sets a 100-char horizontal request; the min_content
        // here just protects the vertical axis so single-line files
        // don't produce a sliver of a window.
        scroll.set_min_content_height(240);

        tracing::info!(
            "code: rendered {:?} syntax={} theme={} truncated={}",
            path,
            syntax.name,
            theme_name,
            truncated
        );

        Ok(scroll.upcast())
    }
}

fn read_capped(path: &std::path::Path) -> anyhow::Result<(String, bool)> {
    use std::io::Read;
    let mut f = File::open(path)?;
    let mut buf = vec![0u8; DISPLAY_CAP_BYTES];
    let n = f.read(&mut buf)?;
    buf.truncate(n);
    let truncated = n >= DISPLAY_CAP_BYTES;
    Ok((String::from_utf8_lossy(&buf).into_owned(), truncated))
}

fn pick_syntax<'a>(
    set: &'a SyntaxSet,
    path: &std::path::Path,
    body: &str,
) -> &'a SyntaxReference {
    if let Some(ext) = path.extension().and_then(|e| e.to_str())
        && let Some(syntax) = set.find_syntax_by_extension(&ext.to_ascii_lowercase())
    {
        return syntax;
    }
    if let Ok(Some(syntax)) = (|| -> anyhow::Result<Option<&SyntaxReference>> {
        let reader = BufReader::new(body.as_bytes());
        for line in reader.lines().take(1) {
            let line = line?;
            if let Some(syntax) = set.find_syntax_by_first_line(&line) {
                return Ok(Some(syntax));
            }
        }
        Ok(None)
    })() {
        return syntax;
    }
    set.find_syntax_plain_text()
}

fn resolve_theme_name<'a>(cfg: &'a PreviewPluginCfg<'_>, set: &'a ThemeSet) -> &'a str {
    let requested = cfg
        .section
        .and_then(|v| v.get("theme"))
        .and_then(|t| t.as_str());
    if let Some(name) = requested {
        if set.themes.contains_key(name) {
            return name_as_static_ref(set, name);
        }
        tracing::warn!(
            "code: [preview.code] theme={:?} unknown, falling back to {:?}",
            name,
            DEFAULT_THEME
        );
    }
    DEFAULT_THEME
}

fn name_as_static_ref<'a>(set: &'a ThemeSet, wanted: &str) -> &'a str {
    set.themes
        .keys()
        .find(|k| k.as_str() == wanted)
        .map(String::as_str)
        .unwrap_or(DEFAULT_THEME)
}

fn render_pango_markup(
    body: &str,
    syntax: &SyntaxReference,
    theme: &Theme,
    set: &SyntaxSet,
) -> anyhow::Result<String> {
    let mut highlighter = HighlightLines::new(syntax, theme);
    let mut out = String::with_capacity(body.len() * 2);
    for line in LinesWithEndings::from(body) {
        let regions: Vec<(Style, &str)> = highlighter.highlight_line(line, set)?;
        for (style, text) in regions {
            out.push_str(&format!(
                "<span foreground=\"#{:02x}{:02x}{:02x}\">",
                style.foreground.r, style.foreground.g, style.foreground.b
            ));
            out.push_str(&escape_pango(text));
            out.push_str("</span>");
        }
    }
    Ok(out)
}

fn escape_pango(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\'', "&#39;")
        .replace('"', "&quot;")
}

inventory::submit! {
    PreviewPluginEntry {
        factory: || Box::new(CodePreview),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lixun_core::paths::canonical_fs_doc_id;
    use lixun_core::{Category, DocId};
    use std::path::PathBuf;

    fn file_hit(path: impl Into<PathBuf>, kind: Option<&str>) -> Hit {
        let path = path.into();
        Hit {
            id: DocId(canonical_fs_doc_id(&path)),
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
    fn rs_extension_scores_seventy() {
        let hit = file_hit("/tmp/x.rs", None);
        assert_eq!(CodePreview.match_score(&hit), 70);
    }

    #[test]
    fn py_extension_scores_seventy() {
        let hit = file_hit("/tmp/x.py", None);
        assert_eq!(CodePreview.match_score(&hit), 70);
    }

    #[test]
    fn unknown_extension_scores_zero() {
        let hit = file_hit("/tmp/weird.xyz", None);
        assert_eq!(CodePreview.match_score(&hit), 0);
    }

    #[test]
    fn non_file_scores_zero() {
        let hit = Hit {
            id: DocId("app:firefox".into()),
            category: Category::App,
            title: "Firefox".into(),
            subtitle: String::new(),
            icon_name: None,
            kind_label: None,
            secondary_action: None,
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
        };
        assert_eq!(CodePreview.match_score(&hit), 0);
    }

    #[test]
    fn escape_pango_basic() {
        assert_eq!(
            escape_pango("a & <b> \"c\" 'd'"),
            "a &amp; &lt;b&gt; &quot;c&quot; &#39;d&#39;"
        );
    }

    #[test]
    fn code_beats_text_for_rs_hits() {
        use lixun_preview::PreviewPlugin as _;
        let hit = file_hit("/tmp/foo.rs", None);
        let code = CodePreview.match_score(&hit);
        let text_score = 50;
        assert!(
            code > text_score,
            "code plugin must beat text plugin on a .rs hit (got code={})",
            code
        );
    }

    #[test]
    fn render_pango_markup_produces_span_tags() {
        let set = SyntaxSet::load_defaults_newlines();
        let theme_set = ThemeSet::load_defaults();
        let theme = &theme_set.themes[DEFAULT_THEME];
        let syntax = set
            .find_syntax_by_extension("rs")
            .expect("rust syntax bundled");
        let out = render_pango_markup("fn main() {}\n", syntax, theme, &set).unwrap();
        assert!(out.contains("<span foreground=\"#"));
        assert!(out.contains("</span>"));
        assert!(out.contains("main"));
    }

    #[test]
    fn resolve_theme_name_valid_override() {
        let set = ThemeSet::load_defaults();
        let toml_value = toml::Value::Table({
            let mut t = toml::map::Map::new();
            t.insert("theme".into(), toml::Value::String("Solarized (light)".into()));
            t
        });
        let cfg = PreviewPluginCfg {
            section: Some(&toml_value),
            max_file_size_mb: 200,
        };
        assert_eq!(resolve_theme_name(&cfg, &set), "Solarized (light)");
    }

    #[test]
    fn resolve_theme_name_unknown_falls_back() {
        let set = ThemeSet::load_defaults();
        let toml_value = toml::Value::Table({
            let mut t = toml::map::Map::new();
            t.insert("theme".into(), toml::Value::String("Notatheme".into()));
            t
        });
        let cfg = PreviewPluginCfg {
            section: Some(&toml_value),
            max_file_size_mb: 200,
        };
        assert_eq!(resolve_theme_name(&cfg, &set), DEFAULT_THEME);
    }

    #[test]
    fn resolve_theme_name_no_section_uses_default() {
        let set = ThemeSet::load_defaults();
        let cfg = PreviewPluginCfg::none();
        assert_eq!(resolve_theme_name(&cfg, &set), DEFAULT_THEME);
    }
}
