use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result};
use async_trait::async_trait;
use lixun_mutation::{AnnHandle, AnnHit};

use crate::embedder::{ClipTextEmbedder, TextEmbedder};
use crate::store::VectorStore;

/// Approximate-nearest-neighbour handle backed by LanceDB. Both
/// `store` and `text_embedder` are filled lazily by the factory
/// (after the async LanceDB connect succeeds and after fastembed
/// finishes downloading model weights) because `PluginFactory::build`
/// needs to return cheaply enough that the daemon can register every
/// plugin synchronously. Until both slots are populated, every
/// `AnnHandle` query method short-circuits to an empty result.
pub struct LanceDbAnnHandle {
    store: OnceLock<Arc<VectorStore>>,
    text_embedder: OnceLock<Arc<Mutex<TextEmbedder>>>,
    clip_text_embedder: OnceLock<Arc<Mutex<ClipTextEmbedder>>>,
}

impl LanceDbAnnHandle {
    pub fn new() -> Self {
        Self {
            store: OnceLock::new(),
            text_embedder: OnceLock::new(),
            clip_text_embedder: OnceLock::new(),
        }
    }

    pub fn install_store(&self, store: Arc<VectorStore>) -> Result<(), Arc<VectorStore>> {
        self.store.set(store)
    }

    pub fn install_text_embedder(
        &self,
        embedder: Arc<Mutex<TextEmbedder>>,
    ) -> Result<(), Arc<Mutex<TextEmbedder>>> {
        self.text_embedder.set(embedder)
    }

    pub fn install_clip_text_embedder(
        &self,
        embedder: Arc<Mutex<ClipTextEmbedder>>,
    ) -> Result<(), Arc<Mutex<ClipTextEmbedder>>> {
        self.clip_text_embedder.set(embedder)
    }

    pub fn store(&self) -> Option<Arc<VectorStore>> {
        self.store.get().cloned()
    }

    fn embed_query_text(&self, query: &str) -> Result<Option<Vec<f32>>> {
        let Some(embedder) = self.text_embedder.get() else {
            return Ok(None);
        };
        let mut guard = embedder
            .lock()
            .map_err(|_| anyhow::anyhow!("text embedder mutex poisoned"))?;
        let mut vectors = guard
            .embed(vec![query.to_string()])
            .context("ann query: text embed")?;
        Ok(vectors.pop())
    }

    fn embed_query_clip_text(&self, query: &str) -> Result<Option<Vec<f32>>> {
        let Some(embedder) = self.clip_text_embedder.get() else {
            return Ok(None);
        };
        let mut guard = embedder
            .lock()
            .map_err(|_| anyhow::anyhow!("CLIP text embedder mutex poisoned"))?;
        let mut vectors = guard
            .embed(vec![query.to_string()])
            .context("ann query: CLIP text embed")?;
        Ok(vectors.pop())
    }
}

impl Default for LanceDbAnnHandle {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AnnHandle for LanceDbAnnHandle {
    async fn search_text(&self, query: &str, k: usize) -> Result<Vec<AnnHit>> {
        let Some(store) = self.store() else {
            return Ok(Vec::new());
        };
        let Some(vector) = self.embed_query_text(query)? else {
            return Ok(Vec::new());
        };
        store.search_text(&vector, k).await
    }

    async fn search_image(&self, query: &str, k: usize) -> Result<Vec<AnnHit>> {
        let Some(store) = self.store() else {
            return Ok(Vec::new());
        };
        let Some(vector) = self.embed_query_clip_text(query)? else {
            return Ok(Vec::new());
        };
        store.search_image(&vector, k).await
    }
}
