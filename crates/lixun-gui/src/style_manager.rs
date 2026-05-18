//! GTK CSS provider stack for the launcher window.
//!
//! The launcher composes three providers, layered by GTK's standard
//! priority constants:
//!
//! 1. **Embedded** — `STYLE_PROVIDER_PRIORITY_APPLICATION`. Loaded
//!    from `include_str!("../style.css")` at compile time. Never
//!    swapped at runtime. Acts as the irreducible default — every
//!    rule the launcher needs to function lives here.
//! 2. **User override** — `APPLICATION + 1`. Loaded from
//!    `${config_dir}/lixun/style.css`. A permanent override of the
//!    embedded defaults, applied whether or not a theme is selected.
//!    Hot-swapped when the file changes.
//! 3. **Theme** — `APPLICATION + 2`. Loaded from
//!    `${config_dir}/lixun/themes/<active>/style.css` when a theme is
//!    selected and the file exists. Empty when no theme is selected
//!    or the theme directory is missing. Sits at the top of the stack
//!    so an explicitly chosen theme wins over the user override
//!    (mirroring Hyprland's `source =` semantics where the later
//!    source wins). Hot-swapped via `CssProvider::load_from_path` on
//!    theme change or theme-file edit.
//!
//! `CssProvider::load_from_path` parses the file and atomically swaps
//! the provider's parsed rule set on the GTK main thread, so callers
//! never observe a half-applied stylesheet.

use crate::theme::ThemeResolver;

const EMBEDDED_STYLESHEET: &str = include_str!("../style.css");

/// The three-layer CSS provider stack installed on the default GDK
/// display. Construct with [`Self::install`]; drive at runtime with
/// [`Self::apply_theme`] and [`Self::reload_user_css`].
pub(crate) struct StyleManager {
    #[allow(dead_code)]
    embedded_provider: gtk::CssProvider,
    theme_provider: gtk::CssProvider,
    user_provider: gtk::CssProvider,
    pub resolver: ThemeResolver,
}

impl StyleManager {
    /// Install the three providers on `display` and load the initial
    /// stylesheets. `theme` is the theme name from
    /// `Config::gui.theme`; pass `None` for the default
    /// (embedded-only) appearance.
    pub fn install(display: &gtk::gdk::Display, theme: Option<&str>) -> Self {
        let config_dir = dirs::config_dir().unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
        let resolver = ThemeResolver::new(config_dir);

        let embedded_provider = gtk::CssProvider::new();
        embedded_provider.load_from_string(EMBEDDED_STYLESHEET);
        gtk::style_context_add_provider_for_display(
            display,
            &embedded_provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );

        let user_provider = gtk::CssProvider::new();
        gtk::style_context_add_provider_for_display(
            display,
            &user_provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
        );

        let theme_provider = gtk::CssProvider::new();
        gtk::style_context_add_provider_for_display(
            display,
            &theme_provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 2,
        );

        let manager = Self {
            embedded_provider,
            theme_provider,
            user_provider,
            resolver,
        };

        manager.apply_theme(theme);
        manager.reload_user_css();
        manager
    }

    /// Swap the theme layer to point at `theme`'s `style.css`. When
    /// the theme is `None` or its directory is missing the layer is
    /// cleared (load_from_string("")) so the previous theme stops
    /// affecting the window.
    pub fn apply_theme(&self, theme: Option<&str>) {
        match self.resolver.resolve(theme) {
            Some(path) => {
                tracing::info!("loading theme stylesheet from {}", path.display());
                self.theme_provider.load_from_path(&path);
            }
            None => {
                if theme.is_some() {
                    tracing::warn!(
                        "theme {:?} not found at {}; falling back to embedded stylesheet",
                        theme,
                        self.resolver
                            .config_dir
                            .join("lixun")
                            .join("themes")
                            .display(),
                    );
                }
                self.theme_provider.load_from_string("");
            }
        }
    }

    /// Reload the user-wide CSS override from disk. When the file is
    /// missing the layer is cleared. Safe to call repeatedly.
    pub fn reload_user_css(&self) {
        let path = self.resolver.user_override();
        if path.is_file() {
            tracing::info!("loading user CSS override from {}", path.display());
            self.user_provider.load_from_path(&path);
        } else {
            self.user_provider.load_from_string("");
        }
    }
}
