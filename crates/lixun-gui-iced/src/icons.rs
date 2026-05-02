use std::path::PathBuf;

use lixun_core::{Category, Hit};

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

fn lookup_theme(name: &str, size: u16) -> Option<PathBuf> {
    freedesktop_icons::lookup(name)
        .with_size(size)
        .with_cache()
        .find()
}

pub fn resolve_icon(hit: &Hit, size: u16) -> Option<PathBuf> {
    if let Some(name) = hit.icon_name.as_deref() {
        let p = std::path::Path::new(name);
        if p.is_absolute() && p.exists() {
            return Some(p.to_path_buf());
        }
        if let Some(path) = lookup_theme(name, size) {
            return Some(path);
        }
    }
    lookup_theme(category_fallback(&hit.category), size)
}
