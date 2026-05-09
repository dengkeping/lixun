//! `PdfPageWidget` — one custom `gtk::Widget` per PDF page.
//!
//! Mirrors GNOME Papers' `PpsViewPage`: each page is its own widget
//! that knows how to measure itself (height = page_height_pt × zoom)
//! and snapshot a single texture from the shared `DocumentSession`
//! cache. The container [`PdfCanvas`] allocates one of these per
//! document page at the right document-space rect; GTK handles
//! clipping and hit-testing natively.
//!
//! Snapshot layer order (PR2b):
//!   1. page texture (cached or grey placeholder)
//!   2. selection overlay (Papers-style `selected_region`,
//!      accent @ 40 %)
//!
//! Q1 invariant: cairo never crosses the GTK boundary. Worker
//! produces `gdk::MemoryTexture`; this widget only ever calls
//! `snapshot.append_texture` and `snapshot.append_color`.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

use crate::canvas::PdfCanvas;
use crate::document_session::{BASE_DPI, DocumentSession, POINTS_PER_INCH, zoom_bucket_q4};
use crate::selection::selection_rect_for_page;
use crate::worker::RenderJob;

mod imp {
    use super::*;

    pub struct PdfPageWidget {
        pub page_index: Cell<u32>,
        pub zoom: Cell<f64>,
        pub session: RefCell<Option<Rc<DocumentSession>>>,
    }

    impl Default for PdfPageWidget {
        fn default() -> Self {
            Self {
                page_index: Cell::new(0),
                zoom: Cell::new(1.0),
                session: RefCell::new(None),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PdfPageWidget {
        const NAME: &'static str = "LixunPdfPageWidget";
        type Type = super::PdfPageWidget;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for PdfPageWidget {}

    impl WidgetImpl for PdfPageWidget {
        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            let (w, h) = self.obj().intrinsic_size();
            let nat = match orientation {
                gtk::Orientation::Horizontal => w,
                _ => h,
            };
            (0, nat, -1, -1)
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            self.obj().on_snapshot(snapshot);
        }
    }
}

glib::wrapper! {
    pub struct PdfPageWidget(ObjectSubclass<imp::PdfPageWidget>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl PdfPageWidget {
    pub fn new(session: Rc<DocumentSession>, page_index: u32, zoom: f64) -> Self {
        let w: Self = glib::Object::new();
        *w.imp().session.borrow_mut() = Some(session);
        w.imp().page_index.set(page_index);
        w.imp().zoom.set(zoom);
        w
    }

    pub fn page_index(&self) -> u32 {
        self.imp().page_index.get()
    }

    pub fn zoom(&self) -> f64 {
        self.imp().zoom.get()
    }

    pub fn update_state(&self, page: u32, zoom: f64) {
        self.imp().page_index.set(page);
        self.imp().zoom.set(zoom);
        self.queue_resize();
        self.queue_draw();
    }

    pub fn on_texture_ready(&self, page: u32, _bucket: u32) {
        if page == self.imp().page_index.get() {
            self.queue_draw();
        }
    }

    fn intrinsic_size(&self) -> (i32, i32) {
        let Some(session) = self.imp().session.borrow().clone() else {
            return (0, 0);
        };
        let Some(sz) = session.page_size(self.imp().page_index.get()) else {
            return (0, 0);
        };
        let scale = (BASE_DPI / POINTS_PER_INCH) * self.imp().zoom.get();
        (
            (sz.width_pt * scale).ceil() as i32,
            (sz.height_pt * scale).ceil() as i32,
        )
    }

    fn on_snapshot(&self, snapshot: &gtk::Snapshot) {
        let Some(session) = self.imp().session.borrow().clone() else {
            return;
        };
        let page = self.imp().page_index.get();
        let zoom = self.imp().zoom.get();
        let bucket = zoom_bucket_q4(zoom);
        let w = self.width() as f32;
        let h = self.height() as f32;
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let rect = graphene::Rect::new(0.0, 0.0, w, h);

        if let Some(cached) = session.get_cached(page, bucket) {
            tracing::info!(
                "page_widget snapshot: page={} bucket={} HIT exact w={} h={}",
                page,
                bucket,
                w,
                h
            );
            snapshot.append_texture(&cached.texture, &rect);
        } else if let Some(fallback) = session.get_best_cached(page, bucket) {
            tracing::info!(
                "page_widget snapshot: page={} bucket={} FALLBACK painted",
                page,
                bucket
            );
            snapshot.append_texture(&fallback.texture, &rect);
        } else {
            tracing::info!(
                "page_widget snapshot: page={} bucket={} MISS grey",
                page,
                bucket
            );
            let color = gdk::RGBA::new(0.95, 0.95, 0.95, 1.0);
            snapshot.append_color(&color, &rect);
        }

        self.snapshot_selection_overlay(snapshot, &session, page, zoom);

        tracing::info!(
            "page_widget snapshot: page={} bucket={} submit_visible epoch={}",
            page,
            bucket,
            session.current_epoch()
        );
        session.submit_visible(RenderJob {
            page_index: page,
            zoom_bucket: bucket,
            epoch: session.current_epoch(),
        });
    }

    fn snapshot_selection_overlay(
        &self,
        snapshot: &gtk::Snapshot,
        session: &Rc<DocumentSession>,
        page_idx: u32,
        zoom: f64,
    ) {
        let Some(canvas) = self.parent().and_then(|p| p.downcast::<PdfCanvas>().ok()) else {
            return;
        };
        let Some(sel) = canvas.selection() else {
            return;
        };
        let Some(sz) = session.page_size(page_idx) else {
            return;
        };
        let Some(mut sel_rect) =
            selection_rect_for_page(&sel, page_idx, sz.width_pt, sz.height_pt)
        else {
            return;
        };
        let Some(page) = session.main_page(page_idx) else {
            return;
        };
        let scale = (BASE_DPI / POINTS_PER_INCH) * zoom;
        let Some(region) = page.selected_region(scale, sel.style, &mut sel_rect) else {
            return;
        };
        paint_region(snapshot, &region, accent_with_alpha(self, 0.4));
    }
}

fn accent_with_alpha(widget: &PdfPageWidget, alpha: f32) -> gdk::RGBA {
    let c = widget.color();
    if c.alpha() < 1e-3 {
        gdk::RGBA::new(0.2, 0.5, 0.95, alpha)
    } else {
        gdk::RGBA::new(c.red(), c.green(), c.blue(), alpha)
    }
}

/// Pathological cap: at >10_000 rectangles (every glyph distinct
/// in cairo terms) we collapse to a single union rect to stay
/// within GSK's render-node budget. Visual fidelity loss accepted,
/// see plan risk #2.
fn paint_region(snapshot: &gtk::Snapshot, region: &cairo::Region, color: gdk::RGBA) {
    let n = region.num_rectangles();
    if n > 10_000 {
        tracing::warn!(
            "selection region has {} rectangles; painting union rect instead",
            n
        );
        let extents = cairo::RectangleInt::new(0, 0, 0, 0);
        region.extents(&extents);
        let r = graphene::Rect::new(
            extents.x() as f32,
            extents.y() as f32,
            extents.width() as f32,
            extents.height() as f32,
        );
        snapshot.append_color(&color, &r);
        return;
    }
    for i in 0..n {
        let cr = region.rectangle(i);
        let r = graphene::Rect::new(
            cr.x() as f32,
            cr.y() as f32,
            cr.width() as f32,
            cr.height() as f32,
        );
        snapshot.append_color(&color, &r);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document_session::DocumentSession;
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
    fn page_widget_measure_scales_with_zoom() {
        if gtk::init().is_err() {
            eprintln!("gtk init failed — skipping");
            return;
        }
        let Some(session) = fixture_session() else {
            eprintln!("fixture missing — skipping");
            return;
        };
        let pw = PdfPageWidget::new(Rc::clone(&session), 0, 1.0);
        let (_, h1) = pw.intrinsic_size();
        assert!(h1 > 0, "intrinsic height at zoom=1 must be positive");
        pw.update_state(0, 2.0);
        let (_, h2) = pw.intrinsic_size();
        assert!(
            (h2 - 2 * h1).abs() <= 1,
            "h2={h2} expected ~{} (2 * {h1})",
            2 * h1,
        );
    }
}
