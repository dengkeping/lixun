use lixun_core::{PluginFieldSpec, RowMenuDef};
use lixun_mutation::{AnnHandle, CliManifest, CliVerb, DocStore, MutationBroadcaster};
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

    /// Collect every post-commit broadcaster contributed by a
    /// registered source. The daemon wraps the result in a
    /// [`lixun_mutation::MultiBroadcaster`] before passing it to the
    /// writer service, so a panic in one consumer cannot break the
    /// fan-out to the others.
    pub fn broadcasters(&self) -> Vec<Arc<dyn MutationBroadcaster>> {
        self.instances
            .iter()
            .filter_map(|inst| inst.source.broadcaster())
            .collect()
    }

    /// Pick the first ANN handle contributed by any registered
    /// source. The hybrid search layer is built around a single
    /// ANN provider per process; if a future deployment ships two
    /// sources with `ann_handle()` returning `Some`, the daemon
    /// should refuse to start rather than silently dropping one,
    /// but that validation lives in the daemon (WD-T7), not here.
    pub fn ann_handle(&self) -> Option<Arc<dyn AnnHandle>> {
        self.instances
            .iter()
            .find_map(|inst| inst.source.ann_handle())
    }

    /// Flatten every plugin's [`CliManifest`] into a single manifest
    /// for the host CLI. Verbs from different plugins are concatenated
    /// in registration order; collisions on the top-level verb name
    /// resolve in favour of the first registered plugin (lookup uses
    /// the same iteration order in [`Self::cli_invoke`]).
    pub fn cli_manifest(&self) -> CliManifest {
        let verbs: Vec<CliVerb> = self
            .instances
            .iter()
            .filter_map(|inst| inst.source.cli_manifest())
            .flat_map(|m| m.verbs.into_iter())
            .collect();
        CliManifest { verbs }
    }

    /// Dispatch a CLI verb to the plugin that declared its top-level
    /// name. Routing is by `verb_path[0]`: the registry walks every
    /// instance and asks for its `cli_manifest()`; the first instance
    /// whose manifest declares the leading verb name handles the
    /// invocation.
    pub async fn cli_invoke(
        &self,
        verb_path: &[String],
        args: &serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let head = verb_path
            .first()
            .ok_or_else(|| anyhow::anyhow!("empty verb path"))?;
        for inst in &self.instances {
            let Some(manifest) = inst.source.cli_manifest() else {
                continue;
            };
            if manifest.verbs.iter().any(|v| &v.name == head) {
                return inst.source.cli_invoke(verb_path, args).await;
            }
        }
        Err(anyhow::anyhow!("unknown verb: {}", head))
    }

    /// Install the daemon-owned [`DocStore`] view on every registered
    /// source. Plugins that override `install_doc_store` (currently
    /// only the semantic source, for backfill) capture the handle;
    /// every other plugin discards it via the trait default.
    pub fn install_doc_store(&self, store: Arc<dyn DocStore>) {
        for inst in &self.instances {
            inst.source.install_doc_store(store.clone());
        }
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
