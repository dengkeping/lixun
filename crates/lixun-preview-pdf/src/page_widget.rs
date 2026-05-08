//! `PdfPageWidget` — one custom `gtk::Widget` per PDF page.
//!
//! Mirrors GNOME Papers' `PpsViewPage`: each page is its own widget
//! that knows how to measure itself (height = page_height_pt × zoom)
//! and snapshot a single texture from the shared `DocumentSession`
//! cache. The container [`PdfCanvas`] allocates one of these per
//! document page at the right document-space rect; GTK handles
//! clipping and hit-testing natively.
//!
//! Cache miss fallback chain matches round-3 behaviour: exact
//! bucket → `get_best_cached` nearest-bucket fallback (stretched by
//! `snapshot.append_texture`) → grey placeholder. The miss path
//! also submits a render job for the exact bucket so the visible
//! frame eventually sharpens.
//!
//! Q1 invariant: cairo never crosses the GTK boundary. Worker
//! produces `gdk::MemoryTexture`; this widget only ever calls
//! `snapshot.append_texture` and `snapshot.append_color`.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

use crate::document_session::{BASE_DPI, DocumentSession, POINTS_PER_INCH, zoom_bucket_q4};
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

    /// Atomic page+zoom binding update: single `queue_resize` plus
    /// `queue_draw` keeps GTK from observing an intermediate state
    /// where page is new but zoom is stale.
    pub fn update_state(&self, page: u32, zoom: f64) {
        self.imp().page_index.set(page);
        self.imp().zoom.set(zoom);
        self.queue_resize();
        self.queue_draw();
    }

    /// Called by [`PdfCanvas::on_render_result`] when a worker
    /// result lands for this widget's page+bucket. Triggers a
    /// repaint so the next frame picks up the freshly cached
    /// texture from the shared session.
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
            return;
        }

        if let Some(fallback) = session.get_best_cached(page, bucket) {
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
