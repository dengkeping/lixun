//! `lixun-preview` binary.
//!
//! Short-lived companion process spawned by `lixund` when the user
//! hits Space on a result row. Reads a `Hit` from a tempfile,
//! deletes the tempfile (child-owns-cleanup — survives daemon
//! crash), picks a plugin via `lixun_preview::select_plugin`, and
//! shows a layer-shell overlay with the plugin's widget.
//!
//! Closes on Escape, Space, or focus-loss. Exits 0 on clean close;
//! non-zero on setup error (logged by daemon's child-watcher).

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use gtk::gio::ApplicationFlags;
use gtk::glib;
use gtk::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use lixun_core::Hit;
use lixun_preview::{
    PreviewPlugin, PreviewPluginCfg, SizingPreference, install_user_css, select_plugin,
};

use lixun_preview_bundle as _;

#[cfg(feature = "stub")]
mod stub;

const APP_ID: &str = "hk.dkp.lixun.preview";
const DEFAULT_WIDTH: i32 = 960;
const DEFAULT_HEIGHT: i32 = 720;
const MIN_WIDTH: i32 = 600;
const MIN_HEIGHT: i32 = 400;
const FOCUS_LEAVE_LATCH: Duration = Duration::from_millis(150);

/// Process exit code used when the user launched the hit's default
/// application from within the preview (Enter, Open button). The
/// daemon's `PreviewSpawner` watches the child's exit status and
/// treats this code as "task completed — do NOT re-show the
/// launcher". Any other exit (0 for plain close, non-zero for
/// crash, any signal) means the user dismissed the preview without
/// launching, and the launcher should reappear with its persisted
/// session state. Chosen as 42 to avoid collision with GTK's
/// typical exit codes (0, 1) and standard Unix signal+128 codes.
const EXIT_CODE_LAUNCHED: i32 = 42;

#[derive(Parser, Debug)]
#[command(
    name = "lixun-preview",
    about = "Short-lived preview window for a single hit"
)]
struct Args {
    /// Path to a JSON file containing the serialized Hit. The file
    /// is deleted by this process immediately after reading, so
    /// tempfile cleanup survives a daemon crash.
    #[arg(long)]
    hit_json: PathBuf,
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
    let hit = read_and_discard_hit(&args.hit_json)
        .with_context(|| format!("reading hit JSON from {:?}", args.hit_json))?;

    let daemon_cfg = lixun_daemon::config::Config::load()?;
    let preview_cfg = Rc::new(daemon_cfg.preview);
    let gui_cfg = Rc::new(daemon_cfg.gui);
    let hit = Rc::new(hit);

    let app = gtk::Application::new(Some(APP_ID), ApplicationFlags::NON_UNIQUE);

    {
        let hit = Rc::clone(&hit);
        let preview_cfg = Rc::clone(&preview_cfg);
        let gui_cfg = Rc::clone(&gui_cfg);
        app.connect_activate(move |app| {
            if let Err(e) = build_preview_window(app, &hit, &preview_cfg, &gui_cfg) {
                tracing::error!("preview: build_preview_window failed: {}", e);
                app.quit();
            }
        });
    }

    app.run_with_args::<&str>(&[]);
    Ok(())
}

fn read_and_discard_hit(path: &PathBuf) -> Result<Hit> {
    let content = std::fs::read_to_string(path)?;
    if let Err(e) = std::fs::remove_file(path) {
        tracing::warn!(
            "preview: failed to remove hit tempfile {:?}: {} (continuing)",
            path,
            e
        );
    }
    let hit: Hit = serde_json::from_str(&content)?;
    Ok(hit)
}

fn build_preview_window(
    app: &gtk::Application,
    hit: &Hit,
    preview_cfg: &lixun_daemon::config::PreviewConfig,
    gui_cfg: &lixun_daemon::config::GuiConfig,
) -> Result<()> {
    let Some(plugin) = select_plugin(hit) else {
        tracing::info!(
            "preview: no plugin matches hit id={} category={:?}, exiting",
            hit.id.0,
            hit.category
        );
        app.quit();
        return Ok(());
    };

    // Shared between the renderer path (build), the header's Open
    // button click, and the key controller's Enter branch. Rc is
    // enough: preview-bin is single-threaded on the GTK main
    // context.
    let plugin: Rc<dyn PreviewPlugin> = Rc::from(plugin);
    let plugin_id = plugin.id();
    let plugin_cfg = PreviewPluginCfg {
        section: preview_cfg.plugin_sections.get(plugin_id),
        max_file_size_mb: preview_cfg.max_file_size_mb,
    };

    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .decorated(false)
        .default_width(DEFAULT_WIDTH)
        .default_height(DEFAULT_HEIGHT)
        .build();
    window.set_widget_name("lixun-preview-root");

    window.init_layer_shell();
    window.set_layer(Layer::Overlay);
    window.set_anchor(Edge::Top, false);
    window.set_anchor(Edge::Left, false);
    window.set_anchor(Edge::Right, false);
    window.set_anchor(Edge::Bottom, false);
    window.set_keyboard_mode(KeyboardMode::Exclusive);

    let display = gtk::gdk::Display::default()
        .ok_or_else(|| anyhow::anyhow!("no default GDK display"))?;

    // Compute the effective cap (w_max, h_max) from config and the
    // active monitor. This is the configured ceiling regardless of
    // sizing strategy; FixedCap uses it as the default window size,
    // FitToContent uses it as the ScrolledWindow max_content clamp.
    let (w_max, h_max) = if let Some(monitor) = pick_monitor(&display) {
        window.set_monitor(Some(&monitor));
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
    };

    let sizing = plugin.sizing();
    match sizing {
        SizingPreference::FixedCap => {
            window.set_default_size(w_max, h_max);
        }
        SizingPreference::FitToContent => {
            // Start minimal; content will push the window up to cap
            // via the ScrolledWindow's propagate_natural_* below.
            window.set_default_size(MIN_WIDTH, MIN_HEIGHT);
        }
    }

    install_user_css(&display);

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let header = build_header(hit, plugin_id, app, Rc::clone(&plugin));
    vbox.append(&header);

    let content_scroll = gtk::ScrolledWindow::new();
    content_scroll.set_widget_name("lixun-preview-content");
    match sizing {
        SizingPreference::FixedCap => {
            // Scroll fills window; content bigger than viewport scrolls.
            // This is the historical default for media plugins.
            content_scroll.set_vexpand(true);
            content_scroll.set_hexpand(true);
        }
        SizingPreference::FitToContent => {
            // propagate_natural_* + max_content_* is the GTK4 idiom
            // for 'natural-size up to cap'. The scroll asks its
            // child for its natural size, clamps at (w_max, h_max),
            // and reports that to the window — which, with a MIN
            // default size, shrinks to fit. Expand flags are dropped
            // so the scroll's size-request is honest, not
            // whatever-the-parent-gives-it.
            content_scroll.set_max_content_width(w_max);
            content_scroll.set_max_content_height(h_max);
            content_scroll.set_propagate_natural_width(true);
            content_scroll.set_propagate_natural_height(true);
        }
    }

    match plugin.build(hit, &plugin_cfg) {
        Ok(widget) => content_scroll.set_child(Some(&widget)),
        Err(e) => {
            tracing::error!("preview: plugin `{}` build failed: {}", plugin_id, e);
            let err_label = gtk::Label::new(Some(&format!(
                "Preview failed ({}):\n{}",
                plugin_id, e
            )));
            err_label.set_wrap(true);
            err_label.set_margin_top(24);
            err_label.set_margin_bottom(24);
            err_label.set_margin_start(24);
            err_label.set_margin_end(24);
            content_scroll.set_child(Some(&err_label));
        }
    }

    vbox.append(&content_scroll);
    window.set_child(Some(&vbox));

    let launchable = plugin.can_launch(hit);
    install_close_controllers(
        &window,
        app,
        if launchable {
            Some((Rc::clone(&plugin), hit.clone()))
        } else {
            None
        },
    );

    window.present();
    tracing::info!(
        "preview: window mapped plugin={} hit_id={}",
        plugin_id,
        hit.id.0
    );
    Ok(())
}

fn build_header(
    hit: &Hit,
    plugin_id: &str,
    app: &gtk::Application,
    plugin: Rc<dyn PreviewPlugin>,
) -> gtk::Box {
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    header.set_widget_name("lixun-preview-header");
    header.set_margin_top(12);
    header.set_margin_bottom(8);
    header.set_margin_start(16);
    header.set_margin_end(16);

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
        open_btn.connect_clicked(move |_| {
            run_plugin_launch(&plugin_for_click, &hit_for_click, &app_for_click);
        });
        header.append(&open_btn);
    }

    let plugin_badge = gtk::Label::new(Some(plugin_id));
    plugin_badge.set_widget_name("lixun-preview-plugin-badge");
    header.append(&plugin_badge);

    header
}

/// Delegate the launch to the plugin that rendered the hit. The
/// plugin owns the launch semantics for its domain; the host binary
/// stays agnostic and does not inspect the action. On success, exit
/// with `EXIT_CODE_LAUNCHED` so the daemon's PreviewSpawner
/// dispatches `GuiCommand::ClearSession` instead of re-showing the
/// launcher. On failure, keep the window open and let the user
/// Escape cleanly.
fn run_plugin_launch(plugin: &Rc<dyn PreviewPlugin>, hit: &Hit, app: &gtk::Application) {
    match plugin.launch(hit) {
        Ok(()) => {
            tracing::info!(
                "preview: plugin `{}` launched hit_id={}",
                plugin.id(),
                hit.id.0
            );
            app.quit();
            std::process::exit(EXIT_CODE_LAUNCHED);
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
/// Order of preference:
/// 1. `LIXUN_PREVIEW_MONITOR` env var set by the daemon — contains
///    the launcher's current monitor connector name (`"eDP-1"`,
///    `"DP-2"`, …). Linear-scan `display.monitors()` for the
///    matching `connector()` and return it. This is the correct
///    path for normal launcher→Space flow.
/// 2. Pointer position — legacy fallback. Unreliable in a fresh
///    preview process because `pointer.surface_at_position()`
///    intersects only the process's own surfaces, and we have
///    none until `window.present()`. Kept for direct `lixun-preview
///    --hit-json` invocation where no daemon set the env var.
/// 3. First monitor in `display.monitors()`.
fn pick_monitor(display: &gtk::gdk::Display) -> Option<gtk::gdk::Monitor> {
    if let Ok(requested) = std::env::var("LIXUN_PREVIEW_MONITOR")
        && !requested.is_empty()
    {
        let monitors = display.monitors();
        for i in 0..monitors.n_items() {
            if let Some(obj) = monitors.item(i)
                && let Ok(monitor) = obj.downcast::<gtk::gdk::Monitor>()
                && let Some(connector) = monitor.connector()
                && connector.as_str() == requested
            {
                tracing::info!("preview: monitor matched connector={}", requested);
                return Some(monitor);
            }
        }
        tracing::warn!(
            "preview: LIXUN_PREVIEW_MONITOR={} did not match any connector; falling back",
            requested
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

fn install_close_controllers(
    window: &gtk::ApplicationWindow,
    app: &gtk::Application,
    launchable: Option<(Rc<dyn PreviewPlugin>, Hit)>,
) {
    let showed_at = Rc::new(RefCell::new(Instant::now()));

    {
        let showed_at = Rc::clone(&showed_at);
        window.connect_map(move |_| {
            *showed_at.borrow_mut() = Instant::now();
        });
    }

    let key = gtk::EventControllerKey::new();
    // Capture phase is mandatory here. In the default (bubble) phase,
    // a focused gtk::Button — and the header's Open button becomes
    // focused by default when the window is mapped — consumes
    // Space as "activate me" before the event reaches this window-
    // level controller, so Space launches the file instead of
    // closing the preview. By running on Capture the preview's own
    // Escape/Space/Enter mapping preempts every child widget's
    // keyboard default, preserving the close-on-Space contract even
    // when the Open button has focus.
    key.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let app = app.clone();
        key.connect_key_pressed(move |_, keyval, _keycode, _state| {
            let sym = keyval.name().map(|g| g.to_string()).unwrap_or_default();
            match sym.as_str() {
                "Escape" | "space" => {
                    app.quit();
                    glib::Propagation::Stop
                }
                "Return" | "KP_Enter" => {
                    if let Some((plugin, hit)) = launchable.as_ref() {
                        run_plugin_launch(plugin, hit, &app);
                    } else {
                        app.quit();
                    }
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
    }
    window.add_controller(key);

    let focus = gtk::EventControllerFocus::new();
    {
        let app = app.clone();
        let showed_at = Rc::clone(&showed_at);
        focus.connect_leave(move |_| {
            if showed_at.borrow().elapsed() < FOCUS_LEAVE_LATCH {
                return;
            }
            app.quit();
        });
    }
    window.add_controller(focus);
}
