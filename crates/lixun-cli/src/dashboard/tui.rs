//! Terminal initialization and restoration.

use std::io::{self, Stdout};

use anyhow::Result;
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    Terminal,
};

/// Enter TUI mode: enable raw mode, enter alternate screen, enable mouse capture.
/// Returns a Terminal instance ready for rendering.
pub fn enter_tui() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;

    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;

    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;

    Ok(terminal)
}

/// Exit TUI mode: disable mouse capture, leave alternate screen, disable raw mode.
/// Also ensures the cursor is visible.
pub fn exit_tui(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    disable_raw_mode()?;
    terminal.show_cursor()?;

    Ok(())
}

/// Install a panic hook that restores the terminal before calling the original hook.
/// This ensures the terminal is left in a usable state even if the application panics.
pub fn install_panic_hook() {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        original_hook(panic_info);
    }));
}
