//! Spawn and lifecycle-manage the short-lived `lixun-preview`
//! child process.
//!
//! Invariants (per G2.8 decisions 5 + 11):
//!
//! - At most one preview process alive per daemon. On Space-press
//!   while a previous preview is running, SIGTERM it first and
//!   wait briefly for exit before spawning the replacement.
//! - The hit JSON tempfile is written here but **owned by the
//!   child**: `lixun-preview` deletes it immediately after read.
//!   That survives daemon crash. The daemon never cleans it up
//!   itself. Worst case on child-crash-before-read: a single
//!   small JSON leaks in `$XDG_RUNTIME_DIR`, which is tmpfs and
//!   clears on reboot.
//! - Env-var plumbing reuses `session_env::discover_gui_env()`
//!   for the same reason we needed it for the GUI: the preview
//!   must find `WAYLAND_DISPLAY` etc. under systemd user-manager.
//! - SIGTERM path reuses the same libc::kill primitive as
//!   `gui_control::terminate`; not factored into a shared helper
//!   because three lines is not worth a new module.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use lixun_core::Hit;
use lixun_ipc::gui::GuiCommand;
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::gui_control::GuiControl;
use crate::session_env;

const TERMINATE_GRACE: Duration = Duration::from_millis(200);
const KILL_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Exit code the preview binary uses when the user launched the
/// hit's default application from within the preview (Enter, Open
/// button). Kept in sync with `EXIT_CODE_LAUNCHED` in
/// `lixun-preview-bin`'s main.rs — that crate owns the value, this
/// constant mirrors it. Any other exit (0 = plain close, non-zero
/// crash, killed by signal) means the user dismissed the preview
/// without launching, and the launcher should reappear.
const PREVIEW_EXIT_CODE_LAUNCHED: i32 = 42;

#[derive(Default)]
struct PreviewState {
    pid: Option<u32>,
}

pub struct PreviewSpawner {
    state: Arc<Mutex<PreviewState>>,
    gui_control: Arc<GuiControl>,
}

impl PreviewSpawner {
    pub fn new(gui_control: Arc<GuiControl>) -> Self {
        Self {
            state: Arc::new(Mutex::new(PreviewState::default())),
            gui_control,
        }
    }

    /// Dispatch a new preview for `hit`. If a previous preview is
    /// still alive, SIGTERM it first and wait up to 200 ms for
    /// exit (escalating to SIGKILL if needed). Writes the Hit to a
    /// tempfile in `$XDG_RUNTIME_DIR` and spawns `lixun-preview
    /// --hit-json <path>`. Returns once the child has been spawned
    /// (does not wait for render).
    ///
    /// `monitor` is the connector name (`"eDP-1"`, `"DP-2"`, …)
    /// the launcher is currently on. When set it's exported as
    /// `LIXUN_PREVIEW_MONITOR`, which the preview binary reads and
    /// matches against `display.monitors()` to open on the same
    /// screen. When `None`, the preview binary falls back to
    /// pointer-based monitor selection — which is unreliable for
    /// a fresh process with no mapped surfaces yet (see BUG-1).
    pub async fn dispatch(&self, hit: Hit, monitor: Option<String>) -> anyhow::Result<()> {
        let mut state = self.state.lock().await;

        if let Some(old_pid) = state.pid.take()
            && process_alive(old_pid)
        {
            tracing::info!("preview_spawn: SIGTERM pid={}", old_pid);
            terminate(old_pid);
            if !wait_for_exit(old_pid, TERMINATE_GRACE).await {
                tracing::warn!(
                    "preview_spawn: pid={} survived SIGTERM, sending SIGKILL",
                    old_pid
                );
                kill_hard(old_pid);
            }
        }

        let hit_json = serde_json::to_vec_pretty(&hit)?;
        let tempfile_path = tempfile_path()?;
        tokio::fs::write(&tempfile_path, &hit_json).await?;

        let mut cmd = Command::new("lixun-preview");
        cmd.arg("--hit-json").arg(&tempfile_path);
        let env = session_env::discover_gui_env();
        for (k, v) in &env {
            cmd.env(k, v);
        }
        if let Some(connector) = &monitor {
            cmd.env("LIXUN_PREVIEW_MONITOR", connector);
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                // Spawn failed — no child will assume tempfile ownership,
                // so clean it up here to honour the "no leaks on error"
                // side of Decision 11. Ignore the remove error: if the
                // file is already gone we're done anyway.
                let _ = std::fs::remove_file(&tempfile_path);
                return Err(anyhow::Error::from(e)
                    .context(format!("spawning lixun-preview with {:?}", tempfile_path)));
            }
        };
        let pid = match child.id() {
            Some(pid) => pid,
            None => {
                let _ = std::fs::remove_file(&tempfile_path);
                anyhow::bail!("spawned lixun-preview has no pid");
            }
        };
        state.pid = Some(pid);
        tracing::info!(
            "preview_spawn: spawned lixun-preview pid={} hit_id={} hit_json={:?}",
            pid,
            hit.id.0,
            tempfile_path
        );

        let state_arc = Arc::clone(&self.state);
        let gui_control = Arc::clone(&self.gui_control);
        tokio::spawn(async move {
            let status = child.wait().await;
            // Decide whether the preview closed because the user
            // completed the task (launched the file → exit 42) or
            // because they dismissed it (Escape/Space/focus-loss →
            // exit 0, or a crash/signal). On dismissal we re-show
            // the launcher so its persisted session state becomes
            // visible again — the Spotlight cycle is
            // launcher → preview → back-to-launcher.
            let launched = matches!(
                &status,
                Ok(s) if s.code() == Some(PREVIEW_EXIT_CODE_LAUNCHED)
            );
            match &status {
                Ok(s) if !s.success() && !launched => {
                    tracing::warn!("preview_spawn: pid={} exited {:?}", pid, s);
                }
                Ok(s) => {
                    tracing::debug!(
                        "preview_spawn: pid={} exited {:?} launched={}",
                        pid,
                        s,
                        launched
                    );
                }
                Err(e) => {
                    tracing::warn!("preview_spawn: pid={} wait failed: {}", pid, e);
                }
            }
            {
                let mut s = state_arc.lock().await;
                if s.pid == Some(pid) {
                    s.pid = None;
                }
            }
            let post_cmd = if launched {
                GuiCommand::ClearSession
            } else {
                GuiCommand::Show
            };
            match gui_control.dispatch(post_cmd).await {
                Ok(_) => tracing::debug!(
                    "preview_spawn: dispatched {:?} to launcher after preview pid={} exit",
                    post_cmd,
                    pid
                ),
                Err(e) => tracing::warn!(
                    "preview_spawn: failed to dispatch {:?} after preview pid={}: {}",
                    post_cmd,
                    pid,
                    e
                ),
            }
        });

        Ok(())
    }
}

fn tempfile_path() -> anyhow::Result<PathBuf> {
    // Prefer XDG_RUNTIME_DIR (tmpfs, user-private, 0700 by default).
    // If absent, fall back to a per-user subdir under /tmp and force
    // 0700 on it so the Hit JSON — which may contain fragments of
    // file contents — isn't world-readable during the short window
    // it exists on disk.
    let runtime = match dirs::runtime_dir() {
        Some(p) => p,
        None => {
            let tmp_base =
                std::env::temp_dir().join(format!("lixun-{}", unsafe { libc::getuid() }));
            std::fs::create_dir_all(&tmp_base)?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp_base, std::fs::Permissions::from_mode(0o700))?;
            tmp_base
        }
    };
    std::fs::create_dir_all(&runtime)?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    Ok(runtime.join(format!("lixun-preview-hit-{}-{}.json", pid, nanos)))
}

fn terminate(pid: u32) {
    let _ = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
}

fn kill_hard(pid: u32) {
    let _ = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
}

async fn wait_for_exit(pid: u32, budget: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + budget;
    while tokio::time::Instant::now() < deadline {
        if !process_alive(pid) {
            return true;
        }
        tokio::time::sleep(KILL_POLL_INTERVAL).await;
    }
    !process_alive(pid)
}

fn process_alive(pid: u32) -> bool {
    let rc = unsafe { libc::kill(pid as i32, 0) };
    let kill_said_alive =
        rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    if !kill_said_alive {
        return false;
    }
    let status_path = format!("/proc/{}/status", pid);
    let Ok(status) = std::fs::read_to_string(&status_path) else {
        return false;
    };
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("State:") {
            let trimmed = rest.trim_start();
            if trimmed.starts_with('Z') || trimmed.starts_with('X') {
                return false;
            }
            return true;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use lixun_core::{Action, Category, DocId};

    fn fake_hit() -> Hit {
        Hit {
            id: DocId("fs:/tmp/demo.txt".into()),
            category: Category::File,
            title: "demo".into(),
            subtitle: "/tmp".into(),
            icon_name: None,
            kind_label: None,
            score: 1.0,
            action: Action::OpenFile {
                path: PathBuf::from("/tmp/demo.txt"),
            },
            extract_fail: false,
            sender: None,
            recipients: None,
            body: None,
            secondary_action: None,
            source_instance: String::new(),
            row_menu: lixun_core::RowMenuDef::empty(),
        }
    }

    #[test]
    fn tempfile_path_lives_under_runtime_dir_or_tmp() {
        let p = tempfile_path().unwrap();
        let s = p.to_string_lossy();
        assert!(
            s.contains("lixun-preview-hit-"),
            "tempfile name pattern preserved: {}",
            s
        );
        assert!(s.ends_with(".json"), "extension preserved: {}", s);
    }

    #[test]
    fn hit_round_trips_through_tempfile_json() {
        let hit = fake_hit();
        let bytes = serde_json::to_vec_pretty(&hit).unwrap();
        let decoded: Hit = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decoded.id.0, hit.id.0);
        assert_eq!(decoded.title, hit.title);
        assert!(matches!(decoded.action, Action::OpenFile { .. }));
    }
}
