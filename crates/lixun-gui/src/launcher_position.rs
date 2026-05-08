//! Per-monitor launcher position persistence.
//!
//! The launcher is a wlr-layer-shell surface anchored top-left with
//! margins. Users can drag it (see `window::install_drag_gesture`),
//! and the resulting `(top, left)` margin pair is persisted under
//! `[gui.position.<connector>]` in `~/.config/lixun/config.toml`.
//!
//! Per-monitor scoping: the key is the GDK monitor connector name
//! (e.g. `eDP-1`, `DP-2`). When the user moves between monitors —
//! laptop screen vs. external display, multi-head setups — each
//! monitor restores its own saved position. First-time on a fresh
//! monitor falls back to the default top-anchored centered layout.
//!
//! Sentinel for connector name: GDK returns `None` for the
//! connector on some compositors / under headless tests; in that
//! case we fall back to the literal string `"default"` so the
//! feature degrades gracefully instead of panicking.

use std::path::PathBuf;

/// Single (top, left) saved position. Margins are absolute pixels
/// from the corresponding screen edge.
#[derive(Debug, Clone, Copy)]
pub struct SavedPosition {
    pub top: i32,
    pub left: i32,
}

fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("lixun/config.toml")
}

fn sanitize_connector(connector: Option<&str>) -> String {
    connector
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "default".to_string())
}

/// Load saved position for `connector`, if any. Returns `None`
/// when the config file is missing, malformed, or has no entry
/// for this monitor.
pub fn load(connector: Option<&str>) -> Option<SavedPosition> {
    let key = sanitize_connector(connector);
    let path = config_path();
    let content = std::fs::read_to_string(&path).ok()?;
    let doc = content.parse::<toml_edit::DocumentMut>().ok()?;
    let position = doc
        .get("gui")?
        .as_table()?
        .get("position")?
        .as_table()?
        .get(&key)?
        .as_table()?;
    let top = position.get("top")?.as_integer()? as i32;
    let left = position.get("left")?.as_integer()? as i32;
    Some(SavedPosition { top, left })
}

/// Persist `(top, left)` for `connector` to the config file.
/// Best-effort: errors are logged at warn and dropped, since UX
/// position tracking should never crash the launcher.
pub fn save(connector: Option<&str>, top: i32, left: i32) {
    let key = sanitize_connector(connector);
    let path = config_path();

    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc = match content.parse::<toml_edit::DocumentMut>() {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("launcher_position: cannot parse {:?}: {}", path, e);
            return;
        }
    };

    // Ensure [gui] table exists.
    if !doc.contains_key("gui") {
        doc.insert("gui", toml_edit::Item::Table(toml_edit::Table::new()));
    }
    let gui = doc.get_mut("gui").and_then(|v| v.as_table_mut());
    let Some(gui) = gui else {
        tracing::warn!("launcher_position: [gui] is not a table");
        return;
    };

    // Ensure [gui.position] table exists.
    if !gui.contains_key("position") {
        gui.insert("position", toml_edit::Item::Table(toml_edit::Table::new()));
    }
    let position = gui.get_mut("position").and_then(|v| v.as_table_mut());
    let Some(position) = position else {
        tracing::warn!("launcher_position: [gui.position] is not a table");
        return;
    };

    // Per-monitor sub-table.
    let mut entry = toml_edit::Table::new();
    entry.insert("top", toml_edit::value(i64::from(top)));
    entry.insert("left", toml_edit::value(i64::from(left)));
    position.insert(&key, toml_edit::Item::Table(entry));

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&path, doc.to_string()) {
        tracing::warn!("launcher_position: write {:?} failed: {}", path, e);
    }
}

/// Remove saved position for `connector` (Ctrl+0 reset).
/// Best-effort: silent on missing file or missing entry.
pub fn clear(connector: Option<&str>) {
    let key = sanitize_connector(connector);
    let path = config_path();
    let Ok(content) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(mut doc) = content.parse::<toml_edit::DocumentMut>() else {
        return;
    };
    if let Some(position) = doc
        .get_mut("gui")
        .and_then(|v| v.as_table_mut())
        .and_then(|t| t.get_mut("position"))
        .and_then(|v| v.as_table_mut())
    {
        position.remove(&key);
    }
    if let Err(e) = std::fs::write(&path, doc.to_string()) {
        tracing::warn!("launcher_position: clear write {:?} failed: {}", path, e);
    }
}
