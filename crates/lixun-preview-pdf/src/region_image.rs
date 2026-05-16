//! Rectangular region → pixel image extraction for PDF preview.
//!
//! Renders a user-selected axis-aligned rectangle of one PDF page
//! into a `gdk::MemoryTexture` at the canvas zoom, suitable for
//! placing on the system clipboard via `gdk::Clipboard::set_texture`.
//!
//! The cairo render path mirrors `poppler_host::render_one`: an
//! ARGB32 `ImageSurface` is filled white, scaled to (BASE_DPI /
//! POINTS_PER_INCH) * zoom, then translated so the selected
//! rectangle lands at the surface origin. `poppler::Page::render`
//! paints the whole page into that transformed context; only the
//! pixels covering the selected rectangle end up inside the
//! surface bounds.
//!
//! Coordinate notes: the selection stores y-up PDF points. Cairo
//! uses y-down. The translate offset is therefore
//! `(-x_lo_pt, -(page_h_pt - y_hi_pt))`, where `y_hi_pt` is the
//! larger y in y-up space (visually the top of the rectangle).

use cairo::{Context as CairoCtx, Format as CairoFormat, ImageSurface};
use gtk::gdk;
use gtk::glib;

use crate::document_session::{BASE_DPI, DocumentSession, POINTS_PER_INCH};
use crate::selection::{PdfSelection, PdfSelectionMode};

/// Render the selected rectangular region to a `gdk::MemoryTexture`
/// at the given canvas zoom. Returns `None` if the selection is not
/// in `Region` mode, spans pages, the page or its size is missing,
/// or the rectangle collapses to less than one PDF point in either
/// dimension after clamping to the page bounds.
pub fn render_region_image(
    session: &DocumentSession,
    sel: &PdfSelection,
    zoom: f64,
) -> Option<gdk::MemoryTexture> {
    if !matches!(sel.mode, PdfSelectionMode::Region) {
        return None;
    }
    if sel.anchor.page != sel.active.page {
        return None;
    }
    let page_idx = sel.anchor.page;
    let sz = session.page_size(page_idx)?;
    let page = session.main_page(page_idx)?;

    // PDF y-up bounding box, clamped to the page rectangle.
    let x_lo_pt = sel.anchor.point.x.min(sel.active.point.x).max(0.0);
    let x_hi_pt = sel
        .anchor
        .point
        .x
        .max(sel.active.point.x)
        .min(sz.width_pt);
    let y_lo_pt = sel.anchor.point.y.min(sel.active.point.y).max(0.0);
    let y_hi_pt = sel
        .anchor
        .point
        .y
        .max(sel.active.point.y)
        .min(sz.height_pt);
    let w_pt = x_hi_pt - x_lo_pt;
    let h_pt = y_hi_pt - y_lo_pt;
    if w_pt < 1.0 || h_pt < 1.0 {
        return None;
    }

    // Top of the selection in cairo y-down space.
    let top_pt_ydown = sz.height_pt - y_hi_pt;
    let scale = (BASE_DPI / POINTS_PER_INCH) * zoom;
    let px_w = (w_pt * scale).ceil().max(1.0) as i32;
    let px_h = (h_pt * scale).ceil().max(1.0) as i32;

    let mut surface = ImageSurface::create(CairoFormat::ARgb32, px_w, px_h).ok()?;
    {
        let ctx = CairoCtx::new(&surface).ok()?;
        ctx.set_source_rgb(1.0, 1.0, 1.0);
        ctx.paint().ok()?;
        ctx.scale(scale, scale);
        ctx.translate(-x_lo_pt, -top_pt_ydown);
        page.render(&ctx);
        ctx.target().flush();
    }

    let stride = surface.stride();
    let buf = {
        let data = surface.data().ok()?;
        let mut v = Vec::with_capacity(data.len());
        v.extend_from_slice(&data);
        v
    };
    drop(surface);

    let glib_bytes = glib::Bytes::from_owned(buf);
    Some(gdk::MemoryTexture::new(
        px_w,
        px_h,
        gdk::MemoryFormat::B8g8r8a8Premultiplied,
        &glib_bytes,
        stride as usize,
    ))
}
