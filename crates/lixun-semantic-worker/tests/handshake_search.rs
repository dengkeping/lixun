//! Phase-1 acceptance test: spawns the worker binary, drives a full
//! handshake + empty-corpus search round-trip, asks for shutdown,
//! confirms a clean exit.
//!
//! NOTE: first run downloads the fastembed model (~120 MB for
//! `bge-small-en-v1.5`) into the per-test cache; budget 3-5 min.
//! Subsequent runs reuse the on-disk cache and finish in seconds.

use std::process::Stdio;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tempfile::tempdir;
use tokio::net::UnixListener;
use tokio::process::Command;
use tokio::time::timeout;
use tokio_util::codec::Framed;

use lixun_semantic_proto::{Cmd, DaemonCodec, Msg, PROTOCOL_VERSION};

const WORKER_BIN: &str = env!("CARGO_BIN_EXE_lixun-semantic-worker");

/* JSON equality is the only practical comparator: `Msg` and `Hit`
intentionally do not derive `PartialEq` because they carry payload
types from `lixun-core` that don't either. Round-tripping through
serde proves structural equality across the wire. */
fn msg_json(m: &Msg) -> String {
    serde_json::to_string(m).expect("serialise Msg")
}

#[tokio::test]
async fn handshake_then_search_then_shutdown() {
    let tmp = tempdir().expect("tempdir");
    let socket_path = tmp.path().join("worker.sock");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("data dir");

    let listener = UnixListener::bind(&socket_path).expect("bind");

    let mut child = Command::new(WORKER_BIN)
        .arg("--socket")
        .arg(&socket_path)
        .env("LIXUN_SEMANTIC_DATA_DIR", &data_dir)
        .env("LIXUN_LOG", "warn")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn worker");

    /* The worker downloads the embedder model on first run; that
    happens after it accepts the connection but before HandshakeOk
    comes back, so accept() must complete fast and the read for
    HandshakeOk must tolerate a multi-minute wait. */
    let (stream, _) = timeout(Duration::from_secs(10), listener.accept())
        .await
        .expect("accept timed out")
        .expect("accept");
    let mut framed = Framed::new(stream, DaemonCodec::new());

    framed
        .send(Cmd::Handshake {
            proto_version: PROTOCOL_VERSION,
        })
        .await
        .expect("send handshake");

    let ack = timeout(Duration::from_secs(600), framed.next())
        .await
        .expect("handshake ack timeout")
        .expect("stream ended")
        .expect("decode handshake ack");
    let expected_ack = Msg::HandshakeOk {
        proto_version: PROTOCOL_VERSION,
        worker_version: env!("CARGO_PKG_VERSION").into(),
    };
    assert_eq!(
        msg_json(&ack),
        msg_json(&expected_ack),
        "handshake ack shape mismatch"
    );

    framed
        .send(Cmd::SearchText {
            req_id: 1,
            query: "hello".into(),
            k: 5,
        })
        .await
        .expect("send search");

    let result = timeout(Duration::from_secs(60), framed.next())
        .await
        .expect("search reply timeout")
        .expect("stream ended")
        .expect("decode search reply");
    let expected_result = Msg::SearchResult {
        req_id: 1,
        hits: Vec::new(),
    };
    assert_eq!(
        msg_json(&result),
        msg_json(&expected_result),
        "empty-corpus search must return zero hits"
    );

    framed.send(Cmd::Shutdown).await.expect("send shutdown");
    drop(framed);

    let status = timeout(Duration::from_secs(30), child.wait())
        .await
        .expect("worker did not exit within 30s")
        .expect("wait child");
    assert!(status.success(), "worker exited with {status:?}");
}
