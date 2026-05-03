//! btop-style TUI dashboard for lixun daemon monitoring.

pub mod app;
pub mod config_mutation;
pub mod event;
pub mod log_entry;
pub mod tui;
pub mod ui;

pub use app::App;
pub use event::EventHandler;
pub use log_entry::{LogEntry, LogLevel};
