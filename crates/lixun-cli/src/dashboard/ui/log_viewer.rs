//! Widget for viewing daemon logs.

use ratatui::{
    layout::Rect,
    style::{Color, Style, Modifier},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem},
    Frame,
};

use crate::dashboard::app::{App, FocusedWidget, WidgetMode};
use crate::dashboard::log_entry::LogLevel;

/// Render the log viewer widget showing scrollable daemon logs.
///
/// Displays filtered logs with color-coded level indicators.
/// The `app.log_scroll` offset controls which line is at the top of the viewport.
pub fn render_log_viewer(frame: &mut Frame, area: Rect, app: &App) {
    let is_focused = app.focused_widget == FocusedWidget::LogViewer;
    let border_color = if is_focused {
        match app.widget_mode {
            WidgetMode::Navigation => Color::Cyan,
            WidgetMode::Focused => Color::Green,
        }
    } else {
        Color::Gray
    };

    let mut title_spans = vec![
        Span::raw("Logs ["),
        Span::styled(
            "I",
            Style::default()
                .fg(if app.log_filter_info { Color::White } else { Color::DarkGray })
                .add_modifier(if app.log_filter_info { Modifier::BOLD } else { Modifier::empty() })
        ),
        Span::raw(" "),
        Span::styled(
            "W",
            Style::default()
                .fg(if app.log_filter_warn { Color::Yellow } else { Color::DarkGray })
                .add_modifier(if app.log_filter_warn { Modifier::BOLD } else { Modifier::empty() })
        ),
        Span::raw(" "),
        Span::styled(
            "E",
            Style::default()
                .fg(if app.log_filter_error { Color::Red } else { Color::DarkGray })
                .add_modifier(if app.log_filter_error { Modifier::BOLD } else { Modifier::empty() })
        ),
        Span::raw(" "),
        Span::styled(
            "D",
            Style::default()
                .fg(if app.log_filter_debug { Color::White } else { Color::DarkGray })
                .add_modifier(if app.log_filter_debug { Modifier::BOLD } else { Modifier::empty() })
        ),
        Span::raw("]"),
    ];

    if app.log_filter_editing {
        title_spans.push(Span::raw(" Filter: "));
        title_spans.push(Span::styled(
            format!("{}|", app.log_filter_text),
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
        ));
    } else if !app.log_filter_text.is_empty() {
        title_spans.push(Span::raw(" Filter: "));
        title_spans.push(Span::styled(
            &app.log_filter_text,
            Style::default().fg(Color::Cyan)
        ));
    }

    let block = Block::default()
        .title(Line::from(title_spans))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    let filtered = app.filtered_logs();
    
    if filtered.is_empty() {
        let empty_list = List::new(vec![ListItem::new("(no logs match filters)")])
            .block(block)
            .style(Style::default().fg(Color::DarkGray));
        frame.render_widget(empty_list, area);
        return;
    }

    let visible_height = area.height.saturating_sub(2) as usize;
    let total_logs = filtered.len();
    let scroll_offset = app.log_scroll.min(total_logs.saturating_sub(1));
    let start_idx = scroll_offset.saturating_sub(visible_height.saturating_sub(1));
    let end_idx = (scroll_offset + 1).min(total_logs);
    
    let items: Vec<ListItem> = filtered
        .iter()
        .skip(start_idx)
        .take(end_idx - start_idx)
        .map(|entry| {
            let level_color = match entry.level {
                LogLevel::Error => Color::Red,
                LogLevel::Warn => Color::Yellow,
                LogLevel::Info => Color::White,
                LogLevel::Debug => Color::DarkGray,
            };
            let level_str = entry.level.as_str();
            
            let line = Line::from(vec![
                Span::styled(format!("[{}] ", level_str), Style::default().fg(level_color).add_modifier(Modifier::BOLD)),
                Span::styled(&entry.message, Style::default().fg(level_color)),
            ]);
            
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items)
        .block(block)
        .style(Style::default().fg(Color::White));

    frame.render_widget(list, area);
}
