//! Bottom status bar: loading spinner, empty-state with web-search fallback,
//! transient copy toast. Exposed as a self-contained widget that `window.rs`
//! appends and drives.

use gtk::prelude::*;

use crate::factory::add_css_class;

pub(crate) struct StatusBar {
    revealer: gtk::Revealer,
    content: gtk::Box,
    /// Bumped on every state change so a pending toast auto-hide
    /// timeout can tell whether the bar still shows *its* toast; a
    /// stale timeout must not collapse a newer loading/empty/error
    /// state that replaced the toast within its 1.5 s lifetime.
    epoch: std::rc::Rc<std::cell::Cell<u64>>,
}

impl StatusBar {
    pub(crate) fn new() -> Self {
        let revealer = gtk::Revealer::new();
        revealer.set_widget_name("lixun-status");
        revealer.set_transition_type(gtk::RevealerTransitionType::Crossfade);
        revealer.set_transition_duration(200);
        revealer.set_reveal_child(false);
        // CRITICAL: a GtkRevealer with Crossfade transition animates
        // opacity, NOT box size; its child's natural height keeps
        // occupying layout space the moment it is first revealed,
        // and a subsequent set_reveal_child(false) only fades the
        // child to transparent. The layout gap stays behind as
        // "phantom bottom margin" that grows on every subsequent
        // empty-clear cycle. Hide the revealer itself with
        // set_visible(false) so GTK drops it from allocation
        // entirely. show_* methods must flip it back on.
        revealer.set_visible(false);

        let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        content.set_widget_name("lixun-status-inner");
        content.set_margin_top(6);
        content.set_margin_bottom(6);
        content.set_margin_start(8);
        content.set_margin_end(8);
        add_css_class(&content, "lixun-status");

        revealer.set_child(Some(&content));

        Self {
            revealer,
            content,
            epoch: std::rc::Rc::new(std::cell::Cell::new(0)),
        }
    }

    pub(crate) fn widget(&self) -> &gtk::Revealer {
        &self.revealer
    }

    fn clear(&self) {
        self.epoch.set(self.epoch.get() + 1);
        while let Some(child) = self.content.first_child() {
            self.content.remove(&child);
        }
    }

    /// Screen-reader announcement for a state transition (A1). The
    /// status bar swaps label widgets silently — Orca users get no
    /// signal that a search finished empty, errored, or copied —
    /// so every state change routes through `announce()` (gtk 4.14
    /// API, enabled via the pinned v4_16 feature) at Medium priority.
    fn announce(&self, message: &str) {
        gtk::prelude::AccessibleExt::announce(
            &self.revealer,
            message,
            gtk::AccessibleAnnouncementPriority::Medium,
        );
    }

    /// Shared chassis for the spinner states (loading, slow search,
    /// indexing): spinner + label, revealed.
    fn show_spinner(&self, text: &str) {
        self.clear();
        let spinner = gtk::Spinner::new();
        spinner.start();
        let label = gtk::Label::new(Some(text));
        add_css_class(&label, "lixun-status-label");
        self.content.append(&spinner);
        self.content.append(&label);
        self.revealer.set_visible(true);
        self.revealer.set_reveal_child(true);
        self.announce(text);
    }

    pub(crate) fn show_loading(&self) {
        self.show_spinner("Searching\u{2026}");
    }

    /// The IPC read watchdog expired without a Final. The reader
    /// keeps the connection open, so a slow Final can still arrive
    /// and replace this; meanwhile the spinner stays up so the state
    /// reads as "still working", never as an authoritative
    /// "No results".
    pub(crate) fn show_still_searching(&self) {
        self.show_spinner("Still searching \u{2014} the daemon is slow\u{2026}");
    }

    /// First-run/reindex state for a zero-hit query: while the
    /// indexer is still filling the index an empty result is not
    /// authoritative, so present progress instead of the definitive
    /// empty state.
    pub(crate) fn show_indexing(&self, indexed_docs: u64) {
        self.show_spinner(&format!(
            "Indexing\u{2026} {} documents so far \u{2014} results may be incomplete",
            indexed_docs
        ));
    }

    /// Transport to the daemon failed (connect/write/read error).
    /// Keeps the user's query intact and offers the existing
    /// `app.relaunch` action — actionable, unlike a fabricated
    /// "No results".
    pub(crate) fn show_daemon_unresponsive(&self) {
        self.clear();
        let label = gtk::Label::new(Some("Search daemon isn't responding"));
        add_css_class(&label, "lixun-status-label");
        add_css_class(&label, "lixun-status-error");
        label.set_hexpand(true);
        label.set_halign(gtk::Align::Start);
        self.content.append(&label);

        let button = gtk::Button::with_label("Relaunch");
        add_css_class(&button, "lixun-status-action");
        button.connect_clicked(|_| {
            if let Some(app) = gtk::gio::Application::default() {
                app.activate_action("relaunch", None);
            }
        });
        self.content.append(&button);
        self.revealer.set_visible(true);
        self.revealer.set_reveal_child(true);
        self.announce("Search daemon isn't responding");
    }

    pub(crate) fn show_empty(&self, query: &str) {
        self.show_empty_with_note(query, None);
    }

    /// Empty state with an optional trailing annotation line — used
    /// by the zero-hit path to disclose that semantic search is
    /// configured but its worker is down (F6), so "No results" is not
    /// mistaken for an authoritative full-index answer.
    pub(crate) fn show_empty_with_note(&self, query: &str, note: Option<&str>) {
        self.clear();
        let text = if query.is_empty() {
            "No results".to_string()
        } else {
            format!("No results for \u{201C}{}\u{201D}", query)
        };
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let label = gtk::Label::new(Some(&text));
        add_css_class(&label, "lixun-status-label");
        label.set_hexpand(true);
        label.set_halign(gtk::Align::Start);
        row.append(&label);

        if !query.is_empty() {
            let button = gtk::Button::with_label("Search the web");
            add_css_class(&button, "lixun-status-action");
            let q = query.to_string();
            button.connect_clicked(move |_| {
                open_web_search(&q);
            });
            row.append(&button);
        }

        let column = gtk::Box::new(gtk::Orientation::Vertical, 2);
        column.set_hexpand(true);
        column.append(&row);
        let mut announced = text.clone();
        if let Some(note) = note {
            let note_label = gtk::Label::new(Some(note));
            add_css_class(&note_label, "lixun-status-label");
            add_css_class(&note_label, "lixun-status-note");
            note_label.set_halign(gtk::Align::Start);
            note_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            column.append(&note_label);
            announced.push_str(". ");
            announced.push_str(note);
        }
        self.content.append(&column);
        self.revealer.set_visible(true);
        self.revealer.set_reveal_child(true);
        self.announce(&announced);
    }

    /// Persistent dimmed key-hint line shown while results are
    /// visible (O3): built by the caller from the LIVE resolved
    /// keybindings so user rebinds display truthfully. Unlike the
    /// other states it is not announced — it is passive chrome, and
    /// announcing it after every result render would spam readers.
    pub(crate) fn show_hints(&self, hints: &str) {
        self.clear();
        let label = gtk::Label::new(Some(hints));
        add_css_class(&label, "lixun-status-label");
        add_css_class(&label, "lixun-status-hints");
        label.set_hexpand(true);
        label.set_halign(gtk::Align::Start);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        self.content.append(&label);
        self.revealer.set_visible(true);
        self.revealer.set_reveal_child(true);
    }

    /// Report a failed launch without dismissing the launcher.
    ///
    /// The counterpart to keeping the window open on `Err`: a silent failure
    /// plus an auto-hiding launcher is indistinguishable from a successful
    /// launch, which is exactly how a broken open path stays invisible.
    pub(crate) fn show_error(&self, message: &str) {
        self.clear();
        let label = gtk::Label::new(Some(message));
        add_css_class(&label, "lixun-status-label");
        add_css_class(&label, "lixun-status-error");
        label.set_hexpand(true);
        label.set_halign(gtk::Align::Start);
        // Long io::Error / anyhow chains must not stretch the launcher.
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        label.set_tooltip_text(Some(message));
        self.content.append(&label);
        self.revealer.set_visible(true);
        self.revealer.set_reveal_child(true);
        self.announce(message);
    }

    // Calculator results are presented as a normal hit row (the
    // calculator source emits the evaluated value as the hit title);
    // the former `show_calculation` status-bar path was dead code.

    /// Transient confirmation ("Copied: …") that auto-hides after
    /// 1.5 s. Any other `show_*`/`hide` within that window wins: the
    /// timeout checks the epoch and leaves newer content alone.
    pub(crate) fn show_toast(&self, message: &str) {
        self.clear();
        let label = gtk::Label::new(Some(message));
        add_css_class(&label, "lixun-status-label");
        label.set_hexpand(true);
        label.set_halign(gtk::Align::Start);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        self.content.append(&label);
        self.revealer.set_visible(true);
        self.revealer.set_reveal_child(true);
        self.announce(message);

        let shown_at = self.epoch.get();
        let epoch = std::rc::Rc::clone(&self.epoch);
        let revealer = self.revealer.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(1500), move || {
            if epoch.get() != shown_at {
                return;
            }
            // Same phantom-margin fix as `hide`: drop the revealer
            // out of allocation, don't just fade the child.
            revealer.set_reveal_child(false);
            revealer.set_visible(false);
        });
    }

    pub(crate) fn hide(&self) {
        // Drop the revealer out of layout immediately (see note in
        // `new`). Skip the fade-out animation on hide: instant
        // collapse matches the Spotlight UX of "empty query =
        // pristine launcher" better than a 200 ms crossfade that
        // still leaves a visible gap during the transition.
        self.revealer.set_reveal_child(false);
        self.revealer.set_visible(false);
    }
}

/// Open a web search for `query` in the default browser. Shared by
/// the status bar's "Search the web" button and the keymap's
/// Enter-on-zero-results fallback so both paths build the identical
/// URL.
pub(crate) fn open_web_search(query: &str) {
    let encoded = urlencode(query);
    let url = format!("https://duckduckgo.com/?q={}", encoded);
    if let Err(e) = opener::open(&url) {
        tracing::error!("Failed to open web search URL: {}", e);
    }
}

fn urlencode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::urlencode;

    #[test]
    fn test_urlencode_plain() {
        assert_eq!(urlencode("hello"), "hello");
    }

    #[test]
    fn test_urlencode_spaces() {
        assert_eq!(urlencode("hello world"), "hello+world");
    }

    #[test]
    fn test_urlencode_special() {
        assert_eq!(urlencode("a&b=c"), "a%26b%3Dc");
    }

    #[test]
    fn test_urlencode_unicode() {
        assert_eq!(urlencode("café"), "caf%C3%A9");
    }
}
