use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use lixun_sources::{
    PluginBuildContext, PluginFactory, PluginFactoryEntry, PluginInstance, inventory,
};

use crate::ann::LanceDbAnnHandle;
use crate::config::SemanticConfig;
use crate::embedder::{load_image_embedder, load_text_embedder};
use crate::journal::{BackfillJournal, default_journal_path};
use crate::source::SemanticSource;
use crate::store::VectorStore;
use crate::worker::spawn_worker;

inventory::submit! {
    PluginFactoryEntry { new: || Box::new(SemanticFactory) as Box<dyn PluginFactory> }
}

pub struct SemanticFactory;

impl PluginFactory for SemanticFactory {
    fn section(&self) -> &'static str {
        "semantic"
    }

    fn build(&self, raw: &toml::Value, ctx: &PluginBuildContext) -> Result<Vec<PluginInstance>> {
        let config: SemanticConfig = raw
            .clone()
            .try_into()
            .context("parsing [semantic] config section")?;

        if !config.enabled {
            return Ok(Vec::new());
        }

        // The daemon's `#[tokio::main]` is already running by the
        // time plugin factories are invoked, so `Handle::current()`
        // is the contract — failing here means a startup-ordering
        // bug in the host (call sequence changed without updating
        // this plugin), which we want to surface loudly.
        let runtime = tokio::runtime::Handle::try_current().context(
            "semantic plugin requires an active tokio runtime in PluginFactory::build; \
             daemon startup ordering changed",
        )?;

        let vectors_dir = vector_store_dir()?;
        let cache_dir = embedder_cache_dir(&config)?;
        let journal_path = default_journal_path()?;

        let onnx_intra = ctx.impact.onnx_intra_threads;
        let onnx_inter = ctx.impact.onnx_inter_threads;
        let effective_batch_size = config.effective_batch_size(ctx.impact.embed_batch_hint);
        let _effective_concurrency =
            config.effective_max_concurrent_embed_tasks(ctx.impact.embed_concurrency_hint);

        tracing::info!(
            "semantic embedder onnx_intra={} onnx_inter={} batch_size={}",
            onnx_intra,
            onnx_inter,
            effective_batch_size
        );

        let text_embedder =
            load_text_embedder(&config.text_model, &cache_dir, onnx_intra, onnx_inter)
                .with_context(|| format!("loading text embedder '{}'", config.text_model))?;
        let image_embedder =
            load_image_embedder(&config.image_model, &cache_dir, onnx_intra, onnx_inter)
                .with_context(|| format!("loading image embedder '{}'", config.image_model))?;
        let text_dim = text_embedder.dim();
        let image_dim = image_embedder.dim();
        let text_embedder = Arc::new(Mutex::new(text_embedder));
        let image_embedder = Arc::new(Mutex::new(image_embedder));

        let store =
            open_store_blocking(vectors_dir.clone(), text_dim, image_dim).with_context(|| {
                format!("opening LanceDB vector store at {}", vectors_dir.display())
            })?;
        let store = Arc::new(store);

        let journal = BackfillJournal::open(&journal_path)?;
        let journal = Arc::new(Mutex::new(journal));

        let worker = spawn_worker(
            config.clone(),
            effective_batch_size,
            store.clone(),
            journal.clone(),
            runtime.clone(),
            text_embedder.clone(),
            image_embedder.clone(),
        )?;

        let ann = Arc::new(LanceDbAnnHandle::new());
        // `install_*` returns `Err` only if a value was already set;
        // freshly constructed handles are always empty.
        let _ = ann.install_store(store.clone());
        let _ = ann.install_text_embedder(text_embedder);

        let source = SemanticSource::new(
            worker,
            ann,
            journal,
            config.backfill_on_start,
            runtime.clone(),
        );

        Ok(vec![PluginInstance {
            instance_id: "semantic".into(),
            source: Arc::new(source),
        }])
    }
}

fn vector_store_dir() -> Result<std::path::PathBuf> {
    let base = dirs::data_local_dir().context("XDG data-local directory unavailable")?;
    Ok(base.join("lixun").join("vectors"))
}

/// Open the LanceDB vector store on a fresh OS thread with its own
/// current-thread tokio runtime.
///
/// The daemon's `#[tokio::main]` is already driving the calling
/// thread when `PluginFactory::build` runs, so calling
/// `Handle::block_on` on that runtime panics ("Cannot start a
/// runtime from within a runtime"). A throwaway `std::thread` with
/// its own runtime sidesteps the constraint without making the
/// `PluginFactory` trait async or deferring the open past
/// registration (the worker, ANN handle, and source all need
/// `Arc<VectorStore>` at build time).
fn open_store_blocking(root: PathBuf, text_dim: usize, image_dim: usize) -> Result<VectorStore> {
    let join = std::thread::Builder::new()
        .name("lixun-semantic-store-open".into())
        .spawn(move || -> Result<VectorStore> {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("building semantic init runtime")?;
            rt.block_on(async move {
                let store = VectorStore::open(&root, text_dim, image_dim).await?;
                // Lance creates a manifest version per write. Pre-batch
                // builds of this plugin upserted one row per call and
                // could leave behind tens of thousands of versions on
                // disk. Compaction reclaims that space but allocates a
                // ~1 GB working set in mimalloc that the runtime
                // retains; gate it so a healthy table pays nothing.
                if let Err(e) = store.compact_if_stale(100).await {
                    tracing::warn!("semantic: compact_if_stale on open failed: {e:#}");
                }
                Ok::<_, anyhow::Error>(store)
            })
        })
        .context("spawning semantic init thread")?;
    join.join()
        .map_err(|_| anyhow!("semantic init thread panicked"))?
}

fn embedder_cache_dir(cfg: &SemanticConfig) -> Result<std::path::PathBuf> {
    if let Ok(env) = std::env::var("FASTEMBED_CACHE_DIR")
        && !env.is_empty()
    {
        return Ok(env.into());
    }
    let base = dirs::cache_dir().context("XDG cache directory unavailable")?;
    Ok(base.join("lixun").join(&cfg.cache_subdir))
}
