//! Tests for [`super::PdfCanvas`]. Lives in a sibling file so
//! `canvas.rs` stays under the 500-line cap.

use super::*;
use crate::worker::RenderResult;
use std::path::PathBuf;

fn fixture_session() -> Option<Rc<DocumentSession>> {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)?
        .join("tests/fixtures/preview/pdf/sample-text.pdf");
    if !fixture.exists() {
        return None;
    }
    let (tx, _rx) = async_channel::unbounded::<RenderResult>();
    DocumentSession::open(fixture, tx).ok()
}

#[test]
fn pdf_canvas_creates_one_child_per_page() {
    if gtk::init().is_err() {
        eprintln!("gtk init failed — skipping");
        return;
    }
    let Some(session) = fixture_session() else {
        eprintln!("fixture missing — skipping");
        return;
    };
    let n = session.n_pages();
    assert!(n >= 1, "fixture must have at least 1 page");
    let canvas = PdfCanvas::new();
    canvas.set_session(session);
    let count = canvas.imp().pages.borrow().len() as u32;
    assert_eq!(
        count, n,
        "expected one PdfPageWidget per document page (n={n}, got={count})"
    );
}

#[test]
fn zoomed_in_clamps_at_max() {
    assert!((zoomed_in(MAX_ZOOM) - MAX_ZOOM).abs() < f64::EPSILON);
}

#[test]
fn zoomed_out_clamps_at_min() {
    assert!((zoomed_out(MIN_ZOOM) - MIN_ZOOM).abs() < f64::EPSILON);
}

#[test]
fn zoomed_in_steps_normally() {
    let result = zoomed_in(1.0);
    assert!((result - 1.25).abs() < f64::EPSILON);
}

#[test]
fn zoomed_out_steps_normally() {
    let result = zoomed_out(1.0);
    assert!((result - 0.8).abs() < f64::EPSILON);
}

#[test]
fn zoomed_in_clamps_near_max() {
    let result = zoomed_in(15.0);
    assert!((result - MAX_ZOOM).abs() < f64::EPSILON);
}

#[test]
fn zoomed_out_clamps_near_min() {
    let result = zoomed_out(0.3);
    assert!((result - MIN_ZOOM).abs() < f64::EPSILON);
}

#[test]
fn fit_to_width_normal() {
    let result = fit_to_width(600.0, 800.0);
    assert!((result - 1.333_333_333_333_333_3).abs() < 1e-6);
}

#[test]
fn fit_to_page_normal() {
    let result = fit_to_page((600.0, 800.0), (800.0, 600.0));
    assert!((result - 0.75).abs() < f64::EPSILON);
}

#[test]
fn fit_to_width_clamps_upper() {
    let result = fit_to_width(600.0, 100_000.0);
    assert!((result - MAX_ZOOM).abs() < f64::EPSILON);
}

#[test]
fn fit_to_page_clamps_lower() {
    let result = fit_to_page((600.0, 800.0), (10.0, 10.0));
    assert!((result - MIN_ZOOM).abs() < f64::EPSILON);
}

#[test]
fn fit_to_width_defensive_zero_page_width() {
    let result = fit_to_width(0.0, 800.0);
    assert!((result - MIN_ZOOM).abs() < f64::EPSILON);
}

#[test]
fn fit_to_width_defensive_nan_viewport() {
    let result = fit_to_width(600.0, f64::NAN);
    assert!((result - MIN_ZOOM).abs() < f64::EPSILON);
}

#[test]
fn fit_to_page_defensive_zero_page_size() {
    let result = fit_to_page((0.0, 0.0), (800.0, 600.0));
    assert!((result - MIN_ZOOM).abs() < f64::EPSILON);
}

#[test]
fn scroll_to_page_clamp_arithmetic() {
    // Forward clamp: page_index.min(n_pages - 1)
    let n = 3u32;
    let target = (n - 1).min(99);
    assert_eq!(target, 2, "min(n-1, large) should clamp to n-1");

    // Backward clamp: current_page.saturating_sub(1)
    let target = 0u32.saturating_sub(1);
    assert_eq!(target, 0, "saturating_sub from 0 should stay 0");
}

#[test]
fn accessible_page_label_is_one_indexed() {
    assert_eq!(
        accessibility::page_accessible_label(0, 12, "report.pdf"),
        "Page 1 of 12 \u{2014} report.pdf"
    );
    assert_eq!(
        accessibility::page_accessible_label(11, 12, "report.pdf"),
        "Page 12 of 12 \u{2014} report.pdf"
    );
}

#[test]
fn accessible_page_label_floors_page_count_at_one() {
    // A zero-page document still reads as "of 1" — the label must
    // never claim "Page 1 of 0".
    assert_eq!(
        accessibility::page_accessible_label(0, 0, "empty.pdf"),
        "Page 1 of 1 \u{2014} empty.pdf"
    );
}

#[test]
fn accessible_truncate_passes_short_text_through() {
    assert_eq!(accessibility::truncate_chars("abc", 10), "abc");
    assert_eq!(accessibility::truncate_chars("", 10), "");
}

#[test]
fn accessible_truncate_caps_length_and_marks_ellipsis() {
    let long = "x".repeat(400);
    let out = accessibility::truncate_chars(&long, accessibility::A11Y_DESCRIPTION_MAX_CHARS);
    assert_eq!(
        out.chars().count(),
        accessibility::A11Y_DESCRIPTION_MAX_CHARS
    );
    assert!(out.ends_with('\u{2026}'));
    // Multi-byte input must not split a code point.
    let unicode = "\u{e4}".repeat(400);
    let out = accessibility::truncate_chars(&unicode, 10);
    assert_eq!(out.chars().count(), 10);
}
