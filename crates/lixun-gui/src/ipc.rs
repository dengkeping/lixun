//! IPC client: push-based event delivery via async_channel.
//!
//! Architecture change from polling: IPC thread sends typed IpcMessage events
//! through async_channel, GUI receives via glib::spawn_future_local. Each
//! message carries epoch for stale-detection. Final-only batching: Initial
//! chunks buffered, only Final triggers GTK model update (single rebuild per
//! query vs previous double rebuild).
//!
//! The search path holds ONE persistent daemon connection: requests are
//! written immediately on keystroke and a dedicated reader thread streams
//! chunks back. Sending every search on the same connection is what arms
//! the daemon's same-connection preemption (a new Search cancels the
//! previous in-flight one server-side), and it removes the per-keystroke
//! connect + connection-setup cost of the old one-shot transport.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use lixun_core::{Calculation, DocId, Hit};
use lixun_ipc::{Phase, Request, Response, socket_path};

#[derive(Debug, Clone)]
pub(crate) enum IpcMessage {
    SearchChunk {
        epoch: u64,
        phase: Phase,
        hits: Vec<Hit>,
        calculation: Option<Calculation>,
        top_hit: Option<DocId>,
        claimed: bool,
    },
}

pub(crate) struct IpcClient {
    pub(crate) request_tx: mpsc::Sender<(String, u32, u64)>,
}

impl Clone for IpcClient {
    fn clone(&self) -> Self {
        Self {
            request_tx: self.request_tx.clone(),
        }
    }
}

/// The persistent search connection: the writer half plus a liveness flag
/// the companion reader clears when it exits. The two halves are separate
/// fds (`try_clone` dups the socket), so a dead reader does NOT make the
/// writer's `write_all` fail — without this flag the writer would keep
/// shipping searches nobody reads. Checked before every write.
struct Conn {
    stream: std::os::unix::net::UnixStream,
    reader_alive: Arc<AtomicBool>,
}

/// Clears the connection's liveness flag on every `read_loop` exit path.
struct ReaderAliveGuard(Arc<AtomicBool>);

impl Drop for ReaderAliveGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Read timeout on the persistent connection. This is a watchdog, not a
/// per-search deadline: on expiry the reader synthesizes a Final for the
/// current session (unblocking the spinner) and KEEPS reading, so a
/// slow-but-successful fused Final is never discarded. Must exceed the
/// daemon's semantic search timeout (5 s).
const READ_WATCHDOG: Duration = Duration::from_secs(10);

pub(crate) fn start_ipc_thread(
    session_epoch: Arc<AtomicU64>,
) -> (IpcClient, async_channel::Receiver<IpcMessage>) {
    let (tx, rx) = mpsc::channel::<(String, u32, u64)>();
    let (event_tx, event_rx) = async_channel::unbounded::<IpcMessage>();
    // Epoch of the most recently written Search. The reader watchdog uses
    // this to know whether a search is still awaiting its Final chunk.
    let last_sent_epoch = Arc::new(AtomicU64::new(0));

    std::thread::spawn(move || {
        let mut conn: Option<Conn> = None;

        while let Ok((query, limit, epoch_at_send)) = rx.recv() {
            if epoch_at_send != session_epoch.load(Ordering::SeqCst) {
                tracing::debug!(
                    "ipc: skipping superseded search request (epoch {})",
                    epoch_at_send
                );
                continue;
            }
            tracing::debug!(
                "ipc: sending search request query={:?} limit={} epoch={}",
                query,
                limit,
                epoch_at_send
            );

            let req = Request::Search {
                q: query,
                limit,
                explain: false,
                epoch: epoch_at_send,
            };
            let Some(frame) = encode_frame(&req) else {
                continue;
            };

            last_sent_epoch.store(epoch_at_send, Ordering::SeqCst);

            // Try the live connection first; on write failure (daemon
            // restarted, socket dropped) reconnect once and retry.
            let mut sent = false;
            for _attempt in 0..2 {
                // A dead reader leaves a writable socket behind, so drop the
                // connection here rather than waiting for a write to fail.
                if conn
                    .as_ref()
                    .is_some_and(|c| !c.reader_alive.load(Ordering::SeqCst))
                {
                    tracing::debug!("ipc: reader thread gone, reconnecting");
                    conn = None;
                }
                if conn.is_none() {
                    conn = connect_with_reader(&session_epoch, &event_tx, &last_sent_epoch);
                    if conn.is_none() {
                        break;
                    }
                }
                if let Some(c) = conn.as_mut() {
                    if c.stream.write_all(&frame).is_ok() {
                        sent = true;
                        break;
                    }
                    tracing::debug!("ipc: write failed, reconnecting");
                    conn = None;
                }
            }

            if !sent {
                tracing::error!("ipc: failed to send search request to daemon");
                // Unblock the spinner for the current session.
                if epoch_at_send == session_epoch.load(Ordering::SeqCst) {
                    let _ = event_tx.send_blocking(IpcMessage::SearchChunk {
                        epoch: epoch_at_send,
                        phase: Phase::Final,
                        hits: Vec::new(),
                        calculation: None,
                        top_hit: None,
                        claimed: false,
                    });
                }
            }
        }
    });

    (IpcClient { request_tx: tx }, event_rx)
}

/// Connect to the daemon and spawn the companion reader thread that
/// consumes response frames for the connection's whole lifetime.
fn connect_with_reader(
    session_epoch: &Arc<AtomicU64>,
    event_tx: &async_channel::Sender<IpcMessage>,
    last_sent_epoch: &Arc<AtomicU64>,
) -> Option<Conn> {
    let sock = socket_path();
    let stream = match std::os::unix::net::UnixStream::connect(&sock) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("Failed to connect to daemon at {:?}: {}", sock, e);
            return None;
        }
    };
    let reader = match stream.try_clone() {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Failed to clone daemon stream for reader: {}", e);
            return None;
        }
    };
    let reader_alive = Arc::new(AtomicBool::new(true));
    let session_epoch = Arc::clone(session_epoch);
    let event_tx = event_tx.clone();
    let last_sent_epoch = Arc::clone(last_sent_epoch);
    let alive = Arc::clone(&reader_alive);
    std::thread::spawn(move || read_loop(reader, session_epoch, event_tx, last_sent_epoch, alive));
    Some(Conn {
        stream,
        reader_alive,
    })
}

/// Reader half of the persistent connection: streams response frames,
/// drops stale chunks (session epoch moved on), and forwards live ones
/// to the GUI event channel. Exits when the connection dies; clearing
/// `reader_alive` on the way out tells the writer to reconnect on the
/// next request instead of writing into a socket nobody is draining.
fn read_loop(
    mut stream: std::os::unix::net::UnixStream,
    session_epoch: Arc<AtomicU64>,
    event_tx: async_channel::Sender<IpcMessage>,
    last_sent_epoch: Arc<AtomicU64>,
    reader_alive: Arc<AtomicBool>,
) {
    let _alive = ReaderAliveGuard(reader_alive);

    if let Err(e) = stream.set_read_timeout(Some(READ_WATCHDOG)) {
        tracing::error!("Failed to set read watchdog: {}", e);
        return;
    }

    // Highest epoch for which a Final (real or synthetic) was delivered.
    let mut last_final = 0u64;

    let synth_final_if_pending = |last_final: &mut u64| {
        let sent = last_sent_epoch.load(Ordering::SeqCst);
        let session = session_epoch.load(Ordering::SeqCst);
        if sent == session && *last_final < sent {
            let _ = event_tx.send_blocking(IpcMessage::SearchChunk {
                epoch: sent,
                phase: Phase::Final,
                hits: Vec::new(),
                calculation: None,
                top_hit: None,
                claimed: false,
            });
            *last_final = sent;
        }
    };

    loop {
        let mut header = [0u8; 4];
        match stream.read_exact(&mut header) {
            Ok(()) => {}
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                // Watchdog: a search has gone unanswered for READ_WATCHDOG.
                // Unblock the GUI but keep the connection open — a late
                // Final still gets delivered if the session hasn't moved.
                synth_final_if_pending(&mut last_final);
                continue;
            }
            Err(e) => {
                tracing::debug!("ipc: reader connection closed: {}", e);
                synth_final_if_pending(&mut last_final);
                return;
            }
        }
        let resp_len = u32::from_be_bytes(header) as usize;
        if !(2..=lixun_ipc::MAX_FRAME_LEN).contains(&resp_len) {
            tracing::error!("ipc: bad response frame length {}", resp_len);
            synth_final_if_pending(&mut last_final);
            return;
        }
        let mut version_buf = [0u8; 2];
        if stream.read_exact(&mut version_buf).is_err() {
            synth_final_if_pending(&mut last_final);
            return;
        }
        let resp_version = u16::from_be_bytes(version_buf);
        let mut resp_buf = vec![0u8; resp_len - 2];
        if stream.read_exact(&mut resp_buf).is_err() {
            synth_final_if_pending(&mut last_final);
            return;
        }

        match lixun_ipc::decode_response(resp_version, &resp_buf) {
            Ok(Response::SearchChunk {
                epoch: resp_epoch,
                phase,
                hits,
                calculation,
                top_hit,
                explanations: _,
                claimed,
            }) => {
                // Authoritative stale check at hand-off time: the chunk
                // belongs to the session it was requested in; anything
                // else is a superseded search still draining.
                if resp_epoch != session_epoch.load(Ordering::SeqCst) {
                    tracing::debug!(
                        "ipc: dropping stale chunk (epoch {}, session moved on)",
                        resp_epoch
                    );
                    continue;
                }
                let is_final = matches!(phase, Phase::Final);
                tracing::debug!(
                    "ipc: chunk received epoch={} phase={:?} hits={}",
                    resp_epoch,
                    phase,
                    hits.len()
                );
                let _ = event_tx.send_blocking(IpcMessage::SearchChunk {
                    epoch: resp_epoch,
                    phase,
                    hits,
                    calculation,
                    top_hit,
                    claimed,
                });
                if is_final {
                    last_final = last_final.max(resp_epoch);
                }
            }
            Ok(Response::Cancelled {
                epoch: cancelled_epoch,
            }) => {
                // Server-side preemption ack for a superseded search;
                // nothing to deliver.
                tracing::debug!("ipc: search cancelled server-side (epoch {})", cancelled_epoch);
            }
            Ok(other) => {
                tracing::debug!("ipc: ignoring unexpected response variant: {:?}", other);
            }
            Err(e) => {
                tracing::error!("Failed to deserialize response: {}", e);
                synth_final_if_pending(&mut last_final);
                return;
            }
        }
    }
}

/// Length-prefix + version frame for a request, ready to write.
fn encode_frame(req: &Request) -> Option<Vec<u8>> {
    let (version, payload) = match lixun_ipc::encode_request(req) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("Failed to serialize request: {}", e);
            return None;
        }
    };
    let total_len = (2 + payload.len()) as u32;
    let mut buf = Vec::with_capacity(4 + 2 + payload.len());
    buf.extend_from_slice(&total_len.to_be_bytes());
    buf.extend_from_slice(&version.to_be_bytes());
    buf.extend_from_slice(&payload);
    Some(buf)
}

#[allow(dead_code)]
pub(crate) fn send_record_query(q: &str) {
    send_request_fire_and_forget(&Request::RecordQuery { q: q.to_string() });
}

pub(crate) fn request_search_history(limit: u32) -> Vec<String> {
    let sock = socket_path();
    let Some(buf) = encode_frame(&Request::SearchHistory { limit }) else {
        return Vec::new();
    };

    let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&sock) else {
        return Vec::new();
    };
    if stream.write_all(&buf).is_err() {
        return Vec::new();
    }

    let mut header = [0u8; 4];
    if stream.read_exact(&mut header).is_err() {
        return Vec::new();
    }
    let resp_len = u32::from_be_bytes(header) as usize;
    if resp_len < 2 {
        return Vec::new();
    }
    let mut version_buf = [0u8; 2];
    if stream.read_exact(&mut version_buf).is_err() {
        return Vec::new();
    }
    let resp_version = u16::from_be_bytes(version_buf);
    let mut resp_buf = vec![0u8; resp_len - 2];
    if stream.read_exact(&mut resp_buf).is_err() {
        return Vec::new();
    }
    match lixun_ipc::decode_response(resp_version, &resp_buf) {
        Ok(Response::Queries(qs)) => qs,
        _ => Vec::new(),
    }
}

pub(crate) fn dispatch_click_pair(doc_id: &str, query: &str) {
    for req in build_click_pair(doc_id, query) {
        send_request_fire_and_forget(&req);
    }
}

/// Build the two-request click pair: always a `RecordClick`, and a
/// `RecordQueryClick` iff `query` is non-empty. Returned in dispatch
/// order (frecency first, latch second) so a reader of the trace can
/// distinguish the two side-effects by arrival order. Kept pure so
/// dual-emit semantics are unit-testable without a live socket.
pub(crate) fn build_click_pair(doc_id: &str, query: &str) -> Vec<Request> {
    let mut out = Vec::with_capacity(2);
    out.push(Request::RecordClick {
        doc_id: doc_id.to_string(),
    });
    if !query.is_empty() {
        out.push(Request::RecordQueryClick {
            doc_id: doc_id.to_string(),
            query: query.to_string(),
        });
    }
    out
}

fn send_request_fire_and_forget(req: &Request) {
    let sock = socket_path();
    let Some(buf) = encode_frame(req) else {
        return;
    };
    if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&sock) {
        let _ = stream.write_all(&buf);
    }
}

pub(crate) fn fetch_claimed_prefixes() -> Vec<String> {
    use std::io::Read;
    let sock = socket_path();
    let Some(buf) = encode_frame(&Request::ClaimedPrefixes) else {
        return Vec::new();
    };
    let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&sock) else {
        return Vec::new();
    };
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(500)));
    if std::io::Write::write_all(&mut stream, &buf).is_err() {
        return Vec::new();
    }
    let mut header = [0u8; 4];
    if stream.read_exact(&mut header).is_err() {
        return Vec::new();
    }
    let resp_len = u32::from_be_bytes(header) as usize;
    if resp_len < 2 {
        return Vec::new();
    }
    let mut version_buf = [0u8; 2];
    if stream.read_exact(&mut version_buf).is_err() {
        return Vec::new();
    }
    let resp_version = u16::from_be_bytes(version_buf);
    let mut body = vec![0u8; resp_len - 2];
    if stream.read_exact(&mut body).is_err() {
        return Vec::new();
    }
    match lixun_ipc::decode_response(resp_version, &body) {
        Ok(Response::ClaimedPrefixes(p)) => p,
        _ => Vec::new(),
    }
}

/// Resolve the connector name of the monitor that `window` is
/// currently placed on. Returns `None` if the window hasn't been
/// mapped yet or no monitor can be determined (unusual — happens
/// on initial show race). The connector string (`"eDP-1"`,
/// `"DP-2"`, …) is what `lixun-preview` matches against its own
/// `display.monitors()` list to open on the same screen.
pub(crate) fn current_monitor_connector(window: &gtk::ApplicationWindow) -> Option<String> {
    use gtk::prelude::*;
    let surface = window.surface()?;
    let display = gtk::prelude::WidgetExt::display(window);
    let monitor = display.monitor_at_surface(&surface)?;
    monitor.connector().map(|s| String::from(s.as_str()))
}

pub(crate) fn send_preview_request(hit: &Hit, monitor: Option<String>) {
    tracing::info!(
        "gui: send_preview_request hit_id={} monitor={:?}",
        hit.id.0,
        monitor
    );
    send_request_fire_and_forget(&Request::Preview {
        hit: Box::new(hit.clone()),
        monitor,
    });
}

pub(crate) fn send_launcher_geometry(monitor: String, x: i32, y: i32, w: i32, h: i32) {
    tracing::debug!(
        "gui: send_launcher_geometry monitor={} x={} y={} w={} h={}",
        monitor,
        x,
        y,
        w,
        h
    );
    send_request_fire_and_forget(&Request::LauncherGeometry {
        monitor,
        x,
        y,
        w,
        h,
    });
}

pub(crate) fn send_preview_hide_request() {
    tracing::info!("gui: send_preview_hide_request");
    send_request_fire_and_forget(&Request::PreviewHide);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn click_emits_both_requests_with_query() {
        let pair = build_click_pair("fs:/tmp/foo.txt", "fo");
        assert_eq!(pair.len(), 2, "both requests expected when query set");
        match &pair[0] {
            Request::RecordClick { doc_id } => assert_eq!(doc_id, "fs:/tmp/foo.txt"),
            other => panic!("expected RecordClick first, got {:?}", other),
        }
        match &pair[1] {
            Request::RecordQueryClick { doc_id, query } => {
                assert_eq!(doc_id, "fs:/tmp/foo.txt");
                assert_eq!(query, "fo");
            }
            other => panic!("expected RecordQueryClick second, got {:?}", other),
        }
    }

    #[test]
    fn click_emits_only_record_click_for_empty_query() {
        let pair = build_click_pair("fs:/tmp/foo.txt", "");
        assert_eq!(
            pair.len(),
            1,
            "empty query must not populate the latch — RecordQueryClick suppressed"
        );
        assert!(matches!(&pair[0], Request::RecordClick { .. }));
    }

    #[test]
    fn v4_search_chunk_roundtrip() {
        let resp = Response::SearchChunk {
            epoch: 42,
            phase: Phase::Initial,
            hits: Vec::new(),
            calculation: None,
            top_hit: Some(lixun_core::DocId("app:editor-a".into())),
            explanations: vec![],
            claimed: false,
        };
        let bytes = serde_json::to_vec(&resp).unwrap();
        let roundtrip: Response = serde_json::from_slice(&bytes).unwrap();
        match roundtrip {
            Response::SearchChunk {
                epoch,
                phase,
                top_hit,
                ..
            } => {
                assert_eq!(epoch, 42);
                assert_eq!(phase, Phase::Initial);
                assert_eq!(top_hit.as_ref().map(|d| d.0.as_str()), Some("app:editor-a"));
            }
            other => panic!("expected SearchChunk, got {:?}", other),
        }
    }
}
