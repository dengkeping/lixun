//! Assistive-technology surface of [`super::PdfCanvas`] (A8).
//!
//! The canvas paints page textures and exposes no accessible
//! children of its own, so a screen reader would otherwise land on
//! an unlabeled pixel area. The canvas carries the `Img` accessible
//! role (set at construction in `PdfCanvas::new`); this module keeps
//! its label ("Page N of M — file") current on every page or
//! document change and publishes the current page's extracted text
//! as the accessible description.
//!
//! Lives in a sibling file so `canvas.rs` stays close to its size
//! cap, mirroring `canvas_scrollable.rs` / `canvas_search.rs`.

use super::*;

/// Character cap for the accessible description sourced from page
/// text. Screen readers read descriptions linearly; a few hundred
/// characters orient the user without narrating the whole page.
pub(crate) const A11Y_DESCRIPTION_MAX_CHARS: usize = 300;

/// 1-indexed page-position label for assistive tech. Pure so
/// `canvas_tests` can exercise it headlessly.
pub(crate) fn page_accessible_label(current_page_0: u32, n_pages: u32, filename: &str) -> String {
    format!(
        "Page {} of {} \u{2014} {}",
        current_page_0.saturating_add(1),
        n_pages.max(1),
        filename
    )
}

/// Trim `text` to at most `max_chars` characters, appending an
/// ellipsis when truncated. Char-based so multi-byte input cannot
/// be split mid code point. Pure for headless unit testing.
pub(crate) fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let cut: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{cut}\u{2026}")
}

impl PdfCanvas {
    /// Refresh the accessible label ("Page N of M — file") for the
    /// current page. Synchronous and cheap (string format + property
    /// write, no poppler involved), so it is safe to call from every
    /// actual page change.
    ///
    /// Called from `set_current_page` (only when the page really
    /// changed) and from `rebuild_page_widgets` (document opened or
    /// swapped, where filename and page count change too).
    pub fn refresh_accessible_state(&self) {
        let Some(session) = self.session() else {
            return;
        };
        let filename = session
            .path()
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let label = page_accessible_label(self.current_page(), session.n_pages(), &filename);
        self.update_property(&[gtk::accessible::Property::Label(&label)]);
    }

    /// Publish the current page's extracted text as the accessible
    /// description. Called only on document arrival
    /// (`rebuild_page_widgets`), never per page change: text
    /// extraction reuses the session's main-thread poppler path (the
    /// same lazily-opened `Document` the selection overlay and
    /// copy-text use), and every `doc.page(idx)` touch grows that
    /// Document's catalog page-tree cache — extracting on every page
    /// change would slowly materialise the multi-MB-per-page cache
    /// the allocation pass works hard to avoid (see the visibility
    /// gating comment in canvas.rs). Runs in an idle callback with
    /// an epoch guard so it never delays the triggering frame and a
    /// rapid document swap cannot publish stale text.
    pub(crate) fn refresh_accessible_description(&self) {
        let Some(session) = self.session() else {
            return;
        };
        let epoch = session.current_epoch();
        let page_idx = self.current_page();
        let canvas_weak = self.downgrade();
        glib::idle_add_local_once(move || {
            let Some(canvas) = canvas_weak.upgrade() else {
                return;
            };
            let Some(session) = canvas.session() else {
                return;
            };
            // Drop stale work: the document was swapped before this
            // idle ran; the swap scheduled its own refresh.
            if session.current_epoch() != epoch {
                return;
            }
            // Always write the property — an empty description for a
            // textless (e.g. scanned) document must overwrite the
            // previous document's text, not leave it dangling.
            let description = session
                .main_page(page_idx)
                .and_then(|page| page.text())
                .map(|text| truncate_chars(text.trim(), A11Y_DESCRIPTION_MAX_CHARS))
                .unwrap_or_default();
            canvas.update_property(&[gtk::accessible::Property::Description(&description)]);
        });
    }
}
