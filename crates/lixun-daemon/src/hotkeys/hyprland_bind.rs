//! Auto-register the Hyprland keybind for our portal global shortcut.
//!
//! `xdg-desktop-portal-hyprland` deliberately ignores the `preferred_trigger`
//! field of `BindShortcuts`: it only registers the shortcut *name*
//! (`app.lixun.daemon:toggle`) and expects the user to bind a key combo
//! manually in `hyprland.conf` via the `global` dispatcher, e.g.:
//!
//! ```text
//! bind = SUPER, SPACE, global, app.lixun.daemon:toggle
//! ```
//!
//! On KDE/Plasma `kglobalaccel.setShortcut` actually grabs the key, so the
//! user never has to touch their compositor config. To reach parity on
//! Hyprland we detect the running compositor via `HYPRLAND_INSTANCE_SIGNATURE`
//! and, if no user bind already exists for our shortcut, register the bind
//! ourselves via `hyprctl keyword bind ...`.
//!
//! Failure mode is soft: if `hyprctl` is missing, fails, or a conflicting
//! user bind exists, we emit a single WARN with a manual-config hint and
//! continue. The portal-side registration is already complete by the time
//! this runs; auto-bind is a UX nicety, not a correctness requirement.
//!
//! The bind is session-local (Hyprland forgets `hyprctl keyword` settings
//! on restart), but `lixund.service` runs `After=hyprland-session.target`,
//! so we re-establish it on every daemon start. Users who prefer a
//! different key combo can add their own `bind = ..., global,
//! app.lixun.daemon:toggle` to `hyprland.conf`; our conflict detection
//! will then no-op.

use crate::portal_identity::DAEMON_APP_ID;
use serde::Deserialize;
use tokio::process::Command;

const SHORTCUT_ID: &str = "toggle";
const HYPRCTL: &str = "hyprctl";

/// Hyprland modifier bitmask values, as emitted by `hyprctl binds -j`.
/// Source: Hyprland src/managers/KeybindManager.cpp (modToMask). The
/// values are stable and not exposed via any other API.
const MOD_SHIFT: u32 = 1 << 0;
const MOD_CAPS: u32 = 1 << 1;
const MOD_CTRL: u32 = 1 << 2;
const MOD_ALT: u32 = 1 << 3;
const MOD_MOD2: u32 = 1 << 4;
const MOD_MOD3: u32 = 1 << 5;
const MOD_SUPER: u32 = 1 << 6;
const MOD_MOD5: u32 = 1 << 7;

#[derive(Debug, Deserialize)]
struct HyprBind {
    modmask: u32,
    key: String,
    dispatcher: String,
    arg: String,
}

/// Register the Hyprland keybind for our portal shortcut if running under
/// Hyprland and no conflicting bind exists. Always returns `Ok(())` — see
/// module docstring for the soft-failure policy.
pub(super) async fn try_register(preferred_trigger: &str) {
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
        return;
    }

    let Some((modmask, key)) = parse_trigger_for_hyprland(preferred_trigger) else {
        tracing::warn!(
            "hotkeys[hyprland]: cannot parse trigger '{}' for hyprctl; \
             add `bind = {trigger}, global, {app_id}:{id}` to hyprland.conf manually",
            preferred_trigger,
            trigger = preferred_trigger,
            app_id = DAEMON_APP_ID,
            id = SHORTCUT_ID,
        );
        return;
    };
    let modlist = describe_modmask(modmask);
    let arg = format!("{}:{}", DAEMON_APP_ID, SHORTCUT_ID);

    match read_existing_binds().await {
        Ok(binds) => {
            // Already bound by us or anyone — no-op.
            if binds
                .iter()
                .any(|b| b.dispatcher == "global" && b.arg == arg)
            {
                tracing::debug!(
                    "hotkeys[hyprland]: bind for '{}' already present; not re-registering",
                    arg
                );
                return;
            }
            // Conflicting bind on our preferred key combo — back off.
            if let Some(conflict) = binds
                .iter()
                .find(|b| b.modmask == modmask && b.key.eq_ignore_ascii_case(&key))
            {
                tracing::warn!(
                    "hotkeys[hyprland]: '{}+{}' already bound to {} {}; not overriding. \
                     Add `bind = {}, {}, global, {}` to hyprland.conf to invoke lixun, \
                     or remove the conflicting bind.",
                    modlist,
                    key,
                    conflict.dispatcher,
                    conflict.arg,
                    modlist,
                    key,
                    arg
                );
                return;
            }
        }
        Err(e) => {
            tracing::warn!(
                "hotkeys[hyprland]: cannot read existing binds ({}); will attempt registration anyway",
                e
            );
        }
    }

    // hyprctl keyword bind takes a single comma-separated string:
    //   "MODS, KEY, dispatcher, arg"
    // The leading field is the modifier list (empty when keyless).
    let spec = format!("{}, {}, global, {}", modlist, key, arg);
    match Command::new(HYPRCTL)
        .arg("keyword")
        .arg("bind")
        .arg(&spec)
        .output()
        .await
    {
        Ok(output) if output.status.success() => {
            tracing::info!(
                "hotkeys[hyprland]: registered `bind = {}` via hyprctl",
                spec
            );
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::warn!(
                "hotkeys[hyprland]: `hyprctl keyword bind {}` failed ({}): {}. \
                 Add it to hyprland.conf manually if you want Super+space to work.",
                spec,
                output.status,
                stderr.trim()
            );
        }
        Err(e) => {
            tracing::warn!(
                "hotkeys[hyprland]: cannot exec hyprctl ({}); \
                 add `bind = {}` to hyprland.conf manually",
                e,
                spec
            );
        }
    }
}

async fn read_existing_binds() -> anyhow::Result<Vec<HyprBind>> {
    let output = Command::new(HYPRCTL)
        .arg("binds")
        .arg("-j")
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!(
            "hyprctl binds -j exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let parsed: Vec<HyprBind> = serde_json::from_slice(&output.stdout)?;
    Ok(parsed)
}

/// Translate a lixun trigger string (e.g. `Super+space`) into a
/// `(modmask, key)` pair using Hyprland's bitmask convention.
///
/// Returns `None` when any modifier is unrecognised so we don't silently
/// register a bind that ignores part of the user's intent.
fn parse_trigger_for_hyprland(trigger: &str) -> Option<(u32, String)> {
    let parts: Vec<&str> = trigger.split('+').map(str::trim).filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        return None;
    }
    let (key_part, mod_parts) = parts.split_last()?;
    let mut mask = 0u32;
    for m in mod_parts {
        let bit = modifier_bit(m)?;
        mask |= bit;
    }
    Some((mask, key_part.to_ascii_uppercase()))
}

fn modifier_bit(modifier: &str) -> Option<u32> {
    Some(match modifier.to_ascii_uppercase().as_str() {
        "SHIFT" => MOD_SHIFT,
        "CAPS" | "CAPSLOCK" => MOD_CAPS,
        "CTRL" | "CONTROL" => MOD_CTRL,
        "ALT" | "OPT" | "OPTION" => MOD_ALT,
        "MOD2" | "NUM" | "NUMLOCK" => MOD_MOD2,
        "MOD3" => MOD_MOD3,
        "SUPER" | "LOGO" | "META" | "WIN" | "WINDOWS" | "CMD" | "COMMAND" => MOD_SUPER,
        "MOD5" => MOD_MOD5,
        _ => return None,
    })
}

/// Convert a Hyprland modmask back to the comma-free string Hyprland
/// accepts as the first field of a `bind` spec (e.g. `64` -> `"SUPER"`,
/// `64 | 1` -> `"SUPERSHIFT"`). Order is fixed so two equivalent triggers
/// always produce the same string.
fn describe_modmask(mask: u32) -> String {
    let mut out = String::new();
    for (bit, name) in [
        (MOD_CTRL, "CTRL"),
        (MOD_ALT, "ALT"),
        (MOD_SHIFT, "SHIFT"),
        (MOD_SUPER, "SUPER"),
        (MOD_CAPS, "CAPS"),
        (MOD_MOD2, "MOD2"),
        (MOD_MOD3, "MOD3"),
        (MOD_MOD5, "MOD5"),
    ] {
        if mask & bit != 0 {
            out.push_str(name);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_super_space() {
        let (mask, key) = parse_trigger_for_hyprland("Super+space").expect("parse");
        assert_eq!(mask, MOD_SUPER);
        assert_eq!(key, "SPACE");
    }

    #[test]
    fn parses_meta_as_super() {
        let (mask, _) = parse_trigger_for_hyprland("Meta+space").expect("parse");
        assert_eq!(mask, MOD_SUPER);
        let (mask, _) = parse_trigger_for_hyprland("Win+space").expect("parse");
        assert_eq!(mask, MOD_SUPER);
    }

    #[test]
    fn parses_multi_mod() {
        let (mask, key) = parse_trigger_for_hyprland("CTRL+ALT+Return").expect("parse");
        assert_eq!(mask, MOD_CTRL | MOD_ALT);
        assert_eq!(key, "RETURN");
    }

    #[test]
    fn rejects_unknown_modifier() {
        assert!(parse_trigger_for_hyprland("Hyper+space").is_none());
    }

    #[test]
    fn rejects_empty() {
        assert!(parse_trigger_for_hyprland("").is_none());
        assert!(parse_trigger_for_hyprland("+").is_none());
    }

    #[test]
    fn tolerates_whitespace() {
        let (mask, key) = parse_trigger_for_hyprland(" Super + space ").expect("parse");
        assert_eq!(mask, MOD_SUPER);
        assert_eq!(key, "SPACE");
    }

    #[test]
    fn single_key_no_modifiers() {
        let (mask, key) = parse_trigger_for_hyprland("F12").expect("parse");
        assert_eq!(mask, 0);
        assert_eq!(key, "F12");
    }

    #[test]
    fn describe_super_only() {
        assert_eq!(describe_modmask(MOD_SUPER), "SUPER");
    }

    #[test]
    fn describe_super_shift() {
        assert_eq!(describe_modmask(MOD_SUPER | MOD_SHIFT), "SHIFTSUPER");
    }

    #[test]
    fn describe_empty() {
        assert_eq!(describe_modmask(0), "");
    }

    #[test]
    fn round_trip_super_space() {
        let (mask, key) = parse_trigger_for_hyprland("Super+space").unwrap();
        assert_eq!(describe_modmask(mask), "SUPER");
        assert_eq!(key, "SPACE");
    }
}
