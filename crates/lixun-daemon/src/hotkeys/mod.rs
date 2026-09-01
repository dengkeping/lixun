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
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::mpsc;
use zbus::Connection;
use zbus::fdo::DBusProxy;
use zbus::names::BusName;

use lixun_ipc::HotkeyStatus;

mod hyprland_bind;
mod kglobalaccel;
mod portal;

/// Live listener state exposed via `Response::Status` (O7). Written
/// by the listener task below and by the backends (which flip
/// `bound` once their registration succeeds); read by the daemon's
/// Status handler through [`status_snapshot`]. Previously a portal
/// failure was journal-only — the operator had no way to learn the
/// hotkey silently died.
static HOTKEY_STATUS: OnceLock<Mutex<HotkeyStatus>> = OnceLock::new();

fn status_cell() -> &'static Mutex<HotkeyStatus> {
    HOTKEY_STATUS.get_or_init(|| Mutex::new(HotkeyStatus::default()))
}

/// Mutate the shared hotkey status. Poisoning is impossible in
/// practice (no panics inside the closures), but recover anyway.
pub(crate) fn update_status(f: impl FnOnce(&mut HotkeyStatus)) {
    let mut guard = match status_cell().lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    f(&mut guard);
}

/// Snapshot the current hotkey listener state for Status replies.
pub fn status_snapshot() -> HotkeyStatus {
    match status_cell().lock() {
        Ok(g) => g.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

const KGLOBALACCEL_BUS: &str = "org.kde.kglobalaccel";
const KGLOBALACCEL_PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const KGLOBALACCEL_PROBE_INTERVAL: Duration = Duration::from_millis(500);

pub async fn spawn_global_toggle_listener(
    preferred_trigger: String,
    state_dir: PathBuf,
) -> Result<mpsc::Receiver<()>> {
    let (tx, rx) = mpsc::channel(16);
    update_status(|st| {
        st.trigger = preferred_trigger.clone();
        st.backend = None;
        st.bound = false;
        st.error = None;
    });
    tokio::spawn(async move {
        if kglobalaccel_available().await {
            tracing::info!("hotkeys: using kglobalaccel backend");
            update_status(|st| st.backend = Some("kglobalaccel".into()));
            if let Err(e) = kglobalaccel::run(preferred_trigger, state_dir, tx).await {
                tracing::warn!("hotkeys[kglobalaccel]: listener failed: {:#}", e);
                update_status(|st| {
                    st.bound = false;
                    st.error = Some(format!("{e:#}"));
                });
            }
        } else {
            tracing::info!("hotkeys: using xdg-desktop-portal backend");
            update_status(|st| st.backend = Some("portal".into()));
            if let Err(e) = portal::run(preferred_trigger, state_dir, tx).await {
                tracing::warn!(
                    "hotkeys[portal]: listener failed: {:#}; the global toggle \
                     hotkey is unavailable — bind 'lixun-cli toggle' in your \
                     compositor config instead",
                    e
                );
                update_status(|st| {
                    st.bound = false;
                    st.error = Some(format!(
                        "{e:#}; bind 'lixun-cli toggle' in your compositor config instead"
                    ));
                });
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
