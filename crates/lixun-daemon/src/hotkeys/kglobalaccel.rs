//! Native KDE global-shortcut backend talking directly to
//! `org.kde.kglobalaccel`.
//!
//! On a KDE/Plasma session the kglobalaccel service is hosted in-process
//! by KWin and is on the session bus before any user systemd unit
//! starts, so registration is reliable across cold logins (this is why
//! Konsole's Ctrl+Alt+T and Yakuake's F12 always work right after
//! reboot). Shortcut storage is the same `~/.config/kglobalshortcutsrc`
//! file the xdg-desktop-portal-kde wrapper writes, so existing
//! `[app.lixun.daemon]` entries are reused as-is.

use anyhow::{Context, Result, anyhow, bail};
use futures::StreamExt;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;
use zbus::Connection;
use zbus::zvariant::OwnedObjectPath;

const SERVICE: &str = "org.kde.kglobalaccel";
const ROOT_PATH: &str = "/kglobalaccel";
const ROOT_IFACE: &str = "org.kde.KGlobalAccel";
const COMPONENT_IFACE: &str = "org.kde.kglobalaccel.Component";

const COMPONENT_UNIQUE: &str = "app.lixun.daemon";
const COMPONENT_FRIENDLY: &str = "Lixun";
const ACTION_UNIQUE: &str = "toggle";
const ACTION_FRIENDLY: &str = "Toggle Lixun launcher";

const FLAG_SET_PRESENT: u32 = 0x2;

const QT_SHIFT: i32 = 0x0200_0000;
const QT_CTRL: i32 = 0x0400_0000;
const QT_ALT: i32 = 0x0800_0000;
const QT_META: i32 = 0x1000_0000;

const RESUBSCRIBE_MIN_BACKOFF: Duration = Duration::from_secs(2);
const RESUBSCRIBE_MAX_BACKOFF: Duration = Duration::from_secs(60);

pub(super) async fn run(
    preferred_trigger: String,
    _state_dir: PathBuf,
    tx: mpsc::Sender<()>,
) -> Result<()> {
    let conn = Connection::session()
        .await
        .context("connecting to session bus")?;

    let action_id = action_id();
    let root = zbus::Proxy::new(&conn, SERVICE, ROOT_PATH, ROOT_IFACE)
        .await
        .context("opening KGlobalAccel root proxy")?;

    let desired = parse_qt_key(&preferred_trigger)
        .with_context(|| format!("parsing trigger '{}'", preferred_trigger))?;

    // Forcefully drop any pre-existing registration before we declare
    // ourselves. When the entry already exists in
    // kglobalshortcutsrc — e.g. because xdg-desktop-portal-kde wrote it
    // during an earlier run — KWin's in-memory grab table can still
    // associate the key with the now-dead portal session. unRegister +
    // doRegister + setShortcut(SetPresent) forces KWin to refresh the
    // grab against our live D-Bus name. Ignore unRegister errors: the
    // action may simply not exist yet on a fresh install.
    let _ = root
        .call::<_, _, ()>("unRegister", &(action_id.clone(),))
        .await;
    let _: () = root
        .call("doRegister", &(action_id.clone(),))
        .await
        .context("KGlobalAccel.doRegister")?;

    // SetPresent (0x02) overwrites the stored binding with our desired
    // key and tells KWin to install the grab now. Using flags=0
    // (Autoloading) leaves stale in-memory state pointing at the dead
    // portal session — the shortcut appears bound on disk but keypresses
    // never reach us.
    let bound: Vec<i32> = root
        .call(
            "setShortcut",
            &(action_id.clone(), vec![desired], FLAG_SET_PRESENT),
        )
        .await
        .context("KGlobalAccel.setShortcut")?;
    let assigned = bound.first().copied().unwrap_or(0);
    if assigned == 0 {
        bail!(
            "KGlobalAccel.setShortcut returned no key for '{}'; the shortcut storage refused our binding",
            preferred_trigger
        );
    }
    if assigned != desired {
        // Another component owns this key in KWin's grab table. Yield
        // gracefully instead of fighting — the user will see the warning
        // and can rebind via System Settings.
        tracing::warn!(
            "hotkeys[kglobalaccel]: requested 0x{:08x} but KGlobalAccel assigned 0x{:08x} for '{}'; \
             another component owns this key (run `busctl --user call org.kde.kglobalaccel \
             /kglobalaccel org.kde.KGlobalAccel globalShortcutsByKey ai 1 {} u 0` to inspect)",
            desired,
            assigned,
            preferred_trigger,
            desired
        );
    }
    tracing::info!(
        "hotkeys[kglobalaccel]: shortcut '{}' bound (keys={:?})",
        preferred_trigger,
        bound
    );

    let component_path: OwnedObjectPath = root
        .call("getComponent", &(COMPONENT_UNIQUE,))
        .await
        .context("KGlobalAccel.getComponent")?;
    tracing::info!(
        "hotkeys[kglobalaccel]: component at {}",
        component_path.as_str()
    );

    supervisor_loop(conn, component_path, tx).await;
    Ok(())
}

async fn supervisor_loop(
    conn: Connection,
    component_path: OwnedObjectPath,
    tx: mpsc::Sender<()>,
) {
    let mut backoff = RESUBSCRIBE_MIN_BACKOFF;
    loop {
        let proxy = match zbus::Proxy::new(
            &conn,
            SERVICE,
            component_path.as_str(),
            COMPONENT_IFACE,
        )
        .await
        {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    "hotkeys[kglobalaccel]: component proxy failed: {}; retry in {:?}",
                    e,
                    backoff
                );
                tokio::time::sleep(backoff).await;
                backoff = std::cmp::min(backoff.saturating_mul(2), RESUBSCRIBE_MAX_BACKOFF);
                continue;
            }
        };

        let mut stream = match proxy.receive_signal("globalShortcutPressed").await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    "hotkeys[kglobalaccel]: receive_signal failed: {}; retry in {:?}",
                    e,
                    backoff
                );
                tokio::time::sleep(backoff).await;
                backoff = std::cmp::min(backoff.saturating_mul(2), RESUBSCRIBE_MAX_BACKOFF);
                continue;
            }
        };

        backoff = RESUBSCRIBE_MIN_BACKOFF;
        tracing::info!("hotkeys[kglobalaccel]: subscribed to globalShortcutPressed");

        while let Some(msg) = stream.next().await {
            let body = msg.body();
            let payload: Result<(String, String, i64), _> = body.deserialize();
            match payload {
                Ok((component, action, _ts))
                    if component == COMPONENT_UNIQUE && action == ACTION_UNIQUE =>
                {
                    if tx.send(()).await.is_err() {
                        tracing::info!(
                            "hotkeys[kglobalaccel]: receiver dropped, exiting listener"
                        );
                        return;
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(
                        "hotkeys[kglobalaccel]: bad signal payload: {}",
                        e
                    );
                }
            }
        }

        tracing::warn!(
            "hotkeys[kglobalaccel]: signal stream ended; resubscribing in {:?}",
            backoff
        );
        tokio::time::sleep(backoff).await;
        backoff = std::cmp::min(backoff.saturating_mul(2), RESUBSCRIBE_MAX_BACKOFF);
    }
}

fn action_id() -> Vec<String> {
    vec![
        COMPONENT_UNIQUE.to_string(),
        ACTION_UNIQUE.to_string(),
        COMPONENT_FRIENDLY.to_string(),
        ACTION_FRIENDLY.to_string(),
    ]
}

fn parse_qt_key(trigger: &str) -> Result<i32> {
    let mut modifiers: i32 = 0;
    let mut key: Option<i32> = None;
    for raw in trigger.split('+') {
        let part = raw.trim();
        if part.is_empty() {
            bail!("empty token in trigger");
        }
        match modifier_for(part) {
            Some(m) => modifiers |= m,
            None => {
                if key.is_some() {
                    bail!("multiple non-modifier keys in trigger '{}'", trigger);
                }
                key = Some(key_for(part).ok_or_else(|| {
                    anyhow!("unknown key '{}' in trigger '{}'", part, trigger)
                })?);
            }
        }
    }
    let key = key.ok_or_else(|| anyhow!("no key in trigger '{}'", trigger))?;
    Ok(modifiers | key)
}

fn modifier_for(token: &str) -> Option<i32> {
    let t = token.to_ascii_lowercase();
    match t.as_str() {
        "ctrl" | "control" => Some(QT_CTRL),
        "shift" => Some(QT_SHIFT),
        "alt" => Some(QT_ALT),
        "meta" | "super" | "logo" | "win" | "windows" | "cmd" | "command" => Some(QT_META),
        _ => None,
    }
}

fn key_for(token: &str) -> Option<i32> {
    if token.len() == 1 {
        let ch = token.chars().next().unwrap();
        if ch.is_ascii_alphabetic() {
            return Some(ch.to_ascii_uppercase() as i32);
        }
        if ch.is_ascii_digit() {
            return Some(ch as i32);
        }
    }
    let lower = token.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix('f') {
        if let Ok(n) = rest.parse::<u32>() {
            if (1..=35).contains(&n) {
                // Qt::Key_F1 = 0x01000030
                return Some(0x0100_0030 + (n as i32 - 1));
            }
        }
    }
    Some(match lower.as_str() {
        "space" => 0x20,
        "escape" | "esc" => 0x0100_0000,
        "tab" => 0x0100_0001,
        "backspace" => 0x0100_0003,
        "return" | "enter" => 0x0100_0004,
        "insert" => 0x0100_0006,
        "delete" | "del" => 0x0100_0007,
        "pause" => 0x0100_0008,
        "print" | "printscreen" => 0x0100_0009,
        "home" => 0x0100_0010,
        "end" => 0x0100_0011,
        "left" => 0x0100_0012,
        "up" => 0x0100_0013,
        "right" => 0x0100_0014,
        "down" => 0x0100_0015,
        "pageup" | "prior" => 0x0100_0016,
        "pagedown" | "next" => 0x0100_0017,
        "comma" => 0x2c,
        "period" => 0x2e,
        "slash" => 0x2f,
        "semicolon" => 0x3b,
        "minus" => 0x2d,
        "equal" => 0x3d,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_super_space() {
        assert_eq!(parse_qt_key("Super+space").unwrap(), 0x1000_0020);
    }

    #[test]
    fn parses_meta_space_case_insensitive() {
        assert_eq!(parse_qt_key("meta+SPACE").unwrap(), 0x1000_0020);
    }

    #[test]
    fn parses_ctrl_alt_t() {
        let expected = QT_CTRL | QT_ALT | ('T' as i32);
        assert_eq!(parse_qt_key("Ctrl+Alt+T").unwrap(), expected);
    }

    #[test]
    fn parses_f12() {
        assert_eq!(parse_qt_key("F12").unwrap(), 0x0100_0030 + 11);
    }

    #[test]
    fn rejects_unknown_key() {
        assert!(parse_qt_key("Super+banana").is_err());
    }

    #[test]
    fn rejects_modifier_only() {
        assert!(parse_qt_key("Super+Meta").is_err());
    }
}
