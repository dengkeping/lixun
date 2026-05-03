//! Main application state and loop for the TUI dashboard.

use chrono::{DateTime, Utc};
use crossterm::event::{KeyCode, KeyEvent, MouseEvent, MouseEventKind, MouseButton};
use lixun_core::Hit;
use lixun_ipc::{MemoryStats, OcrStats, Response, WatcherStats, WriterStats};
use std::collections::VecDeque;
use ratatui::layout::{Position, Rect};
use crate::dashboard::log_entry::{LogEntry, LogLevel};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartStatus {
    Idle,
    Restarting,
    Reconnecting,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SemanticStatus {
    Off,
    On,
    Warning(String),
}

/// Main application state holding all dashboard data.
pub struct App {
    // Socket status
    pub connected: bool,
    pub indexed_docs: u64,
    #[allow(dead_code)]
    pub last_reindex: Option<DateTime<Utc>>,
    pub memory: Option<MemoryStats>,

    // Logs
    pub logs: VecDeque<LogEntry>,
    pub log_scroll: usize,
    pub log_filter_info: bool,
    pub log_filter_warn: bool,
    pub log_filter_error: bool,
    pub log_filter_debug: bool,
    pub log_filter_text: String,
    pub log_filter_editing: bool,

    // Reindex
    pub reindex_in_progress: bool,
    pub reindex_started: Option<DateTime<Utc>>,
    pub reindex_pending: bool,

    // Query
    pub query_input: String,
    pub search_results: Vec<Hit>,
    pub selected_result: usize,
    pub results_scroll: usize,
    pub search_pending: bool,

    // Index stats
    pub watcher: Option<WatcherStats>,
    pub writer: Option<WriterStats>,

    // Services
    pub ocr_stats: Option<OcrStats>,
    pub semantic_status: SemanticStatus,

    // Daemon restart
    pub restart_pending: bool,
    pub restart_status: RestartStatus,
    #[allow(dead_code)]
    pub restart_retry_count: u32,

    // UI state
    pub should_quit: bool,
    pub focused_widget: FocusedWidget,
    pub input_mode: InputMode,
    pub widget_mode: WidgetMode,

    // Mouse support
    pub socket_panel_rect: Rect,
    pub services_panel_rect: Rect,
    pub query_input_rect: Rect,
    pub results_list_rect: Rect,
    pub log_viewer_rect: Rect,
    pub reindex_controls_rect: Rect,
    pub index_stats_rect: Rect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    Navigation,
    Editing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WidgetMode {
    Navigation,
    Focused,
}

/// Which widget currently has focus for keyboard navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusedWidget {
    SocketPanel,
    LogViewer,
    QueryInput,
    ResultsList,
    ReindexControls,
    IndexStats,
    ServicesPanel,
}

impl FocusedWidget {
    /// Cycle to the next widget in the tab order.
    pub fn next(self) -> Self {
        match self {
            Self::SocketPanel => Self::LogViewer,
            Self::LogViewer => Self::QueryInput,
            Self::QueryInput => Self::ResultsList,
            Self::ResultsList => Self::ReindexControls,
            Self::ReindexControls => Self::IndexStats,
            Self::IndexStats => Self::ServicesPanel,
            Self::ServicesPanel => Self::SocketPanel,
        }
    }

    /// Cycle to the previous widget in the tab order.
    pub fn prev(self) -> Self {
        match self {
            Self::SocketPanel => Self::ServicesPanel,
            Self::LogViewer => Self::SocketPanel,
            Self::QueryInput => Self::LogViewer,
            Self::ResultsList => Self::QueryInput,
            Self::ReindexControls => Self::ResultsList,
            Self::IndexStats => Self::ReindexControls,
            Self::ServicesPanel => Self::IndexStats,
        }
    }
}

impl Default for FocusedWidget {
    fn default() -> Self {
        Self::QueryInput
    }
}

impl App {
    /// Create a new App instance with default values.
    pub fn new() -> Self {
        Self {
            // Socket status
            connected: false,
            indexed_docs: 0,
            last_reindex: None,
            memory: None,

            // Logs
            logs: VecDeque::new(),
            log_scroll: 0,
            log_filter_info: true,
            log_filter_warn: true,
            log_filter_error: true,
            log_filter_debug: true,
            log_filter_text: String::new(),
            log_filter_editing: false,

            // Reindex
            reindex_in_progress: false,
            reindex_started: None,
            reindex_pending: false,

            // Query
            query_input: String::new(),
            search_results: Vec::new(),
            selected_result: 0,
            results_scroll: 0,
            search_pending: false,

            // Index stats
            watcher: None,
            writer: None,

            // Services
            ocr_stats: None,
            semantic_status: SemanticStatus::Off,

            // Daemon restart
            restart_pending: false,
            restart_status: RestartStatus::Idle,
            restart_retry_count: 0,

            // UI state
            should_quit: false,
            focused_widget: FocusedWidget::default(),
            input_mode: InputMode::Navigation,
            widget_mode: WidgetMode::Navigation,

            // Mouse support
            socket_panel_rect: Rect::default(),
            services_panel_rect: Rect::default(),
            query_input_rect: Rect::default(),
            results_list_rect: Rect::default(),
            log_viewer_rect: Rect::default(),
            reindex_controls_rect: Rect::default(),
            index_stats_rect: Rect::default(),
        }
    }

    /// Called periodically on every tick for time-based updates.
    #[allow(dead_code)]
    pub fn tick(&mut self) {
        // Placeholder for periodic updates
        // This could be used for:
        // - Blinking cursor animation
        // - Auto-refreshing status
        // - Time-based log rotation
    }

    /// Handle keyboard input events.
    pub fn handle_key(&mut self, key: KeyEvent) {
        if self.log_filter_editing {
            self.handle_filter_editing_key(key);
        } else {
            match self.input_mode {
                InputMode::Navigation => {
                    match self.widget_mode {
                        WidgetMode::Navigation => self.handle_navigation_key(key),
                        WidgetMode::Focused => self.handle_focused_key(key),
                    }
                }
                InputMode::Editing => self.handle_editing_key(key),
            }
        }
    }

    fn handle_navigation_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => {
                self.should_quit = true;
            }
            KeyCode::Tab => {
                self.focused_widget = self.focused_widget.next();
            }
            KeyCode::BackTab => {
                self.focused_widget = self.focused_widget.prev();
            }
            KeyCode::Up => {
                self.focused_widget = self.focused_widget.prev();
            }
            KeyCode::Down => {
                self.focused_widget = self.focused_widget.next();
            }
            KeyCode::Char('o') => {
                if self.focused_widget == FocusedWidget::ServicesPanel {
                    self.toggle_ocr();
                }
            }
            KeyCode::Char('s') => {
                if self.focused_widget == FocusedWidget::ServicesPanel {
                    self.toggle_semantic();
                }
            }
            KeyCode::Char('r') => {
                if self.focused_widget == FocusedWidget::ServicesPanel {
                    self.restart_pending = true;
                }
            }
            KeyCode::Char('i') => {
                if self.focused_widget == FocusedWidget::LogViewer {
                    self.toggle_log_filter_info();
                }
            }
            KeyCode::Char('w') => {
                if self.focused_widget == FocusedWidget::LogViewer {
                    self.toggle_log_filter_warn();
                }
            }
            KeyCode::Char('e') => {
                if self.focused_widget == FocusedWidget::LogViewer {
                    self.toggle_log_filter_error();
                }
            }
            KeyCode::Char('d') => {
                if self.focused_widget == FocusedWidget::LogViewer {
                    self.toggle_log_filter_debug();
                }
            }
            KeyCode::Char('f') => {
                if self.focused_widget == FocusedWidget::LogViewer {
                    self.log_filter_editing = true;
                }
            }
            KeyCode::Enter => {
                match self.focused_widget {
                    FocusedWidget::QueryInput => {
                        self.input_mode = InputMode::Editing;
                    }
                    FocusedWidget::ReindexControls => {
                        self.reindex_pending = true;
                    }
                    FocusedWidget::LogViewer | FocusedWidget::ResultsList => {
                        self.widget_mode = WidgetMode::Focused;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn handle_focused_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.widget_mode = WidgetMode::Navigation;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.handle_scroll_down();
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.handle_scroll_up();
            }
            _ => {}
        }
    }

    fn handle_editing_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.input_mode = InputMode::Navigation;
            }
            KeyCode::Char(c) => {
                self.push_query_char(c);
            }
            KeyCode::Backspace => {
                self.pop_query_char();
            }
            KeyCode::Enter => {
                if !self.query_input.is_empty() {
                    self.search_pending = true;
                    self.input_mode = InputMode::Navigation;
                }
            }
            _ => {}
        }
    }

    fn handle_filter_editing_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.log_filter_editing = false;
            }
            KeyCode::Char(c) => {
                self.log_filter_text.push(c);
            }
            KeyCode::Backspace => {
                self.log_filter_text.pop();
            }
            KeyCode::Enter => {
                self.log_filter_editing = false;
                self.log_scroll = 0; // Reset scroll when filter applied
            }
            _ => {}
        }
    }

    /// Scroll down in the currently focused widget.
    fn handle_scroll_down(&mut self) {
        match self.focused_widget {
            FocusedWidget::LogViewer => {
                let filtered_len = self.filtered_logs().len();
                if self.log_scroll < filtered_len.saturating_sub(1) {
                    self.log_scroll += 1;
                }
            }
            FocusedWidget::ResultsList => {
                if self.selected_result < self.search_results.len().saturating_sub(1) {
                    self.selected_result += 1;
                }
                // Auto-scroll viewport to keep selection visible
                if self.selected_result >= self.results_scroll + 10 {
                    self.results_scroll = self.selected_result.saturating_sub(9);
                }
            }
            _ => {
                // Other widgets don't support vertical scrolling
            }
        }
    }

    /// Scroll up in the currently focused widget.
    fn handle_scroll_up(&mut self) {
        match self.focused_widget {
            FocusedWidget::LogViewer => {
                self.log_scroll = self.log_scroll.saturating_sub(1);
            }
            FocusedWidget::ResultsList => {
                self.selected_result = self.selected_result.saturating_sub(1);
                // Auto-scroll viewport to keep selection visible
                if self.selected_result < self.results_scroll {
                    self.results_scroll = self.selected_result;
                }
            }
            _ => {}
        }
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.handle_scroll_up();
            }
            MouseEventKind::ScrollDown => {
                self.handle_scroll_down();
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let pos = Position::new(mouse.column, mouse.row);
                self.handle_mouse_click(pos);
            }
            _ => {}
        }
    }

    fn handle_mouse_click(&mut self, pos: Position) {
        if self.socket_panel_rect.contains(pos) {
            self.focused_widget = FocusedWidget::SocketPanel;
        } else if self.services_panel_rect.contains(pos) {
            self.focused_widget = FocusedWidget::ServicesPanel;
        } else if self.query_input_rect.contains(pos) {
            self.focused_widget = FocusedWidget::QueryInput;
            self.input_mode = InputMode::Editing;
        } else if self.results_list_rect.contains(pos) {
            self.focused_widget = FocusedWidget::ResultsList;
            let relative_y = pos.y.saturating_sub(self.results_list_rect.y + 1);
            if relative_y < self.search_results.len() as u16 {
                self.selected_result = relative_y as usize;
            }
        } else if self.log_viewer_rect.contains(pos) {
            self.focused_widget = FocusedWidget::LogViewer;
        } else if self.reindex_controls_rect.contains(pos) {
            self.focused_widget = FocusedWidget::ReindexControls;
        } else if self.index_stats_rect.contains(pos) {
            self.focused_widget = FocusedWidget::IndexStats;
        }
    }

    /// Update state from a daemon status response.
    #[allow(dead_code)]
    pub fn update_from_status(&mut self, status: Response) {
        if let Response::Status {
            indexed_docs,
            last_reindex,
            errors: _,
            watcher,
            writer,
            memory,
            reindex_in_progress,
            reindex_started,
            ocr,
        } = status
        {
            self.indexed_docs = indexed_docs;
            self.last_reindex = last_reindex;
            self.watcher = watcher;
            self.writer = writer;
            self.memory = memory;
            self.reindex_in_progress = reindex_in_progress;
            self.reindex_started = reindex_started;
            self.ocr_stats = ocr;
            self.connected = true;
        }
    }

    pub fn update_semantic_status(&mut self) {
        use crate::dashboard::config_mutation::read_semantic_enabled;
        
        let config_enabled = read_semantic_enabled().ok().flatten().unwrap_or(false);
        
        if !config_enabled {
            self.semantic_status = SemanticStatus::Off;
            return;
        }
        
        let worker_running = std::process::Command::new("pgrep")
            .arg("-f")
            .arg("lixun-semantic-worker")
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false);
        
        self.semantic_status = if worker_running {
            SemanticStatus::On
        } else {
            SemanticStatus::Warning("worker missing".to_string())
        };
    }

    /// Add a log entry to the buffer.
    pub fn push_log(&mut self, entry: LogEntry) {
        self.logs.push_back(entry);
        if self.logs.len() > 1000 {
            self.logs.pop_front();
        }
        if self.log_scroll >= self.logs.len().saturating_sub(2) {
            self.log_scroll = self.logs.len().saturating_sub(1);
        }
    }

    pub fn push_log_message(&mut self, message: String, level: LogLevel) {
        self.push_log(LogEntry { level, message });
    }

    /// Clear all logs.
    #[allow(dead_code)]
    pub fn clear_logs(&mut self) {
        self.logs.clear();
        self.log_scroll = 0;
    }

    pub fn toggle_log_filter_info(&mut self) {
        self.log_filter_info = !self.log_filter_info;
        self.log_scroll = 0; // Reset scroll when filter changes
    }

    pub fn toggle_log_filter_warn(&mut self) {
        self.log_filter_warn = !self.log_filter_warn;
        self.log_scroll = 0;
    }

    pub fn toggle_log_filter_error(&mut self) {
        self.log_filter_error = !self.log_filter_error;
        self.log_scroll = 0;
    }

    pub fn toggle_log_filter_debug(&mut self) {
        self.log_filter_debug = !self.log_filter_debug;
        self.log_scroll = 0;
    }

    pub fn filtered_logs(&self) -> Vec<&LogEntry> {
        self.logs.iter().filter(|entry| {
            // Level filter
            let level_match = match entry.level {
                LogLevel::Info => self.log_filter_info,
                LogLevel::Warn => self.log_filter_warn,
                LogLevel::Error => self.log_filter_error,
                LogLevel::Debug => self.log_filter_debug,
            };
            
            // Text filter (case-insensitive substring match)
            let text_match = if self.log_filter_text.is_empty() {
                true
            } else {
                entry.message.to_lowercase().contains(&self.log_filter_text.to_lowercase())
            };
            
            level_match && text_match
        }).collect()
    }

    /// Scroll results list up.
    #[allow(dead_code)]
    pub fn scroll_results_up(&mut self) {
        self.results_scroll = self.results_scroll.saturating_sub(1);
    }

    /// Scroll results list down.
    #[allow(dead_code)]
    pub fn scroll_results_down(&mut self) {
        if self.results_scroll + 1 < self.search_results.len() {
            self.results_scroll += 1;
        }
    }

    /// Set search results and reset selection.
    pub fn set_search_results(&mut self, results: Vec<Hit>) {
        self.search_results = results;
        self.selected_result = 0;
    }

    /// Append a character to the query input.
    pub fn push_query_char(&mut self, c: char) {
        self.query_input.push(c);
    }

    /// Remove the last character from query input.
    pub fn pop_query_char(&mut self) {
        self.query_input.pop();
    }

    /// Clear the query input.
    #[allow(dead_code)]
    pub fn clear_query(&mut self) {
        self.query_input.clear();
        self.search_results.clear();
        self.selected_result = 0;
    }

    /// Mark the socket as disconnected.
    #[allow(dead_code)]
    pub fn set_disconnected(&mut self) {
        self.connected = false;
    }

    /// Mark the socket as connected.
    #[allow(dead_code)]
    pub fn set_connected(&mut self, connected: bool) {
        self.connected = connected;
    }

    pub fn toggle_ocr(&mut self) {
        let new_state = self.ocr_stats.is_none();
        if let Err(e) = super::config_mutation::persist_ocr_enabled(new_state) {
            self.push_log_message(format!("Failed to persist OCR config: {}", e), LogLevel::Error);
        } else {
            self.restart_pending = true;
        }
    }

    pub fn toggle_semantic(&mut self) {
        use crate::dashboard::config_mutation::{read_semantic_enabled, persist_semantic_enabled};
        
        let current = read_semantic_enabled().ok().flatten().unwrap_or(false);
        let new_state = !current;
        
        if let Err(e) = persist_semantic_enabled(new_state) {
            self.push_log_message(format!("Failed to persist semantic config: {}", e), LogLevel::Error);
        } else {
            self.restart_pending = true;
            self.update_semantic_status();
        }
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_app_new() {
        let app = App::new();
        assert!(!app.connected);
        assert!(!app.should_quit);
        assert!(app.logs.is_empty());
        assert!(app.query_input.is_empty());
        assert!(app.search_results.is_empty());
    }

    #[test]
    fn test_focused_widget_next_prev() {
        let mut fw = FocusedWidget::QueryInput;
        fw = fw.next();
        assert!(matches!(fw, FocusedWidget::ResultsList));
        fw = fw.prev();
        assert!(matches!(fw, FocusedWidget::QueryInput));
    }

    #[test]
    fn test_focused_widget_cycles() {
        // Going forward through all widgets should return to start
        let start = FocusedWidget::SocketPanel;
        let mut fw = start;
        for _ in 0..7 {
            fw = fw.next();
        }
        assert_eq!(fw, start);
    }

    #[test]
    fn test_handle_key_quit() {
        let mut app = App::new();
        app.handle_key(KeyEvent::from(KeyCode::Char('q')));
        assert!(app.should_quit);
    }

    #[test]
    fn test_handle_key_tab() {
        let mut app = App::new();
        assert!(matches!(app.focused_widget, FocusedWidget::QueryInput));
        app.handle_key(KeyEvent::from(KeyCode::Tab));
        assert!(matches!(app.focused_widget, FocusedWidget::ResultsList));
    }

    #[test]
    fn test_push_log() {
        let mut app = App::new();
        app.push_log_message("Test message".to_string(), LogLevel::Info);
        assert_eq!(app.logs.len(), 1);
        assert_eq!(app.logs[0].message, "Test message");
    }

    #[test]
    fn test_push_log_limits_size() {
        let mut app = App::new();
        for i in 0..1005 {
            app.push_log_message(format!("Log line {}", i), LogLevel::Info);
        }
        assert_eq!(app.logs.len(), 1000);
    }

    #[test]
    fn test_query_input() {
        let mut app = App::new();
        app.push_query_char('h');
        app.push_query_char('i');
        assert_eq!(app.query_input, "hi");
        app.pop_query_char();
        assert_eq!(app.query_input, "h");
        app.clear_query();
        assert!(app.query_input.is_empty());
    }

    #[test]
    fn test_scroll_logs() {
        let mut app = App::new();
        for i in 0..10 {
            app.push_log_message(format!("Line {}", i), LogLevel::Info);
        }
        app.focused_widget = FocusedWidget::LogViewer;
        assert_eq!(app.log_scroll, 9);

        app.handle_key(KeyEvent::from(KeyCode::Up));
        assert_eq!(app.log_scroll, 8);

        app.handle_key(KeyEvent::from(KeyCode::Down));
        assert_eq!(app.log_scroll, 9);
    }

    #[test]
    fn test_set_search_results() {
        let mut app = App::new();
        app.selected_result = 5;

        let hits = vec![];
        app.set_search_results(hits);

        assert_eq!(app.selected_result, 0);
    }

    #[test]
    fn test_connection_state() {
        let mut app = App::new();
        assert!(!app.connected);

        app.set_connected();
        assert!(app.connected);

        app.set_disconnected();
        assert!(!app.connected);
    }
}
