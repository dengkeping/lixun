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

use lixun_core::{DocId, Hit};
use lixun_ipc::{Phase, Request, Response, socket_path};

#[derive(Debug, Clone)]
pub(crate) enum IpcMessage {
    // The wire `Response::SearchChunk` also carries a `calculation`
    // field; the GUI drops it here — calculator results present as a
    // normal hit row, so the status-bar calculation path is gone.
    SearchChunk {
        epoch: u64,
        phase: Phase,
        hits: Vec<Hit>,
        top_hit: Option<DocId>,
        claimed: bool,
    },
    /// The search never reached the daemon (connect/write failed and
    /// the retry didn't either) or the reader lost the connection
    /// mid-search. Deliberately NOT a synthetic empty Final: an empty
    /// Final renders as an authoritative "No results" for a query the
    /// daemon never answered. The GUI shows a daemon-unresponsive
    /// state with a Relaunch affordance instead.
    TransportFailed { epoch: u64 },
    /// `READ_WATCHDOG` expired with a search still awaiting its
    /// Final. The reader keeps the connection open, so a late Final
    /// may still follow; the GUI keeps its spinner up rather than
    /// claiming "No results".
    SearchTimeout { epoch: u64 },
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
/// per-search deadline: on expiry the reader emits a `SearchTimeout`
/// notice for the current session (the GUI keeps its spinner up) and
/// KEEPS reading, so a slow-but-successful fused Final is never
/// discarded. Must exceed the daemon's semantic search timeout (5 s).
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
                // Tell the GUI the transport is down for the current
                // session so it can show an actionable error instead
                // of a fabricated "No results".
                if epoch_at_send == session_epoch.load(Ordering::SeqCst) {
                    let _ = event_tx.send_blocking(IpcMessage::TransportFailed {
                        epoch: epoch_at_send,
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

    // Highest epoch for which a Final (or terminal transport-failure
    // notice) was delivered.
    let mut last_final = 0u64;
    // Highest epoch the watchdog already flagged, so a search that
    // stays pending across several expiries produces one
    // `SearchTimeout`, not one per READ_WATCHDOG period.
    let mut last_timeout = 0u64;

    // Epoch still awaiting its Final for the *current* session, if
    // any, given the highest epoch already answered (`delivered`).
    let pending_epoch = |delivered: u64| -> Option<u64> {
        let sent = last_sent_epoch.load(Ordering::SeqCst);
        let session = session_epoch.load(Ordering::SeqCst);
        (sent == session && delivered < sent).then_some(sent)
    };

    let fail_if_pending = |last_final: &mut u64| {
        if let Some(epoch) = pending_epoch(*last_final) {
            let _ = event_tx.send_blocking(IpcMessage::TransportFailed { epoch });
            *last_final = epoch;
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
                // Flag it once but keep the connection open — a late
                // Final still gets delivered if the session hasn't
                // moved, and it will replace the GUI's slow-search
                // spinner when it lands.
                if let Some(epoch) = pending_epoch(last_final.max(last_timeout)) {
                    let _ = event_tx.send_blocking(IpcMessage::SearchTimeout { epoch });
                    last_timeout = epoch;
                }
                continue;
            }
            Err(e) => {
                tracing::debug!("ipc: reader connection closed: {}", e);
                fail_if_pending(&mut last_final);
                return;
            }
        }
        let resp_len = u32::from_be_bytes(header) as usize;
        if !(2..=lixun_ipc::MAX_FRAME_LEN).contains(&resp_len) {
            tracing::error!("ipc: bad response frame length {}", resp_len);
            fail_if_pending(&mut last_final);
            return;
        }
        let mut version_buf = [0u8; 2];
        if stream.read_exact(&mut version_buf).is_err() {
            fail_if_pending(&mut last_final);
            return;
        }
        let resp_version = u16::from_be_bytes(version_buf);
        let mut resp_buf = vec![0u8; resp_len - 2];
        if stream.read_exact(&mut resp_buf).is_err() {
            fail_if_pending(&mut last_final);
            return;
        }

        match lixun_ipc::decode_response(resp_version, &resp_buf) {
            Ok(Response::SearchChunk {
                epoch: resp_epoch,
                phase,
                hits,
                calculation: _,
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
                fail_if_pending(&mut last_final);
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

/// One-shot request/response round-trip over a fresh connection with
/// a 500 ms read bound. Shared core of every side-channel fetch
/// (history, status, recents, claimed prefixes): a wedged or
/// restarting daemon must degrade to "no data", never hang a caller.
/// Blocking — run on a worker thread unless the call site tolerates
/// up to ~500 ms (startup-only fetches).
fn one_shot_request(req: &Request) -> Option<Response> {
    let sock = socket_path();
    let buf = encode_frame(req)?;
    let mut stream = std::os::unix::net::UnixStream::connect(&sock).ok()?;
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(500)));
    stream.write_all(&buf).ok()?;

    let mut header = [0u8; 4];
    stream.read_exact(&mut header).ok()?;
    let resp_len = u32::from_be_bytes(header) as usize;
    if !(2..=lixun_ipc::MAX_FRAME_LEN).contains(&resp_len) {
        return None;
    }
    let mut version_buf = [0u8; 2];
    stream.read_exact(&mut version_buf).ok()?;
    let resp_version = u16::from_be_bytes(version_buf);
    let mut resp_buf = vec![0u8; resp_len - 2];
    stream.read_exact(&mut resp_buf).ok()?;
    lixun_ipc::decode_response(resp_version, &resp_buf).ok()
}

/// Run `fetch` on a worker thread and deliver its result back on the
/// GTK main loop. The standard pattern for keeping one-shot daemon
/// round-trips off the main thread (K8b): `std::thread` + bounded
/// `async_channel` + `glib::spawn_future_local`, mirroring
/// `start_ipc_thread`'s boundary. Must be called from the main
/// thread (the delivery future is spawned on the default
/// MainContext).
fn fetch_async<T: Send + 'static>(
    fetch: impl FnOnce() -> T + Send + 'static,
    on_done: impl FnOnce(T) + 'static,
) {
    let (tx, rx) = async_channel::bounded::<T>(1);
    std::thread::spawn(move || {
        let _ = tx.send_blocking(fetch());
    });
    glib::spawn_future_local(async move {
        if let Ok(value) = rx.recv().await {
            on_done(value);
        }
    });
}

pub(crate) fn request_search_history(limit: u32) -> Vec<String> {
    match one_shot_request(&Request::SearchHistory { limit }) {
        Some(Response::Queries(qs)) => qs,
        _ => Vec::new(),
    }
}

/// Async wrapper for [`request_search_history`]: the blocking 500 ms
/// round-trip runs on a worker thread; `on_done` runs back on the
/// GTK main loop (K8b — the Up-arrow history fetch must never stall
/// the key handler).
pub(crate) fn fetch_search_history_async(limit: u32, on_done: impl FnOnce(Vec<String>) + 'static) {
    fetch_async(move || request_search_history(limit), on_done);
}

/// Fetch the daemon's frecency-backed recent hits (O4) off the main
/// thread; `on_done` runs on the GTK main loop with the hydrated
/// hits (empty on transport failure or an older daemon).
pub(crate) fn fetch_recents_async(limit: u32, on_done: impl FnOnce(Vec<Hit>) + 'static) {
    fetch_async(
        move || match one_shot_request(&Request::Recents { limit }) {
            Some(Response::Recents { hits }) => hits,
            _ => Vec::new(),
        },
        on_done,
    );
}

/// Everything the GUI wants from one `Request::Status` round-trip:
/// the zero-hit indexing check plus the live semantic/config health
/// the daemon now reports (F6/O2).
#[derive(Debug, Clone, Default)]
pub(crate) struct DaemonStatusSnapshot {
    pub(crate) indexed_docs: u64,
    pub(crate) reindex_in_progress: bool,
    /// `Some((config_enabled, human state label, worker_ready))`
    /// when the daemon reports the semantic block.
    pub(crate) semantic: Option<(bool, String, bool)>,
    /// Config parse error the daemon fell back to defaults over.
    pub(crate) config_error: Option<String>,
}

/// One-shot `Request::Status` round-trip (500 ms bound). Blocking —
/// callers run this on a worker thread (never the GTK main thread).
pub(crate) fn request_daemon_status() -> Option<DaemonStatusSnapshot> {
    match one_shot_request(&Request::Status) {
        Some(Response::Status {
            indexed_docs,
            reindex_in_progress,
            semantic,
            daemon,
            ..
        }) => Some(DaemonStatusSnapshot {
            indexed_docs,
            reindex_in_progress,
            semantic: semantic.map(|s| {
                (
                    s.enabled,
                    s.state.to_string(),
                    s.state == lixun_ipc::SemanticWorkerState::Ready,
                )
            }),
            config_error: daemon.and_then(|d| d.config_error),
        }),
        _ => None,
    }
}

/// Async wrapper for [`request_daemon_status`].
pub(crate) fn fetch_daemon_status_async(
    on_done: impl FnOnce(Option<DaemonStatusSnapshot>) + 'static,
) {
    fetch_async(request_daemon_status, on_done);
}

/// Fire-and-forget preview page request (P11): the daemon forwards
/// it to the warm preview process as `PreviewCommand::Scroll`.
pub(crate) fn send_preview_scroll(down: bool, pages: u32) {
    send_request_fire_and_forget(&Request::PreviewScroll { down, pages });
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
    match one_shot_request(&Request::ClaimedPrefixes) {
        Some(Response::ClaimedPrefixes(p)) => p,
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
