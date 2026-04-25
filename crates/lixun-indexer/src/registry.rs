use lixun_core::{PluginFieldSpec, RowMenuDef};
use lixun_sources::IndexerSource;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

pub struct SourceInstance {
    pub instance_id: String,
    pub state_dir: PathBuf,
    pub source: Arc<dyn IndexerSource>,
}

pub struct SourceRegistry {
    pub instances: Vec<SourceInstance>,
    pub plugin_fields_by_kind: BTreeMap<&'static str, &'static [PluginFieldSpec]>,
}

impl SourceRegistry {
    pub fn new() -> Self {
        Self {
            instances: Vec::new(),
            plugin_fields_by_kind: BTreeMap::new(),
        }
    }

    /// Look up the row-menu declaration for a given `instance_id`.
    ///
    /// Returns `None` if no instance with that id is registered. The daemon
    /// uses this to stamp `Hit::row_menu` after the plugin-agnostic search
    /// layer returns hits, preserving `AGENTS.md` hard-modularity (host
    /// never names concrete plugins; it dispatches by opaque instance_id).
    pub fn row_menu_for(&self, instance_id: &str) -> Option<RowMenuDef> {
        self.instances
            .iter()
            .find(|inst| inst.instance_id == instance_id)
            .map(|inst| inst.source.row_menu())
    }

    pub fn register(
        &mut self,
        instance_id: String,
        state_dir_root: &std::path::Path,
        source: Arc<dyn IndexerSource>,
    ) {
        let kind = source.kind();
        let fields = source.extra_fields();
        if !fields.is_empty() {
            self.plugin_fields_by_kind.entry(kind).or_insert(fields);
        }
        let state_dir = state_dir_root.join(kind).join(&instance_id);
        let _ = std::fs::create_dir_all(&state_dir);
        self.instances.push(SourceInstance {
            instance_id,
            state_dir,
            source,
        });
    }
}

impl Default for SourceRegistry {
    fn default() -> Self {
        Self::new()
    }
}
