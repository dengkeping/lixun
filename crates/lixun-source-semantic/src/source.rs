use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use lixun_mutation::{AnnHandle, MutationBroadcaster};
use lixun_sources::{IndexerSource, MutationSink, SourceContext};

use crate::ann::LanceDbAnnHandle;
use crate::broadcaster::SemanticBroadcasterAdapter;
use crate::config::SemanticConfig;

pub struct SemanticSource {
    pub(crate) config: SemanticConfig,
    pub(crate) state_dir: PathBuf,
}

impl SemanticSource {
    pub fn new(config: SemanticConfig, state_dir: PathBuf) -> Self {
        Self { config, state_dir }
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
        Some(Arc::new(SemanticBroadcasterAdapter))
    }

    fn ann_handle(&self) -> Option<Arc<dyn AnnHandle>> {
        Some(Arc::new(LanceDbAnnHandle::new()))
    }
}
