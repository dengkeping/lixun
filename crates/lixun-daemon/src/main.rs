//! lixund — Lixun daemon: IPC server, indexer, filesystem watcher.

#[cfg(target_os = "linux")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use bytes::{BufMut, BytesMut};
use chrono::Utc;
use futures::StreamExt;
use lixun_ipc::gui::{GuiCommand, GuiResponse};
use lixun_ipc::{
    ImpactProfileWire, MIN_PROTOCOL_VERSION, PROTOCOL_VERSION, Request, Response, socket_path,
};
use lixun_sources::QueryContext;
use std::os::unix::io::AsRawFd;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, RwLock};
use tokio_util::sync::CancellationToken;

mod battery;
mod frecency;
mod query_latch;
mod query_log;
mod sched;
mod top_hit;
use frecency::FrecencyStore;
use query_latch::QueryLatchStore;
use query_log::QueryLog;

use lixun_daemon::config;
use lixun_daemon::gui_control::GuiControl;
use lixun_daemon::hotkeys;
use lixun_daemon::index_service::{self, IndexMutationTx, SearchHandle};
use lixun_daemon::indexer;
use lixun_daemon::preview_spawn::PreviewSpawner;

use lixun_indexer::plugin_fs_watcher;
use lixun_indexer::watcher;

use lixun_fusion::HybridSearchHandle;

/// IPC-facing search surface. The daemon wraps the lexical
/// [`SearchHandle`] in [`HybridSearchHandle`] so RRF fusion runs
/// transparently on every query when the registry exposes an ANN
/// handle; otherwise the hybrid handle collapses to a thin
/// pass-through. Both modes expose byte-identical
/// `search` / `search_with_breakdown` / `all_doc_ids` / `has_body`
/// / `get_body` / `hydrate_doc` signatures, so call sites stay
/// uniform (DB-3).
type SearchSurface = HybridSearchHandle;

#[derive(Debug, Clone, Default)]
struct IndexStats {
    indexed_docs: u64,
    last_reindex: Option<chrono::DateTime<Utc>>,
    reindex_in_progress: bool,
    reindex_started: Option<chrono::DateTime<Utc>>,
}

/// Map a transport `Result<GuiResponse>` to the daemon's public
/// `Response::Visibility` wire type. Transport errors and semantic
/// errors both surface as `Response::Error`.
/// Clamp the Stage-2 score multiplier (frecency × latch) so no
/// single hit can accumulate unbounded score growth from long-lived
/// click history or exact-query latch streaks. `cap` is
/// `RankingConfig.total_multiplier_cap` (default 6.0).
fn clamp_stage2_mult(stage2: f32, cap: f32) -> f32 {
    stage2.min(cap)
}

/// Format a single hit's score breakdown as a one-line string for
/// `Response::HitsWithExtras{,V3}.explanations`. Emitted only when
/// the CLI invokes `lixun search --explain` (T6). The format is
/// `score=<final> = tantivy(<t>) × cat(<c>) × prefix(<p>) × acronym(<a>) × recency(<r>) × coord(<co>) × stage2(<s>)`
/// so each multiplier is visible alongside the final ranking score.
fn format_breakdown(b: &lixun_core::ScoreBreakdown, _h: &lixun_core::Hit) -> String {
    format!(
        "score={:.4} = tantivy({:.4}) × cat({:.3}) × exact({:.3}) × prefix({:.3}) × acronym({:.3}) × recency({:.3}) × coord({:.3}) × stage2({:.3})",
        b.final_score,
        b.tantivy,
        b.category_mult,
        b.exact_title_mult,
        b.prefix_mult,
        b.acronym_mult,
        b.recency_mult,
        b.coord_mult,
        b.stage2_clamped,
    )
}

/// Total-order comparator for the final hit sort: descending by
/// score, with a deterministic tiebreaker on `doc_id.0` ascending.
/// Without the tiebreaker, near-ties (or NaN scores) produce
/// non-deterministic order, which propagates into Top Hit selection
/// because `select_top_hit` reads `hits[0]` and `hits[1]` directly.
fn compare_hits_for_ranking(a: &lixun_core::Hit, b: &lixun_core::Hit) -> std::cmp::Ordering {
    b.score
        .partial_cmp(&a.score)
        .unwrap_or(std::cmp::Ordering::Equal)
        .then_with(|| a.id.0.cmp(&b.id.0))
}

/// Compare two [`ImpactProfile`]s and split changed knob names into
/// the hot subset (re-applied immediately) and the cold subset
/// (require daemon restart). The split mirrors plan §5.6: every
/// `daemon_nice` and every `ocr_*` knob is hot; everything else is
/// cold. Returns `(applied_hot, requires_restart)`. Both vectors
/// are empty when the new profile equals the old one.
fn impact_diff_hot_cold(
    old: &lixun_core::ImpactProfile,
    new: &lixun_core::ImpactProfile,
) -> (Vec<String>, Vec<String>) {
    let mut hot = Vec::new();
    let mut cold = Vec::new();
    if old.daemon_nice != new.daemon_nice {
        hot.push("daemon_nice".into());
    }
    if old.ocr_jobs_per_tick != new.ocr_jobs_per_tick {
        hot.push("ocr_jobs_per_tick".into());
    }
    if old.ocr_adaptive_throttle != new.ocr_adaptive_throttle {
        hot.push("ocr_adaptive_throttle".into());
    }
    if old.ocr_nice_level != new.ocr_nice_level {
        hot.push("ocr_nice_level".into());
    }
    if old.ocr_io_class_idle != new.ocr_io_class_idle {
        hot.push("ocr_io_class_idle".into());
    }
    if old.ocr_worker_interval != new.ocr_worker_interval {
        hot.push("ocr_worker_interval".into());
    }
    if old.tokio_worker_threads != new.tokio_worker_threads {
        cold.push("tokio_worker_threads".into());
    }
    if old.onnx_intra_threads != new.onnx_intra_threads {
        cold.push("onnx_intra_threads".into());
    }
    if old.onnx_inter_threads != new.onnx_inter_threads {
        cold.push("onnx_inter_threads".into());
    }
    if old.rayon_threads != new.rayon_threads {
        cold.push("rayon_threads".into());
    }
    if old.tantivy_heap_bytes != new.tantivy_heap_bytes {
        cold.push("tantivy_heap_bytes".into());
    }
    if old.tantivy_num_threads != new.tantivy_num_threads {
        cold.push("tantivy_num_threads".into());
    }
    if old.embed_batch_hint != new.embed_batch_hint {
        cold.push("embed_batch_hint".into());
    }
    if old.embed_concurrency_hint != new.embed_concurrency_hint {
        cold.push("embed_concurrency_hint".into());
    }
    if old.extract_cache_max_bytes != new.extract_cache_max_bytes {
        cold.push("extract_cache_max_bytes".into());
    }
    if old.max_file_size_bytes != new.max_file_size_bytes {
        cold.push("max_file_size_bytes".into());
    }
    if old.gloda_batch_size != new.gloda_batch_size {
        cold.push("gloda_batch_size".into());
    }
    if old.daemon_sched_idle != new.daemon_sched_idle {
        cold.push("daemon_sched_idle".into());
    }
    (hot, cold)
}

fn gui_result_to_response(r: anyhow::Result<GuiResponse>) -> Response {
    match r {
        Ok(GuiResponse::Ok { visible }) => Response::Visibility { visible },
        Ok(GuiResponse::Error(msg)) => Response::Error(format!("gui: {msg}")),
        Err(e) => Response::Error(format!("gui_control: {e}")),
    }
}

fn pid_path() -> std::path::PathBuf {
    let runtime = dirs::runtime_dir().unwrap_or_else(|| {
        std::path::PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() }))
    });
    runtime.join("lixun.pid")
}

fn try_single_instance() -> Result<std::fs::File> {
    let path = pid_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&path)?;

    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if ret != 0 {
        anyhow::bail!(
            "another instance of lixund is already running (pid file: {:?})",
            path
        );
    }

    use std::io::Write;
    let _ = file.set_len(0);
    let _ = writeln!(&file, "{}", std::process::id());
    Ok(file)
}

fn main() -> Result<()> {
    let env_filter = match std::env::var("RUST_LOG") {
        Ok(raw) if !raw.trim().is_empty() => tracing_subscriber::EnvFilter::new(raw),
        _ => tracing_subscriber::EnvFilter::new("lixun=info"),
    };
    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    tracing::info!("lixund starting...");

    let _lock = try_single_instance()?;

    let config = config::Config::load()?;
    let initial_profile = config.resolved_profile();
    let profile_swap: Arc<ArcSwap<lixun_core::ImpactProfile>> =
        Arc::new(ArcSwap::from_pointee(initial_profile.clone()));
    let profile = Arc::new(initial_profile);

    sched::apply_profile(&profile);

    tracing::info!(
        "applied impact profile level={:?} tokio_workers={} tantivy_heap_mb={} rayon_threads={}",
        profile.level,
        profile.tokio_worker_threads,
        profile.tantivy_heap_bytes / (1024 * 1024),
        profile.rayon_threads,
    );
    tracing::debug!("impact profile resolved: {:?}", profile);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(profile.tokio_worker_threads.max(1))
        .enable_all()
        .build()?;

    rt.block_on(async_main(config, profile, profile_swap))
}

async fn async_main(
    config: config::Config,
    profile: Arc<lixun_core::ImpactProfile>,
    profile_swap: Arc<ArcSwap<lixun_core::ImpactProfile>>,
) -> Result<()> {
    tracing::info!("Config loaded: roots={:?}", config.roots);

    let mut extract_caps = lixun_extract::ExtractorCapabilities::probe(
        std::time::Duration::from_secs(config.extractor_timeout_secs),
    );
    extract_caps.ocr_enabled = config.ocr.enabled;
    lixun_extract::init_capabilities(extract_caps.clone());

    // Ensure the state directory exists on disk before any subsystem
    // opens a sqlite file or loads a store under it. The ocr queue
    // below is the first consumer; `FrecencyStore`, `QueryLatchStore`,
    // `QueryLog`, the index dir, and the per-source state subdir all
    // expect this path to exist already. On a fresh install or in an
    // isolated sandbox, `$XDG_STATE_HOME/lixun` may be absent.
    std::fs::create_dir_all(&config.state_dir)
        .with_context(|| format!("creating state_dir {}", config.state_dir.display()))?;

    // Open the deferred-OCR queue eagerly (before the indexer or the
    // watcher boot up) so every FsSource extraction path — full
    // reindex, incremental reindex, watcher refresh — can enqueue
    // into it. If OCR is disabled or tesseract is missing, we leave
    // the enqueue sink unset and the extraction path quietly skips
    // the enqueue step.
    let ocr_queue: Option<Arc<lixun_extract::ocr_queue::OcrQueue>> =
        if config.ocr.enabled && extract_caps.has_tesseract {
            let queue_path = config.state_dir.join("ocr-queue.db");
            match lixun_extract::ocr_queue::OcrQueue::open(queue_path.clone()) {
                Ok(q) => Some(Arc::new(q)),
                Err(e) => {
                    tracing::error!(
                        "ocr queue: failed to open at {}: {e:#}; disabling deferred OCR",
                        queue_path.display()
                    );
                    None
                }
            }
        } else {
            None
        };

    let extract_caps_arc = Arc::new(extract_caps.clone());
    let _ = config.extractor_caps.set(Arc::clone(&extract_caps_arc));
    let ocr_enqueue_arc: Option<Arc<dyn lixun_sources::OcrEnqueue>> = ocr_queue
        .as_ref()
        .map(|q| Arc::new(OcrQueueEnqueuer(Arc::clone(q))) as Arc<dyn lixun_sources::OcrEnqueue>);
    if let Some(enq) = &ocr_enqueue_arc {
        let _ = config.ocr_enqueue.set(Arc::clone(enq));
    }

    // Registry build is split across the writer-service boot so the
    // daemon can install `config.body_checker` (which wraps the live
    // SearchHandle) BEFORE `FsSource` is constructed. Without this
    // split the fs source would be wired with `body_checker: None`
    // and the DB-16 OCR enqueue short-circuit would never fire.
    //
    // Step 1: register only the config-driven plugin instances so
    // `plugin_fields_by_kind` is populated for the index schema. The
    // builtin `apps` + `fs` sources carry no plugin fields, so
    // deferring their registration does not change the schema.
    let sources_state_dir = config.state_dir.join("sources");
    let mut registry = lixun_indexer::SourceRegistry::new();

    /* The semantic worker is an out-of-process sidecar. When the
    binary is on disk we spawn its supervisor before plugin
    registration so the stub factory finds an installed connection
    at build time; when it is absent we log once and continue —
    the stub plugin will simply return empty results because its
    AnnHandle never connects. */
    match lixun_daemon::semantic_supervisor::probe_worker_binary() {
        Some(path) => {
            tracing::info!(
                worker = %path.display(),
                "semantic worker probed, supervisor starting"
            );
            tokio::spawn(lixun_daemon::semantic_supervisor::supervise(path));
        }
        None => {
            tracing::info!("semantic worker binary not found, semantic plugin will be no-op");
        }
    }

    register_plugin_sources(
        &mut registry,
        &config,
        &sources_state_dir,
        Arc::clone(&profile),
    )?;

    let index_path = config.state_dir.join("index");
    let (index, rebuilt_from_scratch) = lixun_index::LixunIndex::create_or_open_with_plugins(
        index_path.to_str().unwrap(),
        &registry.plugin_fields_by_kind,
        config.ranking_config(),
    )?;

    let stats = Arc::new(RwLock::new(IndexStats::default()));
    let ocr_worker_stats = Arc::new(lixun_indexer::ocr_tick::OcrWorkerStats::default());
    let state_dir = config.state_dir.clone();
    let watcher_roots = config.roots.clone();
    let watcher_exclude = config.exclude.clone();
    let watcher_exclude_regex = config.exclude_regex.clone();
    let watcher_max_size = config.max_file_size_mb;

    // Spawn the writer service before finishing source registration
    // so we can wire the SearchHandle-backed HasBody adapter into
    // `config.body_checker` — the builtin `fs` source reads that
    // OnceLock during its own construction.
    let (mutation_tx, search, _writer_handle) = {
        let broadcasters = registry.broadcasters();
        let multi: std::sync::Arc<dyn lixun_mutation::MutationBroadcaster> =
            if broadcasters.is_empty() {
                std::sync::Arc::new(lixun_mutation::NoopBroadcaster)
            } else {
                std::sync::Arc::new(lixun_mutation::MultiBroadcaster::new(broadcasters))
            };
        index_service::spawn_writer_service_with_broadcaster(
            index,
            multi,
            profile.tantivy_heap_bytes,
            profile.tantivy_num_threads,
        )?
    };

    let body_checker_arc: Arc<dyn lixun_sources::HasBody> =
        Arc::new(SearchHandleBodyChecker::new(search.clone()));
    let _ = config.body_checker.set(Arc::clone(&body_checker_arc));

    // Step 2: now that `config.body_checker` is populated, register
    // the builtin apps + fs sources. FsSource picks up the checker
    // via `config.build_fs_source` / `with_body_checker`.
    register_builtin_nonplugin_sources(&mut registry, &config, &sources_state_dir, &profile)?;
    let registry = Arc::new(registry);

    // Build the IPC-facing search surface. When a plugin advertises
    // an ANN handle this becomes a `HybridSearchHandle` running RRF
    // fusion; otherwise it stays a lexical-only pass-through. The
    // host names no plugin (AGENTS.md §1) — `registry.ann_handle()`
    // is a generic capability probe. The indexer + body-checker
    // paths keep using the raw lexical `search` because they need
    // the unfused `SearchHandle` API surface (writer-side hooks,
    // OCR enqueue body lookups), which fusion has nothing to add to.
    let ipc_search: SearchSurface = match registry.ann_handle() {
        Some(ann) => HybridSearchHandle::new(search.clone(), ann, 60.0),
        None => HybridSearchHandle::new_lexical_only(search.clone()),
    };

    registry.install_doc_store(Arc::new(search.clone()) as Arc<dyn lixun_mutation::DocStore>);

    let shared_config = Arc::new(config);

    let frecency = FrecencyStore::load(&state_dir)?;
    let frecency = Arc::new(RwLock::new(frecency));

    let query_latch = QueryLatchStore::load(&state_dir)?;
    let query_latch = Arc::new(RwLock::new(query_latch));

    let query_log = QueryLog::load(&state_dir)?;
    let query_log = Arc::new(RwLock::new(query_log));

    let gui_control = Arc::new(GuiControl::new());
    let preview_spawner = Arc::new(PreviewSpawner::new(Arc::clone(&gui_control)));

    let global_toggle_rx = hotkeys::spawn_global_toggle_listener(
        shared_config.keybindings.global_toggle.clone(),
        state_dir.clone(),
    )
    .await;
    let mut global_toggle_rx = match global_toggle_rx {
        Ok(rx) => Some(rx),
        Err(e) => {
            tracing::warn!("Global shortcut portal unavailable: {}", e);
            None
        }
    };

    let indexer_mutation = mutation_tx.clone();
    let indexer_search = search.clone();
    let indexer_stats = Arc::clone(&stats);
    let indexer_state_dir = state_dir.clone();
    let indexer_config = Arc::clone(&shared_config);
    let indexer_registry = Arc::clone(&registry);
    tokio::spawn(async move {
        if let Err(e) = run_incremental_indexer(
            indexer_mutation,
            indexer_search,
            indexer_stats,
            indexer_state_dir,
            indexer_config,
            indexer_registry,
            rebuilt_from_scratch,
        )
        .await
        {
            tracing::error!("Incremental indexer error: {}", e);
        }
    });

    let watcher_mutation = mutation_tx.clone();
    let watcher_caps = Arc::clone(&extract_caps_arc);
    let watcher_enqueue = ocr_enqueue_arc.clone();
    let watcher_body_checker = Some(Arc::clone(&body_checker_arc));
    let watcher_min_image_side_px = shared_config.ocr.min_image_side_px;
    tokio::spawn(async move {
        if let Err(e) = watcher::start(
            watcher_roots,
            watcher_exclude,
            watcher_exclude_regex,
            watcher_max_size,
            watcher_caps,
            watcher_enqueue,
            watcher_body_checker,
            watcher_min_image_side_px,
            watcher_mutation,
        )
        .await
        {
            tracing::error!("Watcher error: {}", e);
        }
    });

    let indexer_sink = Arc::new(lixun_indexer::WriterSink::new(mutation_tx.clone()));
    {
        let tick_handles =
            lixun_indexer::tick_scheduler::spawn_all(&registry, indexer_sink.clone());
        tracing::info!(
            "tick scheduler: {} source instance(s) with tick_interval",
            tick_handles.len()
        );
        std::mem::forget(tick_handles);
    }

    spawn_ocr_worker(
        Arc::clone(&shared_config),
        Arc::clone(&stats),
        indexer_sink.clone(),
        extract_caps.clone(),
        ocr_queue.clone(),
        Arc::clone(&ocr_worker_stats),
        Arc::clone(&profile_swap),
    );

    spawn_cache_sweep_worker(Arc::clone(&shared_config), ocr_queue.clone());

    {
        let pfw_registry = Arc::clone(&registry);
        let pfw_sink = indexer_sink.clone();
        tokio::spawn(async move {
            if let Err(e) = plugin_fs_watcher::start(pfw_registry, pfw_sink).await {
                tracing::error!("plugin fs watcher error: {}", e);
            }
        });
    }

    if shared_config.impact.follow_battery {
        let bw_swap = Arc::clone(&profile_swap);
        let on_ac = shared_config.impact.level;
        let on_battery = shared_config.impact.on_battery_level;
        let cpus = num_cpus::get();
        let apply_swap = Arc::clone(&profile_swap);
        let hot_apply: battery::HotApplyFn = Arc::new(move |level: lixun_core::SystemImpact| {
            let old = apply_swap.load_full();
            if old.level == level {
                return;
            }
            let new_profile = lixun_core::ImpactProfile::from_level(level, cpus);
            let (applied_hot, requires_restart) = impact_diff_hot_cold(old.as_ref(), &new_profile);
            sched::apply_nice_only(new_profile.daemon_nice);
            apply_swap.store(Arc::new(new_profile.clone()));
            tracing::info!(
                "impact level changed (battery): {:?} -> {:?} (hot={} cold={})",
                old.level,
                new_profile.level,
                applied_hot.len(),
                requires_restart.len(),
            );
        });
        tokio::spawn(async move {
            if let Err(e) =
                battery::watch_battery(bw_swap, on_ac, on_battery, cpus, hot_apply).await
            {
                tracing::warn!("battery watcher exited: {e}");
            }
        });
    }

    let socket_path = socket_path();
    if socket_path.exists() {
        std::fs::remove_file(&socket_path)?;
    }

    tracing::info!("Listening on {:?}", socket_path);

    let listener = tokio::net::UnixListener::bind(&socket_path)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = std::fs::metadata(&socket_path)?;
        let mut perms = metadata.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&socket_path, perms)?;
    }

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::channel::<()>(1);

    let shutdown_tx_signal = shutdown_tx.clone();
    let mut signals = signal_hook_tokio::Signals::new([
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGINT,
    ])?;
    tokio::spawn(async move {
        if let Some(sig) = signals.next().await {
            tracing::info!("Received signal {}, shutting down...", sig);
            let _ = shutdown_tx_signal.send(()).await;
        }
    });

    #[allow(unreachable_code)]
    loop {
        tokio::select! {
            hotkey = async {
                if let Some(rx) = &mut global_toggle_rx {
                    rx.recv().await
                } else {
                    futures::future::pending().await
                }
            } => {
                let Some(()) = hotkey else {
                    tracing::warn!("hotkeys: channel closed, disabling listener");
                    global_toggle_rx = None;
                    continue;
                };
                tracing::info!("hotkey: global toggle activated");
                if let Err(e) = gui_control.dispatch(GuiCommand::Toggle).await {
                    tracing::error!("hotkey: gui_control dispatch failed: {}", e);
                }
            }
            result = listener.accept() => {
                let (stream, _) = match result {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::debug!("Accept error: {}", e);
                        continue;
                    }
                };
                let search = ipc_search.clone();
                let mutation_tx = mutation_tx.clone();
                let frecency = Arc::clone(&frecency);
                let query_latch = Arc::clone(&query_latch);
                let query_log = Arc::clone(&query_log);
                let stats = Arc::clone(&stats);
                let gui_control = Arc::clone(&gui_control);
                let preview_spawner = Arc::clone(&preview_spawner);
                let shared_config = Arc::clone(&shared_config);
                let client_registry = Arc::clone(&registry);
                let client_ocr_queue = ocr_queue.clone();
                let client_ocr_worker_stats = Arc::clone(&ocr_worker_stats);
                let client_profile_swap = Arc::clone(&profile_swap);

                tokio::spawn(async move {
                    if let Err(e) = handle_client(stream, search, mutation_tx, frecency, query_latch, query_log, stats, gui_control, preview_spawner, shared_config, client_registry, client_ocr_queue, client_ocr_worker_stats, client_profile_swap).await {
                        tracing::debug!("Client error: {}", e);
                    }
                });
            }
            _ = shutdown_rx.recv() => {
                tracing::info!("Shutting down gracefully...");
                gui_control.shutdown().await;
                let _ = std::fs::remove_file(&socket_path);
                let frecency = frecency.read().await;
                if let Err(e) = frecency.save(&state_dir) {
                    tracing::error!("Failed to save frecency store: {}", e);
                }
                let latch = query_latch.read().await;
                if let Err(e) = latch.save(&state_dir) {
                    tracing::error!("Failed to save query latch store: {}", e);
                }
                let log = query_log.read().await;
                if let Err(e) = log.save(&state_dir) {
                    tracing::error!("Failed to save query log: {}", e);
                }
                tracing::info!("Shutdown complete");
                std::process::exit(0);
            }
        }
    }

    #[allow(unreachable_code)]
    Ok(())
}

async fn run_incremental_indexer(
    mutation_tx: IndexMutationTx,
    search: SearchHandle,
    stats: Arc<RwLock<IndexStats>>,
    state_dir: std::path::PathBuf,
    config: Arc<config::Config>,
    registry: Arc<lixun_indexer::SourceRegistry>,
    rebuilt_from_scratch: bool,
) -> Result<()> {
    let (fs_count, other_count) = indexer::run_incremental(
        &mutation_tx,
        &search,
        config.as_ref(),
        registry.as_ref(),
        &state_dir,
        rebuilt_from_scratch,
    )
    .await?;
    let total = fs_count + other_count;
    if total > 0 {
        let mut stats_lock = stats.write().await;
        stats_lock.indexed_docs += total as u64;
        stats_lock.last_reindex = Some(Utc::now());
    }
    Ok(())
}

async fn do_reindex(
    mutation_tx: &IndexMutationTx,
    stats: &Arc<RwLock<IndexStats>>,
    config: &config::Config,
    registry: &lixun_indexer::SourceRegistry,
    state_dir: &std::path::Path,
    paths: Vec<std::path::PathBuf>,
) -> Result<usize, anyhow::Error> {
    let count = if paths.is_empty() {
        indexer::reindex_full(mutation_tx, config, registry, state_dir)
            .await?
            .total_docs
    } else {
        indexer::reindex_paths(mutation_tx, config, &paths).await?
    };

    {
        let mut stats_lock = stats.write().await;
        stats_lock.indexed_docs += count as u64;
        stats_lock.last_reindex = Some(Utc::now());
    }

    tracing::info!("Reindex complete: {} documents processed", count);
    Ok(count)
}

fn collect_watcher_stats() -> lixun_ipc::WatcherStats {
    let (directories, excluded, errors, overflow_events) = watcher::stats();
    lixun_ipc::WatcherStats {
        directories,
        excluded,
        errors,
        overflow_events,
    }
}

fn collect_writer_stats() -> lixun_ipc::WriterStats {
    let (commits, last_commit_latency_ms, generation) = index_service::stats();
    lixun_ipc::WriterStats {
        commits,
        last_commit_latency_ms: last_commit_latency_ms.min(u32::MAX as u64) as u32,
        generation,
    }
}

/// Hardcoded retry ceiling for the OCR queue. Shared between the
/// worker's drain loop (`peek_next(max_attempts)`) and the Status
/// handler's stats breakdown (`stats(max_attempts)`) so pending/failed
/// counts agree with what the worker would actually process.
const OCR_MAX_ATTEMPTS: u32 = 3;

/// Build the IPC `OcrStats` snapshot for Status responses. Returns
/// `None` when OCR is disabled (no queue allocated) so clients can
/// omit the OCR block entirely without ambiguity.
fn collect_ocr_stats(
    queue: Option<&lixun_extract::ocr_queue::OcrQueue>,
    worker_stats: &lixun_indexer::ocr_tick::OcrWorkerStats,
    max_attempts: u32,
) -> Option<lixun_ipc::OcrStats> {
    let q = queue?;
    let qs = match q.stats(max_attempts) {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!("ocr stats unavailable: {:#}", e);
            return None;
        }
    };
    let (drained_total, last_drain_at_raw) = worker_stats.snapshot();
    let last_drain_at = if last_drain_at_raw > 0 {
        Some(last_drain_at_raw)
    } else {
        None
    };
    Some(lixun_ipc::OcrStats {
        queue_total: qs.total,
        queue_pending: qs.pending,
        queue_failed: qs.failed,
        drained_total,
        last_drain_at,
    })
}

fn collect_memory_stats() -> lixun_ipc::MemoryStats {
    let Ok(text) = std::fs::read_to_string("/proc/self/status") else {
        return lixun_ipc::MemoryStats::default();
    };
    let mut m = lixun_ipc::MemoryStats::default();
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let Some(kb_str) = rest.split_whitespace().next() else {
            continue;
        };
        let Ok(kb) = kb_str.parse::<u64>() else {
            continue;
        };
        let bytes = kb.saturating_mul(1024);
        match key {
            "VmRSS" => m.rss_bytes = bytes,
            "VmPeak" => m.vm_peak_bytes = bytes,
            "VmSize" => m.vm_size_bytes = bytes,
            "VmSwap" => m.vm_swap_bytes = bytes,
            _ => {}
        }
    }
    m
}

struct SearchSlot {
    generation: u64,
    cancel: CancellationToken,
}

struct ConnectionState {
    active_search_slot: Arc<tokio::sync::Mutex<Option<SearchSlot>>>,
    next_search_generation: std::sync::atomic::AtomicU64,
}

impl ConnectionState {
    fn new() -> Self {
        Self {
            active_search_slot: Arc::new(tokio::sync::Mutex::new(None)),
            next_search_generation: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

const PLUGIN_BUDGET: std::time::Duration = std::time::Duration::from_millis(50);

async fn process_search_chunk(
    mut hits: Vec<lixun_core::Hit>,
    mut breakdowns: Vec<lixun_core::ScoreBreakdown>,
    phase: lixun_fusion::Phase,
    explain: bool,
    q: &str,
    frecency: &Arc<RwLock<FrecencyStore>>,
    query_latch: &Arc<RwLock<QueryLatchStore>>,
    config: &Arc<config::Config>,
    registry: &Arc<lixun_indexer::SourceRegistry>,
) -> anyhow::Result<(
    Vec<lixun_core::Hit>,
    Option<lixun_core::Calculation>,
    Option<lixun_core::DocId>,
    Vec<String>,
)> {
    let now = chrono::Utc::now().timestamp();
    
    {
        let frecency_store = frecency.read().await;
        let latch_store = query_latch.read().await;
        let alpha = config.ranking_frecency_alpha;
        let w = config.ranking_latch_weight;
        let cap = config.ranking_latch_cap;
        let total_cap = config.ranking_total_multiplier_cap;
        for (i, hit) in hits.iter_mut().enumerate() {
            let frec = frecency_store.mult(&hit.id.0, now, alpha);
            let lat = latch_store.mult(q, &hit.id.0, now, w, cap);
            let stage2 = frec * lat;
            let clamped = clamp_stage2_mult(stage2, total_cap);
            hit.score *= clamped;
            if explain {
                if let Some(b) = breakdowns.get_mut(i) {
                    b.frecency_mult = frec;
                    b.latch_mult = lat;
                    b.stage2_clamped = clamped;
                    b.final_score = hit.score;
                }
            }
        }
    }

    if phase == lixun_fusion::Phase::Final {
        let mut plugin_tasks = Vec::new();
        for entry in &registry.instances {
            let plugin = entry.source.clone();
            let q_str = q.to_string();
            let instance_id = entry.instance_id.clone();
            let state_dir = entry.state_dir.clone();
            plugin_tasks.push(tokio::task::spawn_blocking(move || {
                let ctx = QueryContext {
                    instance_id: &instance_id,
                    state_dir: &state_dir,
                };
                let start = std::time::Instant::now();
                let result = plugin.on_query(&q_str, &ctx);
                let elapsed = start.elapsed();
                if elapsed > PLUGIN_BUDGET {
                    tracing::warn!(
                        instance = %instance_id,
                        elapsed_ms = elapsed.as_millis(),
                        "plugin on_query exceeded budget"
                    );
                }
                result
            }));
        }
        for task in plugin_tasks {
            let plugin_hits = match task.await {
                Ok(h) => h,
                Err(_) => continue,
            };
            if explain {
                for h in &plugin_hits {
                    breakdowns.push(lixun_core::ScoreBreakdown {
                        tantivy: h.score,
                        category_mult: 1.0,
                        exact_title_mult: 1.0,
                        prefix_mult: 1.0,
                        acronym_mult: 1.0,
                        recency_mult: 1.0,
                        coord_mult: 1.0,
                        frecency_mult: 1.0,
                        latch_mult: 1.0,
                        stage2_clamped: 1.0,
                        final_score: h.score,
                    });
                }
            }
            hits.extend(plugin_hits);
        }
    }

    for hit in hits.iter_mut() {
        if hit.source_instance.is_empty() {
            continue;
        }
        if let Some(menu) = registry.row_menu_for(&hit.source_instance) {
            hit.row_menu = menu;
        }
    }

    let explanations: Vec<String> = if explain {
        let mut idx: Vec<usize> = (0..hits.len()).collect();
        idx.sort_by(|&a, &b| compare_hits_for_ranking(&hits[a], &hits[b]));
        let hits_sorted: Vec<_> = idx.iter().map(|&i| hits[i].clone()).collect();
        let brk_sorted: Vec<_> = idx.iter().map(|&i| breakdowns[i].clone()).collect();
        hits = hits_sorted;
        breakdowns = brk_sorted;
        breakdowns
            .iter()
            .zip(hits.iter())
            .map(|(b, h)| format_breakdown(b, h))
            .collect()
    } else {
        hits.sort_by(compare_hits_for_ranking);
        Vec::new()
    };

    let top_hit_id = if phase == lixun_fusion::Phase::Final {
        let frecency_store = frecency.read().await;
        let latch_store = query_latch.read().await;
        let decision = top_hit::select_top_hit(
            q,
            &hits,
            &frecency_store,
            &latch_store,
            now,
            config.ranking_top_hit_min_confidence,
            config.ranking_top_hit_min_margin,
            config.ranking_strong_latch_threshold,
        );
        tracing::debug!(
            query = %q,
            top_hit = ?decision.id,
            confidence = decision.confidence,
            margin = decision.margin,
            prefix_match = decision.prefix_match,
            acronym_match = decision.acronym_match,
            has_strong_latch = decision.has_strong_latch,
            dominance = decision.dominance,
            "top_hit selection"
        );
        decision.id
    } else {
        None
    };

    let calculation: Option<lixun_core::Calculation> = None;
    Ok((hits, calculation, top_hit_id, explanations))
}

#[allow(clippy::too_many_arguments)]
async fn handle_search(
    generation: u64,
    cancel: CancellationToken,
    write_tx: mpsc::Sender<Response>,
    epoch: u64,
    q: String,
    limit: u32,
    explain: bool,
    fusion: Arc<SearchSurface>,
    frecency: Arc<RwLock<FrecencyStore>>,
    query_latch: Arc<RwLock<QueryLatchStore>>,
    config: Arc<config::Config>,
    registry: Arc<lixun_indexer::SourceRegistry>,
    active_search_slot: Arc<tokio::sync::Mutex<Option<SearchSlot>>>,
) -> anyhow::Result<()> {
    let query_obj = lixun_core::Query {
        text: q.clone(),
        limit,
    };

    let mut rx = fusion.search_streaming(&query_obj, cancel.clone()).await?;

    while let Some(chunk) = rx.recv().await {
        if cancel.is_cancelled() {
            return Ok(());
        }

        let (hits, breakdowns) = if explain {
            let mut hits_v = Vec::with_capacity(chunk.hits.len());
            let mut brk_v = Vec::with_capacity(chunk.hits.len());
            for (h, b) in chunk.hits {
                hits_v.push(h);
                brk_v.push(b);
            }
            (hits_v, brk_v)
        } else {
            (chunk.hits.into_iter().map(|(h, _)| h).collect(), Vec::new())
        };

        let (processed_hits, calculation, top_hit_id, explanations) = process_search_chunk(
            hits,
            breakdowns,
            chunk.phase,
            explain,
            &q,
            &frecency,
            &query_latch,
            &config,
            &registry,
        )
        .await?;

        if cancel.is_cancelled() {
            return Ok(());
        }

        let phase = match chunk.phase {
            lixun_fusion::Phase::Initial => lixun_ipc::Phase::Initial,
            lixun_fusion::Phase::Final => lixun_ipc::Phase::Final,
        };

        let resp = Response::SearchChunk {
            epoch,
            phase,
            hits: processed_hits,
            calculation,
            top_hit: top_hit_id,
            explanations,
        };

        if write_tx.send(resp).await.is_err() {
            return Ok(());
        }
    }

    let mut slot = active_search_slot.lock().await;
    if let Some(s) = slot.as_ref() {
        if s.generation == generation {
            *slot = None;
        }
    }

    Ok(())
}

async fn writer_task(
    mut stream_write: tokio::io::WriteHalf<tokio::net::UnixStream>,
    mut rx: mpsc::Receiver<Response>,
    negotiated_version: u16,
) -> anyhow::Result<()> {
    while let Some(resp) = rx.recv().await {
        let json = serde_json::to_vec(&resp)?;
        let total_len = (2 + json.len()) as u32;
        let mut out = BytesMut::with_capacity(4 + total_len as usize);
        out.put_u32(total_len);
        out.put_u16(negotiated_version);
        out.put_slice(&json);
        stream_write.write_all(&out).await?;
    }
    Ok(())
}

async fn read_request(
    stream_read: &mut tokio::io::ReadHalf<tokio::net::UnixStream>,
    frame_buf: &mut Vec<u8>,
) -> anyhow::Result<Option<(u16, Request)>> {
    let mut hdr = [0u8; 4];
    match stream_read.read_exact(&mut hdr).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let frame_len = u32::from_be_bytes(hdr) as usize;
    if frame_len < 2 {
        anyhow::bail!("frame too short for version");
    }
    let mut ver_buf = [0u8; 2];
    stream_read.read_exact(&mut ver_buf).await?;
    let version = u16::from_be_bytes(ver_buf);
    if !(MIN_PROTOCOL_VERSION..=PROTOCOL_VERSION).contains(&version) {
        anyhow::bail!(
            "unsupported protocol version: got {}, supported {}..={}",
            version,
            MIN_PROTOCOL_VERSION,
            PROTOCOL_VERSION
        );
    }
    frame_buf.clear();
    frame_buf.resize(frame_len - 2, 0);
    stream_read.read_exact(frame_buf).await?;
    let parsed: Request = serde_json::from_slice(frame_buf)?;
    Ok(Some((version, parsed)))
}

#[allow(clippy::too_many_arguments)]
async fn handle_client(
    stream: tokio::net::UnixStream,
    search: SearchSurface,
    mutation_tx: IndexMutationTx,
    frecency: Arc<RwLock<FrecencyStore>>,
    query_latch: Arc<RwLock<QueryLatchStore>>,
    query_log: Arc<RwLock<QueryLog>>,
    stats: Arc<RwLock<IndexStats>>,
    gui_control: Arc<GuiControl>,
    preview_spawner: Arc<PreviewSpawner>,
    config: Arc<config::Config>,
    registry: Arc<lixun_indexer::SourceRegistry>,
    ocr_queue: Option<Arc<lixun_extract::ocr_queue::OcrQueue>>,
    ocr_worker_stats: Arc<lixun_indexer::ocr_tick::OcrWorkerStats>,
    profile_swap: Arc<ArcSwap<lixun_core::ImpactProfile>>,
) -> anyhow::Result<()> {
    let (mut stream_read, stream_write) = tokio::io::split(stream);

    let mut frame_buf: Vec<u8> = Vec::new();

    let (negotiated_version, first_req) = match read_request(&mut stream_read, &mut frame_buf).await {
        Ok(Some(pair)) => pair,
        Ok(None) => return Ok(()),
        Err(e) => return Err(e),
    };

    let conn_state = ConnectionState::new();
    let (write_tx, write_rx) = mpsc::channel::<Response>(32);

    let mut writer_handle = tokio::spawn(writer_task(stream_write, write_rx, negotiated_version));

    let mut search_tasks = tokio::task::JoinSet::<anyhow::Result<()>>::new();

    let fusion = Arc::new(search);

    let mut pending_first: Option<Request> = Some(first_req);

    loop {
        let req: Request = if let Some(r) = pending_first.take() {
            r
        } else {
            tokio::select! {
            biased;
            res = &mut writer_handle => {
                match res {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => tracing::debug!("writer task error: {}", e),
                    Err(e) => tracing::debug!("writer join error: {}", e),
                }
                break;
            }
            Some(joined) = search_tasks.join_next(), if !search_tasks.is_empty() => {
                match joined {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => tracing::warn!("search task error: {}", e),
                    Err(e) => tracing::warn!("search task join error: {}", e),
                }
                continue;
            }
            read = read_request(&mut stream_read, &mut frame_buf) => {
                match read {
                    Ok(Some((ver, r))) => {
                        if ver != negotiated_version {
                            tracing::debug!(
                                "client switched protocol version mid-connection: {} -> {}",
                                negotiated_version,
                                ver
                            );
                            break;
                        }
                        r
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::debug!("read error: {}", e);
                        break;
                    }
                }
            }
            }
        };

        match req {
            Request::Search { q, limit, explain, epoch } => {
                {
                    let mut slot = conn_state.active_search_slot.lock().await;
                    if let Some(prev) = slot.take() {
                        prev.cancel.cancel();
                    }
                    let generation = conn_state
                        .next_search_generation
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let cancel = CancellationToken::new();
                    *slot = Some(SearchSlot {
                        generation,
                        cancel: cancel.clone(),
                    });
                    drop(slot);

                    let write_tx_c = write_tx.clone();
                    let fusion_c = Arc::clone(&fusion);
                    let frecency_c = Arc::clone(&frecency);
                    let query_latch_c = Arc::clone(&query_latch);
                    let config_c = Arc::clone(&config);
                    let registry_c = Arc::clone(&registry);
                    let active_slot_c = Arc::clone(&conn_state.active_search_slot);

                    search_tasks.spawn(handle_search(
                        generation,
                        cancel,
                        write_tx_c,
                        epoch,
                        q,
                        limit,
                        explain,
                        fusion_c,
                        frecency_c,
                        query_latch_c,
                        config_c,
                        registry_c,
                        active_slot_c,
                    ));
                }
            }
            Request::Toggle => {
                let resp = gui_result_to_response(gui_control.dispatch(GuiCommand::Toggle).await);
                if write_tx.send(resp).await.is_err() {
                    break;
                }
            }
            Request::Show => {
                let resp = gui_result_to_response(gui_control.dispatch(GuiCommand::Show).await);
                if write_tx.send(resp).await.is_err() {
                    break;
                }
            }
            Request::Hide => {
                let resp = gui_result_to_response(gui_control.dispatch(GuiCommand::Hide).await);
                if write_tx.send(resp).await.is_err() {
                    break;
                }
            }
            Request::Reindex { paths } => {
                let already_running = {
                    let mut s = stats.write().await;
                    if s.reindex_in_progress {
                        true
                    } else {
                        s.reindex_in_progress = true;
                        s.reindex_started = Some(Utc::now());
                        false
                    }
                };
                let resp = if already_running {
                    let s = stats.read().await;
                    let started = s
                        .reindex_started
                        .map(|t| t.to_rfc3339())
                        .unwrap_or_else(|| "unknown".into());
                    Response::Error(format!("Reindex already in progress (started {})", started))
                } else {
                    let stats_for_task = Arc::clone(&stats);
                    let mutation_tx_for_task = mutation_tx.clone();
                    let config_for_task = Arc::clone(&config);
                    let registry_for_task = Arc::clone(&registry);
                    let state_dir_for_task = config.state_dir.clone();
                    tokio::spawn(async move {
                        let result = do_reindex(
                            &mutation_tx_for_task,
                            &stats_for_task,
                            &config_for_task,
                            registry_for_task.as_ref(),
                            &state_dir_for_task,
                            paths,
                        )
                        .await;
                        let mut s = stats_for_task.write().await;
                        s.reindex_in_progress = false;
                        s.reindex_started = None;
                        match result {
                            Ok(count) => {
                                tracing::info!("Background reindex complete: {} docs", count);
                            }
                            Err(e) => {
                                tracing::error!("Background reindex failed: {}", e);
                            }
                        }
                    });
                    let s = stats.read().await;
                    Response::Status {
                        indexed_docs: s.indexed_docs,
                        last_reindex: s.last_reindex,
                        errors: 0,
                        watcher: Some(collect_watcher_stats()),
                        writer: Some(collect_writer_stats()),
                        memory: Some(collect_memory_stats()),
                        reindex_in_progress: s.reindex_in_progress,
                        reindex_started: s.reindex_started,
                        ocr: collect_ocr_stats(
                            ocr_queue.as_deref(),
                            &ocr_worker_stats,
                            OCR_MAX_ATTEMPTS,
                        ),
                    }
                };
                if write_tx.send(resp).await.is_err() {
                    break;
                }
            }
            Request::Status => {
                let s = stats.read().await;
                let resp = Response::Status {
                    indexed_docs: s.indexed_docs,
                    last_reindex: s.last_reindex,
                    errors: 0,
                    watcher: Some(collect_watcher_stats()),
                    writer: Some(collect_writer_stats()),
                    memory: Some(collect_memory_stats()),
                    reindex_in_progress: s.reindex_in_progress,
                    reindex_started: s.reindex_started,
                    ocr: collect_ocr_stats(ocr_queue.as_deref(), &ocr_worker_stats, OCR_MAX_ATTEMPTS),
                };
                if write_tx.send(resp).await.is_err() {
                    break;
                }
            }
            Request::RecordClick { doc_id } => {
                {
                    let mut frec = frecency.write().await;
                    frec.record_click(&doc_id, chrono::Utc::now().timestamp());
                }
                if write_tx.send(Response::Ok).await.is_err() {
                    break;
                }
            }
            Request::RecordQuery { q } => {
                let excluded = registry
                    .instances
                    .iter()
                    .any(|inst| inst.source.excludes_from_query_log(&q));
                if !excluded {
                    let mut log = query_log.write().await;
                    log.record_query(&q);
                }
                if write_tx.send(Response::Ok).await.is_err() {
                    break;
                }
            }
            Request::RecordQueryClick { doc_id, query } => {
                let now = chrono::Utc::now().timestamp();
                {
                    let mut latch = query_latch.write().await;
                    latch.record(&query, &doc_id, now);
                }
                let excluded = registry
                    .instances
                    .iter()
                    .any(|inst| inst.source.excludes_from_query_log(&query));
                if !excluded {
                    let mut log = query_log.write().await;
                    log.record_query(&query);
                }
                if write_tx.send(Response::Ok).await.is_err() {
                    break;
                }
            }
            Request::SearchHistory { limit } => {
                let log = query_log.read().await;
                let resp = Response::Queries(log.recent(limit as usize));
                if write_tx.send(resp).await.is_err() {
                    break;
                }
            }
            Request::Preview { hit, monitor } => {
                let resp = match preview_spawner.dispatch(*hit, monitor).await {
                    Ok(()) => Response::Ok,
                    Err(e) => {
                        tracing::error!("preview_spawn: dispatch failed: {}", e);
                        Response::Error(format!("preview dispatch failed: {}", e))
                    }
                };
                if write_tx.send(resp).await.is_err() {
                    break;
                }
            }
            Request::PreviewHide => {
                let resp = match preview_spawner.hide().await {
                    Ok(()) => Response::Ok,
                    Err(e) => {
                        tracing::error!("preview_spawn: hide failed: {}", e);
                        Response::Error(format!("preview hide failed: {}", e))
                    }
                };
                if write_tx.send(resp).await.is_err() {
                    break;
                }
            }
            Request::EnumeratePlugins => {
                let resp = Response::PluginManifest(registry.cli_manifest());
                if write_tx.send(resp).await.is_err() {
                    break;
                }
            }
            Request::PluginCommand { verb_path, args } => {
                let resp = match registry.cli_invoke(&verb_path, &args).await {
                    Ok(value) => Response::PluginResult(value),
                    Err(e) => Response::PluginError(format!("{e:#}")),
                };
                if write_tx.send(resp).await.is_err() {
                    break;
                }
            }
            Request::ImpactGet => {
                let p = profile_swap.load_full();
                let resp = Response::ImpactSnapshot {
                    level: p.level,
                    profile: ImpactProfileWire::from(p.as_ref()),
                    applied_hot: Vec::new(),
                    requires_restart: Vec::new(),
                    persisted: false,
                };
                if write_tx.send(resp).await.is_err() {
                    break;
                }
            }
            Request::ImpactExplain => {
                let p = profile_swap.load_full();
                let resp = Response::ImpactSnapshot {
                    level: p.level,
                    profile: ImpactProfileWire::from(p.as_ref()),
                    applied_hot: Vec::new(),
                    requires_restart: Vec::new(),
                    persisted: false,
                };
                if write_tx.send(resp).await.is_err() {
                    break;
                }
            }
            Request::ImpactSet { level, persist } => {
                let old = profile_swap.load_full();
                let new_profile = lixun_core::ImpactProfile::from_level(level, num_cpus::get());
                let (applied_hot, requires_restart) = impact_diff_hot_cold(old.as_ref(), &new_profile);
                sched::apply_nice_only(new_profile.daemon_nice);
                profile_swap.store(Arc::new(new_profile.clone()));
                let persist_outcome: Result<bool, String> = if persist {
                    match config::Config::persist_impact_level(level) {
                        Ok(path) => {
                            tracing::info!("impact: persisted level={} to {}", level, path.display());
                            Ok(true)
                        }
                        Err(e) => {
                            tracing::error!("impact: persist failed: {e:#}");
                            Err(format!("persist failed: {e}"))
                        }
                    }
                } else {
                    Ok(false)
                };
                tracing::info!(
                    "impact level changed: {:?} -> {:?} (hot={} cold={})",
                    old.level,
                    new_profile.level,
                    applied_hot.len(),
                    requires_restart.len(),
                );
                let resp = match persist_outcome {
                    Ok(persisted) => Response::ImpactSnapshot {
                        level: new_profile.level,
                        profile: ImpactProfileWire::from(&new_profile),
                        applied_hot,
                        requires_restart,
                        persisted,
                    },
                    Err(msg) => Response::Error(msg),
                };
                if write_tx.send(resp).await.is_err() {
                    break;
                }
            }
        }

        // Drain finished search tasks (logs panics, non-blocking).
        while let Some(joined) = search_tasks.try_join_next() {
            match joined {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!("search task error: {}", e),
                Err(e) => tracing::warn!("search task join error: {}", e),
            }
        }
    }

    // Cleanup on disconnect.
    {
        let mut slot = conn_state.active_search_slot.lock().await;
        if let Some(prev) = slot.take() {
            prev.cancel.cancel();
        }
    }
    drop(write_tx);
    search_tasks.abort_all();
    while search_tasks.join_next().await.is_some() {}
    let _ = writer_handle.await;

    Ok(())
}

fn plugin_factories() -> Vec<Box<dyn lixun_sources::PluginFactory>> {
    lixun_sources::inventory::iter::<lixun_sources::PluginFactoryEntry>
        .into_iter()
        .map(|entry| (entry.new)())
        .collect()
}

/// Register external plugin instances only (every source built via
/// a `PluginFactoryEntry`). Must run BEFORE the Tantivy index is
/// created so the populated `plugin_fields_by_kind` matches the
/// schema. Separated from the builtin apps/fs registration because
/// the fs source wants to read `config.body_checker`, which is not
/// populated until the writer service — and therefore the index —
/// is already up.
fn register_plugin_sources(
    registry: &mut lixun_indexer::SourceRegistry,
    config: &config::Config,
    state_dir_root: &std::path::Path,
    impact: Arc<lixun_core::ImpactProfile>,
) -> anyhow::Result<()> {
    let factories = plugin_factories();
    let mut known_sections: std::collections::HashSet<&'static str> =
        std::collections::HashSet::new();
    for factory in &factories {
        known_sections.insert(factory.section());
    }
    for section_name in config.plugin_sections.keys() {
        if !known_sections.contains(section_name.as_str()) {
            tracing::warn!(
                "config: unknown plugin section [{}]; no factory registered. Typo?",
                section_name
            );
        }
    }

    let ctx = lixun_sources::PluginBuildContext {
        max_file_size_mb: config.max_file_size_mb,
        state_dir_root: state_dir_root.to_path_buf(),
        impact,
    };
    for factory in factories {
        let section = factory.section();
        let Some(raw) = config.plugin_sections.get(section) else {
            continue;
        };
        let instances = factory
            .build(raw, &ctx)
            .map_err(|e| anyhow::anyhow!("plugin factory '{}' failed to build: {}", section, e))?;
        let count = instances.len();
        for inst in instances {
            registry.register(inst.instance_id, state_dir_root, inst.source);
        }
        tracing::info!(
            "plugin '{}' registered {} instance(s) from config",
            section,
            count
        );
    }

    Ok(())
}

/// Register the builtin apps + fs sources. Must run AFTER
/// `config.body_checker` is populated so the fs source picks up the
/// DB-16 short-circuit adapter via `with_body_checker`.
fn register_builtin_nonplugin_sources(
    registry: &mut lixun_indexer::SourceRegistry,
    config: &config::Config,
    state_dir_root: &std::path::Path,
    profile: &lixun_core::ImpactProfile,
) -> anyhow::Result<()> {
    registry.register(
        "builtin:apps".into(),
        state_dir_root,
        Arc::new(lixun_sources::apps::AppsSource::new()),
    );

    let fs = lixun_sources::fs::FsSource::with_regex_and_ocr(
        config.roots.clone(),
        config.exclude.clone(),
        config.exclude_regex.clone(),
        config.max_file_size_mb,
        config.caps_arc(),
        config.ocr_enqueue.get().cloned(),
    )
    .with_body_checker(config.body_checker.get().cloned())
    .with_min_image_side_px(config.ocr.min_image_side_px)
    .with_rayon_threads(profile.rayon_threads);
    registry.register("builtin:fs".into(), state_dir_root, Arc::new(fs));

    Ok(())
}

// Adapter bridging lixun-sources::HasBody to the concrete async
// SearchHandle. `maybe_enqueue_ocr` runs on rayon worker threads
// (not tokio workers), so we can safely drive the async
// SearchHandle::has_body via Handle::block_on. On any error we fall
// through to enqueue — the OcrQueue's INSERT OR IGNORE dedups, so
// "re-enqueue on probe failure" is the safe default. Isolates
// lixun-sources from tokio + Tantivy per AGENTS.md modularity.
struct SearchHandleBodyChecker {
    search: SearchHandle,
    runtime: tokio::runtime::Handle,
}

impl SearchHandleBodyChecker {
    fn new(search: SearchHandle) -> Self {
        Self {
            search,
            runtime: tokio::runtime::Handle::current(),
        }
    }
}

impl lixun_sources::HasBody for SearchHandleBodyChecker {
    fn has_body(&self, doc_id: &str) -> anyhow::Result<bool> {
        if tokio::runtime::Handle::try_current().is_ok() {
            // On a tokio worker we would deadlock on block_on. The
            // rayon threads that drive maybe_enqueue_ocr are NOT tokio
            // workers, so this branch normally never fires; it is a
            // defensive fall-through for unforeseen callers.
            tracing::debug!(
                "body checker: called from tokio runtime thread, skipping short-circuit"
            );
            return Ok(false);
        }
        let search = self.search.clone();
        let doc_id = doc_id.to_string();
        Ok(self
            .runtime
            .block_on(async move { search.has_body(&doc_id).await })
            .unwrap_or(false))
    }

    fn get_body(&self, doc_id: &str) -> anyhow::Result<Option<String>> {
        if tokio::runtime::Handle::try_current().is_ok() {
            tracing::debug!(
                "body checker: get_body called from tokio runtime thread, skipping preservation"
            );
            return Ok(None);
        }
        let search = self.search.clone();
        let doc_id = doc_id.to_string();
        Ok(self
            .runtime
            .block_on(async move { search.get_body(&doc_id).await })
            .unwrap_or(None))
    }
}

// Adapter bridging lixun-sources::OcrEnqueue to the concrete OcrQueue;
// isolates lixun-sources from the SQLite layer per AGENTS.md modularity.
struct OcrQueueEnqueuer(Arc<lixun_extract::ocr_queue::OcrQueue>);

impl lixun_sources::OcrEnqueue for OcrQueueEnqueuer {
    fn enqueue(
        &self,
        doc_id: &str,
        path: &std::path::Path,
        mtime: i64,
        size: u64,
        ext: &str,
    ) -> anyhow::Result<()> {
        let row = lixun_extract::ocr_queue::OcrQueueRow::new(
            doc_id.to_string(),
            path.to_string_lossy().into_owned(),
            mtime,
            size,
            ext.to_string(),
        );
        self.0.enqueue(row)
    }
}

/// Idle gate driven by the daemon's reindex-progress flag. Reads the
/// tokio `RwLock` non-blocking; when contended we assume the daemon
/// is busy and skip this tick.
struct StatsIdleGate {
    stats: Arc<RwLock<IndexStats>>,
}

impl lixun_indexer::ocr_tick::IdleGate for StatsIdleGate {
    fn is_idle(&self) -> bool {
        match self.stats.try_read() {
            Ok(s) => !s.reindex_in_progress,
            Err(_) => false,
        }
    }
}

/// Bridge between the daemon's live [`ArcSwap<ImpactProfile>`] and
/// the indexer's per-tick [`OcrCfgRefresh`] hook. Every OCR tick
/// re-derives the hot subset of [`OcrWorkerCfg`] from the current
/// profile so `lixun impact set` propagates without a restart.
struct ArcSwapOcrRefresh {
    profile_swap: Arc<ArcSwap<lixun_core::ImpactProfile>>,
}

impl lixun_indexer::ocr_tick::OcrCfgRefresh for ArcSwapOcrRefresh {
    fn refresh(&self) -> lixun_indexer::ocr_tick::OcrCfgUpdate {
        let p = self.profile_swap.load();
        let throttle = if p.ocr_adaptive_throttle {
            Some((p.ocr_nice_level, p.ocr_io_class_idle))
        } else {
            None
        };
        lixun_indexer::ocr_tick::OcrCfgUpdate {
            interval: p.ocr_worker_interval,
            jobs_per_tick: p.ocr_jobs_per_tick,
            throttle,
        }
    }
}

fn spawn_ocr_worker(
    config: Arc<config::Config>,
    stats: Arc<RwLock<IndexStats>>,
    sink: Arc<lixun_indexer::WriterSink>,
    mut caps: lixun_extract::ExtractorCapabilities,
    queue: Option<Arc<lixun_extract::ocr_queue::OcrQueue>>,
    worker_stats: Arc<lixun_indexer::ocr_tick::OcrWorkerStats>,
    profile_swap: Arc<ArcSwap<lixun_core::ImpactProfile>>,
) {
    caps.timeout = std::time::Duration::from_secs(config.ocr.timeout_secs);
    caps.ocr_enabled = config.ocr.enabled;
    if !caps.has_tesseract || !config.ocr.enabled {
        tracing::info!(
            "ocr worker: not starting (has_tesseract={}, ocr.enabled={})",
            caps.has_tesseract,
            config.ocr.enabled
        );
        return;
    }
    let Some(queue) = queue else {
        tracing::error!("ocr worker: queue unavailable (open failed earlier); not starting");
        return;
    };
    let langs = resolve_ocr_langs(&config.ocr.languages, &caps.tesseract_langs);
    let throttle = if config.ocr.adaptive_throttle {
        Some((config.ocr.nice_level, config.ocr.io_class_idle))
    } else {
        None
    };
    let cfg = lixun_indexer::ocr_tick::OcrWorkerCfg {
        interval: std::time::Duration::from_secs(config.ocr.worker_interval_secs),
        langs,
        min_image_side_px: config.ocr.min_image_side_px,
        max_pages_per_pdf: config.ocr.max_pages_per_pdf,
        max_attempts: OCR_MAX_ATTEMPTS,
        jobs_per_tick: config.ocr.jobs_per_tick as usize,
        throttle,
    };
    let idle: Arc<dyn lixun_indexer::ocr_tick::IdleGate> = if config.ocr.adaptive_throttle {
        let stats_gate: Arc<dyn lixun_indexer::ocr_tick::IdleGate> =
            Arc::new(StatsIdleGate { stats });
        let psi_gate: Arc<dyn lixun_indexer::ocr_tick::IdleGate> = Arc::new(
            lixun_indexer::psi_gate::CpuPsiGate::new(config.ocr.max_cpu_pressure_avg10),
        );
        Arc::new(lixun_indexer::psi_gate::CompositeIdleGate {
            gates: vec![stats_gate, psi_gate],
        })
    } else {
        Arc::new(StatsIdleGate { stats })
    };
    let handle = lixun_indexer::ocr_tick::spawn(
        queue,
        idle,
        sink,
        Arc::new(caps),
        cfg,
        worker_stats,
        Some(Arc::new(ArcSwapOcrRefresh { profile_swap })
            as Arc<dyn lixun_indexer::ocr_tick::OcrCfgRefresh>),
    );
    std::mem::forget(handle);
}

/// Adapter exposing the OCR queue's zombie-reap query behind the
/// indexer's generic `ZombieReaper` trait. Kept local to the daemon
/// so the indexer crate remains unaware of the concrete queue type.
struct OcrQueueReaper(Arc<lixun_extract::ocr_queue::OcrQueue>);

impl lixun_indexer::cache_sweep::ZombieReaper for OcrQueueReaper {
    fn reap(&self, max_attempts: u32, older_than_secs: i64) -> Result<u64> {
        self.0.reap_zombies(max_attempts, older_than_secs)
    }
}

fn spawn_cache_sweep_worker(
    config: Arc<config::Config>,
    queue: Option<Arc<lixun_extract::ocr_queue::OcrQueue>>,
) {
    let cfg = lixun_indexer::cache_sweep::CacheSweepCfg {
        cache_root: lixun_extract::cache::cache_root(),
        max_bytes: config.extract.cache_max_mb * 1024 * 1024,
        interval: std::time::Duration::from_secs(config.extract.cache_sweep_interval_secs),
        tmp_max_age: std::time::Duration::from_secs(3600),
        zombie_max_attempts: OCR_MAX_ATTEMPTS,
        zombie_max_age: std::time::Duration::from_secs(30 * 86_400),
    };
    let reaper: Option<Arc<dyn lixun_indexer::cache_sweep::ZombieReaper>> = queue
        .map(|q| Arc::new(OcrQueueReaper(q)) as Arc<dyn lixun_indexer::cache_sweep::ZombieReaper>);
    let handle = lixun_indexer::cache_sweep::spawn(cfg, reaper);
    std::mem::forget(handle);
}

/// Intersect user-configured languages with probed-available ones.
/// Empty user config means "use everything probed". Empty result
/// falls back to `eng` so tesseract gets a runnable `-l` argument.
fn resolve_ocr_langs(configured: &[String], available: &[String]) -> Vec<String> {
    if configured.is_empty() {
        if available.is_empty() {
            return vec!["eng".into()];
        }
        return available.to_vec();
    }
    let mut out: Vec<String> = configured
        .iter()
        .filter(|l| available.iter().any(|a| a == *l))
        .cloned()
        .collect();
    if out.is_empty() {
        out.push("eng".into());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use lixun_core::{Action, Category, DocId, Hit};

    fn mk_hit(id: &str, score: f32) -> Hit {
        Hit {
            id: DocId(id.into()),
            category: Category::App,
            title: id.into(),
            subtitle: String::new(),
            icon_name: None,
            kind_label: None,
            score,
            action: Action::Launch {
                exec: "true".into(),
                terminal: false,
                desktop_id: None,
                desktop_file: None,
                working_dir: None,
            },
            extract_fail: false,
            sender: None,
            recipients: None,
            body: None,
            secondary_action: None,
            source_instance: String::new(),
            row_menu: lixun_core::RowMenuDef::empty(),
            mime: None,
        }
    }

    #[test]
    fn clamp_stage2_mult_caps_at_total() {
        assert_eq!(clamp_stage2_mult(10.0, 6.0), 6.0);
        assert_eq!(clamp_stage2_mult(1000.0, 6.0), 6.0);
    }

    #[test]
    fn clamp_stage2_mult_passes_through_below_cap() {
        assert_eq!(clamp_stage2_mult(2.0, 6.0), 2.0);
        assert_eq!(clamp_stage2_mult(1.0, 6.0), 1.0);
        assert_eq!(clamp_stage2_mult(0.5, 6.0), 0.5);
    }

    #[test]
    fn clamp_stage2_mult_boundary_cap() {
        assert_eq!(clamp_stage2_mult(6.0, 6.0), 6.0);
    }

    #[test]
    fn sort_tiebreaker_deterministic() {
        let mut a = [
            mk_hit("app:c", 5.0),
            mk_hit("app:a", 5.0),
            mk_hit("app:b", 5.0),
        ];
        let mut b = [
            mk_hit("app:b", 5.0),
            mk_hit("app:c", 5.0),
            mk_hit("app:a", 5.0),
        ];
        a.sort_by(compare_hits_for_ranking);
        b.sort_by(compare_hits_for_ranking);
        assert_eq!(
            a.iter().map(|h| h.id.0.as_str()).collect::<Vec<_>>(),
            ["app:a", "app:b", "app:c"]
        );
        assert_eq!(
            b.iter().map(|h| h.id.0.as_str()).collect::<Vec<_>>(),
            ["app:a", "app:b", "app:c"]
        );
    }

    #[test]
    fn sort_score_descending_takes_precedence() {
        let mut hits = [
            mk_hit("app:a", 1.0),
            mk_hit("app:b", 10.0),
            mk_hit("app:c", 5.0),
        ];
        hits.sort_by(compare_hits_for_ranking);
        assert_eq!(
            hits.iter().map(|h| h.id.0.as_str()).collect::<Vec<_>>(),
            ["app:b", "app:c", "app:a"]
        );
    }
}
