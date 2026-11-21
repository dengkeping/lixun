//! Exercise the mounted GTK view in its own display-test process.
use gtk::{glib, prelude::*, subclass::prelude::ObjectSubclassIsExt};
use lixun_preview_pdf::PdfView;
use std::path::PathBuf;

#[test]
#[ignore = "requires a GTK display"]
fn fragment_selects_and_scrolls_to_page_two() {
    gtk::init().unwrap();
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/preview/pdf/sample-text.pdf");
    let view = PdfView::new(path.clone()).unwrap();
    let window = gtk::Window::builder()
        .default_width(440)
        .default_height(360)
        .child(&view)
        .build();
    window.present();
    for (quote, expected_page) in [("Second page", 1), ("Introduction", 0)] {
        if expected_page == 0 {
            view.replace_path(path.clone()).unwrap();
        }
        view.find_quote(quote);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            while glib::MainContext::default().iteration(false) {}
            let state = view.imp().search_state.borrow();
            let state = state.as_ref().unwrap().borrow();
            if let Some((page, rect)) = state.query.current_rect()
                && page == expected_page
                && rect.x2() > rect.x1()
            {
                let offset = view
                    .imp()
                    .scroll
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .vadjustment()
                    .value();
                if expected_page == 0 || offset > 0.0 {
                    break;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "fragment {quote} was not revealed"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    window.close();
}
