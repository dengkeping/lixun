//! Filesystem watcher pipeline (Phase B of watcher-fix-v2).
//!
//! Pipeline:
//!   notify thread (owns RecommendedWatcher, drains control commands, forwards
//!   events via try_send) → coalescer tokio task (HashMap<Path,Intent>, flushes
//!   every 3s) → resolver worker tokio tasks (stat+extract, emit Mutations) →
//!   IndexService (from index_service.rs).
//!
//! Initial crawl uses walkdir with the same exclude list as the indexer,
//! installs per-directory NonRecursive watches, tolerates EACCES/ENOENT.
//! New directories discovered at runtime are scanned (to hydrate git-clone-
//! style bursts) and watched.

use crate::index_service::{IndexMutationTx, Mutation, fs_doc_id, index_file};
use anyhow::Result;
use lixun_extract::ExtractorCapabilities;
use lixun_sources::exclude::path_excluded;
use lixun_sources::{HasBody, OcrEnqueue, SymlinkAliasNoter};
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;
use walkdir::WalkDir;

struct ExcludeSet {
    subs: Vec<String>,
    regexes: Vec<regex::Regex>,
}

impl ExcludeSet {
    fn matches(&self, path: &Path) -> bool {
        path_excluded(path, &self.subs, &self.regexes)
    }
}

const COALESCE_FLUSH_INTERVAL: Duration = Duration::from_secs(3);
const RESOLVER_WORKERS: usize = 2;
const RAW_EVENT_QUEUE_CAP: usize = 8192;
const REFRESH_QUEUE_CAP: usize = 1024;
const CONTROL_QUEUE_CAP: usize = 256;

#[derive(Debug, PartialEq, Eq)]
enum RawEvent {
    Upsert(PathBuf),
    Delete(PathBuf),
}

enum Control {
    AddWatch(PathBuf),
}

enum RefreshJob {
    Refresh(PathBuf),
    /// Purge a doc-id and any descendant ids. Sole shape emitted for
    /// `Delete` intents and `Resolved::Gone` paths because the watcher
    /// cannot stat a vanished path to decide file-vs-directory.
    /// `DeleteSubtree(fs_doc_id)` of a file-path strictly generalises
    /// a single `Delete`: prefix==id matches that one doc, no other id
    /// starts with `path + "/"` for a real file, so no false deletes.
    /// For a directory it purges the dir doc plus every descendant.
    DeleteSubtree(String),
}

static OVERFLOW_COUNT: AtomicUsize = AtomicUsize::new(0);
static OVERFLOW_FLAG: AtomicBool = AtomicBool::new(false);
static DIRS_WATCHED: AtomicUsize = AtomicUsize::new(0);
static DIRS_EXCLUDED: AtomicUsize = AtomicUsize::new(0);
static DIRS_ERRORS: AtomicUsize = AtomicUsize::new(0);

pub fn stats() -> (u64, u64, u64, u64) {
    (
        DIRS_WATCHED.load(Ordering::Relaxed) as u64,
        DIRS_EXCLUDED.load(Ordering::Relaxed) as u64,
        DIRS_ERRORS.load(Ordering::Relaxed) as u64,
        OVERFLOW_COUNT.load(Ordering::Relaxed) as u64,
    )
}

#[allow(clippy::too_many_arguments)]
pub async fn start(
    roots: Vec<PathBuf>,
    exclude: Vec<String>,
    exclude_regex: Vec<regex::Regex>,
    max_file_size_mb: u64,
    caps: Arc<ExtractorCapabilities>,
    ocr_enqueue: Option<Arc<dyn OcrEnqueue>>,
    body_checker: Option<Arc<dyn HasBody>>,
    min_image_side_px: u32,
    alias_noter: Option<Arc<dyn SymlinkAliasNoter>>,
    mutation_tx: IndexMutationTx,
) -> Result<()> {
    let (raw_tx, raw_rx) = mpsc::channel::<RawEvent>(RAW_EVENT_QUEUE_CAP);
    let (ctrl_tx, ctrl_rx) = std::sync::mpsc::sync_channel::<Control>(CONTROL_QUEUE_CAP);
    let (refresh_tx, refresh_rx) = async_channel::bounded::<RefreshJob>(REFRESH_QUEUE_CAP);

    let exclude_arc: Arc<ExcludeSet> = Arc::new(ExcludeSet {
        subs: exclude,
        regexes: exclude_regex,
    });

    let nt_roots = roots.clone();
    let nt_exclude = Arc::clone(&exclude_arc);
    let nt_raw_tx = raw_tx.clone();
    std::thread::Builder::new()
        .name("lixun-notify".into())
        .spawn(move || {
            notify_thread_main(nt_roots, nt_exclude, nt_raw_tx, ctrl_rx);
        })?;

    let coalesce_exclude = Arc::clone(&exclude_arc);
    tokio::spawn(coalescer_task(
        raw_rx,
        refresh_tx.clone(),
        coalesce_exclude,
        roots.clone(),
    ));

    for worker_id in 0..RESOLVER_WORKERS {
        let rx = refresh_rx.clone();
        let mutation_tx = mutation_tx.clone();
        let ctrl_tx = ctrl_tx.clone();
        let exclude = Arc::clone(&exclude_arc);
        let refresh_tx = refresh_tx.clone();
        let env = ResolverEnv {
            exclude,
            max_file_size_mb,
            caps: Arc::clone(&caps),
            ocr_enqueue: ocr_enqueue.clone(),
            body_checker: body_checker.clone(),
            min_image_side_px,
            alias_noter: alias_noter.clone(),
        };
        tokio::spawn(resolver_task(
            worker_id,
            rx,
            mutation_tx,
            ctrl_tx,
            refresh_tx,
            env,
        ));
    }

    Ok(())
}

fn notify_thread_main(
    roots: Vec<PathBuf>,
    exclude: Arc<ExcludeSet>,
    raw_tx: mpsc::Sender<RawEvent>,
    ctrl_rx: std::sync::mpsc::Receiver<Control>,
) {
    let raw_tx_cb = raw_tx.clone();
    let watcher_res = RecommendedWatcher::new(
        move |res: notify::Result<Event>| {
            let event = match res {
                Ok(event) => event,
                Err(e) => {
                    tracing::error!("notify watcher error (events may be lost): {}", e);
                    return;
                }
            };
            dispatch_notify_event(&raw_tx_cb, event);
        },
        Config::default(),
    );
    let mut watcher = match watcher_res {
        Ok(w) => w,
        Err(e) => {
            tracing::error!("notify thread: failed to create watcher: {}", e);
            return;
        }
    };

    let (watched, excluded, errors) = initial_crawl(&mut watcher, &roots, &exclude);
    DIRS_WATCHED.store(watched, Ordering::Relaxed);
    DIRS_EXCLUDED.store(excluded, Ordering::Relaxed);
    DIRS_ERRORS.store(errors, Ordering::Relaxed);
    tracing::info!(
        "File watcher: watching {} directories across {} roots (excluded {}, errors {})",
        watched,
        roots.len(),
        excluded,
        errors
    );

    loop {
        match ctrl_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Control::AddWatch(path)) => {
                if exclude.matches(&path) {
                    DIRS_EXCLUDED.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                match watcher.watch(&path, RecursiveMode::NonRecursive) {
                    Ok(()) => {
                        DIRS_WATCHED.fetch_add(1, Ordering::Relaxed);
                        tracing::debug!("notify thread: added watch {:?}", path);
                    }
                    Err(e) => {
                        DIRS_ERRORS.fetch_add(1, Ordering::Relaxed);
                        tracing::debug!("notify thread: add watch {:?} failed: {}", path, e);
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if raw_tx.is_closed() {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    tracing::info!("notify thread: exiting");
}

fn initial_crawl(
    watcher: &mut RecommendedWatcher,
    roots: &[PathBuf],
    exclude: &ExcludeSet,
) -> (usize, usize, usize) {
    let mut watched = 0usize;
    let mut excluded = 0usize;
    let mut errors = 0usize;

    for root in roots {
        for entry in WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                if exclude.matches(e.path()) {
                    excluded += 1;
                    return false;
                }
                true
            })
        {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => {
                    errors += 1;
                    continue;
                }
            };
            if !entry.file_type().is_dir() {
                continue;
            }
            match watcher.watch(entry.path(), RecursiveMode::NonRecursive) {
                Ok(()) => watched += 1,
                Err(_) => errors += 1,
            }
        }
    }

    (watched, excluded, errors)
}

fn dispatch_notify_event(raw_tx: &mpsc::Sender<RawEvent>, event: Event) {
    let intents: Vec<RawEvent> = match event.kind {
        EventKind::Remove(_) => event.paths.into_iter().map(RawEvent::Delete).collect(),
        EventKind::Modify(notify::event::ModifyKind::Name(notify::event::RenameMode::From)) => {
            event.paths.into_iter().map(RawEvent::Delete).collect()
        }
        EventKind::Modify(notify::event::ModifyKind::Name(notify::event::RenameMode::To)) => {
            event.paths.into_iter().map(RawEvent::Upsert).collect()
        }
        EventKind::Modify(notify::event::ModifyKind::Name(notify::event::RenameMode::Both))
            if event.paths.len() == 2 =>
        {
            let mut paths = event.paths.into_iter();
            let from = paths.next().expect("len checked == 2");
            let to = paths.next().expect("len checked == 2");
            vec![RawEvent::Delete(from), RawEvent::Upsert(to)]
        }
        EventKind::Create(_) | EventKind::Modify(_) => {
            event.paths.into_iter().map(RawEvent::Upsert).collect()
        }
        _ => Vec::new(),
    };

    for ev in intents {
        if let Err(mpsc::error::TrySendError::Full(_)) = raw_tx.try_send(ev) {
            OVERFLOW_FLAG.store(true, Ordering::Relaxed);
            OVERFLOW_COUNT.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Intent {
    Refresh,
    Delete,
}

async fn coalescer_task(
    mut raw_rx: mpsc::Receiver<RawEvent>,
    refresh_tx: async_channel::Sender<RefreshJob>,
    exclude: Arc<ExcludeSet>,
    roots: Vec<PathBuf>,
) {
    let mut pending: HashMap<PathBuf, Intent> = HashMap::new();
    let mut flush_tick = tokio::time::interval(COALESCE_FLUSH_INTERVAL);
    flush_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            biased;

            maybe_ev = raw_rx.recv() => {
                let Some(ev) = maybe_ev else { break };
                match ev {
                    RawEvent::Upsert(p) => {
                        if !exclude.matches(&p) {
                            pending.insert(p, Intent::Refresh);
                        }
                    }
                    RawEvent::Delete(p) => {
                        if !exclude.matches(&p) {
                            pending.insert(p, Intent::Delete);
                        }
                    }
                }
            }

            _ = flush_tick.tick() => {
                // Overflow recovery FIRST (and independent of whether any
                // events survived): a full raw queue means events were
                // dropped at the notify boundary, so the per-path intents
                // below are incomplete. Requeue a walk of every root —
                // coarse, but it degrades granularity instead of
                // correctness; a silently stale index was the old failure
                // mode. Resolver-side directory expansion re-enqueues one
                // Refresh per file, and the extract cache absorbs the
                // re-extraction cost of unchanged files.
                if OVERFLOW_FLAG.swap(false, Ordering::Relaxed) {
                    tracing::warn!(
                        "watcher: raw event queue overflowed (total: {}); rescanning {} root(s) to recover dropped events",
                        OVERFLOW_COUNT.load(Ordering::Relaxed),
                        roots.len()
                    );
                    for root in &roots {
                        if refresh_tx.send(RefreshJob::Refresh(root.clone())).await.is_err() {
                            return;
                        }
                    }
                }

                if pending.is_empty() {
                    continue;
                }
                let snapshot = std::mem::take(&mut pending);
                let count = snapshot.len();
                let surviving = drop_descendant_deletes(snapshot);
                for (path, intent) in surviving {
                    let job = match intent {
                        Intent::Refresh => RefreshJob::Refresh(path),
                        Intent::Delete => RefreshJob::DeleteSubtree(fs_doc_id(&path)),
                    };
                    if refresh_tx.send(job).await.is_err() {
                        return;
                    }
                }
                tracing::debug!("coalescer: flushed {} paths", count);
            }
        }
    }
}

fn drop_descendant_deletes(
    pending: HashMap<PathBuf, Intent>,
) -> Vec<(PathBuf, Intent)> {
    let mut delete_paths: Vec<PathBuf> = pending
        .iter()
        .filter(|(_, i)| matches!(i, Intent::Delete))
        .map(|(p, _)| p.clone())
        .collect();
    delete_paths.sort_by_key(|p| p.as_os_str().len());
    let mut covered: Vec<PathBuf> = Vec::with_capacity(delete_paths.len());
    let mut redundant: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    for p in delete_paths {
        if covered.iter().any(|parent| is_descendant(&p, parent)) {
            redundant.insert(p);
        } else {
            covered.push(p);
        }
    }
    pending
        .into_iter()
        .filter(|(p, i)| !(matches!(i, Intent::Delete) && redundant.contains(p)))
        .collect()
}

fn is_descendant(child: &Path, parent: &Path) -> bool {
    if child == parent {
        return false;
    }
    child.starts_with(parent)
}

struct ResolverEnv {
    exclude: Arc<ExcludeSet>,
    max_file_size_mb: u64,
    caps: Arc<ExtractorCapabilities>,
    ocr_enqueue: Option<Arc<dyn OcrEnqueue>>,
    body_checker: Option<Arc<dyn HasBody>>,
    min_image_side_px: u32,
    alias_noter: Option<Arc<dyn SymlinkAliasNoter>>,
}

async fn resolver_task(
    worker_id: usize,
    rx: async_channel::Receiver<RefreshJob>,
    mutation_tx: IndexMutationTx,
    ctrl_tx: std::sync::mpsc::SyncSender<Control>,
    refresh_tx: async_channel::Sender<RefreshJob>,
    env: ResolverEnv,
) {
    while let Ok(job) = rx.recv().await {
        match job {
            RefreshJob::DeleteSubtree(prefix) => {
                if let Err(e) = mutation_tx
                    .send(Mutation::DeleteSubtree(prefix.clone()))
                    .await
                {
                    tracing::debug!(
                        "resolver[{}]: send delete_subtree {} failed: {}",
                        worker_id,
                        prefix,
                        e
                    );
                }
            }
            RefreshJob::Refresh(path) => {
                let exclude = Arc::clone(&env.exclude);
                let path_blocking = path.clone();
                let caps_b = Arc::clone(&env.caps);
                let enq_b = env.ocr_enqueue.clone();
                let body_b = env.body_checker.clone();
                let alias_b = env.alias_noter.clone();
                let max_size = env.max_file_size_mb;
                let min_side = env.min_image_side_px;
                let result = tokio::task::spawn_blocking(move || {
                    resolve_refresh(
                        &path_blocking,
                        &exclude,
                        max_size,
                        &caps_b,
                        enq_b.as_ref().map(|a| a.as_ref() as &dyn OcrEnqueue),
                        body_b.as_ref().map(|a| a.as_ref() as &dyn HasBody),
                        min_side,
                        alias_b
                            .as_ref()
                            .map(|a| a.as_ref() as &dyn SymlinkAliasNoter),
                    )
                })
                .await;
                let resolved = match result {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::debug!("resolver[{}]: blocking task panicked: {}", worker_id, e);
                        continue;
                    }
                };
                match resolved {
                    Resolved::File(doc) => {
                        if let Err(e) = mutation_tx.send(Mutation::Upsert(doc)).await {
                            tracing::debug!("resolver[{}]: send upsert failed: {}", worker_id, e);
                        }
                    }
                    Resolved::Directory {
                        subtree_files,
                        subdirs,
                    } => {
                        for dir in subdirs {
                            let _ = ctrl_tx.try_send(Control::AddWatch(dir));
                        }
                        for f in subtree_files {
                            if refresh_tx.send(RefreshJob::Refresh(f)).await.is_err() {
                                break;
                            }
                        }
                    }
                    Resolved::Gone => {
                        // Key convention: the alias map is keyed on
                        // the raw observed path string, the same
                        // value `fs_doc_id` falls back to when
                        // `canonicalize` fails on a vanished path.
                        // If a prior index_file recorded an alias,
                        // recover the canonical id and forget the
                        // entry; otherwise fall back to the raw id.
                        let observed = path.to_string_lossy().to_string();
                        let id = match env.alias_noter.as_ref() {
                            Some(noter) => match noter.resolve(&observed) {
                                Some(canonical) => {
                                    noter.forget(&canonical);
                                    canonical
                                }
                                None => fs_doc_id(&path),
                            },
                            None => fs_doc_id(&path),
                        };
                        if let Err(e) = mutation_tx
                            .send(Mutation::DeleteSubtree(id.clone()))
                            .await
                        {
                            tracing::debug!(
                                "resolver[{}]: send gone-delete-subtree {} failed: {}",
                                worker_id,
                                id,
                                e
                            );
                        }
                    }
                    Resolved::Skip => {}
                }
            }
        }
    }
}

enum Resolved {
    File(Box<lixun_core::Document>),
    Directory {
        subtree_files: Vec<PathBuf>,
        subdirs: Vec<PathBuf>,
    },
    Gone,
    Skip,
}

#[allow(clippy::too_many_arguments)]
fn resolve_refresh(
    path: &Path,
    exclude: &ExcludeSet,
    max_file_size_mb: u64,
    caps: &ExtractorCapabilities,
    ocr_enqueue: Option<&dyn OcrEnqueue>,
    body_checker: Option<&dyn HasBody>,
    min_image_side_px: u32,
    alias_noter: Option<&dyn SymlinkAliasNoter>,
) -> Resolved {
    let Ok(meta) = std::fs::metadata(path) else {
        return Resolved::Gone;
    };
    if exclude.matches(path) {
        return Resolved::Skip;
    }
    if meta.is_file() {
        match index_file(
            path,
            max_file_size_mb,
            caps,
            ocr_enqueue,
            body_checker,
            min_image_side_px,
        ) {
            Ok(doc) => {
                // When `path` traversed a symlink, `index_file`
                // canonicalises and produces a doc-id that differs
                // from the raw-path id the Gone arm would compute on
                // a future delete. Record the mapping so the delete
                // can recover the canonical id. Key on the raw
                // observed path string (not the `fs:`-prefixed id)
                // to match the Gone-arm lookup.
                if let Some(noter) = alias_noter {
                    let observed = path.to_string_lossy();
                    let observed_id = format!("fs:{observed}");
                    if observed_id != doc.id.0 {
                        noter.note(&observed, &doc.id.0);
                    }
                }
                Resolved::File(Box::new(doc))
            }
            Err(_) => Resolved::Skip,
        }
    } else if meta.is_dir() {
        let mut subtree_files = Vec::new();
        let mut subdirs = Vec::new();
        for entry in WalkDir::new(path)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| !exclude.matches(e.path()))
            .flatten()
        {
            if entry.file_type().is_dir() {
                subdirs.push(entry.path().to_path_buf());
            } else if entry.file_type().is_file() {
                subtree_files.push(entry.path().to_path_buf());
            }
        }
        Resolved::Directory {
            subtree_files,
            subdirs,
        }
    } else {
        Resolved::Skip
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{ModifyKind, RenameMode};

    fn drain(event: Event) -> Vec<RawEvent> {
        let (tx, mut rx) = mpsc::channel(64);
        dispatch_notify_event(&tx, event);
        drop(tx);
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    #[test]
    fn rename_both_two_paths_maps_delete_then_upsert() {
        let from = PathBuf::from("/tmp/old.txt");
        let to = PathBuf::from("/tmp/new.txt");
        let event = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
            .add_path(from.clone())
            .add_path(to.clone());
        assert_eq!(
            drain(event),
            vec![RawEvent::Delete(from), RawEvent::Upsert(to)]
        );
    }

    #[test]
    fn dispatch_remove_directory_event_yields_subtree_delete() {
        // The watcher cannot stat a vanished path; every Delete intent
        // is emitted as DeleteSubtree at the resolver boundary. Here we
        // verify the coalescer flush translation: a Delete intent for
        // a directory-shaped path becomes RefreshJob::DeleteSubtree.
        let dir = PathBuf::from("/tmp/lixun-watcher-test-dir");
        let mut pending: HashMap<PathBuf, Intent> = HashMap::new();
        pending.insert(dir.clone(), Intent::Delete);
        let surviving = drop_descendant_deletes(pending);
        assert_eq!(surviving.len(), 1);
        let (path, intent) = &surviving[0];
        assert_eq!(path, &dir);
        assert!(matches!(intent, Intent::Delete));
        let job = match intent {
            Intent::Refresh => RefreshJob::Refresh(path.clone()),
            Intent::Delete => RefreshJob::DeleteSubtree(fs_doc_id(path)),
        };
        assert!(matches!(job, RefreshJob::DeleteSubtree(_)));
        if let RefreshJob::DeleteSubtree(prefix) = job {
            assert_eq!(prefix, fs_doc_id(&dir));
        }
    }

    #[test]
    fn coalescer_drops_child_prefix_covered_by_parent() {
        let parent = PathBuf::from("/tmp/lixun-x/a/b");
        let child_file = PathBuf::from("/tmp/lixun-x/a/b/c.txt");
        let child_dir = PathBuf::from("/tmp/lixun-x/a/b/sub");
        let unrelated = PathBuf::from("/tmp/lixun-x/other.txt");
        let mut pending: HashMap<PathBuf, Intent> = HashMap::new();
        pending.insert(parent.clone(), Intent::Delete);
        pending.insert(child_file.clone(), Intent::Delete);
        pending.insert(child_dir.clone(), Intent::Delete);
        pending.insert(unrelated.clone(), Intent::Delete);
        let surviving = drop_descendant_deletes(pending);
        let surviving_paths: std::collections::HashSet<PathBuf> =
            surviving.into_iter().map(|(p, _)| p).collect();
        assert!(surviving_paths.contains(&parent), "parent must survive");
        assert!(
            surviving_paths.contains(&unrelated),
            "unrelated sibling must survive"
        );
        assert!(
            !surviving_paths.contains(&child_file),
            "file under parent must be dropped"
        );
        assert!(
            !surviving_paths.contains(&child_dir),
            "subdir under parent must be dropped"
        );
        assert_eq!(surviving_paths.len(), 2);
    }

    #[test]
    fn rename_both_unexpected_path_count_degrades_to_upsert() {
        // A malformed Both event must never emit a Delete (which would target
        // the wrong document); it degrades to a conservative all-upsert.
        let single = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
            .add_path(PathBuf::from("/tmp/only.txt"));
        assert_eq!(
            drain(single),
            vec![RawEvent::Upsert(PathBuf::from("/tmp/only.txt"))]
        );

        let triple = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
            .add_path(PathBuf::from("/tmp/a.txt"))
            .add_path(PathBuf::from("/tmp/b.txt"))
            .add_path(PathBuf::from("/tmp/c.txt"));
        let out = drain(triple);
        assert!(out.iter().all(|e| matches!(e, RawEvent::Upsert(_))));
        assert_eq!(out.len(), 3);
    }
}
