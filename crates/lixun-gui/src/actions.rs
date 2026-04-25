//! User-facing actions: open file, launch app, reveal in file manager,
//! open URI (xdg-open), open attachment, copy to clipboard.

use anyhow::Result;
use gtk::gio;
use gio::prelude::*;
use gtk::prelude::*;
use lixun_core::{Action, Hit};

use crate::attachments::{
    decode_attachment, sanitize_filename, secure_runtime_dir_from_env, sweep_stale_attachments,
};
use crate::reaper::spawn_reaped;

pub(crate) fn file_uri(abs: &std::path::Path) -> String {
    let mut out = String::from("file://");
    let s = abs.to_string_lossy();
    let mut first = true;
    for seg in s.split('/') {
        if !first {
            out.push('/');
        }
        first = false;
        for b in seg.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(b as char)
                }
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
    }
    out
}

/// How to spawn a terminal-mode app: which binary to invoke and what
/// argv to prepend before the user command's tokens. Encodes the
/// three-way fallback below.
#[derive(Debug, PartialEq, Eq)]
struct TerminalSpawn {
    program: String,
    args_before_exec: Vec<&'static str>,
}

/// Resolve how to spawn a terminal emulator for
/// `Action::Launch { terminal: true, .. }`. Three-tier fallback:
/// 1. `xdg-terminal-exec` — freedesktop's proposed Default Terminal
///    Execution spec; honours user's `xdg-terminals.list`. Takes the
///    command directly (no `-e` prefix).
/// 2. `$TERMINAL` env var — widely-supported de-facto convention.
///    Invoked as `$TERMINAL -e <cmd>`.
/// 3. `xterm -e <cmd>` — universal last-resort fallback.
///
/// Split from `terminal_spawn()` so it can be unit-tested without
/// mutating `$TERMINAL` or `$PATH` (Rust 2024 makes `env::set_var`
/// unsafe). Caller supplies the two environmental inputs.
fn resolve_terminal_spawn(
    xdg_terminal_exec_on_path: bool,
    env_terminal: Option<&str>,
) -> TerminalSpawn {
    if xdg_terminal_exec_on_path {
        return TerminalSpawn {
            program: "xdg-terminal-exec".to_string(),
            args_before_exec: vec![],
        };
    }
    if let Some(t) = env_terminal.filter(|v| !v.is_empty()) {
        return TerminalSpawn {
            program: t.to_string(),
            args_before_exec: vec!["-e"],
        };
    }
    TerminalSpawn {
        program: "xterm".to_string(),
        args_before_exec: vec!["-e"],
    }
}

fn terminal_spawn() -> TerminalSpawn {
    let on_path = std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|p| p.join("xdg-terminal-exec").is_file())
        })
        .unwrap_or(false);
    resolve_terminal_spawn(on_path, std::env::var("TERMINAL").ok().as_deref())
}

fn show_in_file_manager(path: &std::path::Path) -> Result<()> {
    let abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let uri = file_uri(&abs);
    let conn = zbus::blocking::Connection::session()?;
    let _ = conn.call_method(
        Some("org.freedesktop.FileManager1"),
        "/org/freedesktop/FileManager1",
        Some("org.freedesktop.FileManager1"),
        "ShowItems",
        &(vec![uri.as_str()], ""),
    )?;
    Ok(())
}

pub(crate) fn execute_action(hit: &Hit) -> Result<()> {
    dispatch_action(&hit.action)
}

fn dispatch_action(action: &Action) -> Result<()> {
    match action {
        Action::Launch {
            exec,
            terminal,
            desktop_id,
            desktop_file,
            working_dir,
        } => {
            if !terminal {
                if let Some(id) = desktop_id.as_deref()
                    && let Some(app) = gio::DesktopAppInfo::new(id)
                {
                    app.launch(&[], None::<&gio::AppLaunchContext>)?;
                    return Ok(());
                }
                if let Some(path) = desktop_file.as_ref()
                    && let Some(app) = gio::DesktopAppInfo::from_filename(path)
                {
                    app.launch(&[], None::<&gio::AppLaunchContext>)?;
                    return Ok(());
                }
            }

            if *terminal {
                let spawn = terminal_spawn();
                let mut args: Vec<&str> = spawn.args_before_exec.clone();
                args.extend(exec.split_whitespace());
                let mut builder = std::process::Command::new(&spawn.program);
                builder.args(&args);
                spawn_reaped(&mut builder)?;
            } else {
                let mut parts = exec.split_whitespace();
                if let Some(cmd) = parts.next() {
                    let mut builder = std::process::Command::new(cmd);
                    builder.args(parts);
                    if let Some(dir) = working_dir {
                        builder.current_dir(dir);
                    }
                    spawn_reaped(&mut builder)?;
                }
            }
            Ok(())
        }
        Action::OpenFile { path } => {
            let mut builder = std::process::Command::new("xdg-open");
            builder.arg(path);
            spawn_reaped(&mut builder)?;
            Ok(())
        }
        Action::ShowInFileManager { path } => {
            if path.is_dir() {
                let mut builder = std::process::Command::new("xdg-open");
                builder.arg(path);
                spawn_reaped(&mut builder)?;
            } else {
                match show_in_file_manager(path) {
                    Ok(()) => {}
                    Err(e) => {
                        tracing::debug!(
                            "FileManager1 DBus call failed: {e}; falling back to xdg-open"
                        );
                        if let Some(parent) = path.parent() {
                            let mut builder = std::process::Command::new("xdg-open");
                            builder.arg(parent);
                            spawn_reaped(&mut builder)?;
                        }
                    }
                }
            }
            Ok(())
        }
        Action::OpenAttachment {
            mbox_path,
            byte_offset,
            length,
            mime: _,
            encoding,
            suggested_filename,
        } => {
            let target = extract_attachment_to_temp(
                mbox_path,
                *byte_offset,
                *length,
                encoding,
                suggested_filename,
            )?;
            let mut builder = std::process::Command::new("xdg-open");
            builder.arg(&target);
            spawn_reaped(&mut builder)?;
            Ok(())
        }
        Action::OpenUri { uri } => {
            tracing::debug!(uri = %uri, "execute_action: dispatching via xdg-open");
            let mut builder = std::process::Command::new("xdg-open");
            builder.arg(uri);
            spawn_reaped(&mut builder)?;
            Ok(())
        }
        Action::ReplaceQuery { .. } => Ok(()),
        Action::Exec {
            cmdline,
            working_dir,
            terminal,
        } => {
            let Some((program, args)) = cmdline.split_first() else {
                anyhow::bail!("Action::Exec has empty cmdline");
            };
            if *terminal {
                let spawn = terminal_spawn();
                let mut term_cmd = std::process::Command::new(&spawn.program);
                term_cmd.args(&spawn.args_before_exec);
                term_cmd.arg(program);
                term_cmd.args(args);
                if let Some(dir) = working_dir {
                    term_cmd.current_dir(dir);
                }
                spawn_reaped(&mut term_cmd)?;
            } else {
                let mut cmd = std::process::Command::new(program);
                cmd.args(args);
                if let Some(dir) = working_dir {
                    cmd.current_dir(dir);
                }
                spawn_reaped(&mut cmd)?;
            }
            Ok(())
        }
    }
}

pub(crate) fn execute_secondary_action(hit: &Hit) -> Result<()> {
    if let Some(secondary) = hit.secondary_action.as_deref() {
        return dispatch_action(secondary);
    }
    Ok(())
}

fn extract_attachment_to_temp(
    mbox_path: &std::path::Path,
    byte_offset: u64,
    length: u64,
    encoding: &str,
    suggested_filename: &str,
) -> Result<std::path::PathBuf> {
    use std::io::{Read, Seek, SeekFrom};
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let xdg_dir_str = std::env::var("XDG_RUNTIME_DIR").ok();
    let xdg_dir = xdg_dir_str.as_ref().map(std::path::Path::new);
    let runtime_dir = secure_runtime_dir_from_env(xdg_dir)?;
    let dir = runtime_dir.join("lixun/attachments");
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;

    sweep_stale_attachments(&dir, std::time::Duration::from_secs(600));

    let mut f = std::fs::File::open(mbox_path)?;
    f.seek(SeekFrom::Start(byte_offset))?;
    let mut raw = vec![0u8; length as usize];
    f.read_exact(&mut raw)?;

    let decoded = decode_attachment(&raw, encoding)?;
    let safe = sanitize_filename(suggested_filename);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let target = dir.join(format!("{ts}-{safe}"));
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&target)?;
    std::io::Write::write_all(&mut file, &decoded)?;
    Ok(target)
}

pub(crate) fn copy_to_clipboard(hit: &Hit) {
    let text = match &hit.action {
        Action::OpenFile { path } | Action::ShowInFileManager { path } => {
            path.to_string_lossy().to_string()
        }
        Action::OpenUri { uri } => uri.clone(),
        Action::OpenAttachment { .. } => hit.title.clone(),
        _ => hit.title.clone(),
    };

    if let Some(display) = gtk::gdk::Display::default() {
        display.clipboard().set_text(&text);
    }
    tracing::info!("Copied to clipboard: {}", text);
}

#[cfg(test)]
mod tests {
    use super::{file_uri, resolve_terminal_spawn};
    use std::path::Path;

    #[test]
    fn terminal_prefers_xdg_terminal_exec_when_present() {
        let spawn = resolve_terminal_spawn(true, Some("foot"));
        assert_eq!(spawn.program, "xdg-terminal-exec");
        assert!(spawn.args_before_exec.is_empty());
    }

    #[test]
    fn terminal_xdg_exec_wins_even_if_no_env() {
        let spawn = resolve_terminal_spawn(true, None);
        assert_eq!(spawn.program, "xdg-terminal-exec");
        assert!(spawn.args_before_exec.is_empty());
    }

    #[test]
    fn terminal_uses_env_value_when_xdg_exec_absent() {
        let spawn = resolve_terminal_spawn(false, Some("foot"));
        assert_eq!(spawn.program, "foot");
        assert_eq!(spawn.args_before_exec, vec!["-e"]);
    }

    #[test]
    fn terminal_falls_back_to_xterm_when_all_absent() {
        let spawn = resolve_terminal_spawn(false, None);
        assert_eq!(spawn.program, "xterm");
        assert_eq!(spawn.args_before_exec, vec!["-e"]);
    }

    #[test]
    fn terminal_falls_back_to_xterm_when_env_empty() {
        let spawn = resolve_terminal_spawn(false, Some(""));
        assert_eq!(spawn.program, "xterm");
        assert_eq!(spawn.args_before_exec, vec!["-e"]);
    }

    #[test]
    fn test_file_uri_ascii() {
        assert_eq!(file_uri(Path::new("/tmp/foo.txt")), "file:///tmp/foo.txt");
    }

    #[test]
    fn test_file_uri_spaces() {
        assert_eq!(
            file_uri(Path::new("/tmp/hello world.txt")),
            "file:///tmp/hello%20world.txt"
        );
    }

    #[test]
    fn test_file_uri_utf8() {
        assert_eq!(
            file_uri(Path::new("/home/u/Café.pdf")),
            "file:///home/u/Caf%C3%A9.pdf"
        );
    }

    #[test]
    fn test_file_uri_preserves_separators() {
        let result = file_uri(Path::new("/a/b/c/d/e.txt"));
        assert!(!result.contains("%2F"));
        assert!(!result.contains("%2f"));
        assert_eq!(result, "file:///a/b/c/d/e.txt");
    }
}
