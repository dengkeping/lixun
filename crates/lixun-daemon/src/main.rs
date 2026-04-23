//! lixund — Lixun daemon: IPC server, indexer, filesystem watcher.

#[cfg(target_os = "linux")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use anyhow::Result;
use bytes::{BufMut, BytesMut};
use chrono::Utc;
use futures::StreamExt;
use lixun_ipc::gui::{GuiCommand, GuiResponse};
use lixun_ipc::{MIN_PROTOCOL_VERSION, PROTOCOL_VERSION, Request, Response, socket_path};
use std::os::unix::io::AsRawFd;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::RwLock;


mod frecency;
mod query_latch;
mod query_log;
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

/// Total-order comparator for the final hit sort: descending by
/// score, with a deterministic tiebreaker on `doc_id.0` ascending.
/// Without the tiebreaker, near-ties (or NaN scores) produce
/// non-deterministic order, which propagates into Top Hit selection
/// because `select_top_hit` reads `hits[0]` and `hits[1]` directly.
fn compare_hits_for_ranking(
    a: &lixun_core::Hit,
    b: &lixun_core::Hit,
) -> std::cmp::Ordering {
    b.score
        .partial_cmp(&a.score)
        .unwrap_or(std::cmp::Ordering::Equal)
        .then_with(|| a.id.0.cmp(&b.id.0))
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

#[tokio::main]
async fn main() -> Result<()> {
    let env_filter = match std::env::var("RUST_LOG") {
        Ok(raw) if !raw.trim().is_empty() => tracing_subscriber::EnvFilter::new(raw),
        _ => tracing_subscriber::EnvFilter::new("lixun=info"),
    };
    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    tracing::info!("lixund starting...");

    let _lock = try_single_instance()?;

    let config = config::Config::load()?;
    tracing::info!("Config loaded: roots={:?}", config.roots);

    lixun_extract::init_capabilities(lixun_extract::ExtractorCapabilities::probe(
        std::time::Duration::from_secs(config.extractor_timeout_secs),
    ));

    let index_path = config.state_dir.join("index");
    let registry = {
        let mut r = lixun_indexer::SourceRegistry::new();
        let sources_state_dir = config.state_dir.join("sources");
        register_builtin_sources(&mut r, &config, &sources_state_dir)?;
        Arc::new(r)
    };
    let (index, rebuilt_from_scratch) = lixun_index::LixunIndex::create_or_open_with_plugins(
        index_path.to_str().unwrap(),
        &registry.plugin_fields_by_kind,
        config.ranking_config(),
    )?;

    let stats = Arc::new(RwLock::new(IndexStats::default()));
    let state_dir = config.state_dir.clone();
    let watcher_roots = config.roots.clone();
    let watcher_exclude = config.exclude.clone();
    let watcher_exclude_regex = config.exclude_regex.clone();
    let watcher_max_size = config.max_file_size_mb;
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
    ).await;
    let mut global_toggle_rx = match global_toggle_rx {
        Ok(rx) => Some(rx),
        Err(e) => {
            tracing::warn!("Global shortcut portal unavailable: {}", e);
            None
        }
    };

    let (mutation_tx, search, _writer_handle) = index_service::spawn_writer_service(index)?;

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
    tokio::spawn(async move {
        if let Err(e) = watcher::start(
            watcher_roots,
            watcher_exclude,
            watcher_exclude_regex,
            watcher_max_size,
            watcher_mutation,
        )
        .await
        {
            tracing::error!("Watcher error: {}", e);
        }
    });

    let indexer_sink = Arc::new(lixun_indexer::WriterSink::new(mutation_tx.clone()));
    {
        let tick_handles = lixun_indexer::tick_scheduler::spawn_all(&registry, indexer_sink.clone());
        tracing::info!(
            "tick scheduler: {} source instance(s) with tick_interval",
            tick_handles.len()
        );
        std::mem::forget(tick_handles);
    }

    {
        let pfw_registry = Arc::clone(&registry);
        let pfw_sink = indexer_sink.clone();
        tokio::spawn(async move {
            if let Err(e) = plugin_fs_watcher::start(pfw_registry, pfw_sink).await {
                tracing::error!("plugin fs watcher error: {}", e);
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
                let search = search.clone();
                let mutation_tx = mutation_tx.clone();
                let frecency = Arc::clone(&frecency);
                let query_latch = Arc::clone(&query_latch);
                let query_log = Arc::clone(&query_log);
                let stats = Arc::clone(&stats);
                let gui_control = Arc::clone(&gui_control);
                let preview_spawner = Arc::clone(&preview_spawner);
                let shared_config = Arc::clone(&shared_config);
                let client_registry = Arc::clone(&registry);

                tokio::spawn(async move {
                    if let Err(e) = handle_client(stream, search, mutation_tx, frecency, query_latch, query_log, stats, gui_control, preview_spawner, shared_config, client_registry).await {
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

#[allow(clippy::too_many_arguments)]
async fn handle_client(
    mut stream: tokio::net::UnixStream,
    search: SearchHandle,
    mutation_tx: IndexMutationTx,
    frecency: Arc<RwLock<FrecencyStore>>,
    query_latch: Arc<RwLock<QueryLatchStore>>,
    query_log: Arc<RwLock<QueryLog>>,
    stats: Arc<RwLock<IndexStats>>,
    gui_control: Arc<GuiControl>,
    preview_spawner: Arc<PreviewSpawner>,
    config: Arc<config::Config>,
    registry: Arc<lixun_indexer::SourceRegistry>,
) -> anyhow::Result<()> {
    let mut header = [0u8; 4];
    stream.read_exact(&mut header).await?;
    let len = u32::from_be_bytes(header) as usize;
    if len < 2 {
        anyhow::bail!("frame too short for version");
    }
    let mut version_buf = [0u8; 2];
    stream.read_exact(&mut version_buf).await?;
    let version = u16::from_be_bytes(version_buf);
    if !(MIN_PROTOCOL_VERSION..=PROTOCOL_VERSION).contains(&version) {
        let resp = Response::Error(format!(
            "unsupported protocol version: got {}, supported {}..={}",
            version, MIN_PROTOCOL_VERSION, PROTOCOL_VERSION
        ));
        let json = serde_json::to_vec(&resp)?;
        let out_len = (json.len() as u32).to_be_bytes();
        stream.write_all(&out_len).await?;
        stream.write_all(&json).await?;
        return Ok(());
    }
    let negotiated_version = version;
    let mut buf = vec![0u8; len - 2];
    stream.read_exact(&mut buf).await?;

    let req: Request = serde_json::from_slice(&buf)?;

    let resp = match req {
        Request::Toggle => gui_result_to_response(gui_control.dispatch(GuiCommand::Toggle).await),
        Request::Show => gui_result_to_response(gui_control.dispatch(GuiCommand::Show).await),
        Request::Hide => gui_result_to_response(gui_control.dispatch(GuiCommand::Hide).await),
        Request::Search { q, limit } => match search.search(&lixun_core::Query { text: q.clone(), limit }).await {
            Ok(mut hits) => {
                let now = chrono::Utc::now().timestamp();
                {
                    let frecency = frecency.read().await;
                    let latch = query_latch.read().await;
                    let alpha = config.ranking_frecency_alpha;
                    let w = config.ranking_latch_weight;
                    let cap = config.ranking_latch_cap;
                    let total_cap = config.ranking_total_multiplier_cap;
                    for hit in &mut hits {
                        let stage2 = frecency.mult(&hit.id.0, now, alpha)
                            * latch.mult(&q, &hit.id.0, now, w, cap);
                        hit.score *= clamp_stage2_mult(stage2, total_cap);
                    }
                }
                hits.sort_by(compare_hits_for_ranking);
                let decision = {
                    let frecency = frecency.read().await;
                    let latch = query_latch.read().await;
                    top_hit::select_top_hit(
                        &q,
                        &hits,
                        &frecency,
                        &latch,
                        now,
                        config.ranking_top_hit_min_confidence,
                        config.ranking_top_hit_min_margin,
                        config.ranking_strong_latch_threshold,
                    )
                };
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
                let top_hit_id = decision.id;
                let calculation = lixun_index::calculator::detect(&q);
                match negotiated_version {
                    1 => Response::Hits(hits),
                    2 => Response::HitsWithExtras { hits, calculation },
                    _ => Response::HitsWithExtrasV3 {
                        hits,
                        calculation,
                        top_hit: top_hit_id,
                    },
                }
            }
            Err(e) => Response::Error(e.to_string()),
        },
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
            if already_running {
                let s = stats.read().await;
                let started = s
                    .reindex_started
                    .map(|t| t.to_rfc3339())
                    .unwrap_or_else(|| "unknown".into());
                Response::Error(format!(
                    "Reindex already in progress (started {})",
                    started
                ))
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
                }
            }
        }
        Request::Status => {
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
            }
        }
        Request::RecordClick { doc_id } => {
            let mut frecency = frecency.write().await;
            frecency.record_click(&doc_id, chrono::Utc::now().timestamp());
            Response::Ok
        }
        Request::RecordQuery { q } => {
            if lixun_index::calculator::detect(&q).is_some() {
                Response::Ok
            } else {
                let mut log = query_log.write().await;
                log.record_query(&q);
                Response::Ok
            }
        }
        Request::RecordQueryClick { doc_id, query } => {
            let now = chrono::Utc::now().timestamp();
            {
                let mut latch = query_latch.write().await;
                latch.record(&query, &doc_id, now);
            }
            if lixun_index::calculator::detect(&query).is_none() {
                let mut log = query_log.write().await;
                log.record_query(&query);
            }
            Response::Ok
        }
        Request::SearchHistory { limit } => {
            let log = query_log.read().await;
            Response::Queries(log.recent(limit as usize))
        }
        Request::Preview { hit, monitor } => match preview_spawner.dispatch(*hit, monitor).await {
            Ok(()) => Response::Ok,
            Err(e) => {
                tracing::error!("preview_spawn: dispatch failed: {}", e);
                Response::Error(format!("preview dispatch failed: {}", e))
            }
        },
    };

    let json = serde_json::to_vec(&resp)?;
    let total_len = (2 + json.len()) as u32;
    let mut out = BytesMut::with_capacity(4 + total_len as usize);
    out.put_u32(total_len);
    out.put_u16(negotiated_version);
    out.put_slice(&json);
    stream.write_all(&out).await?;

    Ok(())
}

fn plugin_factories() -> Vec<Box<dyn lixun_sources::PluginFactory>> {
    lixun_sources::inventory::iter::<lixun_sources::PluginFactoryEntry>
        .into_iter()
        .map(|entry| (entry.new)())
        .collect()
}

fn register_builtin_sources(
    registry: &mut lixun_indexer::SourceRegistry,
    config: &config::Config,
    state_dir_root: &std::path::Path,
) -> anyhow::Result<()> {
    registry.register(
        "builtin:apps".into(),
        state_dir_root,
        Arc::new(lixun_sources::apps::AppsSource::new()),
    );

    let fs = lixun_sources::fs::FsSource::with_regex(
        config.roots.clone(),
        config.exclude.clone(),
        config.exclude_regex.clone(),
        config.max_file_size_mb,
    );
    registry.register("builtin:fs".into(), state_dir_root, Arc::new(fs));

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
    };
    for factory in factories {
        let section = factory.section();
        let Some(raw) = config.plugin_sections.get(section) else {
            continue;
        };
        let instances = factory.build(raw, &ctx).map_err(|e| {
            anyhow::anyhow!("plugin factory '{}' failed to build: {}", section, e)
        })?;
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
