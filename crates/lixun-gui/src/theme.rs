//! Theme directory resolver for the launcher window.
//!
//! Themes are subfolders of `${XDG_CONFIG_HOME or ~/.config}/lixun/themes/`,
//! each containing a `style.css`. A theme is selected via the `theme`
//! field in the `[gui]` section of `config.toml`. When the
//! selected theme does not exist on disk, callers fall back to the
//! built-in stylesheet embedded at compile time.
//!
//! This module is filesystem-only: it does not touch GTK. The
//! [`crate::style_manager::StyleManager`] consumes the paths produced
//! here and feeds them into `gtk::CssProvider::load_from_path`.

use std::path::PathBuf;

/// Resolves theme directories and the user-wide CSS override under a
/// given config root.
///
/// `config_dir` is the parent of the `lixun/` namespace, i.e. the
/// value returned by [`dirs::config_dir`]. The resolver appends
/// `lixun/themes/<name>/style.css` or `lixun/style.css` as needed.
pub struct ThemeResolver {
    pub config_dir: PathBuf,
}

impl ThemeResolver {
    /// Construct a resolver rooted at `config_dir`.
    pub fn new(config_dir: PathBuf) -> Self {
        Self { config_dir }
    }

    /// Return the `style.css` path for `theme` if it exists on disk,
    /// otherwise `None`. An empty or whitespace-only name returns
    /// `None` without touching the filesystem.
    pub fn resolve(&self, theme: Option<&str>) -> Option<PathBuf> {
        let name = theme?.trim();
        if name.is_empty() {
            return None;
        }
        let candidate = self
            .config_dir
            .join("lixun")
            .join("themes")
            .join(name)
            .join("style.css");
        if candidate.is_file() {
            Some(candidate)
        } else {
            None
        }
    }

    /// Return the user-wide CSS override path
    /// (`${config_dir}/lixun/style.css`) regardless of whether the
    /// file exists. Callers test for existence themselves so the
    /// watcher can pick up the file when the user creates it for the
    /// first time.
    pub fn user_override(&self) -> PathBuf {
        self.config_dir.join("lixun").join("style.css")
    }

    /// Alias for [`Self::resolve`] used by callers that want the
    /// active theme's stylesheet path. Returns `None` when no theme
    /// is selected or the selected theme has no `style.css`.
    pub fn active_css_path(&self, theme: Option<&str>) -> Option<PathBuf> {
        self.resolve(theme)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_theme(root: &std::path::Path, name: &str) -> PathBuf {
        let dir = root.join("lixun").join("themes").join(name);
        fs::create_dir_all(&dir).unwrap();
        let css = dir.join("style.css");
        fs::write(&css, "/* test theme */").unwrap();
        css
    }

    #[test]
    fn resolve_returns_path_for_existing_theme() {
        let tmp = TempDir::new().unwrap();
        let css = make_theme(tmp.path(), "midnight");
        let resolver = ThemeResolver::new(tmp.path().to_path_buf());
        assert_eq!(resolver.resolve(Some("midnight")), Some(css));
    }

    #[test]
    fn resolve_returns_none_for_missing_theme() {
        let tmp = TempDir::new().unwrap();
        let resolver = ThemeResolver::new(tmp.path().to_path_buf());
        assert_eq!(resolver.resolve(Some("does-not-exist")), None);
    }

    #[test]
    fn resolve_returns_none_for_none_input() {
        let tmp = TempDir::new().unwrap();
        let resolver = ThemeResolver::new(tmp.path().to_path_buf());
        assert_eq!(resolver.resolve(None), None);
    }

    #[test]
    fn resolve_returns_none_for_empty_string() {
        let tmp = TempDir::new().unwrap();
        let resolver = ThemeResolver::new(tmp.path().to_path_buf());
        assert_eq!(resolver.resolve(Some("")), None);
        assert_eq!(resolver.resolve(Some("   ")), None);
    }

    #[test]
    fn user_override_returns_path_regardless_of_existence() {
        let tmp = TempDir::new().unwrap();
        let resolver = ThemeResolver::new(tmp.path().to_path_buf());
        let expected = tmp.path().join("lixun").join("style.css");
        assert_eq!(resolver.user_override(), expected);
        // Same answer after the file is created.
        fs::create_dir_all(tmp.path().join("lixun")).unwrap();
        fs::write(&expected, "/* user override */").unwrap();
        assert_eq!(resolver.user_override(), expected);
    }

    #[test]
    fn active_css_path_delegates_to_resolve() {
        let tmp = TempDir::new().unwrap();
        let css = make_theme(tmp.path(), "noir");
        let resolver = ThemeResolver::new(tmp.path().to_path_buf());
        assert_eq!(resolver.active_css_path(Some("noir")), Some(css));
        assert_eq!(resolver.active_css_path(None), None);
    }

    #[test]
    fn resolve_ignores_theme_without_style_css() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("lixun").join("themes").join("broken");
        fs::create_dir_all(&dir).unwrap();
        // Theme directory exists but lacks style.css.
        let resolver = ThemeResolver::new(tmp.path().to_path_buf());
        assert_eq!(resolver.resolve(Some("broken")), None);
    }
}
