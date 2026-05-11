//! Search-state accessors for [`super::PdfCanvas`].
//!
//! Pulled into a sibling module via `#[path]` so [`super`] stays
//! under the 500-line budget after PR2b's search state lands.

use gtk::prelude::*;
use gtk::subclass::prelude::*;

use super::PdfCanvas;
use crate::document_session::{BASE_DPI, PAGE_GAP_PT, POINTS_PER_INCH};
use crate::search::{SearchMatchRef, SearchResults};

impl PdfCanvas {
    pub fn replace_search_results(&self, results: SearchResults) {
        *self.imp().search_results.borrow_mut() = results;
        for child in self.imp().pages.borrow().iter() {
            child.queue_draw();
        }
    }

    pub fn clear_search_results(&self) {
        self.imp().search_results.borrow_mut().clear();
        self.imp().current_match.set(None);
        for child in self.imp().pages.borrow().iter() {
            child.queue_draw();
        }
    }

    pub fn current_match(&self) -> Option<SearchMatchRef> {
        self.imp().current_match.get()
    }

    pub fn set_current_match(&self, m: Option<SearchMatchRef>) {
        self.imp().current_match.set(m);
        for child in self.imp().pages.borrow().iter() {
            child.queue_draw();
        }
    }

    pub fn search_results_for_page(&self, page: u32) -> Vec<poppler::Rectangle> {
        let map = self.imp().search_results.borrow();
        match map.get(&page) {
            Some(rects) => rects
                .iter()
                .map(|r| {
                    let mut out = poppler::Rectangle::default();
                    out.set_x1(r.x1());
                    out.set_y1(r.y1());
                    out.set_x2(r.x2());
                    out.set_y2(r.y2());
                    out
                })
                .collect(),
            None => Vec::new(),
        }
    }

    /// Document-space y-range of `page_idx`'s rect, in the same
    /// coordinate system the vertical adjustment uses.
    pub fn page_y_range(&self, page_idx: u32) -> Option<(f64, f64)> {
        let session = self.session()?;
        let scale = (BASE_DPI / POINTS_PER_INCH) * self.zoom();
        let mut y_pt: f64 = 0.0;
        for i in 0..session.n_pages() {
            let sz = session.page_size(i)?;
            if i == page_idx {
                let top = y_pt * scale;
                let bot = top + sz.height_pt * scale;
                return Some((top, bot));
            }
            y_pt += sz.height_pt + PAGE_GAP_PT;
        }
        None
    }

    /// Scroll the given `ScrolledWindow` so the top of page `page_index`
    /// is at the top of the viewport. Clamps `page_index` to `0..n_pages`.
    ///
    /// We take `&ScrolledWindow` as a parameter rather than storing one
    /// because `PdfCanvas` is the child widget, not the parent — it does
    /// not own its scroll container and should not assume one exists.
    pub fn scroll_to_page(&self, scroll: &gtk::ScrolledWindow, page_index: u32) {
        let Some(session) = self.session() else { return; };
        let n_pages = session.n_pages();
        if n_pages == 0 {
            return;
        }
        let target = page_index.min(n_pages.saturating_sub(1));
        let Some((top, _bot)) = self.page_y_range(target) else {
            return;
        };
        let vadj = scroll.vadjustment();
        let lower = vadj.lower();
        let upper = vadj.upper();
        let page_size = vadj.page_size();
        let clamped = top.clamp(lower, upper - page_size);
        vadj.set_value(clamped);
    }
}
