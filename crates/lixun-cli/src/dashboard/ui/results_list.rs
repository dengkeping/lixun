//! Widget for displaying search results.

use ratatui::{
    Frame,
    layout::{Rect, Constraint},
    widgets::{Block, Borders, Table, Row, Cell},
    style::{Color, Style, Modifier},
};
use crate::dashboard::app::{App, FocusedWidget, WidgetMode};

pub fn render_results_list(frame: &mut Frame, area: Rect, app: &App) {
    let is_focused = app.focused_widget == FocusedWidget::ResultsList;
    let border_color = if is_focused {
        match app.widget_mode {
            WidgetMode::Navigation => Color::Cyan,
            WidgetMode::Focused => Color::Green,
        }
    } else {
        Color::Gray
    };

    let block = Block::default()
        .title("Results")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    if app.search_results.is_empty() {
        frame.render_widget(block, area);
        return;
    }

    let visible_height = area.height.saturating_sub(3) as usize; // minus borders + header
    let start = app.results_scroll;
    let end = (start + visible_height).min(app.search_results.len());

    let rows: Vec<Row> = app.search_results[start..end]
        .iter()
        .enumerate()
        .map(|(idx, hit)| {
            let absolute_idx = start + idx;
            let style = if absolute_idx == app.selected_result {
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };

            Row::new(vec![
                Cell::from(hit.title.clone()),
                Cell::from(hit.subtitle.clone()),
                Cell::from(format!("{:.2}", hit.score)),
            ])
            .style(style)
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Percentage(50),
            Constraint::Percentage(30),
            Constraint::Percentage(20),
        ],
    )
    .block(block)
    .header(
        Row::new(vec![
            Cell::from("Title"),
            Cell::from("Subtitle"),
            Cell::from("Score"),
        ])
        .style(Style::default().add_modifier(Modifier::BOLD))
    );

    frame.render_widget(table, area);
}
