//! User-facing actions: open file, launch app, reveal in file manager,
//! open mail, open attachment, copy to clipboard.

use anyhow::Result;
use gtk::gio;
use gio::prelude::*;
use gtk::prelude::*;
use lixun_core::{Action, Hit};

use crate::attachments::{
    decode_attachment, sanitize_filename, secure_runtime_dir_from_env, sweep_stale_attachments,
};

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
    match &hit.action {
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
                let mut args: Vec<&str> = vec!["-e"];
                args.extend(exec.split_whitespace());
                std::process::Command::new("alacritty")
                    .args(&args)
                    .spawn()?;
            } else {
                let mut parts = exec.split_whitespace();
                if let Some(cmd) = parts.next() {
                    let mut builder = std::process::Command::new(cmd);
                    builder.args(parts);
                    if let Some(dir) = working_dir {
                        builder.current_dir(dir);
                    }
                    builder.spawn()?;
                }
            }
            Ok(())
        }
        Action::OpenFile { path } => {
            std::process::Command::new("xdg-open").arg(path).spawn()?;
            Ok(())
        }
        Action::ShowInFileManager { path } => {
            if path.is_dir() {
                std::process::Command::new("xdg-open").arg(path).spawn()?;
            } else {
                match show_in_file_manager(path) {
                    Ok(()) => {}
                    Err(e) => {
                        tracing::debug!(
                            "FileManager1 DBus call failed: {e}; falling back to xdg-open"
                        );
                        if let Some(parent) = path.parent() {
                            std::process::Command::new("xdg-open").arg(parent).spawn()?;
                        }
                    }
                }
            }
            Ok(())
        }
        Action::OpenMail { message_id } => {
            std::process::Command::new("thunderbird")
                .arg(format!("mid:{}", message_id))
                .spawn()?;
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
            std::process::Command::new("xdg-open")
                .arg(&target)
                .spawn()?;
            Ok(())
        }
        Action::OpenParentMail { message_id } => {
            std::process::Command::new("thunderbird")
                .arg(format!("mid:{}", message_id))
                .spawn()?;
            Ok(())
        }
        Action::ReplaceQuery { .. } => Ok(()),
        Action::Exec {
            cmdline,
            working_dir,
        } => {
            let Some((program, args)) = cmdline.split_first() else {
                anyhow::bail!("Action::Exec has empty cmdline");
            };
            let mut cmd = std::process::Command::new(program);
            cmd.args(args);
            if let Some(dir) = working_dir {
                cmd.current_dir(dir);
            }
            cmd.spawn()?;
            Ok(())
        }
    }
}

pub(crate) fn execute_secondary_action(hit: &Hit) -> Result<()> {
    match &hit.action {
        Action::OpenFile { path } | Action::ShowInFileManager { path } => {
            if path.is_dir() {
                std::process::Command::new("xdg-open").arg(path).spawn()?;
            } else {
                match show_in_file_manager(path) {
                    Ok(()) => {}
                    Err(e) => {
                        tracing::debug!(
                            "FileManager1 DBus call failed: {e}; falling back to xdg-open"
                        );
                        if let Some(parent) = path.parent() {
                            std::process::Command::new("xdg-open").arg(parent).spawn()?;
                        }
                    }
                }
            }
            Ok(())
        }
        Action::OpenParentMail { message_id } => {
            std::process::Command::new("thunderbird")
                .arg("-message")
                .arg(message_id)
                .spawn()?;
            Ok(())
        }
        _ => Ok(()),
    }
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
        Action::OpenMail { message_id } | Action::OpenParentMail { message_id } => {
            message_id.clone()
        }
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
    use super::file_uri;
    use std::path::Path;

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
