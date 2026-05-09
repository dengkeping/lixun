//! Integration test: poppler text + find_text round-trip on a real PDF.
//!
//! Drives `poppler::Document` directly (no `gtk::init`, no
//! `DocumentSession`) so the test stays headless and order-independent.

use poppler::{Document, FindFlags, Rectangle, SelectionStyle};
use std::path::PathBuf;

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root above crates/lixun-preview-pdf")
        .join("tests/fixtures/preview/pdf/sample-text.pdf")
}

fn open_doc() -> Document {
    let path = fixture_path();
    assert!(path.exists(), "fixture missing: {}", path.display());
    let uri = glib::filename_to_uri(&path, None).expect("filename_to_uri");
    Document::from_file(&uri, None).expect("open sample-text.pdf")
}

fn first_word_of_at_least(text: &str, min_len: usize) -> Option<String> {
    text.split(|c: char| !c.is_alphabetic())
        .find(|w| w.chars().count() >= min_len)
        .map(|w| w.to_string())
}

#[test]
fn find_text_matches_a_known_word() {
    let doc = open_doc();
    let n_pages = doc.n_pages();
    assert!(n_pages >= 1, "expected at least one page");

    let page0 = doc.page(0).expect("page 0");
    let text0 = page0
        .text()
        .map(|g| g.to_string())
        .unwrap_or_default();
    let query = first_word_of_at_least(&text0, 4)
        .expect("page 0 should contain a word of length >= 4");

    let mut total = 0usize;
    let mut per_page = Vec::with_capacity(n_pages as usize);
    for i in 0..n_pages {
        let page = doc.page(i).expect("page");
        let rects = page.find_text_with_options(&query, FindFlags::empty());
        per_page.push(rects.len());
        total += rects.len();
    }

    assert!(
        total > 0,
        "find_text_with_options returned zero matches for query {query:?} across {n_pages} pages"
    );
    let sum: usize = per_page.iter().sum();
    assert_eq!(sum, total, "per-page counts disagree with total");
}

#[test]
fn selected_text_round_trips_a_match_rect() {
    let doc = open_doc();
    let n_pages = doc.n_pages();

    let page0 = doc.page(0).expect("page 0");
    let text0 = page0.text().map(|g| g.to_string()).unwrap_or_default();
    let query = first_word_of_at_least(&text0, 4)
        .expect("page 0 should contain a word of length >= 4");

    for i in 0..n_pages {
        let page = doc.page(i).expect("page");
        let rects: Vec<Rectangle> = page.find_text_with_options(&query, FindFlags::empty());
        if let Some(rect) = rects.into_iter().next() {
            let mut r = rect;
            let selected = page.selected_text(SelectionStyle::Glyph, &mut r);
            let s = selected.map(|g| g.to_string()).unwrap_or_default();
            assert!(
                !s.trim().is_empty(),
                "selected_text on a find_text rect should be non-empty (page {i}, query {query:?})"
            );
            return;
        }
    }
    panic!("no page had a match for query {query:?}");
}
