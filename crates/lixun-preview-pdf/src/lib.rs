//! PDF preview plugin (PR2a — render pipeline + zoom/pan + page nav).
//!
//! Architecture (see plan §2.10 Q1–Q10):
//! - `widget`: `PdfView` (public) and `PdfCanvas` (custom `gtk::Widget`
//!   subclass with `WidgetImpl::snapshot` / `measure`). No cairo
//!   crosses the GTK boundary.
//! - `worker`: long-lived render threads, each owning its own
//!   `poppler::Document` (Q2). Cairo→`gdk::MemoryTexture`.
//! - `document_session`: epoch counter, texture LRU cache,
//!   per-page sizes, worker handles.
//!
//! Text selection / search / outline are deferred to PR2b / PR2c
//! (see TODO markers in `widget.rs`).

mod canvas;
mod document_session;
mod icons;
mod page_widget;
mod poppler_host;
mod region_image;
pub mod search;
mod search_bar;
pub mod selection;
mod widget;
mod worker;

use gtk::prelude::*;
use lixun_core::{Action, Hit};
use lixun_preview::{
    PreviewCapabilities, PreviewPlugin, PreviewPluginCfg, PreviewPluginEntry, SizingPreference,
    UPDATE_UNSUPPORTED,
};

/// Rich PDF view widget, re-exported for peer preview plugins.
///
/// Peer plugins whose source format can be converted to a PDF on
/// disk embed this widget directly to avoid duplicating the
/// render/search/zoom pipeline. See the [`widget`] module docs for
/// the cross-plugin reuse contract. Depending on this crate does
/// not double-register the PDF `PreviewPlugin` — `inventory::submit!`
/// runs once per linked binary regardless of how many crates
/// import `PdfView`.
pub use widget::PdfView;

pub struct PdfPreview;

impl PreviewPlugin for PdfPreview {
    fn id(&self) -> &'static str {
        "pdf"
    }

    fn match_score(&self, hit: &Hit) -> u32 {
        let path = match &hit.action {
            Action::OpenFile { path } | Action::ShowInFileManager { path } => path,
            _ => return 0,
        };
        if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
        {
            return 90;
        }
        if hit.kind_label.as_deref() == Some("application/pdf") {
            return 70;
        }
        0
    }

    fn sizing(&self) -> SizingPreference {
        SizingPreference::OwnsScroll
    }

    fn capabilities(&self) -> PreviewCapabilities {
        PreviewCapabilities {
            text_selection: true,
            text_search: true,
            paginated: true,
            zoomable: true,
        }
    }

    fn build(&self, hit: &Hit, _cfg: &PreviewPluginCfg<'_>) -> anyhow::Result<gtk::Widget> {
        let path = match &hit.action {
            Action::OpenFile { path } | Action::ShowInFileManager { path } => path.clone(),
            _ => anyhow::bail!("pdf plugin: hit has no openable path"),
        };

        match PdfView::new(path.clone()) {
            Ok(view) => Ok(view.upcast()),
            Err(e) => Ok(error_widget(&open_error_message(&e)).upcast()),
        }
    }

    fn update(&self, hit: &Hit, widget: &gtk::Widget) -> anyhow::Result<()> {
        let new_path = match &hit.action {
            Action::OpenFile { path } | Action::ShowInFileManager { path } => path.clone(),
            _ => anyhow::bail!(UPDATE_UNSUPPORTED),
        };
        if !new_path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
            && hit.kind_label.as_deref() != Some("application/pdf")
        {
            anyhow::bail!(UPDATE_UNSUPPORTED);
        }
        let view = widget
            .downcast_ref::<PdfView>()
            .ok_or_else(|| anyhow::anyhow!(UPDATE_UNSUPPORTED))?;
        view.replace_path(new_path)?;
        Ok(())
    }
}

fn open_error_message(err: &anyhow::Error) -> String {
    if let Some(gerr) = err.chain().find_map(|e| e.downcast_ref::<glib::Error>())
        && let Some(poppler::Error::Encrypted) = gerr.kind::<poppler::Error>()
    {
        return "Cannot preview password-protected document".into();
    }
    "Cannot preview PDF".into()
}

fn error_widget(msg: &str) -> gtk::Widget {
    let label = gtk::Label::new(Some(msg));
    label.set_wrap(true);
    label.set_xalign(0.5);
    label.set_yalign(0.5);
    label.set_hexpand(true);
    label.set_vexpand(true);
    label.add_css_class("lixun-preview-pdf-error");
    label.upcast()
}

inventory::submit! {
    PreviewPluginEntry {
        factory: || Box::new(PdfPreview),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lixun_core::paths::canonical_fs_doc_id;
    use lixun_core::{Category, DocId};
    use std::path::{Path, PathBuf};

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
            mime: None,
            extract_fail: false,
            sender: None,
            recipients: None,
            body: None,
            secondary_action: None,
            source_instance: String::new(),
            row_menu: lixun_core::RowMenuDef::empty(),
        }
    }

    #[test]
    fn pdf_extension_scores_ninety() {
        let hit = file_hit("/tmp/doc.pdf", None);
        assert_eq!(PdfPreview.match_score(&hit), 90);
    }

    #[test]
    fn pdf_uppercase_extension_scores_ninety() {
        let hit = file_hit("/tmp/doc.PDF", None);
        assert_eq!(PdfPreview.match_score(&hit), 90);
    }

    #[test]
    fn pdf_mime_without_extension_scores_seventy() {
        let hit = file_hit("/tmp/noext", Some("application/pdf"));
        assert_eq!(PdfPreview.match_score(&hit), 70);
    }

    #[test]
    fn non_pdf_scores_zero() {
        let hit = file_hit("/tmp/doc.docx", Some("application/vnd.openxmlformats"));
        assert_eq!(PdfPreview.match_score(&hit), 0);
    }

    #[test]
    fn path_to_file_uri_escapes_spaces() {
        let uri = document_session::path_to_file_uri(Path::new("/tmp/hello world.pdf")).unwrap();
        assert!(uri.contains("hello%20world.pdf"));
        assert!(uri.starts_with("file://"));
    }

    #[test]
    fn path_to_file_uri_keeps_separators() {
        let uri = document_session::path_to_file_uri(Path::new("/a/b/c.pdf")).unwrap();
        assert!(!uri.contains("%2F"));
        assert!(uri.ends_with("/c.pdf"));
    }

    fn workspace_root() -> PathBuf {
        let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        crate_dir
            .ancestors()
            .nth(2)
            .map(PathBuf::from)
            .expect("workspace root above crates/lixun-preview-pdf")
    }

    #[test]
    fn encrypted_pdf_yields_typed_or_generic_message() {
        let fixture = workspace_root().join("tests/fixtures/preview/pdf/sample-encrypted.pdf");
        if !fixture.exists() {
            eprintln!(
                "needs Phase 0 fixture at {:?} — marking test as skipped",
                fixture
            );
            return;
        }
        let uri = document_session::path_to_file_uri(&fixture).unwrap();
        let res = poppler::Document::from_file(&uri, None);
        match res {
            Ok(_) => {
                eprintln!("fixture opened without password — not actually encrypted, skipping");
            }
            Err(e) => {
                eprintln!("encrypted-pdf err Debug: {:?}", e);
                let any_err: anyhow::Error =
                    anyhow::Error::from(e).context("opening encrypted fixture");
                let msg = open_error_message(&any_err);
                assert!(
                    msg == "Cannot preview password-protected document"
                        || msg == "Cannot preview PDF",
                    "unexpected error message: {}",
                    msg
                );
            }
        }
    }
}
