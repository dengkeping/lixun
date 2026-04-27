use std::sync::{Arc, OnceLock};

use anyhow::Result;
use lixun_mutation::{AnnHandle, CliManifest, CliVerb, DocStore, MutationBroadcaster};
use lixun_sources::{IndexerSource, MutationSink, SourceContext};
use tokio::sync::mpsc;

use crate::ann::LanceDbAnnHandle;
use crate::broadcaster::SemanticBroadcasterAdapter;
use crate::journal::BackfillJournal;
use crate::worker::{EmbedJob, WorkerHandle, start_backfill};

const VERB_TOP: &str = "semantic";
const VERB_BACKFILL: &str = "backfill";

pub struct SemanticSource {
    broadcaster: Arc<dyn MutationBroadcaster>,
    ann: Arc<LanceDbAnnHandle>,
    embedder_tx: mpsc::Sender<EmbedJob>,
    journal: Arc<std::sync::Mutex<BackfillJournal>>,
    doc_store: OnceLock<Arc<dyn DocStore>>,
    backfill_on_start: bool,
    runtime: tokio::runtime::Handle,
    _worker: WorkerHandle,
}

impl SemanticSource {
    pub fn new(
        worker: WorkerHandle,
        ann: Arc<LanceDbAnnHandle>,
        journal: Arc<std::sync::Mutex<BackfillJournal>>,
        backfill_on_start: bool,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        let embedder_tx = worker.sender();
        let broadcaster: Arc<dyn MutationBroadcaster> =
            Arc::new(SemanticBroadcasterAdapter::new(embedder_tx.clone()));
        Self {
            broadcaster,
            ann,
            embedder_tx,
            journal,
            doc_store: OnceLock::new(),
            backfill_on_start,
            runtime,
            _worker: worker,
        }
    }
}

impl IndexerSource for SemanticSource {
    fn kind(&self) -> &'static str {
        "semantic"
    }

    /// Semantic owns no canonical corpus; vectors are derived from
    /// other sources' committed mutations via the broadcaster path.
    fn reindex_full(&self, _ctx: &SourceContext, _sink: &dyn MutationSink) -> Result<()> {
        Ok(())
    }

    fn reindex_on_schema_wipe(&self) -> bool {
        false
    }

    fn broadcaster(&self) -> Option<Arc<dyn MutationBroadcaster>> {
        Some(self.broadcaster.clone())
    }

    fn ann_handle(&self) -> Option<Arc<dyn AnnHandle>> {
        Some(self.ann.clone())
    }

    fn install_doc_store(&self, store: Arc<dyn DocStore>) {
        // The host installs the lexical doc store exactly once,
        // after every plugin's worker is wired up. That is the
        // earliest moment a backfill can run, so the `backfill_on_start`
        // config flag is honoured here rather than in `PluginFactory::build`.
        if self.doc_store.set(store.clone()).is_err() {
            return;
        }
        if !self.backfill_on_start {
            return;
        }
        let journal = self.journal.clone();
        let embedder_tx = self.embedder_tx.clone();
        self.runtime.spawn(async move {
            if let Err(e) = start_backfill(store, journal, embedder_tx).await {
                tracing::warn!("semantic backfill_on_start: {e:#}");
            }
        });
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
            match verb_path
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .as_slice()
            {
                [VERB_TOP, VERB_BACKFILL] => {
                    let store = self.doc_store.get().ok_or_else(|| {
                        anyhow::anyhow!(
                            "semantic backfill: lexical document store not installed yet"
                        )
                    })?;
                    start_backfill(
                        Arc::clone(store),
                        self.journal.clone(),
                        self.embedder_tx.clone(),
                    )
                    .await?;
                    Ok(serde_json::json!({
                        "status": "ok",
                        "message": "backfill enumeration complete; embedding continues in the background"
                    }))
                }
                _ => Err(anyhow::anyhow!(
                    "semantic plugin: unknown verb {:?}",
                    verb_path
                )),
            }
        })
    }
}
