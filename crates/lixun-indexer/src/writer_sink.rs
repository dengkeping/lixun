use anyhow::Result;
use lixun_sources::source::{Mutation as SrcMutation, MutationSink};
use tokio::runtime::Handle;

use crate::index_service::{IndexMutationTx, Mutation as IndexMutation};

pub struct WriterSink {
    tx: IndexMutationTx,
    runtime: Handle,
}

impl WriterSink {
    pub fn new(tx: IndexMutationTx) -> Self {
        Self {
            tx,
            runtime: Handle::current(),
        }
    }

    /// Send an `UpsertBody` mutation. The writer task fetches the
    /// doc by `doc_id`, overwrites its `body`, and writes it back.
    /// No-ops (logged at debug) if the doc is already gone.
    ///
    /// Async because callers may already be on a tokio worker
    /// thread (the OCR tick is): nesting `block_on` there panics
    /// with "Cannot start a runtime from within a runtime".
    pub async fn upsert_body(&self, doc_id: &str, body: &str) -> Result<()> {
        self.tx
            .send(IndexMutation::UpsertBody {
                doc_id: doc_id.to_string(),
                body: body.to_string(),
            })
            .await?;
        Ok(())
    }
}

impl MutationSink for WriterSink {
    /// Called from blocking threads only (rayon extract batches,
    /// `spawn_blocking` closures). Nesting `block_on` here is safe
    /// because the caller is not itself driving async tasks.
    fn emit(&self, mutation: SrcMutation) -> Result<()> {
        let tx = self.tx.clone();
        self.runtime.block_on(async move {
            match mutation {
                SrcMutation::Upsert(boxed) => {
                    tx.send(IndexMutation::UpsertMany(vec![*boxed])).await?;
                }
                SrcMutation::UpsertMany(docs) => {
                    if !docs.is_empty() {
                        tx.send(IndexMutation::UpsertMany(docs)).await?;
                    }
                }
                SrcMutation::Delete { doc_id } => {
                    tx.send(IndexMutation::DeleteMany(vec![doc_id])).await?;
                }
                SrcMutation::DeleteSourceInstance { instance_id } => {
                    tx.send(IndexMutation::DeleteSourceInstance { instance_id })
                        .await?;
                }
            }
            Ok::<_, anyhow::Error>(())
        })?;
        Ok(())
    }
}
