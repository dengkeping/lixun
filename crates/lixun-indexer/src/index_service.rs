//! Index service: single long-lived IndexWriter behind a mutation channel.
//!
//! All writers (reconciler, watcher, source_watcher, user reindex) send
//! `Mutation`s via `IndexMutationTx`. One writer task owns the sole
//! `tantivy::IndexWriter` and commits on a timer. Search path uses
//! `SearchHandle` and never waits on the writer.

use anyhow::Result;
use lixun_core::Document;
use lixun_index::{LixunIndex, TantivyDoc, TantivyIndexWriter};
use lixun_mutation::{MutationBatch, MutationBroadcaster, NoopBroadcaster, UpsertedDoc};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{Semaphore, mpsc, oneshot};

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

/// Bound on concurrent searches dispatched to `spawn_blocking`. Sized at
/// 2× CPU cores per Meilisearch / Tantivy guidance: enough to soak the
/// thread pool but not so much that a flood of slow queries starves the
/// writer task or balloons RAM via pinned searcher generations. Floored
/// at 4 so single-core dev machines still parallelise BM25/ANN fan-out.
fn default_search_concurrency() -> usize {
    (std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        * 2)
    .max(4)
}

/// Default Tantivy writer heap when callers do not pass an explicit
/// budget. Retained for `spawn_writer_service` legacy entry points and
/// existing tests that pre-date the impact-profile wiring.
pub const DEFAULT_WRITER_HEAP_BYTES: usize = 100_000_000;

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
        rx.await.map_err(|_| anyhow::anyhow!("commit_now dropped"))
    }
}

/// Read-only handle to the index for the search path. Cheap to clone.
///
/// Holds an `Arc<LixunIndex>` directly — no `Mutex`. Tantivy's
/// `IndexReader` is internally `Arc`-cloneable and lock-free, and
/// `LixunIndex`'s read methods (`search`, `search_with_breakdown`,
/// `all_doc_ids`, `hydrate_doc_by_id`, `get_body_by_id`,
/// `get_doc_by_id`) all take `&self`. The writer task in this same
/// crate also holds a clone of the same `Arc<LixunIndex>` and only
/// touches its own `IndexWriter`; per the Tantivy ARCHITECTURE
/// guarantee, a commit on the writer never blocks an in-flight
/// searcher (segments are immutable, old generations stay mmap'd
/// until all searchers drop them). All synchronous tantivy work is
/// dispatched via `tokio::task::spawn_blocking` to keep the async
/// runtime responsive, with a `Semaphore` bounding in-flight
/// searches at ~2× CPU cores.
#[derive(Clone)]
pub struct SearchHandle {
    index: Arc<LixunIndex>,
    permits: Arc<Semaphore>,
}

impl SearchHandle {
    pub fn new(index: Arc<LixunIndex>) -> Self {
        Self::with_concurrency(index, default_search_concurrency())
    }

    pub fn with_concurrency(index: Arc<LixunIndex>, concurrency: usize) -> Self {
        Self {
            index,
            permits: Arc::new(Semaphore::new(concurrency.max(1))),
        }
    }

    async fn run_blocking<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&LixunIndex) -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let _permit = self
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| anyhow::anyhow!("search semaphore closed"))?;
        let index = Arc::clone(&self.index);
        let res = tokio::task::spawn_blocking(move || f(&index))
            .await
            .map_err(|e| anyhow::anyhow!("search task join error: {e}"))?;
        drop(_permit);
        res
    }

    pub async fn search(&self, query: &lixun_core::Query) -> Result<Vec<lixun_core::Hit>> {
        let q = query.clone();
        self.run_blocking(move |idx| idx.search(&q)).await
    }

    pub async fn search_with_breakdown(
        &self,
        query: &lixun_core::Query,
    ) -> Result<Vec<(lixun_core::Hit, lixun_core::ScoreBreakdown)>> {
        let q = query.clone();
        self.run_blocking(move |idx| idx.search_with_breakdown(&q))
            .await
    }

    pub async fn all_doc_ids(&self) -> Result<std::collections::HashSet<String>> {
        self.run_blocking(|idx| idx.all_doc_ids()).await
    }

    /// Returns true when the doc identified by `doc_id` exists in the
    /// live index AND has a non-empty stored body. Used by the DB-16
    /// OCR enqueue short-circuit so a fresh reindex does not re-queue
    /// documents whose body was recovered in a prior OCR pass.
    pub async fn has_body(&self, doc_id: &str) -> Result<bool> {
        let id = doc_id.to_string();
        self.run_blocking(move |idx| Ok(idx.get_body_by_id(&id)?.is_some()))
            .await
    }

    pub async fn get_body(&self, doc_id: &str) -> Result<Option<String>> {
        let id = doc_id.to_string();
        self.run_blocking(move |idx| idx.get_body_by_id(&id)).await
    }

    /// Reconstruct a `Hit` + `ScoreBreakdown` for a single doc without
    /// running a query. The breakdown is degenerate (tantivy=0.0,
    /// multipliers=1.0) because there is no query context; the caller
    /// (Wave D fusion) assigns the final fused score before publishing.
    pub async fn hydrate_doc(
        &self,
        doc_id: &str,
    ) -> Result<Option<(lixun_core::Hit, lixun_core::ScoreBreakdown)>> {
        let id = doc_id.to_string();
        self.run_blocking(move |idx| idx.hydrate_doc_by_id(&id))
            .await
    }
}

#[async_trait::async_trait]
impl lixun_mutation::DocStore for SearchHandle {
    async fn all_doc_ids(&self) -> Result<std::collections::HashSet<String>> {
        SearchHandle::all_doc_ids(self).await
    }

    async fn hydrate_doc(
        &self,
        doc_id: &str,
    ) -> Result<Option<(lixun_core::Hit, lixun_core::ScoreBreakdown)>> {
        SearchHandle::hydrate_doc(self, doc_id).await
    }

    async fn get_body(&self, doc_id: &str) -> Result<Option<String>> {
        SearchHandle::get_body(self, doc_id).await
    }
}

pub fn spawn_writer_service(
    index: LixunIndex,
) -> Result<(IndexMutationTx, SearchHandle, tokio::task::JoinHandle<()>)> {
    spawn_writer_service_with_broadcaster(
        index,
        Arc::new(NoopBroadcaster),
        DEFAULT_WRITER_HEAP_BYTES,
        4,
    )
}

/// Variant of [`spawn_writer_service`] that fires
/// `broadcaster.broadcast` from `tokio::task::spawn_blocking` after
/// every successful commit. The writer task never awaits on the
/// broadcaster, so a slow consumer cannot stall index commits.
///
/// `tantivy_heap_bytes` and `tantivy_num_threads` are seeded from the
/// active [`lixun_core::ImpactProfile`] by the daemon caller.
pub fn spawn_writer_service_with_broadcaster(
    index: LixunIndex,
    broadcaster: Arc<dyn MutationBroadcaster>,
    tantivy_heap_bytes: usize,
    tantivy_num_threads: usize,
) -> Result<(IndexMutationTx, SearchHandle, tokio::task::JoinHandle<()>)> {
    let num_threads = tantivy_num_threads.max(1);
    let writer = index.writer_with_num_threads(num_threads, tantivy_heap_bytes)?;

    let shared = Arc::new(index);
    let search = SearchHandle::new(Arc::clone(&shared));

    let (tx, rx) = mpsc::channel::<Mutation>(4096);

    let handle = tokio::spawn(writer_loop(
        shared,
        writer,
        rx,
        broadcaster,
        tantivy_heap_bytes,
    ));

    Ok((IndexMutationTx { tx }, search, handle))
}

async fn writer_loop(
    shared: Arc<LixunIndex>,
    mut writer: TantivyIndexWriter<TantivyDoc>,
    mut rx: mpsc::Receiver<Mutation>,
    broadcaster: Arc<dyn MutationBroadcaster>,
    heap_bytes: usize,
) {
    let mut dirty = false;
    let mut last_commit = Instant::now();
    let mut generation: u64 = 0;
    let mut pending_barriers: Vec<oneshot::Sender<u64>> = Vec::new();
    let mut pending_batch = MutationBatch::default();
    let mut commit_tick = tokio::time::interval(COMMIT_CHECK_INTERVAL);
    commit_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    tracing::info!(
        "IndexService: writer started, heap={} MiB, min commit interval {:?}",
        heap_bytes / (1024 * 1024),
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
                        match do_commit(
                            &shared,
                            &mut writer,
                            &mut generation,
                            &mut pending_barriers,
                            &mut dirty,
                            &mut last_commit,
                        ) {
                            Ok(()) => flush_post_commit(
                                &mut pending_batch,
                                generation,
                                &broadcaster,
                            ),
                            Err(e) => {
                                tracing::error!("IndexService: commit_now failed: {}", e);
                            }
                        }
                    }

                    Mutation::Upsert(doc) => {
                        if let Err(e) = apply_upsert(&shared, &mut writer, doc.as_ref()) {
                            tracing::warn!("IndexService: upsert {} failed: {}", doc.id.0, e);
                        } else {
                            pending_batch.upserts.push(upserted_doc_from(doc.as_ref()));
                            dirty = true;
                        }
                    }

                    Mutation::UpsertMany(docs) => {
                        for doc in &docs {
                            if let Err(e) = apply_upsert(&shared, &mut writer, doc) {
                                tracing::warn!("IndexService: upsert {} failed: {}", doc.id.0, e);
                            } else {
                                pending_batch.upserts.push(upserted_doc_from(doc));
                                dirty = true;
                            }
                        }
                    }

                    Mutation::Delete(id) => {
                        if let Err(e) = apply_delete(&shared, &mut writer, &id) {
                            tracing::warn!("IndexService: delete {} failed: {}", id, e);
                        } else {
                            pending_batch.deletes.push(id);
                            dirty = true;
                        }
                    }

                    Mutation::DeleteMany(ids) => {
                        for id in ids {
                            if let Err(e) = apply_delete(&shared, &mut writer, &id) {
                                tracing::warn!("IndexService: delete {} failed: {}", id, e);
                            } else {
                                pending_batch.deletes.push(id);
                                dirty = true;
                            }
                        }
                    }

                    Mutation::DeleteSourceInstance { instance_id } => {
                        if let Err(e) =
                            apply_delete_source_instance(&shared, &mut writer, &instance_id)
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
                            // No broadcast: apply_delete_source_instance does
                            // not return the affected doc_ids, and instance_id
                            // is not a doc_id. Broadcasting it would corrupt
                            // any consumer that keys off doc_id.
                            dirty = true;
                        }
                    }

                    Mutation::UpsertBody { doc_id, body } => {
                        match apply_upsert_body(&shared, &mut writer, &doc_id, body) {
                            Ok(Some(updated)) => {
                                pending_batch.upserts.push(upserted_doc_from(&updated));
                                dirty = true;
                            }
                            Ok(None) => {
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
                if dirty && last_commit.elapsed() >= COMMIT_MIN_INTERVAL {
                    match do_commit(
                        &shared,
                        &mut writer,
                        &mut generation,
                        &mut pending_barriers,
                        &mut dirty,
                        &mut last_commit,
                    ) {
                        Ok(()) => flush_post_commit(
                            &mut pending_batch,
                            generation,
                            &broadcaster,
                        ),
                        Err(e) => {
                            tracing::error!("IndexService: periodic commit failed: {}", e);
                        }
                    }
                }
            }
        }
    }

    tracing::info!("IndexService: shutdown, draining and committing");
    while let Ok(mutation) = rx.try_recv() {
        match mutation {
            Mutation::Upsert(doc) => {
                if apply_upsert(&shared, &mut writer, doc.as_ref()).is_ok() {
                    pending_batch.upserts.push(upserted_doc_from(doc.as_ref()));
                }
                dirty = true;
            }
            Mutation::UpsertMany(docs) => {
                for doc in &docs {
                    if apply_upsert(&shared, &mut writer, doc).is_ok() {
                        pending_batch.upserts.push(upserted_doc_from(doc));
                    }
                    dirty = true;
                }
            }
            Mutation::Delete(id) => {
                if apply_delete(&shared, &mut writer, &id).is_ok() {
                    pending_batch.deletes.push(id);
                }
                dirty = true;
            }
            Mutation::DeleteMany(ids) => {
                for id in ids {
                    if apply_delete(&shared, &mut writer, &id).is_ok() {
                        pending_batch.deletes.push(id);
                    }
                    dirty = true;
                }
            }
            Mutation::DeleteSourceInstance { instance_id } => {
                let _ = apply_delete_source_instance(&shared, &mut writer, &instance_id);
                dirty = true;
            }
            Mutation::UpsertBody { doc_id, body } => {
                if let Ok(Some(updated)) = apply_upsert_body(&shared, &mut writer, &doc_id, body) {
                    pending_batch.upserts.push(upserted_doc_from(&updated));
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
        );
        flush_post_commit(&mut pending_batch, generation, &broadcaster);
    }
    for reply in pending_barriers.drain(..) {
        let _ = reply.send(generation);
    }
    tracing::info!(
        "IndexService: writer task exiting at generation {}",
        generation
    );
}

fn apply_upsert(
    shared: &LixunIndex,
    writer: &mut TantivyIndexWriter<TantivyDoc>,
    doc: &Document,
) -> Result<()> {
    shared.upsert(doc, writer)?;
    Ok(())
}

fn apply_delete(
    shared: &LixunIndex,
    writer: &mut TantivyIndexWriter<TantivyDoc>,
    id: &str,
) -> Result<()> {
    shared.delete_by_id(id, writer)?;
    Ok(())
}

fn apply_upsert_body(
    shared: &LixunIndex,
    writer: &mut TantivyIndexWriter<TantivyDoc>,
    doc_id: &str,
    body: String,
) -> Result<Option<Document>> {
    let Some(mut doc) = shared.get_doc_by_id(doc_id)? else {
        return Ok(None);
    };
    doc.body = Some(body);
    shared.upsert(&doc, writer)?;
    Ok(Some(doc))
}

fn apply_delete_source_instance(
    shared: &LixunIndex,
    writer: &mut TantivyIndexWriter<TantivyDoc>,
    instance_id: &str,
) -> Result<()> {
    shared.delete_by_source_instance(instance_id, writer)?;
    Ok(())
}

fn do_commit(
    shared: &LixunIndex,
    writer: &mut TantivyIndexWriter<TantivyDoc>,
    generation: &mut u64,
    pending_barriers: &mut Vec<oneshot::Sender<u64>>,
    dirty: &mut bool,
    last_commit: &mut Instant,
) -> Result<()> {
    let start = Instant::now();
    shared.commit(writer)?;
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

fn upserted_doc_from(doc: &Document) -> UpsertedDoc {
    UpsertedDoc {
        doc_id: doc.id.0.clone(),
        source_instance: doc.source_instance.clone(),
        mtime: doc.mtime,
        mime: doc.mime.clone(),
        body: doc.body.clone(),
    }
}

fn flush_post_commit(
    pending_batch: &mut MutationBatch,
    generation: u64,
    broadcaster: &Arc<dyn MutationBroadcaster>,
) {
    if pending_batch.is_empty() {
        return;
    }
    pending_batch.generation = generation;
    let batch = std::mem::take(pending_batch);
    let bcaster = Arc::clone(broadcaster);
    tokio::task::spawn_blocking(move || bcaster.broadcast(&batch));
}

pub fn fs_doc_id(path: &std::path::Path) -> String {
    lixun_core::paths::canonical_fs_doc_id(path)
}

pub fn index_file(
    path: &std::path::Path,
    max_file_size_mb: u64,
    caps: &lixun_extract::ExtractorCapabilities,
    enqueue: Option<&dyn lixun_sources::OcrEnqueue>,
    body_checker: Option<&dyn lixun_sources::HasBody>,
    min_image_side_px: u32,
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
        match lixun_sources::fs::FsSource::extract_content(
            path,
            caps,
            enqueue,
            body_checker,
            min_image_side_px,
        ) {
            Ok(Some(text)) => (Some(text), false),
            // Extract returned Ok(None) — source has no synchronous
            // body to offer (cache HIT with empty text, or OCR
            // deferred). If the live index already carries a body
            // for this doc (recovered by a prior OCR pass), preserve
            // it instead of clobbering it with None. reindex_full
            // wipes the manifest so every file looks changed; without
            // this guard the OCR-recovered text would be lost and
            // then re-enqueued on the next pass, wasting a full OCR
            // cycle per affected document.
            Ok(None) => {
                let preserved =
                    body_checker.and_then(|bc| bc.get_body(&fs_doc_id(path)).ok().flatten());
                (preserved, false)
            }
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
        mime: None,
        source_instance: "builtin:fs".into(),
        secondary_action: Some(Action::ShowInFileManager {
            path: path.to_path_buf(),
        }),
        extra: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    static HOME_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn with_isolated_cache<F, R>(f: F) -> R
    where
        F: FnOnce() -> R,
    {
        let lock = HOME_LOCK.get_or_init(|| Mutex::new(()));
        let _g = lock.lock().unwrap();
        let td = tempfile::TempDir::new().unwrap();
        let old_xdg = std::env::var_os("XDG_CACHE_HOME");
        let old_home = std::env::var_os("HOME");
        // SAFETY: env is process-global; HOME_LOCK serializes every test
        // in this module that touches the cache and no other code in the
        // crate reads these vars during the test window.
        unsafe {
            std::env::set_var("XDG_CACHE_HOME", td.path());
            std::env::set_var("HOME", td.path());
        }
        let out = f();
        unsafe {
            match old_xdg {
                Some(v) => std::env::set_var("XDG_CACHE_HOME", v),
                None => std::env::remove_var("XDG_CACHE_HOME"),
            }
            match old_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
        drop(td);
        out
    }

    #[derive(Default)]
    struct MockBodyChecker {
        bodies: Mutex<HashMap<String, String>>,
    }

    impl MockBodyChecker {
        fn with_body(doc_id: &str, body: &str) -> Self {
            let mut bodies = HashMap::new();
            bodies.insert(doc_id.to_string(), body.to_string());
            Self {
                bodies: Mutex::new(bodies),
            }
        }
    }

    impl lixun_sources::HasBody for MockBodyChecker {
        fn has_body(&self, doc_id: &str) -> Result<bool> {
            Ok(self.bodies.lock().unwrap().contains_key(doc_id))
        }

        fn get_body(&self, doc_id: &str) -> Result<Option<String>> {
            Ok(self.bodies.lock().unwrap().get(doc_id).cloned())
        }
    }

    #[test]
    fn index_file_preserves_existing_body_when_extract_returns_none() {
        with_isolated_cache(|| {
            let tmp = tempfile::tempdir().unwrap();
            let txt = tmp.path().join("empty.txt");
            std::fs::write(&txt, b"").unwrap();

            let caps = lixun_extract::ExtractorCapabilities::all_available_no_timeout();
            let doc_id = fs_doc_id(&txt);
            let body_checker = MockBodyChecker::with_body(&doc_id, "recovered by ocr");

            let doc = index_file(&txt, 100, &caps, None, Some(&body_checker), 0).unwrap();
            assert_eq!(
                doc.body.as_deref(),
                Some("recovered by ocr"),
                "extract=Ok(None) with indexed body must preserve the existing body",
            );
            assert!(!doc.extract_fail);
        });
    }

    #[test]
    fn index_file_writes_none_when_extract_none_and_no_prior_body() {
        with_isolated_cache(|| {
            let tmp = tempfile::tempdir().unwrap();
            let txt = tmp.path().join("empty.txt");
            std::fs::write(&txt, b"").unwrap();

            let caps = lixun_extract::ExtractorCapabilities::all_available_no_timeout();
            let body_checker = MockBodyChecker::default();

            let doc = index_file(&txt, 100, &caps, None, Some(&body_checker), 0).unwrap();
            assert!(
                doc.body.is_none(),
                "extract=Ok(None) with no indexed body must leave body None",
            );
            assert!(!doc.extract_fail);
        });
    }

    #[test]
    fn index_file_overwrites_body_when_extract_returns_some() {
        with_isolated_cache(|| {
            let tmp = tempfile::tempdir().unwrap();
            let txt = tmp.path().join("fresh.txt");
            std::fs::write(&txt, b"fresh content").unwrap();

            let caps = lixun_extract::ExtractorCapabilities::all_available_no_timeout();
            let doc_id = fs_doc_id(&txt);
            let body_checker = MockBodyChecker::with_body(&doc_id, "stale body");

            let doc = index_file(&txt, 100, &caps, None, Some(&body_checker), 0).unwrap();
            assert_eq!(
                doc.body.as_deref(),
                Some("fresh content"),
                "extract=Ok(Some) must overwrite, never preserve stale body",
            );
            assert!(!doc.extract_fail);
        });
    }
}
