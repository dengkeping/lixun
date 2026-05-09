//! Search UI wiring for [`super::PdfView`]: SearchBar signal
//! handlers, SearchWorker pump, Ctrl+F / Escape / scroll-to-match.
//!
//! Pulled into a sibling via `#[path]` so [`super`] stays under
//! the 500-line budget once search lands.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;

use super::{PdfView, SearchState};
use crate::canvas::PdfCanvas;
use crate::document_session::DocumentSession;
use crate::search::{PageSearchResult, SearchQueryState, SearchResults, SearchWorker};
use crate::search_bar::PdfSearchBar;
use crate::selection::pdf_rect_to_widget_rect;

pub(super) fn wire_search(
    view: &PdfView,
    scroll: &gtk::ScrolledWindow,
    canvas: &PdfCanvas,
    bar: &PdfSearchBar,
    state: Rc<RefCell<SearchState>>,
    session: &Rc<DocumentSession>,
) {
    let (tx, rx) = async_channel::unbounded::<PageSearchResult>();
    let worker = match SearchWorker::spawn(session.path(), tx) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!("search worker spawn failed: {}", e);
            return;
        }
    };
    state.borrow_mut().worker = Some(worker);

    wire_result_pump(canvas, bar, Rc::clone(&state), rx);
    wire_bar_signals(scroll, canvas, bar, Rc::clone(&state), session);
    wire_search_keys(view, canvas, bar);
}

fn wire_result_pump(
    canvas: &PdfCanvas,
    bar: &PdfSearchBar,
    state: Rc<RefCell<SearchState>>,
    rx: async_channel::Receiver<PageSearchResult>,
) {
    let canvas_weak = canvas.downgrade();
    let bar_weak = bar.downgrade();
    glib::MainContext::default().spawn_local(async move {
        while let Ok(result) = rx.recv().await {
            let Some(canvas) = canvas_weak.upgrade() else {
                break;
            };
            let Some(bar) = bar_weak.upgrade() else {
                break;
            };
            let accepted = state.borrow_mut().query.merge_page_result(result);
            if !accepted {
                continue;
            }
            let (results, current, counter) = {
                let q = &state.borrow().query;
                (snapshot_results(&q.results), q.current, counter_text(q))
            };
            canvas.replace_search_results(results);
            canvas.set_current_match(current);
            bar.set_counter_text(&counter);
        }
    });
}

fn snapshot_results(src: &SearchResults) -> SearchResults {
    let mut out = SearchResults::new();
    for (&page, rects) in src {
        let cloned: Vec<poppler::Rectangle> = rects
            .iter()
            .map(|r| {
                let mut o = poppler::Rectangle::default();
                o.set_x1(r.x1());
                o.set_y1(r.y1());
                o.set_x2(r.x2());
                o.set_y2(r.y2());
                o
            })
            .collect();
        out.insert(page, cloned);
    }
    out
}

fn counter_text(q: &SearchQueryState) -> String {
    if q.total_matches == 0 {
        if q.finished_pages == 0 {
            return String::new();
        }
        return "0 matches".into();
    }
    let current = q.current_index().map(|i| i + 1).unwrap_or(1);
    format!("{}/{}", current, q.total_matches)
}

fn wire_bar_signals(
    scroll: &gtk::ScrolledWindow,
    canvas: &PdfCanvas,
    bar: &PdfSearchBar,
    state: Rc<RefCell<SearchState>>,
    session: &Rc<DocumentSession>,
) {
    {
        let canvas_weak = canvas.downgrade();
        let bar_weak = bar.downgrade();
        let state = Rc::clone(&state);
        let n_pages = session.n_pages();
        bar.connect_closure(
            "query-changed",
            false,
            glib::closure_local!(move |_b: PdfSearchBar, query: &str| {
                let Some(canvas) = canvas_weak.upgrade() else {
                    return;
                };
                let Some(bar) = bar_weak.upgrade() else {
                    return;
                };
                let mut s = state.borrow_mut();
                s.query.generation = s.query.generation.saturating_add(1);
                s.query.query = query.to_string();
                s.query.results.clear();
                s.query.total_matches = 0;
                s.query.finished_pages = 0;
                s.query.total_pages = n_pages as usize;
                s.query.current = None;
                let generation = s.query.generation;
                let q = s.query.query.clone();
                canvas.clear_search_results();
                if q.is_empty() {
                    bar.set_counter_text("");
                    return;
                }
                bar.set_counter_text("");
                if let Some(worker) = s.worker.as_ref() {
                    worker.start(q, generation, n_pages);
                }
            }),
        );
    }
    {
        let canvas_weak = canvas.downgrade();
        let bar_weak = bar.downgrade();
        let scroll_weak = scroll.downgrade();
        let state = Rc::clone(&state);
        let session = Rc::clone(session);
        bar.connect_closure(
            "next-match",
            false,
            glib::closure_local!(move |_b: PdfSearchBar| {
                let (Some(canvas), Some(bar), Some(scroll)) =
                    (canvas_weak.upgrade(), bar_weak.upgrade(), scroll_weak.upgrade())
                else {
                    return;
                };
                advance(&canvas, &bar, &scroll, &state, &session, true);
            }),
        );
    }
    {
        let canvas_weak = canvas.downgrade();
        let bar_weak = bar.downgrade();
        let scroll_weak = scroll.downgrade();
        let state = Rc::clone(&state);
        let session = Rc::clone(session);
        bar.connect_closure(
            "prev-match",
            false,
            glib::closure_local!(move |_b: PdfSearchBar| {
                let (Some(canvas), Some(bar), Some(scroll)) =
                    (canvas_weak.upgrade(), bar_weak.upgrade(), scroll_weak.upgrade())
                else {
                    return;
                };
                advance(&canvas, &bar, &scroll, &state, &session, false);
            }),
        );
    }
    {
        let canvas_weak = canvas.downgrade();
        let bar_weak = bar.downgrade();
        let state = Rc::clone(&state);
        bar.connect_closure(
            "close-requested",
            false,
            glib::closure_local!(move |_b: PdfSearchBar| {
                let Some(canvas) = canvas_weak.upgrade() else {
                    return;
                };
                let Some(bar) = bar_weak.upgrade() else {
                    return;
                };
                let mut s = state.borrow_mut();
                s.query.generation = s.query.generation.saturating_add(1);
                s.query.query.clear();
                s.query.results.clear();
                s.query.total_matches = 0;
                s.query.finished_pages = 0;
                s.query.current = None;
                canvas.clear_search_results();
                bar.clear_query();
                bar.set_visible(false);
            }),
        );
    }
}

fn advance(
    canvas: &PdfCanvas,
    bar: &PdfSearchBar,
    scroll: &gtk::ScrolledWindow,
    state: &Rc<RefCell<SearchState>>,
    session: &Rc<DocumentSession>,
    forward: bool,
) {
    let (current, counter, focus_rect) = {
        let mut s = state.borrow_mut();
        s.query.advance(forward);
        let current = s.query.current;
        let counter = counter_text(&s.query);
        let focus = s.query.current_rect();
        (current, counter, focus)
    };
    canvas.set_current_match(current);
    bar.set_counter_text(&counter);
    if let Some((page_idx, rect)) = focus_rect {
        scroll_to_match(canvas, scroll, session, page_idx, &rect);
    }
}

fn scroll_to_match(
    canvas: &PdfCanvas,
    scroll: &gtk::ScrolledWindow,
    session: &Rc<DocumentSession>,
    page_idx: u32,
    rect: &poppler::Rectangle,
) {
    let Some(sz) = session.page_size(page_idx) else {
        return;
    };
    let zoom = canvas.zoom();
    let w_rect = pdf_rect_to_widget_rect(rect, sz.height_pt, zoom);
    let Some((page_top, _)) = canvas.page_y_range(page_idx) else {
        return;
    };
    let match_top = page_top + w_rect.y() as f64;
    let match_bot = match_top + w_rect.height() as f64;
    let vadj = scroll.vadjustment();
    let vh = vadj.page_size();
    let margin = 5.0;
    let v = vadj.value();
    if match_top < v + margin {
        let target = (match_top - margin).max(vadj.lower());
        vadj.set_value(target);
    } else if match_bot > v + vh - margin {
        let target = (match_bot - vh + margin).min(vadj.upper() - vh).max(vadj.lower());
        vadj.set_value(target);
    }
}

fn wire_search_keys(view: &PdfView, canvas: &PdfCanvas, bar: &PdfSearchBar) {
    let key = gtk::EventControllerKey::new();
    key.set_propagation_phase(gtk::PropagationPhase::Capture);
    let canvas_weak = canvas.downgrade();
    let bar_weak = bar.downgrade();
    key.connect_key_pressed(move |_c, keyval, _code, state| {
        let Some(bar) = bar_weak.upgrade() else {
            return glib::Propagation::Proceed;
        };
        let Some(canvas) = canvas_weak.upgrade() else {
            return glib::Propagation::Proceed;
        };
        let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
        if ctrl && (keyval == gdk::Key::f || keyval == gdk::Key::F) {
            bar.set_visible(true);
            bar.focus_entry();
            return glib::Propagation::Stop;
        }
        if keyval == gdk::Key::Escape {
            if bar.is_visible() {
                bar.emit_by_name::<()>("close-requested", &[]);
                return glib::Propagation::Stop;
            }
            if canvas.selection().is_some() {
                canvas.clear_selection();
                return glib::Propagation::Stop;
            }
        }
        glib::Propagation::Proceed
    });
    view.add_controller(key);
}

pub(super) fn reset_for_path(
    canvas: &PdfCanvas,
    bar: &PdfSearchBar,
    state: &Rc<RefCell<SearchState>>,
    session: &Rc<DocumentSession>,
) {
    let mut s = state.borrow_mut();
    s.query.generation = s.query.generation.saturating_add(1);
    s.query.query.clear();
    s.query.results.clear();
    s.query.total_matches = 0;
    s.query.finished_pages = 0;
    s.query.total_pages = session.n_pages() as usize;
    s.query.current = None;
    if let Some(worker) = s.worker.as_ref() {
        worker.replace_path(session.path());
    }
    canvas.clear_search_results();
    bar.clear_query();
    bar.set_visible(false);
}

pub(super) fn default_state() -> SearchState {
    SearchState {
        query: SearchQueryState::empty(),
        worker: None,
    }
}
