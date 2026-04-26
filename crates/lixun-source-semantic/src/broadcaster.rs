use lixun_mutation::{MutationBatch, MutationBroadcaster};

/// Bridges committed index mutations into the semantic plugin's
/// embedding pipeline. The skeleton lands a tracing-only stub; the
/// embedder worker, batching, and LanceDB writes arrive in WD-T6.
pub struct SemanticBroadcasterAdapter;

impl MutationBroadcaster for SemanticBroadcasterAdapter {
    fn broadcast(&self, batch: &MutationBatch) {
        tracing::trace!(
            "SemanticBroadcasterAdapter::broadcast: {} upserts, {} deletes, gen={}",
            batch.upserts.len(),
            batch.deletes.len(),
            batch.generation
        );
    }
}
