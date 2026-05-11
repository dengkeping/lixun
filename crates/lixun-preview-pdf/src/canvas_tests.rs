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
