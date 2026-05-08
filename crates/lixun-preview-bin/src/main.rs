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
use lixun_ipc::preview::{PreviewCommand, PreviewEvent, read_frame_sync, write_frame_sync};
use lixun_preview::{
    PreviewPlugin, PreviewPluginCfg, SizingPreference, UPDATE_UNSUPPORTED, install_user_css,
    select_plugin,
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

/// How long the preview process stays warm after the user dismisses
/// the preview (Escape, Space, or daemon-driven Close). Mirrors the
/// macOS QuickLook daemon (`quicklookd`) policy: cold-start cost is
/// paid once per Space session, but a brief lull does not kill the
/// process. After this elapses with no new `ShowOrUpdate`, the
/// process self-quits and the next Space pays cold-start again.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

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
    #[arg(long)]
    socket_path: PathBuf,
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
    let daemon_cfg = lixun_daemon::config::Config::load()?;
    let preview_cfg = Rc::new(daemon_cfg.preview);
    let gui_cfg = Rc::new(daemon_cfg.gui);

    let listener = bind_listener(&args.socket_path)?;
    tracing::info!(
        "preview: listening on {:?} pid={}",
        args.socket_path,
        std::process::id()
    );

    let (inbound_tx, inbound_rx) = async_channel::unbounded::<InboundMsg>();
    let (outbound_tx, outbound_rx) = async_channel::unbounded::<PreviewEvent>();

    spawn_socket_workers(listener, inbound_tx, outbound_rx);

    let app = gtk::Application::new(Some(APP_ID), ApplicationFlags::NON_UNIQUE);
    let state = Rc::new(PreviewState::default());
    let socket_path = args.socket_path.clone();

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

    let _ = std::fs::remove_file(&socket_path);

    let code: i32 = exit_code.into();
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
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
    preview_cfg: &Rc<lixun_daemon::config::PreviewConfig>,
    gui_cfg: &Rc<lixun_daemon::config::GuiConfig>,
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
            if let Some(window) = state.window.borrow().as_ref() {
                window.set_visible(false);
            }
            let _ = outbound_tx.send_blocking(PreviewEvent::Closed { epoch });
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
            if let Some(window) = state.window.borrow().as_ref() {
                window.set_visible(false);
            }
            let _ = outbound_tx.send_blocking(PreviewEvent::Closed { epoch });
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
    preview_cfg: &Rc<lixun_daemon::config::PreviewConfig>,
    gui_cfg: &Rc<lixun_daemon::config::GuiConfig>,
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
        build_window_skeleton(app, state, &display, outbound_tx)?;
    }

    // Recompute monitor + cap on every command. Per Oracle: the
    // launcher's monitor may differ between Spaces and we must not
    // remember the first one.
    let (w_max, h_max) = apply_monitor_and_cap(state, &display, requested_monitor, gui_cfg);

    apply_sizing(state, plugin.sizing(), w_max, h_max);
    rebuild_header(state, hit, &plugin_id, app, Rc::clone(&plugin), outbound_tx);

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

    if let Some(window) = state.window.borrow().as_ref() {
        window.set_visible(true);
        window.present();
        try_apply_pending_parent(state, window);
    }

    tracing::info!(
        "preview: showed plugin={} hit_id={} epoch={}",
        plugin_id,
        hit.id.0,
        state.current_epoch.get()
    );
    Ok(())
}

/// Build the persistent xdg-toplevel window skeleton: ApplicationWindow
/// + vbox + header_box + content_scroll. Called once per process
/// lifetime; subsequent commands mutate the existing widgets.
///
/// The preview is a regular xdg-toplevel (not a layer-shell surface).
/// Stacking above the launcher is achieved via xdg-foreign-v2:
/// the launcher exports its toplevel handle, the daemon forwards it
/// as `PreviewCommand::SetParent`, and the preview imports it and
/// calls `set_parent_of` so the compositor stacks the preview as a
/// child of the launcher. See Phase 1 in
/// the rich-quicklook design notes for Phase 1.
///
/// Keyboard focus: under layer-shell we used `KeyboardMode::None` to
/// keep the preview keyboard-passive. xdg-toplevel has no equivalent
/// API; instead we rely on `set_can_focus(false)` plus the launcher
/// retaining its own `KeyboardMode::OnDemand` focus. The launcher's
/// existing keymap dispatches `PreviewCommand::Hide`/`Close` for the
/// user-visible close paths.
fn build_window_skeleton(
    app: &gtk::Application,
    state: &Rc<PreviewState>,
    display: &gtk::gdk::Display,
    outbound_tx: &async_channel::Sender<PreviewEvent>,
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
    vbox.append(&content_scroll);
    window.set_child(Some(&vbox));

    install_close_controllers(&window, app, state, outbound_tx);

    *state.header_box.borrow_mut() = Some(header_box);
    *state.content_scroll.borrow_mut() = Some(content_scroll);
    *state.vbox.borrow_mut() = Some(vbox);
    *state.window.borrow_mut() = Some(window);
    Ok(())
}

fn try_apply_pending_parent(state: &Rc<PreviewState>, window: &gtk::ApplicationWindow) {
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
    if let Some(imp) = importer.as_mut() {
        if let Err(e) = imp.import(&handle, &wl_surface) {
            tracing::warn!("preview: xdg-foreign import failed: {}", e);
        }
    }
}

fn apply_monitor_and_cap(
    state: &Rc<PreviewState>,
    display: &gtk::gdk::Display,
    requested: Option<&str>,
    gui_cfg: &Rc<lixun_daemon::config::GuiConfig>,
) -> (i32, i32) {
    let window_ref = state.window.borrow();
    let Some(_window) = window_ref.as_ref() else {
        return (MIN_WIDTH, MIN_HEIGHT);
    };
    if let Some(monitor) = pick_monitor(display, requested) {
        let geometry = monitor.geometry();
        let w = (geometry.width() * i32::from(gui_cfg.preview_width_percent) / 100)
            .min(gui_cfg.preview_max_width_px)
            .max(MIN_WIDTH);
        let h = (geometry.height() * i32::from(gui_cfg.preview_height_percent) / 100)
            .min(gui_cfg.preview_max_height_px)
            .max(MIN_HEIGHT);
        (w, h)
    } else {
        (MIN_WIDTH, MIN_HEIGHT)
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
            window.set_default_size(w_max, h_max);
            scroll.set_visible(true);
            scroll.set_vexpand(true);
            scroll.set_hexpand(true);
            scroll.set_propagate_natural_width(false);
            scroll.set_propagate_natural_height(false);
        }
        SizingPreference::FitToContent => {
            window.set_default_size(MIN_WIDTH, MIN_HEIGHT);
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
            window.set_default_size(w_max, h_max);
            scroll.set_visible(false);
        }
    }
}

fn rebuild_header(
    state: &Rc<PreviewState>,
    hit: &Hit,
    plugin_id: &str,
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

    let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
    text.set_hexpand(true);

    let title = gtk::Label::new(Some(&hit.title));
    title.set_widget_name("lixun-preview-title");
    title.set_xalign(0.0);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    text.append(&title);

    if !hit.subtitle.is_empty() {
        let subtitle = gtk::Label::new(Some(&hit.subtitle));
        subtitle.set_widget_name("lixun-preview-subtitle");
        subtitle.set_xalign(0.0);
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

    let plugin_badge = gtk::Label::new(Some(plugin_id));
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
        }
    }
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
        let _ = outbound_for_close.send_blocking(PreviewEvent::Closed { epoch });
        if let Some(window) = state_for_close.window.borrow().as_ref() {
            window.set_visible(false);
        }
        schedule_idle(&state_for_close, &app_for_close);
        glib::Propagation::Stop
    });
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
    let _ = outbound_tx.send_blocking(PreviewEvent::Closed { epoch });
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
