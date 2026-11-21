use lixun_daemon::config::{
    Config, ExtractConfig, GuiConfig, ImpactConfig, Keybindings, OcrConfig, PreviewConfig,
};
use lixun_daemon::index_service::spawn_writer_service;
use lixun_daemon::indexer;
use lixun_sources::manifest::Manifest;
use std::collections::BTreeMap;

fn test_config(root: std::path::PathBuf, state_dir: std::path::PathBuf) -> Config {
    Config {
        roots: vec![root],
        exclude: Vec::new(),
        exclude_regex: Vec::new(),
        max_file_size_mb: 1,
        extractor_timeout_secs: 1,
        max_results: 30,
        ranking_apps: 1.0,
        ranking_files: 1.0,
        ranking_mail: 1.0,
        ranking_attachments: 1.0,
        ranking_prefix_boost: 1.0,
        ranking_acronym_boost: 1.0,
        ranking_recency_weight: 0.0,
        ranking_recency_tau_days: 30.0,
        ranking_frecency_alpha: 0.0,
        ranking_latch_weight: 0.0,
        ranking_latch_cap: 1.0,
        ranking_total_multiplier_cap: 6.0,
        ranking_top_hit_min_confidence: 0.6,
        ranking_top_hit_min_margin: 1.3,
        ranking_strong_latch_threshold: 3,
        keybindings: Keybindings::default(),
        preview: PreviewConfig::default(),
        gui: GuiConfig::default(),
        extract: ExtractConfig::default(),
        ocr: OcrConfig::default(),
        impact: ImpactConfig::default(),
        search: lixun_daemon::config::SearchConfig::default(),
        state_dir,
        plugin_sections: BTreeMap::new(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reindex_full_rebuilds_manifest_and_drops_phantom_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    let real_file = root.join("real.txt");
    std::fs::write(&real_file, b"content").unwrap();

    let state_dir = tmp.path().join("state");
    std::fs::create_dir_all(&state_dir).unwrap();

    let mut stale = Manifest::default();
    stale.update("/tmp/phantom/does-not-exist.txt".to_string(), 12345);
    stale.update(real_file.to_string_lossy().to_string(), 0);
    stale.save(&state_dir);
    assert_eq!(Manifest::load(&state_dir).len(), 2);

    let index_dir = state_dir.join("index");
    std::fs::create_dir_all(&index_dir).unwrap();
    let index = lixun_index::LixunIndex::create_or_open(
        index_dir.to_str().unwrap(),
        lixun_core::RankingConfig::default(),
    )
    .unwrap();
    let (mutation_tx, search, _writer_handle) = spawn_writer_service(index).unwrap();

    let config = test_config(root.clone(), state_dir.clone());
    let sources = lixun_daemon::sources_glue::SourcesGlue::new(std::sync::Arc::new(config));
    let registry = lixun_indexer::SourceRegistry::new();
    let outcome = indexer::reindex_full(
        &mutation_tx,
        &sources,
        &registry,
        &state_dir,
        search.all_doc_ids().await.unwrap(),
    )
    .await
    .unwrap();

    assert!(outcome.fs_docs >= 1, "real file must be indexed");

    let after = Manifest::load(&state_dir);
    assert!(
        !after
            .known_paths()
            .any(|p| p == "/tmp/phantom/does-not-exist.txt"),
        "phantom entry must be dropped"
    );
    assert!(
        after
            .known_paths()
            .any(|p| p == real_file.to_string_lossy().as_ref()),
        "real file must be present"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rebuild_reconciles_committed_files_and_keeps_an_offline_root() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    let state = tmp.path().join("state");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&state).unwrap();
    let keep = root.join("keep.txt");
    let removed = root.join("removed.txt");
    std::fs::write(&keep, b"keep").unwrap();
    std::fs::write(&removed, b"drop").unwrap();
    let index = lixun_index::LixunIndex::create_or_open(
        state.join("index").to_str().unwrap(),
        Default::default(),
    )
    .unwrap();
    let (tx, search, _writer) = spawn_writer_service(index).unwrap();
    let sources = lixun_daemon::sources_glue::SourcesGlue::new(std::sync::Arc::new(test_config(
        root.clone(),
        state.clone(),
    )));
    let registry = lixun_indexer::SourceRegistry::new();
    let rebuild = || async {
        indexer::reindex_full(
            &tx,
            &sources,
            &registry,
            &state,
            search.all_doc_ids().await.unwrap(),
        )
        .await
        .unwrap();
    };
    rebuild().await;
    assert_eq!(search.all_doc_ids().await.unwrap().len(), 2);
    std::fs::remove_file(&removed).unwrap();
    Manifest::default().save_checked(&state).unwrap();
    rebuild().await;
    let keep_id = format!("fs:{}", keep.display());
    assert_eq!(
        search.all_doc_ids().await.unwrap(),
        [keep_id.clone()].into()
    );
    // A previously mounted subtree may still be a readable empty directory.
    let manifest_file = state.join("manifest.json");
    let original = std::fs::read(&manifest_file).unwrap();
    let mut saved: serde_json::Value = serde_json::from_slice(&original).unwrap();
    saved["mounts"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!(root));
    std::fs::write(&manifest_file, serde_json::to_vec(&saved).unwrap()).unwrap();
    std::fs::remove_file(&keep).unwrap();
    rebuild().await;
    assert!(search.all_doc_ids().await.unwrap().contains(&keep_id));
    std::fs::write(&manifest_file, original).unwrap();
    std::fs::rename(&root, tmp.path().join("offline")).unwrap();
    rebuild().await;
    assert_eq!(search.all_doc_ids().await.unwrap(), [keep_id].into());
}
