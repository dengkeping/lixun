//! GUI-side IPC schema + sync frame codec helpers.
//!
//! The GUI process (`lixun-gui`) owns a second unix socket at
//! `$XDG_RUNTIME_DIR/lixun-gui.sock` so the daemon can signal
//! show/hide/toggle/quit without spawning a new process per toggle
//! (service mode, G1.6).
//!
//! Wire format is identical to the daemon socket:
//!   u32-BE length (of version + payload) | u16-BE version | JSON
//! but this module exposes BLOCKING std::io helpers because the GUI
//! process has no tokio runtime. The daemon side (async) will reuse
//! the same byte layout via its own tokio_util codec.

use std::io::{Read, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::PROTOCOL_VERSION;

/// Command the daemon sends to the GUI process.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum GuiCommand {
    /// Show the launcher window (no-op if already visible).
    Show,
    /// Hide the launcher window (no-op if already hidden).
    Hide,
    /// Toggle visibility; GUI inspects `window.is_visible()` to decide.
    Toggle,
    /// Graceful exit: close window, quit the GTK application.
    Quit,
    /// Health probe. Reply carries current visibility.
    Ping,
    /// Drop the persisted session cache without changing visibility.
    /// Sent by the daemon after a preview process exits with the
    /// "launched" sentinel (preview opened the file in its default
    /// app), so that the next Super+Space shows a blank launcher
    /// rather than restoring the pre-launch query/results/selection.
    /// The launcher is already hidden by this point (persist_session
    /// fired during the Space → preview handoff), so this command
    /// does not flip visibility.
    ClearSession,
}

/// Response from the GUI to a `GuiCommand`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum GuiResponse {
    /// Command applied successfully. `visible` is the resulting
    /// visibility after the command, so `Toggle` callers can learn
    /// the new state in one round trip.
    Ok { visible: bool },
    /// Command failed (e.g. GTK main loop unresponsive).
    Error(String),
}

/// Socket path for GUI-side IPC. Symmetric with `socket_path()` in the
/// daemon schema — sits next to `lixun.sock` in `$XDG_RUNTIME_DIR`.
pub fn gui_socket_path() -> PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime_dir).join("lixun-gui.sock");
    }
    let uid = crate::get_uid_fallback();
    PathBuf::from(format!("/tmp/lixun-gui-{}.sock", uid))
}

/// Encode `msg` into the daemon wire format and write the whole frame
/// to `w`. Blocks until written or I/O fails.
pub fn write_frame_sync<W, T>(w: &mut W, msg: &T) -> std::io::Result<()>
where
    W: Write,
    T: Serialize,
{
    let json = serde_json::to_vec(msg)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let total_len: u32 = (2 + json.len())
        .try_into()
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "frame too large"))?;
    let mut buf = Vec::with_capacity(4 + 2 + json.len());
    buf.extend_from_slice(&total_len.to_be_bytes());
    buf.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    buf.extend_from_slice(&json);
    w.write_all(&buf)
}

/// Read one frame from `r` and decode the payload as `T`. Blocks until
/// the full frame arrives. Returns an error on version mismatch or
/// payload decode failure.
pub fn read_frame_sync<R, T>(r: &mut R) -> std::io::Result<T>
where
    R: Read,
    T: serde::de::DeserializeOwned,
{
    let mut header = [0u8; 4];
    r.read_exact(&mut header)?;
    let total_len = u32::from_be_bytes(header) as usize;
    if total_len < 2 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame too short for version",
        ));
    }
    let mut version_buf = [0u8; 2];
    r.read_exact(&mut version_buf)?;
    let version = u16::from_be_bytes(version_buf);
    if version != PROTOCOL_VERSION {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "version mismatch: expected {}, got {}",
                PROTOCOL_VERSION, version
            ),
        ));
    }
    let payload_len = total_len - 2;
    let mut payload = vec![0u8; payload_len];
    r.read_exact(&mut payload)?;
    serde_json::from_slice(&payload)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Async counterpart of [`write_frame_sync`]. Same byte layout; intended
/// for the daemon side which has a tokio runtime.
pub async fn write_frame_async<W, T>(w: &mut W, msg: &T) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
    T: Serialize,
{
    use tokio::io::AsyncWriteExt;
    let json = serde_json::to_vec(msg)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let total_len: u32 = (2 + json.len())
        .try_into()
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "frame too large"))?;
    let mut buf = Vec::with_capacity(4 + 2 + json.len());
    buf.extend_from_slice(&total_len.to_be_bytes());
    buf.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    buf.extend_from_slice(&json);
    w.write_all(&buf).await
}

/// Async counterpart of [`read_frame_sync`]. Same byte layout.
pub async fn read_frame_async<R, T>(r: &mut R) -> std::io::Result<T>
where
    R: tokio::io::AsyncRead + Unpin,
    T: serde::de::DeserializeOwned,
{
    use tokio::io::AsyncReadExt;
    let mut header = [0u8; 4];
    r.read_exact(&mut header).await?;
    let total_len = u32::from_be_bytes(header) as usize;
    if total_len < 2 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame too short for version",
        ));
    }
    let mut version_buf = [0u8; 2];
    r.read_exact(&mut version_buf).await?;
    let version = u16::from_be_bytes(version_buf);
    if version != PROTOCOL_VERSION {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "version mismatch: expected {}, got {}",
                PROTOCOL_VERSION, version
            ),
        ));
    }
    let payload_len = total_len - 2;
    let mut payload = vec![0u8; payload_len];
    r.read_exact(&mut payload).await?;
    serde_json::from_slice(&payload)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn roundtrip_cmd(cmd: GuiCommand) -> GuiCommand {
        let mut buf = Vec::<u8>::new();
        write_frame_sync(&mut buf, &cmd).unwrap();
        let mut cur = Cursor::new(buf);
        read_frame_sync(&mut cur).unwrap()
    }

    fn roundtrip_resp(resp: GuiResponse) -> GuiResponse {
        let mut buf = Vec::<u8>::new();
        write_frame_sync(&mut buf, &resp).unwrap();
        let mut cur = Cursor::new(buf);
        read_frame_sync(&mut cur).unwrap()
    }

    #[test]
    fn roundtrip_show() {
        assert_eq!(roundtrip_cmd(GuiCommand::Show), GuiCommand::Show);
    }

    #[test]
    fn roundtrip_hide() {
        assert_eq!(roundtrip_cmd(GuiCommand::Hide), GuiCommand::Hide);
    }

    #[test]
    fn roundtrip_toggle() {
        assert_eq!(roundtrip_cmd(GuiCommand::Toggle), GuiCommand::Toggle);
    }

    #[test]
    fn roundtrip_quit() {
        assert_eq!(roundtrip_cmd(GuiCommand::Quit), GuiCommand::Quit);
    }

    #[test]
    fn roundtrip_ping() {
        assert_eq!(roundtrip_cmd(GuiCommand::Ping), GuiCommand::Ping);
    }

    #[test]
    fn roundtrip_clear_session() {
        assert_eq!(
            roundtrip_cmd(GuiCommand::ClearSession),
            GuiCommand::ClearSession
        );
    }

    #[test]
    fn roundtrip_response_ok_visible_true() {
        let r = GuiResponse::Ok { visible: true };
        assert_eq!(roundtrip_resp(r.clone()), r);
    }

    #[test]
    fn roundtrip_response_ok_visible_false() {
        let r = GuiResponse::Ok { visible: false };
        assert_eq!(roundtrip_resp(r.clone()), r);
    }

    #[test]
    fn roundtrip_response_error() {
        let r = GuiResponse::Error("gtk thread unresponsive".into());
        assert_eq!(roundtrip_resp(r.clone()), r);
    }

    #[test]
    fn version_mismatch_rejected() {
        let mut buf = Vec::<u8>::new();
        let json = serde_json::to_vec(&GuiCommand::Show).unwrap();
        let total_len: u32 = (2 + json.len()) as u32;
        buf.extend_from_slice(&total_len.to_be_bytes());
        buf.extend_from_slice(&99u16.to_be_bytes());
        buf.extend_from_slice(&json);
        let mut cur = Cursor::new(buf);
        let r: std::io::Result<GuiCommand> = read_frame_sync(&mut cur);
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("version mismatch"));
    }

    #[test]
    fn truncated_header_rejected() {
        let buf: Vec<u8> = vec![0x00, 0x00];
        let mut cur = Cursor::new(buf);
        let r: std::io::Result<GuiCommand> = read_frame_sync(&mut cur);
        assert!(r.is_err());
    }

    #[test]
    fn truncated_payload_rejected() {
        let mut buf = Vec::<u8>::new();
        let total_len: u32 = (2 + 100) as u32;
        buf.extend_from_slice(&total_len.to_be_bytes());
        buf.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
        buf.extend_from_slice(&[0u8; 10]);
        let mut cur = Cursor::new(buf);
        let r: std::io::Result<GuiCommand> = read_frame_sync(&mut cur);
        assert!(r.is_err());
    }

    #[test]
    fn frame_too_short_for_version_rejected() {
        let mut buf = Vec::<u8>::new();
        buf.extend_from_slice(&1u32.to_be_bytes());
        buf.push(0x00);
        let mut cur = Cursor::new(buf);
        let r: std::io::Result<GuiCommand> = read_frame_sync(&mut cur);
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("too short"));
    }

    #[test]
    fn gui_socket_path_format() {
        let p = gui_socket_path();
        let s = p.to_string_lossy();
        assert!(s.contains("lixun-gui.sock") || s.contains("lixun-gui-"));
    }

    #[tokio::test]
    async fn async_roundtrip_cmd() {
        let mut buf = Vec::<u8>::new();
        write_frame_async(&mut buf, &GuiCommand::Toggle)
            .await
            .unwrap();
        let mut cur = Cursor::new(buf);
        let c: GuiCommand = read_frame_async(&mut cur).await.unwrap();
        assert_eq!(c, GuiCommand::Toggle);
    }

    #[tokio::test]
    async fn async_roundtrip_response() {
        let mut buf = Vec::<u8>::new();
        let r = GuiResponse::Ok { visible: true };
        write_frame_async(&mut buf, &r).await.unwrap();
        let mut cur = Cursor::new(buf);
        let out: GuiResponse = read_frame_async(&mut cur).await.unwrap();
        assert_eq!(out, r);
    }

    #[tokio::test]
    async fn async_version_mismatch_rejected() {
        let mut buf = Vec::<u8>::new();
        let json = serde_json::to_vec(&GuiCommand::Show).unwrap();
        let total_len: u32 = (2 + json.len()) as u32;
        buf.extend_from_slice(&total_len.to_be_bytes());
        buf.extend_from_slice(&99u16.to_be_bytes());
        buf.extend_from_slice(&json);
        let mut cur = Cursor::new(buf);
        let r: std::io::Result<GuiCommand> = read_frame_async(&mut cur).await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn sync_written_frame_decodes_async() {
        let mut buf = Vec::<u8>::new();
        write_frame_sync(&mut buf, &GuiCommand::Quit).unwrap();
        let mut cur = Cursor::new(buf);
        let c: GuiCommand = read_frame_async(&mut cur).await.unwrap();
        assert_eq!(c, GuiCommand::Quit);
    }

    #[tokio::test]
    async fn async_written_frame_decodes_sync() {
        let mut buf = Vec::<u8>::new();
        write_frame_async(&mut buf, &GuiCommand::Ping)
            .await
            .unwrap();
        let mut cur = Cursor::new(buf);
        let c: GuiCommand = read_frame_sync(&mut cur).unwrap();
        assert_eq!(c, GuiCommand::Ping);
    }
}
