//! GTK CSS provider stack for the launcher window.
//!
//! The launcher composes up to four providers, layered by GTK's
//! standard priority constants. The stack puts the matugen palette
//! at the very top so that an automated colour source recolours
//! every layer beneath it whenever it is enabled. Themes and user
//! overrides remain free to set selectors, layout, typography and
//! literal colours; matugen only wins for declarations of the
//! `--lixun-*` custom properties that the embedded stylesheet
//! consumes via `var()`.
//!
//! 1. **Embedded** — `STYLE_PROVIDER_PRIORITY_APPLICATION`. Loaded
//!    from `include_str!("../style.css")` at compile time. Never
//!    swapped at runtime. Acts as the irreducible default — every
//!    rule the launcher needs to function lives here, including the
//!    `--lixun-*` custom property fallbacks that keep the window
//!    visible when no other layer is present.
//! 2. **User override** — `APPLICATION + 1`. Loaded from
//!    `${config_dir}/lixun/style.css`. A permanent manual override
//!    of the embedded defaults, applied whether or not a theme is
//!    selected. Hot-swapped when the file changes.
//! 3. **Theme** — `APPLICATION + 2`. Loaded from
//!    `${config_dir}/lixun/themes/<active>/style.css` when a theme
//!    is selected and the file exists. Empty when no theme is
//!    selected or the theme directory is missing. Hot-swapped via
//!    `CssProvider::load_from_path` on theme change or theme-file
//!    edit.
//! 4. **Matugen colours** — `APPLICATION + 3`. Loaded from the path
//!    in `Config::gui.matugen.colors_path` (default
//!    `${config_dir}/lixun/colors.css`). Holds a Material You palette
//!    rendered by matugen via the lixun-owned template. Sits at the
//!    top of the cascade so the matugen `--lixun-*` declarations
//!    win over any matching declarations in the embedded sheet, the
//!    user override, or the active theme — themes and user overrides
//!    that reference tokens via `var(--lixun-*)` therefore get
//!    recoloured automatically. Themes that hard-code literal colours
//!    rather than tokens opt out of palette injection by virtue of
//!    not declaring the tokens at all. The layer is only installed
//!    when `Config::gui.matugen.enabled` is true; when disabled the
//!    provider is omitted from the stack entirely so the layer is a
//!    true no-op. Hot-swapped when the file changes.
//!
//! `CssProvider::load_from_path` parses the file and atomically swaps
//! the provider's parsed rule set on the GTK main thread, so callers
//! never observe a half-applied stylesheet.

use std::path::PathBuf;

use crate::theme::ThemeResolver;

const EMBEDDED_STYLESHEET: &str = include_str!("../style.css");

/// The layered CSS provider stack installed on the default GDK
/// display. Construct with [`Self::install`]; drive at runtime with
/// [`Self::apply_theme`], [`Self::reload_user_css`], and
/// [`Self::reload_colors_css`].
///
/// `colors_provider` is `Some` only when matugen integration is
/// enabled in the daemon config. When disabled the field is `None`
/// and the corresponding GTK provider slot is left unregistered, so
/// the layer cannot interfere with the rest of the stack.
pub(crate) struct StyleManager {
    #[allow(dead_code)]
    embedded_provider: gtk::CssProvider,
    /// `[gui] opacity` override layer (A4b): a generated rule that
    /// re-declares `.lixun-window`'s blur-on background with the
    /// configured surface alpha. Registered at the SAME priority as
    /// the embedded sheet but AFTER it, so it wins by order while
    /// every higher layer (user/theme/matugen) and every
    /// higher-specificity rule (`.lixun-no-blur`, `.lixun-light`)
    /// still override it. Empty (a true no-op) while the configured
    /// value equals the shipped default, so default installs carry
    /// zero risk of divergence from style.css.
    opacity_provider: gtk::CssProvider,
    theme_provider: gtk::CssProvider,
    user_provider: gtk::CssProvider,
    colors_provider: Option<gtk::CssProvider>,
    colors_path: PathBuf,
    pub resolver: ThemeResolver,
}

/// Generated CSS for a non-default `[gui] opacity`. Mirrors the
/// `.lixun-window` background declaration in `style.css` with the
/// alpha swapped; `None` for the default value (no override needed).
fn surface_opacity_css(opacity: f64) -> Option<String> {
    if (opacity - lixun_config::DEFAULT_SURFACE_OPACITY).abs() < 0.001 {
        return None;
    }
    let alpha = opacity.clamp(0.0, 1.0);
    Some(format!(
        ".lixun-window {{\n\
         \x20   background:\n\
         \x20       linear-gradient(180deg,\n\
         \x20           rgba(255, 255, 255, 0.035) 0%,\n\
         \x20           rgba(255, 255, 255, 0.0)  50%),\n\
         \x20       alpha(var(--lixun-surface), {alpha});\n\
         }}\n"
    ))
}

impl StyleManager {
    /// Install the providers on `display` and load the initial
    /// stylesheets.
    ///
    /// * `theme` is the theme name from `Config::gui.theme`; pass
    ///   `None` for the default (embedded-only) appearance.
    /// * `matugen_enabled` mirrors `Config::gui.matugen.enabled`.
    ///   When false the matugen layer is not installed at all.
    /// * `matugen_colors_path` is the absolute path the matugen
    ///   layer will load from when enabled, normally
    ///   `Config::gui.matugen.colors_path`.
    pub fn install(
        display: &gtk::gdk::Display,
        theme: Option<&str>,
        matugen_enabled: bool,
        matugen_colors_path: PathBuf,
        surface_opacity: f64,
    ) -> Self {
        let config_dir = dirs::config_dir().unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
        let resolver = ThemeResolver::new(config_dir);

        let embedded_provider = gtk::CssProvider::new();
        embedded_provider.load_from_string(EMBEDDED_STYLESHEET);
        gtk::style_context_add_provider_for_display(
            display,
            &embedded_provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );

        // Same priority as embedded, registered after it: equal-
        // priority providers apply in registration order, so this
        // wins over the embedded rule it mirrors without outranking
        // the user/theme/matugen layers (see field docs).
        let opacity_provider = gtk::CssProvider::new();
        gtk::style_context_add_provider_for_display(
            display,
            &opacity_provider,
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

        let colors_provider = if matugen_enabled {
            let provider = gtk::CssProvider::new();
            gtk::style_context_add_provider_for_display(
                display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 3,
            );
            Some(provider)
        } else {
            None
        };

        let manager = Self {
            embedded_provider,
            opacity_provider,
            theme_provider,
            user_provider,
            colors_provider,
            colors_path: matugen_colors_path,
            resolver,
        };

        manager.set_surface_opacity(surface_opacity);
        manager.apply_theme(theme);
        manager.reload_user_css();
        manager.reload_colors_css();
        manager
    }

    /// Apply (or clear) the `[gui] opacity` override (A4b). Called at
    /// install and on every config live-reload.
    pub fn set_surface_opacity(&self, opacity: f64) {
        match surface_opacity_css(opacity) {
            Some(css) => self.opacity_provider.load_from_string(&css),
            None => self.opacity_provider.load_from_string(""),
        }
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

    /// Reload the matugen-generated palette layer from disk. No-op
    /// when the matugen layer is disabled (the field is `None`) or
    /// when the file is absent (the provider is cleared so a stale
    /// palette stops affecting the window). Safe to call repeatedly.
    pub fn reload_colors_css(&self) {
        let Some(provider) = self.colors_provider.as_ref() else {
            return;
        };
        if self.colors_path.is_file() {
            tracing::info!(
                "loading matugen palette from {}",
                self.colors_path.display(),
            );
            provider.load_from_path(&self.colors_path);
        } else {
            provider.load_from_string("");
        }
    }

    /// The matugen colours path the manager is currently watching.
    // (see also the surface_opacity tests at the bottom of the file)
    /// Used by the style watcher to subscribe to the right file even
    /// when the user has overridden `Config::gui.matugen.colors_path`.
    ///
    /// `daemon_config` is the canonical source today, but this
    /// accessor is part of the public API surface so callers outside
    /// `window.rs` can resolve through the manager.
    #[allow(dead_code)]
    pub fn colors_path(&self) -> &std::path::Path {
        &self.colors_path
    }
}

#[cfg(test)]
mod tests {
    use super::surface_opacity_css;

    #[test]
    fn default_opacity_generates_no_override() {
        assert!(surface_opacity_css(lixun_config::DEFAULT_SURFACE_OPACITY).is_none());
        assert!(surface_opacity_css(0.7004).is_none());
    }

    #[test]
    fn custom_opacity_generates_window_rule() {
        let css = surface_opacity_css(0.5).expect("override expected");
        assert!(css.contains(".lixun-window"));
        assert!(css.contains("alpha(var(--lixun-surface), 0.5)"));
    }

    #[test]
    fn out_of_range_opacity_is_clamped() {
        let css = surface_opacity_css(1.7).expect("override expected");
        assert!(css.contains("alpha(var(--lixun-surface), 1)"));
    }
}
