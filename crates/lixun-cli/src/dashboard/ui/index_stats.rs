//! Widget for displaying index statistics.

use ratatui::{
    Frame,
    layout::Rect,
    widgets::{Block, Borders, Paragraph},
    style::{Color, Style},
    text::{Line, Span},
};
use crate::dashboard::app::{App, FocusedWidget};

pub fn render_index_stats(frame: &mut Frame, area: Rect, app: &App) {
    let is_focused = app.focused_widget == FocusedWidget::IndexStats;
    let border_color = if is_focused { Color::Cyan } else { Color::Gray };

    let block = Block::default()
        .title("Index Stats")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    let mut lines = vec![
        Line::from(vec![
            Span::styled("Indexed Documents: ", Style::default().fg(Color::Yellow)),
            Span::raw(app.indexed_docs.to_string()),
        ]),
        Line::from(""),
    ];

    // Watcher stats
    if let Some(watcher) = &app.watcher {
        lines.push(Line::from(Span::styled("Watcher:", Style::default().fg(Color::Green))));
        lines.push(Line::from(vec![
            Span::raw("  Directories: "),
            Span::raw(watcher.directories.to_string()),
        ]));
        lines.push(Line::from(vec![
            Span::raw("  Excluded: "),
            Span::raw(watcher.excluded.to_string()),
        ]));
        lines.push(Line::from(vec![
            Span::raw("  Errors: "),
            Span::styled(
                watcher.errors.to_string(),
                if watcher.errors > 0 {
                    Style::default().fg(Color::Red)
                } else {
                    Style::default()
                },
            ),
        ]));
        lines.push(Line::from(vec![
            Span::raw("  Overflow Events: "),
            Span::styled(
                watcher.overflow_events.to_string(),
                if watcher.overflow_events > 0 {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default()
                },
            ),
        ]));
    } else {
        lines.push(Line::from(vec![
            Span::styled("Watcher: ", Style::default().fg(Color::Green)),
            Span::styled("N/A", Style::default().fg(Color::DarkGray)),
        ]));
    }

    lines.push(Line::from(""));

    // Writer stats
    if let Some(writer) = &app.writer {
        lines.push(Line::from(Span::styled("Writer:", Style::default().fg(Color::Magenta))));
        lines.push(Line::from(vec![
            Span::raw("  Commits: "),
            Span::raw(writer.commits.to_string()),
        ]));
        lines.push(Line::from(vec![
            Span::raw("  Last Commit Latency: "),
            Span::raw(format!("{}ms", writer.last_commit_latency_ms)),
        ]));
        lines.push(Line::from(vec![
            Span::raw("  Generation: "),
            Span::raw(writer.generation.to_string()),
        ]));
    } else {
        lines.push(Line::from(vec![
            Span::styled("Writer: ", Style::default().fg(Color::Magenta)),
            Span::styled("N/A", Style::default().fg(Color::DarkGray)),
        ]));
    }

    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, area);
}
