//! Widget for entering search queries.

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::dashboard::app::{App, InputMode};

pub fn render_query_input(frame: &mut Frame, area: Rect, app: &App, focused: bool) {
    let is_editing = app.input_mode == InputMode::Editing && focused;
    
    let title = if is_editing {
        "Query [EDIT - ESC to exit]"
    } else if focused {
        "Query [ENTER to edit]"
    } else {
        "Query"
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(if is_editing {
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
        } else if focused {
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        });

    let text = if is_editing {
        format!("{}|", app.query_input)
    } else {
        app.query_input.clone()
    };

    let paragraph = Paragraph::new(text).block(block);
    frame.render_widget(paragraph, area);
}
