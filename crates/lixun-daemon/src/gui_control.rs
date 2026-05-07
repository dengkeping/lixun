//! Daemon-side orchestration of the `lixun-gui` process for service mode
//! (G1.6 commit 3). Unifies the hotkey and IPC toggle paths behind one
//! `dispatch` entrypoint so we implement probe-before-spawn, retry-until-
//! ready, zombie recovery, and graceful quit exactly once.
//!
//! The daemon no longer tracks a `visible: bool`. The GUI is the single
//! source of truth; every reply carries `visible` so callers learn the
//! resulting state in one round trip. See Oracle review
//! `ses_252098efeffeyaqBhzgvv4TuaI` for the full rationale.

use std::sync::Arc;
use std::time::Duration;

use lixun_ipc::gui::{
    GuiCommand, GuiResponse, gui_socket_path, read_frame_async, write_frame_async,
};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

use crate::session_env;

const CONNECT_RETRY_INTERVAL: Duration = Duration::from_millis(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(1);
const QUIT_GRACE_PERIOD: Duration = Duration::from_millis(500);

#[derive(Debug, Default)]
struct State {
    pid: Option<u32>,
}

#[derive(Default)]
pub struct GuiControl {
    state: Mutex<State>,
}

impl GuiControl {
    pub fn new() -> Self {
        Self::default()
    }

    /// Currently tracked GUI pid (may be `None` if the process has
    /// exited or was adopted from a previous daemon instance without
    /// a spawn record).
    pub async fn pid(&self) -> Option<u32> {
        self.state.lock().await.pid
    }

    /// Send a command to the GUI process, (re)spawning if needed.
    /// Returns the `GuiResponse` from the GUI. The error variant of
    /// `GuiResponse` is NOT a transport failure — it's a semantic
    /// failure (e.g. GTK thread unresponsive). Transport failures are
    /// `Err`.
    pub async fn dispatch(self: &Arc<Self>, cmd: GuiCommand) -> anyhow::Result<GuiResponse> {
        let mut state = self.state.lock().await;

        let socket_alive = self.socket_responds().await;
        if !socket_alive {
            if let Some(pid) = state.pid.take() {
                tracing::warn!("gui_control: socket unresponsive for pid={}, SIGTERM", pid);
                terminate(pid);
            }
        } else if state.pid.is_none() {
            // Socket responds but we have no pid record (daemon restart
            // adopting an already-running lixun-gui). Send the command
            // directly without re-spawning; we just don't track the pid.
            tracing::info!("gui_control: adopting running lixun-gui via existing socket");
            drop(state);
            return send_command(cmd, COMMAND_TIMEOUT).await;
        }

        if state.pid.is_none() {
            let pid = self.spawn(Arc::clone(self))?;
            state.pid = Some(pid);
            drop(state);
            wait_for_ready().await?;
            return send_command(cmd, COMMAND_TIMEOUT).await;
        }

        drop(state);
        send_command(cmd, COMMAND_TIMEOUT).await
    }

    /// Daemon shutdown path. Three-stage escalation: (1) send
    /// `GuiCommand::Quit` and poll for graceful exit up to 500 ms;
    /// (2) SIGTERM and poll for exit another 500 ms; (3) SIGKILL.
    /// Always completes within ~1 s. Errors are logged, not returned.
    pub async fn shutdown(&self) {
        let pid = {
            let mut state = self.state.lock().await;
            state.pid.take()
        };
        let Some(pid) = pid else {
            tracing::debug!("gui_control: shutdown with no gui pid, nothing to do");
            return;
        };

        let quit_result = tokio::time::timeout(
            QUIT_GRACE_PERIOD,
            send_command(GuiCommand::Quit, QUIT_GRACE_PERIOD),
        )
        .await;

        if wait_for_exit(pid, QUIT_GRACE_PERIOD).await {
            tracing::info!("gui_control: gui pid={} exited gracefully", pid);
            return;
        }

        tracing::warn!(
            "gui_control: gui pid={} did not exit within {}ms after Quit (quit_result={:?}), sending SIGTERM",
            pid,
            QUIT_GRACE_PERIOD.as_millis(),
            quit_result
                .as_ref()
                .map(|r| r.as_ref().map(|_| "ok").unwrap_or("transport-err")),
        );
        terminate(pid);

        if wait_for_exit(pid, QUIT_GRACE_PERIOD).await {
            tracing::info!("gui_control: gui pid={} exited after SIGTERM", pid);
            return;
        }

        tracing::error!(
            "gui_control: gui pid={} survived SIGTERM, sending SIGKILL",
            pid
        );
        kill_hard(pid);
    }

    /// Called by the spawn-watcher task when the GUI process exits
    /// unexpectedly (any exit outside of `shutdown`).
    async fn on_child_exit(&self, pid: u32) {
        let mut state = self.state.lock().await;
        if state.pid == Some(pid) {
            state.pid = None;
        }
    }

    async fn socket_responds(&self) -> bool {
        let path = gui_socket_path();
        if !path.exists() {
            return false;
        }
        let connect = tokio::time::timeout(Duration::from_millis(200), UnixStream::connect(&path));
        let Ok(Ok(mut stream)) = connect.await else {
            return false;
        };
        if write_frame_async(&mut stream, &GuiCommand::Ping)
            .await
            .is_err()
        {
            return false;
        }
        let read = tokio::time::timeout(
            Duration::from_millis(500),
            read_frame_async::<_, GuiResponse>(&mut stream),
        );
        matches!(read.await, Ok(Ok(_)))
    }

    fn spawn(&self, self_arc: Arc<GuiControl>) -> anyhow::Result<u32> {
        let mut cmd = tokio::process::Command::new("lixun-gui");
        let env = session_env::discover_gui_env();
        for (k, v) in &env {
            cmd.env(k, v);
        }
        // Flag the spawned GUI that its initial visibility is
        // managed by the daemon, not by the GUI itself. Without
        // this the GUI calls controller.show() inside
        // build_window, races the daemon's post-spawn Toggle
        // command (which arrives ~a few ms later, once the gui
        // socket is bound), and toggle() then hides the window
        // because is_visible() is already true. Net effect: the
        // first Super+Space after a daemon restart appears to do
        // nothing and the user has to press it twice.
        cmd.env("LIXUN_GUI_SERVICE_SPAWN", "1");
        let mut child = cmd.spawn()?;
        let pid = child
            .id()
            .ok_or_else(|| anyhow::anyhow!("spawned lixun-gui has no pid"))?;
        tracing::info!(
            "gui_control: spawned lixun-gui pid={} env_keys={:?}",
            pid,
            env.keys().collect::<Vec<_>>()
        );
        tokio::spawn(async move {
            let status = child.wait().await;
            match &status {
                Ok(s) if !s.success() => {
                    tracing::warn!("gui_control: lixun-gui pid={} exited {:?}", pid, s);
                }
                Ok(s) => {
                    tracing::debug!("gui_control: lixun-gui pid={} exited ok {:?}", pid, s);
                }
                Err(e) => {
                    tracing::warn!("gui_control: lixun-gui pid={} wait failed: {}", pid, e);
                }
            }
            self_arc.on_child_exit(pid).await;
        });
        Ok(pid)
    }
}

async fn wait_for_ready() -> anyhow::Result<()> {
    let path = gui_socket_path();
    let deadline = tokio::time::Instant::now() + CONNECT_TIMEOUT;
    loop {
        if let Ok(mut stream) = UnixStream::connect(&path).await
            && write_frame_async(&mut stream, &GuiCommand::Ping)
                .await
                .is_ok()
            && read_frame_async::<_, GuiResponse>(&mut stream)
                .await
                .is_ok()
        {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "gui_control: lixun-gui did not become ready within {:?}",
                CONNECT_TIMEOUT
            );
        }
        tokio::time::sleep(CONNECT_RETRY_INTERVAL).await;
    }
}

async fn send_command(cmd: GuiCommand, timeout: Duration) -> anyhow::Result<GuiResponse> {
    let path = gui_socket_path();
    let fut = async {
        let mut stream = UnixStream::connect(&path).await?;
        write_frame_async(&mut stream, &cmd).await?;
        let resp: GuiResponse = read_frame_async(&mut stream).await?;
        Ok::<GuiResponse, anyhow::Error>(resp)
    };
    tokio::time::timeout(timeout, fut)
        .await
        .map_err(|_| anyhow::anyhow!("gui_control: command {:?} timed out", cmd))?
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
        tokio::time::sleep(Duration::from_millis(25)).await;
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
