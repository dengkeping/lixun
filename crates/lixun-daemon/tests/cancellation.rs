//! Integration test for per-keystroke cancellation through the collector.
//!
//! Run through the release QA script, which creates an isolated daemon,
//! configuration, runtime directory and synthetic corpus for these tests.
//!
//! The test verifies the supersede contract: when a `Search` with epoch=2
//! arrives while epoch=1 is still in-flight, the daemon emits
//! `Response::Cancelled { epoch: 1 }` before the final
//! `Response::SearchChunk { epoch: 2, phase: Final, .. }`.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

fn encode_search(q: &str, limit: u32, epoch: u64) -> Vec<u8> {
    let req = lixun_ipc::Request::Search {
        q: q.to_string(),
        limit,
        explain: false,
        epoch,
        category: None,
    };
    let json = serde_json::to_vec(&req).expect("serialize Request::Search");
    let total_len = (2 + json.len()) as u32;
    let mut buf = Vec::with_capacity(4 + 2 + json.len());
    buf.extend_from_slice(&total_len.to_be_bytes());
    buf.extend_from_slice(&4u16.to_be_bytes());
    buf.extend_from_slice(&json);
    buf
}

fn read_response(stream: &mut UnixStream) -> Option<lixun_ipc::Response> {
    let mut header = [0u8; 4];
    stream.read_exact(&mut header).ok()?;
    let frame_len = u32::from_be_bytes(header) as usize;
    if frame_len < 2 {
        return None;
    }
    let mut ver = [0u8; 2];
    stream.read_exact(&mut ver).ok()?;
    let payload_len = frame_len - 2;
    let mut payload = vec![0u8; payload_len];
    stream.read_exact(&mut payload).ok()?;
    serde_json::from_slice(&payload).ok()
}

#[test]
#[ignore = "requires a running lixund daemon; see file header for rationale"]
fn test_supersede_emits_cancelled() {
    let socket = lixun_ipc::socket_path();
    let mut stream = match UnixStream::connect(&socket) {
        Ok(s) => s,
        Err(e) => {
            panic!(
                "no lixund daemon at {:?}: {} (start the daemon, then re-run with --ignored)",
                socket, e
            );
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set read timeout");

    let req1 = encode_search("a", 50, 1);
    let req2 = encode_search("ab", 50, 2);
    stream.write_all(&req1).expect("write epoch=1");
    stream.write_all(&req2).expect("write epoch=2");
    stream.flush().expect("flush");

    let mut saw_cancelled_for_1 = false;
    let mut saw_final_for_2 = false;
    let mut cancelled_index: Option<usize> = None;
    let mut final2_index: Option<usize> = None;
    let mut idx: usize = 0;

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let Some(resp) = read_response(&mut stream) else {
            break;
        };
        match resp {
            lixun_ipc::Response::Cancelled { epoch: 1 } => {
                if !saw_cancelled_for_1 {
                    saw_cancelled_for_1 = true;
                    cancelled_index = Some(idx);
                }
            }
            lixun_ipc::Response::SearchChunk {
                epoch: 2,
                phase: lixun_ipc::Phase::Final,
                ..
            } => {
                saw_final_for_2 = true;
                final2_index = Some(idx);
                break;
            }
            _ => {}
        }
        idx += 1;
    }

    assert!(
        saw_cancelled_for_1,
        "expected at least one Response::Cancelled {{ epoch: 1 }} after supersede"
    );
    assert!(
        saw_final_for_2,
        "expected a final SearchChunk for epoch=2 within the deadline"
    );
    let (Some(ci), Some(fi)) = (cancelled_index, final2_index) else {
        panic!("response indices not recorded");
    };
    assert!(
        ci < fi,
        "Cancelled{{epoch:1}} (at index {}) must arrive before SearchChunk{{epoch:2, Final}} (at index {})",
        ci,
        fi
    );
}

#[test]
#[ignore = "requires the isolated release QA corpus"]
fn versioned_searches_return_file_application_and_plugin_hits() {
    use lixun_core::Category;
    use lixun_ipc::{Phase, Request, Response};
    for version in [4u16, 5, 6] {
        for (query, category) in [
            ("reference-0.txt", Category::File),
            ("Wirefixture", Category::App),
            ("=2+3", Category::Calculator),
        ] {
            let mut socket = UnixStream::connect(lixun_ipc::socket_path()).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let payload = lixun_ipc::encode_request_for_version(
                version,
                &Request::Search {
                    q: query.into(),
                    limit: 5,
                    explain: false,
                    epoch: 1,
                    category: None,
                },
            )
            .unwrap();
            socket
                .write_all(&((2 + payload.len()) as u32).to_be_bytes())
                .unwrap();
            socket.write_all(&version.to_be_bytes()).unwrap();
            socket.write_all(&payload).unwrap();
            loop {
                let mut header = [0; 6];
                socket.read_exact(&mut header).unwrap();
                assert_eq!(u16::from_be_bytes(header[4..].try_into().unwrap()), version);
                let len = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
                assert!((2..=lixun_ipc::MAX_FRAME_LEN).contains(&len));
                let mut bytes = vec![0; len - 2];
                socket.read_exact(&mut bytes).unwrap();
                match lixun_ipc::decode_response(version, &bytes).unwrap() {
                    Response::SearchChunk {
                        phase: Phase::Final,
                        hits,
                        ..
                    } => {
                        assert!(
                            hits.iter().any(|hit| hit.category == category),
                            "v{version} {query}"
                        );
                        break;
                    }
                    Response::SearchChunk { .. } => {}
                    other => panic!("unexpected response: {other:?}"),
                }
            }
        }
    }
    let mut socket = UnixStream::connect(lixun_ipc::socket_path()).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    socket.write_all(&[0, 0, 0, 2, 0, 99]).unwrap();
    assert!(
        matches!(read_response(&mut socket), Some(Response::Error(detail)) if detail.contains("unsupported"))
    );
}
