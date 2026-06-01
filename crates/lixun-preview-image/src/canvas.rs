//! `ImageCanvas` — a custom `gtk::Widget` that paints a single decoded
//! raster image with interactive zoom, 90°-step rotation, and a
//! display-only marquee rectangle.
//!
//! Why a custom widget instead of `gtk::Picture`: `Picture` can scale
//! a paintable to fit, but it cannot rotate it, and it does not expose
//! a content-space zoom factor that pan/zoom gestures can drive. This
//! widget owns the decoded `gdk::Texture` and renders it in
//! `WidgetImpl::snapshot` by composing translate → rotate → scale
//! transforms, then overlays the marquee. The widget reports its
//! natural size as the zoomed+rotated texture bounds so that a parent
//! `gtk::ScrolledWindow` supplies panning via its adjustments — the
//! same arrangement the basic viewer already used for `Picture`.
//!
//! Layer order in `snapshot`:
//!   1. the texture (translated to centre, rotated, scaled)
//!   2. the marquee overlay (accent fill + border), when present
//!
//! The marquee is stored in image (texture) space so it stays anchored
//! to the same pixels under zoom and rotation. In this version it is
//! purely a visual selection; copying the region to the clipboard is
//! deliberately out of scope.

use std::cell::{Cell, RefCell};

use gtk::glib;
use gtk::graphene;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

/// Smallest permitted zoom factor.
pub const MIN_ZOOM: f64 = 0.1;
/// Largest permitted zoom factor.
pub const MAX_ZOOM: f64 = 32.0;
/// Multiplicative step for keyboard zoom in/out.
pub const ZOOM_STEP: f64 = 1.25;

/// Increase `zoom` by one step, clamped to [`MAX_ZOOM`].
pub fn zoomed_in(zoom: f64) -> f64 {
    (zoom * ZOOM_STEP).clamp(MIN_ZOOM, MAX_ZOOM)
}

/// Decrease `zoom` by one step, clamped to [`MIN_ZOOM`].
pub fn zoomed_out(zoom: f64) -> f64 {
    (zoom / ZOOM_STEP).clamp(MIN_ZOOM, MAX_ZOOM)
}

mod imp {
    use super::*;

    pub struct ImageCanvas {
        /// The decoded image, or `None` until [`super::ImageCanvas::set_texture`].
        pub texture: RefCell<Option<gtk::gdk::Texture>>,
        /// Intrinsic (unrotated, unzoomed) texture width in pixels.
        pub base_w: Cell<i32>,
        /// Intrinsic (unrotated, unzoomed) texture height in pixels.
        pub base_h: Cell<i32>,
        /// Current zoom factor in `[MIN_ZOOM, MAX_ZOOM]`.
        pub zoom: Cell<f64>,
        /// Rotation in quarter turns clockwise: 0, 1, 2, or 3.
        pub rotation_quarter_turns: Cell<u8>,
        /// Marquee rectangle in image space, or `None` when unset.
        pub marquee: RefCell<Option<graphene::Rect>>,
    }

    impl Default for ImageCanvas {
        fn default() -> Self {
            Self {
                texture: RefCell::new(None),
                base_w: Cell::new(0),
                base_h: Cell::new(0),
                zoom: Cell::new(1.0),
                rotation_quarter_turns: Cell::new(0),
                marquee: RefCell::new(None),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ImageCanvas {
        const NAME: &'static str = "LixunImageCanvas";
        type Type = super::ImageCanvas;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for ImageCanvas {}

    impl WidgetImpl for ImageCanvas {
        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            let (w, h) = self.obj().scaled_size();
            let nat = match orientation {
                gtk::Orientation::Horizontal => w,
                _ => h,
            };
            // min == natural == the zoomed+rotated extent. Reporting a
            // non-zero minimum forces the parent ScrolledWindow to give us
            // our full size (rather than shrinking us to the viewport),
            // which is what produces overflow, scrollbars, and room to pan.
            (nat, nat, -1, -1)
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            self.obj().render(snapshot);
        }
    }
}

glib::wrapper! {
    pub struct ImageCanvas(ObjectSubclass<imp::ImageCanvas>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for ImageCanvas {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl ImageCanvas {
    /// Create an empty canvas. Set a texture with [`Self::set_texture`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Install the decoded texture and remember its intrinsic size.
    pub fn set_texture(&self, texture: &gtk::gdk::Texture) {
        let imp = self.imp();
        imp.base_w.set(texture.width());
        imp.base_h.set(texture.height());
        imp.texture.replace(Some(texture.clone()));
        self.queue_resize();
        self.queue_draw();
    }

    /// Current zoom factor.
    pub fn zoom(&self) -> f64 {
        self.imp().zoom.get()
    }

    /// Set the zoom factor, clamped to `[MIN_ZOOM, MAX_ZOOM]`.
    pub fn set_zoom(&self, zoom: f64) {
        let clamped = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        if (clamped - self.imp().zoom.get()).abs() < f64::EPSILON {
            return;
        }
        self.imp().zoom.set(clamped);
        self.queue_resize();
        self.queue_draw();
    }

    /// Rotate 90° clockwise.
    pub fn rotate_cw(&self) {
        let q = self.imp().rotation_quarter_turns.get();
        self.imp().rotation_quarter_turns.set((q + 1) % 4);
        self.queue_resize();
        self.queue_draw();
    }

    /// Rotate 90° counter-clockwise.
    pub fn rotate_ccw(&self) {
        let q = self.imp().rotation_quarter_turns.get();
        self.imp().rotation_quarter_turns.set((q + 3) % 4);
        self.queue_resize();
        self.queue_draw();
    }

    /// Reset zoom to 1.0, rotation to 0, and clear the marquee.
    pub fn reset(&self) {
        let imp = self.imp();
        imp.zoom.set(1.0);
        imp.rotation_quarter_turns.set(0);
        imp.marquee.replace(None);
        self.queue_resize();
        self.queue_draw();
    }

    /// Store a marquee rectangle in image space.
    pub fn set_marquee(&self, rect: graphene::Rect) {
        self.imp().marquee.replace(Some(rect));
        self.queue_draw();
    }

    /// Clear the marquee, if any.
    pub fn clear_marquee(&self) {
        if self.imp().marquee.borrow().is_some() {
            self.imp().marquee.replace(None);
            self.queue_draw();
        }
    }

    /// Whether a marquee is currently set.
    pub fn has_marquee(&self) -> bool {
        self.imp().marquee.borrow().is_some()
    }

    /// Whether the widget-space point `(wx, wy)` falls inside the current
    /// marquee. Returns `false` when no marquee is set. The point is
    /// mapped into image space so the test honours the current zoom and
    /// rotation, matching where the marquee is painted.
    pub fn marquee_contains_widget_point(&self, wx: f64, wy: f64) -> bool {
        let marquee = self.imp().marquee.borrow();
        let Some(rect) = marquee.as_ref() else {
            return false;
        };
        let (ix, iy) = self.widget_point_to_image(wx, wy);
        rect.contains_point(&graphene::Point::new(ix as f32, iy as f32))
    }

    /// Crop the source texture to the current marquee (image-space) rectangle
    /// and return it as a standalone texture, or `None` when there is no
    /// marquee, no texture, or the selection has zero area. Rotation is not
    /// applied; the crop is taken from the unrotated source pixels.
    pub fn marquee_texture(&self) -> Option<gtk::gdk::Texture> {
        let imp = self.imp();
        let marquee = imp.marquee.borrow();
        let rect = marquee.as_ref()?;
        let texture = imp.texture.borrow();
        let texture = texture.as_ref()?;

        let (bw, bh) = (imp.base_w.get(), imp.base_h.get());
        if bw <= 0 || bh <= 0 {
            return None;
        }

        let mx = (rect.x().round() as i32).clamp(0, bw);
        let my = (rect.y().round() as i32).clamp(0, bh);
        let mw = (rect.width().round() as i32).clamp(0, bw - mx);
        let mh = (rect.height().round() as i32).clamp(0, bh - my);
        if mw <= 0 || mh <= 0 {
            return None;
        }

        let src_stride = bw as usize * 4;
        let mut src = vec![0u8; src_stride * bh as usize];
        texture.download(&mut src, src_stride);

        let dst_stride = mw as usize * 4;
        let mut dst = vec![0u8; dst_stride * mh as usize];
        for row in 0..mh as usize {
            let src_off = (my as usize + row) * src_stride + mx as usize * 4;
            let dst_off = row * dst_stride;
            dst[dst_off..dst_off + dst_stride]
                .copy_from_slice(&src[src_off..src_off + dst_stride]);
        }

        let bytes = gtk::glib::Bytes::from_owned(dst);
        let cropped = gtk::gdk::MemoryTexture::new(
            mw,
            mh,
            gtk::gdk::MemoryFormat::R8g8b8a8,
            &bytes,
            dst_stride,
        );
        Some(cropped.upcast())
    }

    /// `true` when the current rotation swaps width and height (90°/270°).
    fn rotation_is_quarter(&self) -> bool {
        self.imp().rotation_quarter_turns.get() % 2 == 1
    }

    /// The zoomed, rotation-aware widget size in device pixels.
    fn scaled_size(&self) -> (i32, i32) {
        let imp = self.imp();
        let (bw, bh) = (imp.base_w.get(), imp.base_h.get());
        if bw == 0 || bh == 0 {
            return (0, 0);
        }
        let zoom = imp.zoom.get();
        let (w, h) = if self.rotation_is_quarter() {
            (bh, bw)
        } else {
            (bw, bh)
        };
        let sw = (w as f64 * zoom).round().max(1.0) as i32;
        let sh = (h as f64 * zoom).round().max(1.0) as i32;
        (sw, sh)
    }

    /// Convert a primary-button drag (origin `(ox, oy)` plus delta
    /// `(dx, dy)`, both in widget space) into an image-space rectangle
    /// suitable for [`Self::set_marquee`]. Returns `None` before a
    /// texture is set. The inverse of [`Self::image_point_to_widget`]:
    /// it undoes the centring offset, zoom, and rotation so the marquee
    /// stays pinned to the same pixels.
    pub fn widget_drag_to_image_rect(
        &self,
        ox: f64,
        oy: f64,
        dx: f64,
        dy: f64,
    ) -> Option<graphene::Rect> {
        let imp = self.imp();
        let (bw, bh) = (imp.base_w.get(), imp.base_h.get());
        if bw == 0 || bh == 0 {
            return None;
        }
        let p0 = self.widget_point_to_image(ox, oy);
        let p1 = self.widget_point_to_image(ox + dx, oy + dy);
        let x = p0.0.min(p1.0).clamp(0.0, bw as f64);
        let y = p0.1.min(p1.1).clamp(0.0, bh as f64);
        let x_max = p0.0.max(p1.0).clamp(0.0, bw as f64);
        let y_max = p0.1.max(p1.1).clamp(0.0, bh as f64);
        Some(graphene::Rect::new(
            x as f32,
            y as f32,
            (x_max - x) as f32,
            (y_max - y) as f32,
        ))
    }

    /// Inverse of [`Self::image_point_to_widget`]: map a widget-space
    /// point back into image (texture) space.
    fn widget_point_to_image(&self, wx: f64, wy: f64) -> (f64, f64) {
        let imp = self.imp();
        let zoom = imp.zoom.get();
        let (alloc_w, alloc_h) = (self.width() as f64, self.height() as f64);
        let (scaled_w, scaled_h) = {
            let (w, h) = self.scaled_size();
            (w as f64, h as f64)
        };
        let off_x = ((alloc_w - scaled_w) * 0.5).max(0.0);
        let off_y = ((alloc_h - scaled_h) * 0.5).max(0.0);
        let (rx, ry) = ((wx - off_x) / zoom, (wy - off_y) / zoom);
        let (bw, bh) = (imp.base_w.get() as f64, imp.base_h.get() as f64);
        match imp.rotation_quarter_turns.get() {
            1 => (ry, bh - rx),
            2 => (bw - rx, bh - ry),
            3 => (bw - ry, rx),
            _ => (rx, ry),
        }
    }

    /// Map a point given in image (texture) space to widget space,
    /// honouring the current zoom and rotation. Used to project the
    /// stored marquee onto the painted surface.
    fn image_point_to_widget(&self, ix: f64, iy: f64) -> (f64, f64) {
        let imp = self.imp();
        let zoom = imp.zoom.get();
        let (bw, bh) = (imp.base_w.get() as f64, imp.base_h.get() as f64);
        let (rx, ry) = match imp.rotation_quarter_turns.get() {
            1 => (bh - iy, ix),
            2 => (bw - ix, bh - iy),
            3 => (iy, bw - ix),
            _ => (ix, iy),
        };
        (rx * zoom, ry * zoom)
    }

    fn render(&self, snapshot: &gtk::Snapshot) {
        let imp = self.imp();
        let texture = imp.texture.borrow();
        let Some(texture) = texture.as_ref() else {
            return;
        };

        let (bw, bh) = (imp.base_w.get() as f32, imp.base_h.get() as f32);
        if bw == 0.0 || bh == 0.0 {
            return;
        }
        let zoom = imp.zoom.get() as f32;
        let (alloc_w, alloc_h) = (self.width() as f32, self.height() as f32);
        let (scaled_w, scaled_h) = {
            let (w, h) = self.scaled_size();
            (w as f32, h as f32)
        };

        // Centre the painted image inside whatever allocation the
        // ScrolledWindow gave us (it can exceed natural size when the
        // viewport is larger than the image).
        let off_x = ((alloc_w - scaled_w) * 0.5).max(0.0);
        let off_y = ((alloc_h - scaled_h) * 0.5).max(0.0);

        snapshot.save();
        // Transform order is load-bearing: translate to the painted
        // centre, rotate about it, then scale. Any other order rotates
        // or scales about the wrong origin and offsets the image.
        let centre_x = off_x + scaled_w * 0.5;
        let centre_y = off_y + scaled_h * 0.5;
        snapshot.translate(&graphene::Point::new(centre_x, centre_y));
        let degrees = imp.rotation_quarter_turns.get() as f32 * 90.0;
        if degrees != 0.0 {
            snapshot.rotate(degrees);
        }
        snapshot.scale(zoom, zoom);
        // Texture is drawn in its own pixel space, centred on the origin.
        let rect = graphene::Rect::new(-bw * 0.5, -bh * 0.5, bw, bh);
        snapshot.append_texture(texture, &rect);
        snapshot.restore();

        // Marquee overlay, projected from image space to widget space.
        if let Some(m) = imp.marquee.borrow().as_ref() {
            let (x0, y0) = self.image_point_to_widget(m.x() as f64, m.y() as f64);
            let (x1, y1) =
                self.image_point_to_widget((m.x() + m.width()) as f64, (m.y() + m.height()) as f64);
            let rx = x0.min(x1) as f32 + off_x;
            let ry = y0.min(y1) as f32 + off_y;
            let rw = (x1 - x0).abs() as f32;
            let rh = (y1 - y0).abs() as f32;
            if rw >= 1.0 && rh >= 1.0 {
                let fill = gtk::gdk::RGBA::new(0.2, 0.5, 0.9, 0.25);
                let border = gtk::gdk::RGBA::new(0.2, 0.5, 0.9, 0.9);
                snapshot.append_color(&fill, &graphene::Rect::new(rx, ry, rw, rh));
                let t = 1.0_f32;
                snapshot.append_color(&border, &graphene::Rect::new(rx, ry, rw, t));
                snapshot.append_color(&border, &graphene::Rect::new(rx, ry + rh - t, rw, t));
                snapshot.append_color(&border, &graphene::Rect::new(rx, ry, t, rh));
                snapshot.append_color(&border, &graphene::Rect::new(rx + rw - t, ry, t, rh));
            }
        }
    }
}
