use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use lixun_mutation::{AnnHandle, AnnHit, CliManifest, CliVerb, Modality, MutationBatch, MutationBroadcaster};
use lixun_semantic_proto::Cmd;
use lixun_sources::{IndexerSource, MutationSink, SourceContext};
use tokio::time::timeout;

use crate::transport::{ClassifyReply, SearchReply, SemanticConnection, current_connection};

const VERB_TOP: &str = "semantic";
const VERB_BACKFILL: &str = "backfill";
const SEARCH_TIMEOUT: Duration = Duration::from_secs(5);
const BACKFILL_TIMEOUT: Duration = Duration::from_secs(60 * 30);

pub(crate) struct SemanticIpcSource;

impl SemanticIpcSource {
    pub fn new() -> Self {
        Self
    }
}

impl IndexerSource for SemanticIpcSource {
    fn kind(&self) -> &'static str {
        "semantic"
    }

    fn reindex_full(&self, _ctx: &SourceContext, _sink: &dyn MutationSink) -> Result<()> {
        Ok(())
    }

    fn reindex_on_schema_wipe(&self) -> bool {
        false
    }

    fn install_doc_store(&self, store: Arc<dyn lixun_mutation::DocStore>) {
        crate::transport::install_doc_store(store);
    }

    fn broadcaster(&self) -> Option<Arc<dyn MutationBroadcaster>> {
        Some(Arc::new(SemanticIpcBroadcaster))
    }

    fn ann_handle(&self) -> Option<Arc<dyn AnnHandle>> {
        Some(Arc::new(SemanticIpcAnnHandle))
    }

    fn cli_manifest(&self) -> Option<CliManifest> {
        Some(CliManifest {
            verbs: vec![CliVerb {
                name: VERB_TOP.to_string(),
                about: "Semantic embedding management.".to_string(),
                subverbs: vec![CliVerb {
                    name: VERB_BACKFILL.to_string(),
                    about: "Re-embed every document currently in the lexical index.".to_string(),
                    subverbs: Vec::new(),
                    args: Vec::new(),
                }],
                args: Vec::new(),
            }],
        })
    }

    fn cli_invoke<'a>(
        &'a self,
        verb_path: &'a [String],
        _args: &'a serde_json::Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<serde_json::Value>> + Send + 'a>>
    {
        Box::pin(async move {
            let parts: Vec<&str> = verb_path.iter().map(String::as_str).collect();
            match parts.as_slice() {
                [VERB_TOP, VERB_BACKFILL] => {
                    let conn = current_connection()
                        .ok_or_else(|| anyhow::anyhow!("semantic worker not connected"))?;
                    let req_id = conn.alloc_req_id();
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    conn.register_backfill(req_id, tx);
                    conn.writer()
                        .send(Cmd::BackfillStart { req_id })
                        .await
                        .map_err(|_| anyhow::anyhow!("semantic worker writer channel closed"))?;
                    let stats = match timeout(BACKFILL_TIMEOUT, rx).await {
                        Ok(Ok(Ok(stats))) => stats,
                        Ok(Ok(Err(e))) => return Err(anyhow::anyhow!("backfill: {e}")),
                        Ok(Err(_)) => {
                            return Err(anyhow::anyhow!("backfill response channel dropped"));
                        }
                        Err(_) => return Err(anyhow::anyhow!("backfill timed out")),
                    };
                    Ok(serde_json::json!({
                        "status": "ok",
                        "submitted": stats.submitted,
                        "total": stats.total,
                    }))
                }
                _ => Err(anyhow::anyhow!(
                    "semantic stub: unknown verb {:?}",
                    verb_path
                )),
            }
        })
    }
}

struct SemanticIpcBroadcaster;

impl MutationBroadcaster for SemanticIpcBroadcaster {
    fn broadcast(&self, batch: &MutationBatch) {
        if batch.is_empty() {
            return;
        }
        let Some(conn) = current_connection() else {
            return;
        };
        /* Sliced upserts: each Cmd::Embed gets its own JSON frame and
        the worker pushes each doc straight onto its embed channel,
        so smaller frames give better backpressure granularity than
        one giant batch per generation. 64 mirrors the in-process
        embedder's default batch size. */
        const CHUNK: usize = 64;
        for chunk in batch.upserts.chunks(CHUNK) {
            let cmd = Cmd::Embed {
                docs: chunk.to_vec(),
            };
            match conn.writer().try_send(cmd) {
                Ok(()) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Full(cmd)) => {
                    /* Channel full: spawn a task that awaits send so
                    the upsert is delivered with backpressure instead
                    of being dropped. The previous code logged WARN
                    and silently lost the batch. */
                    let writer = conn.writer().clone();
                    let len = match &cmd {
                        Cmd::Embed { docs } => docs.len(),
                        _ => 0,
                    };
                    tokio::spawn(async move {
                        if writer.send(cmd).await.is_err() {
                            tracing::warn!(
                                upserts = len,
                                "semantic stub: writer channel closed while awaiting Embed send"
                            );
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!(
                        upserts = chunk.len(),
                        "semantic stub: failed to forward Embed to worker: {e}"
                    );
                }
            }
        }
        // Dedup deletes before forwarding. LanceDB rotates many
        // internal files under `_transactions/` and `_versions/` and
        // an exclude regression upstream could replay the same
        // doc_id many times per second; deduping here caps fan-out at
        // one Cmd::Delete per unique id per batch. Backpressure is
        // handled the same way as Embed: try_send fast path, spawn
        // await on Full.
        let mut seen: std::collections::HashSet<&str> =
            std::collections::HashSet::with_capacity(batch.deletes.len());
        for doc_id in &batch.deletes {
            if !seen.insert(doc_id.as_str()) {
                continue;
            }
            let cmd = Cmd::Delete {
                doc_id: doc_id.clone(),
            };
            match conn.writer().try_send(cmd) {
                Ok(()) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Full(cmd)) => {
                    let writer = conn.writer().clone();
                    let id = match &cmd {
                        Cmd::Delete { doc_id } => doc_id.clone(),
                        _ => String::new(),
                    };
                    tokio::spawn(async move {
                        if writer.send(cmd).await.is_err() {
                            tracing::warn!(
                                doc_id = %id,
                                "semantic stub: writer channel closed while awaiting Delete send"
                            );
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!(
                        doc_id = %doc_id,
                        "semantic stub: failed to forward Delete to worker: {e}"
                    );
                }
            }
        }
    }
}

struct SemanticIpcAnnHandle;

impl SemanticIpcAnnHandle {
    async fn issue(&self, build: impl FnOnce(u64) -> Cmd) -> Result<Vec<AnnHit>> {
        let Some(conn) = current_connection() else {
            return Ok(Vec::new());
        };
        Self::issue_on(&conn, build).await
    }

    async fn issue_on(
        conn: &Arc<SemanticConnection>,
        build: impl FnOnce(u64) -> Cmd,
    ) -> Result<Vec<AnnHit>> {
        let req_id = conn.alloc_req_id();
        let (tx, rx) = tokio::sync::oneshot::channel::<SearchReply>();
        conn.register_search(req_id, tx);
        let cmd = build(req_id);
        if conn.writer().send(cmd).await.is_err() {
            conn.complete_search(
                req_id,
                Err(crate::transport::SemanticIpcError {
                    code: lixun_semantic_proto::ErrorCode::Internal,
                    detail: "writer channel closed".into(),
                }),
            );
            tracing::warn!("semantic stub: search dropped, writer closed");
            return Ok(Vec::new());
        }
        match timeout(SEARCH_TIMEOUT, rx).await {
            Ok(Ok(Ok(hits))) => Ok(hits),
            Ok(Ok(Err(e))) => {
                tracing::warn!("semantic stub: worker error on search: {e}");
                Ok(Vec::new())
            }
            Ok(Err(_)) => {
                tracing::warn!("semantic stub: response channel dropped");
                Ok(Vec::new())
            }
            Err(_) => {
                tracing::warn!(
                    timeout_ms = SEARCH_TIMEOUT.as_millis(),
                    "semantic stub: search timeout"
                );
                Ok(Vec::new())
            }
        }
    }
}

#[async_trait]
impl AnnHandle for SemanticIpcAnnHandle {
    async fn search_text(&self, query: &str, k: usize) -> Result<Vec<AnnHit>> {
        let q = query.to_string();
        let k_u32 = k.min(u32::MAX as usize) as u32;
        self.issue(|req_id| Cmd::SearchText {
            req_id,
            query: q,
            k: k_u32,
        })
        .await
    }

    async fn search_image(&self, query: &str, k: usize) -> Result<Vec<AnnHit>> {
        let q = query.to_string();
        let k_u32 = k.min(u32::MAX as usize) as u32;
        self.issue(|req_id| Cmd::SearchImage {
            req_id,
            query: q,
            k: k_u32,
        })
        .await
    }

    async fn classify_query(&self, query: &str) -> Result<Modality> {
        let Some(conn) = current_connection() else {
            return Ok(Modality::Text);
        };
        let req_id = conn.alloc_req_id();
        let (tx, rx) = tokio::sync::oneshot::channel::<ClassifyReply>();
        conn.register_classify(req_id, tx);
        let cmd = Cmd::ClassifyQuery {
            req_id,
            query: query.to_string(),
        };
        if conn.writer().send(cmd).await.is_err() {
            conn.complete_classify(
                req_id,
                Err(crate::transport::SemanticIpcError {
                    code: lixun_semantic_proto::ErrorCode::Internal,
                    detail: "writer channel closed".into(),
                }),
            );
            tracing::warn!("semantic stub: classify dropped, writer closed");
            return Ok(Modality::Text);
        }
        match timeout(SEARCH_TIMEOUT, rx).await {
            Ok(Ok(Ok(modality))) => Ok(modality),
            Ok(Ok(Err(e))) => {
                tracing::warn!("semantic stub: worker error on classify: {e}");
                Ok(Modality::Text)
            }
            Ok(Err(_)) => {
                tracing::warn!("semantic stub: classify response channel dropped");
                Ok(Modality::Text)
            }
            Err(_) => {
                tracing::warn!(
                    timeout_ms = SEARCH_TIMEOUT.as_millis(),
                    "semantic stub: classify timeout"
                );
                Ok(Modality::Text)
            }
        }
    }
}
