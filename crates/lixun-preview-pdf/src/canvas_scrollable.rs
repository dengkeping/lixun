//! Scrollable-interface plumbing for [`super::PdfCanvas`].
//!
//! Pulled into a sibling module so [`super`] stays under the
//! 500-line budget once selection state + hit-testing land. No
//! behaviour change; this is a pure move.

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

use super::PdfCanvas;
use super::imp;

pub(super) fn scrollable_param_specs() -> Vec<glib::ParamSpec> {
    vec![
        glib::ParamSpecOverride::for_interface::<gtk::Scrollable>("hadjustment"),
        glib::ParamSpecOverride::for_interface::<gtk::Scrollable>("vadjustment"),
        glib::ParamSpecOverride::for_interface::<gtk::Scrollable>("hscroll-policy"),
        glib::ParamSpecOverride::for_interface::<gtk::Scrollable>("vscroll-policy"),
    ]
}

impl ScrollableImpl for imp::PdfCanvas {}

impl PdfCanvas {
    pub(super) fn set_hadjustment_inner(&self, adj: Option<gtk::Adjustment>) {
        let imp = self.imp();
        if let Some(old_id) = imp.hadj_signal.borrow_mut().take()
            && let Some(old_adj) = imp.hadjustment.borrow().as_ref()
        {
            old_adj.disconnect(old_id);
        }
        if let Some(ref a) = adj {
            let weak = self.downgrade();
            let id = a.connect_value_changed(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.queue_allocate();
                }
            });
            *imp.hadj_signal.borrow_mut() = Some(id);
        }
        *imp.hadjustment.borrow_mut() = adj;
        let (w, h) = (self.width(), self.height());
        self.reconfigure_adjustments(w, h);
    }

    pub(super) fn set_vadjustment_inner(&self, adj: Option<gtk::Adjustment>) {
        let imp = self.imp();
        if let Some(old_id) = imp.vadj_signal.borrow_mut().take()
            && let Some(old_adj) = imp.vadjustment.borrow().as_ref()
        {
            old_adj.disconnect(old_id);
        }
        if let Some(ref a) = adj {
            let weak = self.downgrade();
            let id = a.connect_value_changed(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.queue_allocate();
                }
            });
            *imp.vadj_signal.borrow_mut() = Some(id);
        }
        *imp.vadjustment.borrow_mut() = adj;
        let (w, h) = (self.width(), self.height());
        self.reconfigure_adjustments(w, h);
    }
}
