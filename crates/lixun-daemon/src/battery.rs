//! Battery follow watcher — hot-reloads the live [`ImpactProfile`]
//! when the system transitions between AC and battery power.
//!
//! ## D-Bus backend choice
//!
//! We talk to UPower directly through `zbus = 5` rather than using the
//! `upower_dbus` convenience crate. Reason: at the time of writing
//! `upower_dbus = "0.3.2"` (the latest published version) pulls in
//! `zbus = "3.15"`, which would conflict with the rest of the daemon
//! (hotkeys, portal_identity) that already depend on `zbus = 5`.
//! The UPower surface we need is tiny — one boolean property and one
//! `PropertiesChanged` signal — so a hand-rolled `zbus::Proxy` is
//! cheaper than dragging in a second incompatible D-Bus stack.
//!
//! ## Plugin neutrality
//!
//! Per `AGENTS.md` §1, this module knows only about the abstract
//! [`SystemImpact`] enum, the [`ImpactProfile`] swap target, and a
//! caller-supplied hot-apply callback. It does NOT know which sources
//! exist, what their tunables are, or how the daemon dispatches
//! impact changes — that is encapsulated inside `hot_apply`, which
//! is the same code path the IPC `Request::ImpactSet` handler runs.
//!
//! ## Failure mode
//!
//! If UPower is not running or the D-Bus session lacks access to the
//! system bus, the watcher logs one warning and returns `Ok(())` so
//! the daemon stays up with whatever impact level config selected at
//! startup. Battery follow is a comfort feature, not a correctness
//! guarantee.

use std::sync::Arc;

use anyhow::Result;
use arc_swap::ArcSwap;
use futures::StreamExt;
use lixun_core::{ImpactProfile, SystemImpact};
use zbus::Connection;
use zbus::zvariant::OwnedValue;

const UPOWER_SERVICE: &str = "org.freedesktop.UPower";
const UPOWER_PATH: &str = "/org/freedesktop/UPower";
const UPOWER_IFACE: &str = "org.freedesktop.UPower";
const PROPERTIES_IFACE: &str = "org.freedesktop.DBus.Properties";

/// Callback the watcher invokes whenever the AC/battery state flips.
/// Implementations MUST be the same hot-reload path used by
/// [`Request::ImpactSet`](lixun_ipc::Request::ImpactSet) so that
/// transitions stay observable through the same channels (logs,
/// `profile_swap`, `sched::apply_nice_only`, …) regardless of
/// whether the change originated from a user CLI command or the
/// kernel telling us the laptop got unplugged.
pub type HotApplyFn = Arc<dyn Fn(SystemImpact) + Send + Sync>;

/// Subscribe to UPower's `OnBattery` property and apply the matching
/// [`SystemImpact`] level whenever it flips.
///
/// On entry, reads the current `OnBattery` value once and applies
/// the corresponding impact (so the daemon converges to the right
/// profile even if it started under the wrong assumption). Then
/// loops on `PropertiesChanged` signals, no polling.
///
/// `impact` and `num_cpus` are accepted for future extensibility
/// (e.g. a per-transition diff log) but the actual hot-apply is
/// delegated to `hot_apply` to keep the IPC and watcher paths in
/// lockstep.
pub async fn watch_battery(
    impact: Arc<ArcSwap<ImpactProfile>>,
    on_ac: SystemImpact,
    on_battery: SystemImpact,
    num_cpus: usize,
    hot_apply: HotApplyFn,
) -> Result<()> {
    let _ = (impact, num_cpus); // reserved for future per-transition diagnostics

    let conn = match Connection::system().await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("upower unavailable: {e}; battery follow disabled");
            return Ok(());
        }
    };

    let proxy = match zbus::Proxy::new(&conn, UPOWER_SERVICE, UPOWER_PATH, PROPERTIES_IFACE).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("upower unavailable: {e}; battery follow disabled");
            return Ok(());
        }
    };

    // Initial state: pick the right profile up-front.
    let on_battery_now = match read_on_battery(&proxy).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("upower unavailable: {e}; battery follow disabled");
            return Ok(());
        }
    };

    let initial_level = if on_battery_now { on_battery } else { on_ac };
    if on_battery_now {
        tracing::info!(
            "battery watcher: on_battery initial state={:?}",
            initial_level
        );
    } else {
        tracing::info!("battery watcher: on_ac initial state={:?}", initial_level);
    }
    hot_apply(initial_level);

    // Subscribe to PropertiesChanged on the UPower interface.
    let mut signal_stream = match proxy.receive_signal("PropertiesChanged").await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                "upower PropertiesChanged subscribe failed: {e}; battery follow disabled"
            );
            return Ok(());
        }
    };

    let mut last_on_battery = on_battery_now;
    while let Some(msg) = signal_stream.next().await {
        let body = msg.body();
        let Ok((iface, changed, _invalidated)) = body.deserialize::<(
            String,
            std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
            Vec<String>,
        )>() else {
            continue;
        };
        if iface != UPOWER_IFACE {
            continue;
        }
        let Some(val) = changed.get("OnBattery") else {
            // OnBattery wasn't part of this PropertiesChanged batch.
            continue;
        };
        let Ok(now) = <bool as TryFrom<&OwnedValue>>::try_from(val) else {
            continue;
        };
        if now == last_on_battery {
            continue;
        }
        last_on_battery = now;
        if now {
            tracing::info!(
                "battery transition: ac -> battery; applying impact={:?} (hot)",
                on_battery
            );
            hot_apply(on_battery);
        } else {
            tracing::info!(
                "battery transition: battery -> ac; applying impact={:?} (hot)",
                on_ac
            );
            hot_apply(on_ac);
        }
    }

    Ok(())
}

async fn read_on_battery(proxy: &zbus::Proxy<'_>) -> Result<bool> {
    let value: OwnedValue = proxy.call("Get", &(UPOWER_IFACE, "OnBattery")).await?;
    let b = <bool as TryFrom<&OwnedValue>>::try_from(&value)?;
    Ok(b)
}
