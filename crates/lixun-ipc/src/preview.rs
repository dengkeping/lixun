//! Daemon ↔ preview-bin wire protocol for the long-lived preview
//! process.
//!
//! Lives in `lixun-ipc` (not `lixun-preview`) because both ends are
//! plugin-agnostic: the daemon names no plugin, the preview binary
//! receives a plugin-agnostic Hit and dispatches via inventory. The
//! preview trait crate (`lixun-preview`) does not need to depend on
//! IPC types; the preview binary depends on both.
//!
//! ## Frame format
//!
//! Mirrors the framing used by the main `lixun.sock` channel and by
//! `lixun-semantic-proto`:
//!
//! ```text
//! +-------------------+--------------------+--------------------+
//! | u32 BE total_len  | u16 BE proto_ver   | JSON payload       |
//! |   (= 2 + len(JSON)) |   = PROTOCOL_VERSION | (UTF-8, no LF)  |
//! +-------------------+--------------------+--------------------+
//! ```
//!
//! `total_len` covers the version field plus the JSON body but does
//! not include itself. Receivers that see a version outside
//! `MIN_PROTOCOL_VERSION..=PROTOCOL_VERSION` close the connection.
//!
//! ## Direction
//!
//! - The **daemon** sends [`PreviewCommand`] and receives
//!   [`PreviewEvent`].
//! - The **preview binary** sends [`PreviewEvent`] and receives
//!   [`PreviewCommand`].
//!
//! Each side uses a [`FrameCodec<I, O>`] parameterised by what it
//! decodes (`I`) and what it encodes (`O`):
//!
//! | side    | reads (`I`)        | writes (`O`)         |
//! |---------|--------------------|----------------------|
//! | daemon  | [`PreviewEvent`]   | [`PreviewCommand`]   |
//! | preview | [`PreviewCommand`] | [`PreviewEvent`]     |
//!
//! ## Lifecycle and back-pressure
//!
//! The preview process is long-lived: spawned lazily on the first
//! Space, kept warm across hide/show cycles, self-terminates after
//! 60 s of idle. The daemon owns one socket per spawned process,
//! at `$XDG_RUNTIME_DIR/lixun-preview-<pid>.sock`, mode 0600. A new
//! spawn allocates a fresh path; there is no well-known socket
//! (PID-reuse races and stale files would haunt us).
//!
//! Back-pressure is **latest-wins**, not FIFO. If the launcher
//! arrows-down through ten rows in 200 ms, the daemon coalesces
//! to a single `ShowOrUpdate` carrying the latest desired hit. The
//! preview side does the same coalescing on its end via the
//! monotonic `epoch` field — every async render path captures the
//! current epoch and re-checks before mutating the widget tree.
//!
//! ## Liveness
//!
//! EOF on the socket terminates both sides:
//!
//! - Preview reads EOF → daemon is gone → `app.quit()`.
//! - Daemon reads EOF on a Ready connection → preview crashed →
//!   transition to `PreviewLifecycle::Dead`, the next dispatch
//!   spawns a fresh process.

use std::marker::PhantomData;
use std::path::PathBuf;

use bytes::{Buf, BufMut, BytesMut};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio_util::codec::{Decoder, Encoder};

use lixun_core::Hit;

/// Wire version of the preview-channel protocol. Bumped independently
/// of the main `lixun.sock` `PROTOCOL_VERSION`: the two channels carry
/// different message types and evolve on different schedules.
pub const PROTOCOL_VERSION: u16 = 1;
pub const MIN_PROTOCOL_VERSION: u16 = 1;

/// Daemon → preview. Either a render command (`ShowOrUpdate`,
/// `Close`) or a liveness probe (`Ping`).
///
/// `epoch` is monotonically increasing per daemon process. The
/// preview side uses it to discard stale work in the rare case
/// where rapid arrow keys produce overlapping async loads.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PreviewCommand {
    /// Render `hit` in the preview window. If a previous render is
    /// already on screen, the host either calls
    /// `PreviewPlugin::update` on the existing widget (when both
    /// hits resolve to the same plugin and the plugin supports
    /// in-place update) or rebuilds and replaces the child widget
    /// (when the plugin changed or `update` returned the
    /// `UpdateUnsupported` sentinel).
    ///
    /// `monitor` is the connector name of the launcher's current
    /// monitor (`"eDP-1"`, `"DP-2"`, …). Recomputed on every
    /// command — not just the first Space — so the preview tracks
    /// the launcher across multi-monitor moves.
    ShowOrUpdate {
        epoch: u64,
        hit: Box<Hit>,
        monitor: Option<String>,
    },
    /// Hide the preview window but keep the process alive. Called
    /// by the daemon when the launcher hides (Escape, focus-loss
    /// outside preview-mode, launch). Resets the idle timer; if no
    /// further command arrives within 60 s the process self-quits.
    Close { epoch: u64 },
    /// Hide the preview window from launcher's keyboard input
    /// (Escape with preview_mode_active=true). Semantically the
    /// same as `Close`: window hides, process stays warm, idle
    /// timer scheduled. Distinct variant because the daemon must
    /// also reply to launcher with `ExitPreviewMode` to reset
    /// `preview_mode_active`. With `KeyboardMode::None` on the
    /// preview window (Wayland focus theft fix), all close-by-key
    /// flows route through the launcher and arrive here. The
    /// preview's own Escape/Space handlers no longer fire because
    /// the surface is keyboard-passive — see preview-bin
    /// `build_window_skeleton`.
    Hide { epoch: u64 },
    /// Liveness probe. Reserved for future supervision logic; the
    /// preview binary acks with no payload (the daemon notices the
    /// connection is alive). Not used by the current dispatcher.
    Ping,
    /// Install the launcher's exported xdg-foreign-v2 surface as the
    /// transient parent of the preview window. `handle` is the
    /// opaque string produced by `zxdg_exporter_v2::export_toplevel`
    /// on the launcher side. The preview binary feeds it to
    /// `zxdg_importer_v2::import_toplevel` and calls
    /// `xdg_imported_v2::set_parent_of` on its own toplevel surface.
    ///
    /// The daemon buffers this command if it arrives before
    /// [`PreviewEvent::Ready`] (same `latest_desired` pattern as
    /// `ShowOrUpdate`) and replays it once the preview side is up.
    /// Sent eagerly on the first `ShowOrUpdate` for a launcher
    /// session and re-sent if the launcher's surface changes (rare;
    /// only on launcher restart while preview is warm).
    ///
    /// Falls back to a plain centred toplevel when the compositor
    /// does not advertise xdg-foreign-v2; the launcher logs once
    /// per session and skips emitting this command.
    SetParent { handle: String },
    /// Drop the current transient-parent relationship. Sent when
    /// the launcher closes (Escape, focus-loss, launch) so the
    /// preview window is no longer constrained by a now-defunct
    /// parent surface. Best-effort — the preview also notices a
    /// destroyed parent via `zxdg_imported_v2.destroy` and emits
    /// [`PreviewEvent::ParentLost`].
    ClearParent,
}

/// Preview → daemon. Spawn handshake, completion notifications,
/// and out-of-band errors.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PreviewEvent {
    /// Sent once on connection accept, before any
    /// `ShowOrUpdate` is processed. The daemon waits for this
    /// event before transitioning the lifecycle from `Starting`
    /// to `Ready` and draining its `latest_desired` buffer. The
    /// `pid` echoes the preview process's own PID so the daemon
    /// can sanity-check against the spawn handle.
    Ready { pid: u32 },
    /// Acknowledgment that a `Close` was honoured. The window is
    /// no longer visible; the process is still warm waiting for
    /// the next `ShowOrUpdate` or for the idle timer to fire.
    Closed { epoch: u64 },
    /// Plugin build/update failed. The window may still be
    /// visible with the previous content; the daemon logs and
    /// keeps the process running.
    Error { epoch: u64, msg: String },
    /// The transient parent installed via
    /// [`PreviewCommand::SetParent`] is no longer valid: the
    /// compositor revoked the imported handle, the launcher
    /// surface was destroyed, or the import call itself failed.
    /// The preview window remains on screen as a plain toplevel;
    /// the daemon clears its cached handle so the next launcher
    /// session re-exports a fresh one.
    ParentLost,
}

/// Length-prefixed JSON codec parameterised by what each side
/// decodes (`I`) and what it encodes (`O`). Shape mirrors
/// [`lixun_semantic_proto::FrameCodec`] — both channels decided
/// on identical wire framing, so the codec implementation is
/// identical too. Kept independent of the semantic codec so
/// version bumps on either channel cannot accidentally break the
/// other.
pub struct FrameCodec<I, O> {
    state: DecodeState,
    _phantom: PhantomData<fn() -> (I, O)>,
}

#[derive(Default)]
enum DecodeState {
    #[default]
    Header,
    Version(usize),
    Payload(usize),
}

impl<I, O> Default for FrameCodec<I, O> {
    fn default() -> Self {
        Self {
            state: DecodeState::Header,
            _phantom: PhantomData,
        }
    }
}

impl<I, O> FrameCodec<I, O> {
    pub fn new() -> Self {
        Self::default()
    }
}

impl<I, O> Encoder<O> for FrameCodec<I, O>
where
    O: Serialize,
{
    type Error = std::io::Error;

    fn encode(&mut self, item: O, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let json = serde_json::to_vec(&item)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let total_len = 2 + json.len();
        if total_len > u32::MAX as usize {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "preview frame too large",
            ));
        }
        dst.reserve(4 + total_len);
        dst.put_u32(total_len as u32);
        dst.put_u16(PROTOCOL_VERSION);
        dst.put_slice(&json);
        Ok(())
    }
}

impl<I, O> Decoder for FrameCodec<I, O>
where
    I: DeserializeOwned,
{
    type Item = I;
    type Error = std::io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        loop {
            match self.state {
                DecodeState::Header => {
                    if src.len() < 4 {
                        return Ok(None);
                    }
                    let len = src.get_u32() as usize;
                    if len < 2 {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "preview frame too short for version",
                        ));
                    }
                    self.state = DecodeState::Version(len);
                }
                DecodeState::Version(len) => {
                    if src.len() < 2 {
                        return Ok(None);
                    }
                    let version = src.get_u16();
                    if !(MIN_PROTOCOL_VERSION..=PROTOCOL_VERSION).contains(&version) {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!(
                                "preview version {} outside supported {}..={}",
                                version, MIN_PROTOCOL_VERSION, PROTOCOL_VERSION
                            ),
                        ));
                    }
                    self.state = DecodeState::Payload(len - 2);
                }
                DecodeState::Payload(len) => {
                    if src.len() < len {
                        return Ok(None);
                    }
                    let data = src.split_to(len);
                    let item: I = serde_json::from_slice(&data)
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                    self.state = DecodeState::Header;
                    return Ok(Some(item));
                }
            }
        }
    }
}

/// Codec for the daemon side: reads [`PreviewEvent`], writes
/// [`PreviewCommand`].
pub type DaemonPreviewCodec = FrameCodec<PreviewEvent, PreviewCommand>;

/// Codec for the preview-bin side: reads [`PreviewCommand`], writes
/// [`PreviewEvent`].
pub type PreviewBinCodec = FrameCodec<PreviewCommand, PreviewEvent>;

/// Per-process socket path under `$XDG_RUNTIME_DIR`. Falls back to a
/// per-user directory under `/tmp` (chmod 0700) when the runtime dir
/// is unset (rare; only happens outside a logind session). Each
/// spawn allocates its own PID-tagged path; there is no well-known
/// socket because PID reuse would bind the next preview process to a
/// stale connection.
pub fn preview_socket_path(pid: u32) -> std::io::Result<PathBuf> {
    let runtime = match dirs::runtime_dir() {
        Some(p) => p,
        None => {
            let tmp_base =
                std::env::temp_dir().join(format!("lixun-{}", unsafe { libc::getuid() }));
            std::fs::create_dir_all(&tmp_base)?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp_base, std::fs::Permissions::from_mode(0o700))?;
            tmp_base
        }
    };
    std::fs::create_dir_all(&runtime)?;
    Ok(runtime.join(format!("lixun-preview-{}.sock", pid)))
}

// -- Sync (blocking) frame helpers for preview-bin -------------------------
//
// preview-bin runs no tokio runtime (single-threaded GTK + a worker
// thread for socket I/O), so it cannot reuse the async `FrameCodec`
// types above. These helpers mirror the byte layout of the codec but
// use blocking std::io and reference *this module's* PROTOCOL_VERSION
// rather than `crate::PROTOCOL_VERSION`. The two version constants
// evolve independently — bumping one must never silently bump the
// other.

/// Encode `msg` and write the framed bytes to `w`. Blocks until the
/// full frame is written or I/O fails.
pub fn write_frame_sync<W, T>(w: &mut W, msg: &T) -> std::io::Result<()>
where
    W: std::io::Write,
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
/// payload decode failure. EOF mid-frame propagates as `UnexpectedEof`;
/// EOF before any header byte propagates the same way and the caller
/// (preview-bin worker thread) treats it as "daemon disconnected".
pub fn read_frame_sync<R, T>(r: &mut R) -> std::io::Result<T>
where
    R: std::io::Read,
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
    if version < MIN_PROTOCOL_VERSION || version > PROTOCOL_VERSION {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "preview protocol version mismatch: supported {}..={}, got {}",
                MIN_PROTOCOL_VERSION, PROTOCOL_VERSION, version
            ),
        ));
    }
    let payload_len = total_len - 2;
    let mut payload = vec![0u8; payload_len];
    r.read_exact(&mut payload)?;
    serde_json::from_slice(&payload)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;
    use lixun_core::{Action, Category, DocId, Hit, RowMenuDef};
    use std::path::PathBuf;

    fn sample_hit() -> Hit {
        Hit {
            id: DocId("fs:/tmp/demo.txt".into()),
            category: Category::File,
            title: "demo.txt".into(),
            subtitle: "/tmp".into(),
            icon_name: None,
            kind_label: None,
            score: 1.0,
            action: Action::OpenFile {
                path: PathBuf::from("/tmp/demo.txt"),
            },
            extract_fail: false,
            sender: None,
            recipients: None,
            body: None,
            secondary_action: None,
            source_instance: String::new(),
            row_menu: RowMenuDef::empty(),
            mime: None,
        }
    }

    #[test]
    fn command_roundtrip_show_or_update() {
        let mut codec: PreviewBinCodec = FrameCodec::new();
        let mut peer: DaemonPreviewCodec = FrameCodec::new();
        let mut buf = BytesMut::new();
        let cmd = PreviewCommand::ShowOrUpdate {
            epoch: 7,
            hit: Box::new(sample_hit()),
            monitor: Some("eDP-1".into()),
        };
        peer.encode(cmd.clone(), &mut buf).unwrap();
        let decoded = codec.decode(&mut buf).unwrap().unwrap();
        match decoded {
            PreviewCommand::ShowOrUpdate {
                epoch,
                hit,
                monitor,
            } => {
                assert_eq!(epoch, 7);
                assert_eq!(hit.id.0, "fs:/tmp/demo.txt");
                assert_eq!(monitor.as_deref(), Some("eDP-1"));
            }
            other => panic!("expected ShowOrUpdate, got {:?}", other),
        }
    }

    #[test]
    fn command_roundtrip_close_and_ping() {
        let mut codec: PreviewBinCodec = FrameCodec::new();
        let mut peer: DaemonPreviewCodec = FrameCodec::new();
        let mut buf = BytesMut::new();
        peer.encode(PreviewCommand::Close { epoch: 3 }, &mut buf)
            .unwrap();
        peer.encode(PreviewCommand::Ping, &mut buf).unwrap();
        match codec.decode(&mut buf).unwrap().unwrap() {
            PreviewCommand::Close { epoch } => assert_eq!(epoch, 3),
            other => panic!("expected Close, got {:?}", other),
        }
        match codec.decode(&mut buf).unwrap().unwrap() {
            PreviewCommand::Ping => {}
            other => panic!("expected Ping, got {:?}", other),
        }
        assert!(codec.decode(&mut buf).unwrap().is_none());
    }

    #[test]
    fn event_roundtrip_ready_closed_error() {
        let mut codec: DaemonPreviewCodec = FrameCodec::new();
        let mut peer: PreviewBinCodec = FrameCodec::new();
        let mut buf = BytesMut::new();
        peer.encode(PreviewEvent::Ready { pid: 4242 }, &mut buf)
            .unwrap();
        peer.encode(PreviewEvent::Closed { epoch: 9 }, &mut buf)
            .unwrap();
        peer.encode(
            PreviewEvent::Error {
                epoch: 11,
                msg: "boom".into(),
            },
            &mut buf,
        )
        .unwrap();

        match codec.decode(&mut buf).unwrap().unwrap() {
            PreviewEvent::Ready { pid } => assert_eq!(pid, 4242),
            other => panic!("expected Ready, got {:?}", other),
        }
        match codec.decode(&mut buf).unwrap().unwrap() {
            PreviewEvent::Closed { epoch } => assert_eq!(epoch, 9),
            other => panic!("expected Closed, got {:?}", other),
        }
        match codec.decode(&mut buf).unwrap().unwrap() {
            PreviewEvent::Error { epoch, msg } => {
                assert_eq!(epoch, 11);
                assert_eq!(msg, "boom");
            }
            other => panic!("expected Error, got {:?}", other),
        }
    }

    #[test]
    fn command_roundtrip_set_parent_and_clear_parent() {
        let mut codec: PreviewBinCodec = FrameCodec::new();
        let mut peer: DaemonPreviewCodec = FrameCodec::new();
        let mut buf = BytesMut::new();
        peer.encode(
            PreviewCommand::SetParent {
                handle: "xdg-foreign-handle-abc123".into(),
            },
            &mut buf,
        )
        .unwrap();
        peer.encode(PreviewCommand::ClearParent, &mut buf).unwrap();
        match codec.decode(&mut buf).unwrap().unwrap() {
            PreviewCommand::SetParent { handle } => {
                assert_eq!(handle, "xdg-foreign-handle-abc123");
            }
            other => panic!("expected SetParent, got {:?}", other),
        }
        match codec.decode(&mut buf).unwrap().unwrap() {
            PreviewCommand::ClearParent => {}
            other => panic!("expected ClearParent, got {:?}", other),
        }
        assert!(codec.decode(&mut buf).unwrap().is_none());
    }

    #[test]
    fn event_roundtrip_parent_lost() {
        let mut codec: DaemonPreviewCodec = FrameCodec::new();
        let mut peer: PreviewBinCodec = FrameCodec::new();
        let mut buf = BytesMut::new();
        peer.encode(PreviewEvent::ParentLost, &mut buf).unwrap();
        match codec.decode(&mut buf).unwrap().unwrap() {
            PreviewEvent::ParentLost => {}
            other => panic!("expected ParentLost, got {:?}", other),
        }
    }

    #[test]
    fn rejects_unsupported_version() {
        let mut codec: PreviewBinCodec = FrameCodec::new();
        let mut buf = BytesMut::new();
        // total_len = 2 + 2 = 4 (version + minimal "{}" payload),
        // but version is 999 — out of supported range.
        buf.put_u32(4);
        buf.put_u16(999);
        buf.put_slice(b"{}");
        let err = codec.decode(&mut buf).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn socket_path_is_pid_tagged_under_runtime_dir_or_tmp() {
        let path = preview_socket_path(12345).unwrap();
        let s = path.to_string_lossy();
        assert!(
            s.contains("lixun-preview-12345.sock"),
            "expected pid-tagged socket name, got {}",
            s
        );
    }

    #[test]
    fn sync_helpers_roundtrip_through_pipe() {
        let cmd = PreviewCommand::Close { epoch: 7 };
        let mut buf: Vec<u8> = Vec::new();
        write_frame_sync(&mut buf, &cmd).unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let decoded: PreviewCommand = read_frame_sync(&mut cursor).unwrap();
        match decoded {
            PreviewCommand::Close { epoch } => assert_eq!(epoch, 7),
            other => panic!("expected Close, got {:?}", other),
        }
    }

    #[test]
    fn sync_read_rejects_unsupported_version() {
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(&4u32.to_be_bytes());
        buf.extend_from_slice(&999u16.to_be_bytes());
        buf.extend_from_slice(b"{}");
        let mut cursor = std::io::Cursor::new(buf);
        let err = read_frame_sync::<_, PreviewCommand>(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}
