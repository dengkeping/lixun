//! Discover user-session environment variables needed by spawned GUI processes.
//!
//! When `lixund` is started by systemd user-manager that did not inherit
//! `WAYLAND_DISPLAY`, `DBUS_SESSION_BUS_ADDRESS`, `DISPLAY`, and
//! `XDG_RUNTIME_DIR` from the graphical session (e.g. after a Plasma/KWin
//! crash-and-restart where `systemctl --user import-environment` did not
//! rerun), a naïvely spawned `lixun-gui` child inherits that empty env and
//! cannot connect to the Wayland compositor or session bus. GTK then either
//! panics or silently exits, and `lixun show` appears to do nothing.
//!
//! This module rediscovers the session env from filesystem artifacts that
//! the compositor, X server, and dbus-daemon leave behind under
//! `/run/user/$UID` and `/tmp/.X11-unix`. It is intentionally DE-agnostic:
//! it does not ask `plasmashell`, `kwin`, or `systemctl --user` for help.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Keys we propagate into `lixun-gui`'s environment. Listed explicitly so the
/// set is reviewable and we never accidentally leak the daemon's full env.
const GUI_ENV_KEYS: &[&str] = &[
    "WAYLAND_DISPLAY",
    "DISPLAY",
    "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS",
    "XDG_CURRENT_DESKTOP",
    "XDG_SESSION_TYPE",
];

/// Collect the best available value for each GUI env var, preferring the
/// daemon's own inherited value and falling back to filesystem discovery
/// under `/run/user/$uid` and `/tmp/.X11-unix`.
///
/// Returns a map containing only keys for which a value was found. Missing
/// keys are left unset on the child, matching the behaviour before this
/// helper was introduced (so this never *loses* an env var that was there).
pub fn discover_gui_env() -> HashMap<String, String> {
    let uid = unsafe { libc::getuid() };
    let runtime_dir = PathBuf::from(format!("/run/user/{}", uid));
    let inherited: HashMap<String, String> = GUI_ENV_KEYS
        .iter()
        .filter_map(|k| std::env::var(*k).ok().map(|v| ((*k).to_string(), v)))
        .filter(|(_, v)| !v.is_empty())
        .collect();
    discover_gui_env_at(&runtime_dir, Path::new("/tmp/.X11-unix"), &inherited)
}

/// Same as [`discover_gui_env`] but with explicit search paths and an
/// explicitly-injected `inherited` env map (instead of reading the process
/// env). Used by tests to avoid global env races.
pub fn discover_gui_env_at(
    runtime_dir: &Path,
    x11_dir: &Path,
    inherited: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut env: HashMap<String, String> = HashMap::new();

    // 1. Prefer the caller-provided (usually inherited process) environment.
    //    If systemd-import-environment ran, these are already correct and we
    //    propagate them verbatim.
    for key in GUI_ENV_KEYS {
        if let Some(v) = inherited.get(*key)
            && !v.is_empty()
        {
            env.insert((*key).to_string(), v.clone());
        }
    }

    // 2. XDG_RUNTIME_DIR: point at /run/user/$uid if it exists.
    if !env.contains_key("XDG_RUNTIME_DIR") && runtime_dir.is_dir() {
        env.insert(
            "XDG_RUNTIME_DIR".to_string(),
            runtime_dir.to_string_lossy().into_owned(),
        );
    }

    // 3. WAYLAND_DISPLAY: first `wayland-N` socket in runtime_dir that has a
    //    matching `.lock` file (compositor still alive).
    if !env.contains_key("WAYLAND_DISPLAY")
        && let Some(display) = discover_wayland_display(runtime_dir)
    {
        env.insert("WAYLAND_DISPLAY".to_string(), display);
    }

    // 4. DBUS_SESSION_BUS_ADDRESS: standard path under runtime_dir.
    if !env.contains_key("DBUS_SESSION_BUS_ADDRESS") {
        let bus = runtime_dir.join("bus");
        if bus.exists() {
            env.insert(
                "DBUS_SESSION_BUS_ADDRESS".to_string(),
                format!("unix:path={}", bus.display()),
            );
        }
    }

    // 5. DISPLAY: first /tmp/.X11-unix/X<N> socket, if any. Wayland-only
    //    sessions may have none, which is fine.
    if !env.contains_key("DISPLAY")
        && let Some(display) = discover_x11_display(x11_dir)
    {
        env.insert("DISPLAY".to_string(), display);
    }

    env
}

fn discover_wayland_display(runtime_dir: &Path) -> Option<String> {
    let entries = std::fs::read_dir(runtime_dir).ok()?;
    let mut candidates: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("wayland-") {
            continue;
        }
        if name.ends_with(".lock") {
            continue;
        }
        // Strip the wayland-N prefix length check: "wayland-" + at least 1 char.
        if name.len() <= "wayland-".len() {
            continue;
        }
        candidates.push(name);
    }
    // Deterministic ordering: wayland-0 before wayland-1 etc.
    candidates.sort();
    candidates.into_iter().next()
}

fn discover_x11_display(x11_dir: &Path) -> Option<String> {
    let entries = std::fs::read_dir(x11_dir).ok()?;
    let mut candidates: Vec<u32> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(num_str) = name.strip_prefix('X') else {
            continue;
        };
        if let Ok(n) = num_str.parse::<u32>() {
            candidates.push(n);
        }
    }
    candidates.sort();
    candidates.into_iter().next().map(|n| format!(":{}", n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Build a fake /run/user/$UID directory with the given wayland sockets
    /// and optional dbus bus socket.
    fn fake_runtime(wayland_sockets: &[&str], include_bus: bool) -> TempDir {
        let td = TempDir::new().unwrap();
        for name in wayland_sockets {
            fs::write(td.path().join(name), b"").unwrap();
        }
        if include_bus {
            fs::write(td.path().join("bus"), b"").unwrap();
        }
        td
    }

    fn fake_x11(displays: &[&str]) -> TempDir {
        let td = TempDir::new().unwrap();
        for name in displays {
            fs::write(td.path().join(name), b"").unwrap();
        }
        td
    }

    /// When no env is inherited, we discover sockets from runtime_dir.
    #[test]
    fn discovers_wayland_and_dbus_from_runtime_dir() {
        let rt = fake_runtime(&["wayland-0", "wayland-0.lock"], true);
        let x = fake_x11(&["X0"]);
        let inherited = HashMap::new();

        let env = discover_gui_env_at(rt.path(), x.path(), &inherited);

        assert_eq!(env.get("WAYLAND_DISPLAY"), Some(&"wayland-0".to_string()));
        assert_eq!(
            env.get("XDG_RUNTIME_DIR"),
            Some(&rt.path().to_string_lossy().into_owned())
        );
        assert_eq!(
            env.get("DBUS_SESSION_BUS_ADDRESS"),
            Some(&format!("unix:path={}/bus", rt.path().display()))
        );
        assert_eq!(env.get("DISPLAY"), Some(&":0".to_string()));
    }

    /// Skips the `.lock` sibling and picks wayland-0 deterministically.
    #[test]
    fn ignores_wayland_lock_files() {
        let rt = fake_runtime(
            &["wayland-1", "wayland-1.lock", "wayland-0", "wayland-0.lock"],
            false,
        );
        assert_eq!(
            discover_wayland_display(rt.path()),
            Some("wayland-0".to_string())
        );
    }

    /// Returns None when runtime dir has no wayland sockets.
    #[test]
    fn no_wayland_returns_none() {
        let rt = fake_runtime(&[], false);
        assert_eq!(discover_wayland_display(rt.path()), None);
    }

    /// Inherited env takes precedence over filesystem discovery.
    #[test]
    fn inherited_env_wins() {
        let rt = fake_runtime(&["wayland-0", "wayland-0.lock"], true);
        let x = fake_x11(&["X0"]);
        let mut inherited = HashMap::new();
        inherited.insert("WAYLAND_DISPLAY".to_string(), "wayland-99".to_string());

        let env = discover_gui_env_at(rt.path(), x.path(), &inherited);
        assert_eq!(env.get("WAYLAND_DISPLAY"), Some(&"wayland-99".to_string()));
    }

    /// X11 display discovery picks lowest-numbered socket.
    #[test]
    fn x11_picks_lowest_display() {
        let x = fake_x11(&["X1", "X0", "X3"]);
        assert_eq!(discover_x11_display(x.path()), Some(":0".to_string()));
    }
}
