//! Integration test: cross-page text selection on a real multi-page PDF.
//!
//! Drives `poppler::Document` + the public `selection` helpers
//! directly (no `gtk::init`, no `DocumentSession`) so the test
//! stays headless. Mirrors the walk that
//! `lixun_preview_pdf::selection::collect_selected_text` performs,
//! but inlined here because `DocumentSession` is crate-private.

use lixun_preview_pdf::selection::{
    PagePoint, PdfPoint, PdfSelection, PdfSelectionMode, flip_rect_y_for_poppler_selection,
    selection_rect_for_page,
};
use poppler::{Document, SelectionStyle};
use std::path::PathBuf;

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/multi-page.pdf")
}

fn open_doc() -> Document {
    let path = fixture_path();
    assert!(path.exists(), "fixture missing: {}", path.display());
    let uri = glib::filename_to_uri(&path, None).expect("filename_to_uri");
    Document::from_file(&uri, None).expect("open multi-page.pdf")
}

/// Collect text from every page touched by `sel`, mirroring
/// `selection::collect_selected_text` but using only the public
/// `poppler::Page` API (no `DocumentSession`).
fn collect_text(doc: &Document, sel: &PdfSelection) -> String {
    let mut out = String::new();
    let mut first = true;
    let (start_page, end_page) = (sel.anchor.page.min(sel.active.page), sel.anchor.page.max(sel.active.page));
    for page_idx in start_page..=end_page {
        let Some(page) = doc.page(page_idx as i32) else {
            continue;
        };
        let (w, h) = page.size();
        let Some(rect_yup) = selection_rect_for_page(sel, page_idx, w, h) else {
            continue;
        };
        let mut rect = flip_rect_y_for_poppler_selection(&rect_yup, h);
        let text = page.selected_text(SelectionStyle::Glyph, &mut rect);
        let text = text.map(|s| s.to_string()).unwrap_or_default();
        if !text.trim().is_empty() {
            if !first {
                out.push('\n');
            }
            out.push_str(&text);
            first = false;
        }
    }
    out
}

#[test]
fn cross_page_selection_collects_text_from_all_three_pages() {
    let doc = open_doc();
    assert!(doc.n_pages() >= 3, "fixture must have at least 3 pages");

    // Use full-document corner-to-corner so every glyph on every
    // page is inside the selection regardless of layout details.
    let p0 = doc.page(0).expect("page 0");
    let (w0, h0) = p0.size();
    let p2 = doc.page(2).expect("page 2");
    let (w2, _h2) = p2.size();

    let sel = PdfSelection {
        // anchor: top-left of page 0 (PDF y-up: large y == top)
        anchor: PagePoint { page: 0, point: PdfPoint { x: 0.0, y: h0 } },
        // active: bottom-right of page 2 (PDF y-up: y=0 == bottom)
        active: PagePoint { page: 2, point: PdfPoint { x: w2.max(w0), y: 0.0 } },
        mode: PdfSelectionMode::Text { style: SelectionStyle::Glyph },
    };

    let text = collect_text(&doc, &sel);
    assert!(text.contains("Page one"), "missing 'Page one' in: {:?}", text);
    assert!(text.contains("Page two"), "missing 'Page two' in: {:?}", text);
    assert!(text.contains("Page three"), "missing 'Page three' in: {:?}", text);
}

#[test]
fn cross_page_selection_middle_page_is_full_page() {
    // Audit-style check: when start.page < page < end.page,
    // selection_rect_for_page returns the full page rect.
    let sel = PdfSelection {
        anchor: PagePoint { page: 0, point: PdfPoint { x: 10.0, y: 20.0 } },
        active: PagePoint { page: 2, point: PdfPoint { x: 30.0, y: 40.0 } },
        mode: PdfSelectionMode::Text { style: SelectionStyle::Glyph },
    };
    let r = selection_rect_for_page(&sel, 1, 612.0, 792.0).expect("rect for middle page");
    assert_eq!(r.x1(), 0.0);
    assert_eq!(r.y1(), 0.0);
    assert_eq!(r.x2(), 612.0);
    assert_eq!(r.y2(), 792.0);
}
