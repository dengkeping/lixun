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
/// Post-commit broadcast tasks that failed to join (panicked or were
/// cancelled). Module-internal on purpose: surfacing it through
/// `stats()` would ripple into the `lixun_ipc::WriterStats` wire
/// type. Read it via [`broadcast_failures`].
static BROADCAST_FAILURES: AtomicU64 = AtomicU64::new(0);

pub fn stats() -> (u64, u64, u64) {
    (
        COMMITS.load(Ordering::Relaxed),
        LAST_COMMIT_LATENCY_MS.load(Ordering::Relaxed),
        GENERATION.load(Ordering::Relaxed),
    )
}

/// Count of post-commit mutation broadcasts whose `spawn_blocking`
/// task did not complete cleanly. Not part of [`stats`] to keep the
/// IPC `WriterStats` wire type stable.
pub fn broadcast_failures() -> u64 {
    BROADCAST_FAILURES.load(Ordering::Relaxed)
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
    /// Purge every doc whose id equals `prefix` OR starts with
    /// `prefix + "/"`. Sole entry point for directory removals and
    /// rename-aways: a watched dir vanishes, the watcher emits this
    /// once, and the writer enumerates affected ids client-side from
    /// `all_doc_ids()` plus any uncommitted upserts staged in the same
    /// batch, then issues exact-term deletes — no Tantivy schema
    /// change, no prefix/regex query.
    DeleteSubtree(String),
    DeleteSourceInstance {
        instance_id: String,
    },
    /// Fetch the existing document by `doc_id`, overwrite its `body`,
    /// and write it back. Silently skipped if no document matches
    /// (the doc was deleted between enqueue and OCR). Used by the
    /// deferred OCR worker to inject OCR'd text into the live index.
    ///
    /// `expected_mtime` is the file mtime captured when the OCR job
    /// was enqueued (the OCR queue row carries it). The write-back
    /// is a read-modify-write against the last-committed reader, so
    /// without a guard it would clobber any newer upsert for the
    /// same doc that landed between enqueue and apply — whether
    /// already committed or still staged in the current batch. When
    /// `Some`, the writer skips the body injection if either copy of
    /// the doc carries a newer mtime. `None` preserves the old
    /// unguarded behavior for producers without an mtime snapshot.
    UpsertBody {
        doc_id: String,
        body: String,
        expected_mtime: Option<i64>,
    },
    /// Reply woken with the commit generation once every prior mutation has
    /// been applied and a commit has completed.
    Barrier(oneshot::Sender<u64>),
    /// Reply woken once every prior mutation has been applied to the
    /// index writer — NOT necessarily committed. Cheap backpressure
    /// ack for bulk producers (crawl batches): unlike `Barrier`, it
    /// does not park the producer until the commit timer fires, so
    /// ingest throughput is bounded by writer speed instead of
    /// `COMMIT_MIN_INTERVAL`.
    Applied(oneshot::Sender<()>),
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

    /// Wait until every previously sent mutation has been applied to
    /// the index writer (backpressure), without waiting for a commit.
    pub async fn applied(&self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.send(Mutation::Applied(tx)).await?;
        rx.await
            .map_err(|_| anyhow::anyhow!("applied ack dropped"))
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
    /// Bounded lexical result cache keyed by (query text, limit,
    /// reload epoch). The epoch component means a commit+reload
    /// naturally invalidates every prior entry; stage-2 (frecency /
    /// latch) is applied downstream per request, so click feedback
    /// stays live even on a cache hit.
    result_cache: Arc<std::sync::Mutex<ResultCache>>,
}

type CachedPairs = Vec<(lixun_core::Hit, lixun_core::ScoreBreakdown)>;

struct ResultCache(std::collections::HashMap<(String, u32, u64), CachedPairs>);

const RESULT_CACHE_MAX: usize = 64;

impl ResultCache {
    fn get(&self, key: &(String, u32, u64)) -> Option<CachedPairs> {
        self.0.get(key).cloned()
    }

    fn put(&mut self, key: (String, u32, u64), pairs: CachedPairs) {
        if self.0.len() >= RESULT_CACHE_MAX {
            self.0.clear();
        }
        self.0.insert(key, pairs);
    }
}

impl SearchHandle {
    pub fn new(index: Arc<LixunIndex>) -> Self {
        Self::with_concurrency(index, default_search_concurrency())
    }

    pub fn with_concurrency(index: Arc<LixunIndex>, concurrency: usize) -> Self {
        Self {
            index,
            permits: Arc::new(Semaphore::new(concurrency.max(1))),
            result_cache: Arc::new(std::sync::Mutex::new(ResultCache(
                std::collections::HashMap::new(),
            ))),
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
        let key = (
            query.text.clone(),
            query.limit,
            self.index.reload_epoch(),
        );
        if let Ok(cache) = self.result_cache.lock()
            && let Some(pairs) = cache.get(&key)
        {
            return Ok(pairs);
        }
        let q = query.clone();
        let pairs = self
            .run_blocking(move |idx| idx.search_with_breakdown(&q))
            .await?;
        if let Ok(mut cache) = self.result_cache.lock() {
            cache.put(key, pairs.clone());
        }
        Ok(pairs)
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

    /// Batch hydration: one blocking hop for the whole id list instead
    /// of one `spawn_blocking` round trip per doc. Ids with no live doc
    /// are skipped; order is preserved.
    pub async fn hydrate_docs(
        &self,
        doc_ids: Vec<String>,
    ) -> Result<Vec<(lixun_core::Hit, lixun_core::ScoreBreakdown)>> {
        self.run_blocking(move |idx| idx.hydrate_docs_by_ids(&doc_ids))
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
    // Prefixes deleted by `DeleteSubtree` in the current uncommitted
    // batch. `collect_subtree_ids` only sees committed ids plus
    // already-staged upserts, so an upsert that was queued in the
    // channel behind the delete and processed afterwards would
    // silently re-create an orphan under the deleted prefix. Any
    // upsert landing under a tombstoned prefix before the next
    // commit is dropped instead. Cleared by `do_commit` once the
    // batch is durable.
    let mut subtree_tombstones: Vec<String> = Vec::new();
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

                    Mutation::Applied(reply) => {
                        // Channel order guarantees every prior mutation on
                        // this receiver has already been applied above.
                        let _ = reply.send(());
                    }

                    Mutation::CommitNow(reply) => {
                        pending_barriers.push(reply);
                        match do_commit(
                            &shared,
                            &mut writer,
                            &mut generation,
                            &mut pending_barriers,
                            &mut subtree_tombstones,
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
                        if id_tombstoned(&doc.id.0, &subtree_tombstones) {
                            tracing::debug!(
                                "IndexService: upsert {} dropped, subtree deleted in this batch",
                                doc.id.0
                            );
                        } else if let Err(e) = apply_upsert(&shared, &mut writer, doc.as_ref()) {
                            tracing::warn!("IndexService: upsert {} failed: {}", doc.id.0, e);
                        } else {
                            pending_batch.upserts.push(upserted_doc_from(doc.as_ref()));
                            dirty = true;
                        }
                    }

                    Mutation::UpsertMany(docs) => {
                        for doc in &docs {
                            if id_tombstoned(&doc.id.0, &subtree_tombstones) {
                                tracing::debug!(
                                    "IndexService: upsert {} dropped, subtree deleted in this batch",
                                    doc.id.0
                                );
                            } else if let Err(e) = apply_upsert(&shared, &mut writer, doc) {
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

                    Mutation::DeleteSubtree(prefix) => {
                        let matched = collect_subtree_ids(&shared, &pending_batch, &prefix);
                        pending_batch
                            .upserts
                            .retain(|u| !id_in_subtree(&u.doc_id, &prefix));
                        for id in matched {
                            if let Err(e) = apply_delete(&shared, &mut writer, &id) {
                                tracing::warn!(
                                    "IndexService: delete_subtree {} failed: {}",
                                    id,
                                    e
                                );
                            } else {
                                pending_batch.deletes.push(id);
                                dirty = true;
                            }
                        }
                        if !subtree_tombstones.contains(&prefix) {
                            subtree_tombstones.push(prefix);
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

                    Mutation::UpsertBody {
                        doc_id,
                        body,
                        expected_mtime,
                    } => {
                        if id_tombstoned(&doc_id, &subtree_tombstones) {
                            tracing::debug!(
                                "IndexService: upsert_body {} dropped, subtree deleted in this batch",
                                doc_id
                            );
                        } else {
                            match apply_upsert_body(
                                &shared,
                                &mut writer,
                                &pending_batch,
                                &doc_id,
                                body,
                                expected_mtime,
                            ) {
                                Ok(Some(updated)) => {
                                    pending_batch.upserts.push(upserted_doc_from(&updated));
                                    dirty = true;
                                }
                                Ok(None) => {
                                    // Skip reason (doc gone / stale snapshot)
                                    // already logged by apply_upsert_body.
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
            }

            _ = commit_tick.tick() => {
                if dirty && last_commit.elapsed() >= COMMIT_MIN_INTERVAL {
                    match do_commit(
                        &shared,
                        &mut writer,
                        &mut generation,
                        &mut pending_barriers,
                        &mut subtree_tombstones,
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
                if !id_tombstoned(&doc.id.0, &subtree_tombstones)
                    && apply_upsert(&shared, &mut writer, doc.as_ref()).is_ok()
                {
                    pending_batch.upserts.push(upserted_doc_from(doc.as_ref()));
                }
                dirty = true;
            }
            Mutation::UpsertMany(docs) => {
                for doc in &docs {
                    if !id_tombstoned(&doc.id.0, &subtree_tombstones)
                        && apply_upsert(&shared, &mut writer, doc).is_ok()
                    {
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
            Mutation::DeleteSubtree(prefix) => {
                let matched = collect_subtree_ids(&shared, &pending_batch, &prefix);
                pending_batch
                    .upserts
                    .retain(|u| !id_in_subtree(&u.doc_id, &prefix));
                for id in matched {
                    if apply_delete(&shared, &mut writer, &id).is_ok() {
                        pending_batch.deletes.push(id);
                    }
                    dirty = true;
                }
                if !subtree_tombstones.contains(&prefix) {
                    subtree_tombstones.push(prefix);
                }
            }
            Mutation::DeleteSourceInstance { instance_id } => {
                let _ = apply_delete_source_instance(&shared, &mut writer, &instance_id);
                dirty = true;
            }
            Mutation::UpsertBody {
                doc_id,
                body,
                expected_mtime,
            } => {
                if !id_tombstoned(&doc_id, &subtree_tombstones)
                    && let Ok(Some(updated)) = apply_upsert_body(
                        &shared,
                        &mut writer,
                        &pending_batch,
                        &doc_id,
                        body,
                        expected_mtime,
                    )
                {
                    pending_batch.upserts.push(upserted_doc_from(&updated));
                    dirty = true;
                }
            }
            Mutation::Barrier(reply) => pending_barriers.push(reply),
            Mutation::Applied(reply) => {
                let _ = reply.send(());
            }
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
            &mut subtree_tombstones,
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

fn id_in_subtree(id: &str, prefix: &str) -> bool {
    if id == prefix {
        return true;
    }
    if let Some(rest) = id.strip_prefix(prefix) {
        return rest.starts_with('/');
    }
    false
}

/// True when `id` falls under any subtree prefix deleted earlier in
/// the current uncommitted batch (see `subtree_tombstones` in
/// `writer_loop`).
fn id_tombstoned(id: &str, tombstones: &[String]) -> bool {
    tombstones.iter().any(|p| id_in_subtree(id, p))
}

fn collect_subtree_ids(
    shared: &LixunIndex,
    pending_batch: &MutationBatch,
    prefix: &str,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    match shared.all_doc_ids() {
        Ok(committed) => {
            for id in committed {
                if id_in_subtree(&id, prefix) {
                    out.push(id);
                }
            }
        }
        Err(e) => {
            tracing::warn!(
                "IndexService: delete_subtree({}) all_doc_ids failed: {}",
                prefix,
                e
            );
        }
    }
    for u in &pending_batch.upserts {
        if id_in_subtree(&u.doc_id, prefix) {
            out.push(u.doc_id.clone());
        }
    }
    out.sort();
    out.dedup();
    out
}

/// OCR lost-update guard. The body injection is a read-modify-write:
/// fetch the committed doc, set `.body`, write the whole doc back.
/// Without a guard, a full upsert for the same doc that landed after
/// the OCR job was enqueued would be clobbered with the stale
/// metadata snapshot. `expected_mtime` (the file mtime recorded in
/// the OCR queue row at enqueue time) closes the race in both
/// directions the writer can observe:
///
/// - committed: the fetched doc's mtime is newer than the snapshot,
///   meaning a reindex of changed content already landed — the OCR
///   text belongs to dead bytes;
/// - staged: a newer upsert for the same id sits in the uncommitted
///   `pending_batch`, invisible to `get_doc_by_id` (which reads the
///   last-committed generation), so writing back the committed copy
///   would silently undo it.
///
/// Both cases skip with a warn and return `Ok(None)`; the watcher's
/// next pass re-enqueues OCR with fresh metadata if still needed.
/// Equal mtimes proceed: OCR only adds a body to an otherwise
/// unchanged doc.
fn apply_upsert_body(
    shared: &LixunIndex,
    writer: &mut TantivyIndexWriter<TantivyDoc>,
    pending_batch: &MutationBatch,
    doc_id: &str,
    body: String,
    expected_mtime: Option<i64>,
) -> Result<Option<Document>> {
    let Some(mut doc) = shared.get_doc_by_id(doc_id)? else {
        tracing::debug!("IndexService: upsert_body skipped, doc gone: {}", doc_id);
        return Ok(None);
    };
    if let Some(expected) = expected_mtime {
        if doc.mtime > expected {
            tracing::warn!(
                "IndexService: upsert_body {} skipped, committed doc mtime {} newer than OCR snapshot {}",
                doc_id,
                doc.mtime,
                expected
            );
            return Ok(None);
        }
        if let Some(staged) = pending_batch
            .upserts
            .iter()
            .filter(|u| u.doc_id == doc_id && u.mtime > expected)
            .map(|u| u.mtime)
            .max()
        {
            tracing::warn!(
                "IndexService: upsert_body {} skipped, staged upsert mtime {} newer than OCR snapshot {}",
                doc_id,
                staged,
                expected
            );
            return Ok(None);
        }
    }
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
    subtree_tombstones: &mut Vec<String>,
    dirty: &mut bool,
    last_commit: &mut Instant,
) -> Result<()> {
    let start = Instant::now();
    shared.commit(writer)?;
    shared.reload()?;
    tracing::debug!(target: "tantivy_reload", "reload triggered");
    *generation += 1;
    *dirty = false;
    *last_commit = Instant::now();
    // The batch is durable and visible; subtree tombstones only
    // guard the window between a DeleteSubtree and the commit that
    // makes it visible to `collect_subtree_ids`.
    subtree_tombstones.clear();
    let elapsed_ms = start.elapsed().as_millis().min(u64::MAX as u128) as u64;
    COMMITS.fetch_add(1, Ordering::Relaxed);
    LAST_COMMIT_LATENCY_MS.store(elapsed_ms, Ordering::Relaxed);
    GENERATION.store(*generation, Ordering::Relaxed);
    tracing::debug!(
        "IndexService: committed generation {} in {:?}",
        *generation,
        start.elapsed()
    );
    // Ordering is load-bearing: barrier acks must fire only after
    // `shared.reload()` above has succeeded. `barrier()` callers
    // (e.g. search-after-write paths) treat the ack as "my mutations
    // are visible to the next searcher"; acking between commit and
    // reload would let them read a pre-reload generation.
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
    // Fire-and-forget by design — the writer task must never await
    // the broadcaster — but the outcome is still observed: a watcher
    // task awaits the blocking handle so a panicking broadcaster is
    // logged and counted instead of vanishing with a dropped
    // JoinHandle.
    let join = tokio::task::spawn_blocking(move || bcaster.broadcast(&batch));
    tokio::spawn(async move {
        if let Err(e) = join.await {
            BROADCAST_FAILURES.fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                "IndexService: post-commit broadcast for generation {} failed: {}",
                generation,
                e
            );
        }
    });
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
    let (icon_name, kind_label, mime) = if is_dir {
        ("folder".to_string(), "Folder".to_string(), None)
    } else {
        let (icon, kind, mime) = lixun_sources::fs::FsSource::metadata_for_path(path);
        (icon, kind, Some(mime))
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
        mime,
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
    fn index_file_populates_mime_for_files() {
        with_isolated_cache(|| {
            let tmp = tempfile::tempdir().unwrap();
            let txt = tmp.path().join("note.txt");
            std::fs::write(&txt, b"hello").unwrap();

            let caps = lixun_extract::ExtractorCapabilities::all_available_no_timeout();
            let doc = index_file(&txt, 100, &caps, None, None, 0).unwrap();
            // Preview plugins fall back to `Hit.mime` when the
            // extension is inconclusive; the live-index path must
            // populate it (kind_label carries the human label).
            assert_eq!(doc.mime.as_deref(), Some("text/plain"));

            let dir_doc = index_file(tmp.path(), 100, &caps, None, None, 0).unwrap();
            assert!(dir_doc.mime.is_none(), "directories carry no mime");
        });
    }

    fn make_fs_doc(id: &str) -> Document {
        use lixun_core::{Action, Category, DocId};
        Document {
            id: DocId(id.to_string()),
            category: Category::File,
            title: id.rsplit('/').next().unwrap_or(id).to_string(),
            subtitle: id.to_string(),
            icon_name: None,
            kind_label: None,
            body: None,
            path: id.to_string(),
            mtime: 0,
            size: 0,
            action: Action::OpenFile {
                path: std::path::PathBuf::from(id),
            },
            extract_fail: false,
            sender: None,
            recipients: None,
            mime: None,
            source_instance: "builtin:fs".into(),
            secondary_action: None,
            extra: Vec::new(),
        }
    }

    fn fresh_writer_service() -> (
        tempfile::TempDir,
        IndexMutationTx,
        SearchHandle,
        tokio::task::JoinHandle<()>,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let idx = LixunIndex::create_or_open(
            tmp.path().to_str().unwrap(),
            lixun_core::RankingConfig::default(),
        )
        .unwrap();
        let (tx, search, handle) = spawn_writer_service(idx).unwrap();
        (tmp, tx, search, handle)
    }

    #[tokio::test]
    async fn writer_subtree_delete_purges_matching_ids() {
        let (_tmp, tx, search, _handle) = fresh_writer_service();
        let docs = vec![
            make_fs_doc("fs:/d"),
            make_fs_doc("fs:/d/a.txt"),
            make_fs_doc("fs:/d/sub/b.txt"),
            make_fs_doc("fs:/other.txt"),
        ];
        tx.send(Mutation::UpsertMany(docs)).await.unwrap();
        let _ = tx.commit_now().await.unwrap();
        let before = search.all_doc_ids().await.unwrap();
        assert!(before.contains("fs:/d"));
        assert!(before.contains("fs:/d/a.txt"));
        assert!(before.contains("fs:/d/sub/b.txt"));
        assert!(before.contains("fs:/other.txt"));

        tx.send(Mutation::DeleteSubtree("fs:/d".into()))
            .await
            .unwrap();
        let _ = tx.commit_now().await.unwrap();

        let after = search.all_doc_ids().await.unwrap();
        assert!(!after.contains("fs:/d"), "prefix doc must be purged");
        assert!(
            !after.contains("fs:/d/a.txt"),
            "direct child must be purged"
        );
        assert!(
            !after.contains("fs:/d/sub/b.txt"),
            "deep descendant must be purged"
        );
        assert!(
            after.contains("fs:/other.txt"),
            "unrelated sibling must survive",
        );
    }

    #[tokio::test]
    async fn writer_subtree_delete_catches_uncommitted_same_subtree_upsert() {
        let (_tmp, tx, search, _handle) = fresh_writer_service();
        tx.send(Mutation::UpsertMany(vec![make_fs_doc("fs:/keep.txt")]))
            .await
            .unwrap();
        let _ = tx.commit_now().await.unwrap();

        tx.send(Mutation::Upsert(Box::new(make_fs_doc("fs:/d/new.txt"))))
            .await
            .unwrap();
        tx.send(Mutation::DeleteSubtree("fs:/d".into()))
            .await
            .unwrap();
        let _ = tx.commit_now().await.unwrap();

        let after = search.all_doc_ids().await.unwrap();
        assert!(
            !after.contains("fs:/d/new.txt"),
            "uncommitted upsert in same batch must be caught by DeleteSubtree",
        );
        assert!(after.contains("fs:/keep.txt"), "unrelated doc must remain");
    }

    #[tokio::test]
    async fn writer_subtree_delete_tombstones_later_upserts_in_same_batch() {
        let (_tmp, tx, search, _handle) = fresh_writer_service();
        tx.send(Mutation::UpsertMany(vec![
            make_fs_doc("fs:/d/old.txt"),
            make_fs_doc("fs:/keep.txt"),
        ]))
        .await
        .unwrap();
        let _ = tx.commit_now().await.unwrap();

        // Upsert queued AFTER the subtree delete but applied within
        // the same uncommitted batch: the tombstone must drop it,
        // otherwise it re-creates an orphan under the deleted prefix.
        tx.send(Mutation::DeleteSubtree("fs:/d".into()))
            .await
            .unwrap();
        tx.send(Mutation::Upsert(Box::new(make_fs_doc("fs:/d/orphan.txt"))))
            .await
            .unwrap();
        tx.send(Mutation::UpsertMany(vec![make_fs_doc(
            "fs:/d/sub/orphan2.txt",
        )]))
        .await
        .unwrap();
        let _ = tx.commit_now().await.unwrap();

        let after = search.all_doc_ids().await.unwrap();
        assert!(
            !after.contains("fs:/d/old.txt"),
            "committed doc under prefix must be purged"
        );
        assert!(
            !after.contains("fs:/d/orphan.txt"),
            "upsert processed after DeleteSubtree in the same batch must be dropped",
        );
        assert!(
            !after.contains("fs:/d/sub/orphan2.txt"),
            "upsert_many processed after DeleteSubtree in the same batch must be dropped",
        );
        assert!(after.contains("fs:/keep.txt"), "unrelated doc must remain");

        // Tombstones are cleared once the batch commits: a genuine
        // re-creation of the subtree in a later batch must index.
        tx.send(Mutation::Upsert(Box::new(make_fs_doc("fs:/d/reborn.txt"))))
            .await
            .unwrap();
        let _ = tx.commit_now().await.unwrap();
        let later = search.all_doc_ids().await.unwrap();
        assert!(
            later.contains("fs:/d/reborn.txt"),
            "tombstone must not outlive the commit that made the delete durable",
        );
    }

    #[tokio::test]
    async fn writer_upsert_body_skips_when_committed_doc_newer_than_snapshot() {
        let (_tmp, tx, search, _handle) = fresh_writer_service();
        let mut doc = make_fs_doc("fs:/scan.png");
        doc.mtime = 100;
        tx.send(Mutation::Upsert(Box::new(doc))).await.unwrap();
        let _ = tx.commit_now().await.unwrap();

        // Snapshot older than the indexed doc: the OCR text belongs
        // to bytes that were since reindexed — must be dropped.
        tx.send(Mutation::UpsertBody {
            doc_id: "fs:/scan.png".into(),
            body: "stale ocr".into(),
            expected_mtime: Some(50),
        })
        .await
        .unwrap();
        let _ = tx.commit_now().await.unwrap();
        assert_eq!(
            search.get_body("fs:/scan.png").await.unwrap(),
            None,
            "write-back with an older snapshot must be skipped",
        );

        // Snapshot matching the indexed doc: body lands.
        tx.send(Mutation::UpsertBody {
            doc_id: "fs:/scan.png".into(),
            body: "fresh ocr".into(),
            expected_mtime: Some(100),
        })
        .await
        .unwrap();
        let _ = tx.commit_now().await.unwrap();
        assert_eq!(
            search.get_body("fs:/scan.png").await.unwrap().as_deref(),
            Some("fresh ocr"),
        );
    }

    #[tokio::test]
    async fn writer_upsert_body_skips_when_newer_upsert_staged_in_same_batch() {
        let (_tmp, tx, search, _handle) = fresh_writer_service();
        let mut doc = make_fs_doc("fs:/scan.png");
        doc.mtime = 100;
        tx.send(Mutation::Upsert(Box::new(doc))).await.unwrap();
        let _ = tx.commit_now().await.unwrap();

        // A newer full upsert is staged (uncommitted) when the OCR
        // write-back arrives. The committed reader still shows
        // mtime=100, so only the staged-batch check can catch this;
        // without it the read-modify-write clobbers the new doc.
        let mut newer = make_fs_doc("fs:/scan.png");
        newer.mtime = 200;
        newer.body = Some("fresh extract".into());
        tx.send(Mutation::Upsert(Box::new(newer))).await.unwrap();
        tx.send(Mutation::UpsertBody {
            doc_id: "fs:/scan.png".into(),
            body: "ocr of old bytes".into(),
            expected_mtime: Some(100),
        })
        .await
        .unwrap();
        let _ = tx.commit_now().await.unwrap();

        assert_eq!(
            search.get_body("fs:/scan.png").await.unwrap().as_deref(),
            Some("fresh extract"),
            "staged newer upsert must win over the stale OCR write-back",
        );
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
