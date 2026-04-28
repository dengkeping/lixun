use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::Result;
use lixun_mutation::{AnnHit, DocStore};
use lixun_semantic_proto::{Cmd, ErrorCode};
use tokio::sync::{mpsc, oneshot};

pub type SearchReply = Result<Vec<AnnHit>, SemanticIpcError>;
pub type BackfillReply = Result<BackfillStats, SemanticIpcError>;

#[derive(Debug, Clone)]
pub struct BackfillStats {
    pub submitted: u64,
    pub total: u64,
}

#[derive(Debug, Clone)]
pub struct SemanticIpcError {
    pub code: ErrorCode,
    pub detail: String,
}

impl std::fmt::Display for SemanticIpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.detail)
    }
}

impl std::error::Error for SemanticIpcError {}

/// IPC handle to the semantic worker.
///
/// Built by the daemon's `semantic_supervisor` after a successful
/// handshake, then handed to this crate via [`install_connection`]
/// so the inventory-registered factory can pick it up at plugin
/// build time. Owns the writer mpsc and the request-correlation
/// maps; cloning is cheap (reference counted).
pub struct SemanticConnection {
    writer: mpsc::Sender<Cmd>,
    next_req_id: AtomicU64,
    pending_search: Mutex<HashMap<u64, oneshot::Sender<SearchReply>>>,
    pending_backfill: Mutex<HashMap<u64, oneshot::Sender<BackfillReply>>>,
}

impl SemanticConnection {
    pub fn new(writer: mpsc::Sender<Cmd>) -> Arc<Self> {
        Arc::new(Self {
            writer,
            /* req_id 0 is reserved for fire-and-forget commands by the
            wire protocol; start the counter at 1 so we never collide. */
            next_req_id: AtomicU64::new(1),
            pending_search: Mutex::new(HashMap::new()),
            pending_backfill: Mutex::new(HashMap::new()),
        })
    }

    pub fn alloc_req_id(&self) -> u64 {
        self.next_req_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn writer(&self) -> &mpsc::Sender<Cmd> {
        &self.writer
    }

    pub fn register_search(&self, req_id: u64, tx: oneshot::Sender<SearchReply>) {
        if let Ok(mut g) = self.pending_search.lock() {
            g.insert(req_id, tx);
        }
    }

    pub fn complete_search(&self, req_id: u64, reply: SearchReply) {
        let tx = self
            .pending_search
            .lock()
            .ok()
            .and_then(|mut g| g.remove(&req_id));
        if let Some(tx) = tx {
            let _ = tx.send(reply);
        }
    }

    pub fn register_backfill(&self, req_id: u64, tx: oneshot::Sender<BackfillReply>) {
        if let Ok(mut g) = self.pending_backfill.lock() {
            g.insert(req_id, tx);
        }
    }

    pub fn complete_backfill(&self, req_id: u64, reply: BackfillReply) {
        let tx = self
            .pending_backfill
            .lock()
            .ok()
            .and_then(|mut g| g.remove(&req_id));
        if let Some(tx) = tx {
            let _ = tx.send(reply);
        }
    }

    /// Drain every pending request, completing each with `err`. Used
    /// by the supervisor when the worker connection drops so callers
    /// stop waiting on a dead socket.
    pub fn fail_all_pending(&self, err: SemanticIpcError) {
        if let Ok(mut g) = self.pending_search.lock() {
            for (_, tx) in g.drain() {
                let _ = tx.send(Err(err.clone()));
            }
        }
        if let Ok(mut g) = self.pending_backfill.lock() {
            for (_, tx) in g.drain() {
                let _ = tx.send(Err(err.clone()));
            }
        }
    }
}

static GLOBAL_CONN: OnceLock<Arc<SemanticConnection>> = OnceLock::new();

/// Install the active semantic-worker connection. Called exactly
/// once per process by the daemon's supervisor after the handshake
/// completes. Subsequent calls are silently ignored — Phase 2's
/// supervisor reuses the same connection across worker restarts by
/// recycling the `SemanticConnection`'s internal channel.
pub fn install_connection(conn: Arc<SemanticConnection>) {
    let _ = GLOBAL_CONN.set(conn);
}

pub(crate) fn current_connection() -> Option<Arc<SemanticConnection>> {
    GLOBAL_CONN.get().cloned()
}

/// Returns whether [`install_connection`] has been called. Public
/// because the daemon's supervisor tests need to wait for the
/// handshake without polling the AnnHandle (which would race the
/// worker's first model load).
pub fn is_connected() -> bool {
    GLOBAL_CONN.get().is_some()
}

static GLOBAL_DOC_STORE: OnceLock<Arc<dyn DocStore>> = OnceLock::new();

/// Install the daemon's read-only `DocStore` so the supervisor's
/// worker→daemon callback handlers can serve the worker's backfill
/// loop. The daemon calls this once after `register.install_doc_store`
/// fans out to every plugin; the stub plugin's
/// `install_doc_store` override forwards into here.
pub fn install_doc_store(store: Arc<dyn DocStore>) {
    let _ = GLOBAL_DOC_STORE.set(store);
}

/// Returns the installed DocStore if any. Used by the daemon's
/// `semantic_supervisor` reader loop to serve `Msg::CallAllDocIds`,
/// `Msg::CallHydrateDoc`, and `Msg::CallGetBody`.
pub fn current_doc_store() -> Option<Arc<dyn DocStore>> {
    GLOBAL_DOC_STORE.get().cloned()
}
