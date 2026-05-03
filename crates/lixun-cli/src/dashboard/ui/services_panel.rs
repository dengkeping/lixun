//! Services control widget showing OCR/semantic status and daemon restart hint.

use ratatui::{
    Frame,
    layout::Rect,
    widgets::{Block, Borders, Paragraph},
    style::{Color, Style, Modifier},
    text::{Line, Span},
};

use crate::dashboard::app::{App, FocusedWidget, RestartStatus, SemanticStatus};

/// Render the services control panel showing OCR/semantic status.
///
/// Displays:
/// - OCR status: ON (with queue stats) or OFF
/// - Semantic search status: ON or OFF
/// - Daemon restart hint: "Press 'r' to restart daemon"
///
/// Color coding:
/// - Green: enabled/active
/// - Gray: disabled
/// - Yellow: warnings (e.g., failed queue items)
pub fn render_services_panel(frame: &mut Frame, area: Rect, app: &App) {
    let is_focused = app.focused_widget == FocusedWidget::ServicesPanel;
    let border_color = if is_focused { Color::Cyan } else { Color::Gray };

    let block = Block::default()
        .title("Services")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    // Build status lines
    let mut lines = Vec::new();

    // OCR status line
    if let Some(ref stats) = app.ocr_stats {
        let mut ocr_line = vec![
            Span::styled("OCR: ", Style::default().fg(Color::White)),
            Span::styled("ON", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
        ];

        // Add queue stats
        let queue_info = format!(
            " (queue: {} pending, {} failed)",
            stats.queue_pending,
            stats.queue_failed
        );
        
        let queue_color = if stats.queue_failed > 0 {
            Color::Yellow
        } else {
            Color::Gray
        };
        
        ocr_line.push(Span::styled(queue_info, Style::default().fg(queue_color)));
        lines.push(Line::from(ocr_line));
    } else {
        lines.push(Line::from(vec![
            Span::styled("OCR: ", Style::default().fg(Color::White)),
            Span::styled("OFF", Style::default().fg(Color::Gray)),
        ]));
    }

    // Semantic status line
    let semantic_spans = match &app.semantic_status {
        SemanticStatus::On => vec![
            Span::styled("Semantic: ", Style::default().fg(Color::White)),
            Span::styled("ON", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
        ],
        SemanticStatus::Off => vec![
            Span::styled("Semantic: ", Style::default().fg(Color::White)),
            Span::styled("OFF", Style::default().fg(Color::Gray)),
        ],
        SemanticStatus::Warning(msg) => vec![
            Span::styled("Semantic: ", Style::default().fg(Color::White)),
            Span::styled("WARN", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::styled(format!(" ({})", msg), Style::default().fg(Color::Yellow)),
        ],
    };
    
    lines.push(Line::from(semantic_spans));

    // Empty line for spacing
    lines.push(Line::from(""));

    // Daemon restart status line
    let restart_status_text = match app.restart_status {
        RestartStatus::Idle => {
            if app.restart_pending {
                vec![
                    Span::styled("Daemon: ", Style::default().fg(Color::White)),
                    Span::styled("[Restarting...]", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                ]
            } else {
                vec![
                    Span::styled("Daemon: ", Style::default().fg(Color::White)),
                    Span::styled("Running", Style::default().fg(Color::Green)),
                ]
            }
        }
        RestartStatus::Restarting => vec![
            Span::styled("Daemon: ", Style::default().fg(Color::White)),
            Span::styled("[Restarting...]", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        ],
        RestartStatus::Reconnecting => vec![
            Span::styled("Daemon: ", Style::default().fg(Color::White)),
            Span::styled("[Reconnecting...]", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        ],
        RestartStatus::Failed(ref msg) => vec![
            Span::styled("Daemon: ", Style::default().fg(Color::White)),
            Span::styled(format!("[Failed: {}]", msg), Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
        ],
    };
    lines.push(Line::from(restart_status_text));

    // Restart hint (only show when idle and not pending)
    if matches!(app.restart_status, RestartStatus::Idle) && !app.restart_pending {
        lines.push(Line::from(vec![
            Span::styled("Press ", Style::default().fg(Color::DarkGray)),
            Span::styled("'r'", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::styled(" to restart daemon", Style::default().fg(Color::DarkGray)),
        ]));
    }

    let paragraph = Paragraph::new(lines)
        .block(block)
        .style(Style::default().fg(Color::White));

    frame.render_widget(paragraph, area);
}
