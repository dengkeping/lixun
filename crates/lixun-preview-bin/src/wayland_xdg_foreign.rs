//! xdg-foreign-v2 importer for the preview window.
//!
//! Phase 1.2 of the preview-rich-quicklook plan. The launcher
//! (`lixun-gui`) exports its toplevel `wl_surface` via
//! `zxdg_exporter_v2` and ferries the resulting opaque handle string
//! to this process through `PreviewCommand::SetParent`. This module
//! takes that handle, calls `zxdg_importer_v2.import_toplevel(handle)`
//! to obtain a `zxdg_imported_v2` proxy, and then
//! `set_parent_of(preview_surface)` to install the parent-child
//! relationship. The compositor uses that relationship to keep the
//! preview stacked above the launcher and to follow the launcher
//! across workspace switches — the exact ordering guarantee the
//! previous gtk4-layer-shell approach gave us, restored on regular
//! xdg-toplevel windows.
//!
//! Connection sharing: we reuse GDK's already-established Wayland
//! `Connection` (via `gdk_surface.display().wl_display().backend()`).
//! Opening a second `wl_display` connection to the same compositor
//! would race with GDK's dispatch loop. Our `EventQueue` is private
//! though — `registry_queue_init` returns a fresh queue distinct from
//! GDK's, so events on the proxies we create accumulate here until we
//! dispatch them.
//!
//! V1 limitation: we do not pump the private queue. The
//! `zxdg_imported_v2.destroyed` event (compositor revokes the import,
//! e.g. launcher quits before the preview) is therefore not surfaced
//! as `PreviewEvent::ParentLost` in this revision — the imported
//! object remains valid client-side until we drop it. Compositor
//! revoke is rare in practice (KWin/Hyprland/sway/Niri keep imports
//! alive until the exporter destroys its side, which already triggers
//! a server-side revoke we observe by losing window focus). Wiring a
//! glib heartbeat to `dispatch_pending` is straightforward when this
//! gap matters; tracked alongside Phase 1.6 acceptance.
//!
//! Fallback: if the compositor does not advertise `zxdg_importer_v2`
//! in its global registry, [`WaylandImporter::new`] returns
//! `Ok(None)` and the caller logs a single warning. The preview then
//! shows as a plain centred xdg-toplevel — usable, just without the
//! parent-of relationship.

use anyhow::{Context, Result};
use gdk4_wayland::prelude::WaylandSurfaceExtManual;
use gtk::prelude::*;
use wayland_client::globals::{registry_queue_init, GlobalList, GlobalListContents};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::xdg::foreign::zv2::client::zxdg_imported_v2::ZxdgImportedV2;
use wayland_protocols::xdg::foreign::zv2::client::zxdg_importer_v2::ZxdgImporterV2;

/// Sink type for events on objects we create. Empty — see V1
/// limitation in the module docstring.
struct RegistryState;

impl Dispatch<WlRegistry, GlobalListContents> for RegistryState {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as wayland_client::Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZxdgImporterV2, ()> for RegistryState {
    fn event(
        _: &mut Self,
        _: &ZxdgImporterV2,
        _: <ZxdgImporterV2 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZxdgImportedV2, ()> for RegistryState {
    fn event(
        _: &mut Self,
        _: &ZxdgImportedV2,
        _: <ZxdgImportedV2 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // `destroyed` lands here when pumped. V1 swallows it; see
        // module docstring for the deferred ParentLost emission.
    }
}

/// Holds the long-lived proxies and connection bits needed to keep
/// an xdg-foreign-v2 import alive. Dropping this destroys the
/// imported handle on the server (via the proxy's Drop impl).
pub struct WaylandImporter {
    conn: Connection,
    /// Must outlive the proxies — wayland-rs ties proxy lifetime to
    /// the queue they were registered on.
    _queue: EventQueue<RegistryState>,
    qh: QueueHandle<RegistryState>,
    importer: ZxdgImporterV2,
    imported: Option<ZxdgImportedV2>,
}

impl WaylandImporter {
    /// Build an importer rooted on the GDK Wayland display backing
    /// `gdk_surface`. Returns `Ok(None)` when the surface is not
    /// Wayland-backed or when the compositor does not advertise
    /// `zxdg_importer_v2`.
    pub fn new(gdk_surface: &gtk::gdk::Surface) -> Result<Option<Self>> {
        let display = gdk_surface.display();
        if display.backend() != gtk::gdk::Backend::Wayland {
            return Ok(None);
        }
        let Ok(wl_display) = display.downcast::<gdk4_wayland::WaylandDisplay>() else {
            return Ok(None);
        };
        let Some(raw_display) = wl_display.wl_display() else {
            return Ok(None);
        };
        let Some(backend) = raw_display.backend().upgrade() else {
            return Ok(None);
        };
        let conn = Connection::from_backend(backend);
        let (globals, queue): (GlobalList, EventQueue<RegistryState>) =
            registry_queue_init::<RegistryState>(&conn)
                .context("xdg-foreign: registry_queue_init")?;
        let qh = queue.handle();
        let importer = match globals.bind::<ZxdgImporterV2, _, _>(&qh, 1..=1, ()) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    "preview: compositor does not advertise zxdg_importer_v2 ({e}); \
                     preview will show without xdg-foreign parenting"
                );
                return Ok(None);
            }
        };
        Ok(Some(Self {
            conn,
            _queue: queue,
            qh,
            importer,
            imported: None,
        }))
    }

    /// Import `handle` (an opaque string the launcher exported via
    /// `zxdg_exporter_v2`) and set the resulting parent on
    /// `parent_target`, the preview window's own `wl_surface`. Drops
    /// any previous import first (server-side destroy via Drop).
    pub fn import(&mut self, handle: &str, parent_target: &WlSurface) -> Result<()> {
        // Drop the previous import (if any) before issuing a new
        // one. Sending `set_parent_of` again on the same imported
        // proxy is allowed by the protocol, but if the launcher
        // exported a fresh handle we must follow that one.
        self.imported = None;
        let imported = self
            .importer
            .import_toplevel(handle.to_string(), &self.qh, ());
        imported.set_parent_of(parent_target);
        self.imported = Some(imported);
        self.conn
            .flush()
            .context("xdg-foreign: flush after import")?;
        Ok(())
    }

    /// Drop the active import (if any) and flush. Idempotent.
    pub fn clear(&mut self) {
        if self.imported.take().is_some() {
            let _ = self.conn.flush();
        }
    }
}

/// Pull the raw `wl_surface` out of a GDK surface, if Wayland-backed.
pub fn wl_surface_of(gdk_surface: &gtk::gdk::Surface) -> Option<WlSurface> {
    let wayland_surface = gdk_surface
        .clone()
        .downcast::<gdk4_wayland::WaylandSurface>()
        .ok()?;
    wayland_surface.wl_surface()
}
