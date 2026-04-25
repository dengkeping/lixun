//! PDF preview plugin.
//!
//! # Cairo ABI isolation
//!
//! Poppler-rs 0.26 links cairo-rs 0.22. Our workspace's GTK4 0.9
//! uses cairo-rs 0.20. The two versions are two different crates
//! and their `cairo::Context` / `cairo::ImageSurface` types are
//! NOT interchangeable — you cannot pass a 0.22 `Context` into a
//! `DrawingArea::set_draw_func` callback (which hands you a 0.20
//! `Context`) without UB.
//!
//! We therefore NEVER let cairo types cross the crate boundary.
//! Each page is:
//!
//! 1. Rendered into a cairo-0.22 `ImageSurface` owned by this
//!    crate.
//! 2. Read out as raw ARGB32 bytes (`surface.data()`).
//! 3. Handed to GTK as a `gdk::MemoryTexture` via `glib::Bytes`,
//!    which is the cross-version-safe currency.
//! 4. Displayed in a `gtk::Picture`, one per page, stacked in a
//!    vertical `Box` inside a `ScrolledWindow`.
//!
//! The original plan (`gui-ux-v1-tier2-plugins.md:232`) called for
//! one `DrawingArea` per page with an on-draw callback. That shape
//! is blocked by the cairo ABI split above. The texture shape
//! uses more RAM (pre-rasterised vs vector on-draw) but isolates
//! the version conflict cleanly.
//!
//! # Page cap
//!
//! Renders at most `PAGE_CAP` pages (20). Beyond that we show a
//! footer line "N of M pages — open in <default PDF viewer> for
//! the rest". 20 pages at 96 DPI A4 is ~68 MiB of pre-rasterised
//! texture — acceptable; 100 pages would be ~340 MiB which blows
//! past any sane preview budget.

use std::path::Path;

use gtk::glib;
use gtk::prelude::*;
use lixun_core::{Action, Hit};
use lixun_preview::{PreviewPlugin, PreviewPluginCfg, PreviewPluginEntry};

use cairo::{Context as CairoCtx, Format as CairoFormat, ImageSurface};
use poppler::Document;

const PAGE_CAP: usize = 20;
const DPI: f64 = 96.0;
const POINTS_PER_INCH: f64 = 72.0;

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

    fn build(&self, hit: &Hit, _cfg: &PreviewPluginCfg<'_>) -> anyhow::Result<gtk::Widget> {
        let path = match &hit.action {
            Action::OpenFile { path } | Action::ShowInFileManager { path } => path.clone(),
            _ => anyhow::bail!("pdf plugin: hit has no openable path"),
        };

        let uri = path_to_file_uri(&path)?;
        let doc = Document::from_file(&uri, None)
            .map_err(|e| anyhow::anyhow!("poppler: open {:?}: {}", path, e))?;
        let total_pages = doc.n_pages() as usize;
        let render_count = total_pages.min(PAGE_CAP);

        let vbox = gtk::Box::new(gtk::Orientation::Vertical, 12);
        vbox.set_margin_top(12);
        vbox.set_margin_bottom(12);
        vbox.set_margin_start(12);
        vbox.set_margin_end(12);
        vbox.add_css_class("lixun-preview-pdf");

        for i in 0..render_count {
            match render_page(&doc, i as i32) {
                Ok(picture) => {
                    picture.add_css_class("lixun-preview-pdf-page");
                    vbox.append(&picture);
                }
                Err(e) => {
                    tracing::warn!("pdf: render page {}/{} failed: {}", i + 1, total_pages, e);
                    let err =
                        gtk::Label::new(Some(&format!("Page {} failed to render: {}", i + 1, e)));
                    err.set_xalign(0.0);
                    err.add_css_class("lixun-preview-pdf-error");
                    vbox.append(&err);
                }
            }
        }

        if total_pages > render_count {
            let footer = gtk::Label::new(Some(&format!(
                "Showing {} of {} pages — open the file for the rest.",
                render_count, total_pages
            )));
            footer.set_xalign(0.0);
            footer.set_margin_top(8);
            footer.add_css_class("lixun-preview-pdf-footer");
            vbox.append(&footer);
        }

        let scroll = gtk::ScrolledWindow::new();
        scroll.set_hscrollbar_policy(gtk::PolicyType::Automatic);
        scroll.set_vscrollbar_policy(gtk::PolicyType::Automatic);
        scroll.set_child(Some(&vbox));
        scroll.set_hexpand(true);
        scroll.set_vexpand(true);

        tracing::info!(
            "pdf: rendered {} of {} pages from {:?}",
            render_count,
            total_pages,
            path
        );

        Ok(scroll.upcast())
    }
}

fn render_page(doc: &Document, index: i32) -> anyhow::Result<gtk::Picture> {
    let page = doc
        .page(index)
        .ok_or_else(|| anyhow::anyhow!("page {} missing", index))?;
    let (pt_w, pt_h) = page.size();
    let scale = DPI / POINTS_PER_INCH;
    let px_w = (pt_w * scale).ceil() as i32;
    let px_h = (pt_h * scale).ceil() as i32;

    let surface = ImageSurface::create(CairoFormat::ARgb32, px_w, px_h)
        .map_err(|e| anyhow::anyhow!("cairo surface create {}x{}: {}", px_w, px_h, e))?;
    {
        let ctx = CairoCtx::new(&surface)?;
        ctx.set_source_rgb(1.0, 1.0, 1.0);
        ctx.paint()?;
        ctx.scale(scale, scale);
        page.render(&ctx);
        ctx.target().flush();
    }
    let stride = surface.stride();
    let data = surface.take_data()?;
    let bytes = glib::Bytes::from_owned(data.to_vec());

    let texture = gdk::MemoryTexture::new(
        px_w,
        px_h,
        gdk::MemoryFormat::B8g8r8a8Premultiplied,
        &bytes,
        stride as usize,
    );

    let picture = gtk::Picture::for_paintable(&texture);
    picture.set_content_fit(gtk::ContentFit::ScaleDown);
    picture.set_can_shrink(true);
    picture.set_hexpand(true);
    Ok(picture)
}

fn path_to_file_uri(path: &Path) -> anyhow::Result<String> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let canon = abs.canonicalize().unwrap_or(abs);
    let s = canon.to_string_lossy();
    let mut uri = String::from("file://");
    for b in s.as_bytes() {
        let c = *b;
        let safe = c.is_ascii_alphanumeric()
            || c == b'/'
            || c == b'-'
            || c == b'_'
            || c == b'.'
            || c == b'~';
        if safe {
            uri.push(c as char);
        } else {
            uri.push_str(&format!("%{:02X}", c));
        }
    }
    Ok(uri)
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
        let uri = path_to_file_uri(Path::new("/tmp/hello world.pdf")).unwrap();
        assert!(uri.contains("hello%20world.pdf"));
        assert!(uri.starts_with("file://"));
    }

    #[test]
    fn path_to_file_uri_keeps_separators() {
        let uri = path_to_file_uri(Path::new("/a/b/c.pdf")).unwrap();
        assert!(!uri.contains("%2F"));
        assert!(uri.ends_with("/c.pdf"));
    }
}
