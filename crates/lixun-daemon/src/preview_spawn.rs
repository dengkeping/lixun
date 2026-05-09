//! Long-lived `lixun-preview` child process and its IPC channel.
//!
//! This module owns the daemon-side half of the live-preview
//! architecture. The contract with `lixun-preview-bin`:
//!
//! - **One warm preview process per daemon.** First Space-press
//!   cold-starts it; subsequent presses reuse the existing
//!   process. Preview self-quits after 60s idle (its own timer).
//!   On preview exit we transition to `Dead` and the next
//!   dispatch cold-starts again.
//! - **Daemon connects, preview binds.** The socket path is
//!   chosen here and passed to preview via `--socket-path`; we
//!   retry `UnixStream::connect` with backoff until the child
//!   binds or we give up.
//! - **Latest-wins backpressure.** No FIFO queue. While
//!   `Starting`, the latest desired `(hit, monitor)` overwrites
//!   any buffered prior one; once `Ready`, new commands go
//!   straight through the unbounded mpsc to the socket. If the
//!   writer task is gone, the send fails and we treat the
//!   process as dead.
//! - **Epoch is daemon-authoritative.** A monotonic `u64`
//!   counter, incremented per `dispatch`, tags every
//!   `ShowOrUpdate` and lets preview-side async work cancel
//!   stale widget mutations.
//! - **`Closed`/EOF from preview means "user dismissed or
//!   process gone".** Either way we dispatch `GuiCommand::Show`
//!   so the launcher reappears (Spotlight cycle:
//!   launcher → preview → back to launcher). Note: session
//!   clearing on launcher Escape is handled by the launcher
//!   itself via its `preview_mode_active` flag (Step 5) — the
//!   daemon no longer infers "launched vs. dismissed" from a
//!   child exit code.
//!
//! AGENTS.md modularity: this file never names a concrete
//! preview plugin. It speaks the abstract `PreviewCommand` /
//! `PreviewEvent` protocol defined in
//! `lixun_ipc::preview` and forwards `Hit` verbatim.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Context;
use futures::{SinkExt, StreamExt};
use lixun_core::Hit;
use lixun_ipc::gui::GuiCommand;
use lixun_ipc::preview::{
    DaemonPreviewCodec, PreviewCommand, PreviewEvent, preview_socket_path,
};
use tokio::net::UnixStream;
use tokio::process::Command;
use tokio::sync::{Mutex, mpsc};
use tokio_util::codec::Framed;

use crate::gui_control::GuiControl;
use crate::session_env;

/// SIGTERM grace before escalating to SIGKILL, reused from the
/// previous short-lived design. 200 ms is long enough for a
/// cooperating process to flush logs and short enough that a
/// stuck preview doesn't stall the daemon's shutdown path.
const TERMINATE_GRACE: Duration = Duration::from_millis(200);
const KILL_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Budget for the initial `UnixStream::connect` loop. Preview
/// typically binds its listener within a few hundred ms of
/// process spawn on cold start; we retry up to `CONNECT_BUDGET`
/// with `CONNECT_POLL_INTERVAL` between attempts. If we exceed
/// the budget we treat the spawn as failed.
const CONNECT_BUDGET: Duration = Duration::from_millis(1500);
const CONNECT_POLL_INTERVAL: Duration = Duration::from_millis(15);

/// State machine for the preview subprocess.
///
/// Transitions:
/// - `Dead → Starting` on the first `dispatch` call.
/// - `Starting → Ready` when the reader task decodes a
///   `PreviewEvent::Ready`. The buffered `latest_desired` (if
///   any) is drained into the socket at that moment.
/// - `Starting → Dead` on connect timeout / spawn error.
/// - `Ready → Dead` on reader-stream EOF, decode error, or
///   explicit `shutdown()` call.
///
/// The `pid` is kept in both `Starting` and `Ready` so
/// `shutdown()` can SIGTERM the child without waiting for the
/// state to progress.
enum PreviewLifecycle {
    Dead,
    Starting {
        pid: u32,
        socket_path: PathBuf,
        /// Latest-wins buffer used until `Ready` arrives. The
        /// `u64` is the epoch we already assigned; the reader
        /// task replays it verbatim so epoch ordering is
        /// preserved across the state transition.
        latest_desired: Option<(u64, Hit, Option<String>)>,
        /// Latest-wins buffer for the launcher's exported
        /// xdg-foreign-v2 handle. Drained alongside
        /// `latest_desired` when `Ready` arrives, and replayed
        /// as `PreviewCommand::SetParent` *before* the buffered
        /// `ShowOrUpdate` so the import resolves before the
        /// preview window is presented.
        latest_parent_handle: Option<String>,
        /// Latest-wins buffer for the launcher's on-screen
        /// geometry (monitor connector + rect). Replayed as
        /// `PreviewCommand::LauncherGeometry` after the buffered
        /// `ShowOrUpdate` so the preview can compute overlap with
        /// the launcher and request its hide on first paint.
        latest_launcher_geometry: Option<(String, i32, i32, i32, i32)>,
    },
    Ready {
        pid: u32,
        socket_path: PathBuf,
        /// Unbounded mpsc into the writer task; `send` failure
        /// means the writer has exited and the process is
        /// effectively dead.
        cmd_tx: mpsc::UnboundedSender<PreviewCommand>,
        last_used: tokio::time::Instant,
    },
}

impl Default for PreviewLifecycle {
    fn default() -> Self {
        PreviewLifecycle::Dead
    }
}

impl PreviewLifecycle {
    fn pid(&self) -> Option<u32> {
        match self {
            PreviewLifecycle::Dead => None,
            PreviewLifecycle::Starting { pid, .. } => Some(*pid),
            PreviewLifecycle::Ready { pid, .. } => Some(*pid),
        }
    }

    fn socket_path(&self) -> Option<&std::path::Path> {
        match self {
            PreviewLifecycle::Dead => None,
            PreviewLifecycle::Starting { socket_path, .. } => Some(socket_path.as_path()),
            PreviewLifecycle::Ready { socket_path, .. } => Some(socket_path.as_path()),
        }
    }
}

pub struct PreviewSpawner {
    state: Arc<Mutex<PreviewLifecycle>>,
    gui_control: Arc<GuiControl>,
    /// Monotonic counter assigned per `dispatch` call. Exposed
    /// to preview as `PreviewCommand::ShowOrUpdate { epoch }`.
    /// Daemon-authoritative: every mutation path must generate a
    /// fresh epoch so preview-side debouncing can reliably drop
    /// stale async work.
    epoch: AtomicU64,
    /// Nonce mixer used to pick unique socket paths across
    /// consecutive cold starts. `preview_socket_path` takes a
    /// `u32` tag; we combine daemon pid, a per-spawn counter,
    /// and wall-clock nanoseconds to avoid collisions on fast
    /// restart cycles.
    spawn_counter: AtomicU64,
}

impl PreviewSpawner {
    pub fn new(gui_control: Arc<GuiControl>) -> Self {
        Self {
            state: Arc::new(Mutex::new(PreviewLifecycle::default())),
            gui_control,
            epoch: AtomicU64::new(0),
            spawn_counter: AtomicU64::new(0),
        }
    }

    /// Dispatch a new preview for `hit`. Cold-starts the preview
    /// process if no warm instance exists; otherwise pushes a
    /// `ShowOrUpdate` through the live IPC channel.
    ///
    /// `monitor` is the connector name (`"eDP-1"`, `"DP-2"`, …)
    /// the launcher is currently on. Preview recomputes its
    /// target monitor on every update, so this is re-read per
    /// call (not just on cold start).
    ///
    /// Never blocks on preview rendering: returns as soon as the
    /// command is buffered (Starting) or handed off to the
    /// writer task (Ready).
    pub async fn dispatch(&self, hit: Hit, monitor: Option<String>) -> anyhow::Result<()> {
        let epoch = self.epoch.fetch_add(1, Ordering::Relaxed) + 1;
        let mut state = self.state.lock().await;

        match &mut *state {
            PreviewLifecycle::Dead => {
                // Cold start. Spawn the process, transition to
                // Starting, buffer the desired command; the
                // reader task will drain it when Ready arrives.
                let (pid, socket_path) = self
                    .spawn_preview_process()
                    .await
                    .context("cold-start lixun-preview")?;
                *state = PreviewLifecycle::Starting {
                    pid,
                    socket_path: socket_path.clone(),
                    latest_desired: Some((epoch, hit, monitor)),
                    latest_parent_handle: None,
                    latest_launcher_geometry: None,
                };
                // Supervisor task owns the socket, reader and
                // writer lifetimes; it locks `state` to promote
                // Starting → Ready and to fall back to Dead.
                self.spawn_supervisor_task(pid, socket_path);
                Ok(())
            }
            PreviewLifecycle::Starting { latest_desired, .. } => {
                // Latest-wins: overwrite any prior buffered
                // command with the newer one. The epoch is
                // always fresher because we just incremented it.
                *latest_desired = Some((epoch, hit, monitor));
                Ok(())
            }
            PreviewLifecycle::Ready {
                cmd_tx, last_used, ..
            } => {
                let cmd = PreviewCommand::ShowOrUpdate {
                    epoch,
                    hit: Box::new(hit),
                    monitor,
                };
                if cmd_tx.send(cmd).is_err() {
                    // Writer task gone — process effectively
                    // dead. Transition and cold-start anew by
                    // recursing once. We can't await recursive
                    // calls while holding the lock, so drop it
                    // first.
                    tracing::warn!(
                        "preview_spawn: writer channel closed, transitioning to Dead"
                    );
                    *state = PreviewLifecycle::Dead;
                    drop(state);
                    // Re-extract the hit and monitor is tricky
                    // after the send consumed them; we let the
                    // caller retry by surfacing an error. This
                    // is expected to be vanishingly rare (only
                    // on a race with the supervisor tearing
                    // down).
                    anyhow::bail!("preview writer channel closed, retry next dispatch");
                }
                *last_used = tokio::time::Instant::now();
                Ok(())
            }
        }
    }

    /// Hide the preview window without killing the process.
    ///
    /// Sent by the launcher when the user dismisses the preview
    /// (e.g. Escape with `preview_mode_active=true`). The preview
    /// hides its window and schedules a 60s idle timer; if no new
    /// dispatch arrives in that window the process self-quits.
    /// Until then it stays warm so the next Space/scrub is a hot
    /// path, not a cold spawn.
    ///
    /// Only meaningful when the preview is `Ready` — in `Starting`
    /// the window has not even been built yet, and in `Dead` there
    /// is no process to talk to. Both are no-ops; the launcher's
    /// local `preview_mode_active` reset is the source of truth
    /// for UI state, this method only adjusts the warm process.
    pub async fn hide(&self) -> anyhow::Result<()> {
        let epoch = self.epoch.fetch_add(1, Ordering::Relaxed) + 1;
        let mut state = self.state.lock().await;

        match &mut *state {
            PreviewLifecycle::Dead | PreviewLifecycle::Starting { .. } => Ok(()),
            PreviewLifecycle::Ready { cmd_tx, .. } => {
                let cmd = PreviewCommand::Hide { epoch };
                if cmd_tx.send(cmd).is_err() {
                    tracing::warn!(
                        "preview_spawn: writer channel closed during hide, transitioning to Dead"
                    );
                    *state = PreviewLifecycle::Dead;
                }
                Ok(())
            }
        }
    }

    /// Apply or update the launcher's exported xdg-foreign-v2
    /// handle on the live preview. Buffered until `Ready` if
    /// the preview is still warming up; sent immediately when
    /// already `Ready`. No-op when `Dead` — the launcher always
    /// dispatches a preview before exporting, so this branch is
    /// only reached on a teardown race and is logged at warn.
    pub async fn set_parent(&self, handle: String) {
        let mut state = self.state.lock().await;
        match &mut *state {
            PreviewLifecycle::Dead => {
                tracing::warn!(
                    "preview_spawn: SetParent received while Dead, dropping handle"
                );
            }
            PreviewLifecycle::Starting {
                latest_parent_handle,
                ..
            } => {
                *latest_parent_handle = Some(handle);
            }
            PreviewLifecycle::Ready { cmd_tx, .. } => {
                if cmd_tx
                    .send(PreviewCommand::SetParent { handle })
                    .is_err()
                {
                    tracing::warn!(
                        "preview_spawn: writer channel closed during set_parent"
                    );
                    *state = PreviewLifecycle::Dead;
                }
            }
        }
    }

    /// Clear any previously set xdg-foreign-v2 parent. Mirrors
    /// `set_parent`'s state handling.
    pub async fn clear_parent(&self) {
        let mut state = self.state.lock().await;
        match &mut *state {
            PreviewLifecycle::Dead => {}
            PreviewLifecycle::Starting {
                latest_parent_handle,
                ..
            } => {
                *latest_parent_handle = None;
            }
            PreviewLifecycle::Ready { cmd_tx, .. } => {
                if cmd_tx.send(PreviewCommand::ClearParent).is_err() {
                    tracing::warn!(
                        "preview_spawn: writer channel closed during clear_parent"
                    );
                    *state = PreviewLifecycle::Dead;
                }
            }
        }
    }

    pub async fn set_launcher_geometry(
        &self,
        monitor: String,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) {
        let mut state = self.state.lock().await;
        match &mut *state {
            PreviewLifecycle::Dead => {}
            PreviewLifecycle::Starting {
                latest_launcher_geometry,
                ..
            } => {
                *latest_launcher_geometry = Some((monitor, x, y, w, h));
            }
            PreviewLifecycle::Ready { cmd_tx, .. } => {
                if cmd_tx
                    .send(PreviewCommand::LauncherGeometry { monitor, x, y, w, h })
                    .is_err()
                {
                    tracing::warn!(
                        "preview_spawn: writer channel closed during set_launcher_geometry"
                    );
                    *state = PreviewLifecycle::Dead;
                }
            }
        }
    }

    /// Graceful teardown — SIGTERM the preview if alive, wait
    /// briefly, escalate to SIGKILL. Called by the daemon on
    /// shutdown. Safe to call when no preview is running.
    pub async fn shutdown(&self) {
        let (pid, socket_path) = {
            let mut state = self.state.lock().await;
            let pid = state.pid();
            let socket = state.socket_path().map(|p| p.to_path_buf());
            *state = PreviewLifecycle::Dead;
            (pid, socket)
        };
        if let Some(pid) = pid
            && process_alive(pid)
        {
            tracing::info!("preview_spawn: shutdown SIGTERM pid={}", pid);
            terminate(pid);
            if !wait_for_exit(pid, TERMINATE_GRACE).await {
                tracing::warn!(
                    "preview_spawn: shutdown pid={} survived SIGTERM, sending SIGKILL",
                    pid
                );
                kill_hard(pid);
            }
        }
        if let Some(path) = socket_path {
            // Best-effort unlink. Preview also unlinks on its
            // own exit path; duplicate cleanup is harmless.
            let _ = std::fs::remove_file(&path);
        }
    }

    /// Spawn the `lixun-preview` child and return its pid +
    /// chosen socket path. Does NOT wait for the socket to be
    /// bound — that's the supervisor task's job.
    async fn spawn_preview_process(&self) -> anyhow::Result<(u32, PathBuf)> {
        // Compose a unique socket tag: daemon pid xor per-spawn
        // counter xor low-32 of wall-clock nanoseconds. Avoids
        // collisions with previous preview instances even when
        // the daemon restarts quickly.
        let nonce_nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u32)
            .unwrap_or(0);
        let nonce_ctr = self.spawn_counter.fetch_add(1, Ordering::Relaxed) as u32;
        let daemon_pid = std::process::id();
        let tag = daemon_pid ^ nonce_ctr ^ nonce_nanos;
        let socket_path = preview_socket_path(tag).context("compute preview socket path")?;

        // Unlink any stale file at that path before letting
        // preview bind. `preview_socket_path` is chosen per-tag
        // so collisions are improbable, but a leftover from a
        // prior crashed run is still possible if tag reuses.
        let _ = std::fs::remove_file(&socket_path);

        let mut cmd = Command::new("lixun-preview");
        cmd.arg("--socket-path").arg(&socket_path);
        // Reuse the same session-env discovery we use for the
        // GUI: preview must find `WAYLAND_DISPLAY`,
        // `XDG_RUNTIME_DIR`, etc. under systemd user manager.
        let env = session_env::discover_gui_env();
        for (k, v) in &env {
            cmd.env(k, v);
        }
        // `kill_on_drop` is a last-resort safety net: if the
        // supervisor task panics before we transition to Ready,
        // the tokio `Child` handle dropped with it will SIGKILL
        // the preview instead of leaking a zombie process.
        cmd.kill_on_drop(true);

        let child = cmd
            .spawn()
            .with_context(|| format!("spawn lixun-preview --socket-path {:?}", socket_path))?;
        let pid = child
            .id()
            .context("spawned lixun-preview has no pid")?;

        // Supervisor owns the Child via
        // `self.spawn_supervisor_task`; we smuggle it through a
        // one-shot channel so the `async fn` boundary doesn't
        // force us to hand ownership through the return tuple.
        // Simpler: store Child in the supervisor's closure by
        // returning it alongside. But Rust makes that awkward
        // with the current signature; instead we let
        // `kill_on_drop` + the OS parent-death handling do the
        // cleanup if we drop the handle early. We retain the
        // handle in a tokio task that just waits on it and logs
        // exit status for observability.
        tokio::spawn(async move {
            let mut child = child;
            match child.wait().await {
                Ok(status) => tracing::debug!(
                    "preview_spawn: pid={} child.wait() returned {:?}",
                    pid,
                    status
                ),
                Err(e) => tracing::warn!(
                    "preview_spawn: pid={} child.wait() failed: {}",
                    pid,
                    e
                ),
            }
        });

        tracing::info!(
            "preview_spawn: spawned lixun-preview pid={} socket={:?}",
            pid,
            socket_path
        );
        Ok((pid, socket_path))
    }

    /// Spawn the supervisor task that connects to the preview's
    /// socket, promotes the state to `Ready`, and runs reader +
    /// writer loops until EOF or decode error.
    fn spawn_supervisor_task(&self, pid: u32, socket_path: PathBuf) {
        let state_arc = Arc::clone(&self.state);
        let gui_control = Arc::clone(&self.gui_control);
        tokio::spawn(async move {
            // Stage 1: connect with backoff. Preview binds its
            // listener within ~tens of ms of process spawn; we
            // retry until CONNECT_BUDGET expires.
            let stream = match connect_with_backoff(&socket_path).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(
                        "preview_spawn: connect to preview pid={} failed: {}",
                        pid,
                        e
                    );
                    reset_to_dead(&state_arc, pid, &socket_path, &gui_control).await;
                    return;
                }
            };

            // Stage 2: frame the stream and split into sink +
            // reader. Latest-wins semantics live in the writer
            // queue: unbounded mpsc, writer task drains
            // sequentially, `dispatch` overwrites nothing (it
            // can't — mpsc is FIFO) but every new `dispatch`
            // carries a fresh epoch so preview-side epoch
            // checks discard obsolete work in flight.
            let framed = Framed::new(stream, DaemonPreviewCodec::new());
            let (sink, mut reader) = framed.split();
            let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<PreviewCommand>();

            // Writer task: drain mpsc and push frames to socket.
            let writer_handle = tokio::spawn(async move {
                let mut sink = sink;
                while let Some(cmd) = cmd_rx.recv().await {
                    if let Err(e) = sink.send(cmd).await {
                        tracing::warn!("preview_spawn: sink write failed: {}", e);
                        break;
                    }
                }
            });

            // Stage 3: reader loop. First frame MUST be
            // `PreviewEvent::Ready`. While awaiting it we stay
            // in `Starting`; the `latest_desired` buffer may
            // grow (overwritten by concurrent `dispatch` calls).
            // Once Ready arrives we promote to Ready and drain
            // the buffer.
            while let Some(frame) = reader.next().await {
                let event = match frame {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::warn!(
                            "preview_spawn: pid={} decode error: {}",
                            pid,
                            e
                        );
                        break;
                    }
                };
                match event {
                    PreviewEvent::Ready { pid: preview_pid } => {
                        // Promote Starting → Ready and drain
                        // any buffered command in one atomic
                        // state-lock region.
                        let buffered = {
                            let mut s = state_arc.lock().await;
                            // Only promote if this supervisor
                            // still owns the current state; a
                            // shutdown() race could have
                            // already set Dead.
                            let (current_pid, current_socket) = match &*s {
                                PreviewLifecycle::Starting {
                                    pid, socket_path, ..
                                } => (*pid, socket_path.clone()),
                                PreviewLifecycle::Dead => {
                                    tracing::debug!(
                                        "preview_spawn: pid={} Ready arrived after shutdown, ignoring",
                                        pid
                                    );
                                    break;
                                }
                                PreviewLifecycle::Ready { .. } => {
                                    tracing::warn!(
                                        "preview_spawn: pid={} duplicate Ready event, ignoring",
                                        pid
                                    );
                                    continue;
                                }
                            };
                            if current_pid != pid {
                                tracing::warn!(
                                    "preview_spawn: pid mismatch on Ready (state={} me={}), bailing",
                                    current_pid,
                                    pid
                                );
                                break;
                            }
                            let old = std::mem::replace(&mut *s, PreviewLifecycle::Dead);
                            let (latest, parent, launcher_geom) = match old {
                                PreviewLifecycle::Starting {
                                    latest_desired,
                                    latest_parent_handle,
                                    latest_launcher_geometry,
                                    ..
                                } => (latest_desired, latest_parent_handle, latest_launcher_geometry),
                                _ => (None, None, None),
                            };
                            *s = PreviewLifecycle::Ready {
                                pid,
                                socket_path: current_socket,
                                cmd_tx: cmd_tx.clone(),
                                last_used: tokio::time::Instant::now(),
                            };
                            (latest, parent, launcher_geom)
                        };
                        tracing::info!(
                            "preview_spawn: Ready from preview pid={} (wire pid={})",
                            pid,
                            preview_pid
                        );
                        let (buffered, parent_handle, launcher_geom) = buffered;
                        if let Some(handle) = parent_handle
                            && cmd_tx
                                .send(PreviewCommand::SetParent { handle })
                                .is_err()
                        {
                            tracing::warn!(
                                "preview_spawn: pid={} writer gone before buffered SetParent",
                                pid
                            );
                            break;
                        }
                        if let Some((epoch, hit, monitor)) = buffered {
                            let cmd = PreviewCommand::ShowOrUpdate {
                                epoch,
                                hit: Box::new(hit),
                                monitor,
                            };
                            if cmd_tx.send(cmd).is_err() {
                                tracing::warn!(
                                    "preview_spawn: pid={} writer gone before buffered send",
                                    pid
                                );
                                break;
                            }
                        }
                        if let Some((monitor, x, y, w, h)) = launcher_geom {
                            if cmd_tx
                                .send(PreviewCommand::LauncherGeometry { monitor, x, y, w, h })
                                .is_err()
                            {
                                tracing::warn!(
                                    "preview_spawn: pid={} writer gone before buffered LauncherGeometry",
                                    pid
                                );
                                break;
                            }
                        }
                    }
                    PreviewEvent::Closed { epoch } => {
                        tracing::debug!(
                            "preview_spawn: pid={} Closed epoch={}",
                            pid,
                            epoch
                        );
                        // Preview hid itself (Escape/Space).
                        // Reveal launcher so the user can keep
                        // navigating. The warm process stays
                        // alive; we do NOT transition to Dead.
                        if let Err(e) = gui_control.dispatch(GuiCommand::Show).await {
                            tracing::warn!(
                                "preview_spawn: dispatch Show after Closed failed: {}",
                                e
                            );
                        }
                        // ExitPreviewMode MUST arrive after Show:
                        // it resets the launcher's preview_mode
                        // flag and grabs keyboard focus back to
                        // the search entry. Without it, the next
                        // arrow keypress would re-fire the debounce
                        // path and spawn a new preview window.
                        if let Err(e) = gui_control.dispatch(GuiCommand::ExitPreviewMode).await {
                            tracing::warn!(
                                "preview_spawn: dispatch ExitPreviewMode after Closed failed: {}",
                                e
                            );
                        }
                    }
                    PreviewEvent::Launched { epoch } => {
                        tracing::debug!(
                            "preview_spawn: pid={} Launched epoch={}",
                            pid,
                            epoch
                        );
                        if let Err(e) = gui_control.dispatch(GuiCommand::Hide).await {
                            tracing::warn!(
                                "preview_spawn: dispatch Hide after Launched failed: {}",
                                e
                            );
                        }
                        if let Err(e) = gui_control.dispatch(GuiCommand::ClearSession).await {
                            tracing::warn!(
                                "preview_spawn: dispatch ClearSession after Launched failed: {}",
                                e
                            );
                        }
                        if let Err(e) = gui_control.dispatch(GuiCommand::ExitPreviewMode).await {
                            tracing::warn!(
                                "preview_spawn: dispatch ExitPreviewMode after Launched failed: {}",
                                e
                            );
                        }
                    }
                    PreviewEvent::Error { epoch, msg } => {
                        tracing::warn!(
                            "preview_spawn: pid={} plugin error epoch={} msg={}",
                            pid,
                            epoch,
                            msg
                        );
                    }
                    PreviewEvent::ParentLost => {
                        tracing::debug!(
                            "preview_spawn: pid={} parent lost; awaiting next SetParent",
                            pid
                        );
                    }
                    PreviewEvent::SetLauncherVisible { visible } => {
                        let cmd = if visible {
                            GuiCommand::Show
                        } else {
                            GuiCommand::Hide
                        };
                        if let Err(e) = gui_control.dispatch(cmd).await {
                            tracing::warn!(
                                "preview_spawn: pid={} SetLauncherVisible({}) dispatch failed: {}",
                                pid,
                                visible,
                                e
                            );
                        }
                    }
                }
            }

            // Reader exited: EOF or decode error. Either way
            // the preview is gone. Tear down the writer task
            // by closing the mpsc, then reset state to Dead
            // and reveal the launcher.
            drop(cmd_tx);
            let _ = writer_handle.await;
            reset_to_dead(&state_arc, pid, &socket_path, &gui_control).await;
        });
    }
}

/// Retry `UnixStream::connect` until the socket is reachable or
/// `CONNECT_BUDGET` elapses. Returns the connected stream or an
/// error explaining which phase failed.
async fn connect_with_backoff(path: &std::path::Path) -> anyhow::Result<UnixStream> {
    let deadline = tokio::time::Instant::now() + CONNECT_BUDGET;
    let mut last_err: Option<std::io::Error> = None;
    while tokio::time::Instant::now() < deadline {
        match UnixStream::connect(path).await {
            Ok(s) => return Ok(s),
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(CONNECT_POLL_INTERVAL).await;
            }
        }
    }
    match last_err {
        Some(e) => Err(anyhow::Error::from(e)
            .context(format!("connect to preview socket {:?} timed out", path))),
        None => anyhow::bail!("connect loop exited without attempting (budget=0?)"),
    }
}

/// Reset the lifecycle to `Dead` and notify the launcher. Called
/// on connect failure, reader EOF, or decode error. Idempotent:
/// if `shutdown()` has already moved the state to Dead, we just
/// log and still dispatch `Show` so the launcher reappears.
async fn reset_to_dead(
    state: &Arc<Mutex<PreviewLifecycle>>,
    pid: u32,
    socket_path: &std::path::Path,
    gui_control: &Arc<GuiControl>,
) {
    {
        let mut s = state.lock().await;
        // Only clear if the state still refers to our pid; if
        // a newer preview has already been cold-started we
        // must leave it alone.
        let owned_by_us = matches!(
            &*s,
            PreviewLifecycle::Starting { pid: p, .. } | PreviewLifecycle::Ready { pid: p, .. }
                if *p == pid
        );
        if owned_by_us {
            *s = PreviewLifecycle::Dead;
        }
    }
    // Best-effort SIGTERM on the pid — preview may already be
    // gone (which is why we got here), in which case libc::kill
    // returns ESRCH and we ignore it.
    if process_alive(pid) {
        terminate(pid);
    }
    // Unlink the per-process socket. Preview's own exit path
    // also does this; duplicate cleanup is harmless.
    let _ = std::fs::remove_file(socket_path);
    if let Err(e) = gui_control.dispatch(GuiCommand::Show).await {
        tracing::warn!(
            "preview_spawn: dispatch Show after pid={} exit failed: {}",
            pid,
            e
        );
    }
    // Preview process died unexpectedly (crash, SIGKILL, etc.)
    // — also leave preview mode so the launcher is usable again.
    if let Err(e) = gui_control.dispatch(GuiCommand::ExitPreviewMode).await {
        tracing::warn!(
            "preview_spawn: dispatch ExitPreviewMode after pid={} exit failed: {}",
            pid,
            e
        );
    }
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
            mime: None,
        }
    }

    #[test]
    fn lifecycle_default_is_dead() {
        let s = PreviewLifecycle::default();
        assert!(matches!(s, PreviewLifecycle::Dead));
        assert!(s.pid().is_none());
        assert!(s.socket_path().is_none());
    }

    #[test]
    fn lifecycle_starting_exposes_pid_and_socket() {
        let s = PreviewLifecycle::Starting {
            pid: 1234,
            socket_path: PathBuf::from("/tmp/lixun-preview-1234.sock"),
            latest_desired: Some((7, fake_hit(), Some("eDP-1".into()))),
            latest_parent_handle: Some("export-handle-xyz".into()),
        };
        assert_eq!(s.pid(), Some(1234));
        assert_eq!(
            s.socket_path().unwrap().to_string_lossy(),
            "/tmp/lixun-preview-1234.sock"
        );
    }

    #[test]
    fn hit_still_serialises_after_box_wrap() {
        // Smoke test: the Box<Hit> boundary in PreviewCommand
        // must not break serde round-trip (that's what the
        // preview channel carries on ShowOrUpdate). If this
        // fails the entire dispatch path is broken; easier to
        // catch here than in an integration test.
        let boxed: Box<Hit> = Box::new(fake_hit());
        let bytes = serde_json::to_vec(&boxed).unwrap();
        let decoded: Box<Hit> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decoded.id.0, "fs:/tmp/demo.txt");
        assert!(matches!(decoded.action, Action::OpenFile { .. }));
    }
}
