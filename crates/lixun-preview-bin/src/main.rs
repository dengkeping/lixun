//! `lixun-preview` binary.
//!
//! Long-lived companion process for `lixund`. Spawned lazily on the
//! first preview request for a launcher session, then kept warm
//! across Space toggles and selection changes until either:
//!
//! * the daemon sends `PreviewCommand::Close{epoch}` and 60s of idle
//!   elapses with no new `ShowOrUpdate`, or
//! * the daemon disconnects (EOF on the IPC socket — treated as
//!   "daemon gone, self-quit").
//!
//! Speaks `lixun_ipc::preview::{PreviewCommand, PreviewEvent}` over
//! a per-process Unix socket whose path is passed via
//! `--socket-path`. The daemon is the only client; the listener
//! accepts exactly one connection and then drops the listener so any
//! second connection attempt fails fast.
//!
//! Concurrency model: GTK runs on the main thread; two `std::thread`
//! workers (reader + writer) own the split halves of the
//! `UnixStream` and shuttle frames through `async_channel`s. The
//! GTK side picks them up via `glib::spawn_future_local`. There is
//! no tokio runtime here — the preview process must stay small and
//! responsive, and tokio would conflict with the GTK main loop.

use std::cell::{Cell, RefCell};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use gtk::gio::ApplicationFlags;
use gtk::glib;
use gtk::prelude::*;
use lixun_core::Hit;
use lixun_ipc::preview::{
    PreviewCommand, PreviewEvent, ScrollDirection, read_frame_sync, write_frame_sync,
};
use lixun_preview::{
    PreviewPlugin, PreviewPluginCfg, ScrollRequest, SizingPreference, UPDATE_UNSUPPORTED,
    install_user_css, select_plugin,
};

use lixun_preview_bundle as _;

#[cfg(feature = "stub")]
mod stub;
mod wayland_xdg_foreign;

const APP_ID: &str = "app.lixun.preview";
const DEFAULT_WIDTH: i32 = 960;
const DEFAULT_HEIGHT: i32 = 720;
const MIN_WIDTH: i32 = 600;
const MIN_HEIGHT: i32 = 400;

/// Gap between the layer-shell preview surface and the right monitor
/// edge. Mirrored by the launcher's slide math
/// (`lixun-gui/src/preview_layout.rs::PREVIEW_EDGE_MARGIN`) — the
/// two processes never exchange layout decisions, they only agree on
/// the same arithmetic.
const PREVIEW_EDGE_MARGIN: i32 = 16;

/// Launcher width floor for the fits-beside predicate. Mirrors
/// `lixun-gui/src/preview_layout.rs::LAUNCHER_MIN_WIDTH`.
const LAUNCHER_MIN_WIDTH: i32 = 480;

/// Per-side breathing room the launcher needs inside the left
/// column. Mirrors
/// `lixun-gui/src/preview_layout.rs::LAUNCHER_COLUMN_GAP`.
const LAUNCHER_COLUMN_GAP: i32 = 16;

/// How long the preview process stays warm after the user dismisses
/// the preview (Escape, Space, or daemon-driven Close). Mirrors the
/// macOS QuickLook daemon (`quicklookd`) policy: cold-start cost is
/// paid once per Space session, but a brief lull does not kill the
/// process. After this elapses with no new `ShowOrUpdate`, the
/// process self-quits and the next Space pays cold-start again.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a programmatic `set_default_size` is allowed to echo
/// back through `notify::default-width` / `notify::default-height`
/// before those notifies are again interpreted as user-driven
/// resizes. Covers the asynchronous configure round-trip through the
/// compositor (request → configure → allocate → property update),
/// which lands well inside one frame under normal load; the margin
/// absorbs a busy compositor.
const PROGRAMMATIC_RESIZE_SETTLE: Duration = Duration::from_millis(250);

/// How long the transient launch-failure strip (P10) stays mounted
/// in the header before removing itself.
const ERROR_STRIP_TIMEOUT: Duration = Duration::from_secs(4);

/// Character cap for the inline launch-failure strip. The full
/// message stays readable via the strip's tooltip.
const ERROR_STRIP_MAX_CHARS: usize = 160;

#[derive(Parser, Debug)]
#[command(
    name = "lixun-preview",
    about = "Long-lived preview window driven over IPC by lixund"
)]
struct Args {
    /// Path to the per-process Unix socket the daemon will connect
    /// to. The daemon picks the path (typically
    /// `$XDG_RUNTIME_DIR/lixun-preview-{pid}.sock`) and passes it
    /// here. The preview process owns the socket file: it binds it,
    /// chmods it 0600, and is responsible for unlinking it on exit.
    ///
    /// Mutually exclusive with `--standalone-file`.
    #[arg(
        long,
        required_unless_present = "standalone_file",
        conflicts_with = "standalone_file"
    )]
    socket_path: Option<PathBuf>,

    /// Debug-only: open the given file directly, bypassing the daemon
    /// IPC layer entirely. Synthesises a generic `Hit` from the path
    /// and runs the normal `ShowOrUpdate` dispatch (plugin selection
    /// via `match_score`, host mounts whatever widget the matching
    /// plugin returns). Used for profiling under tools like
    /// `heaptrack` or `valgrind massif` where the daemon-mediated
    /// path would obscure allocations.
    ///
    /// The flag is plugin-agnostic — the host does not branch on
    /// file type; the matching `PreviewPlugin` is resolved via the
    /// same machinery used for daemon-dispatched hits.
    #[arg(long)]
    standalone_file: Option<PathBuf>,
}

/// Marker the reader thread sends to the GTK side when the daemon
/// closes the socket. The main loop treats it as authoritative
/// "daemon is gone, shut down" and calls `app.quit()`.
enum InboundMsg {
    Cmd(PreviewCommand),
    DaemonGone,
}

/// All long-lived UI state held on the GTK thread. The renderer
/// path mutates these from inside `glib::spawn_future_local`
/// closures; nothing here crosses thread boundaries (no Send/Sync).
#[derive(Default)]
struct PreviewState {
    /// Monotonically-increasing sequence number set by the daemon on
    /// every `ShowOrUpdate`. Stored on each command and re-checked
    /// before any async result commits a widget mutation. Plugins
    /// that schedule heavy work via `glib::spawn_future_local` MUST
    /// capture this value at scheduling time and re-check before
    /// touching the widget tree.
    current_epoch: Cell<u64>,
    current_plugin_id: RefCell<Option<String>>,
    current_plugin: RefCell<Option<Rc<dyn PreviewPlugin>>>,
    current_hit: RefCell<Option<Hit>>,
    current_widget: RefCell<Option<gtk::Widget>>,
    /// True when `current_widget` is mounted directly into `vbox`
    /// (plugin returned `SizingPreference::OwnsScroll`); false when
    /// it lives inside `content_scroll`. Drives the cleanup branch
    /// on the next mount so we never leave an orphan widget behind.
    current_widget_owns_scroll: Cell<bool>,
    window: RefCell<Option<gtk::ApplicationWindow>>,
    vbox: RefCell<Option<gtk::Box>>,
    header_box: RefCell<Option<gtk::Box>>,
    content_scroll: RefCell<Option<gtk::ScrolledWindow>>,
    /// Bottom keyboard-hint strip (P7), rebuilt per `ShowOrUpdate`
    /// from the active plugin's `capabilities()`.
    hints_label: RefCell<Option<gtk::Label>>,
    /// Transient launch-failure strip currently mounted in the
    /// header (P10). Removed by its own timeout or by the next
    /// `rebuild_header`, whichever fires first.
    error_strip: RefCell<Option<gtk::Label>>,
    /// Sizing class applied by the most recent `apply_sizing` call,
    /// `None` until the first `ShowOrUpdate`. An unchanged class on
    /// an already-mapped window skips the re-size entirely so the
    /// window stops thrashing between FitToContent and cap sizes
    /// while the user scrubs same-class content (P9).
    last_sizing_class: Cell<Option<SizingPreference>>,
    /// Latched when the user interactively resizes the
    /// window-managed toplevel (P9). While set, `apply_sizing` is
    /// skipped — the WM and the user own the size — until the
    /// sizing class changes, which clears the latch. Never set in
    /// overlay placement: a layer surface has no interactive
    /// resize, and the notify handler bails there.
    user_resized: Cell<bool>,
    /// True while a programmatic `set_default_size` is settling
    /// (see [`PROGRAMMATIC_RESIZE_SETTLE`]). Default-size notifies
    /// arriving in this window update `programmatic_size` instead
    /// of arming the `user_resized` latch.
    programmatic_resize_active: Cell<bool>,
    /// Generation counter pairing each programmatic resize with its
    /// settle timeout, so an older timeout cannot clear the flag
    /// while a newer resize is still in flight.
    programmatic_resize_gen: Cell<u64>,
    /// Last default size this process set itself (or observed while
    /// a programmatic resize was settling). A notify that reports
    /// exactly this size is our own echo, not a user resize.
    programmatic_size: Cell<Option<(i32, i32)>>,
    /// Active 60s self-quit timer. Replaced (after cancellation) on
    /// every `ShowOrUpdate` and (re)scheduled on every Close /
    /// Escape / Space / launch. Storing the `SourceId` is the only
    /// way to cancel a glib timeout once scheduled.
    idle_source: RefCell<Option<glib::SourceId>>,
    /// GApplication hold-guard. Without this, the GTK main loop exits
    /// as soon as `connect_activate` returns with no window attached —
    /// and since we build the window lazily inside the async command
    /// loop, the first ShowOrUpdate race-loses to app termination and
    /// the preview window flickers in and immediately dies. The guard
    /// keeps the internal reference count above zero for the whole
    /// warm lifetime; explicit `app.quit()` (idle timeout, DaemonGone,
    /// EOF) is the only way out.
    hold_guard: RefCell<Option<gtk::gio::ApplicationHoldGuard>>,
    /// Latest xdg-foreign-v2 export handle ferried via
    /// [`PreviewCommand::SetParent`]. Recorded here in Phase 1.3 so
    /// the import call (Phase 1.2) can pick it up once the window
    /// surface exists. Cleared on [`PreviewCommand::ClearParent`].
    parent_handle: RefCell<Option<String>>,
    /// xdg-foreign-v2 importer holding the active parent-of
    /// relationship. Lazy-initialized on first SetParent receipt once
    /// the window surface is realized. Cleared on ClearParent.
    wayland_importer: RefCell<Option<wayland_xdg_foreign::WaylandImporter>>,
    launcher_monitor: RefCell<Option<String>>,
    launcher_rect: Cell<Option<(i32, i32, i32, i32)>>,
    launcher_hidden_by_us: Cell<bool>,
    /// True when the window was initialized as a layer-shell surface:
    /// `[gui] preview_placement = "overlay"` AND
    /// `gtk4_layer_shell::is_supported()` at skeleton build time.
    /// False in the default window-managed placement (and when
    /// "overlay" is configured without runtime layer-shell support,
    /// which degrades to window-managed behaviour). Selects the
    /// side-by-side visibility logic in
    /// `update_launcher_visibility`, and gates every `gtk_layer_*`
    /// call so none ever runs against a plain toplevel window.
    layer_shell_active: Cell<bool>,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();
    let daemon_cfg = lixun_config::Config::load()?;
    let preview_cfg = Rc::new(daemon_cfg.preview);
    let gui_cfg = Rc::new(daemon_cfg.gui);

    let standalone_file = args.standalone_file.clone();
    let socket_path_opt = args.socket_path.clone();

    let (inbound_tx, inbound_rx) = async_channel::unbounded::<InboundMsg>();
    let (outbound_tx, outbound_rx) = async_channel::unbounded::<PreviewEvent>();

    if let Some(ref path) = standalone_file {
        tracing::info!(
            "preview: standalone mode, file={:?} pid={}",
            path,
            std::process::id()
        );
        spawn_outbound_drain(outbound_rx);
        push_standalone_command(&inbound_tx, path.clone());
    } else {
        let socket_path = socket_path_opt
            .as_ref()
            .expect("clap guarantees socket_path or standalone_file");
        let listener = bind_listener(socket_path)?;
        tracing::info!(
            "preview: listening on {:?} pid={}",
            socket_path,
            std::process::id()
        );
        spawn_socket_workers(listener, inbound_tx.clone(), outbound_rx);
    }

    let app = gtk::Application::new(Some(APP_ID), ApplicationFlags::NON_UNIQUE);
    let state = Rc::new(PreviewState::default());

    {
        let state = Rc::clone(&state);
        let outbound_tx = outbound_tx.clone();
        let preview_cfg = Rc::clone(&preview_cfg);
        let gui_cfg = Rc::clone(&gui_cfg);
        let inbound_rx = inbound_rx.clone();
        app.connect_activate(move |app| {
            // Pin the app's internal reference count for the lifetime
            // of the warm process. Without this, GTK auto-quits the
            // main loop when activate returns with no visible window,
            // and the first ShowOrUpdate races against termination.
            state.hold_guard.replace(Some(app.hold()));

            // Announce readiness before draining commands. The daemon
            // buffers the latest_desired ShowOrUpdate until Ready
            // arrives — see the daemon-side state machine.
            let _ = outbound_tx.send_blocking(PreviewEvent::Ready {
                pid: std::process::id(),
            });

            let app = app.clone();
            let state = Rc::clone(&state);
            let outbound_tx = outbound_tx.clone();
            let preview_cfg = Rc::clone(&preview_cfg);
            let gui_cfg = Rc::clone(&gui_cfg);
            let inbound_rx = inbound_rx.clone();

            glib::spawn_future_local(async move {
                while let Ok(msg) = inbound_rx.recv().await {
                    match msg {
                        InboundMsg::Cmd(cmd) => {
                            handle_command(cmd, &app, &state, &outbound_tx, &preview_cfg, &gui_cfg)
                        }
                        InboundMsg::DaemonGone => {
                            tracing::info!("preview: daemon disconnected, quitting");
                            app.quit();
                            break;
                        }
                    }
                }
            });
        });
    }

    let exit_code = app.run_with_args::<&str>(&[]);

    if let Some(ref path) = socket_path_opt {
        let _ = std::fs::remove_file(path);
    }

    let code: i32 = exit_code.into();
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

fn spawn_outbound_drain(rx: async_channel::Receiver<PreviewEvent>) {
    std::thread::Builder::new()
        .name("standalone-event-drain".into())
        .spawn(move || while rx.recv_blocking().is_ok() {})
        .expect("spawn standalone event drain");
}

fn push_standalone_command(tx: &async_channel::Sender<InboundMsg>, path: PathBuf) {
    use lixun_core::{Action, Category, DocId, Hit};

    let title = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    let mime = mime_guess_from_path(&path);
    let meta = std::fs::metadata(&path).ok();
    let timestamp = meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64);
    let size = meta.as_ref().map(|m| m.len());

    let hit = Hit {
        id: DocId("standalone".into()),
        category: Category::File,
        title,
        subtitle: String::new(),
        icon_name: None,
        kind_label: mime.clone(),
        score: 100.0,
        action: Action::OpenFile { path },
        extract_fail: false,
        sender: None,
        recipients: None,
        body: None,
        secondary_action: None,
        source_instance: String::new(),
        row_menu: Default::default(),
        mime,
        timestamp,
        size,
    };

    let _ = tx.send_blocking(InboundMsg::Cmd(PreviewCommand::ShowOrUpdate {
        epoch: 1,
        hit: Box::new(hit),
        monitor: None,
    }));
}

fn mime_guess_from_path(path: &std::path::Path) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(
        match ext.as_str() {
            "pdf" => "application/pdf",
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "svg" => "image/svg+xml",
            "txt" | "md" | "log" => "text/plain",
            "html" | "htm" => "text/html",
            _ => return None,
        }
        .into(),
    )
}

/// Bind the per-process Unix socket. The path was passed by the
/// daemon and is expected to live under `$XDG_RUNTIME_DIR` (or the
/// 0700 fallback `/tmp/lixun-{uid}/`). We unlink any stale leftover
/// from a prior crashed instance — it is safe because the path is
/// per-pid and no other process should hold it. Permissions are
/// chmod'd to 0600 and FD_CLOEXEC is set so we do not leak the
/// socket into any plugin-spawned subprocess.
fn bind_listener(path: &PathBuf) -> Result<UnixListener> {
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(path)?;
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(path, perms)?;
    set_cloexec(listener.as_raw_fd())?;
    Ok(listener)
}

fn set_cloexec(fd: std::os::unix::io::RawFd) -> Result<()> {
    // Race-free CLOEXEC set. `fcntl(F_SETFD)` does not need locking
    // because only this thread holds the fd at this point.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}

/// Single-client accept + reader thread + writer thread. The
/// listener accepts exactly one connection (the daemon) and is
/// dropped immediately after — any second connection attempt would
/// hit ECONNREFUSED, which is the desired single-client invariant.
/// The accepted stream is `try_clone`d so reader and writer threads
/// own independent halves with no locking.
fn spawn_socket_workers(
    listener: UnixListener,
    inbound_tx: async_channel::Sender<InboundMsg>,
    outbound_rx: async_channel::Receiver<PreviewEvent>,
) {
    std::thread::Builder::new()
        .name("lixun-preview-net".into())
        .spawn(move || {
            let (stream, _addr) = match listener.accept() {
                Ok(pair) => pair,
                Err(e) => {
                    tracing::error!("preview: accept failed: {}", e);
                    let _ = inbound_tx.send_blocking(InboundMsg::DaemonGone);
                    return;
                }
            };
            // Drop listener so a second daemon (or a stray client)
            // cannot connect — the daemon-vs-preview lifetime is 1:1.
            drop(listener);

            let writer_stream = match stream.try_clone() {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("preview: try_clone failed: {}", e);
                    let _ = inbound_tx.send_blocking(InboundMsg::DaemonGone);
                    return;
                }
            };

            // Writer thread: drains outbound_rx and writes frames.
            // Spawned as a detached thread; it self-exits on channel
            // close or write error.
            std::thread::Builder::new()
                .name("lixun-preview-tx".into())
                .spawn(move || {
                    let mut w = writer_stream;
                    while let Ok(ev) = outbound_rx.recv_blocking() {
                        if let Err(e) = write_frame_sync(&mut w, &ev) {
                            tracing::warn!("preview: write_frame_sync failed: {}", e);
                            break;
                        }
                    }
                })
                .expect("spawn lixun-preview-tx");

            // Reader loop runs on this (net) thread. EOF / decode
            // error are both treated as "daemon gone".
            let mut r = stream;
            loop {
                match read_frame_sync::<_, PreviewCommand>(&mut r) {
                    Ok(cmd) => {
                        tracing::debug!("preview: read_frame_sync received command: {:?}", cmd);
                        if inbound_tx.send_blocking(InboundMsg::Cmd(cmd)).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        if e.kind() != std::io::ErrorKind::UnexpectedEof {
                            tracing::warn!("preview: read_frame_sync failed: {}", e);
                        }
                        let _ = inbound_tx.send_blocking(InboundMsg::DaemonGone);
                        break;
                    }
                }
            }
        })
        .expect("spawn lixun-preview-net");
}

/// Dispatch one inbound `PreviewCommand` on the GTK main thread.
///
/// This is the single point where IPC turns into widget mutation,
/// which is why epoch handling and idle-timer reset live here and
/// nowhere else. Each command bumps `current_epoch` (ShowOrUpdate)
/// or schedules the warm-process idle countdown (Close), so any
/// future async work spawned from a plugin must capture the epoch
/// at start and re-check it before committing widget changes.
fn handle_command(
    cmd: PreviewCommand,
    app: &gtk::Application,
    state: &Rc<PreviewState>,
    outbound_tx: &async_channel::Sender<PreviewEvent>,
    preview_cfg: &Rc<lixun_config::PreviewConfig>,
    gui_cfg: &Rc<lixun_config::GuiConfig>,
) {
    match cmd {
        PreviewCommand::ShowOrUpdate {
            epoch,
            hit,
            monitor,
        } => {
            state.current_epoch.set(epoch);
            cancel_idle(state);
            let hit = *hit;
            if let Err(e) = show_or_update(
                app,
                state,
                outbound_tx,
                preview_cfg,
                gui_cfg,
                &hit,
                monitor.as_deref(),
            ) {
                tracing::error!("preview: show_or_update failed: {}", e);
                let _ = outbound_tx.send_blocking(PreviewEvent::Error {
                    epoch,
                    msg: format!("{e:#}"),
                });
            }
        }
        PreviewCommand::Close { epoch } => {
            cancel_idle(state);
            let activation_token = mint_activation_token();
            if let Some(window) = state.window.borrow().as_ref() {
                window.set_visible(false);
            }
            if state.launcher_hidden_by_us.get() {
                let _ =
                    outbound_tx.send_blocking(PreviewEvent::SetLauncherVisible { visible: true });
                state.launcher_hidden_by_us.set(false);
            }
            let _ = outbound_tx.send_blocking(PreviewEvent::Closed {
                epoch,
                activation_token,
            });
            schedule_idle(state, app);
        }
        PreviewCommand::Hide { epoch } => {
            // Same wire effect as Close: hide window, keep process
            // warm, schedule the 60s idle timer, emit Closed so the
            // daemon dispatches Show + ExitPreviewMode back to the
            // launcher. The variant exists separately because the
            // launcher's Escape path (under KeyboardMode::None) sends
            // this rather than relying on the preview's own keyboard
            // controller — the controller no longer fires because the
            // preview surface does not participate in the keyboard
            // seat. See PreviewCommand::Hide docstring in
            // lixun-ipc::preview for the full rationale.
            cancel_idle(state);
            let activation_token = mint_activation_token();
            if let Some(window) = state.window.borrow().as_ref() {
                window.set_visible(false);
            }
            if state.launcher_hidden_by_us.get() {
                let _ =
                    outbound_tx.send_blocking(PreviewEvent::SetLauncherVisible { visible: true });
                state.launcher_hidden_by_us.set(false);
            }
            let _ = outbound_tx.send_blocking(PreviewEvent::Closed {
                epoch,
                activation_token,
            });
            schedule_idle(state, app);
        }
        PreviewCommand::Ping => {
            // Keepalive only. No state change, no event reply.
        }
        PreviewCommand::SetParent { handle } => {
            tracing::debug!("preview: SetParent received (handle={})", handle);
            state.parent_handle.replace(Some(handle));
            if let Some(window) = state.window.borrow().as_ref() {
                try_apply_pending_parent(state, window);
            }
        }
        PreviewCommand::ClearParent => {
            tracing::debug!("preview: ClearParent received");
            state.parent_handle.replace(None);
            if let Some(imp) = state.wayland_importer.borrow_mut().as_mut() {
                imp.clear();
            }
        }
        PreviewCommand::Scroll {
            epoch: _,
            direction,
            pages,
        } => {
            // Idempotent view mutation — applied regardless of epoch
            // (a stale scroll can at worst nudge the same document;
            // gating it would drop legitimate keystrokes racing a
            // selection change). Does NOT touch current_epoch.
            let request = match direction {
                ScrollDirection::Up => ScrollRequest::PageUp,
                ScrollDirection::Down => ScrollRequest::PageDown,
            };
            scroll_content(state, request, pages);
        }
        PreviewCommand::LauncherGeometry {
            monitor,
            x,
            y,
            w,
            h,
        } => {
            tracing::debug!(
                "preview: received LauncherGeometry monitor={} x={} y={} w={} h={}",
                monitor,
                x,
                y,
                w,
                h
            );
            state.launcher_monitor.replace(Some(monitor));
            state.launcher_rect.set(Some((x, y, w, h)));
            if let Some(window) = state.window.borrow().as_ref() {
                update_launcher_visibility(state, window, outbound_tx, gui_cfg);
            } else {
                tracing::debug!("preview: LauncherGeometry received but window not yet created");
            }
        }
    }
}

/// Lazy-build the preview window the first time, then update or
/// rebuild content for subsequent commands. Plugin id parity decides
/// `update` vs `rebuild` — when `plugin.update` returns the
/// `UPDATE_UNSUPPORTED` sentinel or the plugin id changes, we drop
/// the old widget and call `plugin.build` against the same
/// ScrolledWindow container. The header is rebuilt unconditionally
/// per command (cheap; four widgets) so title/subtitle/badge always
/// reflect the current hit.
fn show_or_update(
    app: &gtk::Application,
    state: &Rc<PreviewState>,
    outbound_tx: &async_channel::Sender<PreviewEvent>,
    preview_cfg: &Rc<lixun_config::PreviewConfig>,
    gui_cfg: &Rc<lixun_config::GuiConfig>,
    hit: &Hit,
    requested_monitor: Option<&str>,
) -> Result<()> {
    let Some(plugin) = select_plugin(hit) else {
        anyhow::bail!(
            "no plugin matches hit id={} category={:?}",
            hit.id.0,
            hit.category
        );
    };
    let plugin: Rc<dyn PreviewPlugin> = Rc::from(plugin);
    let plugin_id = plugin.id().to_string();
    let plugin_cfg = PreviewPluginCfg {
        section: preview_cfg.plugin_sections.get(plugin_id.as_str()),
        max_file_size_mb: preview_cfg.max_file_size_mb,
    };

    let display =
        gtk::gdk::Display::default().ok_or_else(|| anyhow::anyhow!("no default GDK display"))?;

    if state.window.borrow().is_none() {
        build_window_skeleton(app, state, &display, outbound_tx, gui_cfg)?;
    }

    // Recompute monitor + cap on every command. Per Oracle: the
    // launcher's monitor may differ between Spaces and we must not
    // remember the first one.
    let (w_max, h_max) = apply_monitor_and_cap(state, &display, requested_monitor, gui_cfg);

    // P9: re-apply window sizing only when the decision helper says
    // so. A sizing-class change always re-sizes and re-arms host
    // control (clearing any user-resize latch); an unchanged class
    // on a mapped window is left alone so arrow-scrub across
    // same-class hits cannot thrash the window, and a user-resized
    // window keeps its size until the class changes.
    let sizing = plugin.sizing();
    let prev_class = state.last_sizing_class.get();
    if prev_class != Some(sizing) {
        state.user_resized.set(false);
    }
    let mapped = state
        .window
        .borrow()
        .as_ref()
        .is_some_and(|w| w.is_mapped());
    if should_apply_sizing(prev_class, sizing, mapped, state.user_resized.get()) {
        apply_sizing(state, sizing, w_max, h_max);
    }
    state.last_sizing_class.set(Some(sizing));
    rebuild_header(state, hit, app, Rc::clone(&plugin), outbound_tx);

    let same_plugin = state
        .current_plugin_id
        .borrow()
        .as_deref()
        .is_some_and(|id| id == plugin_id);

    let needs_rebuild = if same_plugin && let Some(widget) = state.current_widget.borrow().as_ref()
    {
        match plugin.update(hit, widget) {
            Ok(()) => false,
            Err(e) => {
                let msg = format!("{e}");
                if msg.contains(UPDATE_UNSUPPORTED) {
                    true
                } else {
                    tracing::warn!(
                        "preview: plugin `{}` update failed, rebuilding: {}",
                        plugin_id,
                        e
                    );
                    true
                }
            }
        }
    } else {
        true
    };

    if needs_rebuild {
        let new_widget = match plugin.build(hit, &plugin_cfg) {
            Ok(w) => w,
            Err(e) => {
                tracing::error!("preview: plugin `{}` build failed: {}", plugin_id, e);
                let err_label =
                    gtk::Label::new(Some(&format!("Preview failed ({plugin_id}):\n{e}")));
                err_label.set_wrap(true);
                err_label.set_margin_top(24);
                err_label.set_margin_bottom(24);
                err_label.set_margin_start(24);
                err_label.set_margin_end(24);
                err_label.upcast::<gtk::Widget>()
            }
        };

        // Detach the previously-mounted widget from whichever
        // container owns it. Mirrors the mount branch below: an
        // OwnsScroll widget sits directly in vbox; everything else
        // sits inside content_scroll. Without this cleanup a plugin
        // switch from OwnsScroll → FixedCap (or vice versa) would
        // leak the old widget into vbox alongside the new one.
        let prev_owns_scroll = state.current_widget_owns_scroll.get();
        if let Some(prev_widget) = state.current_widget.borrow_mut().take() {
            if prev_owns_scroll {
                if let Some(vbox) = state.vbox.borrow().as_ref() {
                    vbox.remove(&prev_widget);
                }
            } else if let Some(scroll) = state.content_scroll.borrow().as_ref() {
                scroll.set_child(gtk::Widget::NONE);
            }
        }

        let owns_scroll = matches!(plugin.sizing(), SizingPreference::OwnsScroll);
        if owns_scroll {
            if let Some(vbox) = state.vbox.borrow().as_ref() {
                new_widget.set_hexpand(true);
                new_widget.set_vexpand(true);
                vbox.append(&new_widget);
            }
        } else if let Some(scroll) = state.content_scroll.borrow().as_ref() {
            scroll.set_child(Some(&new_widget));
        }
        state.current_widget_owns_scroll.set(owns_scroll);
        *state.current_widget.borrow_mut() = Some(new_widget);
        *state.current_plugin_id.borrow_mut() = Some(plugin_id.clone());
        *state.current_plugin.borrow_mut() = Some(Rc::clone(&plugin));
    }

    *state.current_hit.borrow_mut() = Some(hit.clone());

    apply_content_accessible_label(state, hit, &plugin);
    update_hints(state, &plugin, hit);

    if let Some(window) = state.window.borrow().as_ref() {
        window.set_visible(true);
        window.present();
        // Land initial keyboard focus on the CONTENT, not the
        // header's Open button (P7): plugin-wired keys (PgUp/PgDn,
        // zoom, search) are dead until focus reaches the widget
        // they're attached to. Fall back to the outer scroll
        // container when the plugin widget refuses focus.
        let focused = state
            .current_widget
            .borrow()
            .as_ref()
            .map(|w| w.grab_focus())
            .unwrap_or(false);
        if !focused
            && let Some(scroll) = state.content_scroll.borrow().as_ref()
        {
            scroll.grab_focus();
        }
        try_apply_pending_parent(state, window);
        {
            let state_clone = state.clone();
            let window_clone = window.clone();
            let outbound_clone = outbound_tx.clone();
            let gui_cfg_clone = Rc::clone(gui_cfg);
            glib::timeout_add_local_once(std::time::Duration::from_millis(100), move || {
                update_launcher_visibility(&state_clone, &window_clone, &outbound_clone, &gui_cfg_clone);
            });
        }
    }

    tracing::info!(
        "preview: showed plugin={} hit_id={} epoch={}",
        plugin_id,
        hit.id.0,
        state.current_epoch.get()
    );
    Ok(())
}

/// Build the persistent preview window skeleton: ApplicationWindow,
/// vbox, header_box, content_scroll. Called once per process
/// lifetime; subsequent commands mutate the existing widgets.
///
/// Surface role — `[gui] preview_placement`:
///
/// * `"window"` (default): a normal decorated xdg-toplevel. The WM
///   owns placement, stacking, and dragging (the decoration is the
///   drag handle); users can pin placement with WM rules targeting
///   app-id `app.lixun.preview` (set via the `GtkApplication` id;
///   title "Lixun Preview"). Tradeoff: the launcher is an
///   overlay-layer surface and always stacks above the preview where
///   they overlap — the launcher tucks against the left screen edge
///   during preview mode to minimise that overlap, and the
///   pre-existing xdg-foreign `SetParent` path keeps working here.
/// * `"overlay"` with runtime layer-shell support: joins the
///   launcher's `Layer::Overlay` (Wayland stacks that layer above
///   every xdg-toplevel), anchored to the RIGHT monitor edge (small
///   margin; Top/Bottom unanchored so the compositor centers the
///   surface vertically); the launcher centers itself in the left
///   column (lixun-gui's `slide_for_preview`) and the two surfaces
///   sit side by side deterministically. `"overlay"` without
///   layer-shell support degrades to the `"window"` behaviour.
///
/// Keyboard (overlay): `KeyboardMode::OnDemand` keeps the seat with
/// the launcher when the preview maps — arrow-scrub keeps flowing
/// through the launcher's selection→preview pipeline — while a user
/// who clicks into the preview can still use its internal keys (PDF
/// search, zoom). In window mode the WM may focus the preview; the
/// capture-phase controller relays Up/Down as `NavKey` and closes on
/// Escape/Space, so both mode's close paths stay
/// `PreviewCommand::Hide`/`Close` driven from the launcher keymap.
fn build_window_skeleton(
    app: &gtk::Application,
    state: &Rc<PreviewState>,
    display: &gtk::gdk::Display,
    outbound_tx: &async_channel::Sender<PreviewEvent>,
    gui_cfg: &Rc<lixun_config::GuiConfig>,
) -> Result<()> {
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("Lixun Preview")
        .icon_name("lixun-logo-light")
        .decorated(true)
        .default_width(DEFAULT_WIDTH)
        .default_height(DEFAULT_HEIGHT)
        .resizable(true)
        .build();
    window.set_widget_name("lixun-preview-root");

    // Overlay placement (opt-in): only another Overlay layer
    // surface can render beside (rather than under) the launcher's
    // Overlay layer surface. The default window-managed placement
    // deliberately skips ALL layer-shell setup so the WM owns the
    // window. See the docstring above for the full rationale.
    let layer_shell = matches!(
        gui_cfg.preview_placement,
        lixun_config::PreviewPlacement::Overlay
    ) && gtk4_layer_shell::is_supported();
    if layer_shell {
        use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
        window.init_layer_shell();
        window.set_namespace(Some("lixun-preview"));
        window.set_layer(Layer::Overlay);
        window.set_anchor(Edge::Right, true);
        window.set_margin(Edge::Right, PREVIEW_EDGE_MARGIN);
        window.set_keyboard_mode(KeyboardMode::OnDemand);
        // Layer surfaces get no compositor decorations, and a CSD
        // titlebar inside the overlay would only waste pixels. The
        // close affordances are Escape/Space (launcher keymap plus
        // the capture-phase controller installed below).
        window.set_decorated(false);
    } else {
        // Window-managed placement: pin a stable Wayland app-id so
        // users can target the preview with WM rules (placement,
        // floating, size) as documented in docs/config.example.toml.
        // GTK4 would otherwise fall back to the process name;
        // setting it explicitly keeps the contract independent of
        // how the binary was invoked. Realize-time is the earliest
        // point the GDK toplevel exists, and it precedes the first
        // map — which is when compositors read the app-id. The
        // downcast quietly skips non-Wayland backends (X11 uses
        // WM_CLASS from the process name instead).
        window.connect_realize(|w| {
            if let Some(surface) = w.surface()
                && let Ok(toplevel) = surface.downcast::<gdk4_wayland::WaylandToplevel>()
            {
                toplevel.set_application_id(APP_ID);
            }
        });
    }
    state.layer_shell_active.set(layer_shell);

    // P9: observe default-size changes to detect interactive
    // resizes. Installed unconditionally; the handler bails in
    // overlay placement where no interactive resize exists.
    {
        let state_for_notify = Rc::clone(state);
        window.connect_default_width_notify(move |w| {
            note_default_size_change(&state_for_notify, w);
        });
        let state_for_notify = Rc::clone(state);
        window.connect_default_height_notify(move |w| {
            note_default_size_change(&state_for_notify, w);
        });
    }

    // Embedded stylesheet first (APPLICATION priority), then the
    // user's ~/.config/lixun/style.css above it (APPLICATION + 1,
    // inside install_user_css) — the same layering the launcher's
    // style_manager uses. Before this the preview shipped with
    // stock light-grey GTK next to the dark launcher (P3).
    install_embedded_css(display);
    install_user_css(display);

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let header_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    header_box.set_widget_name("lixun-preview-header");
    header_box.set_margin_top(12);
    header_box.set_margin_bottom(8);
    header_box.set_margin_start(16);
    header_box.set_margin_end(16);
    vbox.append(&header_box);

    let content_scroll = gtk::ScrolledWindow::new();
    content_scroll.set_widget_name("lixun-preview-content");
    // Focusable so the post-mount content focus (P7) has a landing
    // spot even when a plugin's widget itself refuses focus; a
    // focused ScrolledWindow also gets native keyboard scrolling.
    content_scroll.set_focusable(true);
    vbox.append(&content_scroll);

    // Keyboard-hint strip (P7): populated per ShowOrUpdate from the
    // plugin's capability flags; sits below the content area.
    let hints = gtk::Label::new(None);
    hints.set_widget_name("lixun-preview-hints");
    hints.set_halign(gtk::Align::Start);
    hints.set_ellipsize(gtk::pango::EllipsizeMode::End);
    vbox.append(&hints);

    window.set_child(Some(&vbox));

    install_close_controllers(&window, app, state, outbound_tx);

    *state.header_box.borrow_mut() = Some(header_box);
    *state.content_scroll.borrow_mut() = Some(content_scroll);
    *state.hints_label.borrow_mut() = Some(hints);
    *state.vbox.borrow_mut() = Some(vbox);
    *state.window.borrow_mut() = Some(window);
    Ok(())
}

/// Register the compiled-in stylesheet at the base APPLICATION
/// priority. The user override (install_user_css) and any theme sit
/// above it, so every rule here is a default, not a mandate.
fn install_embedded_css(display: &gtk::gdk::Display) {
    const EMBEDDED_STYLESHEET: &str = include_str!("../style.css");
    let provider = gtk::CssProvider::new();
    provider.load_from_string(EMBEDDED_STYLESHEET);
    gtk::style_context_add_provider_for_display(
        display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

fn try_apply_pending_parent(state: &Rc<PreviewState>, window: &gtk::ApplicationWindow) {
    // A layer surface carries no xdg_toplevel role, so importing an
    // xdg-foreign parent handle for it would be a protocol error.
    // The daemon does not send SetParent while the launcher itself
    // is a layer surface, so this guard is belt-and-suspenders for
    // the xdg-toplevel fallback path only.
    if state.layer_shell_active.get() {
        return;
    }
    let handle = match state.parent_handle.borrow().clone() {
        Some(h) => h,
        None => return,
    };
    let Some(gdk_surface) = window.surface() else {
        return;
    };
    let Some(wl_surface) = wayland_xdg_foreign::wl_surface_of(&gdk_surface) else {
        return;
    };
    let mut importer = state.wayland_importer.borrow_mut();
    if importer.is_none() {
        match wayland_xdg_foreign::WaylandImporter::new(&gdk_surface) {
            Ok(Some(i)) => *importer = Some(i),
            Ok(None) => return,
            Err(e) => {
                tracing::warn!("preview: xdg-foreign importer init failed: {}", e);
                return;
            }
        }
    }
    if let Some(imp) = importer.as_mut()
        && let Err(e) = imp.import(&handle, &wl_surface) {
            tracing::warn!("preview: xdg-foreign import failed: {}", e);
        }
}

/// Pure predicate for [`update_launcher_visibility`] (layer-shell
/// mode): can the launcher stay on screen beside a right-anchored
/// preview surface? The left column spans everything left of the
/// preview (which sits [`PREVIEW_EDGE_MARGIN`] short of the right
/// monitor edge); the launcher fits when that column holds its
/// width — floored at [`LAUNCHER_MIN_WIDTH`] so a bogus small
/// measurement cannot approve a hopeless column — plus
/// [`LAUNCHER_COLUMN_GAP`] on each side.
///
/// Must stay arithmetic-identical to the launcher's own slide
/// predicate (`lixun-gui/src/preview_layout.rs`,
/// `launcher_fits_column` over `left_column_width`): the launcher
/// slides into the left column exactly when this returns true, and
/// we soft-hide it exactly when this returns false. The decision
/// depends only on widths — never on positions — so the launcher's
/// post-slide geometry report cannot flip the answer and oscillate.
fn launcher_fits_beside(monitor_w: i32, launcher_w: i32, preview_w: i32) -> bool {
    if monitor_w <= 0 || preview_w <= 0 {
        return false;
    }
    let column_w = monitor_w - preview_w - PREVIEW_EDGE_MARGIN;
    column_w >= launcher_w.max(LAUNCHER_MIN_WIDTH) + 2 * LAUNCHER_COLUMN_GAP
}

/// Window-mode predicate: true when a launcher tucked to the LEFT
/// edge (at [`PREVIEW_EDGE_MARGIN`], matching the GUI's window-mode
/// slide) clears a WM-CENTRED preview of width `preview_w` — i.e. the
/// launcher's right edge sits left of the centred preview's left
/// edge, with [`LAUNCHER_COLUMN_GAP`] of breathing room.
///
/// Unlike [`launcher_fits_beside`] (right-anchored preview, overlay
/// mode) this accounts for the preview being centred by the
/// compositor, which a Wayland client cannot reposition. On a laptop
/// panel a left launcher and a centred half-screen preview overlap
/// even though their widths would fit side by side; there the
/// launcher is soft-hidden and arrow-scrub carries navigation.
fn launcher_clears_centered_preview(monitor_w: i32, launcher_w: i32, preview_w: i32) -> bool {
    if monitor_w <= 0 || preview_w <= 0 {
        return false;
    }
    let launcher_right = PREVIEW_EDGE_MARGIN + launcher_w.max(LAUNCHER_MIN_WIDTH);
    let centered_preview_left = (monitor_w - preview_w) / 2;
    launcher_right + LAUNCHER_COLUMN_GAP <= centered_preview_left
}

/// Decide whether the launcher can stay visible while this preview
/// is mapped, and ask the daemon to soft-hide it when it cannot.
/// No-op while the preview window itself is hidden.
///
/// Overlay placement (layer-shell active): the preview hugs the
/// RIGHT monitor edge, so the launcher survives exactly when it fits
/// in the remaining left column ([`launcher_fits_beside`]); the
/// launcher slides itself into that column with the same math. A
/// launcher on a different monitor always stays visible. When the
/// column is too narrow, `visible=false` routes through
/// `GuiCommand::SoftHide`, which preserves the launcher's preview
/// session; every close path (`Close`/`Hide`/close-request/
/// Escape/Space) restores visibility via `launcher_hidden_by_us`,
/// backstopped by the daemon's Closed → `GuiCommand::Show` dispatch.
///
/// Window-managed placement (default): the WM owns the preview's
/// position, which a Wayland client cannot observe — but
/// [`launcher_fits_beside`] depends only on WIDTHS, so one statement
/// remains valid: when it returns false, launcher and preview cannot
/// coexist at ANY position the WM might pick (laptop-width monitors),
/// and the launcher is soft-hidden exactly like overlay mode —
/// arrow-scrub keeps working via NavKey forwarding, Escape restores.
/// When it returns true, visibility is left alone: the WM may still
/// have centered the preview under the launcher, and that residual,
/// position-dependent overlap is the documented tradeoff of this mode
/// (drag the preview, or pin it with a WM rule on app-id
/// `app.lixun.preview`).
fn update_launcher_visibility(
    state: &Rc<PreviewState>,
    window: &gtk::ApplicationWindow,
    outbound_tx: &async_channel::Sender<PreviewEvent>,
    gui_cfg: &Rc<lixun_config::GuiConfig>,
) {
    if !window.is_visible() {
        return;
    }

    let hide_if_visible = |reason: &str| {
        if !state.launcher_hidden_by_us.get() {
            tracing::info!("visibility: soft-hiding launcher ({reason})");
            let _ = outbound_tx.send_blocking(PreviewEvent::SetLauncherVisible { visible: false });
            state.launcher_hidden_by_us.set(true);
        }
    };
    let restore_if_hidden = |reason: &str| {
        if state.launcher_hidden_by_us.get() {
            tracing::info!("visibility: restoring launcher ({reason})");
            let _ = outbound_tx.send_blocking(PreviewEvent::SetLauncherVisible { visible: true });
            state.launcher_hidden_by_us.set(false);
        }
    };

    // Both placements run the same width-only fits check below: in
    // window mode it fires only for the cannot-coexist-anywhere case
    // (see docstring) — the monitor/connector resolution and the
    // hide/restore plumbing are placement-independent.
    let launcher_monitor = state.launcher_monitor.borrow();
    let Some(launcher_mon) = launcher_monitor.as_ref() else {
        tracing::debug!("visibility: no launcher monitor yet");
        return;
    };
    let Some((_lx, _ly, lw, _lh)) = state.launcher_rect.get() else {
        tracing::debug!("visibility: no launcher rect yet");
        return;
    };

    let Some(display) = gtk::gdk::Display::default() else {
        tracing::debug!("visibility: no default display");
        return;
    };
    let Some(surface) = window.surface() else {
        tracing::debug!("visibility: no preview surface");
        return;
    };
    let Some(monitor) = display.monitor_at_surface(&surface) else {
        tracing::debug!("visibility: no monitor for preview surface");
        return;
    };
    let Some(preview_mon) = monitor.connector().map(|c| c.to_string()) else {
        tracing::debug!("visibility: no connector for preview monitor");
        return;
    };

    if preview_mon != *launcher_mon {
        restore_if_hidden("different monitors");
        return;
    }

    let mon_w = monitor.geometry().width();
    // Reason about the wider of (a) the width the launcher predicted
    // from the shared config when it decided whether to slide and
    // (b) the width the plugin actually rendered. Using only (b)
    // would let a narrow FitToContent render approve a column the
    // launcher never slid into (it predicted a wider preview and
    // skipped the slide), leaving the centered launcher overlapping
    // the preview; using only (a) would miss a plugin whose minimum
    // content forced the surface wider than configured.
    let pw = surface
        .width()
        .max(configured_preview_width(mon_w, gui_cfg));
    // The fit model depends on WHERE the preview sits, which differs
    // by placement mode:
    //   overlay: the preview is right-anchored (we place it), so the
    //     launcher survives whenever the two widths fit side by side.
    //   window:  the WM centres the preview (KWin default), and the
    //     launcher — even slid to the left edge — only clears a
    //     centred preview when its right edge is left of the
    //     preview's left edge. Width-sum "fits" is NOT enough here:
    //     a left launcher + centred preview overlap unless the
    //     monitor is wide enough for the whole preview to sit right
    //     of the launcher.
    let fits = if state.layer_shell_active.get() {
        launcher_fits_beside(mon_w, lw, pw)
    } else {
        launcher_clears_centered_preview(mon_w, lw, pw)
    };
    tracing::debug!(
        "visibility: mode={} monitor_w={} launcher_w={} preview_w={} fits={} hidden_by_us={}",
        if state.layer_shell_active.get() { "overlay" } else { "window" },
        mon_w,
        lw,
        pw,
        fits,
        state.launcher_hidden_by_us.get()
    );
    if fits {
        restore_if_hidden("launcher clears the preview");
    } else {
        hide_if_visible("launcher would overlap the preview");
    }
}

/// Width the preview claims on a monitor `monitor_w` logical pixels
/// wide: the configured percent of the monitor, capped by
/// `preview_max_width_px`, floored at [`MIN_WIDTH`]. Kept as a named
/// helper because the launcher predicts this exact number for its
/// slide math (`lixun-gui/src/preview_layout.rs`,
/// `predicted_preview_width`) and [`update_launcher_visibility`]
/// must reason about the same width the launcher predicted, not just
/// whatever the current plugin happened to render.
fn configured_preview_width(monitor_w: i32, gui_cfg: &lixun_config::GuiConfig) -> i32 {
    (monitor_w * i32::from(gui_cfg.preview_width_percent) / 100)
        .min(gui_cfg.preview_max_width_px)
        .max(MIN_WIDTH)
}

fn apply_monitor_and_cap(
    state: &Rc<PreviewState>,
    display: &gtk::gdk::Display,
    requested: Option<&str>,
    gui_cfg: &Rc<lixun_config::GuiConfig>,
) -> (i32, i32) {
    let window_ref = state.window.borrow();
    let Some(window) = window_ref.as_ref() else {
        return (MIN_WIDTH, MIN_HEIGHT);
    };
    if let Some(monitor) = pick_monitor(display, requested) {
        // Layer surfaces belong to an output: bind the surface to
        // the launcher's monitor so the right-edge anchor lands on
        // the screen the launcher slid over on. Guarded on the
        // layer-shell path — gtk_layer_* setters must never run
        // against the xdg-toplevel fallback window — and on an
        // actual output change: gtk4-layer-shell remaps a mapped
        // surface on set_monitor, and remapping on every
        // ShowOrUpdate would flicker the preview during arrow-scrub.
        if state.layer_shell_active.get() {
            use gtk4_layer_shell::LayerShell;
            if window.monitor().as_ref() != Some(&monitor) {
                window.set_monitor(Some(&monitor));
            }
        }
        let geometry = monitor.geometry();
        let w = configured_preview_width(geometry.width(), gui_cfg);
        let h = (geometry.height() * i32::from(gui_cfg.preview_height_percent) / 100)
            .min(gui_cfg.preview_max_height_px)
            .max(MIN_HEIGHT);
        (w, h)
    } else {
        (MIN_WIDTH, MIN_HEIGHT)
    }
}

/// Decide whether `apply_sizing` should run for this `ShowOrUpdate`
/// (P9). Pure so the policy is unit-testable headlessly.
///
/// Precedence, highest first:
/// 1. Sizing-class change → always apply. The window layout for the
///    new class is genuinely different; this is also the point that
///    re-enables host sizing after a user resize (the caller clears
///    the latch on class change before consulting this function).
/// 2. User-resize latch → never apply. The user set a size by hand;
///    the host stops overriding it until the class changes.
/// 3. Window already mapped with an unchanged class → skip. The
///    current size came from this same class (or the WM); rewriting
///    it on every arrow-step is what caused the size thrash.
/// 4. Otherwise (unmapped, e.g. reopening after Close) → apply.
fn should_apply_sizing(
    prev_class: Option<SizingPreference>,
    new_class: SizingPreference,
    mapped: bool,
    user_resized: bool,
) -> bool {
    if prev_class != Some(new_class) {
        return true;
    }
    if user_resized {
        return false;
    }
    !mapped
}

/// Set the window default size, recording it so the resulting
/// `notify::default-width` / `notify::default-height` echoes are not
/// mistaken for user resizes (P9). The settle window covers the
/// asynchronous configure round-trip; the generation counter keeps
/// an older timeout from clearing the flag under a newer resize.
fn set_default_size_tracked(
    state: &Rc<PreviewState>,
    window: &gtk::ApplicationWindow,
    width: i32,
    height: i32,
) {
    state.programmatic_size.set(Some((width, height)));
    state.programmatic_resize_active.set(true);
    let generation = state.programmatic_resize_gen.get().wrapping_add(1);
    state.programmatic_resize_gen.set(generation);
    window.set_default_size(width, height);
    let state = Rc::clone(state);
    glib::timeout_add_local_once(PROGRAMMATIC_RESIZE_SETTLE, move || {
        if state.programmatic_resize_gen.get() == generation {
            state.programmatic_resize_active.set(false);
        }
    });
}

/// Classify a `notify::default-width` / `notify::default-height`
/// emission (P9). GTK4 records the surface's post-configure size
/// back into the default-size properties, so on Wayland these
/// notifies are the only client-visible trace of an interactive
/// resize — but they also fire as echoes of our own
/// `set_default_size` calls. While a programmatic resize is
/// settling, the observed size just updates the record (the
/// compositor may clamp what we asked for); afterwards, any size
/// that differs from the record arms the user-resize latch.
/// Layer-shell surfaces cannot be user-resized, so overlay
/// placement never latches.
fn note_default_size_change(state: &Rc<PreviewState>, window: &gtk::ApplicationWindow) {
    if state.layer_shell_active.get() {
        return;
    }
    let size = (window.default_width(), window.default_height());
    if state.programmatic_resize_active.get() {
        state.programmatic_size.set(Some(size));
        return;
    }
    if state.programmatic_size.get() == Some(size) {
        return;
    }
    if !state.user_resized.get() {
        tracing::debug!(
            "preview: user resize to {}x{}; host sizing paused until the sizing class changes",
            size.0,
            size.1
        );
        state.user_resized.set(true);
    }
}

fn apply_sizing(state: &Rc<PreviewState>, sizing: SizingPreference, w_max: i32, h_max: i32) {
    let window_ref = state.window.borrow();
    let scroll_ref = state.content_scroll.borrow();
    let (Some(window), Some(scroll)) = (window_ref.as_ref(), scroll_ref.as_ref()) else {
        return;
    };
    match sizing {
        SizingPreference::FixedCap => {
            set_default_size_tracked(state, window, w_max, h_max);
            scroll.set_visible(true);
            scroll.set_vexpand(true);
            scroll.set_hexpand(true);
            scroll.set_propagate_natural_width(false);
            scroll.set_propagate_natural_height(false);
        }
        SizingPreference::FitToContent => {
            set_default_size_tracked(state, window, MIN_WIDTH, MIN_HEIGHT);
            scroll.set_visible(true);
            scroll.set_vexpand(false);
            scroll.set_hexpand(false);
            scroll.set_max_content_width(w_max);
            scroll.set_max_content_height(h_max);
            scroll.set_propagate_natural_width(true);
            scroll.set_propagate_natural_height(true);
        }
        SizingPreference::OwnsScroll => {
            // Plugin's widget contains its own scroll container plus
            // any non-scrolling chrome. Hide the host's outer scroll
            // so its container does not also scroll the chrome out
            // of view, and let the widget itself fill the cap.
            set_default_size_tracked(state, window, w_max, h_max);
            scroll.set_visible(false);
        }
    }
}

fn rebuild_header(
    state: &Rc<PreviewState>,
    hit: &Hit,
    app: &gtk::Application,
    plugin: Rc<dyn PreviewPlugin>,
    outbound_tx: &async_channel::Sender<PreviewEvent>,
) {
    let Some(header) = state.header_box.borrow().clone() else {
        return;
    };
    while let Some(child) = header.first_child() {
        header.remove(&child);
    }
    // Any launch-error strip from the previous document was removed
    // with the children above; drop the reference too so its pending
    // timeout recognises it as stale and does nothing (P10).
    state.error_strip.borrow_mut().take();

    let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
    text.set_hexpand(true);

    // halign (not xalign) so RTL locales mirror the header. The
    // ellipsis still triggers: a non-Fill label's allocation is
    // capped at min(natural, available).
    let title = gtk::Label::new(Some(&hit.title));
    title.set_widget_name("lixun-preview-title");
    title.set_halign(gtk::Align::Start);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    text.append(&title);

    if !hit.subtitle.is_empty() {
        let subtitle = gtk::Label::new(Some(&hit.subtitle));
        subtitle.set_widget_name("lixun-preview-subtitle");
        subtitle.set_halign(gtk::Align::Start);
        subtitle.set_ellipsize(gtk::pango::EllipsizeMode::End);
        text.append(&subtitle);
    }
    header.append(&text);

    if plugin.can_launch(hit) {
        let open_btn = gtk::Button::from_icon_name("document-open-symbolic");
        open_btn.set_tooltip_text(Some("Open (Enter)"));
        open_btn.set_widget_name("lixun-preview-open-btn");
        open_btn.add_css_class("flat");
        let plugin_for_click = Rc::clone(&plugin);
        let hit_for_click = hit.clone();
        let app_for_click = app.clone();
        let state_for_click = Rc::clone(state);
        let outbound_for_click = outbound_tx.clone();
        open_btn.connect_clicked(move |_| {
            run_plugin_launch(
                &plugin_for_click,
                &hit_for_click,
                &app_for_click,
                &state_for_click,
                &outbound_for_click,
            );
        });
        header.append(&open_btn);
    }

    // Humanized plugin name (falls back to the raw id via the trait
    // default); the host renders it verbatim and never branches on it.
    let plugin_badge = gtk::Label::new(Some(plugin.display_name()));
    plugin_badge.set_widget_name("lixun-preview-plugin-badge");
    header.append(&plugin_badge);
}

/// Delegate the launch to the plugin and notify the daemon. The
/// process stays warm afterwards (no `process::exit`); the daemon
/// learns about the close via `PreviewEvent::Closed{epoch}`, the
/// same path used by Escape/Space. The 60s idle timer then decides
/// whether the process actually exits.
fn run_plugin_launch(
    plugin: &Rc<dyn PreviewPlugin>,
    hit: &Hit,
    app: &gtk::Application,
    state: &Rc<PreviewState>,
    outbound_tx: &async_channel::Sender<PreviewEvent>,
) {
    match plugin.launch(hit) {
        Ok(()) => {
            tracing::info!(
                "preview: plugin `{}` launched hit_id={}",
                plugin.id(),
                hit.id.0
            );
            let epoch = state.current_epoch.get();
            let _ = outbound_tx.send_blocking(PreviewEvent::Launched { epoch });
            if let Some(window) = state.window.borrow().as_ref() {
                window.set_visible(false);
            }
            schedule_idle(state, app);
        }
        Err(e) => {
            tracing::error!(
                "preview: plugin `{}` launch failed for hit_id={}: {}",
                plugin.id(),
                hit.id.0,
                e
            );
            // A silent failure looks like a dead Enter key (P10):
            // surface the cause in the header. `{:#}` renders the
            // whole anyhow context chain down to the root cause.
            show_launch_error(state, &format!("Open failed: {e:#}"));
        }
    }
}

/// Mount a transient error strip in the preview header (P10). The
/// strip removes itself after [`ERROR_STRIP_TIMEOUT`]; a header
/// rebuild for the next document removes it earlier and invalidates
/// the pending timeout via the `error_strip` identity check, so a
/// strip can never linger onto a different document. Generic host
/// UI: the message comes from the plugin's error, never from any
/// knowledge of which plugin failed.
fn show_launch_error(state: &Rc<PreviewState>, message: &str) {
    let Some(header) = state.header_box.borrow().clone() else {
        return;
    };
    // Replace any strip still standing from an earlier failure so
    // repeated attempts don't stack labels.
    if let Some(prev) = state.error_strip.borrow_mut().take()
        && prev.parent().as_ref() == Some(header.upcast_ref())
    {
        header.remove(&prev);
    }

    let strip = gtk::Label::new(Some(&truncate_chars(message, ERROR_STRIP_MAX_CHARS)));
    strip.add_css_class("lixun-preview-error");
    strip.set_ellipsize(gtk::pango::EllipsizeMode::End);
    strip.set_tooltip_text(Some(message));
    header.append(&strip);
    *state.error_strip.borrow_mut() = Some(strip.clone());

    let state_for_timeout = Rc::clone(state);
    glib::timeout_add_local_once(ERROR_STRIP_TIMEOUT, move || {
        // Remove only the strip this timeout mounted: a later
        // rebuild_header (next document) or show_launch_error
        // (newer failure) already replaced it, and tearing down
        // the newer strip early would truncate its display time.
        let is_current = state_for_timeout
            .error_strip
            .borrow()
            .as_ref()
            .is_some_and(|current| current == &strip);
        if !is_current {
            return;
        }
        state_for_timeout.error_strip.borrow_mut().take();
        if let Some(parent) = strip.parent()
            && let Ok(container) = parent.downcast::<gtk::Box>()
        {
            container.remove(&strip);
        }
    });
}

/// Trim `text` to at most `max_chars` characters, appending an
/// ellipsis when truncated. Char-based, never byte-based, so
/// multi-byte input cannot be split mid code point. Pure for
/// headless unit testing.
fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let cut: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{cut}\u{2026}")
}

/// Describe the mounted content to assistive technology (A8). The
/// preview content is typically a flat rendered canvas that exposes
/// nothing useful to a screen reader on its own, so the host labels
/// the content area "{title}, {kind} preview". The kind comes from
/// the hit's own `kind_label` or, absent that, the plugin's
/// `display_name()` — both opaque data the host renders verbatim,
/// never plugin identity the host branches on. The label lands on
/// whichever container actually hosts the content: the plugin's
/// widget for OwnsScroll mounts, the outer ScrolledWindow otherwise
/// (the hidden counterpart is not rendered to the a11y tree).
fn apply_content_accessible_label(
    state: &Rc<PreviewState>,
    hit: &Hit,
    plugin: &Rc<dyn PreviewPlugin>,
) {
    let kind = hit
        .kind_label
        .clone()
        .unwrap_or_else(|| plugin.display_name().to_string());
    let text = content_accessible_label(&hit.title, &kind);
    let props = [gtk::accessible::Property::Label(&text)];
    if state.current_widget_owns_scroll.get() {
        if let Some(widget) = state.current_widget.borrow().as_ref() {
            widget.update_property(&props);
        }
    } else if let Some(scroll) = state.content_scroll.borrow().as_ref() {
        scroll.update_property(&props);
    }
}

/// Pure formatter for the content-area accessible label (A8); split
/// out for headless unit testing.
fn content_accessible_label(title: &str, kind: &str) -> String {
    format!("{title}, {kind} preview")
}

/// Resolve which monitor the preview window should open on.
///
/// Order:
/// 1. `requested` connector name from the IPC `ShowOrUpdate.monitor`
///    field — the launcher's current monitor. This is the canonical
///    path; the daemon recomputes it on every Space press so the
///    preview always opens where the launcher lives.
/// 2. Pointer-position fallback for direct invocation without a
///    daemon (manual debug runs).
/// 3. First monitor.
fn pick_monitor(display: &gtk::gdk::Display, requested: Option<&str>) -> Option<gtk::gdk::Monitor> {
    if let Some(name) = requested
        && !name.is_empty()
    {
        let monitors = display.monitors();
        for i in 0..monitors.n_items() {
            if let Some(obj) = monitors.item(i)
                && let Ok(monitor) = obj.downcast::<gtk::gdk::Monitor>()
                && let Some(connector) = monitor.connector()
                && connector.as_str() == name
            {
                return Some(monitor);
            }
        }
        tracing::warn!(
            "preview: requested monitor connector `{}` did not match; falling back",
            name
        );
    }

    if let Some(seat) = display.default_seat()
        && let Some(pointer) = seat.pointer()
    {
        let surface_under_pointer = pointer.surface_at_position();
        if let Some(surface) = surface_under_pointer.0
            && let Some(monitor) = display.monitor_at_surface(&surface)
        {
            return Some(monitor);
        }
    }
    display.monitors().item(0).and_then(|m| m.downcast().ok())
}

/// Install the keyboard controller for Escape/Space/Enter.
///
/// Capture phase is mandatory. In bubble phase a focused `gtk::Button`
/// — and the header's Open button takes focus by default — would
/// consume Space as "activate me" before this controller sees it,
/// turning Space into "open the file" instead of "close the
/// preview". Capture phase preempts every child widget's keyboard
/// default and preserves the close-on-Space contract.
///
/// No `EventControllerFocus` is installed: under `KeyboardMode::
/// OnDemand` `connect_leave` is unreliable, and the warm-process
/// model forbids quitting on focus loss anyway — the only ways out
/// are explicit `Close` IPC from the daemon and the 60s idle timer.
fn install_close_controllers(
    window: &gtk::ApplicationWindow,
    app: &gtk::Application,
    state: &Rc<PreviewState>,
    outbound_tx: &async_channel::Sender<PreviewEvent>,
) {
    let key = gtk::EventControllerKey::new();
    key.set_propagation_phase(gtk::PropagationPhase::Capture);
    let app_for_key = app.clone();
    let state_for_key = Rc::clone(state);
    let outbound_for_key = outbound_tx.clone();
    let outbound_for_keyclose = outbound_tx.clone();
    key.connect_key_pressed(move |_, keyval, _keycode, _state| {
        let sym = keyval.name().map(|g| g.to_string()).unwrap_or_default();
        match sym.as_str() {
            "Escape" | "space" => {
                close_via_keyboard(&state_for_key, &app_for_key, &outbound_for_keyclose);
                glib::Propagation::Stop
            }
            // Arrow keys are result-scrub, not content navigation
            // (P1 keyboard continuity): while the preview toplevel
            // holds the seat keyboard, relay Up/Down to the daemon
            // as NavKey so the launcher moves its selection and the
            // 50 ms selection→preview pipeline updates this window.
            // Capture phase on purpose — no plugin widget may steal
            // the scrub keys.
            "Up" | "KP_Up" => {
                let _ = outbound_for_key.send_blocking(PreviewEvent::NavKey {
                    epoch: state_for_key.current_epoch.get(),
                    delta: -1,
                });
                glib::Propagation::Stop
            }
            "Down" | "KP_Down" => {
                let _ = outbound_for_key.send_blocking(PreviewEvent::NavKey {
                    epoch: state_for_key.current_epoch.get(),
                    delta: 1,
                });
                glib::Propagation::Stop
            }
            "Return" | "KP_Enter" => {
                // Enter inside preview: launch the current hit via
                // the plugin, same as the Open button. Previously
                // this branch degenerated to close because the key
                // controller had no access to the outbound channel;
                // threading `outbound_tx` through
                // `build_window_skeleton` fixes that. If the plugin
                // can't launch this hit, fall back to closing.
                let plugin = state_for_key.current_plugin.borrow().clone();
                let hit = state_for_key.current_hit.borrow().clone();
                if let (Some(plugin), Some(hit)) = (plugin, hit) {
                    if plugin.can_launch(&hit) {
                        run_plugin_launch(
                            &plugin,
                            &hit,
                            &app_for_key,
                            &state_for_key,
                            &outbound_for_key,
                        );
                    } else {
                        close_via_keyboard(&state_for_key, &app_for_key, &outbound_for_keyclose);
                    }
                } else {
                    close_via_keyboard(&state_for_key, &app_for_key, &outbound_for_keyclose);
                }
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        }
    });
    window.add_controller(key);

    // Window-manager close (titlebar X, Alt+F4, compositor close).
    // Now that the preview is a decorated xdg-toplevel, the user can
    // dismiss it via the system close button. We must tell the daemon
    // so it exits preview mode in the launcher; otherwise the launcher
    // stays in preview_mode_active and re-opens the preview on every
    // arrow-key navigation. We hide+idle (mirroring close_via_keyboard)
    // and return Stop to keep the warm process alive for the next
    // ShowOrUpdate.
    let state_for_close = Rc::clone(state);
    let app_for_close = app.clone();
    let outbound_for_close = outbound_tx.clone();
    window.connect_close_request(move |_| {
        let epoch = state_for_close.current_epoch.get();
        if state_for_close.launcher_hidden_by_us.get() {
            let _ = outbound_for_close
                .send_blocking(PreviewEvent::SetLauncherVisible { visible: true });
            state_for_close.launcher_hidden_by_us.set(false);
        }
        let activation_token = mint_activation_token();
        let _ = outbound_for_close.send_blocking(PreviewEvent::Closed {
            epoch,
            activation_token,
        });
        if let Some(window) = state_for_close.window.borrow().as_ref() {
            window.set_visible(false);
        }
        schedule_idle(&state_for_close, &app_for_close);
        glib::Propagation::Stop
    });

    // Bubble-phase paging fallthrough (P7/P11): runs only when the
    // focused widget did NOT consume the key, so plugins with their
    // own PgUp/PgDn wiring (paginated viewers) keep first claim and
    // everything else still pages the outer scroll container.
    let page_key = gtk::EventControllerKey::new();
    page_key.set_propagation_phase(gtk::PropagationPhase::Bubble);
    let state_for_page = Rc::clone(state);
    page_key.connect_key_pressed(move |_, keyval, _keycode, _state| {
        let sym = keyval.name().map(|g| g.to_string()).unwrap_or_default();
        match sym.as_str() {
            "Page_Up" | "KP_Page_Up" => {
                scroll_content(&state_for_page, ScrollRequest::PageUp, 1);
                glib::Propagation::Stop
            }
            "Page_Down" | "KP_Page_Down" => {
                scroll_content(&state_for_page, ScrollRequest::PageDown, 1);
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        }
    });
    window.add_controller(page_key);
}

/// Apply a page-scroll to the current preview content (P11). The
/// active plugin gets first refusal through its `scroll` hook (the
/// only route for `OwnsScroll` widgets); on `false` the host drives
/// its own outer ScrolledWindow by `pages` viewport-heights, clamped
/// to the adjustment bounds.
fn scroll_content(state: &Rc<PreviewState>, request: ScrollRequest, pages: u32) {
    let handled = {
        let plugin = state.current_plugin.borrow();
        let widget = state.current_widget.borrow();
        match (plugin.as_ref(), widget.as_ref()) {
            (Some(plugin), Some(widget)) => plugin.scroll(widget, request, pages),
            _ => false,
        }
    };
    if handled {
        return;
    }
    let scroll_ref = state.content_scroll.borrow();
    let Some(scroll) = scroll_ref.as_ref() else {
        return;
    };
    if !scroll.is_visible() {
        // OwnsScroll mount: the outer container is hidden and empty;
        // nothing generic left to move.
        return;
    }
    let adj = scroll.vadjustment();
    let delta = adj.page_size() * f64::from(pages);
    let target = match request {
        ScrollRequest::PageUp => adj.value() - delta,
        ScrollRequest::PageDown => adj.value() + delta,
    };
    adj.set_value(clamp_scroll_value(
        target,
        adj.lower(),
        adj.upper(),
        adj.page_size(),
    ));
}

/// Clamp a prospective vadjustment value to the scrollable range.
/// GtkAdjustment does this internally too; duplicated as a pure
/// function so the paging math is unit-testable headlessly.
fn clamp_scroll_value(target: f64, lower: f64, upper: f64, page_size: f64) -> f64 {
    target.clamp(lower, (upper - page_size).max(lower))
}

/// Rebuild the bottom hint strip from the active plugin's declared
/// capabilities (P7). Capability flags only — never plugin identity.
fn update_hints(state: &Rc<PreviewState>, plugin: &Rc<dyn PreviewPlugin>, hit: &Hit) {
    let Some(hints) = state.hints_label.borrow().clone() else {
        return;
    };
    let caps = plugin.capabilities();
    let owns_scroll = matches!(plugin.sizing(), SizingPreference::OwnsScroll);
    let mut parts: Vec<&str> = vec!["\u{2191}\u{2193} results"];
    if caps.paginated || !owns_scroll {
        parts.push("PgUp/PgDn pages");
    }
    if caps.zoomable {
        parts.push("Ctrl\u{b1} zoom");
    }
    if caps.text_search {
        parts.push("Ctrl+F search");
    }
    if plugin.can_launch(hit) {
        parts.push("\u{21b5} open");
    }
    parts.push("Esc close");
    hints.set_text(&parts.join(" \u{b7} "));
}

/// Mint an xdg-activation token from the seat keyboard the preview
/// currently owns. KWin only grants the seat to a layer-shell surface
/// (the launcher) on an explicit activation request, never on its own
/// when a sibling xdg-toplevel hides; the launcher consumes this token
/// to reactivate itself. Must be called while the preview surface is
/// still mapped and focused — i.e. before `set_visible(false)`.
fn mint_activation_token() -> Option<String> {
    let display = gtk::gdk::Display::default()?;
    let ctx = display.app_launch_context();
    ctx.startup_notify_id(None::<&gtk::gio::AppInfo>, &[] as &[gtk::gio::File])
        .map(|s| s.to_string())
}

/// Hide window + start idle timer for Escape/Space keyboard close.
/// Intentionally does NOT send `PreviewEvent::Closed` — the launcher
/// treats Escape as its own preview-exit (resetting
/// `preview_mode_active` and returning focus to the entry), and the
/// daemon catches up via that path. Enter/KP_Enter, by contrast,
/// goes through `run_plugin_launch` which DOES send Closed, because
/// a launched hit is a real user decision the daemon must record.
fn close_via_keyboard(
    state: &Rc<PreviewState>,
    app: &gtk::Application,
    outbound_tx: &async_channel::Sender<PreviewEvent>,
) {
    let epoch = state.current_epoch.get();
    if state.launcher_hidden_by_us.get() {
        let _ = outbound_tx.send_blocking(PreviewEvent::SetLauncherVisible { visible: true });
        state.launcher_hidden_by_us.set(false);
    }
    let activation_token = mint_activation_token();
    let _ = outbound_tx.send_blocking(PreviewEvent::Closed {
        epoch,
        activation_token,
    });
    if let Some(window) = state.window.borrow().as_ref() {
        window.set_visible(false);
    }
    schedule_idle(state, app);
}

/// (Re)arm the 60s warm-process idle timer. Cancels any prior
/// timer first, then registers a single-shot `glib::timeout` that
/// calls `app.quit()` when fired. Each ShowOrUpdate cancels this
/// timer; each Close re-arms it. The number 60s mirrors macOS
/// `quicklookd`'s warm-process window — long enough that arrow-key
/// browsing keeps the same preview process, short enough that an
/// idle session doesn't pin RAM forever.
fn schedule_idle(state: &Rc<PreviewState>, app: &gtk::Application) {
    cancel_idle(state);
    let app = app.clone();
    let id = glib::timeout_add_local_once(IDLE_TIMEOUT, move || {
        tracing::info!("preview: idle timeout fired, quitting");
        app.quit();
    });
    *state.idle_source.borrow_mut() = Some(id);
}

fn cancel_idle(state: &Rc<PreviewState>) {
    if let Some(id) = state.idle_source.borrow_mut().take() {
        id.remove();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        SizingPreference, clamp_scroll_value, content_accessible_label, launcher_clears_centered_preview,
        launcher_fits_beside, should_apply_sizing, truncate_chars,
    };

    #[test]
    fn sizing_applies_on_first_show() {
        // No previous class recorded: always size the window.
        assert!(should_apply_sizing(
            None,
            SizingPreference::FixedCap,
            false,
            false
        ));
        assert!(should_apply_sizing(
            None,
            SizingPreference::FitToContent,
            true,
            false
        ));
    }

    #[test]
    fn sizing_applies_on_class_change_even_when_user_resized() {
        // Class change re-enables host sizing; the caller clears the
        // latch, but the decision must not depend on that ordering.
        assert!(should_apply_sizing(
            Some(SizingPreference::FitToContent),
            SizingPreference::FixedCap,
            true,
            true
        ));
        assert!(should_apply_sizing(
            Some(SizingPreference::FixedCap),
            SizingPreference::OwnsScroll,
            true,
            false
        ));
    }

    #[test]
    fn sizing_skipped_for_same_class_on_mapped_window() {
        // The anti-thrash core: arrow-scrub across same-class hits
        // must not rewrite the window size.
        assert!(!should_apply_sizing(
            Some(SizingPreference::FixedCap),
            SizingPreference::FixedCap,
            true,
            false
        ));
        assert!(!should_apply_sizing(
            Some(SizingPreference::FitToContent),
            SizingPreference::FitToContent,
            true,
            false
        ));
    }

    #[test]
    fn sizing_reapplied_on_remap_unless_user_resized() {
        // Reopening after Close (window unmapped): re-apply for the
        // same class — unless the user resized, in which case their
        // size persists until the class changes.
        assert!(should_apply_sizing(
            Some(SizingPreference::FixedCap),
            SizingPreference::FixedCap,
            false,
            false
        ));
        assert!(!should_apply_sizing(
            Some(SizingPreference::FixedCap),
            SizingPreference::FixedCap,
            false,
            true
        ));
    }

    #[test]
    fn truncate_chars_passes_short_text_through() {
        assert_eq!(truncate_chars("short", 10), "short");
        assert_eq!(truncate_chars("", 10), "");
    }

    #[test]
    fn truncate_chars_appends_ellipsis_and_respects_char_boundaries() {
        let long = "a".repeat(20);
        let out = truncate_chars(&long, 10);
        assert_eq!(out.chars().count(), 10);
        assert!(out.ends_with('\u{2026}'));
        // Multi-byte input must not split a code point.
        let unicode = "\u{e9}".repeat(20);
        let out = truncate_chars(&unicode, 10);
        assert_eq!(out.chars().count(), 10);
    }

    #[test]
    fn content_accessible_label_format() {
        assert_eq!(
            content_accessible_label("report.pdf", "PDF"),
            "report.pdf, PDF preview"
        );
    }

    #[test]
    fn fits_beside_mirrors_launcher_slide_math() {
        // 1920 monitor, 50% preview (960): column = 944 holds a
        // 720 launcher plus 16px gaps on both sides.
        assert!(launcher_fits_beside(1920, 720, 960));
        // 1366 laptop panel, 683 preview: column = 667 < 720 + 32.
        assert!(!launcher_fits_beside(1366, 720, 683));
    }

    #[test]
    fn clears_centered_preview_hides_on_laptop_panel() {
        // The reported field case: 1646 scaled panel, 654 launcher,
        // 823 (50%) preview. Widths sum below the monitor, so
        // fits_beside (overlay model) says "fits" — but the WM
        // centres the preview at left edge (1646-823)/2 = 411, and
        // the left launcher's right edge is 16+654 = 670 > 411. The
        // centred model must return false so the launcher hides.
        assert!(launcher_fits_beside(1646, 654, 823)); // overlay model: fits
        assert!(!launcher_clears_centered_preview(1646, 654, 823)); // window model: overlaps
    }

    #[test]
    fn clears_centered_preview_keeps_launcher_on_wide_monitor() {
        // 3440 ultrawide: centred 1720 preview starts at 860; a
        // left launcher (16 + 720) = 736, +16 gap = 752 <= 860.
        assert!(launcher_clears_centered_preview(3440, 720, 1720));
    }

    #[test]
    fn clears_centered_preview_floors_launcher_width() {
        // Sub-floor launcher width still reserves the 480 minimum.
        assert!(!launcher_clears_centered_preview(1646, 100, 823));
    }

    #[test]
    fn clears_centered_preview_rejects_degenerate_sizes() {
        assert!(!launcher_clears_centered_preview(0, 720, 960));
        assert!(!launcher_clears_centered_preview(1920, 720, 0));
    }

    #[test]
    fn fits_beside_floors_launcher_width() {
        // A 100px launcher reading clamps to the 480 floor: the
        // column must still hold 480 + 2x16.
        assert!(!launcher_fits_beside(1000, 100, 500)); // column 484
        assert!(launcher_fits_beside(1090, 100, 550)); // column 524
    }

    #[test]
    fn fits_beside_rejects_degenerate_sizes() {
        assert!(!launcher_fits_beside(0, 720, 960));
        assert!(!launcher_fits_beside(1920, 720, 0));
        assert!(!launcher_fits_beside(-1, 720, 960));
    }

    #[test]
    fn scroll_clamp_stays_within_range() {
        // Page down past the end clamps to upper - page_size.
        assert_eq!(clamp_scroll_value(950.0, 0.0, 1000.0, 100.0), 900.0);
        // Page up past the start clamps to lower.
        assert_eq!(clamp_scroll_value(-40.0, 0.0, 1000.0, 100.0), 0.0);
        // In-range value passes through.
        assert_eq!(clamp_scroll_value(300.0, 0.0, 1000.0, 100.0), 300.0);
    }

    #[test]
    fn scroll_clamp_handles_content_shorter_than_viewport() {
        // upper - page_size would be negative; clamp to lower.
        assert_eq!(clamp_scroll_value(50.0, 0.0, 80.0, 100.0), 0.0);
    }
}
