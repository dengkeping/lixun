use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use lixun_core::{Calculation, DocId, Hit};
use lixun_ipc::{Phase, Request, Response, socket_path, PROTOCOL_VERSION};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub(crate) enum IpcEvent {
    SearchChunk {
        epoch: u64,
        phase: Phase,
        hits: Vec<Hit>,
        calculation: Option<Calculation>,
        top_hit: Option<DocId>,
    },
}

#[derive(Clone)]
pub(crate) struct IpcClient {
    tx: mpsc::UnboundedSender<(String, u32, u64)>,
    session_epoch: std::sync::Arc<AtomicU64>,
}

impl IpcClient {
    pub(crate) fn new(event_tx: mpsc::UnboundedSender<IpcEvent>) -> Self {
        let (req_tx, mut req_rx) = mpsc::unbounded_channel::<(String, u32, u64)>();
        let session_epoch = std::sync::Arc::new(AtomicU64::new(0));
        let session_epoch_clone = std::sync::Arc::clone(&session_epoch);

        tokio::spawn(async move {
            while let Some((query, limit, epoch_at_send)) = req_rx.recv().await {
                let event_tx = event_tx.clone();
                let session_epoch = std::sync::Arc::clone(&session_epoch_clone);
                tokio::spawn(async move {
                    if let Err(e) = search_task(query, limit, epoch_at_send, event_tx, session_epoch).await {
                        tracing::error!("ipc: search task failed: {}", e);
                    }
                });
            }
        });

        Self {
            tx: req_tx,
            session_epoch,
        }
    }

    pub(crate) fn bump_session_epoch(&self) {
        self.session_epoch.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn search(&self, query: String, limit: u32) {
        let epoch_at_send = self.session_epoch.load(Ordering::SeqCst);
        tracing::debug!("ipc: search query={:?} limit={} epoch={}", query, limit, epoch_at_send);
        let _ = self.tx.send((query, limit, epoch_at_send));
    }
}

async fn search_task(
    query: String,
    limit: u32,
    epoch_at_send: u64,
    event_tx: mpsc::UnboundedSender<IpcEvent>,
    session_epoch: std::sync::Arc<AtomicU64>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let sock = socket_path();
    let req = Request::Search {
        q: query,
        limit,
        explain: false,
        epoch: epoch_at_send,
    };
    let json = serde_json::to_vec(&req)?;
    let total_len = (2 + json.len()) as u32;
    let mut buf = Vec::with_capacity(4 + 2 + json.len());
    buf.extend_from_slice(&total_len.to_be_bytes());
    buf.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    buf.extend_from_slice(&json);

    let mut stream = UnixStream::connect(&sock).await?;
    stream.write_all(&buf).await?;
    tracing::debug!("ipc: request written ({} bytes), entering read loop", buf.len());

    loop {
        if epoch_at_send != session_epoch.load(Ordering::SeqCst) {
            tracing::debug!("ipc: dropping reply from stale session (sent in epoch {})", epoch_at_send);
            break;
        }

        let mut header = [0u8; 4];
        match tokio::time::timeout(Duration::from_secs(3), stream.read_exact(&mut header)).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                tracing::error!("ipc: read error: {}", e);
                break;
            }
            Err(_) => {
                tracing::debug!("ipc: read timeout, treating as Final");
                break;
            }
        }

        let resp_len = u32::from_be_bytes(header) as usize;
        if resp_len < 2 {
            tracing::error!("ipc: invalid response length {}", resp_len);
            break;
        }

        let mut version = [0u8; 2];
        if stream.read_exact(&mut version).await.is_err() {
            tracing::error!("ipc: failed to read version");
            break;
        }

        let mut resp_buf = vec![0u8; resp_len - 2];
        stream.read_exact(&mut resp_buf).await?;

        let resp: Response = serde_json::from_slice(&resp_buf)?;
        match resp {
            Response::SearchChunk {
                epoch: resp_epoch,
                phase,
                hits,
                calculation,
                top_hit,
                explanations: _,
            } => {
                if resp_epoch != epoch_at_send {
                    tracing::debug!("ipc: dropping chunk with mismatched epoch {} (expected {})", resp_epoch, epoch_at_send);
                    continue;
                }

                if epoch_at_send != session_epoch.load(Ordering::SeqCst) {
                    tracing::debug!("ipc: session epoch changed after read, dropping chunk");
                    break;
                }

                tracing::debug!("ipc: received chunk phase={:?} hits={}", phase, hits.len());

                let is_final = matches!(phase, Phase::Final);

                let _ = event_tx.send(IpcEvent::SearchChunk {
                    epoch: epoch_at_send,
                    phase,
                    hits,
                    calculation,
                    top_hit,
                });

                if is_final {
                    break;
                }
            }
            other => {
                tracing::warn!("ipc: unexpected response type: {:?}", other);
            }
        }
    }

    Ok(())
}
