//! Integration test for per-keystroke cancellation through the collector.
//!
//! This test is `#[ignore]` by default because:
//!  * The daemon's search path is implemented inside `bin/lixund` (`src/main.rs`)
//!    and is not exposed through the `lixun_daemon` library, so we cannot spawn
//!    an in-process daemon without duplicating ~1200 lines of wiring.
//!  * Tantivy holds a single-writer lock on the index directory, so spinning a
//!    second daemon on a temporary directory would still fight the system
//!    daemon for any shared paths.
//!
//! Run manually against a live daemon with:
//!     cargo test -p lixun-daemon --test cancellation -- --ignored
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
