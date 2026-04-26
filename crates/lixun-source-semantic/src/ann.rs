use std::sync::{Arc, OnceLock};

use lixun_mutation::AnnHandle;

use crate::store::VectorStore;

/// Approximate-nearest-neighbour handle backed by LanceDB. The
/// store is filled lazily by the worker (WD-T6) because LanceDB
/// `connect` + table creation is async + IO-heavy, and the
/// synchronous `PluginFactory::build` path must not block on it.
pub struct LanceDbAnnHandle {
    store: OnceLock<Arc<VectorStore>>,
}

impl LanceDbAnnHandle {
    pub fn new() -> Self {
        Self {
            store: OnceLock::new(),
        }
    }

    pub fn install(&self, store: Arc<VectorStore>) -> Result<(), Arc<VectorStore>> {
        self.store.set(store)
    }

    pub fn store(&self) -> Option<Arc<VectorStore>> {
        self.store.get().cloned()
    }
}

impl Default for LanceDbAnnHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl AnnHandle for LanceDbAnnHandle {}
