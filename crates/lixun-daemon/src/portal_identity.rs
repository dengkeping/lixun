//! Register with `org.freedesktop.host.portal.Registry` so the portal
//! backend knows which `.desktop` file describes this process.
//!
//! Without a registration call, xdg-desktop-portal sends an empty
//! `app_id` string to backends (e.g. xdg-desktop-portal-kde). KDE
//! then displays the global-shortcut entry as `token_lixun_<random>`
//! and cannot resolve a friendly name or icon. Flatpak and Snap apps
//! get implicit app ids via their sandbox; host (unsandboxed) apps
//! must self-register.
//!
//! The `Registry.Register` method takes an app id that matches the
//! basename of an installed `.desktop` file. The backend looks up
//! `${app_id}.desktop` in XDG applications directories and reads
//! `Name=` and `Icon=` from it.
//!
//! Register can be called at most once per D-Bus connection, and must
//! happen before any portal method call. When the portal service goes
//! away and comes back, the registration is lost — callers should
//! listen for NameOwnerChanged on `org.freedesktop.portal.Desktop`
//! and re-register.
//!
//! References:
//!   - xdg-desktop-portal/src/registry.c
//!   - xdg-desktop-portal/data/org.freedesktop.host.portal.Registry.xml
//!   - Available since xdg-desktop-portal 1.18.

use anyhow::{Context, Result};
use futures::StreamExt;
use std::collections::HashMap;
use zbus::Connection;
use zbus::zvariant::Value;

/// D-Bus app id for the daemon. Must match the basename of the
/// installed desktop file (`app.lixun.daemon.desktop`) so the portal
/// backend can read `Name=` and `Icon=` from it.
pub const DAEMON_APP_ID: &str = "app.lixun.daemon";

const PORTAL_SERVICE: &str = "org.freedesktop.portal.Desktop";
const REGISTRY_PATH: &str = "/org/freedesktop/host/portal/Registry";
const REGISTRY_IFACE: &str = "org.freedesktop.host.portal.Registry";

/// Register this connection with the portal Registry. Idempotent from
/// the caller's perspective: a second Register call on the same
/// connection returns an error, which we downgrade to a debug log so
/// callers can retry safely on portal restart without special-casing
/// the first call.
pub async fn register(conn: &Connection, app_id: &str) -> Result<()> {
    let proxy = zbus::Proxy::new(conn, PORTAL_SERVICE, REGISTRY_PATH, REGISTRY_IFACE)
        .await
        .context("creating Registry proxy")?;
    let options: HashMap<&str, Value<'_>> = HashMap::new();
    match proxy.call::<_, _, ()>("Register", &(app_id, options)).await {
        Ok(()) => {
            tracing::info!("portal_identity: registered as '{}'", app_id);
            Ok(())
        }
        Err(e) => {
            // Registry is optional — xdg-desktop-portal < 1.18 does not
            // implement it. KDE falls back to the token-derived name in
            // that case; not a fatal error.
            tracing::warn!(
                "portal_identity: Register('{}') failed: {} \
                 (falls back to token_* in KDE shortcuts)",
                app_id,
                e
            );
            Ok(())
        }
    }
}

/// Spawn a background task that re-registers whenever the portal
/// service (re)appears on the bus. Uses zbus' NameOwnerChanged signal
/// on `org.freedesktop.DBus`. The task holds a clone of the
/// connection; if the connection dies, the task exits.
pub async fn spawn_reregister_watcher(conn: Connection, app_id: String) -> Result<()> {
    let proxy = zbus::Proxy::new(
        &conn,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .await
    .context("creating DBus proxy for NameOwnerChanged")?;
    let mut stream = proxy
        .receive_signal("NameOwnerChanged")
        .await
        .context("subscribing to NameOwnerChanged")?;

    tokio::spawn(async move {
        while let Some(msg) = stream.next().await {
            let body: Result<(String, String, String), _> = msg.body().deserialize();
            let Ok((name, _old_owner, new_owner)) = body else {
                continue;
            };
            if name != PORTAL_SERVICE {
                continue;
            }
            if new_owner.is_empty() {
                // Portal went away; nothing to do until it returns.
                tracing::debug!(
                    "portal_identity: {} disappeared from the bus",
                    PORTAL_SERVICE
                );
                continue;
            }
            tracing::info!(
                "portal_identity: {} reappeared, re-registering as '{}'",
                PORTAL_SERVICE,
                app_id
            );
            if let Err(e) = register(&conn, &app_id).await {
                tracing::warn!("portal_identity: re-register failed: {}", e);
            }
        }
        tracing::debug!("portal_identity: NameOwnerChanged stream ended");
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_are_nonempty() {
        assert!(!PORTAL_SERVICE.is_empty());
        assert!(!REGISTRY_PATH.is_empty());
        assert!(!REGISTRY_IFACE.is_empty());
    }

    #[test]
    fn registry_iface_matches_upstream_spec() {
        assert_eq!(REGISTRY_IFACE, "org.freedesktop.host.portal.Registry");
        assert_eq!(REGISTRY_PATH, "/org/freedesktop/host/portal/Registry");
    }
}
