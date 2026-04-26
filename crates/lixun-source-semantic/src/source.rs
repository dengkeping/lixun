use std::sync::Arc;

use anyhow::Result;
use lixun_mutation::{AnnHandle, MutationBroadcaster};
use lixun_sources::{IndexerSource, MutationSink, SourceContext};

use crate::ann::LanceDbAnnHandle;
use crate::broadcaster::SemanticBroadcasterAdapter;
use crate::worker::WorkerHandle;

pub struct SemanticSource {
    broadcaster: Arc<dyn MutationBroadcaster>,
    ann: Arc<LanceDbAnnHandle>,
    _worker: WorkerHandle,
}

impl SemanticSource {
    pub fn new(
        worker: WorkerHandle,
        ann: Arc<LanceDbAnnHandle>,
    ) -> Self {
        let broadcaster: Arc<dyn MutationBroadcaster> =
            Arc::new(SemanticBroadcasterAdapter::new(worker.sender()));
        Self {
            broadcaster,
            ann,
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
}
