//! Widget for reindexing controls and progress.

use ratatui::{
    Frame,
    layout::Rect,
    widgets::{Block, Borders, Gauge},
    style::{Color, Style},
};

use crate::dashboard::app::{App, FocusedWidget};

/// Render the reindex controls widget showing progress and status.
pub fn render_reindex_controls(frame: &mut Frame, area: Rect, app: &App) {
    let is_focused = app.focused_widget == FocusedWidget::ReindexControls;
    let border_color = if is_focused { Color::Cyan } else { Color::Gray };

    let block = Block::default()
        .title("Reindex Controls")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    let (status_text, progress_ratio) = if app.reindex_in_progress {
        let elapsed = if let Some(started) = app.reindex_started {
            let now = chrono::Utc::now();
            let duration = now.signed_duration_since(started);
            format!(" ({}s elapsed)", duration.num_seconds())
        } else {
            String::new()
        };
        (format!("In Progress{}", elapsed), 0.5)
    } else if app.reindex_started.is_some() {
        ("Completed".to_string(), 1.0)
    } else {
        ("Idle".to_string(), 0.0)
    };

    let gauge = Gauge::default()
        .block(block)
        .gauge_style(
            Style::default()
                .fg(if app.reindex_in_progress {
                    Color::Yellow
                } else if app.reindex_started.is_some() {
                    Color::Green
                } else {
                    Color::Gray
                })
        )
        .ratio(progress_ratio)
        .label(status_text);

    frame.render_widget(gauge, area);
}
