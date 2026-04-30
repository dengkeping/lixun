//! Phase-3 end-to-end backfill test.
//!
//! Exercises the full callback cycle: spawn the worker via the
//! supervisor, install a mock `DocStore` with 100 synthetic
//! documents, trigger `semantic backfill` over the IPC stub, then
//! assert that a semantic search for a unique phrase recovers the
//! expected document from the top-3 results.
//!
//! Wall-time budget: ≤ 5 min on a clean fastembed cache (model
//! download dominates first run); a few seconds on warm cache.
//! Requires the worker binary at `target/release/lixun-semantic-worker`
//! — Phase 1's tests already build it; this file panics with a
//! clear message if it is missing.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;
use lixun_core::{Action, Category, DocId, Hit, RowMenuDef, ScoreBreakdown};
use lixun_daemon::semantic_supervisor;
use lixun_mutation::DocStore;
use lixun_source_semantic_stub::{SemanticIpcFactory, install_doc_store, is_connected};
use lixun_sources::{IndexerSource, PluginBuildContext, PluginFactory};
use serde_json::Value;
use tokio::time::{sleep, timeout};

const UNIQUE_DOC_ID: &str = "doc-zephyr";
const UNIQUE_PHRASE: &str = "The quick zephyr jumps over a lazy zebra near a dusty xylophone";

fn worker_bin() -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace_root = Path::new(manifest_dir)
        .parent()
        .and_then(Path::parent)
        .expect("workspace root");
    if let Some(env) = std::env::var_os("LIXUN_SEMANTIC_WORKER") {
        let p = PathBuf::from(env);
        if p.exists() {
            return p;
        }
    }
    let candidate = workspace_root
        .join("target")
        .join("release")
        .join("lixun-semantic-worker");
    if !candidate.exists() {
        panic!(
            "worker binary not found at {}; \
             run `cargo build -p lixun-semantic-worker --release` first",
            candidate.display()
        );
    }
    candidate
}

/// Synthetic 100-document corpus. One doc carries [`UNIQUE_PHRASE`];
/// the other 99 use generic short bodies that don't share its
/// semantic neighbourhood, so the embedder's cosine top-1 for the
/// query must be the unique doc.
struct MockCorpus {
    docs: HashMap<String, (Hit, String)>,
}

impl MockCorpus {
    fn new() -> Self {
        let mut docs = HashMap::new();
        docs.insert(
            UNIQUE_DOC_ID.to_string(),
            (
                mock_hit(UNIQUE_DOC_ID, "Zephyr Notes"),
                UNIQUE_PHRASE.to_string(),
            ),
        );
        for i in 0..99 {
            let id = format!("doc-{i:03}");
            let title = format!("Notes {i}");
            /* Body content is deliberately generic and topically
            distant from the unique phrase. Mixing across distinct
            topics makes the test less sensitive to embedder
            release-to-release drift than a single repeated theme
            would. */
            let body = match i % 4 {
                0 => format!(
                    "Meeting notes for project alpha covering quarterly \
                     budget review and headcount planning, item {i}."
                ),
                1 => format!(
                    "Recipe for sourdough bread, hydration ratio and bulk \
                     fermentation timing, attempt {i}."
                ),
                2 => format!(
                    "Trip report from the highlands, weather conditions \
                     and route notes, day {i}."
                ),
                _ => format!(
                    "Shopping list with milk, eggs, butter, and \
                     miscellaneous staples, week {i}."
                ),
            };
            docs.insert(id.clone(), (mock_hit(&id, &title), body));
        }
        Self { docs }
    }
}

fn mock_hit(id: &str, title: &str) -> Hit {
    Hit {
        id: DocId(id.to_string()),
        category: Category::File,
        title: title.to_string(),
        subtitle: String::new(),
        icon_name: None,
        kind_label: None,
        score: 0.0,
        action: Action::OpenUri {
            uri: format!("test:{id}"),
        },
        extract_fail: false,
        sender: None,
        recipients: None,
        body: None,
        secondary_action: None,
        source_instance: "mock".to_string(),
        row_menu: RowMenuDef::empty(),
        mime: None,
    }
}

#[async_trait]
impl DocStore for MockCorpus {
    async fn all_doc_ids(&self) -> Result<HashSet<String>> {
        Ok(self.docs.keys().cloned().collect())
    }

    async fn hydrate_doc(&self, doc_id: &str) -> Result<Option<(Hit, ScoreBreakdown)>> {
        Ok(self
            .docs
            .get(doc_id)
            .map(|(hit, _)| (hit.clone(), ScoreBreakdown::default())))
    }

    async fn get_body(&self, doc_id: &str) -> Result<Option<String>> {
        Ok(self.docs.get(doc_id).map(|(_, body)| body.clone()))
    }
}

fn stub_source() -> Arc<dyn IndexerSource> {
    let factory = SemanticIpcFactory;
    let ctx = PluginBuildContext {
        max_file_size_mb: 1,
        state_dir_root: std::env::temp_dir(),
        impact: Arc::new(lixun_core::ImpactProfile::from_level(
            lixun_core::SystemImpact::Low,
            1,
        )),
    };
    let raw: toml::Value = toml::from_str("enabled = true").expect("test fixture parses");
    factory
        .build(&raw, &ctx)
        .expect("stub factory")
        .into_iter()
        .next()
        .expect("stub registered no instance — env not set?")
        .source
}

async fn wait_until_connected(deadline: Duration) {
    let start = Instant::now();
    while !is_connected() {
        if start.elapsed() >= deadline {
            panic!(
                "supervisor did not handshake within {}s",
                deadline.as_secs()
            );
        }
        sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backfill_e2e_completes_and_search_recovers_unique_doc() {
    let bin = worker_bin();
    let tmp = tempfile::tempdir().expect("tempdir");
    unsafe {
        std::env::set_var("LIXUN_SEMANTIC_DATA_DIR", tmp.path());
        std::env::set_var("LIXUN_SEMANTIC_WORKER", &bin);
    }

    let supervisor = tokio::spawn(semantic_supervisor::supervise(bin.clone()));
    wait_until_connected(Duration::from_secs(180)).await;

    /* Build the stub source THROUGH the inventory factory so the
    install_doc_store override fires the same way it would inside
    the real daemon. */
    let source = stub_source();
    install_doc_store(Arc::new(MockCorpus::new()) as Arc<dyn DocStore>);
    /* The trait's install_doc_store on the stub also forwards into
    the global slot; calling it explicitly above is belt-and-braces
    so test setup is independent of plugin-build ordering. */
    source.install_doc_store(Arc::new(MockCorpus::new()) as Arc<dyn DocStore>);

    let backfill_result = timeout(
        Duration::from_secs(300),
        source.cli_invoke(
            &["semantic".to_string(), "backfill".to_string()],
            &Value::Null,
        ),
    )
    .await
    .expect("backfill exceeded 5 min")
    .expect("backfill returned Err");

    let submitted = backfill_result
        .get("submitted")
        .and_then(Value::as_u64)
        .expect("backfill response missing 'submitted'");
    let total = backfill_result
        .get("total")
        .and_then(Value::as_u64)
        .expect("backfill response missing 'total'");
    assert_eq!(total, 100, "DocStore reported wrong total");
    assert_eq!(submitted, 100, "every doc should have been submitted");

    let ann = source.ann_handle().expect("stub provides ann_handle");

    /* The worker embeds asynchronously after start_backfill returns;
    poll the ANN until the unique doc appears in the top-3 or the
    deadline fires. flush_ms is 2s in SemanticConfig::default(),
    so the first hit should land within ~5s after the last submit. */
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if Instant::now() >= deadline {
            panic!("unique doc never appeared in top-3 for query '{UNIQUE_PHRASE}'");
        }
        let hits = timeout(Duration::from_secs(15), ann.search_text(UNIQUE_PHRASE, 3))
            .await
            .expect("ANN search hung")
            .expect("ANN search returned Err");
        if hits.iter().any(|h| h.doc_id == UNIQUE_DOC_ID) {
            supervisor.abort();
            return;
        }
        sleep(Duration::from_millis(500)).await;
    }
}
