//! Lixun GUI — GTK4 + gtk4-layer-shell launcher window.
//!
//! A standalone binary that connects to the lixund daemon via IPC socket
//! and provides a Spotlight-like search interface.

use anyhow::Result;
use gtk::prelude::*;

mod actions;
mod attachments;
mod factory;
mod gui_server;
mod icons;
mod ipc;
mod kde_blur;
mod keymap;
mod reaper;
mod status;
mod window;

pub fn run() -> Result<()> {
    // RUST_LOG wins; fall back to lixun_gui=info only when env is unset/empty so
    // operators can still raise the level for diagnosis without recompiling.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("lixun_gui=info")),
        )
        .init();

    let app = gtk::Application::builder()
        .application_id("app.lixun.gui")
        .build();

    app.connect_activate(|app| {
        if let Err(e) = window::build_window(app) {
            tracing::error!("Failed to build window: {}", e);
        }
    });

    app.run_with_args(&Vec::<String>::new());
    Ok(())
}
