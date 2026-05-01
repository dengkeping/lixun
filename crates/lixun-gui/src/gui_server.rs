//! GUI-side command server.
//!
//! Owns `$XDG_RUNTIME_DIR/lixun-gui.sock`. Accepts one `GuiCommand` per
//! connection, dispatches it onto the GTK main thread via
//! `glib::spawn_future_local` + `async_channel`, and writes back a
//! `GuiResponse`. This module is the steady-state control plane for
//! service mode (G1.6): the daemon stays a client here, the GUI lives
//! across toggles instead of being spawned per-show.
//!
//! Thread model:
//! - Main (GTK) thread installs one `async_channel::Receiver` future
//!   via `glib::spawn_future_local`. Futures pinned to a MainContext
//!   run with GTK widget access (the controller is `!Send`).
//! - Dedicated `std::thread` runs the blocking accept loop. It sends
//!   `ControllerRequest { cmd, reply }` values across the channel.
//!   `GuiCommand` and `async_channel::Sender<GuiResponse>` are both
//!   `Send`, so no `!Send` state ever crosses the thread boundary.
//! - Each request carries its own oneshot reply channel
//!   (`std::sync::mpsc::sync_channel(1)`), so there's no head-of-line
//!   blocking and the accept thread can time out a wedged GTK loop via
//!   `recv_timeout(500ms)`.

use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::time::Duration;

use lixun_ipc::gui::{GuiCommand, GuiResponse, gui_socket_path, read_frame_sync, write_frame_sync};

use crate::window::LauncherController;

pub(crate) struct ControllerRequest {
    pub(crate) cmd: GuiCommand,
    pub(crate) reply: SyncSender<GuiResponse>,
}

/// Bring up the command server. Must be called from the GTK main thread
/// (it spawns a `glib::spawn_future_local` future). Returns once setup
/// is done; the listener thread runs for the remaining process lifetime.
///
/// Fails the whole process if the socket path is already owned by
/// another running `lixun-gui` (probed via `GuiCommand::Ping`). Stale
/// socket files from crashed peers are unlinked.
pub(crate) fn start(controller: std::rc::Rc<LauncherController>) -> anyhow::Result<()> {
    let path = gui_socket_path();

    if path.exists() {
        let probe = UnixStream::connect(&path).ok().and_then(|mut s| {
            let _ = s.set_read_timeout(Some(Duration::from_millis(200)));
            let _ = s.set_write_timeout(Some(Duration::from_millis(200)));
            write_frame_sync(&mut s, &GuiCommand::Ping).ok()?;
            read_frame_sync::<_, GuiResponse>(&mut s).ok()
        });
        if probe.is_some() {
            tracing::error!(
                "gui_server: another lixun-gui already owns {:?}; exiting",
                path
            );
            std::process::exit(0);
        }
        let _ = std::fs::remove_file(&path);
    }

    let listener =
        UnixListener::bind(&path).map_err(|e| anyhow::anyhow!("bind {:?}: {}", path, e))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| anyhow::anyhow!("chmod {:?}: {}", path, e))?;
    set_cloexec(&listener);

    let (tx, rx) = async_channel::unbounded::<ControllerRequest>();

    glib::spawn_future_local(async move {
        while let Ok(req) = rx.recv().await {
            let resp = dispatch(&controller, req.cmd);
            let _ = req.reply.send(resp);
        }
        tracing::debug!("gui_server: controller dispatch future exited");
    });

    std::thread::Builder::new()
        .name("lixun-gui-server".into())
        .spawn(move || accept_loop(listener, tx))
        .map_err(|e| anyhow::anyhow!("spawn gui server thread: {}", e))?;

    tracing::info!("gui_server: listening on {:?}", path);
    Ok(())
}

fn dispatch(controller: &LauncherController, cmd: GuiCommand) -> GuiResponse {
    match cmd {
        GuiCommand::Show => GuiResponse::Ok {
            visible: controller.show(),
        },
        GuiCommand::Hide => GuiResponse::Ok {
            visible: controller.hide(),
        },
        GuiCommand::Toggle => GuiResponse::Ok {
            visible: controller.toggle(),
        },
        GuiCommand::Ping => GuiResponse::Ok {
            visible: controller.is_visible(),
        },
        GuiCommand::Quit => {
            controller.quit();
            GuiResponse::Ok { visible: false }
        }
        GuiCommand::ClearSession => {
            controller.drop_cached_session();
            GuiResponse::Ok {
                visible: controller.is_visible(),
            }
        }
        GuiCommand::ExitPreviewMode => {
            controller.exit_preview_mode();
            GuiResponse::Ok {
                visible: controller.is_visible(),
            }
        }
    }
}

fn accept_loop(listener: UnixListener, tx: async_channel::Sender<ControllerRequest>) {
    loop {
        let (mut stream, _) = match listener.accept() {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("gui_server: accept failed: {e}");
                continue;
            }
        };
        let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
        let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));

        let cmd: GuiCommand = match read_frame_sync(&mut stream) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("gui_server: decode failed: {e}");
                continue;
            }
        };

        let (reply_tx, reply_rx) = sync_channel::<GuiResponse>(1);
        if tx
            .send_blocking(ControllerRequest {
                cmd,
                reply: reply_tx,
            })
            .is_err()
        {
            tracing::warn!("gui_server: controller channel closed; exiting");
            return;
        }

        let resp = match reply_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(r) => r,
            Err(_) => {
                tracing::warn!("gui_server: gtk thread unresponsive");
                GuiResponse::Error("gtk thread unresponsive".into())
            }
        };

        if let Err(e) = write_frame_sync(&mut stream, &resp) {
            tracing::warn!("gui_server: write failed: {e}");
        }

        if matches!(cmd, GuiCommand::Quit) {
            tracing::info!("gui_server: Quit dispatched; accept loop exiting");
            return;
        }
    }
}

fn set_cloexec(listener: &UnixListener) {
    let fd = listener.as_raw_fd();
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
    }
}
