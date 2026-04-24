//! Library-side indexing entry points so the logic can be tested without
//! the daemon binary scaffolding in `main.rs`.

use anyhow::Result;
use lixun_core::Document;
use lixun_sources::manifest::Manifest;
use lixun_sources::source::{Mutation as SourceMutation, MutationSink, SourceContext};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::index_service::{IndexMutationTx, Mutation, SearchHandle};
use crate::registry::SourceRegistry;
use crate::sources_api::IndexerSources;
use crate::writer_sink::WriterSink;

pub struct ReindexOutcome {
    pub total_docs: usize,
    pub fs_docs: usize,
    pub other_docs: usize,
}

pub async fn reindex_full(
    mutation_tx: &IndexMutationTx,
    config: &dyn IndexerSources,
    registry: &SourceRegistry,
    state_dir: &Path,
) -> Result<ReindexOutcome> {
    let state_dir_owned = state_dir.to_path_buf();
    let _ = std::fs::remove_file(state_dir_owned.join("manifest.json"));

    let fs_count = {
        let (batch_tx, mut batch_rx) = tokio::sync::mpsc::channel::<Vec<Document>>(2);
        let fs_source = config.build_fs_source()?;
        let empty_ids: HashSet<String> = HashSet::new();
        let runtime = tokio::runtime::Handle::current();
        let state_dir_for_walk = state_dir_owned.clone();

        let walk_task = tokio::task::spawn_blocking(move || -> Result<()> {
            let mut manifest = Manifest::default();
            let _deleted = fs_source.index_incremental_batched(
                &mut manifest,
                &empty_ids,
                |docs| {
                    runtime
                        .block_on(batch_tx.send(docs))
                        .map_err(|_| anyhow::anyhow!("reindex consumer closed"))?;
                    Ok(())
                },
            )?;
            manifest.save(&state_dir_for_walk);
            Ok(())
        });

        let mut fs_count = 0usize;
        while let Some(batch) = batch_rx.recv().await {
            fs_count += batch.len();
            mutation_tx.send(Mutation::UpsertMany(batch)).await?;
            let _ = mutation_tx.barrier().await?;
        }
        walk_task.await??;
        fs_count
    };

    let other_count = reindex_non_fs_from_registry(mutation_tx, registry).await?;
    mutation_tx.commit_now().await?;

    tracing::info!(
        "Full reindex: {} fs docs, {} other docs, manifest rebuilt",
        fs_count,
        other_count
    );

    Ok(ReindexOutcome {
        total_docs: fs_count + other_count,
        fs_docs: fs_count,
        other_docs: other_count,
    })
}

pub async fn reindex_paths(
    mutation_tx: &IndexMutationTx,
    config: &dyn IndexerSources,
    paths: &[std::path::PathBuf],
) -> Result<usize> {
    let mut all_docs: Vec<Document> = Vec::new();

    for path in paths {
        if path.is_file() {
            if let Ok(doc) = crate::index_service::index_file(path, config.max_file_size_mb()) {
                all_docs.push(doc);
            }
        } else if path.is_dir() {
            let source = lixun_sources::fs::FsSource::new(
                vec![path.clone()],
                config.exclude().to_vec(),
                config.max_file_size_mb(),
            );
            all_docs.extend(source.index_all()?);
        }
    }

    let count = all_docs.len();
    mutation_tx.send(Mutation::UpsertMany(all_docs)).await?;
    mutation_tx.commit_now().await?;
    Ok(count)
}

pub async fn run_incremental(
    mutation_tx: &IndexMutationTx,
    search: &SearchHandle,
    config: &dyn IndexerSources,
    registry: &SourceRegistry,
    state_dir: &Path,
    rebuilt_from_scratch: bool,
) -> Result<(usize, usize)> {
    let mut manifest = Manifest::load(state_dir);
    let indexed_ids: HashSet<String> = search.all_doc_ids().await.unwrap_or_else(|e| {
        tracing::warn!(
            "Could not enumerate existing index doc ids ({}); treating index as empty, will re-surface all manifest entries",
            e
        );
        HashSet::new()
    });
    tracing::info!(
        "Incremental indexer: manifest has {} entries, index has {} docs",
        manifest.len(),
        indexed_ids.len()
    );

    let fs_count = {
        let (batch_tx, mut batch_rx) = tokio::sync::mpsc::channel::<Vec<Document>>(2);
        let fs_source = config.build_fs_source()?;
        let indexed_ids_cloned = indexed_ids.clone();
        let manifest_for_walk = std::mem::take(&mut manifest);
        let runtime = tokio::runtime::Handle::current();

        let walk_task = tokio::task::spawn_blocking(move || -> Result<(Manifest, Vec<String>)> {
            let mut manifest = manifest_for_walk;
            let deleted = fs_source.index_incremental_batched(
                &mut manifest,
                &indexed_ids_cloned,
                |docs| {
                    runtime
                        .block_on(batch_tx.send(docs))
                        .map_err(|_| anyhow::anyhow!("indexer consumer closed"))?;
                    Ok(())
                },
            )?;
            Ok((manifest, deleted))
        });

        let mut fs_count = 0usize;
        while let Some(batch) = batch_rx.recv().await {
            fs_count += batch.len();
            mutation_tx.send(Mutation::UpsertMany(batch)).await?;
            let _ = mutation_tx.barrier().await?;
        }

        let (returned_manifest, deleted_ids) = walk_task.await??;
        manifest = returned_manifest;

        if !deleted_ids.is_empty() {
            let del_count = deleted_ids.len();
            mutation_tx.send(Mutation::DeleteMany(deleted_ids)).await?;
            let _ = mutation_tx.barrier().await?;
            tracing::info!(
                "Filesystem incremental: +{} docs, -{} deleted",
                fs_count,
                del_count
            );
        } else if fs_count > 0 {
            tracing::info!("Filesystem incremental: +{} docs", fs_count);
        }

        fs_count
    };

    let other_count = if rebuilt_from_scratch {
        reindex_non_fs_from_registry(mutation_tx, registry).await?
    } else {
        tracing::info!(
            "Incremental indexer: skipping non-fs reindex_full (warm start, {} instance(s) will catch up via on_fs_events/on_tick)",
            registry.instances.len()
        );
        0
    };

    manifest.save(state_dir);
    Ok((fs_count, other_count))
}

async fn reindex_non_fs_from_registry(
    mutation_tx: &IndexMutationTx,
    registry: &SourceRegistry,
) -> Result<usize> {
    let writer_sink: Arc<dyn MutationSink> = Arc::new(WriterSink::new(mutation_tx.clone()));
    let counter = Arc::new(AtomicUsize::new(0));
    let counting_sink: Arc<dyn MutationSink> =
        Arc::new(CountingSink::new(Arc::clone(&writer_sink), Arc::clone(&counter)));

    for inst in &registry.instances {
        if inst.source.kind() == "fs" {
            continue;
        }
        if !inst.source.reindex_on_schema_wipe() {
            tracing::info!(
                "Source instance {} opts out of schema-wipe reindex (explicit `lixun reindex` required)",
                inst.instance_id
            );
            continue;
        }
        let instance_id = inst.instance_id.clone();
        let state_dir = inst.state_dir.clone();
        let source = Arc::clone(&inst.source);
        let sink_for_task = Arc::clone(&counting_sink);
        let before = counter.load(Ordering::Relaxed);

        let (tx, rx) = tokio::sync::oneshot::channel::<Result<()>>();
        tokio::task::spawn_blocking(move || {
            let ctx = SourceContext {
                instance_id: &instance_id,
                state_dir: &state_dir,
            };
            let r = source.reindex_full(&ctx, sink_for_task.as_ref());
            let _ = tx.send(r);
        });
        rx.await??;
        mutation_tx.barrier().await?;
        let emitted = counter.load(Ordering::Relaxed) - before;
        tracing::info!(
            "Source instance {} reindexed ({} upserts)",
            inst.instance_id,
            emitted
        );
    }

    Ok(counter.load(Ordering::Relaxed))
}

struct CountingSink {
    inner: Arc<dyn MutationSink>,
    counter: Arc<AtomicUsize>,
}

impl CountingSink {
    fn new(inner: Arc<dyn MutationSink>, counter: Arc<AtomicUsize>) -> Self {
        Self { inner, counter }
    }
}

impl MutationSink for CountingSink {
    fn emit(&self, mutation: SourceMutation) -> Result<()> {
        match &mutation {
            SourceMutation::Upsert(_) => {
                self.counter.fetch_add(1, Ordering::Relaxed);
            }
            SourceMutation::UpsertMany(docs) => {
                self.counter.fetch_add(docs.len(), Ordering::Relaxed);
            }
            _ => {}
        }
        self.inner.emit(mutation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lixun_core::{Action, Category, DocId};
    use lixun_sources::IndexerSource;
    use lixun_sources::source::{MutationSink as SourceSink, WatchSpec};
    use std::path::PathBuf;
    use std::sync::Mutex;

    struct CaptureSink(Mutex<Vec<SourceMutation>>);

    impl SourceSink for CaptureSink {
        fn emit(&self, mutation: SourceMutation) -> Result<()> {
            self.0.lock().unwrap().push(mutation);
            Ok(())
        }
    }

    struct StubSource {
        kind: &'static str,
        emitted_for: Mutex<Vec<String>>,
    }

    impl IndexerSource for StubSource {
        fn kind(&self) -> &'static str {
            self.kind
        }
        fn watch_paths(&self, _ctx: &SourceContext) -> Result<Vec<WatchSpec>> {
            Ok(Vec::new())
        }
        fn reindex_full(&self, ctx: &SourceContext, sink: &dyn SourceSink) -> Result<()> {
            self.emitted_for
                .lock()
                .unwrap()
                .push(ctx.instance_id.to_string());
            sink.emit(SourceMutation::DeleteSourceInstance {
                instance_id: ctx.instance_id.to_string(),
            })?;
            let doc = Document {
                id: DocId(format!("{}:1", ctx.instance_id)),
                category: Category::File,
                title: "stub".into(),
                subtitle: String::new(),
                icon_name: None,
                kind_label: None,
                body: None,
                path: String::new(),
                mtime: 0,
                size: 0,
                action: Action::OpenFile {
                    path: PathBuf::from("/"),
                },
                extract_fail: false,
                sender: None,
                recipients: None,
                source_instance: ctx.instance_id.to_string(),
                secondary_action: None,
                extra: Vec::new(),
            };
            sink.emit(SourceMutation::Upsert(Box::new(doc)))?;
            Ok(())
        }
    }

    #[test]
    fn counting_sink_counts_only_upserts() {
        let inner: Arc<dyn MutationSink> = Arc::new(CaptureSink(Mutex::new(Vec::new())));
        let counter = Arc::new(AtomicUsize::new(0));
        let sink = CountingSink::new(Arc::clone(&inner), Arc::clone(&counter));

        sink.emit(SourceMutation::DeleteSourceInstance {
            instance_id: "x".into(),
        })
        .unwrap();
        assert_eq!(counter.load(Ordering::Relaxed), 0);

        let doc = Document {
            id: DocId("x:1".into()),
            category: Category::File,
            title: String::new(),
            subtitle: String::new(),
            icon_name: None,
            kind_label: None,
            body: None,
            path: String::new(),
            mtime: 0,
            size: 0,
            action: Action::OpenFile {
                path: PathBuf::from("/"),
            },
            extract_fail: false,
            sender: None,
            recipients: None,
            source_instance: "x".into(),
            secondary_action: None,
            extra: Vec::new(),
        };
        sink.emit(SourceMutation::Upsert(Box::new(doc))).unwrap();
        assert_eq!(counter.load(Ordering::Relaxed), 1);

        sink.emit(SourceMutation::Delete {
            doc_id: "x:1".into(),
        })
        .unwrap();
        assert_eq!(counter.load(Ordering::Relaxed), 1);

        let mk_doc = |id: &str| Document {
            id: DocId(id.into()),
            category: Category::File,
            title: String::new(),
            subtitle: String::new(),
            icon_name: None,
            kind_label: None,
            body: None,
            path: String::new(),
            mtime: 0,
            size: 0,
            action: Action::OpenFile {
                path: PathBuf::from("/"),
            },
            extract_fail: false,
            sender: None,
            recipients: None,
            source_instance: "x".into(),
            secondary_action: None,
            extra: Vec::new(),
        };
        sink.emit(SourceMutation::UpsertMany(vec![
            mk_doc("x:2"),
            mk_doc("x:3"),
            mk_doc("x:4"),
        ]))
        .unwrap();
        assert_eq!(
            counter.load(Ordering::Relaxed),
            4,
            "UpsertMany must add len to counter"
        );
    }

    #[test]
    fn stub_source_emits_delete_then_upsert() {
        let stub = Arc::new(StubSource {
            kind: "stub",
            emitted_for: Mutex::new(Vec::new()),
        });
        let sink = CaptureSink(Mutex::new(Vec::new()));
        let tmp = PathBuf::from("/tmp");
        let ctx = SourceContext {
            instance_id: "stub:1",
            state_dir: &tmp,
        };
        stub.reindex_full(&ctx, &sink).unwrap();
        let captured = sink.0.into_inner().unwrap();
        assert_eq!(captured.len(), 2);
        assert!(matches!(
            captured[0],
            SourceMutation::DeleteSourceInstance { .. }
        ));
        assert!(matches!(captured[1], SourceMutation::Upsert(_)));
        assert_eq!(
            stub.emitted_for.lock().unwrap().as_slice(),
            &["stub:1".to_string()]
        );
    }
}
