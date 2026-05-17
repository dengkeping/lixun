//! Global toggle hotkey listener.
//!
//! Two backends are available:
//!
//! * `kglobalaccel` — talks directly to `org.kde.kglobalaccel`, the same
//!   D-Bus service that native KDE applications (Konsole, Yakuake,
//!   Spectacle, KRunner) use. On KDE/Plasma this service is owned by
//!   KWin and is on the bus before user services start, so registration
//!   is reliable across cold logins. Shortcut storage lives in the same
//!   `~/.config/kglobalshortcutsrc` file used by every other KDE app.
//!
//! * `portal` — the freedesktop `org.freedesktop.portal.GlobalShortcuts`
//!   interface. Required for non-KDE desktops and sandboxed contexts.
//!   See [`portal`] for the full rationale and login-race handling.
//!
//! Backend selection probes the session bus for `org.kde.kglobalaccel`.
//! The systemd user manager does not always have `KDE_FULL_SESSION` /
//! `XDG_CURRENT_DESKTOP` in scope at unit-start time (Plasma imports
//! them later via `systemctl --user import-environment`), so env vars
//! are not authoritative.

use anyhow::Result;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;
use zbus::Connection;
use zbus::fdo::DBusProxy;
use zbus::names::BusName;

mod hyprland_bind;
mod kglobalaccel;
mod portal;

const KGLOBALACCEL_BUS: &str = "org.kde.kglobalaccel";
const KGLOBALACCEL_PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const KGLOBALACCEL_PROBE_INTERVAL: Duration = Duration::from_millis(500);

pub async fn spawn_global_toggle_listener(
    preferred_trigger: String,
    state_dir: PathBuf,
) -> Result<mpsc::Receiver<()>> {
    let (tx, rx) = mpsc::channel(16);
    tokio::spawn(async move {
        if kglobalaccel_available().await {
            tracing::info!("hotkeys: using kglobalaccel backend");
            if let Err(e) = kglobalaccel::run(preferred_trigger, state_dir, tx).await {
                tracing::warn!("hotkeys[kglobalaccel]: listener failed: {:#}", e);
            }
        } else {
            tracing::info!("hotkeys: using xdg-desktop-portal backend");
            if let Err(e) = portal::run(preferred_trigger, state_dir, tx).await {
                tracing::warn!("hotkeys[portal]: listener failed: {:#}", e);
            }
        }
    });
    Ok(rx)
}

async fn kglobalaccel_available() -> bool {
    let conn = match Connection::session().await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("hotkeys: cannot reach session bus: {}", e);
            return false;
        }
    };
    let dbus = match DBusProxy::new(&conn).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("hotkeys: cannot open DBus proxy: {}", e);
            return false;
        }
    };
    let target = match BusName::try_from(KGLOBALACCEL_BUS) {
        Ok(n) => n,
        Err(_) => return false,
    };

    let deadline = tokio::time::Instant::now() + KGLOBALACCEL_PROBE_TIMEOUT;
    loop {
        match dbus.name_has_owner(target.clone()).await {
            Ok(true) => return true,
            Ok(false) => {}
            Err(e) => {
                tracing::warn!("hotkeys: NameHasOwner failed: {}", e);
                return false;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(KGLOBALACCEL_PROBE_INTERVAL).await;
    }
}
