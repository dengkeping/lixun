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

use std::cell::Cell;
use std::path::Path;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gdk, glib};
use lixun_core::{Action, Hit};
use lixun_preview::{PreviewCapabilities, PreviewPlugin, PreviewPluginCfg, PreviewPluginEntry};

mod canvas;
use canvas::{ImageCanvas, zoomed_in, zoomed_out, MAX_ZOOM, MIN_ZOOM, ZOOM_STEP};

const STRONG_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "avif", "bmp", "tiff", "tif", "svg", "ico",
    "heic", "heif", "jxl",
    "cr2", "cr3", "nef", "nrw", "arw", "srf", "sr2", "dng", "raf", "orf", "rw2", "pef",
];

/// Extensions we route to `MediaFile` instead of `Texture` because
/// they may be animated. Static-only decoders use the faster
/// texture path; the media path starts a playback pipeline which
/// is overhead for a single frame.
const ANIMATED_EXTENSIONS: &[&str] = &["gif", "webp"];

/// Extensions that GTK renders via librsvg — vectors, so we use
/// `Picture::for_filename` to let GTK rescale on window resize.
const VECTOR_EXTENSIONS: &[&str] = &["svg"];

/// Pixels panned per unit of two-finger scroll delta. Touchpad deltas are
/// small fractional values per event, so this stays modest to keep panning
/// smooth rather than jumpy.
const SCROLL_PAN_STEP: f64 = 12.0;

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

    fn capabilities(&self) -> PreviewCapabilities {
        PreviewCapabilities {
            zoomable: true,
            ..PreviewCapabilities::default()
        }
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

        let mut intrinsic: Option<(i32, i32)> = None;

        let scroll = gtk::ScrolledWindow::new();
        scroll.set_hscrollbar_policy(gtk::PolicyType::Automatic);
        scroll.set_vscrollbar_policy(gtk::PolicyType::Automatic);
        scroll.set_hexpand(true);
        scroll.set_vexpand(true);
        // Honour the canvas' own natural size so an oversized image overflows the
        // viewport and the scrollbars (hence panning) become active.
        scroll.set_propagate_natural_width(true);
        scroll.set_propagate_natural_height(true);

        let is_vector = VECTOR_EXTENSIONS.iter().any(|&e| e == ext);
        let is_animated = ANIMATED_EXTENSIONS.iter().any(|&e| e == ext);

        let decoded_texture = if is_vector || is_animated {
            None
        } else {
            decode_texture(&path, &mut intrinsic)
        };

        let mut toolbar: Option<gtk::Widget> = None;

        if let Some(texture) = decoded_texture {
            let canvas = ImageCanvas::new();
            canvas.set_texture(&texture);
            canvas.add_css_class("lixun-preview-image");
            canvas.set_focusable(true);
            canvas.set_can_focus(true);
            wire_canvas_gestures(&canvas, &scroll);
            toolbar = Some(build_image_toolbar(&canvas, &scroll));
            scroll.set_child(Some(&canvas));
        } else {
            let picture = gtk::Picture::new();
            picture.set_content_fit(gtk::ContentFit::Contain);
            picture.set_can_shrink(true);
            picture.set_hexpand(true);
            picture.set_vexpand(true);
            picture.add_css_class("lixun-preview-image");

            if is_vector {
                picture.set_filename(Some(&path));
            } else if is_animated {
                let media = gtk::MediaFile::for_filename(&path);
                media.set_loop(true);
                media.play();
                picture.set_paintable(Some(&media));
            } else {
                picture.set_filename(Some(&path));
            }
            scroll.set_child(Some(&picture));
        }

        let footer = gtk::Label::new(Some(&format_footer(&path, intrinsic)));
        footer.set_xalign(0.0);
        footer.set_margin_top(4);
        footer.set_margin_bottom(8);
        footer.set_margin_start(16);
        footer.set_margin_end(16);
        footer.add_css_class("lixun-preview-image-footer");

        let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
        if let Some(toolbar) = &toolbar {
            vbox.append(toolbar);
        }
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

/// Decode `path` into a GPU texture, recording its intrinsic size.
/// Returns `None` (and logs) when decoding fails, so the caller can
/// fall back to GTK's own loader via `Picture::set_filename`.
fn decode_texture(path: &Path, intrinsic: &mut Option<(i32, i32)>) -> Option<gdk::Texture> {
    #[cfg(feature = "image-decode")]
    let result: anyhow::Result<gdk::Texture> = (|| {
        let img = lixun_image_decode::decode_to_dynamic_image(path)?;
        let width = img.width() as i32;
        let height = img.height() as i32;
        let rgba = img.to_rgba8();
        let bytes = glib::Bytes::from_owned(rgba.into_raw());
        let texture = gdk::MemoryTexture::new(
            width,
            height,
            gdk::MemoryFormat::R8g8b8a8,
            &bytes,
            (width * 4) as usize,
        );
        *intrinsic = Some((width, height));
        Ok(texture.upcast())
    })();

    #[cfg(not(feature = "image-decode"))]
    let result: anyhow::Result<gdk::Texture> = gdk::Texture::from_filename(path)
        .map(|texture| {
            *intrinsic = Some((texture.width(), texture.height()));
            texture
        })
        .map_err(Into::into);

    match result {
        Ok(texture) => Some(texture),
        Err(e) => {
            tracing::warn!(
                "image: texture decode failed for {:?} ({}), falling back to Picture::set_filename",
                path,
                e
            );
            None
        }
    }
}

fn toolbar_button(icon: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::from_icon_name(icon);
    button.set_tooltip_text(Some(tooltip));
    button.set_focus_on_click(false);
    button.add_css_class("flat");
    button
}

fn build_image_toolbar(canvas: &ImageCanvas, scroll: &gtk::ScrolledWindow) -> gtk::Widget {
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    bar.set_halign(gtk::Align::Center);
    bar.set_margin_top(4);
    bar.set_margin_bottom(4);
    bar.add_css_class("lixun-preview-image-toolbar");

    let zoom_out = toolbar_button("zoom-out-symbolic", "Zoom out");
    let zoom_in = toolbar_button("zoom-in-symbolic", "Zoom in");
    let zoom_reset = toolbar_button("zoom-original-symbolic", "Reset zoom");
    let rotate_left = toolbar_button("object-rotate-left-symbolic", "Rotate left");
    let rotate_right = toolbar_button("object-rotate-right-symbolic", "Rotate right");

    {
        let canvas_weak = canvas.downgrade();
        let scroll_weak = scroll.downgrade();
        zoom_out.connect_clicked(move |_| {
            if let (Some(canvas), Some(scroll)) = (canvas_weak.upgrade(), scroll_weak.upgrade()) {
                apply_zoom_centered(&canvas, &scroll, zoomed_out(canvas.zoom()), None, None);
            }
        });
    }
    {
        let canvas_weak = canvas.downgrade();
        let scroll_weak = scroll.downgrade();
        zoom_in.connect_clicked(move |_| {
            if let (Some(canvas), Some(scroll)) = (canvas_weak.upgrade(), scroll_weak.upgrade()) {
                apply_zoom_centered(&canvas, &scroll, zoomed_in(canvas.zoom()), None, None);
            }
        });
    }
    {
        let canvas_weak = canvas.downgrade();
        let scroll_weak = scroll.downgrade();
        zoom_reset.connect_clicked(move |_| {
            if let (Some(canvas), Some(scroll)) = (canvas_weak.upgrade(), scroll_weak.upgrade()) {
                apply_zoom_centered(&canvas, &scroll, 1.0, None, None);
            }
        });
    }
    {
        let canvas_weak = canvas.downgrade();
        rotate_left.connect_clicked(move |_| {
            if let Some(canvas) = canvas_weak.upgrade() {
                canvas.rotate_ccw();
            }
        });
    }
    {
        let canvas_weak = canvas.downgrade();
        rotate_right.connect_clicked(move |_| {
            if let Some(canvas) = canvas_weak.upgrade() {
                canvas.rotate_cw();
            }
        });
    }

    bar.append(&zoom_out);
    bar.append(&zoom_in);
    bar.append(&zoom_reset);
    bar.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    bar.append(&rotate_left);
    bar.append(&rotate_right);

    bar.upcast()
}

/// Wire pinch/scroll zoom, drag panning, Shift+drag marquee selection,
/// and keyboard zoom/rotate onto an [`ImageCanvas`] inside `scroll`.
/// Ported from the PDF viewer's gesture pattern.
fn wire_canvas_gestures(canvas: &ImageCanvas, scroll: &gtk::ScrolledWindow) {
    // Pinch-to-zoom (touchpad): scale relative to the zoom at gesture start.
    let zoom_gesture = gtk::GestureZoom::new();
    let initial = Rc::new(Cell::new(1.0_f64));
    {
        let canvas_weak = canvas.downgrade();
        let initial = Rc::clone(&initial);
        zoom_gesture.connect_begin(move |_g, _seq| {
            if let Some(canvas) = canvas_weak.upgrade() {
                initial.set(canvas.zoom());
            }
        });
    }
    {
        let canvas_weak = canvas.downgrade();
        let scroll_weak = scroll.downgrade();
        zoom_gesture.connect_scale_changed(move |_g, scale| {
            let (Some(canvas), Some(scroll)) = (canvas_weak.upgrade(), scroll_weak.upgrade()) else {
                return;
            };
            let target = (initial.get() * scale).clamp(MIN_ZOOM, MAX_ZOOM);
            apply_zoom_centered(&canvas, &scroll, target, None, None);
        });
    }
    canvas.add_controller(zoom_gesture);

    // Ctrl+scroll zooms around the cursor; plain (two-finger) scroll pans the
    // viewport on both axes.
    let scroll_ctrl =
        gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
    {
        let canvas_weak = canvas.downgrade();
        let scroll_weak = scroll.downgrade();
        scroll_ctrl.connect_scroll(move |ctl, dx, dy| {
            let (Some(canvas), Some(scroll)) = (canvas_weak.upgrade(), scroll_weak.upgrade()) else {
                return glib::Propagation::Proceed;
            };
            let state = ctl.current_event_state();
            if !state.contains(gdk::ModifierType::CONTROL_MASK) {
                let hadj = scroll.hadjustment();
                let vadj = scroll.vadjustment();
                hadj.set_value(hadj.value() + dx * SCROLL_PAN_STEP);
                vadj.set_value(vadj.value() + dy * SCROLL_PAN_STEP);
                return glib::Propagation::Stop;
            }
            let factor = if dy < 0.0 { ZOOM_STEP } else { 1.0 / ZOOM_STEP };
            let target = (canvas.zoom() * factor).clamp(MIN_ZOOM, MAX_ZOOM);
            apply_zoom_centered(&canvas, &scroll, target, None, None);
            glib::Propagation::Stop
        });
    }
    canvas.add_controller(scroll_ctrl);

    // Middle-button drag pans the viewport. The gesture lives on the
    // ScrolledWindow, not the canvas, so it receives button-2 events without
    // competing with the canvas's primary-button selection gesture.
    let pan = gtk::GestureDrag::builder().button(2).build();
    let pan_start = Rc::new(Cell::new((0.0_f64, 0.0_f64)));
    {
        let scroll_weak = scroll.downgrade();
        let pan_start = Rc::clone(&pan_start);
        pan.connect_drag_begin(move |g, _x, _y| {
            g.set_state(gtk::EventSequenceState::Claimed);
            if let Some(scroll) = scroll_weak.upgrade() {
                pan_start.set((scroll.hadjustment().value(), scroll.vadjustment().value()));
            }
        });
    }
    {
        let scroll_weak = scroll.downgrade();
        let pan_start = Rc::clone(&pan_start);
        pan.connect_drag_update(move |_g, dx, dy| {
            if let Some(scroll) = scroll_weak.upgrade() {
                let (sx, sy) = pan_start.get();
                scroll.hadjustment().set_value(sx - dx);
                scroll.vadjustment().set_value(sy - dy);
            }
        });
    }
    scroll.add_controller(pan);

    // Primary-button drag draws a selection marquee; a stationary click outside
    // an existing marquee releases it. Capture phase so the gesture pre-empts the
    // ScrolledWindow's built-in drag. Panning lives on the middle button / scroll.
    let select = gtk::GestureDrag::builder().button(1).build();
    select.set_propagation_phase(gtk::PropagationPhase::Capture);
    let sel_origin = Rc::new(Cell::new((0.0_f64, 0.0_f64)));
    {
        let canvas_weak = canvas.downgrade();
        let sel_origin = Rc::clone(&sel_origin);
        select.connect_drag_begin(move |g, x, y| {
            g.set_state(gtk::EventSequenceState::Claimed);
            sel_origin.set((x, y));
            if let Some(canvas) = canvas_weak.upgrade() {
                canvas.clear_marquee();
            }
        });
    }
    {
        let canvas_weak = canvas.downgrade();
        let sel_origin = Rc::clone(&sel_origin);
        select.connect_drag_update(move |_g, dx, dy| {
            if let Some(canvas) = canvas_weak.upgrade() {
                let (ox, oy) = sel_origin.get();
                if let Some(rect) = canvas.widget_drag_to_image_rect(ox, oy, dx, dy) {
                    canvas.set_marquee(rect);
                }
            }
        });
    }
    {
        let canvas_weak = canvas.downgrade();
        let sel_origin = Rc::clone(&sel_origin);
        select.connect_drag_end(move |_g, dx, dy| {
            // A near-zero drag is a plain click: release the marquee if the press
            // landed outside it.
            if dx.abs() >= 3.0 || dy.abs() >= 3.0 {
                return;
            }
            if let Some(canvas) = canvas_weak.upgrade() {
                let (ox, oy) = sel_origin.get();
                if canvas.has_marquee() && !canvas.marquee_contains_widget_point(ox, oy) {
                    canvas.clear_marquee();
                }
            }
        });
    }
    canvas.add_controller(select);

    // Keyboard: Ctrl +/-/0 zoom, R / Shift+R rotate, Ctrl+C copies the marquee.
    let key = gtk::EventControllerKey::new();
    key.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let canvas_weak = canvas.downgrade();
        key.connect_key_pressed(move |_ctl, keyval, _code, state| {
            let Some(canvas) = canvas_weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
            let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
            match keyval {
                gdk::Key::equal | gdk::Key::plus if ctrl => {
                    canvas.set_zoom(zoomed_in(canvas.zoom()));
                    glib::Propagation::Stop
                }
                gdk::Key::minus | gdk::Key::underscore if ctrl => {
                    canvas.set_zoom(zoomed_out(canvas.zoom()));
                    glib::Propagation::Stop
                }
                gdk::Key::_0 if ctrl => {
                    canvas.set_zoom(1.0);
                    glib::Propagation::Stop
                }
                gdk::Key::r | gdk::Key::R => {
                    if shift {
                        canvas.rotate_ccw();
                    } else {
                        canvas.rotate_cw();
                    }
                    glib::Propagation::Stop
                }
                gdk::Key::c | gdk::Key::C if ctrl => {
                    if let Some(texture) = canvas.marquee_texture() {
                        if let Some(display) = gdk::Display::default() {
                            display.clipboard().set_texture(&texture);
                        }
                        canvas.clear_marquee();
                    }
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
    }
    scroll.add_controller(key);

    let canvas_weak = canvas.downgrade();
    scroll.connect_map(move |_| {
        if let Some(canvas) = canvas_weak.upgrade() {
            canvas.grab_focus();
        }
    });
}

/// Set `canvas` zoom to `new_zoom`, keeping the cursor point (or the
/// viewport centre when unspecified) anchored. Mirrors the PDF viewer's
/// `apply_zoom_centered`: it records the document-space point under the
/// cursor, applies the zoom, then re-aligns the scroll adjustments on
/// the next idle tick once the new natural size has been measured.
fn apply_zoom_centered(
    canvas: &ImageCanvas,
    scroll: &gtk::ScrolledWindow,
    new_zoom: f64,
    cursor_x: Option<f64>,
    cursor_y: Option<f64>,
) {
    let old_zoom = canvas.zoom();
    if (new_zoom - old_zoom).abs() < 1e-4 {
        return;
    }
    let hadj = scroll.hadjustment();
    let vadj = scroll.vadjustment();
    let cx = cursor_x.unwrap_or(scroll.width() as f64 * 0.5);
    let cy = cursor_y.unwrap_or(scroll.height() as f64 * 0.5);
    let doc_x = (hadj.value() + cx) / old_zoom;
    let doc_y = (vadj.value() + cy) / old_zoom;

    canvas.set_zoom(new_zoom);

    let scroll_weak = scroll.downgrade();
    let canvas_weak = canvas.downgrade();
    glib::idle_add_local_once(move || {
        let (Some(scroll), Some(canvas)) = (scroll_weak.upgrade(), canvas_weak.upgrade()) else {
            return;
        };
        let z = canvas.zoom();
        let hadj = scroll.hadjustment();
        let vadj = scroll.vadjustment();
        let target_h = doc_x * z - cx;
        let target_v = doc_y * z - cy;
        let h = target_h.clamp(
            hadj.lower(),
            (hadj.upper() - hadj.page_size()).max(hadj.lower()),
        );
        let v = target_v.clamp(
            vadj.lower(),
            (vadj.upper() - vadj.page_size()).max(vadj.lower()),
        );
        hadj.set_value(h);
        vadj.set_value(v);
    });
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
            source_instance: String::new(),
            row_menu: lixun_core::RowMenuDef::empty(),
            mime: None,
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
            source_instance: String::new(),
            row_menu: lixun_core::RowMenuDef::empty(),
            mime: None,
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
