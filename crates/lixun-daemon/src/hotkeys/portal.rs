//! Global shortcut portal listener with a persistent session token.
//!
//! ashpd 0.13 does not expose `session_handle_token` on CreateSessionOptions
//! (the field is `pub(crate)`), so every ashpd-based daemon run allocates a
//! fresh random token. KDE Plasma persists bindings by token, producing an
//! extra entry per run. We bypass ashpd and drive the portal with zbus so we
//! can reuse the same token across restarts.
//!
//! Flow:
//!   1. Load a persistent token from state_dir/global_shortcuts_token or
//!      generate one on first run.
//!   2. Call GlobalShortcuts.CreateSession with a deterministic
//!      session_handle_token + a fresh per-call handle_token.
//!   3. Wait for the Response signal on the Request object path, extract
//!      the session_handle from the response results.
//!   4. Call GlobalShortcuts.BindShortcuts for the "toggle" shortcut.
//!   5. Subscribe to the Activated signal (filtered by our session_handle)
//!      and forward matching events to an mpsc channel.
//!   6. If the signal stream ends, re-subscribe without recreating the
//!      session (so Plasma does not create another entry).
//!
//! Login-race resilience: at user login, the systemd unit orders against
//! `xdg-desktop-portal.service` (the frontend), but on KDE the
//! `GlobalShortcuts` interface is exported by `xdg-desktop-portal-kde`,
//! which is D-Bus-activated lazily. The frontend can answer Registry
//! calls before the KDE backend has exported GlobalShortcuts, so a
//! one-shot `CreateSession` reliably loses the race during cold login
//! and the shortcut never registers. We therefore retry session
//! establishment with bounded exponential backoff. The persistent token
//! keeps KDE from accumulating ghost entries across retries.

use anyhow::{Context, Result};
use futures::StreamExt;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;
use zbus::Connection;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

const PORTAL_SERVICE: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const IFACE_GLOBAL_SHORTCUTS: &str = "org.freedesktop.portal.GlobalShortcuts";
const IFACE_REQUEST: &str = "org.freedesktop.portal.Request";
const TOKEN_FILE: &str = "global_shortcuts_token";
const SHORTCUT_ID: &str = "toggle";
const RESUBSCRIBE_MIN_BACKOFF: Duration = Duration::from_secs(2);
const RESUBSCRIBE_MAX_BACKOFF: Duration = Duration::from_secs(60);

pub(super) async fn run(
    preferred_trigger: String,
    state_dir: PathBuf,
    tx: mpsc::Sender<()>,
) -> Result<()> {
    let token = load_or_create_token(&state_dir)?;
    let conn = Connection::session()
        .await
        .context("connecting to session bus")?;

    crate::portal_identity::register(&conn, crate::portal_identity::DAEMON_APP_ID).await?;
    crate::portal_identity::spawn_reregister_watcher(
        conn.clone(),
        crate::portal_identity::DAEMON_APP_ID.to_string(),
    )
    .await?;

    let session_handle = create_session_with_retry(&conn, &token).await?;
    tracing::info!(
        "hotkeys: session established at {} (token {})",
        session_handle.as_str(),
        token
    );

    bind_shortcut_with_retry(&conn, &session_handle, &preferred_trigger).await?;
    tracing::info!("hotkeys: shortcut '{}' bound", preferred_trigger);

    super::hyprland_bind::try_register(&preferred_trigger).await;

    supervisor_loop(conn, session_handle, tx).await;
    Ok(())
}

async fn create_session_with_retry(
    conn: &Connection,
    token: &str,
) -> Result<OwnedObjectPath> {
    let mut backoff = RESUBSCRIBE_MIN_BACKOFF;
    loop {
        match create_session(conn, token).await {
            Ok(handle) => return Ok(handle),
            Err(e) if is_portal_unready(&e) => {
                tracing::warn!(
                    "hotkeys: portal backend not ready ({}); retry in {:?}",
                    e,
                    backoff
                );
                tokio::time::sleep(backoff).await;
                backoff = std::cmp::min(backoff.saturating_mul(2), RESUBSCRIBE_MAX_BACKOFF);
            }
            Err(e) => return Err(e),
        }
    }
}

async fn bind_shortcut_with_retry(
    conn: &Connection,
    session_handle: &OwnedObjectPath,
    preferred_trigger: &str,
) -> Result<()> {
    let mut backoff = RESUBSCRIBE_MIN_BACKOFF;
    loop {
        match bind_shortcut(conn, session_handle, preferred_trigger).await {
            Ok(()) => return Ok(()),
            Err(e) if is_portal_unready(&e) => {
                tracing::warn!(
                    "hotkeys: BindShortcuts not ready ({}); retry in {:?}",
                    e,
                    backoff
                );
                tokio::time::sleep(backoff).await;
                backoff = std::cmp::min(backoff.saturating_mul(2), RESUBSCRIBE_MAX_BACKOFF);
            }
            Err(e) => return Err(e),
        }
    }
}

fn is_portal_unready(err: &anyhow::Error) -> bool {
    let msg = format!("{:#}", err).to_lowercase();
    msg.contains("unknownmethod")
        || msg.contains("serviceunknown")
        || msg.contains("nameownerlost")
        || msg.contains("noreply")
        || msg.contains("disconnected")
        || msg.contains("no such interface")
        || msg.contains("not provided by any .service")
}

fn token_path(state_dir: &Path) -> PathBuf {
    state_dir.join(TOKEN_FILE)
}

fn load_or_create_token(state_dir: &Path) -> Result<String> {
    let path = token_path(state_dir);
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim().to_string();
        if is_valid_token(&trimmed) {
            return Ok(trimmed);
        }
        tracing::warn!("hotkeys: token at {:?} is invalid, regenerating", path);
    }

    let token = generate_token();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&path, &token).context("writing session token")?;
    tracing::info!("hotkeys: generated new persistent token at {:?}", path);
    Ok(token)
}

fn is_valid_token(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn generate_token() -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("entropy available");
    let mut s = String::from("lixun_");
    for b in &bytes {
        s.push(ALPHABET[(*b as usize) % ALPHABET.len()] as char);
    }
    s
}

fn random_handle() -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut bytes = [0u8; 10];
    getrandom::fill(&mut bytes).expect("entropy available");
    let mut s = String::from("lixun_req_");
    for b in &bytes {
        s.push(ALPHABET[(*b as usize) % ALPHABET.len()] as char);
    }
    s
}

async fn create_session(conn: &Connection, session_token: &str) -> Result<OwnedObjectPath> {
    let handle_token = random_handle();
    let mut options: HashMap<&str, Value<'_>> = HashMap::new();
    options.insert("session_handle_token", session_token.into());
    options.insert("handle_token", handle_token.as_str().into());

    let response = portal_request(conn, "CreateSession", &options, &handle_token)
        .await
        .context("GlobalShortcuts.CreateSession")?;
    extract_session_handle(&response)
}

async fn bind_shortcut(
    conn: &Connection,
    session_handle: &OwnedObjectPath,
    preferred_trigger: &str,
) -> Result<()> {
    let normalized = normalize_trigger(preferred_trigger);
    if normalized != preferred_trigger {
        tracing::info!(
            "hotkeys: normalized trigger {:?} -> {:?} (XDG shortcuts spec)",
            preferred_trigger,
            normalized
        );
    }
    let handle_token = random_handle();
    let mut shortcut_opts: HashMap<&str, Value<'_>> = HashMap::new();
    shortcut_opts.insert("description", "Toggle Lixun launcher".into());
    shortcut_opts.insert("preferred_trigger", normalized.as_str().into());
    let shortcuts: Vec<(&str, HashMap<&str, Value<'_>>)> = vec![(SHORTCUT_ID, shortcut_opts)];

    let mut options: HashMap<&str, Value<'_>> = HashMap::new();
    options.insert("handle_token", handle_token.as_str().into());

    let parent_window = "";

    let unique = conn
        .unique_name()
        .context("session bus connection has no unique name")?
        .to_string();
    let request_path = predict_request_path(&unique, &handle_token)?;
    let request_proxy =
        zbus::Proxy::new(conn, PORTAL_SERVICE, request_path.clone(), IFACE_REQUEST).await?;
    let mut stream = request_proxy.receive_signal("Response").await?;

    let proxy = zbus::Proxy::new(conn, PORTAL_SERVICE, PORTAL_PATH, IFACE_GLOBAL_SHORTCUTS).await?;
    let actual_path: OwnedObjectPath = proxy
        .call(
            "BindShortcuts",
            &(session_handle, shortcuts, parent_window, &options),
        )
        .await
        .context("BindShortcuts call")?;

    let _ = if actual_path.as_str() != request_path.as_str() {
        await_request_response(conn, &actual_path).await?
    } else {
        await_response_stream(&mut stream).await?
    };
    Ok(())
}

/// Predict the Request object path the portal will use for a method call with
/// the given `handle_token`, following the spec at
/// <https://flatpak.github.io/xdg-desktop-portal/docs/request.html>.
///
/// The predictable form is
/// `/org/freedesktop/portal/desktop/request/SENDER/HANDLE_TOKEN` where
/// `SENDER` is the caller's unique bus name with the leading `:` stripped
/// and `.` replaced by `_`. Subscribing to the Response signal at the
/// predicted path BEFORE issuing the method call closes a race where the
/// portal answers fast enough (common on hyprland/wlroots backends) that a
/// post-call subscription misses the signal entirely.
fn predict_request_path(unique_name: &str, handle_token: &str) -> Result<OwnedObjectPath> {
    let sender = unique_name
        .strip_prefix(':')
        .unwrap_or(unique_name)
        .replace('.', "_");
    let raw = format!("/org/freedesktop/portal/desktop/request/{sender}/{handle_token}");
    let path = ObjectPath::try_from(raw.as_str()).context("predicted request path invalid")?;
    Ok(path.into())
}

async fn portal_request(
    conn: &Connection,
    method: &'static str,
    body: &HashMap<&str, Value<'_>>,
    handle_token: &str,
) -> Result<HashMap<String, OwnedValue>> {
    // Subscribe BEFORE issuing the call to avoid losing the Response signal
    // when the portal answers synchronously.
    let unique = conn
        .unique_name()
        .context("session bus connection has no unique name")?
        .to_string();
    let request_path = predict_request_path(&unique, handle_token)?;
    let request_proxy =
        zbus::Proxy::new(conn, PORTAL_SERVICE, request_path.clone(), IFACE_REQUEST).await?;
    let mut stream = request_proxy.receive_signal("Response").await?;

    let proxy = zbus::Proxy::new(conn, PORTAL_SERVICE, PORTAL_PATH, IFACE_GLOBAL_SHORTCUTS).await?;
    let actual_path: OwnedObjectPath = proxy.call(method, body).await?;
    if actual_path.as_str() != request_path.as_str() {
        // Spec violation by the portal; fall back to the path it returned.
        return await_request_response(conn, &actual_path).await;
    }

    await_response_stream(&mut stream).await
}

async fn await_request_response(
    conn: &Connection,
    request_path: &OwnedObjectPath,
) -> Result<HashMap<String, OwnedValue>> {
    let request_proxy =
        zbus::Proxy::new(conn, PORTAL_SERVICE, request_path.as_str(), IFACE_REQUEST).await?;
    let mut stream = request_proxy.receive_signal("Response").await?;
    await_response_stream(&mut stream).await
}

async fn await_response_stream(
    stream: &mut zbus::proxy::SignalStream<'_>,
) -> Result<HashMap<String, OwnedValue>> {
    let Some(msg) = stream.next().await else {
        anyhow::bail!("Response stream closed without a message");
    };
    let (code, results): (u32, HashMap<String, OwnedValue>) = msg.body().deserialize()?;
    if code != 0 {
        anyhow::bail!("portal request failed with code {}", code);
    }
    Ok(results)
}

fn extract_session_handle(results: &HashMap<String, OwnedValue>) -> Result<OwnedObjectPath> {
    let value = results
        .get("session_handle")
        .context("response missing session_handle")?;
    let s: &str = value
        .downcast_ref()
        .context("session_handle is not a string")?;
    let path = ObjectPath::try_from(s).context("session_handle not a valid object path")?;
    Ok(path.into())
}

async fn supervisor_loop(conn: Connection, session_handle: OwnedObjectPath, tx: mpsc::Sender<()>) {
    let mut backoff = RESUBSCRIBE_MIN_BACKOFF;
    loop {
        let proxy_res =
            zbus::Proxy::new(&conn, PORTAL_SERVICE, PORTAL_PATH, IFACE_GLOBAL_SHORTCUTS).await;
        let proxy = match proxy_res {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    "hotkeys: proxy creation failed: {}; retry in {:?}",
                    e,
                    backoff
                );
                tokio::time::sleep(backoff).await;
                backoff = std::cmp::min(backoff.saturating_mul(2), RESUBSCRIBE_MAX_BACKOFF);
                continue;
            }
        };

        let mut stream = match proxy.receive_signal("Activated").await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    "hotkeys: receive_signal(Activated) failed: {}; retry in {:?}",
                    e,
                    backoff
                );
                tokio::time::sleep(backoff).await;
                backoff = std::cmp::min(backoff.saturating_mul(2), RESUBSCRIBE_MAX_BACKOFF);
                continue;
            }
        };

        tracing::info!("hotkeys: subscribed to Activated");
        backoff = RESUBSCRIBE_MIN_BACKOFF;

        while let Some(msg) = stream.next().await {
            match parse_activated(&msg) {
                Ok((msg_session, shortcut_id)) => {
                    if msg_session.as_str() != session_handle.as_str() {
                        continue;
                    }
                    if shortcut_id != SHORTCUT_ID {
                        continue;
                    }
                    if tx.send(()).await.is_err() {
                        tracing::info!("hotkeys: downstream closed, exiting");
                        return;
                    }
                }
                Err(e) => {
                    tracing::debug!("hotkeys: parse Activated failed: {}", e);
                }
            }
        }

        tracing::warn!(
            "hotkeys: Activated stream ended; re-subscribing in {:?}",
            backoff
        );
        tokio::time::sleep(backoff).await;
        backoff = std::cmp::min(backoff.saturating_mul(2), RESUBSCRIBE_MAX_BACKOFF);
    }
}

fn parse_activated(msg: &zbus::Message) -> Result<(OwnedObjectPath, String)> {
    let (session, shortcut_id, _timestamp, _options): (
        OwnedObjectPath,
        String,
        u64,
        HashMap<String, OwnedValue>,
    ) = msg.body().deserialize()?;
    Ok((session, shortcut_id))
}

/// Normalize a user-supplied shortcut trigger to the XDG shortcuts
/// specification (CTRL/ALT/SHIFT/NUM/LOGO). Common aliases like "Super",
/// "Meta", "Win" are mapped to LOGO; "Control" to CTRL. The key identifier
/// after the last `+` is preserved verbatim (xkbcommon keysym names are
/// case-sensitive, e.g. "space" vs "Return").
///
/// Ref: https://specifications.freedesktop.org/shortcuts-spec/latest/
fn normalize_trigger(trigger: &str) -> String {
    let parts: Vec<&str> = trigger.split('+').map(str::trim).collect();
    if parts.is_empty() {
        return trigger.to_string();
    }
    let mut out: Vec<String> = Vec::with_capacity(parts.len());
    for (i, part) in parts.iter().enumerate() {
        if i == parts.len() - 1 {
            out.push((*part).to_string());
        } else {
            out.push(normalize_modifier(part));
        }
    }
    out.join("+")
}

fn normalize_modifier(m: &str) -> String {
    match m.to_ascii_uppercase().as_str() {
        "CTRL" | "CONTROL" => "CTRL".to_string(),
        "ALT" | "OPT" | "OPTION" => "ALT".to_string(),
        "SHIFT" => "SHIFT".to_string(),
        "NUM" | "NUMLOCK" => "NUM".to_string(),
        "LOGO" | "SUPER" | "META" | "WIN" | "WINDOWS" | "CMD" | "COMMAND" => "LOGO".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_generation_shape() {
        let t = generate_token();
        assert!(t.starts_with("lixun_"));
        assert!(is_valid_token(&t));
        assert_eq!(t.len(), "lixun_".len() + 16);
    }

    #[test]
    fn validate_rejects_bad_chars() {
        assert!(is_valid_token("lixun_abc123"));
        assert!(!is_valid_token(""));
        assert!(!is_valid_token("has space"));
        assert!(!is_valid_token("dash-not-allowed"));
        assert!(!is_valid_token("slash/bad"));
    }

    #[test]
    fn normalize_trigger_maps_super_to_logo() {
        assert_eq!(normalize_trigger("Super+space"), "LOGO+space");
        assert_eq!(normalize_trigger("super+space"), "LOGO+space");
        assert_eq!(normalize_trigger("SUPER+space"), "LOGO+space");
    }

    #[test]
    fn normalize_trigger_maps_meta_and_win() {
        assert_eq!(normalize_trigger("Meta+space"), "LOGO+space");
        assert_eq!(normalize_trigger("Win+space"), "LOGO+space");
        assert_eq!(normalize_trigger("Windows+space"), "LOGO+space");
    }

    #[test]
    fn normalize_trigger_preserves_valid_spec_modifiers() {
        assert_eq!(normalize_trigger("CTRL+A"), "CTRL+A");
        assert_eq!(normalize_trigger("CTRL+ALT+Return"), "CTRL+ALT+Return");
        assert_eq!(normalize_trigger("SHIFT+a"), "SHIFT+a");
        assert_eq!(normalize_trigger("LOGO+space"), "LOGO+space");
    }

    #[test]
    fn normalize_trigger_preserves_key_case() {
        assert_eq!(normalize_trigger("Control+Return"), "CTRL+Return");
        assert_eq!(normalize_trigger("Ctrl+a"), "CTRL+a");
        assert_eq!(normalize_trigger("ctrl+a"), "CTRL+a");
    }

    #[test]
    fn normalize_trigger_handles_single_key() {
        assert_eq!(normalize_trigger("F1"), "F1");
        assert_eq!(normalize_trigger("Return"), "Return");
    }

    #[test]
    fn normalize_trigger_multi_modifier() {
        assert_eq!(normalize_trigger("Super+Shift+p"), "LOGO+SHIFT+p");
        assert_eq!(
            normalize_trigger("Ctrl+Alt+Super+Delete"),
            "CTRL+ALT+LOGO+Delete"
        );
    }

    #[test]
    fn normalize_trigger_tolerates_whitespace() {
        assert_eq!(normalize_trigger("Super + space"), "LOGO+space");
    }
}
