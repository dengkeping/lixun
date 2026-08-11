//! Discover user-session environment variables needed by spawned GUI processes.
//!
//! When `lixund` is started by systemd user-manager that did not inherit
//! `WAYLAND_DISPLAY`, `DBUS_SESSION_BUS_ADDRESS`, `DISPLAY`, `XAUTHORITY`,
//! and `XDG_RUNTIME_DIR` from the graphical session (e.g. `lixund` winning
//! the login race against KWin's `systemctl --user import-environment`, or a
//! Plasma/KWin crash-and-restart where the import did not rerun), a naïvely
//! spawned `lixun-gui` child inherits that empty env and cannot connect to
//! the Wayland compositor or session bus. GTK then either panics or silently
//! exits, and `lixun show` appears to do nothing.
//!
//! The same env is inherited in turn by every process the GUI launches on
//! behalf of a search hit (`xdg-open`, a desktop entry's `Exec`), so a gap
//! here surfaces as "pressing Enter on a hit does nothing" — the launcher
//! hides, the child starts, and the child dies before it maps a window.
//!
//! This module rediscovers the session env in two passes. [`discover_gui_env`]
//! reads filesystem artifacts that the compositor, X server, and dbus-daemon
//! leave behind under `/run/user/$UID` and `/tmp/.X11-unix`.
//! [`discover_gui_env_async`] then fills whatever is still missing from the
//! systemd user manager's `Environment` property over the session bus.
//!
//! Both passes are intentionally DE-agnostic: neither asks `plasmashell`,
//! `kwin`, or any other desktop component for help. The systemd query is not
//! an exception to that rule — `org.freedesktop.systemd1` is a service
//! manager, present and answering identically under Plasma, GNOME, Sway or a
//! bare compositor. It earns its place because the desktop-identity keys
//! (`XDG_CURRENT_DESKTOP` and friends) have no filesystem artifact at all, so
//! a filesystem-only reconstruction can never recover them — and their
//! absence silently changes which application a search hit opens in.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Keys we propagate into `lixun-gui`'s environment. Listed explicitly so the
/// set is reviewable and we never accidentally leak the daemon's full env.
///
/// The desktop-identity block (`XDG_CURRENT_DESKTOP` onward) has no filesystem
/// artifact and is recoverable only from the systemd user manager — see
/// [`overlay_manager_env`]. It is load-bearing for *which* application a hit
/// opens in: `xdg-open` consults `detectDE`, and without `XDG_CURRENT_DESKTOP`
/// it skips desktop-specific `kde-mimeapps.list` / `gnome-mimeapps.list` and
/// falls through to the first `mimeinfo.cache` entry — so a PDF that should
/// open in Okular opens in whatever alphabetically-first browser claims it.
const GUI_ENV_KEYS: &[&str] = &[
    "WAYLAND_DISPLAY",
    "DISPLAY",
    "XAUTHORITY",
    "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS",
    "XDG_SESSION_TYPE",
    "XDG_CURRENT_DESKTOP",
    "KDE_SESSION_VERSION",
    "KDE_FULL_SESSION",
    "XDG_DATA_DIRS",
    "XDG_CONFIG_DIRS",
];

/// Runtime-dir subdirectories known to hold a display manager's cookie
/// (`gdm/Xauthority`, `sddm/Xauthority`). Named explicitly rather than
/// walked: `/run/user/$UID` also hosts FUSE mounts (`gvfs`, `doc`) whose
/// `stat` can block indefinitely when the backing daemon is wedged, and
/// this runs on the GUI spawn path.
const XAUTH_SUBDIRS: &[&str] = &["gdm", "sddm", "lightdm"];

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
    let home = std::env::var_os("HOME").map(PathBuf::from);
    discover_gui_env_at(
        &runtime_dir,
        Path::new("/tmp/.X11-unix"),
        home.as_deref(),
        &inherited,
    )
}

/// Wall-clock ceiling on the systemd user-manager round trip. This sits on
/// the GUI/preview spawn path, so a wedged or absent session bus must cost a
/// bounded delay and then degrade to filesystem-only discovery.
const MANAGER_ENV_TIMEOUT: Duration = Duration::from_millis(500);

/// [`discover_gui_env`] plus a final pass that fills still-missing keys from
/// the systemd user manager's `Environment` property.
///
/// This is the only recovery path for the desktop-identity keys, which have no
/// filesystem artifact. Querying the *service manager* keeps the module's
/// DE-agnostic contract intact: `org.freedesktop.systemd1` is not a desktop
/// environment, and the same call answers identically under Plasma, GNOME,
/// Sway or a bare compositor. It exists because `lixund` can win the login
/// race against the session's `systemctl --user import-environment` (`After=`
/// on `graphical-session.target` is inert unless the unit also hangs off it —
/// see `packaging/systemd/lixund.service`), freezing a pre-session env for the
/// life of the process while the manager's own copy is correct.
///
/// Deliberately uncached: the manager env is empty before the session imports
/// it, so a cache populated by an early spawn would pin exactly the broken
/// state this exists to repair. Spawns are rare (once or twice per session).
pub async fn discover_gui_env_async() -> HashMap<String, String> {
    let mut env = discover_gui_env();
    let missing: Vec<&str> = GUI_ENV_KEYS
        .iter()
        .copied()
        .filter(|k| !env.contains_key(*k))
        .collect();
    if missing.is_empty() {
        return env;
    }
    match tokio::time::timeout(MANAGER_ENV_TIMEOUT, read_manager_environment()).await {
        Ok(Ok(manager)) => overlay_manager_env(&mut env, &missing, &manager),
        Ok(Err(e)) => {
            tracing::debug!("session_env: systemd manager Environment unavailable: {}", e);
        }
        Err(_) => {
            tracing::warn!(
                "session_env: systemd manager Environment timed out after {:?}; \
                 spawning with filesystem-discovered env only",
                MANAGER_ENV_TIMEOUT
            );
        }
    }
    env
}

/// Merge `manager` values for `missing` keys into `env`.
///
/// Split out from the I/O so the coupling rule below is unit-testable.
fn overlay_manager_env(
    env: &mut HashMap<String, String>,
    missing: &[&str],
    manager: &HashMap<String, String>,
) {
    for key in missing {
        if let Some(v) = manager.get(*key)
            && !v.is_empty()
        {
            env.insert((*key).to_string(), v.clone());
        }
    }

    // Coupling rule: a KDE desktop identity is only safe to hand over WITH
    // `KDE_SESSION_VERSION`. `xdg-open`'s `open_kde()` branches on it —
    // set, it runs `kde-open`; unset, it runs `kfmclient exec`, which is not
    // shipped by Plasma 6 at all, so the open fails outright
    // (`exit_failure_operation_failed`). Announcing KDE without the version
    // is therefore strictly worse than announcing nothing: it converts
    // "opens in the wrong application" into "opens nothing".
    if env
        .get("XDG_CURRENT_DESKTOP")
        .is_some_and(|d| d.split(':').any(|part| part.eq_ignore_ascii_case("KDE")))
        && !env.contains_key("KDE_SESSION_VERSION")
    {
        tracing::warn!(
            "session_env: dropping XDG_CURRENT_DESKTOP (KDE) — KDE_SESSION_VERSION \
             is unavailable, and xdg-open would fall back to the unshipped kfmclient"
        );
        env.remove("XDG_CURRENT_DESKTOP");
    }
}

/// Read `org.freedesktop.systemd1.Manager.Environment` (an array of
/// `KEY=VALUE` strings) off the session bus and parse it into a map.
async fn read_manager_environment() -> anyhow::Result<HashMap<String, String>> {
    let conn = zbus::Connection::session().await?;
    let proxy = zbus::Proxy::new(
        &conn,
        "org.freedesktop.systemd1",
        "/org/freedesktop/systemd1",
        "org.freedesktop.DBus.Properties",
    )
    .await?;
    let raw: zbus::zvariant::OwnedValue = proxy
        .call("Get", &("org.freedesktop.systemd1.Manager", "Environment"))
        .await?;
    let entries: Vec<String> = raw.try_into()?;
    Ok(entries
        .into_iter()
        .filter_map(|entry| {
            // Values may themselves contain '=' (XDG_DATA_DIRS does not, but
            // DBUS_SESSION_BUS_ADDRESS can), so split on the FIRST separator.
            let (k, v) = entry.split_once('=')?;
            Some((k.to_string(), v.to_string()))
        })
        .collect())
}

/// Same as [`discover_gui_env`] but with explicit search paths and an
/// explicitly-injected `inherited` env map (instead of reading the process
/// env). Used by tests to avoid global env races.
pub fn discover_gui_env_at(
    runtime_dir: &Path,
    x11_dir: &Path,
    home_dir: Option<&Path>,
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

    // 5. XAUTHORITY: the X11 auth cookie. A compositor that starts its own
    //    XWayland (KWin, Mutter) writes a per-session cookie under
    //    /run/user/$uid rather than $HOME/.Xauthority, so libX11's default
    //    lookup does NOT find it — the variable has to be carried. A child
    //    holding DISPLAY but no cookie gets "Authorization required, but no
    //    authorization protocol specified" and XOpenDisplay returns NULL.
    if !env.contains_key("XAUTHORITY")
        && let Some(xauth) = discover_xauthority(runtime_dir, home_dir)
    {
        env.insert("XAUTHORITY".to_string(), xauth);
    }

    // 6. DISPLAY: first /tmp/.X11-unix/X<N> socket, if any. Wayland-only
    //    sessions may have none, which is fine.
    //
    //    Synthesize it ONLY when a cookie is also going out. A DISPLAY with
    //    no reachable XAUTHORITY is never better than no DISPLAY: a toolkit
    //    that auto-selects a backend (Qt's QT_QPA_PLATFORM default, GTK's
    //    GDK_BACKEND default) sees DISPLAY, PREFERS X11 over Wayland, fails
    //    the auth handshake and exits — where with DISPLAY unset it would
    //    have taken the Wayland socket and worked. It does not rescue a
    //    client whose backend is pinned to X11 at build time (Arch's
    //    Chromium, which has no ozone-platform-hint configured, aborts
    //    identically either way); for those the cookie above is the fix and
    //    this guard merely avoids making other clients worse.
    //
    //    An *inherited* DISPLAY is left alone even without a cookie: on a
    //    real X11 session the cookie normally sits at libX11's default
    //    $HOME/.Xauthority and needs no env var, so second-guessing a value
    //    the session handed us deliberately would break those sessions.
    if !env.contains_key("DISPLAY")
        && env.contains_key("XAUTHORITY")
        && let Some(display) = discover_x11_display(x11_dir)
    {
        env.insert("DISPLAY".to_string(), display);
    }

    // 7. XDG_SESSION_TYPE: inferred from whichever display socket resolved,
    //    never queried — the module stays DE-agnostic. Wayland wins when
    //    both are present; that is an XWayland-capable Wayland session.
    //    (XDG_CURRENT_DESKTOP has no DE-agnostic filesystem source and is
    //    deliberately left unset when the session did not provide it.)
    if !env.contains_key("XDG_SESSION_TYPE") {
        if env.contains_key("WAYLAND_DISPLAY") {
            env.insert("XDG_SESSION_TYPE".to_string(), "wayland".to_string());
        } else if env.contains_key("DISPLAY") {
            env.insert("XDG_SESSION_TYPE".to_string(), "x11".to_string());
        }
    }

    env
}

/// Locate an X11 auth cookie without asking the desktop environment.
///
/// Searched, in one pass: `$XDG_RUNTIME_DIR/xauth*` (KWin/Plasma writes
/// `xauth_XXXXXX`), `$XDG_RUNTIME_DIR/{gdm,sddm,lightdm}/Xauthority`, and
/// `$HOME/.Xauthority`. The newest non-empty file wins, so a stale cookie
/// left in the runtime dir by a previous session cannot shadow the live one.
fn discover_xauthority(runtime_dir: &Path, home_dir: Option<&Path>) -> Option<String> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Ok(entries) = std::fs::read_dir(runtime_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("xauth") || name == "Xauthority" {
                candidates.push(entry.path());
            }
        }
    }
    for sub in XAUTH_SUBDIRS {
        candidates.push(runtime_dir.join(sub).join("Xauthority"));
    }
    if let Some(home) = home_dir {
        candidates.push(home.join(".Xauthority"));
    }

    candidates
        .into_iter()
        .filter_map(|path| {
            let meta = std::fs::metadata(&path).ok()?;
            // An empty cookie file authenticates nothing; treat it as absent
            // so a truncated leftover cannot suppress a good candidate.
            if !meta.is_file() || meta.len() == 0 {
                return None;
            }
            Some((meta.modified().ok()?, path))
        })
        .max_by(|(a, _), (b, _)| a.cmp(b))
        .map(|(_, path)| path.to_string_lossy().into_owned())
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

    /// Drop a KWin-style per-session cookie into a fake runtime dir.
    fn write_cookie(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, b"fake-cookie").unwrap();
        path
    }

    /// When no env is inherited, we discover sockets from runtime_dir. With a
    /// cookie present, DISPLAY is synthesized and carried alongside it.
    #[test]
    fn discovers_wayland_and_dbus_from_runtime_dir() {
        let rt = fake_runtime(&["wayland-0", "wayland-0.lock"], true);
        let x = fake_x11(&["X0"]);
        let cookie = write_cookie(rt.path(), "xauth_ABCDEF");
        let inherited = HashMap::new();

        let env = discover_gui_env_at(rt.path(), x.path(), None, &inherited);

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
        assert_eq!(
            env.get("XAUTHORITY"),
            Some(&cookie.to_string_lossy().into_owned()),
            "KWin-style xauth_* cookie must be carried to the child"
        );
    }

    /// Regression: a synthesized DISPLAY with no cookie to authenticate it
    /// makes Chromium/Qt clients prefer X11, fail the handshake and exit,
    /// instead of falling back to the Wayland socket. Suppress it entirely.
    #[test]
    fn synthesized_display_suppressed_without_cookie() {
        let rt = fake_runtime(&["wayland-0", "wayland-0.lock"], true);
        let x = fake_x11(&["X0"]);

        let env = discover_gui_env_at(rt.path(), x.path(), None, &HashMap::new());

        assert_eq!(env.get("XAUTHORITY"), None, "no cookie exists to find");
        assert_eq!(
            env.get("DISPLAY"),
            None,
            "DISPLAY without XAUTHORITY breaks X11-preferring clients"
        );
        // The Wayland path is still fully populated, so the child works.
        assert_eq!(env.get("WAYLAND_DISPLAY"), Some(&"wayland-0".to_string()));
        assert_eq!(env.get("XDG_SESSION_TYPE"), Some(&"wayland".to_string()));
    }

    /// An inherited DISPLAY is never second-guessed: a classic X11 session
    /// keeps its cookie at libX11's default $HOME/.Xauthority and passes no
    /// XAUTHORITY, so dropping DISPLAY there would break it.
    #[test]
    fn inherited_display_kept_without_cookie() {
        let rt = fake_runtime(&[], false);
        let x = fake_x11(&[]);
        let mut inherited = HashMap::new();
        inherited.insert("DISPLAY".to_string(), ":7".to_string());

        let env = discover_gui_env_at(rt.path(), x.path(), None, &inherited);

        assert_eq!(env.get("DISPLAY"), Some(&":7".to_string()));
        assert_eq!(env.get("XDG_SESSION_TYPE"), Some(&"x11".to_string()));
    }

    /// A display manager's `gdm/Xauthority` is found one level down, and
    /// `$HOME/.Xauthority` backstops both.
    #[test]
    fn finds_display_manager_and_home_cookies() {
        let rt = fake_runtime(&[], false);
        fs::create_dir(rt.path().join("gdm")).unwrap();
        let dm_cookie = write_cookie(&rt.path().join("gdm"), "Xauthority");
        assert_eq!(
            discover_xauthority(rt.path(), None),
            Some(dm_cookie.to_string_lossy().into_owned())
        );

        let home = TempDir::new().unwrap();
        let home_cookie = write_cookie(home.path(), ".Xauthority");
        let empty_rt = fake_runtime(&[], false);
        assert_eq!(
            discover_xauthority(empty_rt.path(), Some(home.path())),
            Some(home_cookie.to_string_lossy().into_owned())
        );
    }

    /// An empty cookie authenticates nothing; it must not shadow a good one.
    #[test]
    fn empty_cookie_ignored() {
        let rt = fake_runtime(&[], false);
        fs::write(rt.path().join("xauth_EMPTY"), b"").unwrap();
        assert_eq!(discover_xauthority(rt.path(), None), None);

        let home = TempDir::new().unwrap();
        let good = write_cookie(home.path(), ".Xauthority");
        assert_eq!(
            discover_xauthority(rt.path(), Some(home.path())),
            Some(good.to_string_lossy().into_owned()),
            "empty runtime-dir cookie must lose to a real one"
        );
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

        let env = discover_gui_env_at(rt.path(), x.path(), None, &inherited);
        assert_eq!(env.get("WAYLAND_DISPLAY"), Some(&"wayland-99".to_string()));
    }

    /// X11 display discovery picks lowest-numbered socket.
    #[test]
    fn x11_picks_lowest_display() {
        let x = fake_x11(&["X1", "X0", "X3"]);
        assert_eq!(discover_x11_display(x.path()), Some(":0".to_string()));
    }

    fn manager_env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    /// The manager env fills desktop-identity keys the filesystem cannot.
    #[test]
    fn manager_overlay_fills_desktop_identity() {
        let mut env = HashMap::new();
        let missing = ["XDG_CURRENT_DESKTOP", "KDE_SESSION_VERSION", "XDG_DATA_DIRS"];
        let manager = manager_env(&[
            ("XDG_CURRENT_DESKTOP", "KDE"),
            ("KDE_SESSION_VERSION", "6"),
            ("XDG_DATA_DIRS", "/usr/share"),
            ("SOME_UNLISTED_KEY", "leak"),
        ]);

        overlay_manager_env(&mut env, &missing, &manager);

        assert_eq!(env.get("XDG_CURRENT_DESKTOP"), Some(&"KDE".to_string()));
        assert_eq!(env.get("KDE_SESSION_VERSION"), Some(&"6".to_string()));
        assert_eq!(env.get("XDG_DATA_DIRS"), Some(&"/usr/share".to_string()));
        assert_eq!(
            env.get("SOME_UNLISTED_KEY"),
            None,
            "overlay must copy only the keys it was asked for"
        );
    }

    /// Regression guard for the kfmclient trap: announcing a KDE desktop
    /// without KDE_SESSION_VERSION makes xdg-open's open_kde() shell out to
    /// `kfmclient exec`, which Plasma 6 does not ship — turning "wrong app"
    /// into "no app". Better to announce no desktop at all.
    #[test]
    fn kde_identity_dropped_without_session_version() {
        let mut env = HashMap::new();
        let missing = ["XDG_CURRENT_DESKTOP", "KDE_SESSION_VERSION"];
        let manager = manager_env(&[("XDG_CURRENT_DESKTOP", "KDE")]);

        overlay_manager_env(&mut env, &missing, &manager);

        assert_eq!(env.get("XDG_CURRENT_DESKTOP"), None);
    }

    /// The KDE check reads the colon-separated list, not the whole string:
    /// Plasma sets `XDG_CURRENT_DESKTOP=KDE`, but derivatives ship values
    /// like `plasma:KDE` that must be recognised too.
    #[test]
    fn kde_identity_detected_within_colon_list() {
        let mut env = HashMap::new();
        let missing = ["XDG_CURRENT_DESKTOP"];
        overlay_manager_env(
            &mut env,
            &missing,
            &manager_env(&[("XDG_CURRENT_DESKTOP", "plasma:KDE")]),
        );
        assert_eq!(env.get("XDG_CURRENT_DESKTOP"), None, "colon list, no version");

        let mut env = HashMap::new();
        let missing = ["XDG_CURRENT_DESKTOP", "KDE_SESSION_VERSION"];
        overlay_manager_env(
            &mut env,
            &missing,
            &manager_env(&[
                ("XDG_CURRENT_DESKTOP", "plasma:KDE"),
                ("KDE_SESSION_VERSION", "6"),
            ]),
        );
        assert_eq!(
            env.get("XDG_CURRENT_DESKTOP"),
            Some(&"plasma:KDE".to_string()),
            "paired with a version it is safe to propagate"
        );
    }

    /// A non-KDE desktop identity is never subject to the coupling rule.
    #[test]
    fn non_kde_identity_passes_through() {
        let mut env = HashMap::new();
        let missing = ["XDG_CURRENT_DESKTOP"];
        overlay_manager_env(
            &mut env,
            &missing,
            &manager_env(&[("XDG_CURRENT_DESKTOP", "GNOME")]),
        );
        assert_eq!(env.get("XDG_CURRENT_DESKTOP"), Some(&"GNOME".to_string()));
    }

    /// Filesystem-discovered values win; the overlay only fills gaps.
    #[test]
    fn manager_overlay_never_overwrites_discovered_values() {
        let mut env = HashMap::new();
        env.insert("WAYLAND_DISPLAY".to_string(), "wayland-0".to_string());
        // WAYLAND_DISPLAY is deliberately absent from `missing`.
        overlay_manager_env(
            &mut env,
            &["XDG_SESSION_TYPE"],
            &manager_env(&[
                ("WAYLAND_DISPLAY", "wayland-99"),
                ("XDG_SESSION_TYPE", "wayland"),
            ]),
        );
        assert_eq!(env.get("WAYLAND_DISPLAY"), Some(&"wayland-0".to_string()));
        assert_eq!(env.get("XDG_SESSION_TYPE"), Some(&"wayland".to_string()));
    }
}
