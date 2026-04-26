use std::sync::atomic::{AtomicBool, Ordering};

use lixun_mutation::{MutationBatch, MutationBroadcaster};
use tokio::sync::mpsc;

use crate::worker::EmbedJob;

pub struct SemanticBroadcasterAdapter {
    tx: mpsc::Sender<EmbedJob>,
    closed_logged: AtomicBool,
}

impl SemanticBroadcasterAdapter {
    pub fn new(tx: mpsc::Sender<EmbedJob>) -> Self {
        Self {
            tx,
            closed_logged: AtomicBool::new(false),
        }
    }

    fn enqueue(&self, job: EmbedJob) {
        match self.tx.try_send(job) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!(
                    "semantic broadcaster: embed channel full; dropping job (next backfill recovers)"
                );
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                if !self.closed_logged.swap(true, Ordering::Relaxed) {
                    tracing::error!("semantic broadcaster: embed worker channel closed");
                }
            }
        }
    }
}

impl MutationBroadcaster for SemanticBroadcasterAdapter {
    fn broadcast(&self, batch: &MutationBatch) {
        tracing::trace!(
            upserts = batch.upserts.len(),
            deletes = batch.deletes.len(),
            generation = batch.generation,
            "semantic broadcaster: forwarding batch"
        );
        for doc in &batch.upserts {
            self.enqueue(EmbedJob::Upsert(doc.clone()));
        }
        for id in &batch.deletes {
            self.enqueue(EmbedJob::Delete(id.clone()));
        }
    }
}
