//! System color-scheme detection via the freedesktop XDG Settings portal.
//!
//! The launcher's embedded stylesheet is authored as a translucent
//! *dark* theme. On a desktop running a light system theme that dark
//! tint mixes against bright wallpaper into mid-gray, and the
//! near-white text plus white specular overlays wash out into
//! unreadability. This module gives the GTK main loop a live signal
//! of the user's preferred color scheme so the window can toggle a
//! `.lixun-light` CSS class on its root, which the stylesheet uses
//! to swap in a light-mode palette and dark-on-light overlays.
//!
//! Detection goes through `org.freedesktop.portal.Settings` (the
//! freedesktop appearance portal) which is the same path libadwaita
//! follows. It works under GNOME, KDE, Hyprland, sway, niri — any
//! compositor that ships a configured xdg-desktop-portal backend.
//! When the portal is absent or returns "no preference" we fall back
//! to dark, preserving the historical behaviour and never failing
//! startup on portal-less setups.
//!
//! ## Threading
//!
//! The GUI runs on a glib main context, not a tokio runtime, so the
//! async zbus API would need its own executor. We sidestep that
//! complexity entirely by using `zbus::blocking` from a dedicated
//! `std::thread`:
//!
//! * [`read_initial`] is a one-shot blocking call from the GUI
//!   thread before the window is shown.
//! * [`spawn_listener`] hands back an `async_channel::Receiver` that
//!   the GUI pump drains via `glib::MainContext::spawn_local`. The
//!   sender half lives on a dedicated thread that owns a separate
//!   bus connection and iterates `SignalIterator` synchronously.
//!
//! Two connections (one blocking-call, one signal-loop) is the
//! conventional shape — sharing a single blocking connection across
//! threads would force serialization between the rare scheme-change
//! event and any future portal usage on the GUI thread.

use std::thread;

use anyhow::{Context, Result};
use async_channel::{Receiver, Sender};
use zbus::blocking;
use zbus::zvariant::{OwnedValue, Value};

const PORTAL_SERVICE: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const SETTINGS_IFACE: &str = "org.freedesktop.portal.Settings";
const APPEARANCE_NS: &str = "org.freedesktop.appearance";
const COLOR_SCHEME_KEY: &str = "color-scheme";

/// Coarse classification of the user's preferred system colour
/// scheme, as exposed by the freedesktop appearance portal.
///
/// `Default` covers both the "no preference" case (portal returned
/// `0`) and the absent-portal case. Either way the launcher renders
/// in its historical dark skin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorScheme {
    /// Portal absent, or `color-scheme = 0` ("no preference").
    Default,
    /// Portal reported `color-scheme = 1`.
    PreferDark,
    /// Portal reported `color-scheme = 2`.
    PreferLight,
}

impl ColorScheme {
    /// Decode the integer encoding used by
    /// `org.freedesktop.appearance.color-scheme`:
    ///
    /// * `0` → [`ColorScheme::Default`]
    /// * `1` → [`ColorScheme::PreferDark`]
    /// * `2` → [`ColorScheme::PreferLight`]
    /// * any other value falls back to [`ColorScheme::Default`]
    fn from_portal_code(code: u32) -> Self {
        match code {
            1 => Self::PreferDark,
            2 => Self::PreferLight,
            _ => Self::Default,
        }
    }

    /// True iff the launcher should swap to its `.lixun-light` skin.
    /// Both `Default` and `PreferDark` keep the legacy dark skin so
    /// users without a configured portal never see a behaviour
    /// regression.
    pub fn is_light(self) -> bool {
        matches!(self, Self::PreferLight)
    }
}

/// Run a single blocking `Settings.Read` call to discover the user's
/// current preference. Failure for any reason (portal not running,
/// key never set, malformed reply) yields [`ColorScheme::Default`]
/// because portal absence must never block launcher startup.
pub fn read_initial() -> ColorScheme {
    match read_initial_inner() {
        Ok(scheme) => {
            tracing::debug!(?scheme, "initial color-scheme from XDG portal");
            scheme
        }
        Err(e) => {
            tracing::debug!(
                "XDG Settings portal unavailable for color-scheme: {e:#}; \
                 staying on dark skin"
            );
            ColorScheme::Default
        }
    }
}

fn read_initial_inner() -> Result<ColorScheme> {
    let conn = blocking::Connection::session().context("connect to session bus")?;
    let proxy = blocking::Proxy::new(&conn, PORTAL_SERVICE, PORTAL_PATH, SETTINGS_IFACE)
        .context("open Settings portal proxy")?;
    let reply: OwnedValue = proxy
        .call("Read", &(APPEARANCE_NS, COLOR_SCHEME_KEY))
        .context("call Settings.Read(appearance, color-scheme)")?;
    extract_color_scheme(reply.into())
}

/// Spawn the listener thread and return the receiving half of the
/// channel to be drained by `glib::MainContext::spawn_local` on the
/// GUI thread. The thread terminates when the receiver is dropped.
pub fn spawn_listener() -> Receiver<ColorScheme> {
    let (tx, rx) = async_channel::unbounded();
    thread::Builder::new()
        .name("lixun-color-scheme".to_string())
        .spawn(move || {
            if let Err(e) = listen_loop(&tx) {
                tracing::debug!("color-scheme listener exited: {e:#}");
            }
        })
        .expect("spawn color-scheme listener thread");
    rx
}

fn listen_loop(tx: &Sender<ColorScheme>) -> Result<()> {
    let conn =
        blocking::Connection::session().context("connect to session bus for listener")?;
    let proxy = blocking::Proxy::new(&conn, PORTAL_SERVICE, PORTAL_PATH, SETTINGS_IFACE)
        .context("open Settings portal proxy for listener")?;
    let iter = proxy
        .receive_signal("SettingChanged")
        .context("subscribe to Settings.SettingChanged")?;
    for msg in iter {
        match decode_setting_changed(&msg) {
            Ok(Some(scheme)) => {
                tracing::debug!(?scheme, "color-scheme changed via portal");
                if tx.send_blocking(scheme).is_err() {
                    // Receiver dropped → GUI is shutting down.
                    return Ok(());
                }
            }
            Ok(None) => {} // unrelated namespace/key
            Err(e) => tracing::debug!("ignoring malformed SettingChanged: {e:#}"),
        }
    }
    Ok(())
}

fn decode_setting_changed(msg: &zbus::Message) -> Result<Option<ColorScheme>> {
    let body = msg.body();
    // Signature: (s namespace, s key, v value)
    let (namespace, key, value): (String, String, OwnedValue) = body
        .deserialize()
        .context("deserialize SettingChanged body")?;
    if namespace != APPEARANCE_NS || key != COLOR_SCHEME_KEY {
        return Ok(None);
    }
    extract_color_scheme(value.into()).map(Some)
}

/// Walk through any chain of nested `Value::Value` wrappers to find
/// the integer payload. The portal's wire shape is `v` containing
/// `v` containing `u`; zbus may unwrap one of the layers depending on
/// the deserialization path, so we unwrap iteratively up to a small
/// bound. Beyond that we treat it as malformed and let the caller
/// fall back to the default.
fn extract_color_scheme(reply: Value<'_>) -> Result<ColorScheme> {
    let mut current = reply;
    for _ in 0..4 {
        match current {
            Value::U32(n) => return Ok(ColorScheme::from_portal_code(n)),
            Value::U16(n) => return Ok(ColorScheme::from_portal_code(u32::from(n))),
            Value::U8(n) => return Ok(ColorScheme::from_portal_code(u32::from(n))),
            Value::I32(n) if n >= 0 => return Ok(ColorScheme::from_portal_code(n as u32)),
            Value::Value(inner) => current = *inner,
            other => anyhow::bail!(
                "color-scheme variant has unexpected type: {:?}",
                other
            ),
        }
    }
    anyhow::bail!("color-scheme variant nested deeper than expected")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portal_code_mapping() {
        assert_eq!(ColorScheme::from_portal_code(0), ColorScheme::Default);
        assert_eq!(ColorScheme::from_portal_code(1), ColorScheme::PreferDark);
        assert_eq!(ColorScheme::from_portal_code(2), ColorScheme::PreferLight);
        // Unknown values fall back to default rather than misclassifying.
        assert_eq!(ColorScheme::from_portal_code(7), ColorScheme::Default);
        assert_eq!(ColorScheme::from_portal_code(u32::MAX), ColorScheme::Default);
    }

    #[test]
    fn is_light_only_when_explicit() {
        assert!(!ColorScheme::Default.is_light());
        assert!(!ColorScheme::PreferDark.is_light());
        assert!(ColorScheme::PreferLight.is_light());
    }

    #[test]
    fn extract_handles_bare_u32() {
        let v = Value::U32(2);
        assert_eq!(extract_color_scheme(v).unwrap(), ColorScheme::PreferLight);
    }

    #[test]
    fn extract_handles_single_variant_wrap() {
        let inner = Value::U32(1);
        let v = Value::Value(Box::new(inner));
        assert_eq!(extract_color_scheme(v).unwrap(), ColorScheme::PreferDark);
    }

    #[test]
    fn extract_handles_double_variant_wrap() {
        let inner = Value::U32(2);
        let v = Value::Value(Box::new(Value::Value(Box::new(inner))));
        assert_eq!(extract_color_scheme(v).unwrap(), ColorScheme::PreferLight);
    }

    #[test]
    fn extract_rejects_non_integer() {
        let v = Value::Str("dark".into());
        assert!(extract_color_scheme(v).is_err());
    }

    #[test]
    fn extract_rejects_pathologically_nested_variants() {
        let mut v = Value::U32(1);
        for _ in 0..10 {
            v = Value::Value(Box::new(v));
        }
        assert!(extract_color_scheme(v).is_err());
    }
}
