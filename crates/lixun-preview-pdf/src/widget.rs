//! `PdfView` — public widget tree exported by this crate.
//!
//! Layout:
//! ```text
//! PdfView (gtk::Box vertical)
//! ├── PdfToolbar (gtk::ActionBar) — page label + ± zoom buttons
//! ├── PdfSearchBar (hidden by default, toggled via Ctrl+F)
//! └── PdfScroll  (gtk::ScrolledWindow, kinetic OFF — Q3)
//!     └── PdfCanvas (custom gtk::Widget subclass — see canvas.rs)
//! ```
//!
//! `PdfView::replace_path` is the entry point used by
//! `PreviewPlugin::update`: bumps the session epoch, reopens all
//! three Documents (main + 2 workers + search worker), clears the
//! texture cache, resets the viewport to top, clears search state.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

use crate::canvas::{MAX_ZOOM, MIN_ZOOM, PdfCanvas, fit_to_page, fit_to_width, zoomed_in, zoomed_out};
use crate::document_session::DocumentSession;
use crate::search::{SearchQueryState, SearchWorker};
use crate::search_bar::PdfSearchBar;
use crate::selection::{PagePoint, PdfSelection, collect_selected_text};
use crate::worker::RenderResult;

pub struct SearchState {
    pub query: SearchQueryState,
    pub worker: Option<SearchWorker>,
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct PdfView {
        pub canvas: RefCell<Option<PdfCanvas>>,
        pub scroll: RefCell<Option<gtk::ScrolledWindow>>,
        pub page_label: RefCell<Option<gtk::Label>>,
        pub page_entry: RefCell<Option<gtk::Entry>>,
        pub search_bar: RefCell<Option<PdfSearchBar>>,
        pub search_state: RefCell<Option<Rc<RefCell<SearchState>>>>,
        pub session: RefCell<Option<Rc<DocumentSession>>>,
        pub max_page_size: Cell<(f64, f64)>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PdfView {
        const NAME: &'static str = "LixunPdfView";
        type Type = super::PdfView;
        type ParentType = gtk::Box;
    }

    impl ObjectImpl for PdfView {}
    impl WidgetImpl for PdfView {}
    impl BoxImpl for PdfView {}
}

#[path = "widget_search.rs"]
mod search_ui;

glib::wrapper! {
    pub struct PdfView(ObjectSubclass<imp::PdfView>)
        @extends gtk::Widget, gtk::Box,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
}

impl PdfView {
    pub fn new(path: PathBuf) -> anyhow::Result<Self> {
        let view: Self = glib::Object::builder()
            .property("orientation", gtk::Orientation::Vertical)
            .property("spacing", 0)
            .build();
        view.set_hexpand(true);
        view.set_vexpand(true);
        view.set_overflow(gtk::Overflow::Hidden);
        // Make the view itself focusable so our key controllers (Ctrl+F,
        // Ctrl+C, Escape) receive events even when no descendant holds
        // focus. Without this, a freshly-mapped PdfView with no focused
        // child drops all key events before our controllers see them.
        view.set_focusable(true);
        view.set_focus_on_click(false);
        view.connect_map(|v| {
            v.grab_focus();
        });

        let (tx, rx) = async_channel::unbounded::<RenderResult>();
        let session = DocumentSession::open(path, tx)?;

        // Compute document-wide max page size once and cache it.
        let max_page_size = Rc::new(Cell::new((0.0_f64, 0.0_f64)));
        {
            let n = session.n_pages();
            let mut max_w = 0.0_f64;
            let mut max_h = 0.0_f64;
            for i in 0..n {
                if let Some(size) = session.page_size(i) {
                    if size.width_pt > max_w {
                        max_w = size.width_pt;
                    }
                    if size.height_pt > max_h {
                        max_h = size.height_pt;
                    }
                }
            }
            max_page_size.set((max_w, max_h));
        }

        let toolbar = gtk::ActionBar::new();
        toolbar.add_css_class("lixun-preview-pdf-toolbar");
        let zoom_out = gtk::Button::from_icon_name("zoom-out-symbolic");
        let zoom_in = gtk::Button::from_icon_name("zoom-in-symbolic");
        let fit_width_btn = gtk::Button::from_icon_name("zoom-fit-width-symbolic");
        let fit_page_btn = gtk::Button::from_icon_name("zoom-fit-best-symbolic");
        let page_entry = gtk::Entry::new();
        page_entry.set_text("1");
        page_entry.set_max_width_chars(4);
        page_entry.set_width_chars(4);
        gtk::prelude::EntryExt::set_alignment(&page_entry, 0.5);
        let page_label = gtk::Label::new(Some(&format!(" / {}", session.n_pages().max(1))));
        let page_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        page_box.append(&page_entry);
        page_box.append(&page_label);
        toolbar.pack_start(&zoom_out);
        toolbar.pack_start(&zoom_in);
        toolbar.pack_start(&fit_width_btn);
        toolbar.pack_start(&fit_page_btn);
        toolbar.set_center_widget(Some(&page_box));

        let search_bar = PdfSearchBar::new();

        let scroll = gtk::ScrolledWindow::new();
        scroll.set_kinetic_scrolling(false);
        scroll.set_hexpand(true);
        scroll.set_vexpand(true);
        scroll.set_hscrollbar_policy(gtk::PolicyType::Automatic);
        scroll.set_vscrollbar_policy(gtk::PolicyType::Automatic);

        let canvas = PdfCanvas::new();
        canvas.set_session(Rc::clone(&session));
        scroll.set_child(Some(&canvas));

        view.append(&toolbar);
        view.append(&search_bar);
        view.append(&scroll);

        *view.imp().canvas.borrow_mut() = Some(canvas.clone());
        *view.imp().scroll.borrow_mut() = Some(scroll.clone());
        *view.imp().page_label.borrow_mut() = Some(page_label.clone());
        *view.imp().page_entry.borrow_mut() = Some(page_entry.clone());
        *view.imp().search_bar.borrow_mut() = Some(search_bar.clone());
        *view.imp().session.borrow_mut() = Some(Rc::clone(&session));
        view.imp().max_page_size.set(max_page_size.get());

        let state = Rc::new(RefCell::new(search_ui::default_state()));
        *view.imp().search_state.borrow_mut() = Some(Rc::clone(&state));

        wire_render_pump(&canvas, rx);
        wire_gestures(&canvas, &scroll);
        wire_selection_gestures(&canvas);
        wire_clipboard_key(&view, &canvas, &session);
        wire_page_keys(&view, &canvas, &scroll);
        wire_buttons(
            &canvas,
            &zoom_in,
            &zoom_out,
            &fit_width_btn,
            &fit_page_btn,
            &scroll,
            Rc::clone(&max_page_size),
        );
        wire_page_tracking(&canvas, &scroll, &page_entry, &session);
        wire_page_entry(&view, &canvas, &scroll, &page_entry, &session);
        search_ui::wire_search(&view, &scroll, &canvas, &search_bar, state, &session);

        Ok(view)
    }

    pub fn replace_path(&self, new_path: PathBuf) -> anyhow::Result<()> {
        let session = self
            .imp()
            .session
            .borrow()
            .clone()
            .ok_or_else(|| anyhow::anyhow!("PdfView has no session"))?;
        session.replace_path(new_path)?;

        // Recompute cached max page size for the new document.
        {
            let n = session.n_pages();
            let mut max_w = 0.0_f64;
            let mut max_h = 0.0_f64;
            for i in 0..n {
                if let Some(size) = session.page_size(i) {
                    if size.width_pt > max_w {
                        max_w = size.width_pt;
                    }
                    if size.height_pt > max_h {
                        max_h = size.height_pt;
                    }
                }
            }
            self.imp().max_page_size.set((max_w, max_h));
        }

        if let Some(canvas) = self.imp().canvas.borrow().as_ref() {
            canvas.set_zoom(1.0);
            canvas.set_current_page(0);
            canvas.clear_selection();
            canvas.rebuild_page_widgets();
            canvas.queue_resize();
            canvas.queue_draw();
            let canvas_weak = canvas.downgrade();
            glib::idle_add_local_once(move || {
                if let Some(c) = canvas_weak.upgrade() {
                    c.queue_resize();
                    c.queue_draw();
                }
            });
        }
        if let Some(scroll) = self.imp().scroll.borrow().as_ref() {
            scroll.vadjustment().set_value(0.0);
            scroll.hadjustment().set_value(0.0);
        }
        if let Some(label) = self.imp().page_label.borrow().as_ref() {
            label.set_text(&format!(" / {}", session.n_pages().max(1)));
        }
        if let Some(entry) = self.imp().page_entry.borrow().as_ref() {
            entry.set_text("1");
        }
        if let (Some(canvas), Some(bar), Some(state)) = (
            self.imp().canvas.borrow().as_ref(),
            self.imp().search_bar.borrow().as_ref(),
            self.imp().search_state.borrow().as_ref(),
        ) {
            search_ui::reset_for_path(canvas, bar, state, &session);
        }
        Ok(())
    }
}

fn wire_render_pump(canvas: &PdfCanvas, rx: async_channel::Receiver<RenderResult>) {
    let canvas_weak = canvas.downgrade();
    glib::MainContext::default().spawn_local(async move {
        while let Ok(result) = rx.recv().await {
            let Some(canvas) = canvas_weak.upgrade() else {
                break;
            };
            canvas.on_render_result(result);
        }
    });
}

fn wire_gestures(canvas: &PdfCanvas, scroll: &gtk::ScrolledWindow) {
    let zoom_gesture = gtk::GestureZoom::new();
    {
        let canvas_weak = canvas.downgrade();
        let scroll_weak = scroll.downgrade();
        let initial = Rc::new(std::cell::Cell::new(1.0_f64));
        {
            let initial = Rc::clone(&initial);
            let canvas_weak = canvas_weak.clone();
            zoom_gesture.connect_begin(move |_g, _seq| {
                if let Some(c) = canvas_weak.upgrade() {
                    initial.set(c.zoom());
                }
            });
        }
        zoom_gesture.connect_scale_changed(move |_g, scale| {
            let Some(canvas) = canvas_weak.upgrade() else {
                return;
            };
            let Some(scroll) = scroll_weak.upgrade() else {
                return;
            };
            let target = (initial.get() * scale).clamp(MIN_ZOOM, MAX_ZOOM);
            apply_zoom_centered(&canvas, &scroll, target, None, None);
        });
    }
    canvas.add_controller(zoom_gesture);

    let drag = gtk::GestureDrag::builder().button(2).build();
    {
        let scroll_weak = scroll.downgrade();
        let start = Rc::new(std::cell::Cell::new((0.0_f64, 0.0_f64)));
        {
            let start = Rc::clone(&start);
            let scroll_weak = scroll_weak.clone();
            drag.connect_drag_begin(move |g, _x, _y| {
                g.set_state(gtk::EventSequenceState::Claimed);
                if let Some(scroll) = scroll_weak.upgrade() {
                    start.set((scroll.hadjustment().value(), scroll.vadjustment().value()));
                }
            });
        }
        drag.connect_drag_update(move |_g, dx, dy| {
            let Some(scroll) = scroll_weak.upgrade() else {
                return;
            };
            let (sx, sy) = start.get();
            scroll.hadjustment().set_value(sx - dx);
            scroll.vadjustment().set_value(sy - dy);
        });
    }
    scroll.add_controller(drag);

    let scroll_ctrl = gtk::EventControllerScroll::new(
        gtk::EventControllerScrollFlags::VERTICAL | gtk::EventControllerScrollFlags::HORIZONTAL,
    );
    {
        let canvas_weak = canvas.downgrade();
        let scroll_weak = scroll.downgrade();
        scroll_ctrl.connect_scroll(move |g, _dx, dy| {
            let Some(canvas) = canvas_weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            let Some(scroll) = scroll_weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            let state = g.current_event_state();
            if state.contains(gdk::ModifierType::CONTROL_MASK) {
                let cur = canvas.zoom();
                let factor = if dy < 0.0 { 1.10 } else { 1.0 / 1.10 };
                let target = (cur * factor).clamp(MIN_ZOOM, MAX_ZOOM);
                let cursor = g.current_event().and_then(|e| e.position());
                let (cx, cy) =
                    cursor.unwrap_or((canvas.width() as f64 * 0.5, canvas.height() as f64 * 0.5));
                apply_zoom_centered(&canvas, &scroll, target, Some(cx), Some(cy));
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
    }
    canvas.add_controller(scroll_ctrl);
}

fn wire_buttons(
    canvas: &PdfCanvas,
    zoom_in: &gtk::Button,
    zoom_out: &gtk::Button,
    fit_width_btn: &gtk::Button,
    fit_page_btn: &gtk::Button,
    scroll: &gtk::ScrolledWindow,
    max_page_size: Rc<Cell<(f64, f64)>>,
) {
    {
        let canvas_weak = canvas.downgrade();
        zoom_in.connect_clicked(move |_| {
            if let Some(c) = canvas_weak.upgrade() {
                c.set_zoom(zoomed_in(c.zoom()));
            }
        });
    }
    {
        let canvas_weak = canvas.downgrade();
        zoom_out.connect_clicked(move |_| {
            if let Some(c) = canvas_weak.upgrade() {
                c.set_zoom(zoomed_out(c.zoom()));
            }
        });
    }
    {
        let canvas_weak = canvas.downgrade();
        let scroll_weak = scroll.downgrade();
        let max_page_size = Rc::clone(&max_page_size);
        fit_width_btn.connect_clicked(move |_| {
            let Some(canvas) = canvas_weak.upgrade() else {
                return;
            };
            let Some(scroll) = scroll_weak.upgrade() else {
                return;
            };
            let (max_w, _) = max_page_size.get();
            if max_w <= 0.0 {
                return;
            }
            let viewport_w = scroll.hadjustment().page_size();
            let zoom = fit_to_width(max_w, viewport_w);
            canvas.set_zoom(zoom);
        });
    }
    {
        let canvas_weak = canvas.downgrade();
        let scroll_weak = scroll.downgrade();
        let max_page_size = Rc::clone(&max_page_size);
        fit_page_btn.connect_clicked(move |_| {
            let Some(canvas) = canvas_weak.upgrade() else {
                return;
            };
            let Some(scroll) = scroll_weak.upgrade() else {
                return;
            };
            let (max_w, max_h) = max_page_size.get();
            if max_w <= 0.0 || max_h <= 0.0 {
                return;
            }
            let viewport_w = scroll.hadjustment().page_size();
            let viewport_h = scroll.vadjustment().page_size();
            let zoom = fit_to_page((max_w, max_h), (viewport_w, viewport_h));
            canvas.set_zoom(zoom);
        });
    }
}

fn wire_selection_gestures(canvas: &PdfCanvas) {
    let drag = gtk::GestureDrag::builder().button(1).build();
    let anchor_cell: Rc<RefCell<Option<PagePoint>>> = Rc::new(RefCell::new(None));
    {
        let canvas_weak = canvas.downgrade();
        let anchor_cell = Rc::clone(&anchor_cell);
        drag.connect_drag_begin(move |g, x, y| {
            let Some(canvas) = canvas_weak.upgrade() else {
                return;
            };
            let hit = canvas.hit_test_page(x, y);
            let Some(anchor) = hit else {
                *anchor_cell.borrow_mut() = None;
                return;
            };
            g.set_state(gtk::EventSequenceState::Claimed);
            *anchor_cell.borrow_mut() = Some(anchor);
            canvas.set_selection(Some(PdfSelection {
                anchor,
                active: anchor,
                style: poppler::SelectionStyle::Glyph,
            }));
        });
    }
    {
        let canvas_weak = canvas.downgrade();
        let anchor_cell = Rc::clone(&anchor_cell);
        drag.connect_drag_update(move |g, dx, dy| {
            let Some(canvas) = canvas_weak.upgrade() else {
                return;
            };
            if anchor_cell.borrow().is_none() {
                return;
            }
            let Some((sx, sy)) = g.start_point() else {
                return;
            };
            let wx = sx + dx;
            let wy = sy + dy;
            let hit = canvas.hit_test_page(wx, wy);
            if let Some(active) = hit {
                canvas.update_selection_active(active);
            }
        });
    }
    {
        let anchor_cell = Rc::clone(&anchor_cell);
        drag.connect_drag_end(move |_g, _dx, _dy| {
            *anchor_cell.borrow_mut() = None;
        });
    }
    canvas.add_controller(drag);
}

fn wire_clipboard_key(view: &PdfView, canvas: &PdfCanvas, session: &Rc<DocumentSession>) {
    let key = gtk::EventControllerKey::new();
    key.set_propagation_phase(gtk::PropagationPhase::Capture);
    let canvas_weak = canvas.downgrade();
    let session = Rc::clone(session);
    key.connect_key_pressed(move |_ctl, keyval, _code, state| {
        let Some(canvas) = canvas_weak.upgrade() else {
            return glib::Propagation::Proceed;
        };
        let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
        if ctrl && (keyval == gdk::Key::c || keyval == gdk::Key::C) {
            let Some(sel) = canvas.selection() else {
                return glib::Propagation::Proceed;
            };
            let text = collect_selected_text(&session, &sel);
            if text.is_empty() {
                return glib::Propagation::Stop;
            }
            if let Some(display) = gdk::Display::default() {
                display.clipboard().set_text(&text);
            }
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    view.add_controller(key);
}

fn wire_page_keys(view: &PdfView, canvas: &PdfCanvas, scroll: &gtk::ScrolledWindow) {
    let key = gtk::EventControllerKey::new();
    key.set_propagation_phase(gtk::PropagationPhase::Capture);
    let canvas_weak = canvas.downgrade();
    let scroll_weak = scroll.downgrade();
    key.connect_key_pressed(move |_ctl, keyval, _code, state| {
        let Some(canvas) = canvas_weak.upgrade() else {
            return glib::Propagation::Proceed;
        };
        let Some(scroll) = scroll_weak.upgrade() else {
            return glib::Propagation::Proceed;
        };
        if !state.is_empty() {
            return glib::Propagation::Proceed;
        }
        match keyval {
            gdk::Key::Page_Down => {
                let next = canvas.current_page().saturating_add(1);
                canvas.scroll_to_page(&scroll, next);
                glib::Propagation::Stop
            }
            gdk::Key::Page_Up => {
                let prev = canvas.current_page().saturating_sub(1);
                canvas.scroll_to_page(&scroll, prev);
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        }
    });
    view.add_controller(key);
}

fn wire_page_entry(
    view: &PdfView,
    canvas: &PdfCanvas,
    scroll: &gtk::ScrolledWindow,
    page_entry: &gtk::Entry,
    session: &Rc<DocumentSession>,
) {
    let activate_canvas_weak = canvas.downgrade();
    let activate_scroll_weak = scroll.downgrade();
    let activate_view_weak = view.downgrade();
    let activate_session = Rc::clone(session);
    page_entry.connect_activate(move |entry| {
        let Some(canvas) = activate_canvas_weak.upgrade() else {
            return;
        };
        let Some(scroll) = activate_scroll_weak.upgrade() else {
            return;
        };
        let n_pages = activate_session.n_pages() as usize;
        let text = entry.text();
        let Some(target_1) = parse_page_input(text.as_str(), n_pages) else {
            return;
        };
        let target_0 = (target_1.saturating_sub(1)) as u32;
        canvas.scroll_to_page(&scroll, target_0);
        entry.set_text(&format!("{}", target_1));
        if let Some(view) = activate_view_weak.upgrade() {
            view.grab_focus();
        }
    });

    let esc_canvas_weak = canvas.downgrade();
    let esc_entry_weak = page_entry.downgrade();
    let esc_view_weak = view.downgrade();
    let key = gtk::EventControllerKey::new();
    key.set_propagation_phase(gtk::PropagationPhase::Capture);
    key.connect_key_pressed(move |_, keyval, _keycode, _state| {
        if keyval != gdk::Key::Escape {
            return glib::Propagation::Proceed;
        }
        let Some(entry) = esc_entry_weak.upgrade() else {
            return glib::Propagation::Proceed;
        };
        let cur = esc_canvas_weak
            .upgrade()
            .map(|c| c.current_page())
            .unwrap_or(0);
        entry.set_text(&format!("{}", cur + 1));
        if let Some(view) = esc_view_weak.upgrade() {
            view.grab_focus();
        }
        glib::Propagation::Stop
    });
    page_entry.add_controller(key);
}

fn wire_page_tracking(
    canvas: &PdfCanvas,
    scroll: &gtk::ScrolledWindow,
    page_entry: &gtk::Entry,
    _session: &Rc<DocumentSession>,
) {
    let vadj = scroll.vadjustment();
    let canvas_weak = canvas.downgrade();
    let entry_weak = page_entry.downgrade();
    let scroll_weak = scroll.downgrade();
    vadj.connect_value_changed(move |adj| {
        let Some(canvas) = canvas_weak.upgrade() else {
            return;
        };
        let Some(entry) = entry_weak.upgrade() else {
            return;
        };
        if entry.has_focus() {
            return;
        }
        let viewport_h = scroll_weak
            .upgrade()
            .map(|s| s.height() as f64)
            .unwrap_or(0.0);
        let v = adj.value();
        canvas.recompute_current_page(v, viewport_h);
        let cur = canvas.current_page();
        entry.set_text(&format!("{}", cur + 1));
    });
}

/// Q4: zoom toward `(cursor_x, cursor_y)` in document space.
/// Anchor is computed BEFORE zoom change; clamping happens AFTER
/// the canvas allocation pass via `idle_add_local_once`, so the
/// adjustments see the new `upper` and the anchor stays put near
/// bottom/right edges.
fn apply_zoom_centered(
    canvas: &PdfCanvas,
    scroll: &gtk::ScrolledWindow,
    new_zoom: f64,
    cursor_x: Option<f64>,
    cursor_y: Option<f64>,
) {
    let hadj = scroll.hadjustment();
    let vadj = scroll.vadjustment();
    let old_zoom = canvas.zoom();
    if (new_zoom - old_zoom).abs() < 1e-4 {
        return;
    }

    let cx = cursor_x.unwrap_or(scroll.width() as f64 * 0.5);
    let cy = cursor_y.unwrap_or(scroll.height() as f64 * 0.5);

    let doc_x = (hadj.value() + cx) / old_zoom;
    let doc_y = (vadj.value() + cy) / old_zoom;

    canvas.set_zoom(new_zoom);

    let scroll_weak = scroll.downgrade();
    let canvas_weak = canvas.downgrade();
    glib::idle_add_local_once(move || {
        let Some(scroll) = scroll_weak.upgrade() else {
            return;
        };
        let Some(canvas) = canvas_weak.upgrade() else {
            return;
        };
        let new_zoom = canvas.zoom();
        let target_h = doc_x * new_zoom - cx;
        let target_v = doc_y * new_zoom - cy;
        let hadj = scroll.hadjustment();
        let vadj = scroll.vadjustment();
        let h = target_h.clamp(
            hadj.lower(),
            (hadj.upper() - hadj.page_size()).max(hadj.lower()),
        );
        let v = target_v.clamp(
            vadj.lower(),
            (vadj.upper() - vadj.page_size()).max(vadj.lower()),
        );
        hadj.set_value(h);
        vadj.set_value(v);
    });
}

/// Parse a raw page-number string into a 1-indexed page number.
///
/// Returns `None` for empty, non-numeric, or negative input, or when
/// `n_pages == 0`.  Zero is silently clamped to `1`; values greater
/// than `n_pages` are silently clamped to `n_pages`.
pub(crate) fn parse_page_input(raw: &str, n_pages: usize) -> Option<usize> {
    if n_pages == 0 {
        return None;
    }
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let parsed = trimmed.parse::<usize>().ok()?;
    if parsed == 0 {
        Some(1)
    } else if parsed > n_pages {
        Some(n_pages)
    } else {
        Some(parsed)
    }
}

#[cfg(test)]
mod parse_page_input_tests {
    use super::parse_page_input;

    #[test]
    fn acceptance_criteria() {
        let cases: Vec<(&str, usize, Option<usize>)> = vec![
            ("", 10, None),
            ("abc", 10, None),
            ("0", 10, Some(1)),
            ("5", 10, Some(5)),
            ("999", 10, Some(10)),
            ("  7  ", 10, Some(7)),
            ("-3", 10, None),
        ];
        for (raw, n_pages, expected) in cases {
            assert_eq!(
                parse_page_input(raw, n_pages),
                expected,
                "input={:?}, n_pages={}",
                raw,
                n_pages
            );
        }
    }

    #[test]
    fn edge_cases() {
        assert_eq!(parse_page_input("0", 0), None, "n_pages=0 should yield None");
        assert_eq!(parse_page_input("1", 0), None, "n_pages=0 should yield None");
        assert_eq!(
            parse_page_input("1.5", 10),
            None,
            "decimal input should be rejected"
        );
    }
}
