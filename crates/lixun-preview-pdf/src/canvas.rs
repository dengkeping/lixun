//! `PdfCanvas` — `gtk::Scrollable` container of [`PdfPageWidget`]
//! children, one per PDF page (Papers `PpsView` pattern).
//!
//! `gtk::Scrollable` wiring (`set_*adjustment_inner` +
//! `ScrollableImpl` + ParamSpec list) lives in the sibling
//! [`canvas_scrollable`] submodule so this file stays under 500
//! lines once selection state + hit-testing are added.
//!
//! Q1 invariant: cairo never crosses the GTK boundary. Children
//! paint via `snapshot.append_texture`; the worker stays the only
//! cairo user.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

use crate::document_session::{BASE_DPI, DocumentSession, PAGE_GAP_PT, POINTS_PER_INCH};
use crate::page_widget::PdfPageWidget;
use crate::search::{SearchMatchRef, SearchResults};
use crate::selection::{PagePoint, PdfSelection, widget_point_to_pdf_point};
use crate::worker::{RenderOutcome, RenderResult};

pub const MIN_ZOOM: f64 = 0.25;
pub const MAX_ZOOM: f64 = 16.0;
pub(crate) const ZOOM_STEP: f64 = 1.25;

pub(crate) fn zoomed_in(current: f64) -> f64 {
    (current * ZOOM_STEP).clamp(MIN_ZOOM, MAX_ZOOM)
}

pub(crate) fn zoomed_out(current: f64) -> f64 {
    (current / ZOOM_STEP).clamp(MIN_ZOOM, MAX_ZOOM)
}

mod imp {
    use super::*;
    use std::sync::OnceLock;

    pub struct PdfCanvas {
        pub session: RefCell<Option<Rc<DocumentSession>>>,
        pub zoom: Cell<f64>,
        pub current_page: Cell<u32>,
        pub pages: RefCell<Vec<PdfPageWidget>>,
        pub hadjustment: RefCell<Option<gtk::Adjustment>>,
        pub vadjustment: RefCell<Option<gtk::Adjustment>>,
        pub hscroll_policy: Cell<gtk::ScrollablePolicy>,
        pub vscroll_policy: Cell<gtk::ScrollablePolicy>,
        pub hadj_signal: RefCell<Option<glib::SignalHandlerId>>,
        pub vadj_signal: RefCell<Option<glib::SignalHandlerId>>,
        pub selection: RefCell<Option<PdfSelection>>,
        pub search_results: RefCell<SearchResults>,
        pub current_match: Cell<Option<SearchMatchRef>>,
    }

    impl Default for PdfCanvas {
        fn default() -> Self {
            Self {
                session: RefCell::new(None),
                zoom: Cell::new(1.0),
                current_page: Cell::new(0),
                pages: RefCell::new(Vec::new()),
                hadjustment: RefCell::new(None),
                vadjustment: RefCell::new(None),
                hscroll_policy: Cell::new(gtk::ScrollablePolicy::Minimum),
                vscroll_policy: Cell::new(gtk::ScrollablePolicy::Minimum),
                hadj_signal: RefCell::new(None),
                vadj_signal: RefCell::new(None),
                selection: RefCell::new(None),
                search_results: RefCell::new(SearchResults::new()),
                current_match: Cell::new(None),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PdfCanvas {
        const NAME: &'static str = "LixunPdfCanvas";
        type Type = super::PdfCanvas;
        type ParentType = gtk::Widget;
        type Interfaces = (gtk::Scrollable,);
    }

    impl ObjectImpl for PdfCanvas {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPS: OnceLock<Vec<glib::ParamSpec>> = OnceLock::new();
            PROPS.get_or_init(super::scrollable::scrollable_param_specs)
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            match pspec.name() {
                "hadjustment" => self.obj().set_hadjustment_inner(value.get().unwrap()),
                "vadjustment" => self.obj().set_vadjustment_inner(value.get().unwrap()),
                "hscroll-policy" => self.hscroll_policy.set(value.get().unwrap()),
                "vscroll-policy" => self.vscroll_policy.set(value.get().unwrap()),
                _ => unimplemented!(),
            }
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            match pspec.name() {
                "hadjustment" => self.hadjustment.borrow().to_value(),
                "vadjustment" => self.vadjustment.borrow().to_value(),
                "hscroll-policy" => self.hscroll_policy.get().to_value(),
                "vscroll-policy" => self.vscroll_policy.get().to_value(),
                _ => unimplemented!(),
            }
        }

        fn dispose(&self) {
            for child in self.pages.borrow_mut().drain(..) {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for PdfCanvas {
        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            let (w, h) = self.obj().intrinsic_size();
            let nat = match orientation {
                gtk::Orientation::Horizontal => w,
                _ => h,
            };
            (0, nat, -1, -1)
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            tracing::info!(
                "canvas size_allocate: w={} h={} baseline={}",
                width,
                height,
                baseline
            );
            self.parent_size_allocate(width, height, baseline);
            self.obj().reconfigure_adjustments(width, height);
            self.obj().allocate_children(width, height);
        }
    }
}

#[path = "canvas_scrollable.rs"]
mod scrollable;

#[path = "canvas_search.rs"]
mod search_accessors;

glib::wrapper! {
    pub struct PdfCanvas(ObjectSubclass<imp::PdfCanvas>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Scrollable;
}

impl PdfCanvas {
    pub fn new() -> Self {
        let canvas: Self = glib::Object::new();
        canvas.set_overflow(gtk::Overflow::Hidden);
        canvas
    }

    pub fn set_session(&self, session: Rc<DocumentSession>) {
        *self.imp().session.borrow_mut() = Some(session);
        self.imp().zoom.set(1.0);
        self.imp().current_page.set(0);
        *self.imp().selection.borrow_mut() = None;
        self.imp().search_results.borrow_mut().clear();
        self.imp().current_match.set(None);
        self.rebuild_page_widgets();
        self.queue_resize();
    }

    pub fn session(&self) -> Option<Rc<DocumentSession>> {
        self.imp().session.borrow().clone()
    }

    pub fn zoom(&self) -> f64 {
        self.imp().zoom.get()
    }

    pub fn set_zoom(&self, new_zoom: f64) {
        let z = new_zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        self.imp().zoom.set(z);
        for (i, child) in self.imp().pages.borrow().iter().enumerate() {
            child.update_state(i as u32, z);
        }
        self.queue_resize();
    }

    pub fn current_page(&self) -> u32 {
        self.imp().current_page.get()
    }

    pub fn set_current_page(&self, p: u32) {
        self.imp().current_page.set(p);
    }

    pub fn selection(&self) -> Option<PdfSelection> {
        self.imp().selection.borrow().clone()
    }

    pub fn set_selection(&self, sel: Option<PdfSelection>) {
        *self.imp().selection.borrow_mut() = sel;
        for child in self.imp().pages.borrow().iter() {
            child.queue_draw();
        }
    }

    pub fn clear_selection(&self) {
        if self.imp().selection.borrow().is_some() {
            *self.imp().selection.borrow_mut() = None;
            for child in self.imp().pages.borrow().iter() {
                child.queue_draw();
            }
        }
    }

    pub fn update_selection_active(&self, new_active: PagePoint) {
        let mut sel_ref = self.imp().selection.borrow_mut();
        if let Some(sel) = sel_ref.as_mut() {
            sel.active = new_active;
        } else {
            return;
        }
        drop(sel_ref);
        for child in self.imp().pages.borrow().iter() {
            child.queue_draw();
        }
    }

    /// Map a widget-space pointer to `(page, pdf_point)`. Returns
    /// `None` in gaps between pages or before first allocation.
    pub fn hit_test_page(&self, wx: f64, wy: f64) -> Option<PagePoint> {
        let session = self.session()?;
        let zoom = self.zoom();
        for child in self.imp().pages.borrow().iter() {
            let bounds = child.compute_bounds(self)?;
            let bx = bounds.x() as f64;
            let by = bounds.y() as f64;
            let bw = bounds.width() as f64;
            let bh = bounds.height() as f64;
            if wx >= bx && wx < bx + bw && wy >= by && wy < by + bh {
                let local_x = wx - bx;
                let local_y = wy - by;
                let page_idx = child.page_index();
                let sz = session.page_size(page_idx)?;
                let pdf = widget_point_to_pdf_point(local_x, local_y, sz.height_pt, zoom);
                return Some(PagePoint {
                    page: page_idx,
                    point: pdf,
                });
            }
        }
        None
    }

    pub fn on_render_result(&self, result: RenderResult) {
        let Some(session) = self.session() else {
            return;
        };
        let page = result.page_index;
        let bucket = result.zoom_bucket;
        let cur = session.current_epoch();
        if result.epoch != cur {
            tracing::info!(
                "canvas on_render_result STALE: page={} bucket={} result_epoch={} session_epoch={}",
                page, bucket, result.epoch, cur
            );
            session.clear_pending(page, bucket);
            return;
        }
        tracing::info!("canvas on_render_result KEEP: page={} bucket={} epoch={}", page, bucket, result.epoch);
        session.clear_pending(page, bucket);
        let RenderOutcome::Ok { texture, width, height, bytes } = result.outcome else {
            return;
        };
        session.insert_cached(
            page,
            bucket,
            crate::document_session::CachedTexture { texture, width, height, bytes },
        );
        if let Some(child) = self.imp().pages.borrow().get(page as usize) {
            child.on_texture_ready(page, bucket);
        }
    }

    pub fn recompute_current_page(&self, vadj_value: f64, viewport_h: f64) {
        let pages = self.imp().pages.borrow();
        if !pages.is_empty() && pages.iter().any(|p| p.height() > 0) {
            let viewport_top = vadj_value;
            let viewport_bot = vadj_value + viewport_h;
            let mut best: (u32, f64) = (0, -1.0);
            for child in pages.iter() {
                let Some(bounds) = child.compute_bounds(self) else { continue };
                let child_top = bounds.y() as f64 + vadj_value;
                let child_bot = child_top + bounds.height() as f64;
                let inter = (child_bot.min(viewport_bot) - child_top.max(viewport_top)).max(0.0);
                if inter > best.1 {
                    best = (child.page_index(), inter);
                }
            }
            if best.1 > 0.0 {
                self.set_current_page(best.0);
                return;
            }
        }
        let Some(session) = self.session() else { return };
        let zoom = self.zoom();
        let scale = (BASE_DPI / POINTS_PER_INCH) * zoom;
        let n = session.n_pages();
        if n == 0 {
            return;
        }
        let viewport_center = vadj_value + viewport_h * 0.5;
        let mut y_pt: f64 = 0.0;
        let mut best: (u32, f64) = (0, f64::INFINITY);
        for i in 0..n {
            let Some(sz) = session.page_size(i) else { continue };
            let page_center_px = y_pt * scale + sz.height_pt * scale * 0.5;
            let dist = (page_center_px - viewport_center).abs();
            if dist < best.1 {
                best = (i, dist);
            }
            y_pt += sz.height_pt + PAGE_GAP_PT;
        }
        self.set_current_page(best.0);
    }

    pub fn rebuild_page_widgets(&self) {
        for child in self.imp().pages.borrow_mut().drain(..) {
            child.unparent();
        }
        let Some(session) = self.session() else {
            return;
        };
        let n = session.n_pages();
        let zoom = self.zoom();
        let mut new_children = Vec::with_capacity(n as usize);
        for i in 0..n {
            let pw = PdfPageWidget::new(Rc::clone(&session), i, zoom);
            pw.set_parent(self);
            new_children.push(pw);
        }
        *self.imp().pages.borrow_mut() = new_children;
        self.queue_resize();
        self.reconfigure_adjustments(self.width(), self.height());
        self.allocate_children(self.width(), self.height());
        self.queue_draw();
    }

    fn intrinsic_size(&self) -> (i32, i32) {
        let Some(session) = self.session() else {
            return (0, 0);
        };
        let zoom = self.zoom();
        let scale = (BASE_DPI / POINTS_PER_INCH) * zoom;
        let n = session.n_pages();
        if n == 0 {
            return (0, 0);
        }
        let mut max_w_pt = 0.0_f64;
        let mut total_h_pt = 0.0_f64;
        for i in 0..n {
            if let Some(sz) = session.page_size(i) {
                if sz.width_pt > max_w_pt {
                    max_w_pt = sz.width_pt;
                }
                total_h_pt += sz.height_pt;
            }
        }
        total_h_pt += PAGE_GAP_PT * f64::from(n.saturating_sub(1));
        (
            (max_w_pt * scale).ceil() as i32,
            (total_h_pt * scale).ceil() as i32,
        )
    }

    pub(crate) fn reconfigure_adjustments(&self, width: i32, height: i32) {
        let (intrinsic_w, intrinsic_h) = self.intrinsic_size();
        if let Some(hadj) = self.imp().hadjustment.borrow().as_ref() {
            let upper = (width as f64).max(intrinsic_w as f64);
            hadj.configure(
                hadj.value().min(upper - width as f64).max(0.0),
                0.0,
                upper,
                0.1 * width as f64,
                0.9 * width as f64,
                width as f64,
            );
        }
        if let Some(vadj) = self.imp().vadjustment.borrow().as_ref() {
            let upper = (height as f64).max(intrinsic_h as f64);
            vadj.configure(
                vadj.value().min(upper - height as f64).max(0.0),
                0.0,
                upper,
                0.1 * height as f64,
                0.9 * height as f64,
                height as f64,
            );
        }
    }

    fn allocate_children(&self, width: i32, _height: i32) {
        let Some(session) = self.session() else {
            return;
        };
        let zoom = self.imp().zoom.get();
        let display_scale = (BASE_DPI / POINTS_PER_INCH) * zoom;
        let hadj_value = self
            .imp()
            .hadjustment
            .borrow()
            .as_ref()
            .map(|a| a.value())
            .unwrap_or(0.0);
        let vadj_value = self
            .imp()
            .vadjustment
            .borrow()
            .as_ref()
            .map(|a| a.value())
            .unwrap_or(0.0);
        let intrinsic_w = self.intrinsic_size().0 as f64;
        let centerline = (width as f64 * 0.5).max(intrinsic_w * 0.5);

        let pages = self.imp().pages.borrow();
        let mut y_pt: f64 = 0.0;
        for (i, child) in pages.iter().enumerate() {
            let Some(sz) = session.page_size(i as u32) else {
                continue;
            };
            let page_top_doc = y_pt * display_scale;
            let page_w_px = sz.width_pt * display_scale;
            let page_h_px = sz.height_pt * display_scale;
            let x_doc = centerline - page_w_px * 0.5;
            let x_widget = (x_doc - hadj_value).round() as i32;
            let y_widget = (page_top_doc - vadj_value).round() as i32;
            let w = page_w_px.ceil() as i32;
            let h = page_h_px.ceil() as i32;
            let transform = gtk::gsk::Transform::new()
                .translate(&graphene::Point::new(x_widget as f32, y_widget as f32));
            child.allocate(w, h, -1, Some(transform));
            y_pt += sz.height_pt + PAGE_GAP_PT;
        }
    }
}

impl Default for PdfCanvas {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[path = "canvas_tests.rs"]
mod tests;
