//! `PdfView` — public widget tree exported by this crate.
//!
//! Layout:
//! ```text
//! PdfView (gtk::Box vertical)
//! ├── PdfToolbar (gtk::ActionBar) — page label + ± zoom buttons
//! └── PdfScroll  (gtk::ScrolledWindow, kinetic OFF — Q3)
//!     └── PdfCanvas (custom gtk::Widget subclass — see canvas.rs)
//! ```
//!
//! `PdfView::replace_path` is the entry point used by
//! `PreviewPlugin::update`: bumps the session epoch, reopens all
//! three Documents (main + 2 workers), clears the texture cache,
//! resets the viewport to top.
//!
//! TODO PR2c: outline sidebar and `PreviewCapabilities` extension.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

use crate::canvas::{MAX_ZOOM, MIN_ZOOM, PdfCanvas};
use crate::document_session::DocumentSession;
use crate::selection::{PagePoint, PdfSelection, collect_selected_text};
use crate::worker::RenderResult;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct PdfView {
        pub canvas: RefCell<Option<PdfCanvas>>,
        pub scroll: RefCell<Option<gtk::ScrolledWindow>>,
        pub page_label: RefCell<Option<gtk::Label>>,
        pub session: RefCell<Option<Rc<DocumentSession>>>,
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

        let (tx, rx) = async_channel::unbounded::<RenderResult>();
        let session = DocumentSession::open(path, tx)?;

        let toolbar = gtk::ActionBar::new();
        toolbar.add_css_class("lixun-preview-pdf-toolbar");
        let zoom_out = gtk::Button::from_icon_name("zoom-out-symbolic");
        let zoom_in = gtk::Button::from_icon_name("zoom-in-symbolic");
        let page_label = gtk::Label::new(Some(&format!("1 / {}", session.n_pages().max(1))));
        toolbar.pack_start(&zoom_out);
        toolbar.pack_start(&zoom_in);
        toolbar.set_center_widget(Some(&page_label));

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
        view.append(&scroll);

        *view.imp().canvas.borrow_mut() = Some(canvas.clone());
        *view.imp().scroll.borrow_mut() = Some(scroll.clone());
        *view.imp().page_label.borrow_mut() = Some(page_label.clone());
        *view.imp().session.borrow_mut() = Some(Rc::clone(&session));

        wire_render_pump(&canvas, rx);
        wire_gestures(&canvas, &scroll);
        wire_selection_gestures(&canvas);
        wire_clipboard_keys(&view, &canvas, &session);
        wire_buttons(&canvas, &zoom_in, &zoom_out);
        wire_page_tracking(&canvas, &scroll, &page_label, &session);

        Ok(view)
    }

    pub fn replace_path(&self, new_path: PathBuf) -> anyhow::Result<()> {
        tracing::info!("PdfView replace_path: enter path={:?}", new_path);
        let session = self
            .imp()
            .session
            .borrow()
            .clone()
            .ok_or_else(|| anyhow::anyhow!("PdfView has no session"))?;
        session.replace_path(new_path)?;

        if let Some(canvas) = self.imp().canvas.borrow().as_ref() {
            canvas.set_zoom(1.0);
            canvas.set_current_page(0);
            canvas.rebuild_page_widgets();
            canvas.queue_resize();
            canvas.queue_draw();
            // Defer a second resize+redraw to the next idle tick.
            // `update()` runs while the preview window may still be
            // hidden (host calls `set_visible(true)` AFTER plugin
            // update). At update-time `canvas.width()` is 0 and the
            // synchronous `allocate_children` inside
            // `rebuild_page_widgets` produces 0-sized child rects.
            // GTK does not always re-allocate the canvas after the
            // window becomes visible if the canvas's outer
            // allocation is unchanged, leaving page children at
            // 0x0 and the preview frozen on a white page until the
            // next user input. Forcing a fresh resize cycle once
            // the window has been presented unsticks the layout.
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
            label.set_text(&format!("1 / {}", session.n_pages().max(1)));
        }
        tracing::info!("PdfView replace_path: exit");
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

fn wire_buttons(canvas: &PdfCanvas, zoom_in: &gtk::Button, zoom_out: &gtk::Button) {
    {
        let canvas_weak = canvas.downgrade();
        zoom_in.connect_clicked(move |_| {
            if let Some(c) = canvas_weak.upgrade() {
                let cur = c.zoom();
                c.set_zoom((cur * 1.25).clamp(MIN_ZOOM, MAX_ZOOM));
            }
        });
    }
    {
        let canvas_weak = canvas.downgrade();
        zoom_out.connect_clicked(move |_| {
            if let Some(c) = canvas_weak.upgrade() {
                let cur = c.zoom();
                c.set_zoom((cur / 1.25).clamp(MIN_ZOOM, MAX_ZOOM));
            }
        });
    }
}

/// Left-drag on the canvas drives text selection. Claims the
/// gesture on `drag_begin` so the middle-drag pan handler on the
/// parent ScrolledWindow does not steal it.
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
            let Some(anchor) = canvas.hit_test_page(x, y) else {
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
            if let Some(active) = canvas.hit_test_page(wx, wy) {
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

/// Ctrl+C copies the currently selected text to the system
/// clipboard. Escape with no active search clears the selection.
fn wire_clipboard_keys(view: &PdfView, canvas: &PdfCanvas, session: &Rc<DocumentSession>) {
    let key = gtk::EventControllerKey::new();
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
        if keyval == gdk::Key::Escape && canvas.selection().is_some() {
            canvas.clear_selection();
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    view.add_controller(key);
}
fn wire_page_tracking(
    canvas: &PdfCanvas,
    scroll: &gtk::ScrolledWindow,
    page_label: &gtk::Label,
    session: &Rc<DocumentSession>,
) {
    let vadj = scroll.vadjustment();
    let canvas_weak = canvas.downgrade();
    let label_weak = page_label.downgrade();
    let scroll_weak = scroll.downgrade();
    let session = Rc::clone(session);
    vadj.connect_value_changed(move |adj| {
        let Some(canvas) = canvas_weak.upgrade() else {
            return;
        };
        let Some(label) = label_weak.upgrade() else {
            return;
        };
        let viewport_h = scroll_weak
            .upgrade()
            .map(|s| s.height() as f64)
            .unwrap_or(0.0);
        let v = adj.value();
        canvas.recompute_current_page(v, viewport_h);
        let cur = canvas.current_page();
        let n = session.n_pages().max(1);
        let text = format!("{} / {}", cur + 1, n);
        tracing::info!(
            "page tracking: vadj={} viewport_h={} → page={} n={} label='{}'",
            v,
            viewport_h,
            cur,
            n,
            text
        );
        label.set_text(&text);
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
