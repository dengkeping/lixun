//! `PdfSearchBar` — inline search UI for [`super::PdfView`].
//!
//! A simple horizontal `gtk::Box` wrapper: SearchEntry + counter
//! Label + Prev/Next buttons + Close button. Emits four signals
//! the host wires to mutate [`crate::search::SearchQueryState`]:
//! `query-changed`, `next-match`, `prev-match`, `close-requested`.
//!
//! The bar is constructed hidden; the host toggles visibility on
//! Ctrl+F.

use gtk::glib;
use gtk::glib::subclass::Signal;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use std::cell::RefCell;
use std::sync::OnceLock;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct PdfSearchBar {
        pub entry: RefCell<Option<gtk::SearchEntry>>,
        pub counter: RefCell<Option<gtk::Label>>,
        pub prev: RefCell<Option<gtk::Button>>,
        pub next: RefCell<Option<gtk::Button>>,
        pub close: RefCell<Option<gtk::Button>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PdfSearchBar {
        const NAME: &'static str = "LixunPdfSearchBar";
        type Type = super::PdfSearchBar;
        type ParentType = gtk::Box;
    }

    impl ObjectImpl for PdfSearchBar {
        fn signals() -> &'static [Signal] {
            static SIGS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGS.get_or_init(|| {
                vec![
                    Signal::builder("query-changed")
                        .param_types([str::static_type()])
                        .build(),
                    Signal::builder("next-match").build(),
                    Signal::builder("prev-match").build(),
                    Signal::builder("close-requested").build(),
                ]
            })
        }

        fn constructed(&self) {
            self.parent_constructed();
            self.obj().build_ui();
        }
    }

    impl WidgetImpl for PdfSearchBar {}
    impl BoxImpl for PdfSearchBar {}
}

glib::wrapper! {
    pub struct PdfSearchBar(ObjectSubclass<imp::PdfSearchBar>)
        @extends gtk::Widget, gtk::Box,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
}

impl Default for PdfSearchBar {
    fn default() -> Self {
        Self::new()
    }
}

impl PdfSearchBar {
    pub fn new() -> Self {
        let bar: Self = glib::Object::builder()
            .property("orientation", gtk::Orientation::Horizontal)
            .property("spacing", 6)
            .build();
        bar.add_css_class("lixun-preview-pdf-searchbar");
        bar.set_visible(false);
        bar
    }

    fn build_ui(&self) {
        let entry = gtk::SearchEntry::new();
        entry.set_hexpand(true);
        let counter = gtk::Label::new(Some(""));
        counter.add_css_class("dim-label");
        let prev = gtk::Button::from_icon_name("go-up-symbolic");
        prev.set_tooltip_text(Some("Previous match"));
        let next = gtk::Button::from_icon_name("go-down-symbolic");
        next.set_tooltip_text(Some("Next match"));
        let close = gtk::Button::from_icon_name("window-close-symbolic");
        close.set_tooltip_text(Some("Close search"));

        self.append(&entry);
        self.append(&counter);
        self.append(&prev);
        self.append(&next);
        self.append(&close);

        *self.imp().entry.borrow_mut() = Some(entry.clone());
        *self.imp().counter.borrow_mut() = Some(counter);
        *self.imp().prev.borrow_mut() = Some(prev.clone());
        *self.imp().next.borrow_mut() = Some(next.clone());
        *self.imp().close.borrow_mut() = Some(close.clone());

        let weak = self.downgrade();
        entry.connect_search_changed(move |e| {
            if let Some(this) = weak.upgrade() {
                let text = e.text().to_string();
                this.emit_by_name::<()>("query-changed", &[&text]);
            }
        });
        let weak = self.downgrade();
        entry.connect_activate(move |_| {
            if let Some(this) = weak.upgrade() {
                this.emit_by_name::<()>("next-match", &[]);
            }
        });
        let weak = self.downgrade();
        let key = gtk::EventControllerKey::new();
        key.connect_key_pressed(move |_c, keyval, _code, state| {
            let Some(this) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if keyval == gdk::Key::Return || keyval == gdk::Key::KP_Enter {
                if state.contains(gdk::ModifierType::SHIFT_MASK) {
                    this.emit_by_name::<()>("prev-match", &[]);
                } else {
                    this.emit_by_name::<()>("next-match", &[]);
                }
                return glib::Propagation::Stop;
            }
            if keyval == gdk::Key::Escape {
                this.emit_by_name::<()>("close-requested", &[]);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        entry.add_controller(key);

        let weak = self.downgrade();
        prev.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.emit_by_name::<()>("prev-match", &[]);
            }
        });
        let weak = self.downgrade();
        next.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.emit_by_name::<()>("next-match", &[]);
            }
        });
        let weak = self.downgrade();
        close.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.emit_by_name::<()>("close-requested", &[]);
            }
        });
    }

    pub fn focus_entry(&self) {
        if let Some(entry) = self.imp().entry.borrow().as_ref() {
            entry.grab_focus();
        }
    }

    pub fn clear_query(&self) {
        if let Some(entry) = self.imp().entry.borrow().as_ref() {
            entry.set_text("");
        }
        self.set_counter_text("");
    }

    pub fn set_counter_text(&self, text: &str) {
        if let Some(label) = self.imp().counter.borrow().as_ref() {
            label.set_text(text);
        }
    }

    pub fn query(&self) -> String {
        self.imp()
            .entry
            .borrow()
            .as_ref()
            .map(|e| e.text().to_string())
            .unwrap_or_default()
    }
}
