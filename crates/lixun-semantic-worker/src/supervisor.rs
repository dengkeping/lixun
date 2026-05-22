//! Idle-eviction supervisor for embedder sessions (B4).
//!
//! Holds three swappable slots, one per embedder: BGE text, CLIP
//! image, CLIP text. Each slot is an `ArcSwapOption<Mutex<E>>` so
//! callers can read the current `Arc<Mutex<E>>` without blocking
//! the eviction tick that swaps it for `None`.
//!
//! ## Why a supervisor at all
//! `.workspace-local/plans/launcher-perf-and-semantic-memory.md` §B4
//! describes the rationale: the steady-state working set is
//! dominated by three FP32 ONNX sessions (~300 MB combined) that
//! sit resident even when the launcher is idle for hours. Dropping
//! the `TextEmbedding` / `ImageEmbedding` returns the ORT arenas
//! to jemalloc, which the B1 tuning then releases to the OS within
//! one `dirty_decay_ms` window (5 s).
//!
//! ## Mid-batch safety
//! `text()` / `image()` / `clip_text()` hand back a cloned
//! `Arc<Mutex<E>>` to the caller. `ArcSwapOption::store(None)`
//! only drops the supervisor's reference; the caller's `Arc` keeps
//! the embedder alive until the lock is released and the `Arc` is
//! dropped. That makes mid-flush eviction structurally impossible:
//! the worker thread holds the `Arc` across the entire batch.
//!
//! ## Lazy reload race
//! Two callers may observe a `None` slot concurrently. A per-slot
//! reload `Mutex` serialises them; the second caller observes the
//! post-store state and reuses the embedder the first caller
//! constructed.

#![cfg(feature = "idle-eviction")]

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use arc_swap::ArcSwapOption;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::embedder::{
    ClipTextEmbedder, ImageEmbedder, TextEmbedder, load_clip_text_embedder, load_image_embedder,
    load_text_embedder,
};

type TextLoader = Box<dyn Fn() -> Result<TextEmbedder> + Send + Sync>;
type ImageLoader = Box<dyn Fn() -> Result<ImageEmbedder> + Send + Sync>;
type ClipTextLoader = Box<dyn Fn() -> Result<ClipTextEmbedder> + Send + Sync>;

/// Runtime knobs for the eviction supervisor.
///
/// `tick_secs` is the wake-up cadence of the eviction task and must
/// be small enough to react within roughly half the idle threshold.
/// `idle_threshold_secs` is the silence window after which a slot is
/// dropped. Plan §B4 requires this floor at 30 s — rapid churn is
/// worse for RSS than steady residence.
#[derive(Debug, Clone, Copy)]
pub struct EvictionConfig {
    pub tick_secs: u64,
    pub idle_threshold_secs: u64,
}

impl Default for EvictionConfig {
    fn default() -> Self {
        Self {
            tick_secs: 30,
            idle_threshold_secs: 300,
        }
    }
}

impl EvictionConfig {
    /// Reject thresholds below 30 s per the MUST-NOT list in plan §B4.
    fn sanitised(self) -> Self {
        Self {
            tick_secs: self.tick_secs.max(1),
            idle_threshold_secs: self.idle_threshold_secs.max(30),
        }
    }
}

struct Slot<E> {
    slot: ArcSwapOption<Mutex<E>>,
    last_used_unix_secs: AtomicI64,
    reload_gate: Mutex<()>,
}

impl<E> Slot<E> {
    fn new(initial: E) -> Self {
        Self {
            slot: ArcSwapOption::from(Some(Arc::new(Mutex::new(initial)))),
            last_used_unix_secs: AtomicI64::new(now_unix_secs()),
            reload_gate: Mutex::new(()),
        }
    }

    fn mark_used(&self) {
        self.last_used_unix_secs
            .store(now_unix_secs(), Ordering::Relaxed);
    }

    fn idle_secs(&self) -> i64 {
        now_unix_secs().saturating_sub(self.last_used_unix_secs.load(Ordering::Relaxed))
    }

    fn try_evict(&self, label: &'static str, threshold_secs: u64) {
        if self.slot.load_full().is_none() {
            return;
        }
        let idle = self.idle_secs();
        if idle >= threshold_secs as i64 {
            self.slot.store(None);
            tracing::info!(
                target: "supervisor",
                embedder = label,
                idle_secs = idle,
                "embedder evicted"
            );
        }
    }
}

/// Holds the three embedder slots and the loaders used to refill
/// them after eviction. Cheap to clone behind an `Arc`.
pub struct EmbedderSupervisor {
    text: Slot<TextEmbedder>,
    image: Slot<ImageEmbedder>,
    clip_text: Slot<ClipTextEmbedder>,
    load_text: TextLoader,
    load_image: ImageLoader,
    load_clip_text: ClipTextLoader,
}

/// Construction inputs for [`EmbedderSupervisor::new`].
pub struct SupervisorInit {
    pub text: TextEmbedder,
    pub image: ImageEmbedder,
    pub clip_text: ClipTextEmbedder,
    pub load_text: TextLoader,
    pub load_image: ImageLoader,
    pub load_clip_text: ClipTextLoader,
}

impl EmbedderSupervisor {
    pub fn new(init: SupervisorInit) -> Self {
        Self {
            text: Slot::new(init.text),
            image: Slot::new(init.image),
            clip_text: Slot::new(init.clip_text),
            load_text: init.load_text,
            load_image: init.load_image,
            load_clip_text: init.load_clip_text,
        }
    }

    /// Convenience constructor that captures the cache and ONNX
    /// thread settings into the loader closures. Mirrors the
    /// eager-load sequence in `main.rs`.
    pub fn from_config(
        cache_dir: std::path::PathBuf,
        text_model: String,
        image_model: String,
        onnx_intra_threads: usize,
        onnx_inter_threads: usize,
        text: TextEmbedder,
        image: ImageEmbedder,
        clip_text: ClipTextEmbedder,
    ) -> Self {
        let text_model_arc = Arc::new(text_model);
        let image_model_arc = Arc::new(image_model);
        let cache_for_text = cache_dir.clone();
        let cache_for_image = cache_dir.clone();
        let cache_for_clip = cache_dir;
        let text_model_for_text = text_model_arc.clone();
        let image_model_for_image = image_model_arc.clone();

        let load_text: TextLoader = Box::new(move || {
            load_text_embedder(
                &text_model_for_text,
                &cache_for_text,
                onnx_intra_threads,
                onnx_inter_threads,
            )
        });
        let load_image: ImageLoader = Box::new(move || {
            load_image_embedder(
                &image_model_for_image,
                &cache_for_image,
                onnx_intra_threads,
                onnx_inter_threads,
            )
        });
        let load_clip_text: ClipTextLoader = Box::new(move || {
            load_clip_text_embedder(&cache_for_clip, onnx_intra_threads, onnx_inter_threads)
        });

        Self::new(SupervisorInit {
            text,
            image,
            clip_text,
            load_text,
            load_image,
            load_clip_text,
        })
    }

    /// Return a handle to the BGE text embedder, lazy-loading if it
    /// was previously evicted. Marks the slot as freshly used.
    pub fn text(&self) -> Result<Arc<Mutex<TextEmbedder>>> {
        get_or_reload(&self.text, &self.load_text, "text")
    }

    /// Return a handle to the CLIP image embedder, lazy-loading on
    /// demand. Marks the slot as freshly used.
    pub fn image(&self) -> Result<Arc<Mutex<ImageEmbedder>>> {
        get_or_reload(&self.image, &self.load_image, "image")
    }

    /// Return a handle to the CLIP text embedder, lazy-loading on
    /// demand. Marks the slot as freshly used.
    pub fn clip_text(&self) -> Result<Arc<Mutex<ClipTextEmbedder>>> {
        get_or_reload(&self.clip_text, &self.load_clip_text, "clip-text")
    }

    /// Spawn the background eviction task. Returns the join handle so
    /// the binary can attach a shutdown observer if desired; today
    /// the task runs for the lifetime of the worker process.
    pub fn spawn_eviction_task(self: Arc<Self>, config: EvictionConfig) -> JoinHandle<()> {
        let cfg = config.sanitised();
        tracing::info!(
            target: "supervisor",
            tick_secs = cfg.tick_secs,
            idle_threshold_secs = cfg.idle_threshold_secs,
            "eviction supervisor task starting"
        );
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(cfg.tick_secs));
            tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                self.text.try_evict("text", cfg.idle_threshold_secs);
                self.image.try_evict("image", cfg.idle_threshold_secs);
                self.clip_text
                    .try_evict("clip-text", cfg.idle_threshold_secs);
            }
        })
    }
}

fn get_or_reload<E, L>(slot: &Slot<E>, loader: &L, label: &'static str) -> Result<Arc<Mutex<E>>>
where
    L: Fn() -> Result<E> + ?Sized,
{
    if let Some(arc) = slot.slot.load_full() {
        slot.mark_used();
        return Ok(arc);
    }

    let _gate = slot
        .reload_gate
        .lock()
        .map_err(|_| anyhow::anyhow!("supervisor: {label} reload gate poisoned"))?;
    if let Some(arc) = slot.slot.load_full() {
        slot.mark_used();
        return Ok(arc);
    }

    let started = Instant::now();
    let embedder =
        loader().with_context(|| format!("supervisor: lazy reload of {label} embedder failed"))?;
    let load_ms = started.elapsed().as_millis() as u64;
    let arc = Arc::new(Mutex::new(embedder));
    slot.slot.store(Some(arc.clone()));
    slot.mark_used();
    tracing::info!(
        target: "supervisor",
        embedder = label,
        load_ms,
        "embedder loaded"
    );
    Ok(arc)
}

fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
