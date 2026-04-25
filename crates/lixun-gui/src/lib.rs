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
mod keymap;
mod reaper;
mod status;
mod window;

pub fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("lixun_gui=info".parse().unwrap()),
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
