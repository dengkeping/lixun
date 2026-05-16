//! Runtime registration of plugin-bundled symbolic icons.
//!
//! GTK's symbolic recolour pipeline only kicks in when the icon is
//! resolved through `gtk::IconTheme`, not when a `gdk::Texture` is
//! loaded from raw bytes. We do not ship an `.gresource`, so this
//! module materialises the SVG into a per-user cache dir on first
//! use and points the live icon theme at it.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

use gtk::gdk;
use gtk::glib;

const MARQUEE_ICON_NAME: &str = "lixun-marquee-select-symbolic";

const MARQUEE_SYMBOLIC_SVG: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16">
  <rect x="3" y="3" width="10" height="10" fill="#bebebe" opacity="0.25"/>
  <rect x="2" y="2" width="2.5" height="1" fill="#bebebe"/>
  <rect x="6.75" y="2" width="2.5" height="1" fill="#bebebe"/>
  <rect x="11.5" y="2" width="2.5" height="1" fill="#bebebe"/>
  <rect x="2" y="13" width="2.5" height="1" fill="#bebebe"/>
  <rect x="6.75" y="13" width="2.5" height="1" fill="#bebebe"/>
  <rect x="11.5" y="13" width="2.5" height="1" fill="#bebebe"/>
  <rect x="2" y="2" width="1" height="2.5" fill="#bebebe"/>
  <rect x="2" y="6.75" width="1" height="2.5" fill="#bebebe"/>
  <rect x="2" y="11.5" width="1" height="2.5" fill="#bebebe"/>
  <rect x="13" y="2" width="1" height="2.5" fill="#bebebe"/>
  <rect x="13" y="6.75" width="1" height="2.5" fill="#bebebe"/>
  <rect x="13" y="11.5" width="1" height="2.5" fill="#bebebe"/>
</svg>
"##;

static ICON_ROOT: OnceLock<Option<PathBuf>> = OnceLock::new();

fn write_icon_once() -> Option<PathBuf> {
    ICON_ROOT
        .get_or_init(|| {
            let cache = glib::user_cache_dir();
            let root = cache.join("lixun").join("icons");
            let actions_dir = root.join("hicolor").join("scalable").join("actions");
            fs::create_dir_all(&actions_dir).ok()?;
            let path = actions_dir.join(format!("{MARQUEE_ICON_NAME}.svg"));
            let needs_write = match fs::metadata(&path) {
                Ok(m) => m.len() as usize != MARQUEE_SYMBOLIC_SVG.len(),
                Err(_) => true,
            };
            if needs_write {
                let mut f = fs::File::create(&path).ok()?;
                f.write_all(MARQUEE_SYMBOLIC_SVG).ok()?;
            }
            Some(root)
        })
        .clone()
}

pub fn marquee_icon_name() -> &'static str {
    MARQUEE_ICON_NAME
}

pub fn ensure_registered(display: &gdk::Display) {
    let Some(root) = write_icon_once() else { return };
    let theme = gtk::IconTheme::for_display(display);
    if !theme.search_path().iter().any(|p| p == &root) {
        theme.add_search_path(&root);
    }
}
