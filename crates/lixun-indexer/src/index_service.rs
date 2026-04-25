//! Index service: single long-lived IndexWriter behind a mutation channel.
//!
//! All writers (reconciler, watcher, source_watcher, user reindex) send
//! `Mutation`s via `IndexMutationTx`. One writer task owns the sole
//! `tantivy::IndexWriter` and commits on a timer. Search path uses
//! `SearchHandle` and never waits on the writer.

use anyhow::Result;
use lixun_core::Document;
use lixun_index::{LixunIndex, TantivyDoc, TantivyIndexWriter};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, mpsc, oneshot};

static COMMITS: AtomicU64 = AtomicU64::new(0);
static LAST_COMMIT_LATENCY_MS: AtomicU64 = AtomicU64::new(0);
static GENERATION: AtomicU64 = AtomicU64::new(0);

pub fn stats() -> (u64, u64, u64) {
    (
        COMMITS.load(Ordering::Relaxed),
        LAST_COMMIT_LATENCY_MS.load(Ordering::Relaxed),
        GENERATION.load(Ordering::Relaxed),
    )
}

const COMMIT_MIN_INTERVAL: Duration = Duration::from_secs(3);
const COMMIT_CHECK_INTERVAL: Duration = Duration::from_millis(500);
pub const WRITER_HEAP_BYTES: usize = 32_000_000;

#[allow(dead_code)]
pub enum Mutation {
    Upsert(Box<Document>),
    Delete(String),
    UpsertMany(Vec<Document>),
    DeleteMany(Vec<String>),
    DeleteSourceInstance {
        instance_id: String,
    },
    /// Fetch the existing document by `doc_id`, overwrite its `body`,
    /// and write it back. Silently skipped if no document matches
    /// (the doc was deleted between enqueue and OCR). Used by the
    /// deferred OCR worker to inject OCR'd text into the live index.
    UpsertBody {
        doc_id: String,
        body: String,
    },
    /// Reply woken with the commit generation once every prior mutation has
    /// been applied and a commit has completed.
    Barrier(oneshot::Sender<u64>),
    Shutdown,
    /// Force a commit now and reply with the resulting generation.
    CommitNow(oneshot::Sender<u64>),
}

#[derive(Clone)]
pub struct IndexMutationTx {
    tx: mpsc::Sender<Mutation>,
}

impl IndexMutationTx {
    pub async fn send(&self, m: Mutation) -> Result<()> {
        self.tx
            .send(m)
            .await
            .map_err(|_| anyhow::anyhow!("index writer service has shut down"))
    }

    pub async fn barrier(&self) -> Result<u64> {
        let (tx, rx) = oneshot::channel();
        self.send(Mutation::Barrier(tx)).await?;
        rx.await
            .map_err(|_| anyhow::anyhow!("barrier dropped before commit"))
    }

    pub async fn commit_now(&self) -> Result<u64> {
        let (tx, rx) = oneshot::channel();
        self.send(Mutation::CommitNow(tx)).await?;
        rx.await
            .map_err(|_| anyhow::anyhow!("commit_now dropped"))
    }
}

/// Read-only handle to the index for the search path. Cheap to clone. Briefly
/// locks a tokio `Mutex` on query; never blocks on writer progress because the
/// writer task only holds the mutex per individual upsert/delete, not across
/// commits.
#[derive(Clone)]
pub struct SearchHandle {
    index: Arc<Mutex<LixunIndex>>,
}

impl SearchHandle {
    pub fn new(index: Arc<Mutex<LixunIndex>>) -> Self {
        Self { index }
    }

    pub async fn search(
        &self,
        query: &lixun_core::Query,
    ) -> Result<Vec<lixun_core::Hit>> {
        let idx = self.index.lock().await;
        idx.search(query)
    }

    pub async fn all_doc_ids(&self) -> Result<std::collections::HashSet<String>> {
        let idx = self.index.lock().await;
        idx.all_doc_ids()
    }

    /// Returns true when the doc identified by `doc_id` exists in the
    /// live index AND has a non-empty stored body. Used by the DB-16
    /// OCR enqueue short-circuit so a fresh reindex does not re-queue
    /// documents whose body was recovered in a prior OCR pass.
    pub async fn has_body(&self, doc_id: &str) -> Result<bool> {
        let idx = self.index.lock().await;
        Ok(idx.get_body_by_id(doc_id)?.is_some())
    }
}

pub fn spawn_writer_service(
    index: LixunIndex,
) -> Result<(IndexMutationTx, SearchHandle, tokio::task::JoinHandle<()>)> {
    let writer = index.writer(WRITER_HEAP_BYTES)?;

    let shared = Arc::new(Mutex::new(index));
    let search = SearchHandle::new(Arc::clone(&shared));

    let (tx, rx) = mpsc::channel::<Mutation>(4096);

    let handle = tokio::spawn(writer_loop(shared, writer, rx));

    Ok((IndexMutationTx { tx }, search, handle))
}

async fn writer_loop(
    shared: Arc<Mutex<LixunIndex>>,
    mut writer: TantivyIndexWriter<TantivyDoc>,
    mut rx: mpsc::Receiver<Mutation>,
) {
    let mut dirty = false;
    let mut last_commit = Instant::now();
    let mut generation: u64 = 0;
    let mut pending_barriers: Vec<oneshot::Sender<u64>> = Vec::new();
    let mut commit_tick = tokio::time::interval(COMMIT_CHECK_INTERVAL);
    commit_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    tracing::info!(
        "IndexService: writer started, heap={} MiB, min commit interval {:?}",
        WRITER_HEAP_BYTES / (1024 * 1024),
        COMMIT_MIN_INTERVAL
    );

    loop {
        tokio::select! {
            biased;

            maybe_mutation = rx.recv() => {
                let Some(mutation) = maybe_mutation else {
                    break;
                };
                match mutation {
                    Mutation::Shutdown => break,

                    Mutation::Barrier(reply) => {
                        if !dirty {
                            let _ = reply.send(generation);
                        } else {
                            pending_barriers.push(reply);
                        }
                    }

                    Mutation::CommitNow(reply) => {
                        pending_barriers.push(reply);
                        if let Err(e) = do_commit(
                            &shared,
                            &mut writer,
                            &mut generation,
                            &mut pending_barriers,
                            &mut dirty,
                            &mut last_commit,
                        ).await {
                            tracing::error!("IndexService: commit_now failed: {}", e);
                        }
                    }

                    Mutation::Upsert(doc) => {
                        if let Err(e) = apply_upsert(&shared, &mut writer, doc.as_ref()).await {
                            tracing::warn!("IndexService: upsert {} failed: {}", doc.id.0, e);
                        } else {
                            dirty = true;
                        }
                    }

                    Mutation::UpsertMany(docs) => {
                        for doc in &docs {
                            if let Err(e) = apply_upsert(&shared, &mut writer, doc).await {
                                tracing::warn!("IndexService: upsert {} failed: {}", doc.id.0, e);
                            } else {
                                dirty = true;
                            }
                        }
                    }

                    Mutation::Delete(id) => {
                        if let Err(e) = apply_delete(&shared, &mut writer, &id).await {
                            tracing::warn!("IndexService: delete {} failed: {}", id, e);
                        } else {
                            dirty = true;
                        }
                    }

                    Mutation::DeleteMany(ids) => {
                        for id in &ids {
                            if let Err(e) = apply_delete(&shared, &mut writer, id).await {
                                tracing::warn!("IndexService: delete {} failed: {}", id, e);
                            } else {
                                dirty = true;
                            }
                        }
                    }

                    Mutation::DeleteSourceInstance { instance_id } => {
                        if let Err(e) =
                            apply_delete_source_instance(&shared, &mut writer, &instance_id).await
                        {
                            tracing::warn!(
                                "IndexService: delete_source_instance {} failed: {}",
                                instance_id,
                                e
                            );
                        } else {
                            tracing::info!(
                                "IndexService: purged all docs for source instance {}",
                                instance_id
                            );
                            dirty = true;
                        }
                    }

                    Mutation::UpsertBody { doc_id, body } => {
                        match apply_upsert_body(&shared, &mut writer, &doc_id, body).await {
                            Ok(true) => dirty = true,
                            Ok(false) => {
                                tracing::debug!(
                                    "IndexService: upsert_body skipped, doc gone: {}",
                                    doc_id
                                );
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "IndexService: upsert_body {} failed: {}",
                                    doc_id,
                                    e
                                );
                            }
                        }
                    }
                }
            }

            _ = commit_tick.tick() => {
                if dirty && last_commit.elapsed() >= COMMIT_MIN_INTERVAL
                    && let Err(e) = do_commit(
                        &shared,
                        &mut writer,
                        &mut generation,
                        &mut pending_barriers,
                        &mut dirty,
                        &mut last_commit,
                    ).await {
                        tracing::error!("IndexService: periodic commit failed: {}", e);
                    }
            }
        }
    }

    tracing::info!("IndexService: shutdown, draining and committing");
    while let Ok(mutation) = rx.try_recv() {
        match mutation {
            Mutation::Upsert(doc) => {
                let _ = apply_upsert(&shared, &mut writer, doc.as_ref()).await;
                dirty = true;
            }
            Mutation::UpsertMany(docs) => {
                for doc in &docs {
                    let _ = apply_upsert(&shared, &mut writer, doc).await;
                    dirty = true;
                }
            }
            Mutation::Delete(id) => {
                let _ = apply_delete(&shared, &mut writer, &id).await;
                dirty = true;
            }
            Mutation::DeleteMany(ids) => {
                for id in &ids {
                    let _ = apply_delete(&shared, &mut writer, id).await;
                    dirty = true;
                }
            }
            Mutation::DeleteSourceInstance { instance_id } => {
                let _ = apply_delete_source_instance(&shared, &mut writer, &instance_id).await;
                dirty = true;
            }
            Mutation::UpsertBody { doc_id, body } => {
                if let Ok(true) = apply_upsert_body(&shared, &mut writer, &doc_id, body).await {
                    dirty = true;
                }
            }
            Mutation::Barrier(reply) => pending_barriers.push(reply),
            Mutation::CommitNow(reply) => pending_barriers.push(reply),
            Mutation::Shutdown => {}
        }
    }
    if dirty {
        let _ = do_commit(
            &shared,
            &mut writer,
            &mut generation,
            &mut pending_barriers,
            &mut dirty,
            &mut last_commit,
        )
        .await;
    }
    for reply in pending_barriers.drain(..) {
        let _ = reply.send(generation);
    }
    tracing::info!(
        "IndexService: writer task exiting at generation {}",
        generation
    );
}

async fn apply_upsert(
    shared: &Arc<Mutex<LixunIndex>>,
    writer: &mut TantivyIndexWriter<TantivyDoc>,
    doc: &Document,
) -> Result<()> {
    let mut idx = shared.lock().await;
    idx.upsert(doc, writer)?;
    Ok(())
}

async fn apply_delete(
    shared: &Arc<Mutex<LixunIndex>>,
    writer: &mut TantivyIndexWriter<TantivyDoc>,
    id: &str,
) -> Result<()> {
    let mut idx = shared.lock().await;
    idx.delete_by_id(id, writer)?;
    Ok(())
}

async fn apply_upsert_body(
    shared: &Arc<Mutex<LixunIndex>>,
    writer: &mut TantivyIndexWriter<TantivyDoc>,
    doc_id: &str,
    body: String,
) -> Result<bool> {
    let mut idx = shared.lock().await;
    let Some(mut doc) = idx.get_doc_by_id(doc_id)? else {
        return Ok(false);
    };
    doc.body = Some(body);
    idx.upsert(&doc, writer)?;
    Ok(true)
}

async fn apply_delete_source_instance(
    shared: &Arc<Mutex<LixunIndex>>,
    writer: &mut TantivyIndexWriter<TantivyDoc>,
    instance_id: &str,
) -> Result<()> {
    let mut idx = shared.lock().await;
    idx.delete_by_source_instance(instance_id, writer)?;
    Ok(())
}

async fn do_commit(
    shared: &Arc<Mutex<LixunIndex>>,
    writer: &mut TantivyIndexWriter<TantivyDoc>,
    generation: &mut u64,
    pending_barriers: &mut Vec<oneshot::Sender<u64>>,
    dirty: &mut bool,
    last_commit: &mut Instant,
) -> Result<()> {
    let start = Instant::now();
    {
        let mut idx = shared.lock().await;
        idx.commit(writer)?;
    }
    *generation += 1;
    *dirty = false;
    *last_commit = Instant::now();
    let elapsed_ms = start.elapsed().as_millis().min(u64::MAX as u128) as u64;
    COMMITS.fetch_add(1, Ordering::Relaxed);
    LAST_COMMIT_LATENCY_MS.store(elapsed_ms, Ordering::Relaxed);
    GENERATION.store(*generation, Ordering::Relaxed);
    tracing::debug!(
        "IndexService: committed generation {} in {:?}",
        *generation,
        start.elapsed()
    );
    for reply in pending_barriers.drain(..) {
        let _ = reply.send(*generation);
    }
    Ok(())
}

pub fn fs_doc_id(path: &std::path::Path) -> String {
    format!("fs:{}", path.to_string_lossy())
}

pub fn index_file(
    path: &std::path::Path,
    max_file_size_mb: u64,
    caps: &lixun_extract::ExtractorCapabilities,
    enqueue: Option<&dyn lixun_sources::OcrEnqueue>,
    body_checker: Option<&dyn lixun_sources::HasBody>,
) -> Result<Document> {
    use lixun_core::{Action, Category, DocId};

    let path_str = path.to_string_lossy().to_string();
    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    let metadata = std::fs::metadata(path)?;
    let is_dir = metadata.is_dir();
    let mtime = metadata
        .modified()
        .map(|t| {
            t.duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        })
        .unwrap_or(0);
    let size = metadata.len();

    let max_size = max_file_size_mb * 1024 * 1024;
    let (body, extract_fail) = if is_dir {
        (None, false)
    } else if size <= max_size {
        match lixun_sources::fs::FsSource::extract_content(path, caps, enqueue, body_checker) {
            Ok(Some(text)) => (Some(text), false),
            Ok(None) => (None, false),
            Err(_) => (None, true),
        }
    } else {
        (None, false)
    };
    let (icon_name, kind_label) = if is_dir {
        ("folder".to_string(), "Folder".to_string())
    } else {
        lixun_sources::fs::FsSource::metadata_for_path(path)
    };

    Ok(Document {
        id: DocId(fs_doc_id(path)),
        category: Category::File,
        title: filename,
        subtitle: path_str.clone(),
        icon_name: Some(icon_name),
        kind_label: Some(kind_label),
        body,
        path: path_str,
        mtime,
        size,
        action: Action::OpenFile {
            path: path.to_path_buf(),
        },
        extract_fail,
        sender: None,
        recipients: None,
        source_instance: "builtin:fs".into(),
        secondary_action: Some(Action::ShowInFileManager {
            path: path.to_path_buf(),
        }),
        extra: Vec::new(),
    })
}
