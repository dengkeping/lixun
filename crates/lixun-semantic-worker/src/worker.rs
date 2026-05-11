use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use lixun_mutation::UpsertedDoc;
use tokio::sync::mpsc;

use crate::config::SemanticConfig;
use crate::embedder::{ClipTextEmbedder, ImageEmbedder, TextEmbedder};
use crate::journal::BackfillJournal;
use crate::store::VectorStore;

pub const CHANNEL_TEXT: &str = "text";
pub const CHANNEL_IMAGE: &str = "image";

const QUEUE_CAPACITY: usize = 4096;

#[derive(Debug)]
pub enum EmbedJob {
    Upsert(UpsertedDoc),
    Delete(String),
}

pub struct WorkerHandle {
    tx: mpsc::Sender<EmbedJob>,
    _join: std::thread::JoinHandle<()>,
}

impl WorkerHandle {
    pub fn sender(&self) -> mpsc::Sender<EmbedJob> {
        self.tx.clone()
    }
}

/// Spawns the synchronous embedder thread.
///
/// Embedders are loaded by the caller and shared via `Arc<Mutex<…>>`
/// so the query-side ANN path (`LanceDbAnnHandle::search_text`) can
/// reach the same `TextEmbedding` session without instantiating a
/// second one. fastembed sessions own ONNX `Session`s that must
/// only be invoked serially per session, which the `Mutex`
/// guarantees. The worker thread is `std::thread`, not a tokio
/// task, because the ONNX session pins itself to a CPU thread that
/// must not migrate across tokio's worker pool.
pub fn spawn_worker(
    cfg: SemanticConfig,
    batch_size: usize,
    store: Arc<VectorStore>,
    journal: Arc<Mutex<BackfillJournal>>,
    runtime: tokio::runtime::Handle,
    text_embedder: Arc<Mutex<TextEmbedder>>,
    image_embedder: Arc<Mutex<ImageEmbedder>>,
    clip_text_embedder: Arc<Mutex<ClipTextEmbedder>>,
) -> Result<WorkerHandle> {
    let (tx, rx) = mpsc::channel::<EmbedJob>(QUEUE_CAPACITY);

    let worker = WorkerThread {
        cfg,
        batch_size: batch_size.max(1),
        store,
        journal,
        runtime: runtime.clone(),
        text: text_embedder,
        image: image_embedder,
        clip_text: clip_text_embedder,
        rx,
        pending_text: Vec::new(),
        pending_images: Vec::new(),
        pending_deletes: Vec::new(),
        last_flush: Instant::now(),
        flush_count: 0,
    };

    let join = std::thread::Builder::new()
        .name("lixun-semantic-embed".into())
        .spawn(move || worker.run())
        .context("spawning semantic embed thread")?;

    Ok(WorkerHandle { tx, _join: join })
}

struct WorkerThread {
    cfg: SemanticConfig,
    batch_size: usize,
    store: Arc<VectorStore>,
    journal: Arc<Mutex<BackfillJournal>>,
    runtime: tokio::runtime::Handle,
    text: Arc<Mutex<TextEmbedder>>,
    image: Arc<Mutex<ImageEmbedder>>,
    clip_text: Arc<Mutex<ClipTextEmbedder>>,
    rx: mpsc::Receiver<EmbedJob>,
    pending_text: Vec<UpsertedDoc>,
    pending_images: Vec<UpsertedDoc>,
    pending_deletes: Vec<String>,
    last_flush: Instant,
    flush_count: u64,
}

enum Channel {
    Text,
    Image,
    Skip,
}

impl WorkerThread {
    fn run(mut self) {
        let flush_period = Duration::from_millis(self.cfg.flush_ms);
        let batch_size = self.batch_size;

        loop {
            let elapsed = self.last_flush.elapsed();
            let remaining = flush_period.saturating_sub(elapsed);

            let recv_result = self.runtime.block_on(async {
                if remaining.is_zero() {
                    Ok(self.rx.recv().await)
                } else {
                    match tokio::time::timeout(remaining, self.rx.recv()).await {
                        Ok(msg) => Ok(msg),
                        Err(_) => Err(()),
                    }
                }
            });

            match recv_result {
                Ok(Some(job)) => {
                    if let Err(e) = self.handle_job(job) {
                        tracing::warn!("semantic embed worker: handle_job failed: {e:#}");
                    }
                    if self.pending_text.len() >= batch_size
                        || self.pending_images.len() >= batch_size
                        || self.pending_deletes.len() >= batch_size
                    {
                        self.flush_text();
                        self.flush_images();
                        self.flush_deletes();
                        self.maybe_compact();
                    }
                }
                Ok(None) => {
                    self.flush_text();
                    self.flush_images();
                    self.flush_deletes();
                    self.maybe_compact();
                    tracing::info!("semantic embed worker: channel closed, exiting");
                    return;
                }
                Err(()) => {
                    self.flush_text();
                    self.flush_images();
                    self.flush_deletes();
                    self.maybe_compact();
                }
            }
        }
    }

    fn handle_job(&mut self, job: EmbedJob) -> Result<()> {
        match job {
            EmbedJob::Upsert(doc) => match classify(&doc) {
                Channel::Text => {
                    self.pending_text.push(doc);
                    Ok(())
                }
                Channel::Image => {
                    self.pending_images.push(doc);
                    Ok(())
                }
                Channel::Skip => {
                    tracing::trace!(
                        doc_id = %doc.doc_id,
                        mime = ?doc.mime,
                        body_present = doc.body.is_some(),
                        "semantic embed worker: doc skipped (no embeddable signal)"
                    );
                    Ok(())
                }
            },
            EmbedJob::Delete(doc_id) => {
                self.pending_deletes.push(doc_id);
                Ok(())
            }
        }
    }

    fn maybe_compact(&mut self) {
        self.flush_count = self.flush_count.wrapping_add(1);
        if self.flush_count % 50 != 0 {
            return;
        }
        let store = self.store.clone();
        let res = self.runtime.block_on(async move {
            store.compact_if_stale(32).await
        });
        match res {
            Ok(true) => tracing::info!(
                "semantic embed worker: compaction ran (flush_count={})",
                self.flush_count
            ),
            Ok(false) => tracing::debug!(
                "semantic embed worker: compaction skipped, fragments below threshold"
            ),
            Err(e) => tracing::warn!(
                "semantic embed worker: compaction failed: {e:#}"
            ),
        }
    }

    fn flush_deletes(&mut self) {
        if self.pending_deletes.is_empty() {
            return;
        }
        /* Dedup at flush time as a second line of defence; the
        broadcaster also dedupes per-batch but a worker can see
        repeats across flush windows. One LanceDB delete per unique
        id is far cheaper than one per event. */
        let mut ids: Vec<String> = std::mem::take(&mut self.pending_deletes);
        ids.sort_unstable();
        ids.dedup();
        let store = self.store.clone();
        let ids_for_delete = ids.clone();
        let res = self
            .runtime
            .block_on(async move { store.delete(&ids_for_delete).await });
        if let Err(e) = res {
            tracing::warn!(
                count = ids.len(),
                "semantic embed worker: lancedb delete batch failed: {e:#}"
            );
            return;
        }
        if let Ok(mut j) = self.journal.lock() {
            for id in &ids {
                let _ = j.forget(id);
            }
        }
    }

    fn flush_text(&mut self) {
        self.last_flush = Instant::now();
        if self.pending_text.is_empty() {
            return;
        }
        let batch = std::mem::take(&mut self.pending_text);

        let texts: Vec<String> = batch
            .iter()
            .map(|d| compose_text_input(&d.doc_id, d.body.as_deref()))
            .collect();

        let vectors = match self.text.lock() {
            Ok(mut t) => match t.embed(texts) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        "semantic embed worker: text embed batch of {} failed: {e:#}",
                        batch.len()
                    );
                    return;
                }
            },
            Err(e) => {
                tracing::error!("semantic embed worker: text embedder mutex poisoned: {e}");
                return;
            }
        };

        if vectors.len() != batch.len() {
            tracing::warn!(
                "semantic embed worker: text embed returned {} vectors for {} docs; dropping batch",
                vectors.len(),
                batch.len()
            );
            return;
        }

        let now = unix_seconds();
        let rows: Vec<crate::store::VectorRow> = batch
            .iter()
            .zip(vectors.iter())
            .map(|(doc, vector)| crate::store::VectorRow {
                doc_id: doc.doc_id.clone(),
                source_instance: doc.source_instance.clone(),
                mtime: doc.mtime,
                vector: vector.clone(),
            })
            .collect();

        let store = self.store.clone();
        let upsert_res = self
            .runtime
            .block_on(async move { store.upsert_text_batch(&rows).await });
        match upsert_res {
            Ok(()) => {
                if let Ok(mut j) = self.journal.lock() {
                    for doc in &batch {
                        if let Err(e) = j.record(&doc.doc_id, CHANNEL_TEXT, now) {
                            tracing::warn!(
                                doc_id = %doc.doc_id,
                                "semantic embed worker: journal record failed: {e:#}"
                            );
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    batch_len = batch.len(),
                    "semantic embed worker: lancedb upsert_text_batch failed: {e:#}"
                );
            }
        }
    }

    fn flush_images(&mut self) {
        if self.pending_images.is_empty() {
            return;
        }
        let batch = std::mem::take(&mut self.pending_images);

        let original_paths: Vec<std::path::PathBuf> = batch
            .iter()
            .filter_map(|d| {
                d.doc_id
                    .strip_prefix("fs:")
                    .map(|p| std::path::PathBuf::from(p))
            })
            .collect();

        if original_paths.is_empty() {
            tracing::warn!(
                "semantic embed worker: image batch of {} had no valid fs: paths; dropping",
                batch.len()
            );
            return;
        }

        let original_count = original_paths.len();

        #[cfg(feature = "image-decode")]
        let (paths_for_embed, _temp_files) = match lixun_image_decode::prepare_batch(&original_paths) {
            Ok(result) => result,
            Err(e) => {
                tracing::warn!("image pre-decode failed, skipping batch: {:#}", e);
                return;
            }
        };

        #[cfg(not(feature = "image-decode"))]
        let paths_for_embed = original_paths;

        #[cfg(feature = "image-decode")]
        if paths_for_embed.len() < original_count {
            tracing::debug!(
                "image batch: {} files after pre-decode ({} skipped)",
                paths_for_embed.len(),
                original_count - paths_for_embed.len()
            );
        }

        if paths_for_embed.is_empty() {
            tracing::warn!("image batch: all {} files skipped after pre-decode", original_count);
            return;
        }

        let vectors = match self.image.lock() {
            Ok(mut img) => match img.embed(paths_for_embed) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        "semantic embed worker: image embed batch of {} failed: {e:#}",
                        batch.len()
                    );
                    return;
                }
            },
            Err(e) => {
                tracing::error!("semantic embed worker: image embedder mutex poisoned: {e}");
                return;
            }
        };

        if vectors.len() != batch.len() {
            tracing::warn!(
                "semantic embed worker: image embed returned {} vectors for {} docs; dropping batch",
                vectors.len(),
                batch.len()
            );
            return;
        }

        let now = unix_seconds();
        let rows: Vec<crate::store::VectorRow> = batch
            .iter()
            .zip(vectors.iter())
            .map(|(doc, vector)| crate::store::VectorRow {
                doc_id: doc.doc_id.clone(),
                source_instance: doc.source_instance.clone(),
                mtime: doc.mtime,
                vector: vector.clone(),
            })
            .collect();

        let store = self.store.clone();
        let upsert_res = self
            .runtime
            .block_on(async move { store.upsert_image_batch(&rows).await });
        match upsert_res {
            Ok(()) => {
                if let Ok(mut j) = self.journal.lock() {
                    for doc in &batch {
                        if let Err(e) = j.record(&doc.doc_id, CHANNEL_IMAGE, now) {
                            tracing::warn!(
                                doc_id = %doc.doc_id,
                                "semantic embed worker: journal record failed: {e:#}"
                            );
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    batch_len = batch.len(),
                    "semantic embed worker: lancedb upsert_image_batch failed: {e:#}"
                );
            }
        }
    }
}

fn classify(doc: &UpsertedDoc) -> Channel {
    if doc.mime.as_deref().is_some_and(|m| m.starts_with("image/")) {
        tracing::debug!(
            "classify: doc_id={} mime={:?} -> Channel::Image",
            doc.doc_id,
            doc.mime
        );
        return Channel::Image;
    }
    match doc.body.as_deref() {
        Some(body) if !body.trim().is_empty() => {
            tracing::debug!(
                "classify: doc_id={} mime={:?} body_len={} -> Channel::Text",
                doc.doc_id,
                doc.mime,
                body.len()
            );
            Channel::Text
        }
        _ => {
            tracing::debug!(
                "classify: doc_id={} mime={:?} body=None -> Channel::Skip",
                doc.doc_id,
                doc.mime
            );
            Channel::Skip
        }
    }
}

fn compose_text_input(_doc_id: &str, body: Option<&str>) -> String {
    body.unwrap_or("").to_string()
}

fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Drives a one-shot backfill pass. Walks every doc id known to the
/// lexical index, skips ids whose channel is already recorded in the
/// journal, hydrates each remaining doc, and forwards an
/// [`EmbedJob::Upsert`] into the worker's channel using awaited
/// `send` so backpressure flows naturally.
pub async fn start_backfill(
    search: Arc<dyn lixun_mutation::DocStore>,
    journal: Arc<Mutex<BackfillJournal>>,
    embedder_tx: mpsc::Sender<EmbedJob>,
) -> Result<(u64, u64)> {
    let all_ids = search
        .all_doc_ids()
        .await
        .context("backfill: enumerating doc ids")?;

    let mut total = 0u64;
    let mut submitted = 0u64;
    for doc_id in all_ids {
        total += 1;
        let already_text = match journal.lock() {
            Ok(j) => j.was_embedded(&doc_id, CHANNEL_TEXT).unwrap_or(false),
            Err(_) => false,
        };
        let already_image = match journal.lock() {
            Ok(j) => j.was_embedded(&doc_id, CHANNEL_IMAGE).unwrap_or(false),
            Err(_) => false,
        };
        if already_text && already_image {
            continue;
        }

        let hydrated = search
            .hydrate_doc(&doc_id)
            .await
            .with_context(|| format!("backfill: hydrate {doc_id}"))?;
        let Some((hit, _bd)) = hydrated else {
            continue;
        };

        let body = match hit.body.as_deref() {
            Some(b) if !b.trim().is_empty() => Some(b.to_string()),
            _ => search
                .get_body(&doc_id)
                .await
                .with_context(|| format!("backfill: get_body {doc_id}"))?,
        };

        let doc = UpsertedDoc {
            doc_id: doc_id.clone(),
            source_instance: hit.source_instance.clone(),
            mtime: 0,
            mime: hit.mime.clone(),
            body,
        };

        if embedder_tx.send(EmbedJob::Upsert(doc)).await.is_err() {
            anyhow::bail!("backfill: embedder channel closed");
        }
        submitted += 1;
    }

    if let Ok(mut j) = journal.lock() {
        let now = unix_seconds().to_string();
        let _ = j.meta_set("last_backfill_completed_at", &now);
        let _ = j.meta_set("last_backfill_total", &total.to_string());
        let _ = j.meta_set("last_backfill_submitted", &submitted.to_string());
    }

    tracing::info!(total, submitted, "semantic backfill: enumeration complete");
    Ok((submitted, total))
}
