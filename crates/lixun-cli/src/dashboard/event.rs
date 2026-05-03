//! Async event handling for TUI dashboard.
//!
//! Provides an async event loop that multiplexes crossterm events (keyboard, mouse, resize)
//! with periodic tick events using tokio's async runtime.

use anyhow::{anyhow, Result};
use crossterm::event::{Event as CrosstermEvent, EventStream, KeyEvent, MouseEvent};
use futures::{FutureExt, StreamExt};
use std::time::Duration;
use tokio::sync::mpsc::{self, UnboundedReceiver};
use tokio::task::JoinHandle;
use tokio::time::interval;
use crate::dashboard::log_entry::LogEntry;

/// Events that can be received from the event handler.
#[derive(Debug, Clone)]
pub enum Event {
    /// Keyboard input event.
    Key(KeyEvent),
    /// Mouse input event.
    Mouse(MouseEvent),
    /// Terminal resize event with new width and height.
    #[allow(dead_code)]
    Resize(u16, u16),
    /// Periodic tick event for updates/animations.
    Tick,
    /// Log entry from journalctl.
    LogLine(LogEntry),
}

/// Async event handler that runs a background task to collect events.
///
/// The handler spawns a tokio task that listens for crossterm events
/// and sends them through an async channel. It also generates periodic
/// tick events at the specified rate.
pub struct EventHandler {
    /// Receiver end of the event channel.
    rx: UnboundedReceiver<Event>,
    /// Handle to the background event processing task.
    /// Stored to keep the task alive as long as the handler exists.
    _task: JoinHandle<()>,
}

impl EventHandler {
    /// Creates a new event handler with the specified tick rate.
    ///
    /// # Arguments
    ///
    /// * `tick_rate` - Duration between tick events
    /// * `log_rx` - Optional receiver for log entries from journalctl
    ///
    /// # Returns
    ///
    /// A new `EventHandler` instance with a running background task.
    pub fn new(tick_rate: Duration, log_rx: Option<UnboundedReceiver<LogEntry>>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();

        let _task = tokio::spawn(async move {
            let mut event_stream = EventStream::new();
            let mut tick_interval = interval(tick_rate);
            let mut log_rx = log_rx;

            loop {
                tokio::select! {
                    // Handle crossterm events (keyboard, mouse, resize)
                    Some(Ok(event)) = event_stream.next().fuse() => {
                        let event = match event {
                            CrosstermEvent::Key(key_event) => Event::Key(key_event),
                            CrosstermEvent::Mouse(mouse_event) => Event::Mouse(mouse_event),
                            CrosstermEvent::Resize(width, height) => Event::Resize(width, height),
                            _ => continue,
                        };

                        if tx.send(event).is_err() {
                            break;
                        }
                    }

                    // Handle periodic tick events
                    _ = tick_interval.tick() => {
                        if tx.send(Event::Tick).is_err() {
                            break;
                        }
                    }

                    // Handle log lines from journalctl
                    Some(line) = async {
                        match &mut log_rx {
                            Some(rx) => rx.recv().await,
                            None => std::future::pending().await,
                        }
                    } => {
                        if tx.send(Event::LogLine(line)).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        Self { rx, _task }
    }

    /// Waits for the next event from the event handler.
    ///
    /// This method asynchronously waits for an event to be received
    /// from the background task. It returns an error if the channel
    /// is closed (which happens when the event handler is dropped).
    ///
    /// # Returns
    ///
    /// * `Ok(Event)` - The next event received
    /// * `Err(anyhow::Error)` - If the event channel is closed
    ///
    /// # Example
    ///
    /// ```rust
    /// use crate::dashboard::event::EventHandler;
    /// use std::time::Duration;
    ///
    /// async fn handle_events(handler: &mut EventHandler) -> anyhow::Result<()> {
    ///     loop {
    ///         match handler.next().await? {
    ///             Event::Key(key) => println!("Key pressed: {:?}", key),
    ///             Event::Mouse(mouse) => println!("Mouse event: {:?}", mouse),
    ///             Event::Resize(w, h) => println!("Resized to {}x{}", w, h),
    ///             Event::Tick => println!("Tick"),
    ///         }
    ///     }
    /// }
    /// ```
    pub async fn next(&mut self) -> Result<Event> {
        self.rx
            .recv()
            .await
            .ok_or_else(|| anyhow!("Event channel closed"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_event_handler_creation() {
        let handler = EventHandler::new(Duration::from_millis(100), None);
        drop(handler);
    }
}
