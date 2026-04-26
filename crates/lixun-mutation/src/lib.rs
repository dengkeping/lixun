#![allow(dead_code)]

use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// One document upserted in a single committed batch. The fields are
/// the minimum a downstream consumer (e.g. a vector-store backfill
/// worker) needs to decide whether to re-embed the doc and to attach
/// the resulting vector to the right primary key.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UpsertedDoc {
    pub doc_id: String,
    pub source_instance: String,
    pub mtime: i64,
    pub mime: Option<String>,
    pub body: Option<String>,
}

/// All mutations that landed in a single committed generation.
/// `generation` is the writer-service generation counter value
/// AFTER the commit succeeded; consumers can use it as a watermark
/// for resumable backfill journals.
#[derive(Clone, Debug, Default)]
pub struct MutationBatch {
    pub upserts: Vec<UpsertedDoc>,
    pub deletes: Vec<String>,
    pub generation: u64,
}

impl MutationBatch {
    pub fn is_empty(&self) -> bool {
        self.upserts.is_empty() && self.deletes.is_empty()
    }
}

/// Implemented by sinks that want to react to committed index
/// mutations. The writer service invokes `broadcast` from a
/// `tokio::task::spawn_blocking` after every successful commit, so
/// implementations may do synchronous IO (sqlite writes, embedding
/// queue pushes) without stalling the writer task.
pub trait MutationBroadcaster: Send + Sync {
    fn broadcast(&self, batch: &MutationBatch);
}

/// Default broadcaster used when no consumer is wired in.
pub struct NoopBroadcaster;

impl MutationBroadcaster for NoopBroadcaster {
    fn broadcast(&self, _batch: &MutationBatch) {}
}

/// Fan-out broadcaster. A panic in one inner broadcaster does not
/// prevent the others from running.
pub struct MultiBroadcaster {
    inner: Vec<Arc<dyn MutationBroadcaster>>,
}

impl MultiBroadcaster {
    pub fn new(inner: Vec<Arc<dyn MutationBroadcaster>>) -> Self {
        Self { inner }
    }

    pub fn push(&mut self, b: Arc<dyn MutationBroadcaster>) {
        self.inner.push(b);
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

impl MutationBroadcaster for MultiBroadcaster {
    fn broadcast(&self, batch: &MutationBatch) {
        for b in &self.inner {
            let b = Arc::clone(b);
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                b.broadcast(batch);
            }));
        }
    }
}

/// Async query interface implemented by ANN-providing plugins.
/// Lives in this leaf crate so `lixun-fusion` (the RRF consumer)
/// and `lixun-source-semantic` (the producer) can share the type
/// without either depending on the other (DB-3, AGENTS.md §1).
#[async_trait::async_trait]
pub trait AnnHandle: Send + Sync {
    async fn search_text(&self, query: &str, k: usize) -> anyhow::Result<Vec<AnnHit>>;
    async fn search_image(&self, query: &str, k: usize) -> anyhow::Result<Vec<AnnHit>>;
}

/// One result from an approximate-nearest-neighbour query. Lives in
/// the plugin-agnostic leaf crate so `lixun-fusion` (RRF consumer)
/// and `lixun-source-semantic` (ANN producer) share the type without
/// either depending on the other (DB-3).
#[derive(Clone, Debug)]
pub struct AnnHit {
    pub doc_id: String,
    pub distance: f32,
}

/// Plugin-contributed CLI verb tree. The host (`lixun-cli`) queries
/// the daemon for a flattened `CliManifest` at startup and synthesizes
/// clap subcommands from it, so plugin verb names never appear as
/// literals in any host crate (AGENTS.md §1).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CliManifest {
    pub verbs: Vec<CliVerb>,
}

/// One verb in a [`CliManifest`]. `subverbs` may nest arbitrarily
/// (e.g. `lixun-cli foo bar baz`); leaf verbs use an empty
/// `subverbs` and declare their positional/named arguments via
/// `args`. Empty `args` means the verb takes none.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CliVerb {
    pub name: String,
    pub about: String,
    #[serde(default)]
    pub subverbs: Vec<CliVerb>,
    #[serde(default)]
    pub args: Vec<CliArg>,
}

/// One named argument of a [`CliVerb`]. Forwarded to the plugin via
/// `Request::PluginCommand.args` as a JSON object keyed by `name`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CliArg {
    pub name: String,
    pub required: bool,
    pub help: String,
}

/// Read-only view onto the lexical document store, exposed to plugins
/// that need to walk every indexed doc (e.g. semantic backfill) without
/// pulling in a hard dependency on `lixun-indexer`. The daemon installs
/// an implementation backed by `SearchHandle` after the writer service
/// boots; plugins that don't need it never observe the install.
#[async_trait::async_trait]
pub trait DocStore: Send + Sync {
    async fn all_doc_ids(&self) -> anyhow::Result<std::collections::HashSet<String>>;
    async fn hydrate_doc(
        &self,
        doc_id: &str,
    ) -> anyhow::Result<Option<(lixun_core::Hit, lixun_core::ScoreBreakdown)>>;
    async fn get_body(&self, doc_id: &str) -> anyhow::Result<Option<String>>;
}
