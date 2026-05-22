use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result};
use async_trait::async_trait;
use lixun_mutation::{AnnHandle, AnnHit, Modality};

use crate::embedder::{ClipTextEmbedder, TextEmbedder};
use crate::query_router::QueryRouter;
use crate::store::VectorStore;
#[cfg(feature = "idle-eviction")]
use crate::supervisor::EmbedderSupervisor;

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
    query_router: OnceLock<Arc<QueryRouter>>,
    #[cfg(feature = "idle-eviction")]
    supervisor: OnceLock<Arc<EmbedderSupervisor>>,
}

impl LanceDbAnnHandle {
    pub fn new() -> Self {
        Self {
            store: OnceLock::new(),
            text_embedder: OnceLock::new(),
            clip_text_embedder: OnceLock::new(),
            query_router: OnceLock::new(),
            #[cfg(feature = "idle-eviction")]
            supervisor: OnceLock::new(),
        }
    }

    /// Install the idle-eviction supervisor so query-side embed
    /// dispatch goes through it instead of holding fixed `Arc<Mutex>`
    /// handles. Only present under `idle-eviction`; the supervisor
    /// owns lazy reload + last-used bookkeeping for each slot.
    #[cfg(feature = "idle-eviction")]
    pub fn install_supervisor(
        &self,
        supervisor: Arc<EmbedderSupervisor>,
    ) -> Result<(), Arc<EmbedderSupervisor>> {
        self.supervisor.set(supervisor)
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

    pub fn install_query_router(&self, router: Arc<QueryRouter>) -> Result<(), Arc<QueryRouter>> {
        self.query_router.set(router)
    }

    pub fn store(&self) -> Option<Arc<VectorStore>> {
        self.store.get().cloned()
    }

    fn embed_query_text(&self, query: &str) -> Result<Option<Vec<f32>>> {
        #[cfg(feature = "idle-eviction")]
        let embedder = match self.supervisor.get() {
            Some(s) => s.text()?,
            None => match self.text_embedder.get() {
                Some(e) => e.clone(),
                None => return Ok(None),
            },
        };
        #[cfg(not(feature = "idle-eviction"))]
        let embedder = match self.text_embedder.get() {
            Some(e) => e.clone(),
            None => return Ok(None),
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
        #[cfg(feature = "idle-eviction")]
        let embedder = match self.supervisor.get() {
            Some(s) => s.clip_text()?,
            None => match self.clip_text_embedder.get() {
                Some(e) => e.clone(),
                None => return Ok(None),
            },
        };
        #[cfg(not(feature = "idle-eviction"))]
        let embedder = match self.clip_text_embedder.get() {
            Some(e) => e.clone(),
            None => return Ok(None),
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

    async fn classify_query(&self, query: &str) -> Result<Modality> {
        let Some(router) = self.query_router.get() else {
            return Ok(Modality::Text);
        };
        let Some(vector) = self.embed_query_clip_text(query)? else {
            return Ok(Modality::Text);
        };
        Ok(router.classify(&vector))
    }
}
