use serde::Deserialize;

fn default_enabled() -> bool {
    false
}

fn default_text_model() -> String {
    "bge-small-en-v1.5".into()
}

fn default_image_model() -> String {
    "clip-vit-b-32".into()
}

fn default_flush_ms() -> u64 {
    2000
}

fn default_min_image_side_px() -> u32 {
    300
}

fn default_backfill_on_start() -> bool {
    false
}

fn default_rrf_k() -> f32 {
    60.0
}

fn default_cache_subdir() -> String {
    "fastembed".into()
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_text_model")]
    pub text_model: String,
    #[serde(default = "default_image_model")]
    pub image_model: String,
    #[serde(default)]
    pub batch_size: Option<usize>,
    #[serde(default = "default_flush_ms")]
    pub flush_ms: u64,
    #[serde(default = "default_min_image_side_px")]
    pub min_image_side_px: u32,
    #[serde(default = "default_backfill_on_start")]
    pub backfill_on_start: bool,
    #[serde(default)]
    pub max_concurrent_embed_tasks: Option<usize>,
    #[serde(default = "default_rrf_k")]
    pub rrf_k: f32,
    #[serde(default = "default_cache_subdir")]
    pub cache_subdir: String,
}

impl SemanticConfig {
    /// Resolve the effective embed batch size: explicit
    /// `[semantic].batch_size` from operator config wins, otherwise
    /// fall back to the impact-profile hint plumbed in by the host.
    /// Always at least 1 — a zero batch would deadlock the worker.
    pub fn effective_batch_size(&self, hint: usize) -> usize {
        self.batch_size.unwrap_or(hint).max(1)
    }

    /// Resolve effective concurrency for parallel embed tasks. Same
    /// rule as [`Self::effective_batch_size`]: explicit wins, hint is
    /// the fallback. `None` from both means "no cap".
    pub fn effective_max_concurrent_embed_tasks(&self, hint: Option<usize>) -> Option<usize> {
        self.max_concurrent_embed_tasks.or(hint)
    }
}

impl Default for SemanticConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            text_model: default_text_model(),
            image_model: default_image_model(),
            batch_size: None,
            flush_ms: default_flush_ms(),
            min_image_side_px: default_min_image_side_px(),
            backfill_on_start: default_backfill_on_start(),
            max_concurrent_embed_tasks: None,
            rrf_k: default_rrf_k(),
            cache_subdir: default_cache_subdir(),
        }
    }
}
