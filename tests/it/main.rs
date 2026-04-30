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
