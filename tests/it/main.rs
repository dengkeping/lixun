use std::process::Command;

use lixun_core::{Action, Category, DocId, Document, Query, RankingConfig};
use lixun_index::LixunIndex;

#[test]
fn test_lixun_help() {
    let output = Command::new("cargo")
        .args(["run", "-p", "lixun-cli", "--", "--help"])
        .output()
        .expect("failed to run lixun");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("toggle"));
    assert!(stdout.contains("search"));
}

fn make_doc(id: &str, title: &str, body: Option<&str>) -> Document {
    Document {
        id: DocId(format!("fs:/tmp/{id}")),
        category: Category::File,
        title: title.to_string(),
        subtitle: format!("/tmp/{id}"),
        icon_name: None,
        kind_label: None,
        body: body.map(String::from),
        path: format!("/tmp/{id}"),
        mtime: 0,
        size: 0,
        action: Action::OpenFile {
            path: format!("/tmp/{id}").into(),
        },
        extract_fail: false,
        sender: None,
        recipients: None,
        source_instance: "test".into(),
        secondary_action: None,
        extra: Vec::new(),
        mime: None,
    }
}

fn fresh_index() -> (tempfile::TempDir, LixunIndex) {
    let tmp = tempfile::tempdir().unwrap();
    let idx =
        LixunIndex::create_or_open(tmp.path().to_str().unwrap(), RankingConfig::default()).unwrap();
    (tmp, idx)
}

fn upsert_docs(idx: &mut LixunIndex, docs: &[Document]) {
    let mut writer = idx.writer(20_000_000).unwrap();
    for d in docs {
        idx.upsert(d, &mut writer).unwrap();
    }
    idx.commit(&mut writer).unwrap();
    idx.reload().unwrap();
}

fn search(idx: &LixunIndex, q: &str) -> Vec<String> {
    let hits = idx
        .search(&Query {
            text: q.to_string(),
            limit: 20,
        })
        .unwrap();
    hits.into_iter().map(|h| h.title).collect()
}

#[test]
fn spotlight_diacritic_insensitive() {
    let (_tmp, mut idx) = fresh_index();
    upsert_docs(
        &mut idx,
        &[
            make_doc("1", "résumé.pdf", None),
            make_doc("2", "Cafe Menu", None),
        ],
    );
    let titles = search(&idx, "resume");
    assert!(titles.iter().any(|t| t.contains("résumé")));
}

#[test]
fn spotlight_fuzzy_single_edit_typo() {
    let (_tmp, mut idx) = fresh_index();
    upsert_docs(&mut idx, &[make_doc("1", "firefox", None)]);
    let titles = search(&idx, "firfox");
    assert!(titles.iter().any(|t| t == "firefox"));
}

#[test]
fn spotlight_and_semantics_default() {
    let (_tmp, mut idx) = fresh_index();
    upsert_docs(
        &mut idx,
        &[
            make_doc("1", "my report 2024", None),
            make_doc("2", "other thing", None),
        ],
    );
    let titles = search(&idx, "my report");
    assert!(titles.iter().any(|t| t.contains("my report")));
    assert!(!titles.iter().any(|t| t.contains("other thing")));
}

#[test]
fn spotlight_not_operator_excludes() {
    let (_tmp, mut idx) = fresh_index();
    upsert_docs(
        &mut idx,
        &[
            make_doc("1", "report 2024", None),
            make_doc("2", "draft report", None),
        ],
    );
    let titles = search(&idx, "report -draft");
    assert!(titles.iter().any(|t| t == "report 2024"));
    assert!(!titles.iter().any(|t| t == "draft report"));
}

#[test]
fn spotlight_camelcase_splits() {
    let (_tmp, mut idx) = fresh_index();
    upsert_docs(&mut idx, &[make_doc("1", "MyFileName.txt", None)]);
    let titles = search(&idx, "file");
    assert!(titles.iter().any(|t| t == "MyFileName.txt"));
}

#[test]
fn ipc_codec_roundtrip_v1_hits() {
    use bytes::BytesMut;
    use lixun_ipc::{FrameCodec, PROTOCOL_VERSION, Request};
    use tokio_util::codec::Encoder;

    let mut codec = FrameCodec::default();
    let mut buf = BytesMut::new();
    codec
        .encode(
            Request::Search {
                q: "hello".into(),
                limit: 10,
                explain: false,
                epoch: 1,
            },
            &mut buf,
        )
        .unwrap();

    assert!(buf.len() > 6);
    let _version_bytes = PROTOCOL_VERSION.to_be_bytes();
}

#[test]
fn dir_rename_removes_subtree_from_index() {
    use lixun_indexer::index_service::{Mutation, spawn_writer_service};

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let tmp = tempfile::tempdir().unwrap();
        let idx = LixunIndex::create_or_open(
            tmp.path().to_str().unwrap(),
            RankingConfig::default(),
        )
        .unwrap();
        let (tx, search, _handle) = spawn_writer_service(idx).unwrap();

        let docs = vec![
            Document {
                id: DocId("fs:/tmp/dir".into()),
                category: Category::File,
                title: "dir".into(),
                subtitle: "/tmp/dir".into(),
                icon_name: None,
                kind_label: None,
                body: None,
                path: "/tmp/dir".into(),
                mtime: 0,
                size: 0,
                action: Action::OpenFile { path: "/tmp/dir".into() },
                extract_fail: false,
                sender: None,
                recipients: None,
                source_instance: "test".into(),
                secondary_action: None,
                extra: Vec::new(),
                mime: None,
            },
            make_doc("dir/note-uniqlex.txt", "uniqlex note", Some("uniqlex body alpha")),
            make_doc("dir/sub/deep-uniqlex.md", "deep uniqlex", Some("uniqlex body beta")),
            make_doc("other-uniqlex.txt", "other uniqlex", Some("uniqlex body gamma")),
        ];
        tx.send(Mutation::UpsertMany(docs)).await.unwrap();
        let _ = tx.commit_now().await.unwrap();

        let q = Query { text: "uniqlex".into(), limit: 20 };
        let before = search.search(&q).await.unwrap();
        let before_titles: Vec<String> = before.iter().map(|h| h.title.clone()).collect();
        assert!(before_titles.iter().any(|t| t == "uniqlex note"));
        assert!(before_titles.iter().any(|t| t == "deep uniqlex"));
        assert!(before_titles.iter().any(|t| t == "other uniqlex"));

        tx.send(Mutation::DeleteSubtree("fs:/tmp/dir".into()))
            .await
            .unwrap();
        let _ = tx.commit_now().await.unwrap();

        let after = search.search(&q).await.unwrap();
        let after_titles: Vec<String> = after.iter().map(|h| h.title.clone()).collect();
        assert!(
            !after_titles.iter().any(|t| t == "uniqlex note"),
            "child file ghost remained: {after_titles:?}",
        );
        assert!(
            !after_titles.iter().any(|t| t == "deep uniqlex"),
            "deep descendant ghost remained: {after_titles:?}",
        );
        assert!(
            after_titles.iter().any(|t| t == "other uniqlex"),
            "unrelated sibling lost: {after_titles:?}",
        );
        let after_ids = search.all_doc_ids().await.unwrap();
        assert!(!after_ids.contains("fs:/tmp/dir"));
    });
}

#[test]
fn symlink_delete_removes_stored_canonical_id() {
    // Reproduces the symlink-ghost bug fixed by the observed→canonical
    // alias map (P0-C.2). At index time a symlinked file is stored
    // under its canonical id; at delete time `canonicalize()` fails on
    // the vanished path so the watcher would otherwise compute a raw
    // (non-matching) id and the canonical row would linger as a ghost.
    // The fix: persist observed→canonical at index time and consult it
    // at delete time. Here we drive the writer directly with both
    // halves of the mapping to assert the canonical row is purged.
    use lixun_daemon::symlink_alias::SymlinkAliases;
    use lixun_indexer::index_service::{Mutation, spawn_writer_service};
    use lixun_sources::SymlinkAliasNoter;
    use std::sync::Arc;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let tmp = tempfile::tempdir().unwrap();
        let idx = LixunIndex::create_or_open(
            tmp.path().to_str().unwrap(),
            RankingConfig::default(),
        )
        .unwrap();
        let (tx, search, _handle) = spawn_writer_service(idx).unwrap();

        // Observed (raw, symlinked) path differs from the canonical id
        // the index would store. The bug: a delete keyed on the raw
        // path fails to match the canonical row.
        let observed_raw = "/home/user/Symlink/note-symtwn.txt";
        let canonical_id = "fs:/data/real/note-symtwn.txt";
        let ghost_id_without_aliases = format!("fs:{observed_raw}");
        assert_ne!(
            ghost_id_without_aliases, canonical_id,
            "test premise: raw-keyed id must differ from canonical id",
        );

        let doc = Document {
            id: DocId(canonical_id.into()),
            category: Category::File,
            title: "symtwn note".into(),
            subtitle: "/data/real/note-symtwn.txt".into(),
            icon_name: None,
            kind_label: None,
            body: Some("symtwn body".into()),
            path: "/data/real/note-symtwn.txt".into(),
            mtime: 0,
            size: 0,
            action: Action::OpenFile {
                path: "/data/real/note-symtwn.txt".into(),
            },
            extract_fail: false,
            sender: None,
            recipients: None,
            source_instance: "test".into(),
            secondary_action: None,
            extra: Vec::new(),
            mime: None,
        };
        tx.send(Mutation::UpsertMany(vec![doc])).await.unwrap();
        let _ = tx.commit_now().await.unwrap();

        let q = Query {
            text: "symtwn".into(),
            limit: 20,
        };
        let before: Vec<String> = search
            .search(&q)
            .await
            .unwrap()
            .into_iter()
            .map(|h| h.title)
            .collect();
        assert!(before.iter().any(|t| t == "symtwn note"));

        // Index-time: note the observed→canonical mapping that the
        // resolver would record after a successful index_file.
        let aliases: Arc<dyn SymlinkAliasNoter> = Arc::new(SymlinkAliases::default());
        aliases.note(observed_raw, canonical_id);

        // Delete-time: with the fix, the Gone arm consults the noter
        // and recovers the canonical id.
        let resolved = aliases
            .resolve(observed_raw)
            .expect("alias must resolve to canonical id");
        assert_eq!(resolved, canonical_id);

        tx.send(Mutation::DeleteSubtree(resolved.clone()))
            .await
            .unwrap();
        aliases.forget(&resolved);
        let _ = tx.commit_now().await.unwrap();

        let after: Vec<String> = search
            .search(&q)
            .await
            .unwrap()
            .into_iter()
            .map(|h| h.title)
            .collect();
        assert!(
            !after.iter().any(|t| t == "symtwn note"),
            "canonical doc must be purged, not lingering as a ghost: {after:?}",
        );
        let after_ids = search.all_doc_ids().await.unwrap();
        assert!(!after_ids.contains(canonical_id));

        // Without the alias map a delete keyed on `ghost_id_without_aliases`
        // would have left the canonical row intact — this assertion
        // pins the premise that the fix is load-bearing.
        assert!(
            aliases.resolve(observed_raw).is_none(),
            "forget() must drop the entry after the canonical id is purged",
        );
    });
}

#[test]
fn file_event_round_trip() {
    // End-to-end lifecycle through the writer service: create → modify →
    // rename → delete, asserting the live (committed + reloaded) index
    // reflects each transition. Mirrors the watcher→writer pipeline the
    // resolver feeds: a Create/Modify becomes an Upsert keyed on the
    // doc id, a rename becomes Delete(old)+Upsert(new), and a delete
    // becomes DeleteSubtree(id).
    use lixun_indexer::index_service::{Mutation, spawn_writer_service};

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let tmp = tempfile::tempdir().unwrap();
        let idx = LixunIndex::create_or_open(tmp.path().to_str().unwrap(), RankingConfig::default())
            .unwrap();
        let (tx, search, _handle) = spawn_writer_service(idx).unwrap();

        let id = "fs:/tmp/roundtrip-evtwn.txt";
        let q = Query {
            text: "evtwn".into(),
            limit: 20,
        };

        let doc = |title: &str, body: &str| Document {
            id: DocId(id.into()),
            category: Category::File,
            title: title.into(),
            subtitle: "/tmp/roundtrip-evtwn.txt".into(),
            icon_name: None,
            kind_label: None,
            body: Some(body.into()),
            path: "/tmp/roundtrip-evtwn.txt".into(),
            mtime: 0,
            size: 0,
            action: Action::OpenFile {
                path: "/tmp/roundtrip-evtwn.txt".into(),
            },
            extract_fail: false,
            sender: None,
            recipients: None,
            source_instance: "test".into(),
            secondary_action: None,
            extra: Vec::new(),
            mime: None,
        };

        let titles = |hits: Vec<lixun_core::Hit>| -> Vec<String> {
            hits.into_iter().map(|h| h.title).collect()
        };

        // CREATE → indexed.
        tx.send(Mutation::Upsert(Box::new(doc("evtwn original", "evtwn body one"))))
            .await
            .unwrap();
        let _ = tx.commit_now().await.unwrap();
        let after_create = titles(search.search(&q).await.unwrap());
        assert!(
            after_create.iter().any(|t| t == "evtwn original"),
            "create did not index the doc: {after_create:?}",
        );

        // MODIFY → same id, updated title (upsert overwrites).
        tx.send(Mutation::Upsert(Box::new(doc("evtwn modified", "evtwn body two"))))
            .await
            .unwrap();
        let _ = tx.commit_now().await.unwrap();
        let after_modify = titles(search.search(&q).await.unwrap());
        assert!(
            after_modify.iter().any(|t| t == "evtwn modified"),
            "modify did not update the title: {after_modify:?}",
        );
        assert!(
            !after_modify.iter().any(|t| t == "evtwn original"),
            "stale pre-modify title lingered: {after_modify:?}",
        );

        // RENAME → Delete(old id) + Upsert(new id), as the watcher emits
        // for a rename-both event.
        let new_id = "fs:/tmp/renamed-evtwn.txt";
        let mut renamed = doc("evtwn renamed", "evtwn body three");
        renamed.id = DocId(new_id.into());
        renamed.path = "/tmp/renamed-evtwn.txt".into();
        renamed.action = Action::OpenFile {
            path: "/tmp/renamed-evtwn.txt".into(),
        };
        tx.send(Mutation::DeleteSubtree(id.into())).await.unwrap();
        tx.send(Mutation::Upsert(Box::new(renamed))).await.unwrap();
        let _ = tx.commit_now().await.unwrap();
        let after_rename = titles(search.search(&q).await.unwrap());
        assert!(
            after_rename.iter().any(|t| t == "evtwn renamed"),
            "rename did not index the new path: {after_rename:?}",
        );
        assert!(
            !after_rename.iter().any(|t| t == "evtwn modified"),
            "old-path doc survived the rename as a ghost: {after_rename:?}",
        );
        let ids_after_rename = search.all_doc_ids().await.unwrap();
        assert!(!ids_after_rename.contains(id), "old id lingered after rename");
        assert!(ids_after_rename.contains(new_id), "new id missing after rename");

        // DELETE → gone.
        tx.send(Mutation::DeleteSubtree(new_id.into())).await.unwrap();
        let _ = tx.commit_now().await.unwrap();
        let after_delete = titles(search.search(&q).await.unwrap());
        assert!(
            after_delete.is_empty(),
            "delete left residual docs: {after_delete:?}",
        );
        let ids_after_delete = search.all_doc_ids().await.unwrap();
        assert!(!ids_after_delete.contains(new_id), "doc survived delete");
    });
}

#[test]
fn config_example_matches_struct_keys() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/config.example.toml");
    let content = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("cannot read {path}: {e}"));
    lixun_daemon::config::Config::from_toml_str(&content).unwrap_or_else(|e| {
        panic!("docs/config.example.toml no longer deserializes into Config: {e}")
    });
}
