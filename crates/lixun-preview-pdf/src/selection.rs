//! Point-based PDF selection primitives (Papers shape, adapted to
//! poppler-rs 0.26).
//!
//! Poppler-rs 0.26 does not expose `poppler_page_get_text_layout`
//! (ignored in `Gir.toml`), so we cannot do per-glyph rectangle
//! math ourselves. Instead we store the selection as a pair of
//! document-space points — anchor + active — and let poppler snap
//! to glyph bounds at render time via
//! `Page::selected_region(scale, style, &mut rect)` and
//! `Page::selected_text(style, &mut rect)`.
//!
//! Coordinate convention: PDF points, origin bottom-left, y
//! increases UP, 72 pt per inch. Widget pixels run origin top-left.
//! The conversion helpers below are the single source of truth for
//! the y-flip.

use poppler::SelectionStyle;

use crate::document_session::DocumentSession;

/// A point in PDF document space (points, y-up).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PdfPoint {
    pub x: f64,
    pub y: f64,
}

/// A point paired with the page it sits on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PagePoint {
    pub page: u32,
    pub point: PdfPoint,
}

/// Selection state: anchor is the drag-start point, active follows
/// the pointer. Both are stored in document space so zoom changes
/// do not stretch the selection.
#[derive(Clone, Debug)]
pub struct PdfSelection {
    pub anchor: PagePoint,
    pub active: PagePoint,
    pub style: SelectionStyle,
}

/// Normalize a selection to `(start, end)` in reading order.
///
/// Rule: lower page first; within a page, PDF y is top-first so
/// the point with larger `y` comes first; x breaks final ties.
/// This matches Papers' `compare_region_point` rule.
pub fn normalized_selection(sel: &PdfSelection) -> (PagePoint, PagePoint) {
    let a = sel.anchor;
    let b = sel.active;
    if a.page < b.page {
        return (a, b);
    }
    if a.page > b.page {
        return (b, a);
    }
    // same page — y-up: higher y (closer to top) comes first
    if a.point.y > b.point.y {
        return (a, b);
    }
    if a.point.y < b.point.y {
        return (b, a);
    }
    if a.point.x <= b.point.x {
        (a, b)
    } else {
        (b, a)
    }
}

/// Compute the per-page selection rectangle for `page_idx`. Returns
/// `None` if the page is outside the selection span.
///
/// Papers rule (`pps_view_page_compute_selection_rect`):
/// - outside `[start.page, end.page]` → None
/// - single page: corners are anchor and active
/// - start page (multi-page): from the anchor point to the
///   top-right corner of the page (anchor.x..page_w, 0..anchor.y)
/// - middle page: the whole page rectangle
/// - end page: from the origin to the active point on that page
///   (0..active.x, active.y..page_h)
///
/// All coords in PDF points (y-up).
pub fn selection_rect_for_page(
    sel: &PdfSelection,
    page_idx: u32,
    page_w_pt: f64,
    page_h_pt: f64,
) -> Option<poppler::Rectangle> {
    let (start, end) = normalized_selection(sel);
    if page_idx < start.page || page_idx > end.page {
        return None;
    }
    let (x1, y1, x2, y2) = if start.page == end.page {
        // single page — order corners so x1<=x2, y1<=y2 (poppler
        // tolerates either order, but keeping them ordered makes
        // debug traces readable)
        let x_lo = start.point.x.min(end.point.x);
        let x_hi = start.point.x.max(end.point.x);
        let y_lo = start.point.y.min(end.point.y);
        let y_hi = start.point.y.max(end.point.y);
        (x_lo, y_lo, x_hi, y_hi)
    } else if page_idx == start.page {
        // first page: from anchor down-right to the page edges
        (start.point.x, 0.0, page_w_pt, start.point.y)
    } else if page_idx == end.page {
        // last page: from the top-left down to the active point
        (0.0, end.point.y, end.point.x, page_h_pt)
    } else {
        // middle page: whole page
        (0.0, 0.0, page_w_pt, page_h_pt)
    };
    let mut r = poppler::Rectangle::default();
    r.set_x1(x1);
    r.set_y1(y1);
    r.set_x2(x2);
    r.set_y2(y2);
    Some(r)
}

/// Convert a PDF-space rectangle (y-up) to widget pixels (y-down),
/// sized for a widget at the given zoom factor.
///
/// `scale = (BASE_DPI / POINTS_PER_INCH) * zoom` — matches the
/// scale used everywhere else in the crate.
pub fn pdf_rect_to_widget_rect(
    rect: &poppler::Rectangle,
    page_h_pt: f64,
    zoom: f64,
) -> graphene::Rect {
    let scale = (96.0_f64 / 72.0_f64) * zoom;
    let x_lo = rect.x1().min(rect.x2());
    let x_hi = rect.x1().max(rect.x2());
    let y_top_pt = rect.y1().max(rect.y2());
    let y_bot_pt = rect.y1().min(rect.y2());
    // y-flip: widget_y_top = (page_h - y_top_pt) * scale
    let wx = (x_lo * scale) as f32;
    let wy = ((page_h_pt - y_top_pt) * scale) as f32;
    let ww = ((x_hi - x_lo) * scale) as f32;
    let wh = ((y_top_pt - y_bot_pt) * scale) as f32;
    graphene::Rect::new(wx, wy, ww, wh)
}

/// Convert a widget-space point (pixels, origin top-left) to a
/// PDF-space point (points, origin bottom-left) for a page of the
/// given PDF height and the current zoom factor.
pub fn widget_point_to_pdf_point(wx: f64, wy: f64, page_h_pt: f64, zoom: f64) -> PdfPoint {
    let scale = (96.0_f64 / 72.0_f64) * zoom;
    let x_pt = wx / scale;
    let y_pt = page_h_pt - (wy / scale);
    PdfPoint { x: x_pt, y: y_pt }
}

/// Walk every page touched by `sel`, pull the poppler-snapped
/// selected text for the per-page rect, and join the non-empty
/// segments with `"\n"`. Mirrors Papers' `pps_view_get_selected_text`.
pub fn collect_selected_text(session: &DocumentSession, sel: &PdfSelection) -> String {
    let (start, end) = normalized_selection(sel);
    let mut out = String::new();
    let mut first = true;
    for page_idx in start.page..=end.page {
        let Some(sz) = session.page_size(page_idx) else {
            continue;
        };
        let Some(mut rect) = selection_rect_for_page(sel, page_idx, sz.width_pt, sz.height_pt)
        else {
            continue;
        };
        let Some(page) = session.main_page(page_idx) else {
            continue;
        };
        let Some(text) = page.selected_text(sel.style, &mut rect) else {
            continue;
        };
        let s = text.to_string();
        if s.is_empty() {
            continue;
        }
        if !first {
            out.push('\n');
        }
        out.push_str(&s);
        first = false;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pp(page: u32, x: f64, y: f64) -> PagePoint {
        PagePoint {
            page,
            point: PdfPoint { x, y },
        }
    }

    fn sel(a: PagePoint, b: PagePoint) -> PdfSelection {
        PdfSelection {
            anchor: a,
            active: b,
            style: SelectionStyle::Glyph,
        }
    }

    #[test]
    fn normalized_same_page_higher_y_first() {
        let s = sel(pp(0, 100.0, 200.0), pp(0, 50.0, 500.0));
        let (start, end) = normalized_selection(&s);
        assert_eq!(start, pp(0, 50.0, 500.0), "higher y is earlier");
        assert_eq!(end, pp(0, 100.0, 200.0));
    }

    #[test]
    fn normalized_same_page_same_y_uses_x() {
        let s = sel(pp(0, 300.0, 400.0), pp(0, 100.0, 400.0));
        let (start, end) = normalized_selection(&s);
        assert_eq!(start, pp(0, 100.0, 400.0));
        assert_eq!(end, pp(0, 300.0, 400.0));
    }

    #[test]
    fn normalized_across_pages_prefers_lower_page() {
        let s = sel(pp(2, 10.0, 10.0), pp(0, 100.0, 100.0));
        let (start, end) = normalized_selection(&s);
        assert_eq!(start.page, 0);
        assert_eq!(end.page, 2);
    }

    #[test]
    fn single_page_rect_picks_corner_bounds() {
        let s = sel(pp(0, 100.0, 600.0), pp(0, 300.0, 200.0));
        let r = selection_rect_for_page(&s, 0, 595.0, 842.0).expect("in-span");
        assert!((r.x1() - 100.0).abs() < 1e-9);
        assert!((r.x2() - 300.0).abs() < 1e-9);
        assert!((r.y1() - 200.0).abs() < 1e-9);
        assert!((r.y2() - 600.0).abs() < 1e-9);
    }

    #[test]
    fn three_page_span_start_rect_runs_from_anchor_to_edges() {
        let s = sel(pp(0, 100.0, 600.0), pp(2, 50.0, 200.0));
        let r = selection_rect_for_page(&s, 0, 595.0, 842.0).expect("start page in span");
        // Start page: anchor is start; rect is anchor.x..page_w, 0..anchor.y
        assert!((r.x1() - 100.0).abs() < 1e-9);
        assert!((r.x2() - 595.0).abs() < 1e-9);
        assert!((r.y1() - 0.0).abs() < 1e-9);
        assert!((r.y2() - 600.0).abs() < 1e-9);
    }

    #[test]
    fn three_page_span_middle_rect_is_whole_page() {
        let s = sel(pp(0, 100.0, 600.0), pp(2, 50.0, 200.0));
        let r = selection_rect_for_page(&s, 1, 595.0, 842.0).expect("middle in span");
        assert!((r.x1() - 0.0).abs() < 1e-9);
        assert!((r.y1() - 0.0).abs() < 1e-9);
        assert!((r.x2() - 595.0).abs() < 1e-9);
        assert!((r.y2() - 842.0).abs() < 1e-9);
    }

    #[test]
    fn three_page_span_end_rect_runs_from_top_to_active() {
        let s = sel(pp(0, 100.0, 600.0), pp(2, 50.0, 200.0));
        let r = selection_rect_for_page(&s, 2, 595.0, 842.0).expect("end in span");
        // End page: rect runs origin..active, i.e. 0..active.x,
        // active.y..page_h
        assert!((r.x1() - 0.0).abs() < 1e-9);
        assert!((r.x2() - 50.0).abs() < 1e-9);
        assert!((r.y1() - 200.0).abs() < 1e-9);
        assert!((r.y2() - 842.0).abs() < 1e-9);
    }

    #[test]
    fn out_of_span_pages_yield_none() {
        let s = sel(pp(1, 100.0, 600.0), pp(3, 50.0, 200.0));
        assert!(selection_rect_for_page(&s, 0, 595.0, 842.0).is_none());
        assert!(selection_rect_for_page(&s, 4, 595.0, 842.0).is_none());
    }

    #[test]
    fn coordinate_flip_round_trip() {
        let page_h = 842.0;
        let zoom = 1.5;
        let wx = 123.0_f64;
        let wy = 456.0_f64;
        let pdf = widget_point_to_pdf_point(wx, wy, page_h, zoom);
        // Back to widget: build a zero-size rect at pdf and flip it.
        let mut r = poppler::Rectangle::default();
        r.set_x1(pdf.x);
        r.set_x2(pdf.x);
        r.set_y1(pdf.y);
        r.set_y2(pdf.y);
        let w = pdf_rect_to_widget_rect(&r, page_h, zoom);
        assert!((w.x() as f64 - wx).abs() < 1e-6);
        assert!((w.y() as f64 - wy).abs() < 1e-6);
    }

    #[test]
    fn widget_rect_flips_y_axis() {
        // PDF rect at the top of the page (high y) must land at
        // widget y near 0.
        let page_h = 1000.0;
        let zoom = 1.0;
        let mut r = poppler::Rectangle::default();
        r.set_x1(0.0);
        r.set_x2(100.0);
        r.set_y1(900.0); // bottom of rect in PDF = far from bottom edge
        r.set_y2(990.0); // top of rect in PDF = near top edge
        let w = pdf_rect_to_widget_rect(&r, page_h, zoom);
        let scale = 96.0 / 72.0;
        // widget top = (page_h - y_top_pt) * scale = (1000 - 990) * 4/3 ≈ 13.33
        assert!((w.y() as f64 - (10.0 * scale)).abs() < 1e-4);
        // widget height = (y_top - y_bot) * scale = 90 * 4/3 = 120
        assert!((w.height() as f64 - (90.0 * scale)).abs() < 1e-4);
    }
}
