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
//! across hide/show cycles. The blur object is bound to a specific
//! `wl_surface`, so we re-attach on every `map` signal rather than once
//! at realize time. We unset on `unmap` so a stale blur object never
//! references a dead surface.
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
use wayland_client::globals::{registry_queue_init, GlobalList, GlobalListContents};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols_plasma::blur::client::org_kde_kwin_blur::OrgKdeKwinBlur;
use wayland_protocols_plasma::blur::client::org_kde_kwin_blur_manager::OrgKdeKwinBlurManager;

/// Wire compositor blur to a window. Returns silently on non-Wayland
/// or non-KDE sessions; the caller never needs to branch on session type.
pub fn attach(window: &gtk::ApplicationWindow) {
    let state: Rc<RefCell<Option<BlurAttachment>>> = Rc::new(RefCell::new(None));

    let state_map = Rc::clone(&state);
    window.connect_map(move |w| match BlurAttachment::create(w) {
        Ok(Some(att)) => {
            tracing::debug!("KDE blur enabled on launcher surface");
            *state_map.borrow_mut() = Some(att);
        }
        Ok(None) => {
            tracing::debug!(
                "KDE blur unavailable (not on Wayland, or compositor lacks org_kde_kwin_blur_manager)"
            );
        }
        Err(e) => {
            tracing::warn!("KDE blur attach failed: {e:#}");
        }
    });

    let state_unmap = Rc::clone(&state);
    window.connect_unmap(move |_| {
        if let Some(att) = state_unmap.borrow_mut().take() {
            att.detach();
        }
    });
}

struct BlurAttachment {
    blur: OrgKdeKwinBlur,
    manager: OrgKdeKwinBlurManager,
    surface: WlSurface,
    conn: Connection,
    _queue: EventQueue<RegistryState>,
}

impl BlurAttachment {
    fn create(window: &gtk::ApplicationWindow) -> anyhow::Result<Option<Self>> {
        let Some(gdk_surface) = window.surface() else {
            return Ok(None);
        };
        let display = gdk_surface.display();
        if display.backend() != gtk::gdk::Backend::Wayland {
            return Ok(None);
        }

        let Ok(wayland_display) = display.downcast::<gdk4_wayland::WaylandDisplay>() else {
            return Ok(None);
        };
        let Ok(wayland_surface) = gdk_surface.downcast::<gdk4_wayland::WaylandSurface>() else {
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

        let blur = manager.create(&wl_surface, &qh, ());
        blur.set_region(None);
        blur.commit();
        // The Connection we hold is a clone of GDK's; GDK flushes on
        // its own ticks, but we want the request out immediately rather
        // than at the next GDK roundtrip, so push it ourselves.
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
