//! Worker-side proxy implementing [`lixun_mutation::DocStore`] over
//! IPC callbacks. Each method allocates a `req_id`, sends the
//! corresponding `Msg::Call*` upstream, and awaits the matching
//! `Cmd::CallbackReply` from the daemon.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use lixun_core::{Hit, ScoreBreakdown};
use lixun_mutation::DocStore;
use lixun_semantic_proto::{CallbackResp, Msg};
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

const CALLBACK_TIMEOUT: Duration = Duration::from_secs(30);

pub struct IpcDocStore {
    out: mpsc::Sender<Msg>,
    next_req_id: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<CallbackResp>>>,
}

impl IpcDocStore {
    pub fn new(out: mpsc::Sender<Msg>) -> Self {
        Self {
            out,
            /* req_id 0 is reserved for non-correlated frames; start
            our callback counter at a value that cannot collide
            with daemon-issued search/backfill ids (those start at
            1 in SemanticConnection). We bump to 2^32 so the two
            id namespaces — daemon→worker and worker→daemon — are
            visibly disjoint in logs without needing a tag. */
            next_req_id: AtomicU64::new(1u64 << 32),
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// Route a `Cmd::CallbackReply` from the dispatch loop into the
    /// matching pending oneshot. Unknown req_ids are silently dropped
    /// (likely a late reply after our timeout fired).
    pub fn deliver(&self, req_id: u64, resp: CallbackResp) {
        let tx = self.pending.lock().ok().and_then(|mut g| g.remove(&req_id));
        if let Some(tx) = tx {
            let _ = tx.send(resp);
        } else {
            tracing::debug!(req_id, "ipc_doc_store: late or unknown CallbackReply");
        }
    }

    async fn call(&self, build: impl FnOnce(u64) -> Msg) -> Result<CallbackResp> {
        let req_id = self.next_req_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        if let Ok(mut g) = self.pending.lock() {
            g.insert(req_id, tx);
        }
        let msg = build(req_id);
        if self.out.send(msg).await.is_err() {
            if let Ok(mut g) = self.pending.lock() {
                g.remove(&req_id);
            }
            return Err(anyhow!("ipc_doc_store: writer channel closed"));
        }
        match timeout(CALLBACK_TIMEOUT, rx).await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(_)) => Err(anyhow!("ipc_doc_store: response channel dropped")),
            Err(_) => {
                if let Ok(mut g) = self.pending.lock() {
                    g.remove(&req_id);
                }
                Err(anyhow!(
                    "ipc_doc_store: callback timed out after {}s",
                    CALLBACK_TIMEOUT.as_secs()
                ))
            }
        }
    }
}

#[async_trait]
impl DocStore for IpcDocStore {
    async fn all_doc_ids(&self) -> Result<HashSet<String>> {
        let resp = self.call(|req_id| Msg::CallAllDocIds { req_id }).await?;
        match resp {
            CallbackResp::AllDocIds { ids } => Ok(ids.into_iter().collect()),
            CallbackResp::Error { code, detail } => {
                tracing::warn!(?code, %detail, "ipc_doc_store: all_doc_ids failed");
                Err(anyhow!("daemon callback error: {detail}"))
            }
            other => Err(anyhow!(
                "ipc_doc_store: wrong reply variant for all_doc_ids: {:?}",
                other
            )),
        }
    }

    async fn hydrate_doc(&self, doc_id: &str) -> Result<Option<(Hit, ScoreBreakdown)>> {
        let id = doc_id.to_string();
        let resp = self
            .call(move |req_id| Msg::CallHydrateDoc { req_id, doc_id: id })
            .await?;
        match resp {
            CallbackResp::HydrateDoc { hit } => {
                /* The wire payload drops ScoreBreakdown (see
                lixun_semantic_proto::CallbackResp::HydrateDoc
                doc-comment); the only consumer in start_backfill
                ignores it, so a default value is sufficient. */
                Ok(hit.map(|h| (h, ScoreBreakdown::default())))
            }
            CallbackResp::Error { code, detail } => {
                tracing::warn!(?code, %detail, doc_id = %doc_id, "ipc_doc_store: hydrate_doc failed");
                Ok(None)
            }
            other => Err(anyhow!(
                "ipc_doc_store: wrong reply variant for hydrate_doc: {:?}",
                other
            )),
        }
    }

    async fn get_body(&self, doc_id: &str) -> Result<Option<String>> {
        let id = doc_id.to_string();
        let resp = self
            .call(move |req_id| Msg::CallGetBody { req_id, doc_id: id })
            .await?;
        match resp {
            CallbackResp::GetBody { body } => Ok(body),
            CallbackResp::Error { code, detail } => {
                tracing::warn!(?code, %detail, doc_id = %doc_id, "ipc_doc_store: get_body failed");
                Ok(None)
            }
            other => Err(anyhow!(
                "ipc_doc_store: wrong reply variant for get_body: {:?}",
                other
            )),
        }
    }
}
