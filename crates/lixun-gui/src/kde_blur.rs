//! KDE Plasma compositor blur for the launcher window.
//!
//! GTK4 has no `backdrop-filter` and Wayland forbids reading the backdrop
//! pixels, so a real blur can only come from the compositor. KDE exposes
//! `org_kde_kwin_blur_manager`; this module binds it from the registry
//! and asks the compositor to blur the area underneath our surface.
//!
//! On compositors without the protocol (sway, niri, GNOME, …) every entry
//! point silently no-ops: the user gets the translucent CSS panel without
//! a backdrop blur, which is the same result as if this module did not
//! exist. Hyprland users get blur via `layerrule = blur, lixun-gui` in
//! their compositor config and don't need this module either.
//!
//! # Surface lifecycle
//!
//! `gtk4-layer-shell` may destroy and recreate the underlying `wl_surface`
//! across hide/show cycles, and the surface is resized by the compositor
//! AFTER it first becomes visible (GTK presents a 1×1 placeholder until
//! the compositor confirms the real geometry). The blur region is bound
//! to surface dimensions, so we re-attach on every `GdkSurface::layout`
//! signal — that fires both on first sizing and on every resize, and
//! gives us the new width/height directly. Hooking only `show` would
//! pin the region to the 1×1 placeholder and KWin would fall back to a
//! rectangular full-surface blur (visible as a halo around the rounded
//! corners). We unset on `hide` so a stale blur object never references
//! a dead surface.
//!
//! # Connection sharing
//!
//! GDK already owns the wayland connection for our process. Opening a
//! second `Connection` on the same fd would race with GDK's event loop
//! and corrupt protocol state. We rely on `gdk4-wayland`'s `wl_display()`
//! / `wl_surface()` accessors, which return proxies bound to a cached
//! `wayland_client::Connection` GDK keeps in qdata — every caller in the
//! process talks to the same backend, so dispatch order stays sane.

use gdk4_wayland::prelude::WaylandSurfaceExtManual;
use gtk::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;
use wayland_client::globals::{GlobalList, GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_region::WlRegion;
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols_plasma::blur::client::org_kde_kwin_blur::OrgKdeKwinBlur;
use wayland_protocols_plasma::blur::client::org_kde_kwin_blur_manager::OrgKdeKwinBlurManager;

/// Border-radius (in CSS pixels) of `.lixun-window` in style.css. Must
/// match so the blur region honours the rounded silhouette and the
/// compositor doesn't blur outside the visible rounded body.
const WINDOW_BORDER_RADIUS: i32 = 14;

/// CSS class added to `.lixun-window` when blur is disabled so the
/// stylesheet can compensate with a heavier background fill. This is
/// the WM-agnostic half of the toggle: on compositors that don't
/// implement `org_kde_kwin_blur_manager` (Hyprland, sway, niri, GNOME)
/// the protocol attach is a no-op, but the class still toggles, so
/// the panel always reflects the user's preference visually.
const NO_BLUR_CLASS: &str = "lixun-no-blur";

/// Runtime controller for KDE compositor blur.
///
/// Owns the window-side state needed to toggle blur on and off after
/// the window has been realised. Created with the initial enabled
/// state from config and driven later by [`Self::set_enabled`] from
/// the style watcher.
///
/// On non-KDE compositors the protocol attach silently no-ops (see
/// the module docstring). The CSS class toggle still runs, so theme
/// authors can style the panel for a no-blur compositor by targeting
/// `.lixun-window.lixun-no-blur`.
pub struct BlurController {
    state: Rc<RefCell<ControllerState>>,
    window: gtk::ApplicationWindow,
}

struct ControllerState {
    enabled: bool,
    attachment: Option<BlurAttachment>,
}

impl BlurController {
    /// Build a controller for `window` and wire up the realise/hide
    /// hooks. The compositor attach happens lazily on the first
    /// `GdkSurface::layout` after realise, gated on the current
    /// `enabled` flag.
    pub fn new(window: &gtk::ApplicationWindow, initial_enabled: bool) -> Self {
        let state = Rc::new(RefCell::new(ControllerState {
            enabled: initial_enabled,
            attachment: None,
        }));

        apply_no_blur_class(window, !initial_enabled);

        let state_layout = Rc::clone(&state);
        window.connect_realize(move |w| {
            let Some(gdk_surface) = w.surface() else {
                return;
            };
            let state_inner = Rc::clone(&state_layout);
            gdk_surface.connect_layout(move |surface, width, height| {
                if width <= 1 || height <= 1 {
                    return;
                }
                let mut st = state_inner.borrow_mut();
                if !st.enabled {
                    return;
                }
                match BlurAttachment::create(surface, width, height) {
                    Ok(Some(att)) => {
                        tracing::debug!("KDE blur enabled: {width}×{height} (layout)");
                        st.attachment = Some(att);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!("KDE blur attach failed: {e:#}");
                    }
                }
            });
        });

        let state_hide = Rc::clone(&state);
        window.connect_hide(move |_| {
            if let Some(att) = state_hide.borrow_mut().attachment.take() {
                att.detach();
            }
        });

        Self {
            state,
            window: window.clone(),
        }
    }

    /// Switch blur on or off at runtime. When turning off, drops the
    /// current compositor attachment (which calls `unset` on the blur
    /// manager) and adds the `lixun-no-blur` CSS class. When turning
    /// on, removes the class and asks GTK to redraw — the next
    /// `GdkSurface::layout` will re-attach the protocol. If the window
    /// has not been realised yet, the toggle simply records the new
    /// flag and the realise path picks it up.
    pub fn set_enabled(&self, enabled: bool) {
        let mut st = self.state.borrow_mut();
        if st.enabled == enabled {
            return;
        }
        st.enabled = enabled;
        if !enabled {
            if let Some(att) = st.attachment.take() {
                att.detach();
            }
        }
        drop(st);

        apply_no_blur_class(&self.window, !enabled);

        if enabled {
            if let Some(surface) = self.window.surface() {
                let (w, h) = (surface.width(), surface.height());
                if w > 1 && h > 1 {
                    match BlurAttachment::create(&surface, w, h) {
                        Ok(Some(att)) => {
                            tracing::debug!("KDE blur re-enabled: {w}×{h}");
                            self.state.borrow_mut().attachment = Some(att);
                        }
                        Ok(None) => {}
                        Err(e) => tracing::warn!("KDE blur re-attach failed: {e:#}"),
                    }
                }
            }
        }
    }
}

fn apply_no_blur_class(window: &gtk::ApplicationWindow, no_blur: bool) {
    if no_blur {
        window.add_css_class(NO_BLUR_CLASS);
    } else {
        window.remove_css_class(NO_BLUR_CLASS);
    }
}

struct BlurAttachment {
    blur: OrgKdeKwinBlur,
    manager: OrgKdeKwinBlurManager,
    surface: WlSurface,
    conn: Connection,
    _queue: EventQueue<RegistryState>,
}

impl BlurAttachment {
    fn create(
        gdk_surface: &gtk::gdk::Surface,
        width: i32,
        height: i32,
    ) -> anyhow::Result<Option<Self>> {
        let scale = gdk_surface.scale_factor();
        tracing::debug!(
            "blur region target: width={width} height={height} scale={scale} radius={WINDOW_BORDER_RADIUS}"
        );

        let display = gdk_surface.display();
        if display.backend() != gtk::gdk::Backend::Wayland {
            return Ok(None);
        }

        let Ok(wayland_display) = display.downcast::<gdk4_wayland::WaylandDisplay>() else {
            return Ok(None);
        };
        let Ok(wayland_surface) = gdk_surface
            .clone()
            .downcast::<gdk4_wayland::WaylandSurface>()
        else {
            return Ok(None);
        };

        // gdk4-wayland's `wl_display()` and `wl_surface()` wrap GDK's
        // underlying pointers in proxies attached to a cached
        // wayland_client::Connection (stored on the GdkDisplay's qdata).
        // Reusing that Connection is mandatory: a second one on the
        // same fd would race with GDK's dispatch loop.
        let Some(wl_display) = wayland_display.wl_display() else {
            return Ok(None);
        };
        let Some(wl_surface) = wayland_surface.wl_surface() else {
            return Ok(None);
        };

        let conn = wl_display
            .backend()
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("wl_display backend went away"))
            .map(Connection::from_backend)?;

        let (globals, queue): (GlobalList, EventQueue<RegistryState>) =
            registry_queue_init::<RegistryState>(&conn)?;
        let qh: QueueHandle<RegistryState> = queue.handle();

        let manager = match globals.bind::<OrgKdeKwinBlurManager, _, _>(&qh, 1..=1, ()) {
            Ok(m) => m,
            Err(_) => return Ok(None),
        };
        let compositor = match globals.bind::<WlCompositor, _, _>(&qh, 4..=6, ()) {
            Ok(c) => c,
            Err(_) => return Ok(None),
        };

        let blur = manager.create(&wl_surface, &qh, ());

        // Blur region follows the rounded silhouette so the compositor
        // doesn't blur pixels outside the visible rounded body (which
        // would produce a rectangular blur halo around the 14px corner
        // radius cutouts).
        let region = compositor.create_region(&qh, ());
        add_rounded_rect(&region, width, height, WINDOW_BORDER_RADIUS);
        blur.set_region(Some(&region));
        region.destroy();

        blur.commit();
        // Blur state is double-buffered: blur.commit() copies pending
        // → current on the compositor side, but the change only takes
        // effect on the next wl_surface.commit. GDK already committed
        // the surface before our show hook ran, so we must commit the
        // surface ourselves — otherwise KWin keeps the previous
        // (infinite) blur region and the user sees a rectangular halo
        // around the rounded corners. See blur-unstable-v1.xml: "The
        // blur region is double-buffered state, and will be applied on
        // the next wl_surface.commit."
        wl_surface.commit();
        // Push everything to the compositor immediately rather than
        // waiting for the next GDK roundtrip.
        let _ = conn.flush();

        Ok(Some(Self {
            blur,
            manager,
            surface: wl_surface,
            conn,
            _queue: queue,
        }))
    }

    fn detach(self) {
        self.manager.unset(&self.surface);
        self.blur.release();
        let _ = self.conn.flush();
    }
}

/// Dispatch sink for the registry init handshake. The blur protocol is
/// fire-and-forget from the client side — none of these interfaces emit
/// events — but `registry_queue_init` and `GlobalList::bind` both
/// require a Dispatch impl to exist on the state type.
struct RegistryState;

impl Dispatch<WlRegistry, GlobalListContents> for RegistryState {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<OrgKdeKwinBlurManager, ()> for RegistryState {
    fn event(
        _: &mut Self,
        _: &OrgKdeKwinBlurManager,
        _: <OrgKdeKwinBlurManager as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<OrgKdeKwinBlur, ()> for RegistryState {
    fn event(
        _: &mut Self,
        _: &OrgKdeKwinBlur,
        _: <OrgKdeKwinBlur as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlCompositor, ()> for RegistryState {
    fn event(
        _: &mut Self,
        _: &WlCompositor,
        _: <WlCompositor as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlRegion, ()> for RegistryState {
    fn event(
        _: &mut Self,
        _: &WlRegion,
        _: <WlRegion as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// Approximate a rounded rectangle as a union of axis-aligned rectangles
/// and add it to `region`. `wl_region` only understands rectangles, so we
/// build the shape scanline-by-scanline: one tall rect covers the inner
/// body (full width, height minus 2×radius), then for each of the top and
/// bottom `radius` pixels we add a horizontally-inset rect whose inset is
/// determined by the circle equation (cutting off the corner).
///
/// `radius` is clamped if it exceeds half the shorter side. On degenerate
/// input the function falls back to a single full-surface rect.
fn add_rounded_rect(region: &WlRegion, width: i32, height: i32, radius: i32) {
    if width <= 0 || height <= 0 {
        tracing::debug!("add_rounded_rect: degenerate input w={width} h={height}, skipping");
        return;
    }
    let r = radius.min(width / 2).min(height / 2);
    if r <= 0 {
        tracing::debug!("add_rounded_rect: radius clamped to 0, fallback to full rect");
        region.add(0, 0, width, height);
        return;
    }

    tracing::debug!("add_rounded_rect: w={width} h={height} r={r} (requested radius={radius})");
    // Inner body rect: full width minus top/bottom rounded bands.
    region.add(0, r, width, height - 2 * r);
    tracing::debug!("  body: x=0 y={r} w={width} h={}", height - 2 * r);

    // Top and bottom bands: for each pixel row within `radius`, add a
    // horizontally-inset rect. Inset = r - sqrt(r^2 - (r - y - 1)^2),
    // rounded up to avoid over-blurring into the transparent corners.
    let mut band_count = 0;
    for y in 0..r {
        let dy = r - y - 1;
        let inset_sq = (r * r - dy * dy).max(0) as f64;
        let chord = inset_sq.sqrt().floor() as i32;
        let inset = r - chord;
        let row_width = width - 2 * inset;
        if row_width <= 0 {
            continue;
        }
        // Top band: rows 0..r.
        region.add(inset, y, row_width, 1);
        // Bottom band: mirror around vertical center.
        region.add(inset, height - 1 - y, row_width, 1);
        band_count += 2;
    }
    tracing::debug!("  bands: {band_count} rects (top+bottom)");
}
