//! Image preview plugin.
//!
//! Handles raster (png/jpeg/gif/webp/avif/bmp/tiff/ico) and vector
//! (svg) formats. Static raster images go through
//! `gdk::Texture::from_filename` for fast GPU-backed rendering.
//! Animated formats (gif, animated webp) and SVG delegate to
//! `gtk::MediaFile::for_filename` / `gtk::Picture::for_filename`
//! so GTK handles the loop/vector scaling pipeline.
//!
//! A footer label under the `Picture` shows the intrinsic
//! dimensions and on-disk file size so the user does not need to
//! alt-tab to a file manager to check "how big is this".

use std::path::Path;

use gtk::prelude::*;
use lixun_core::{Action, Hit};
use lixun_preview::{PreviewPlugin, PreviewPluginCfg, PreviewPluginEntry};

const STRONG_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "avif", "bmp", "tiff", "tif", "svg", "ico",
];

/// Extensions we route to `MediaFile` instead of `Texture` because
/// they may be animated. Static-only decoders use the faster
/// texture path; the media path starts a playback pipeline which
/// is overhead for a single frame.
const ANIMATED_EXTENSIONS: &[&str] = &["gif", "webp"];

/// Extensions that GTK renders via librsvg — vectors, so we use
/// `Picture::for_filename` to let GTK rescale on window resize.
const VECTOR_EXTENSIONS: &[&str] = &["svg"];

pub struct ImagePreview;

impl PreviewPlugin for ImagePreview {
    fn id(&self) -> &'static str {
        "image"
    }

    fn match_score(&self, hit: &Hit) -> u32 {
        let path = match &hit.action {
            Action::OpenFile { path } | Action::ShowInFileManager { path } => path,
            _ => return 0,
        };

        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            let lower = ext.to_ascii_lowercase();
            if STRONG_EXTENSIONS.iter().any(|&e| e == lower) {
                return 80;
            }
        }

        if hit
            .kind_label
            .as_deref()
            .is_some_and(|m| m.starts_with("image/"))
        {
            return 50;
        }

        0
    }

    fn build(&self, hit: &Hit, _cfg: &PreviewPluginCfg<'_>) -> anyhow::Result<gtk::Widget> {
        let path = match &hit.action {
            Action::OpenFile { path } | Action::ShowInFileManager { path } => path.clone(),
            _ => anyhow::bail!("image plugin: hit has no openable path"),
        };

        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();

        let picture = gtk::Picture::new();
        picture.set_content_fit(gtk::ContentFit::Contain);
        picture.set_can_shrink(true);
        picture.set_hexpand(true);
        picture.set_vexpand(true);
        picture.add_css_class("lixun-preview-image");

        let mut intrinsic: Option<(i32, i32)> = None;

        if VECTOR_EXTENSIONS.iter().any(|&e| e == ext) {
            picture.set_filename(Some(&path));
        } else if ANIMATED_EXTENSIONS.iter().any(|&e| e == ext) {
            let media = gtk::MediaFile::for_filename(&path);
            media.set_loop(true);
            media.play();
            picture.set_paintable(Some(&media));
        } else {
            match gdk::Texture::from_filename(&path) {
                Ok(texture) => {
                    intrinsic = Some((texture.width(), texture.height()));
                    picture.set_paintable(Some(&texture));
                }
                Err(e) => {
                    tracing::warn!(
                        "image: texture decode failed for {:?} ({}), falling back to Picture::set_filename",
                        path,
                        e
                    );
                    picture.set_filename(Some(&path));
                }
            }
        }

        let scroll = gtk::ScrolledWindow::new();
        scroll.set_hscrollbar_policy(gtk::PolicyType::Automatic);
        scroll.set_vscrollbar_policy(gtk::PolicyType::Automatic);
        scroll.set_child(Some(&picture));
        scroll.set_hexpand(true);
        scroll.set_vexpand(true);

        let footer = gtk::Label::new(Some(&format_footer(&path, intrinsic)));
        footer.set_xalign(0.0);
        footer.set_margin_top(4);
        footer.set_margin_bottom(8);
        footer.set_margin_start(16);
        footer.set_margin_end(16);
        footer.add_css_class("lixun-preview-image-footer");

        let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
        vbox.append(&scroll);
        vbox.append(&footer);
        vbox.add_css_class("lixun-preview-image-container");

        tracing::info!(
            "image: rendered {:?} ext={} intrinsic={:?}",
            path,
            ext,
            intrinsic
        );

        Ok(vbox.upcast())
    }
}

fn format_footer(path: &Path, intrinsic: Option<(i32, i32)>) -> String {
    let size_bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let size_str = human_bytes(size_bytes);
    match intrinsic {
        Some((w, h)) => format!("{} × {}   ·   {}", w, h, size_str),
        None => size_str,
    }
}

fn human_bytes(n: u64) -> String {
    const UNITS: &[(&str, u64)] = &[
        ("GiB", 1024 * 1024 * 1024),
        ("MiB", 1024 * 1024),
        ("KiB", 1024),
    ];
    for (unit, factor) in UNITS {
        if n >= *factor {
            return format!("{:.1} {}", n as f64 / *factor as f64, unit);
        }
    }
    format!("{} B", n)
}

inventory::submit! {
    PreviewPluginEntry {
        factory: || Box::new(ImagePreview),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lixun_core::paths::canonical_fs_doc_id;
    use lixun_core::{Category, DocId};
    use std::path::PathBuf;

    fn file_hit(path: impl Into<PathBuf>, kind: Option<&str>) -> Hit {
        let path = path.into();
        Hit {
            id: DocId(canonical_fs_doc_id(&path)),
            category: Category::File,
            title: path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            subtitle: path.display().to_string(),
            icon_name: None,
            kind_label: kind.map(str::to_string),
            score: 1.0,
            action: Action::OpenFile { path },
            extract_fail: false,
            sender: None,
            recipients: None,
            body: None,
            secondary_action: None,
        }
    }

    #[test]
    fn png_scores_eighty() {
        let hit = file_hit("/tmp/x.png", None);
        assert_eq!(ImagePreview.match_score(&hit), 80);
    }

    #[test]
    fn jpeg_uppercase_scores_eighty() {
        let hit = file_hit("/tmp/photo.JPEG", None);
        assert_eq!(ImagePreview.match_score(&hit), 80);
    }

    #[test]
    fn svg_scores_eighty() {
        let hit = file_hit("/tmp/logo.svg", None);
        assert_eq!(ImagePreview.match_score(&hit), 80);
    }

    #[test]
    fn mime_image_scores_fifty_without_extension() {
        let hit = file_hit("/tmp/noext", Some("image/png"));
        assert_eq!(ImagePreview.match_score(&hit), 50);
    }

    #[test]
    fn text_mime_does_not_match() {
        let hit = file_hit("/tmp/whatever", Some("text/plain"));
        assert_eq!(ImagePreview.match_score(&hit), 0);
    }

    #[test]
    fn no_file_action_no_score() {
        let hit = Hit {
            id: DocId("app:firefox".into()),
            category: Category::App,
            title: "Firefox".into(),
            subtitle: String::new(),
            icon_name: None,
            kind_label: None,
            score: 1.0,
            action: Action::Launch {
                exec: "firefox".into(),
                terminal: false,
                desktop_id: None,
                desktop_file: None,
                working_dir: None,
            },
            extract_fail: false,
            sender: None,
            recipients: None,
            body: None,
            secondary_action: None,
        };
        assert_eq!(ImagePreview.match_score(&hit), 0);
    }

    #[test]
    fn image_beats_text_for_png() {
        let hit = file_hit("/tmp/shot.png", Some("text/plain"));
        assert!(
            ImagePreview.match_score(&hit) > 50,
            "image plugin must win the png extension even if kind_label is wrong"
        );
    }

    #[test]
    fn human_bytes_scales() {
        assert_eq!(human_bytes(42), "42 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
        assert_eq!(human_bytes(3 * 1024 * 1024 + 512 * 1024), "3.5 MiB");
        assert_eq!(human_bytes(5 * 1024 * 1024 * 1024), "5.0 GiB");
    }

    #[test]
    fn format_footer_with_intrinsic() {
        let tmp = std::env::temp_dir().join(format!("lixun-image-fmt-{}.dat", std::process::id()));
        std::fs::write(&tmp, vec![0u8; 10240]).unwrap();
        let s = format_footer(&tmp, Some((1920, 1080)));
        std::fs::remove_file(&tmp).ok();
        assert!(s.starts_with("1920 × 1080"));
        assert!(s.contains("KiB"));
    }
}
