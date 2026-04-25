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
use lixun_sources::OcrEnqueue;
use lixun_sources::exclude::path_excluded;
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

enum RawEvent {
    Upsert(PathBuf),
    Delete(PathBuf),
}

enum Control {
    AddWatch(PathBuf),
}

enum RefreshJob {
    Refresh(PathBuf),
    Delete(String),
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

pub async fn start(
    roots: Vec<PathBuf>,
    exclude: Vec<String>,
    exclude_regex: Vec<regex::Regex>,
    max_file_size_mb: u64,
    caps: Arc<ExtractorCapabilities>,
    ocr_enqueue: Option<Arc<dyn OcrEnqueue>>,
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
    tokio::spawn(coalescer_task(raw_rx, refresh_tx.clone(), coalesce_exclude));

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
        };
        tokio::spawn(resolver_task(worker_id, rx, mutation_tx, ctrl_tx, refresh_tx, env));
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
            let Ok(event) = res else { return };
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
        EventKind::Modify(notify::event::ModifyKind::Name(notify::event::RenameMode::Both)) => {
            let mut v = Vec::with_capacity(event.paths.len());
            for (i, p) in event.paths.into_iter().enumerate() {
                if i == 0 {
                    v.push(RawEvent::Delete(p));
                } else {
                    v.push(RawEvent::Upsert(p));
                }
            }
            v
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
                        pending.insert(p, Intent::Delete);
                    }
                }
            }

            _ = flush_tick.tick() => {
                if pending.is_empty() {
                    continue;
                }
                let snapshot = std::mem::take(&mut pending);
                let count = snapshot.len();
                for (path, intent) in snapshot {
                    let job = match intent {
                        Intent::Refresh => RefreshJob::Refresh(path),
                        Intent::Delete => RefreshJob::Delete(fs_doc_id(&path)),
                    };
                    if refresh_tx.send(job).await.is_err() {
                        return;
                    }
                }
                tracing::debug!("coalescer: flushed {} paths", count);

                if OVERFLOW_FLAG.swap(false, Ordering::Relaxed) {
                    tracing::warn!(
                        "watcher: raw event queue overflowed (total: {})",
                        OVERFLOW_COUNT.load(Ordering::Relaxed)
                    );
                }
            }
        }
    }
}

struct ResolverEnv {
    exclude: Arc<ExcludeSet>,
    max_file_size_mb: u64,
    caps: Arc<ExtractorCapabilities>,
    ocr_enqueue: Option<Arc<dyn OcrEnqueue>>,
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
            RefreshJob::Delete(id) => {
                if let Err(e) = mutation_tx.send(Mutation::Delete(id.clone())).await {
                    tracing::debug!("resolver[{}]: send delete {} failed: {}", worker_id, id, e);
                }
            }
            RefreshJob::Refresh(path) => {
                let exclude = Arc::clone(&env.exclude);
                let path_blocking = path.clone();
                let caps_b = Arc::clone(&env.caps);
                let enq_b = env.ocr_enqueue.clone();
                let max_size = env.max_file_size_mb;
                let result = tokio::task::spawn_blocking(move || {
                    resolve_refresh(
                        &path_blocking,
                        &exclude,
                        max_size,
                        &caps_b,
                        enq_b.as_ref().map(|a| a.as_ref() as &dyn OcrEnqueue),
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
                            tracing::debug!(
                                "resolver[{}]: send upsert failed: {}",
                                worker_id,
                                e
                            );
                        }
                    }
                    Resolved::Directory { subtree_files, subdirs } => {
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
                        let id = fs_doc_id(&path);
                        if let Err(e) = mutation_tx.send(Mutation::Delete(id.clone())).await {
                            tracing::debug!(
                                "resolver[{}]: send gone-delete {} failed: {}",
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

fn resolve_refresh(
    path: &Path,
    exclude: &ExcludeSet,
    max_file_size_mb: u64,
    caps: &ExtractorCapabilities,
    ocr_enqueue: Option<&dyn OcrEnqueue>,
) -> Resolved {
    let Ok(meta) = std::fs::metadata(path) else {
        return Resolved::Gone;
    };
    if exclude.matches(path) {
        return Resolved::Skip;
    }
    if meta.is_file() {
        match index_file(path, max_file_size_mb, caps, ocr_enqueue) {
            Ok(doc) => Resolved::File(Box::new(doc)),
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
