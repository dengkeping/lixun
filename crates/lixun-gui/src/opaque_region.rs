//! Transparent rounded corners for the launcher window.
//!
//! GTK4 automatically calls `wl_surface.set_opaque_region()` to let the
//! compositor skip rendering whatever lies behind fully opaque pixels.
//! It computes that region as a plain rectangle matching the surface
//! buffer and does **not** honour CSS `border-radius`. So the four
//! corners of a rounded window — which GTK leaves transparent — are
//! still advertised as opaque; the compositor happily skips composing
//! the desktop underneath and the user sees whatever was last in the
//! framebuffer at those corner triangles. Most of the time that's a
//! solid black wedge.
//!
//! The fix is to overwrite GTK's opaque region with an **empty**
//! region. Empty means "no pixel in this surface is opaque", which
//! forces the compositor to alpha-blend the whole surface, corners
//! included. We pay a tiny composition cost on the body of the window
//! (fine — it was already semi-transparent for the glass effect) and
//! gain correctly transparent corners on every Wayland compositor.
//!
//! Unlike the KDE blur protocol, `wl_compositor` is a core Wayland
//! interface present everywhere, so this fix is portable.
//!
//! # Surface lifecycle
//!
//! `gtk4-layer-shell` may destroy and recreate the `wl_surface` across
//! hide/show cycles. `set_opaque_region` state is stored on the surface
//! object itself, so a recreated surface loses our override. We
//! re-apply on every `map` signal rather than once at realize time.
//!
//! # Connection sharing
//!
//! Same constraint as the blur module: GDK owns the wayland
//! connection for this process. We reuse it via `gdk4-wayland`'s
//! `wl_display()` / `wl_surface()` accessors instead of opening a
//! second `Connection` on the same fd.

use gdk4_wayland::prelude::WaylandSurfaceExtManual;
use gtk::prelude::*;
use wayland_client::globals::{registry_queue_init, GlobalList, GlobalListContents};
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_region::WlRegion;
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};

/// Advertise an empty opaque region on the window's `wl_surface` so
/// the compositor alpha-blends the full surface, including the rounded
/// corners GTK leaves transparent. Silently no-ops on non-Wayland
/// sessions; logs a warning and keeps going if anything along the
/// wayland-client path fails.
pub fn attach(window: &gtk::ApplicationWindow) {
    window.connect_map(|w| match clear_opaque_region(w) {
        Ok(true) => tracing::debug!("cleared opaque region on launcher surface"),
        Ok(false) => tracing::debug!(
            "opaque region clear skipped (not on Wayland or surface not ready)"
        ),
        Err(e) => tracing::warn!("opaque region clear failed: {e:#}"),
    });
}

fn clear_opaque_region(window: &gtk::ApplicationWindow) -> anyhow::Result<bool> {
    let Some(gdk_surface) = window.surface() else {
        return Ok(false);
    };
    let display = gdk_surface.display();
    if display.backend() != gtk::gdk::Backend::Wayland {
        return Ok(false);
    }

    let Ok(wayland_display) = display.downcast::<gdk4_wayland::WaylandDisplay>() else {
        return Ok(false);
    };
    let Ok(wayland_surface) = gdk_surface.downcast::<gdk4_wayland::WaylandSurface>() else {
        return Ok(false);
    };

    let Some(wl_display) = wayland_display.wl_display() else {
        return Ok(false);
    };
    let Some(wl_surface) = wayland_surface.wl_surface() else {
        return Ok(false);
    };

    // Same GDK-shared Connection pattern as the KDE blur module — a
    // second Connection on the same fd would race with GDK's dispatch
    // loop. See kde_blur.rs for the full rationale.
    let conn = wl_display
        .backend()
        .upgrade()
        .ok_or_else(|| anyhow::anyhow!("wl_display backend went away"))
        .map(Connection::from_backend)?;

    let (globals, queue): (GlobalList, _) = registry_queue_init::<RegistryState>(&conn)?;
    let qh: QueueHandle<RegistryState> = queue.handle();

    // wl_compositor has been stable at version ≥ 4 for years. Accept
    // anything from 4 upward so we keep working as the protocol grows.
    let compositor = globals
        .bind::<WlCompositor, _, _>(&qh, 4..=6, ())
        .map_err(|e| anyhow::anyhow!("bind wl_compositor failed: {e:?}"))?;

    // Empty region = no `add` calls. Compositor interprets this as
    // "zero opaque pixels on the surface" and alpha-blends everything.
    let region = compositor.create_region(&qh, ());
    wl_surface.set_opaque_region(Some(&region));
    wl_surface.commit();
    region.destroy();

    // GDK flushes on its own ticks; push our request out now so the
    // corner fix is visible the instant the window appears rather
    // than after the next GDK roundtrip.
    let _ = conn.flush();

    Ok(true)
}

/// Dispatch sink for the registry init handshake. None of the
/// interfaces we touch emit events, but `registry_queue_init`,
/// `GlobalList::bind`, and each proxy require a `Dispatch` impl.
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
