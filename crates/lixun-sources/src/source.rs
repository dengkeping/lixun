use anyhow::Result;
use lixun_core::{Document, Hit, PluginFieldSpec, RowMenuDef};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub struct SourceContext<'a> {
    pub instance_id: &'a str,
    pub state_dir: &'a Path,
}

/// Per-query context passed to `IndexerSource::on_query`. Mirrors
/// `SourceContext` but scoped to a single synchronous search. Plugins
/// may use `state_dir` for per-instance caches keyed by query.
pub struct QueryContext<'a> {
    pub instance_id: &'a str,
    pub state_dir: &'a Path,
}

pub struct WatchSpec {
    pub path: PathBuf,
    pub recursive: bool,
}

#[derive(Clone, Debug)]
pub struct SourceEvent {
    pub path: PathBuf,
    pub kind: SourceEventKind,
}

#[derive(Clone, Debug)]
pub enum SourceEventKind {
    Created,
    Modified,
    Removed,
    Renamed { from: PathBuf },
}

pub enum Mutation {
    Upsert(Box<Document>),
    UpsertMany(Vec<Document>),
    Delete { doc_id: String },
    DeleteSourceInstance { instance_id: String },
}

pub trait MutationSink: Send + Sync {
    fn emit(&self, mutation: Mutation) -> Result<()>;
}

pub trait IndexerSource: Send + Sync {
    fn kind(&self) -> &'static str;

    fn watch_paths(&self, _ctx: &SourceContext) -> Result<Vec<WatchSpec>> {
        Ok(Vec::new())
    }

    fn tick_interval(&self) -> Option<Duration> {
        None
    }

    fn on_tick(&self, _ctx: &SourceContext, _sink: &dyn MutationSink) -> Result<()> {
        Ok(())
    }

    fn on_fs_events(
        &self,
        _ctx: &SourceContext,
        _events: &[SourceEvent],
        _sink: &dyn MutationSink,
    ) -> Result<()> {
        Ok(())
    }

    fn reindex_full(&self, ctx: &SourceContext, sink: &dyn MutationSink) -> Result<()>;

    /// Whether this source should participate in the daemon-driven full
    /// reindex after a schema wipe. Return `false` for sources whose
    /// `reindex_full` is too expensive to run unattended (e.g. multi-minute
    /// full mbox scans). Such sources are expected to be reindexed on
    /// explicit user request (`lixun reindex`) only.
    fn reindex_on_schema_wipe(&self) -> bool {
        true
    }

    fn extra_fields(&self) -> &'static [PluginFieldSpec] {
        &[]
    }

    /// Synchronous per-query hook. Called once per search after the
    /// Tantivy pass returns and per-category multipliers have been
    /// applied. Plugins return extra `Hit`s with scores set
    /// explicitly (not subject to ranking multipliers). Default
    /// returns no hits — plugins that don't need query-time
    /// augmentation can ignore this method.
    fn on_query(&self, _query: &str, _ctx: &QueryContext) -> Vec<Hit> {
        Vec::new()
    }

    /// Whether `query` should be excluded from the frecency log.
    /// The daemon fans out to every plugin in `RecordQuery` /
    /// `RecordQueryClick` paths; if any plugin returns `true`, the
    /// query is not recorded. Used by calculator (`= 2+2`) and
    /// shell (`> ls`) prefixes to keep computational queries out of
    /// the frecency history. Default returns `false`.
    fn excludes_from_query_log(&self, _query: &str) -> bool {
        false
    }

    /// Declarative right-click menu for rows produced by this
    /// source. The GUI caches the translated menu model keyed by
    /// `Hit::source_instance`, so sources MUST return a stable,
    /// data-independent declaration (no per-hit branching). Items
    /// whose visibility depends on per-hit state should use
    /// [`RowMenuVisibility::RequiresSecondaryAction`] and rely on
    /// action enablement at bind time rather than inserting or
    /// omitting items.
    ///
    /// Default returns an empty menu, which tells the GUI to hide
    /// the context menu for rows from this source.
    fn row_menu(&self) -> RowMenuDef {
        RowMenuDef::empty()
    }

    /// Optional post-commit mutation broadcaster contributed by this
    /// source. The daemon collects every `Some` returned across all
    /// registered sources and routes committed mutations to each of
    /// them via [`lixun_mutation::MultiBroadcaster`]. Default returns
    /// `None`, so existing sources need no changes.
    fn broadcaster(&self) -> Option<Arc<dyn lixun_mutation::MutationBroadcaster>> {
        None
    }

    /// Optional ANN handle contributed by this source. The daemon
    /// uses the first `Some` it finds (single ANN provider per
    /// process); the hybrid search layer consults it alongside the
    /// lexical index. Default returns `None`.
    fn ann_handle(&self) -> Option<Arc<dyn lixun_mutation::AnnHandle>> {
        None
    }
}

pub struct PluginBuildContext {
    pub max_file_size_mb: u64,
    pub state_dir_root: PathBuf,
}

pub struct PluginInstance {
    pub instance_id: String,
    pub source: Arc<dyn IndexerSource>,
}

/// Config-driven registration of opt-in source plugins.
///
/// Daemon startup iterates over all `PluginFactoryEntry` values registered
/// via `inventory::submit!` across the workspace. For each factory whose
/// `section()` matches a top-level key in the user's config, `build()` is
/// called with the raw TOML subtree. Absent sections are skipped — the
/// factory never sees a `None`.
pub trait PluginFactory: Send + Sync {
    fn section(&self) -> &'static str;

    fn build(&self, raw: &toml::Value, ctx: &PluginBuildContext) -> Result<Vec<PluginInstance>>;
}

/// Compile-time plugin registration slot.
///
/// Each plugin crate submits one of these via `inventory::submit!` at
/// crate root. The daemon enumerates all submitted entries at startup
/// via `inventory::iter::<PluginFactoryEntry>` — no plugin names in
/// daemon code.
pub struct PluginFactoryEntry {
    pub new: fn() -> Box<dyn PluginFactory>,
}

inventory::collect!(PluginFactoryEntry);

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct CaptureSink(Mutex<Vec<Mutation>>);

    impl MutationSink for CaptureSink {
        fn emit(&self, m: Mutation) -> Result<()> {
            self.0.lock().unwrap().push(m);
            Ok(())
        }
    }

    struct StubSource;

    impl IndexerSource for StubSource {
        fn kind(&self) -> &'static str {
            "stub"
        }
        fn reindex_full(&self, ctx: &SourceContext, sink: &dyn MutationSink) -> Result<()> {
            sink.emit(Mutation::DeleteSourceInstance {
                instance_id: ctx.instance_id.to_string(),
            })?;
            sink.emit(Mutation::Delete {
                doc_id: "stub:1".into(),
            })?;
            Ok(())
        }
    }

    #[test]
    fn indexer_source_reindex_full_emits_expected_mutations() {
        let sink = CaptureSink(Mutex::new(Vec::new()));
        let tmp = std::path::PathBuf::from("/tmp");
        let ctx = SourceContext {
            instance_id: "s1",
            state_dir: &tmp,
        };
        StubSource.reindex_full(&ctx, &sink).unwrap();

        let collected = sink.0.into_inner().unwrap();
        assert_eq!(collected.len(), 2);
        match &collected[0] {
            Mutation::DeleteSourceInstance { instance_id } => assert_eq!(instance_id, "s1"),
            _ => panic!("first mutation must be DeleteSourceInstance"),
        }
        match &collected[1] {
            Mutation::Delete { doc_id } => assert_eq!(doc_id, "stub:1"),
            _ => panic!("second mutation must be Delete"),
        }
    }
}
