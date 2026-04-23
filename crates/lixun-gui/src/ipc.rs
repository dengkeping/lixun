//! IPC client: background thread sending search requests and a fire-and-forget
//! record-click helper.
//!
//! Service-mode session epoch (G1.6): each request carries the session
//! epoch at send-time. The response thread compares that snapshot to the
//! current epoch before committing results; hides bump the epoch via
//! `LauncherController::reset_session` so stale replies land in a new
//! session and are discarded.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use lixun_core::{Calculation, DocId, Hit};
use lixun_ipc::{socket_path, Request, Response, PROTOCOL_VERSION};

pub(crate) struct IpcClient {
    pub(crate) request_tx: mpsc::Sender<(String, u32, u64)>,
    pub(crate) responses: Arc<Mutex<Vec<Hit>>>,
    pub(crate) calculation: Arc<Mutex<Option<Calculation>>>,
    /// Protocol v3: id of the hit the daemon selected as Top Hit for
    /// the last search reply (`None` if no confident pick). The
    /// window render path uses this to decide whether to populate the
    /// hero region above the results list. Reset to `None` on any v1
    /// or v2 response arm so a v3 → v2 downgrade (unlikely in practice
    /// but defensive) does not leave stale hero state.
    pub(crate) top_hit: Arc<Mutex<Option<DocId>>>,
    pub(crate) response_epoch: Arc<AtomicU64>,
}

impl Clone for IpcClient {
    fn clone(&self) -> Self {
        Self {
            request_tx: self.request_tx.clone(),
            responses: Arc::clone(&self.responses),
            calculation: Arc::clone(&self.calculation),
            top_hit: Arc::clone(&self.top_hit),
            response_epoch: Arc::clone(&self.response_epoch),
        }
    }
}

pub(crate) fn start_ipc_thread(session_epoch: Arc<AtomicU64>) -> IpcClient {
    let (tx, rx) = mpsc::channel::<(String, u32, u64)>();
    let responses: Arc<Mutex<Vec<Hit>>> = Arc::new(Mutex::new(Vec::new()));
    let calculation: Arc<Mutex<Option<Calculation>>> = Arc::new(Mutex::new(None));
    let top_hit: Arc<Mutex<Option<DocId>>> = Arc::new(Mutex::new(None));
    let response_epoch: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
    let resp_clone = Arc::clone(&responses);
    let calc_clone = Arc::clone(&calculation);
    let top_hit_clone = Arc::clone(&top_hit);
    let epoch_clone = Arc::clone(&response_epoch);

    std::thread::spawn(move || {
        while let Ok((query, limit, epoch_at_send)) = rx.recv() {
            let sock = socket_path();
            let req = Request::Search { q: query, limit };
            let json = match serde_json::to_vec(&req) {
                Ok(j) => j,
                Err(e) => {
                    tracing::error!("Failed to serialize search request: {}", e);
                    continue;
                }
            };
            let total_len = (2 + json.len()) as u32;
            let mut buf = Vec::with_capacity(4 + 2 + json.len());
            buf.extend_from_slice(&total_len.to_be_bytes());
            buf.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
            buf.extend_from_slice(&json);

            let mut stream = match std::os::unix::net::UnixStream::connect(&sock) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("Failed to connect to daemon at {:?}: {}", sock, e);
                    continue;
                }
            };

            if let Err(e) = stream.write_all(&buf) {
                tracing::error!("Failed to send search request: {}", e);
                continue;
            }

            let mut header = [0u8; 4];
            if let Err(e) = stream.read_exact(&mut header) {
                tracing::error!("Failed to read response header: {}", e);
                continue;
            }
            let resp_len = u32::from_be_bytes(header) as usize;
            if resp_len < 2 {
                tracing::error!("Response frame too short");
                continue;
            }
            let mut _version = [0u8; 2];
            if let Err(e) = stream.read_exact(&mut _version) {
                tracing::error!("Failed to read response version: {}", e);
                continue;
            }
            let mut resp_buf = vec![0u8; resp_len - 2];
            if let Err(e) = stream.read_exact(&mut resp_buf) {
                tracing::error!("Failed to read response body: {}", e);
                continue;
            }

            if epoch_at_send != session_epoch.load(Ordering::SeqCst) {
                tracing::debug!(
                    "ipc: dropping reply from stale session (sent in epoch {})",
                    epoch_at_send
                );
                continue;
            }

            match serde_json::from_slice::<Response>(&resp_buf) {
                Ok(Response::Hits(hits)) => {
                    if let Ok(mut r) = resp_clone.lock() {
                        *r = hits;
                    }
                    if let Ok(mut c) = calc_clone.lock() {
                        *c = None;
                    }
                    if let Ok(mut t) = top_hit_clone.lock() {
                        *t = None;
                    }
                    epoch_clone.fetch_add(1, Ordering::SeqCst);
                }
                Ok(Response::HitsWithExtras { hits, calculation }) => {
                    if let Ok(mut r) = resp_clone.lock() {
                        *r = hits;
                    }
                    if let Ok(mut c) = calc_clone.lock() {
                        *c = calculation;
                    }
                    if let Ok(mut t) = top_hit_clone.lock() {
                        *t = None;
                    }
                    epoch_clone.fetch_add(1, Ordering::SeqCst);
                }
                Ok(Response::HitsWithExtrasV3 {
                    hits,
                    calculation,
                    top_hit,
                }) => {
                    if let Ok(mut r) = resp_clone.lock() {
                        *r = hits;
                    }
                    if let Ok(mut c) = calc_clone.lock() {
                        *c = calculation;
                    }
                    if let Ok(mut t) = top_hit_clone.lock() {
                        *t = top_hit;
                    }
                    epoch_clone.fetch_add(1, Ordering::SeqCst);
                }
                Ok(Response::Error(msg)) => {
                    tracing::error!("Daemon error: {}", msg);
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::error!("Failed to parse response: {}", e);
                }
            }
        }
    });

    IpcClient {
        request_tx: tx,
        responses,
        calculation,
        top_hit,
        response_epoch,
    }
}

#[allow(dead_code)]
pub(crate) fn send_record_query(q: &str) {
    let sock = socket_path();
    let req = Request::RecordQuery { q: q.to_string() };
    let Ok(json) = serde_json::to_vec(&req) else {
        return;
    };
    let total_len = (2 + json.len()) as u32;
    let mut buf = Vec::with_capacity(4 + 2 + json.len());
    buf.extend_from_slice(&total_len.to_be_bytes());
    buf.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    buf.extend_from_slice(&json);

    if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&sock) {
        let _ = stream.write_all(&buf);
    }
}

pub(crate) fn request_search_history(limit: u32) -> Vec<String> {
    let sock = socket_path();
    let req = Request::SearchHistory { limit };
    let Ok(json) = serde_json::to_vec(&req) else {
        return Vec::new();
    };
    let total_len = (2 + json.len()) as u32;
    let mut buf = Vec::with_capacity(4 + 2 + json.len());
    buf.extend_from_slice(&total_len.to_be_bytes());
    buf.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    buf.extend_from_slice(&json);

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
    let mut version = [0u8; 2];
    if stream.read_exact(&mut version).is_err() {
        return Vec::new();
    }
    let mut resp_buf = vec![0u8; resp_len - 2];
    if stream.read_exact(&mut resp_buf).is_err() {
        return Vec::new();
    }
    match serde_json::from_slice::<Response>(&resp_buf) {
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
    let Ok(json) = serde_json::to_vec(req) else {
        return;
    };
    let total_len = (2 + json.len()) as u32;
    let mut buf = Vec::with_capacity(4 + 2 + json.len());
    buf.extend_from_slice(&total_len.to_be_bytes());
    buf.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    buf.extend_from_slice(&json);
    if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&sock) {
        let _ = stream.write_all(&buf);
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
    let sock = socket_path();
    let req = Request::Preview {
        hit: Box::new(hit.clone()),
        monitor: monitor.clone(),
    };
    let Ok(json) = serde_json::to_vec(&req) else {
        return;
    };
    let total_len = (2 + json.len()) as u32;
    let mut buf = Vec::with_capacity(4 + 2 + json.len());
    buf.extend_from_slice(&total_len.to_be_bytes());
    buf.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    buf.extend_from_slice(&json);

    tracing::info!(
        "gui: send_preview_request hit_id={} monitor={:?}",
        hit.id.0,
        monitor
    );
    if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&sock) {
        let _ = stream.write_all(&buf);
    }
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
    fn v3_response_parses_with_top_hit() {
        let resp = Response::HitsWithExtrasV3 {
            hits: Vec::new(),
            calculation: None,
            top_hit: Some(lixun_core::DocId("app:firefox".into())),
        };
        let bytes = serde_json::to_vec(&resp).unwrap();
        let roundtrip: Response = serde_json::from_slice(&bytes).unwrap();
        match roundtrip {
            Response::HitsWithExtrasV3 { top_hit, .. } => {
                assert_eq!(top_hit.as_ref().map(|d| d.0.as_str()), Some("app:firefox"));
            }
            other => panic!("expected HitsWithExtrasV3, got {:?}", other),
        }
    }
}
