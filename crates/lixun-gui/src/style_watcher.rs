//! Filesystem watcher that drives live theme reloads.
//!
//! Watches three paths and emits a coalesced [`StyleEvent`] when any
//! of them changes:
//!
//! * `config.toml` — config-driven theme switch or blur toggle
//! * `${config_dir}/lixun/style.css` — user-wide CSS override
//! * `${config_dir}/lixun/themes/<active>/style.css` — active theme
//!
//! The watcher debounces inotify events with a 80 ms window because
//! editors (vim, VS Code, Helix) typically write a file via
//! truncate-then-write, which inotify reports as two or three back-to-
//! back events per save. Without coalescing we would call
//! `CssProvider::load_from_path` two or three times in rapid
//! succession on every save, which is wasteful and occasionally
//! races a partially written file.
//!
//! The notify thread is owned by the returned [`notify::RecommendedWatcher`].
//! The caller keeps the handle alive for as long as it wants events;
//! dropping it stops the watcher.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

/// Debounce window for editor save bursts. 80 ms covers truncate→write
/// → close-write inotify triples without adding perceptible reload lag.
const DEBOUNCE_MS: u64 = 80;

/// Coalesced category of file-system change emitted to the GTK main
/// thread. The pump in `window.rs` reads these from an
/// `async_channel::Receiver` and reloads the matching CSS provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StyleEvent {
    /// `config.toml` was modified. Caller reloads `Config` and applies
    /// the new theme / blur flag.
    ConfigChanged,
    /// `${config_dir}/lixun/style.css` was modified or created.
    UserCssChanged,
    /// The active theme's `style.css` was modified. Carries no path
    /// because the active theme is owned by the caller; if the theme
    /// selection itself changed the caller will see a
    /// `ConfigChanged` event first and rebuild the watcher with the
    /// new path.
    ThemeCssChanged,
}

/// Spawn the watcher and return the live `RecommendedWatcher` handle.
///
/// `config_path` is the absolute path to `config.toml`.
/// `user_css` is `${config_dir}/lixun/style.css` (need not exist —
/// the watcher tolerates absent files and starts emitting events as
/// soon as they appear). `active_theme_css` is `Some(path)` when a
/// theme is selected and its `style.css` exists, or `None` otherwise.
/// `tx` is the sender side of an `async_channel` whose receiver lives
/// on the GTK main thread.
///
/// Dropping the returned watcher stops the dedicated debounce thread
/// (it terminates when its internal channel closes).
pub fn spawn(
    config_path: PathBuf,
    user_css: PathBuf,
    active_theme_css: Option<PathBuf>,
    tx: async_channel::Sender<StyleEvent>,
) -> notify::Result<RecommendedWatcher> {
    let (raw_tx, raw_rx) = mpsc::channel::<RawHit>();

    let config_path_clone = config_path.clone();
    let user_css_clone = user_css.clone();
    let theme_css_clone = active_theme_css.clone();

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let event = match res {
            Ok(ev) => ev,
            Err(e) => {
                tracing::warn!("style watcher error: {e}");
                return;
            }
        };
        if !is_payload_change(&event.kind) {
            return;
        }
        for path in &event.paths {
            if let Some(hit) = classify(
                path,
                &config_path_clone,
                &user_css_clone,
                theme_css_clone.as_deref(),
            ) {
                let _ = raw_tx.send(hit);
            }
        }
    })?;

    // Watch the *parent directories* of each file so we still get
    // events when an editor renames-over the target (vim's default
    // write strategy) or when the user creates the file for the first
    // time. Watching a non-existent file directly would fail; watching
    // the parent never does as long as the config dir exists.
    //
    // Canonicalise each path before deriving its parent: when the file
    // is a symlink (e.g. dotfiles-managed `~/.config/lixun/config.toml`
    // pointing into a separate repo) inotify reports modifications
    // under the *resolved* parent directory, not the symlink's parent.
    // Watching the symlink parent silently misses every event. Canonical
    // parents capture writes via either path. Fall back to the lexical
    // parent when canonicalisation fails (e.g. the file does not yet
    // exist), so first-creation still works for greenfield setups.
    let mut watched_parents: Vec<PathBuf> = Vec::new();
    for path in [Some(&config_path), Some(&user_css), active_theme_css.as_ref()]
        .into_iter()
        .flatten()
    {
        let canonical_parent = path.canonicalize().ok().and_then(|p| p.parent().map(PathBuf::from));
        let lexical_parent = path.parent().map(PathBuf::from);
        for parent in canonical_parent.into_iter().chain(lexical_parent) {
            if watched_parents.iter().any(|p| p == &parent) {
                continue;
            }
            if parent.is_dir() {
                if let Err(e) = watcher.watch(&parent, RecursiveMode::NonRecursive) {
                    tracing::warn!("style watcher cannot watch {}: {e}", parent.display());
                } else {
                    tracing::debug!("style watcher watching {}", parent.display());
                    watched_parents.push(parent);
                }
            }
        }
    }

    thread::Builder::new()
        .name("lixun-style-notify".into())
        .spawn(move || debounce_loop(raw_rx, tx))
        .expect("spawn lixun-style-notify thread");

    Ok(watcher)
}

#[derive(Debug, Clone, Copy)]
enum RawHit {
    Config,
    UserCss,
    ThemeCss,
}

fn classify(
    path: &Path,
    config_path: &Path,
    user_css: &Path,
    theme_css: Option<&Path>,
) -> Option<RawHit> {
    if paths_equivalent(path, config_path) {
        return Some(RawHit::Config);
    }
    if paths_equivalent(path, user_css) {
        return Some(RawHit::UserCss);
    }
    if let Some(theme) = theme_css {
        if paths_equivalent(path, theme) {
            return Some(RawHit::ThemeCss);
        }
    }
    None
}

fn paths_equivalent(a: &Path, b: &Path) -> bool {
    // `canonicalize` requires the file to exist; during rename-over
    // events the path may briefly not. Fall back to lexical equality
    // when canonicalisation fails on either side.
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a == b,
    }
}

fn is_payload_change(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
    )
}

fn debounce_loop(rx: mpsc::Receiver<RawHit>, tx: async_channel::Sender<StyleEvent>) {
    let debounce = Duration::from_millis(DEBOUNCE_MS);
    let mut pending_config = false;
    let mut pending_user = false;
    let mut pending_theme = false;
    let mut last_seen: Option<Instant> = None;

    loop {
        let hit = match last_seen {
            None => match rx.recv() {
                Ok(h) => h,
                Err(_) => return,
            },
            Some(stamp) => {
                let elapsed = stamp.elapsed();
                if elapsed >= debounce {
                    flush(&mut pending_config, &mut pending_user, &mut pending_theme, &tx);
                    last_seen = None;
                    continue;
                }
                match rx.recv_timeout(debounce - elapsed) {
                    Ok(h) => h,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        flush(&mut pending_config, &mut pending_user, &mut pending_theme, &tx);
                        last_seen = None;
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        flush(&mut pending_config, &mut pending_user, &mut pending_theme, &tx);
                        return;
                    }
                }
            }
        };

        match hit {
            RawHit::Config => pending_config = true,
            RawHit::UserCss => pending_user = true,
            RawHit::ThemeCss => pending_theme = true,
        }
        last_seen = Some(Instant::now());
    }
}

fn flush(
    config: &mut bool,
    user: &mut bool,
    theme: &mut bool,
    tx: &async_channel::Sender<StyleEvent>,
) {
    // ConfigChanged is dispatched first because the caller reacts to
    // it by reloading the whole config (which may switch the active
    // theme path); flushing the CSS events after lets the caller
    // rebuild the watcher with the new path without losing edits.
    if *config {
        let _ = tx.send_blocking(StyleEvent::ConfigChanged);
        *config = false;
    }
    if *theme {
        let _ = tx.send_blocking(StyleEvent::ThemeCssChanged);
        *theme = false;
    }
    if *user {
        let _ = tx.send_blocking(StyleEvent::UserCssChanged);
        *user = false;
    }
}
