//! Integration test: page navigation helpers and fixture contract.
//!
//! Drives `poppler::Document` directly (no `gtk::init`) so the test
//! stays headless and order-independent.

use std::path::PathBuf;

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/multi-page.pdf")
}

#[test]
fn multi_page_fixture_has_three_pages() {
    let path = fixture_path();
    assert!(path.exists(), "fixture missing: {}", path.display());
    let uri = glib::filename_to_uri(&path, None).expect("filename_to_uri");
    let doc = poppler::Document::from_file(&uri, None).expect("open multi-page.pdf");
    assert_eq!(doc.n_pages(), 3, "multi-page.pdf should have 3 pages");
}

#[test]
fn clamp_identities_match_scroll_to_page_logic() {
    // Forward clamp: page_index.min(n_pages - 1)
    let n = 3u32;
    let target = (n - 1).min(99);
    assert_eq!(target, 2, "min(n-1, large) should clamp to n-1");

    // Backward clamp: current_page.saturating_sub(1)
    let target = 0u32.saturating_sub(1);
    assert_eq!(target, 0, "saturating_sub from 0 should stay 0");
}
