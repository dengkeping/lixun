//! IPC client: push-based event delivery via async_channel.
//!
//! Architecture change from polling: IPC thread sends typed IpcMessage events
//! through async_channel, GUI receives via glib::spawn_future_local. Each
//! message carries epoch for stale-detection. Final-only batching: Initial
//! chunks buffered, only Final triggers GTK model update (single rebuild per
//! query vs previous double rebuild).

use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use lixun_core::{Calculation, DocId, Hit};
use lixun_ipc::{PROTOCOL_VERSION, Phase, Request, Response, socket_path};

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

pub(crate) fn start_ipc_thread(
    session_epoch: Arc<AtomicU64>,
) -> (IpcClient, async_channel::Receiver<IpcMessage>) {
    let (tx, rx) = mpsc::channel::<(String, u32, u64)>();
    let (event_tx, event_rx) = async_channel::unbounded::<IpcMessage>();

    std::thread::spawn(move || {
        while let Ok((query, limit, epoch_at_send)) = rx.recv() {
            tracing::debug!("ipc: received search request query={:?} limit={} epoch={}", query, limit, epoch_at_send);

            let sock = socket_path();
            let req = Request::Search {
                q: query,
                limit,
                explain: false,
                epoch: epoch_at_send,
            };
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
            tracing::debug!("ipc: request written ({} bytes), entering read loop", buf.len());

            if let Err(e) = stream.set_read_timeout(Some(Duration::from_secs(3))) {
                tracing::error!("Failed to set read timeout: {}", e);
                continue;
            }

            loop {
                if epoch_at_send != session_epoch.load(Ordering::SeqCst) {
                    tracing::debug!(
                        "ipc: dropping reply from stale session (sent in epoch {})",
                        epoch_at_send
                    );
                    break;
                }

                let mut header = [0u8; 4];
                match stream.read_exact(&mut header) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {
                        tracing::debug!("ipc: read timeout, treating as Final");
                        let _ = event_tx.send_blocking(IpcMessage::SearchChunk {
                            epoch: epoch_at_send,
                            phase: Phase::Final,
                            hits: Vec::new(),
                            calculation: None,
                            top_hit: None,
                            claimed: false,
                        });
                        break;
                    }
                    Err(e) => {
                        tracing::error!("Failed to read response header: {}", e);
                        break;
                    }
                }
                let resp_len = u32::from_be_bytes(header) as usize;
                if resp_len < 2 {
                    tracing::error!("Response frame too short");
                    break;
                }
                let mut _version = [0u8; 2];
                if let Err(e) = stream.read_exact(&mut _version) {
                    tracing::error!("Failed to read response version: {}", e);
                    break;
                }
                let mut resp_buf = vec![0u8; resp_len - 2];
                if let Err(e) = stream.read_exact(&mut resp_buf) {
                    tracing::error!("Failed to read response body: {}", e);
                    break;
                }

                match serde_json::from_slice::<Response>(&resp_buf) {
                    Ok(Response::SearchChunk {
                        epoch: resp_epoch,
                        phase,
                        hits,
                        calculation,
                        top_hit,
                        explanations: _,
                        claimed,
                    }) => {
                        if resp_epoch != epoch_at_send {
                            tracing::debug!(
                                "ipc: dropping chunk with mismatched epoch (got {}, expected {})",
                                resp_epoch,
                                epoch_at_send
                            );
                            break;
                        }

                        if epoch_at_send != session_epoch.load(Ordering::SeqCst) {
                            tracing::debug!(
                                "ipc: session epoch changed after chunk read, dropping commit (sent in epoch {})",
                                epoch_at_send
                            );
                            break;
                        }

                        let is_final = matches!(phase, Phase::Final);
                        tracing::debug!("ipc: chunk received epoch={} phase={:?} hits={}", resp_epoch, phase, hits.len());
                        let _ = event_tx.send_blocking(IpcMessage::SearchChunk {
                            epoch: resp_epoch,
                            phase,
                            hits,
                            calculation,
                            top_hit,
                            claimed,
                        });

                        if is_final {
                            break;
                        }
                    }
                    Ok(other) => {
                        tracing::warn!("ipc: unexpected response variant: {:?}", other);
                        break;
                    }
                    Err(e) => {
                        tracing::error!("Failed to deserialize response: {}", e);
                        break;
                    }
                }
            }
        }
    });

    (IpcClient { request_tx: tx }, event_rx)
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

pub(crate) fn fetch_claimed_prefixes() -> Vec<String> {
    use std::io::Read;
    let sock = socket_path();
    let req = Request::ClaimedPrefixes;
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
    let mut version = [0u8; 2];
    if stream.read_exact(&mut version).is_err() {
        return Vec::new();
    }
    let mut body = vec![0u8; resp_len - 2];
    if stream.read_exact(&mut body).is_err() {
        return Vec::new();
    }
    match serde_json::from_slice::<Response>(&body) {
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

pub(crate) fn send_preview_hide_request() {
    let sock = socket_path();
    let req = Request::PreviewHide;
    let Ok(json) = serde_json::to_vec(&req) else {
        return;
    };
    let total_len = (2 + json.len()) as u32;
    let mut buf = Vec::with_capacity(4 + 2 + json.len());
    buf.extend_from_slice(&total_len.to_be_bytes());
    buf.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    buf.extend_from_slice(&json);

    tracing::info!("gui: send_preview_hide_request");
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
    fn v4_search_chunk_roundtrip() {
        let resp = Response::SearchChunk {
            epoch: 42,
            phase: Phase::Initial,
            hits: Vec::new(),
            calculation: None,
            top_hit: Some(lixun_core::DocId("app:firefox".into())),
            explanations: vec![],
            claimed: false,
        };
        let bytes = serde_json::to_vec(&resp).unwrap();
        let roundtrip: Response = serde_json::from_slice(&bytes).unwrap();
        match roundtrip {
            Response::SearchChunk { epoch, phase, top_hit, .. } => {
                assert_eq!(epoch, 42);
                assert_eq!(phase, Phase::Initial);
                assert_eq!(top_hit.as_ref().map(|d| d.0.as_str()), Some("app:firefox"));
            }
            other => panic!("expected SearchChunk, got {:?}", other),
        }
    }
}
