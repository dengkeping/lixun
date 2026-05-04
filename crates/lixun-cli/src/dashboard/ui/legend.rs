//! Legend widget showing keyboard shortcuts.

use ratatui::{
    Frame,
    layout::Rect,
    widgets::{Block, Borders, Paragraph},
    style::{Color, Style, Modifier},
    text::{Line, Span},
};

use crate::dashboard::app::{App, FocusedWidget, InputMode, WidgetMode};

pub fn render_legend(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let shortcuts = if app.log_filter_editing {
        vec![
            Span::styled("FILTER EDIT: ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            Span::raw("Type to filter | "),
            Span::styled("Enter", Style::default().fg(Color::Yellow)),
            Span::raw(" apply | "),
            Span::styled("Esc", Style::default().fg(Color::Yellow)),
            Span::raw(" cancel"),
        ]
    } else {
        match (app.input_mode, app.widget_mode) {
            (InputMode::Editing, _) => {
                vec![
                    Span::styled("EDIT MODE: ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                    Span::raw("Type to search | "),
                    Span::styled("Enter", Style::default().fg(Color::Yellow)),
                    Span::raw(" submit | "),
                    Span::styled("Esc", Style::default().fg(Color::Yellow)),
                    Span::raw(" exit"),
                ]
            }
            (InputMode::Navigation, WidgetMode::Focused) => {
                let mut spans = vec![
                    Span::styled("FOCUSED: ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                    Span::styled("j/k ↑↓", Style::default().fg(Color::Yellow)),
                    Span::raw(" scroll | "),
                    Span::styled("Esc", Style::default().fg(Color::Yellow)),
                    Span::raw(" exit"),
                ];
                
                if app.focused_widget == FocusedWidget::LogViewer {
                    spans.push(Span::raw(" | "));
                    spans.push(Span::styled("i/w/e/d", Style::default().fg(Color::Yellow)));
                    spans.push(Span::raw(" levels | "));
                    spans.push(Span::styled("f", Style::default().fg(Color::Yellow)));
                    spans.push(Span::raw(" filter"));
                }
                
                spans
            }
            (InputMode::Navigation, WidgetMode::Navigation) => {
                let mut spans = vec![
                    Span::styled("q", Style::default().fg(Color::Yellow)),
                    Span::raw(" quit | "),
                    Span::styled("Tab ↑↓←→", Style::default().fg(Color::Yellow)),
                    Span::raw(" navigate | "),
                    Span::styled("Enter", Style::default().fg(Color::Yellow)),
                    Span::raw(" focus | "),
                    Span::styled("o", Style::default().fg(Color::Yellow)),
                    Span::raw(" OCR | "),
                    Span::styled("s", Style::default().fg(Color::Yellow)),
                    Span::raw(" semantic | "),
                    Span::styled("r", Style::default().fg(Color::Yellow)),
                    Span::raw(" restart"),
                ];
                
                if app.focused_widget == FocusedWidget::LogViewer {
                    spans.push(Span::raw(" | "));
                    spans.push(Span::styled("i/w/e/d", Style::default().fg(Color::Yellow)));
                    spans.push(Span::raw(" levels | "));
                    spans.push(Span::styled("f", Style::default().fg(Color::Yellow)));
                    spans.push(Span::raw(" filter"));
                }
                
                spans
            }
        }
    };

    let paragraph = Paragraph::new(Line::from(shortcuts))
        .block(block)
        .style(Style::default().fg(Color::White));

    frame.render_widget(paragraph, area);
}
