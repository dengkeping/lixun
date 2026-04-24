//! Icon resolution for hits: theme icon names, absolute-path icons,
//! category fallbacks. Returns GdkPaintable for use in `gtk::Image::set_from_paintable`.

use std::cell::OnceCell;

use gtk::gdk;
use gtk::prelude::*;
use lixun_core::{Category, Hit};

thread_local! {
    static THEME: OnceCell<Option<gtk::IconTheme>> = const { OnceCell::new() };
}

fn icon_theme() -> Option<gtk::IconTheme> {
    THEME.with(|cell| {
        cell.get_or_init(|| gdk::Display::default().map(|d| gtk::IconTheme::for_display(&d)))
            .clone()
    })
}

fn category_fallback(cat: &Category) -> &'static str {
    match cat {
        Category::App => "application-x-executable",
        Category::File => "text-x-generic",
        Category::Mail => "mail-message",
        Category::Attachment => "mail-attachment",
        Category::Calculator => "accessories-calculator",
        Category::Shell => "utilities-terminal",
    }
}

fn lookup_theme_icon(theme: &gtk::IconTheme, name: &str, size: i32) -> Option<gdk::Paintable> {
    if !theme.has_icon(name) {
        return None;
    }
    let paintable = theme.lookup_icon(
        name,
        &[],
        size,
        1,
        gtk::TextDirection::Ltr,
        gtk::IconLookupFlags::empty(),
    );
    Some(paintable.upcast::<gdk::Paintable>())
}

fn lookup_absolute_path_icon(path: &std::path::Path, size: i32) -> Option<gdk::Paintable> {
    if !path.exists() {
        return None;
    }
    let texture = gdk::Texture::from_filename(path).ok()?;
    let _ = size;
    Some(texture.upcast::<gdk::Paintable>())
}

pub(crate) fn resolve_icon(hit: &Hit, size: i32) -> Option<gdk::Paintable> {
    let theme = icon_theme()?;

    if let Some(name) = hit.icon_name.as_deref() {
        let p = std::path::Path::new(name);
        if p.is_absolute()
            && let Some(paintable) = lookup_absolute_path_icon(p, size)
        {
            return Some(paintable);
        }
        if let Some(paintable) = lookup_theme_icon(&theme, name, size) {
            return Some(paintable);
        }
    }

    let fallback = category_fallback(&hit.category);
    lookup_theme_icon(&theme, fallback, size)
}
