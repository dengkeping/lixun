//! Dashboard UI widgets.

pub mod socket_panel;
pub mod log_viewer;
pub mod query_input;
pub mod results_list;
pub mod reindex_controls;
pub mod index_stats;
pub mod services_panel;
pub mod legend;

pub use socket_panel::render_socket_panel;
pub use log_viewer::render_log_viewer;
pub use query_input::render_query_input;
pub use results_list::render_results_list;
pub use reindex_controls::render_reindex_controls;
pub use index_stats::render_index_stats;
pub use services_panel::render_services_panel;
pub use legend::render_legend;

use crate::dashboard::app::{App, FocusedWidget};
use ratatui::{
    layout::{Constraint, Direction, Layout},
    Frame,
};

/// Compose all dashboard widgets into a btop-style layout.
///
/// Layout structure:
/// ```text
/// ┌─────────────────────────────────────────────────────────┐
/// │ Socket │ Index Stats │ Services │ Reindex Controls     │  <- Top row (20%)
/// ├─────────────────────────────────────────────────────────┤
/// │ Logs (big window)                                       │  <- Logs (45%)
/// ├─────────────────────────────────────────────────────────┤
/// │ Results                                                 │  <- Results (23%)
/// ├─────────────────────────────────────────────────────────┤
/// │ Query Input                                             │  <- Query (5%)
/// ├─────────────────────────────────────────────────────────┤
/// │ Legend (keyboard shortcuts)                             │  <- Legend (7%)
/// └─────────────────────────────────────────────────────────┘
/// ```
pub fn render_dashboard(frame: &mut Frame, app: &mut App) {
    let main_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(15),
            Constraint::Percentage(45),
            Constraint::Percentage(25),
            Constraint::Percentage(10),
            Constraint::Percentage(5),
        ])
        .split(frame.area());

    let top_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
        ])
        .split(main_chunks[0]);

    app.socket_panel_rect = top_chunks[0];
    app.index_stats_rect = top_chunks[1];
    app.services_panel_rect = top_chunks[2];
    app.reindex_controls_rect = top_chunks[3];
    render_socket_panel(frame, top_chunks[0], app);
    render_index_stats(frame, top_chunks[1], app);
    render_services_panel(frame, top_chunks[2], app);
    render_reindex_controls(frame, top_chunks[3], app);

    app.log_viewer_rect = main_chunks[1];
    render_log_viewer(frame, main_chunks[1], app);

    app.results_list_rect = main_chunks[2];
    render_results_list(frame, main_chunks[2], app);

    app.query_input_rect = main_chunks[3];
    let query_focused = app.focused_widget == FocusedWidget::QueryInput;
    render_query_input(frame, main_chunks[3], app, query_focused);

    render_legend(frame, main_chunks[4], app);
}
