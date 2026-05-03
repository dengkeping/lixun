//! Socket status widget showing connection state, indexed docs, and memory usage.

use crate::dashboard::app::{App, FocusedWidget};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

/// Render the socket status panel showing connection state and daemon metrics.
pub fn render_socket_panel(frame: &mut Frame, area: Rect, app: &App) {
    let is_focused = app.focused_widget == FocusedWidget::SocketPanel;
    let border_color = if is_focused { Color::Cyan } else { Color::Gray };

    let block = Block::default()
        .title("Socket Status")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    // Connection status with color coding
    let status_color = if app.connected {
        Color::Green
    } else {
        Color::Red
    };
    let status_text = if app.connected {
        "Connected"
    } else {
        "Disconnected"
    };

    let mut lines = vec![
        Line::from(vec![
            Span::raw("Status: "),
            Span::styled(
                status_text,
                Style::default()
                    .fg(status_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(format!("Indexed docs: {}", app.indexed_docs)),
    ];

    // Add memory stats if available
    if let Some(ref mem) = app.memory {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Memory Usage:",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(format!(
            "  RSS: {}",
            format_bytes(mem.rss_bytes)
        )));
        lines.push(Line::from(format!(
            "  VmSize: {}",
            format_bytes(mem.vm_size_bytes)
        )));
        lines.push(Line::from(format!(
            "  VmPeak: {}",
            format_bytes(mem.vm_peak_bytes)
        )));
        if mem.vm_swap_bytes > 0 {
            lines.push(Line::from(format!(
                "  VmSwap: {}",
                format_bytes(mem.vm_swap_bytes)
            )));
        }
    }

    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, area);
}

/// Format bytes as human-readable string (KB, MB, GB).
fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.00 KB");
        assert_eq!(format_bytes(1536), "1.50 KB");
        assert_eq!(format_bytes(1024 * 1024), "1.00 MB");
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.00 GB");
        assert_eq!(format_bytes(1536 * 1024 * 1024), "1.50 GB");
    }
}
