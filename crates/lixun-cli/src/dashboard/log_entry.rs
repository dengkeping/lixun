//! Log entry parsing from journalctl JSON output.

use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Error,   // PRIORITY 0-3
    Warn,    // PRIORITY 4
    Info,    // PRIORITY 5-6
    Debug,   // PRIORITY 7
}

impl LogLevel {
    pub fn from_priority(priority: u8) -> Self {
        match priority {
            0..=3 => Self::Error,
            4 => Self::Warn,
            5..=6 => Self::Info,
            7.. => Self::Debug,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Error => "ERROR",
            Self::Warn => "WARN",
            Self::Info => "INFO",
            Self::Debug => "DEBUG",
        }
    }
}

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub level: LogLevel,
    pub message: String,
}

#[derive(Deserialize)]
struct JournalEntry {
    #[serde(rename = "MESSAGE")]
    message: Option<Value>,
    #[serde(rename = "PRIORITY")]
    priority: Option<String>,
}

impl LogEntry {
    /// Parse a journalctl JSON line into a LogEntry.
    pub fn parse(line: &str) -> Option<Self> {
        let entry: JournalEntry = serde_json::from_str(line).ok()?;
        
        let message = match entry.message? {
            Value::String(s) => s,
            Value::Array(bytes) => {
                let byte_vec: Vec<u8> = bytes.iter()
                    .filter_map(|v| v.as_u64().map(|n| n as u8))
                    .collect();
                String::from_utf8_lossy(&byte_vec).to_string()
            }
            _ => return None,
        };
        
        let message = strip_ansi_codes(&message);
        
        let priority = entry.priority
            .and_then(|p| p.parse::<u8>().ok())
            .unwrap_or(6);
        
        Some(Self {
            level: LogLevel::from_priority(priority),
            message,
        })
    }
}

fn strip_ansi_codes(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&c) = chars.peek() {
                    chars.next();
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            result.push(ch);
        }
    }
    
    result
}
