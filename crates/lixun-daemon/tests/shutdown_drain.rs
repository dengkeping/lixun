use lixun_core::{Action, Category, DocId, Document, Query, RankingConfig};
use lixun_daemon::index_service::{
    DEFAULT_WRITER_HEAP_BYTES, Mutation, spawn_writer_service_with_broadcaster,
};
use lixun_index::LixunIndex;
use lixun_mutation::NoopBroadcaster;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

fn test_document(id: &str, path: &Path, token: &str) -> Document {
    Document {
        id: DocId(id.to_string()),
        category: Category::File,
        title: token.to_string(),
        subtitle: path.to_string_lossy().to_string(),
        icon_name: None,
        kind_label: None,
        body: Some(format!("body {token}")),
        path: path.to_string_lossy().to_string(),
        mtime: 0,
        size: 100,
        action: Action::OpenFile {
            path: path.to_path_buf(),
        },
        extract_fail: false,
        sender: None,
        recipients: None,
        source_instance: "shutdown-drain-test".into(),
        extra: Vec::new(),
        secondary_action: None,
        mime: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_drains_pending_upsert_within_timeout() {
    let tempdir = tempfile::tempdir().unwrap();
    let index_path = tempdir.path().join("index");
    let index = LixunIndex::create_or_open(&index_path, RankingConfig::default()).unwrap();

    let (mutation_tx, _search, writer_handle) = spawn_writer_service_with_broadcaster(
        index,
        Arc::new(NoopBroadcaster),
        DEFAULT_WRITER_HEAP_BYTES,
        1,
    )
    .unwrap();

    let mut expected = Vec::new();
    for n in 0..50 {
        let token = format!("shutdowndrainterm{n:02}");
        let path = tempdir.path().join(format!("doc-{n:02}.txt"));
        let id = format!("fs:{}", path.display());
        mutation_tx
            .send(Mutation::Upsert(Box::new(test_document(
                &id, &path, &token,
            ))))
            .await
            .unwrap();
        expected.push((id, token));
    }

    mutation_tx.send(Mutation::Shutdown).await.unwrap();
    tokio::time::timeout(Duration::from_secs(30), writer_handle)
        .await
        .expect("writer should drain within timeout")
        .expect("writer task should not panic");

    let reopened = LixunIndex::create_or_open(&index_path, RankingConfig::default()).unwrap();
    reopened.reload().unwrap();

    for (id, token) in expected {
        let hits = reopened
            .search(&Query {
                text: token,
                limit: 10,
            })
            .unwrap();
        assert!(
            hits.iter().any(|hit| hit.id.0 == id),
            "committed index should contain {id} after shutdown drain"
        );
    }

    let daemon_main =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs")).unwrap();
    assert!(
        !daemon_main.contains("_writer_handle"),
        "daemon shutdown must retain the writer handle so it can drain"
    );
    assert!(
        daemon_main.contains("index_service::Mutation::Shutdown")
            || daemon_main.contains("lixun_indexer::Mutation::Shutdown"),
        "daemon shutdown must signal the writer before awaiting it"
    );
    assert!(
        daemon_main.contains("shutdown: writer drain complete"),
        "daemon shutdown should surface a successful writer drain"
    );
    assert!(
        !daemon_main.contains("std::process::exit(0)"),
        "daemon shutdown should return through main instead of killing the process"
    );
}
