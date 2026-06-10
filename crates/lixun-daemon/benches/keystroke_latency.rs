use criterion::{Criterion, black_box, criterion_group, criterion_main};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn system_socket_path() -> PathBuf {
    lixun_ipc::socket_path()
}

fn connect_system_daemon() -> UnixStream {
    let path = system_socket_path();
    UnixStream::connect(&path).unwrap_or_else(|e| {
        panic!(
            "No lixund daemon available at {:?}. Start one before benchmarking: {}",
            path, e
        )
    })
}

fn encode_search(q: &str, limit: u32, epoch: u64) -> Vec<u8> {
    let req = lixun_ipc::Request::Search {
        q: q.to_string(),
        limit,
        explain: false,
        epoch,
    };
    let json = serde_json::to_vec(&req).unwrap();
    let total_len = (2 + json.len()) as u32;
    let mut buf = Vec::with_capacity(4 + 2 + json.len());
    buf.extend_from_slice(&total_len.to_be_bytes());
    buf.extend_from_slice(&4u16.to_be_bytes());
    buf.extend_from_slice(&json);
    buf
}

fn read_response(stream: &mut UnixStream) -> Option<serde_json::Value> {
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

fn search_roundtrip(stream: &mut UnixStream, q: &str, limit: u32, epoch: u64) -> Duration {
    let req = encode_search(q, limit, epoch);
    let start = Instant::now();
    stream.write_all(&req).unwrap();
    stream.flush().unwrap();

    loop {
        let resp = read_response(stream).expect("response frame");
        if let Some(phase) = resp.get("SearchChunk").and_then(|c| c.get("phase")) {
            if phase == "Final" {
                break;
            }
        } else {
            break;
        }
    }
    start.elapsed()
}

fn bench_cold_3char(c: &mut Criterion) {
    let mut group = c.benchmark_group("cold-3char");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(30));

    group.bench_function("cold-3char", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let mut stream = connect_system_daemon();
                total += search_roundtrip(&mut stream, "rep", 30, 1);
                drop(stream);
                std::thread::sleep(Duration::from_millis(50));
            }
            total
        });
    });

    group.finish();
}

fn bench_warm_3char(c: &mut Criterion) {
    let mut group = c.benchmark_group("warm-3char");
    group.sample_size(100);
    group.measurement_time(Duration::from_secs(60));

    let mut stream = connect_system_daemon();

    for i in 0..50u64 {
        let _ = search_roundtrip(&mut stream, "rep", 30, i);
    }

    group.bench_function("warm-3char", |b| {
        let mut epoch = 50u64;
        b.iter(|| {
            epoch += 1;
            black_box(search_roundtrip(&mut stream, "rep", 30, epoch))
        });
    });

    group.finish();
    drop(stream);
}

fn bench_warm_30char(c: &mut Criterion) {
    let mut group = c.benchmark_group("warm-30char");
    group.sample_size(100);
    group.measurement_time(Duration::from_secs(60));

    let query_30 = "abcdefghijklmnopqrstuvwxyzabcd";
    let mut stream = connect_system_daemon();

    for i in 0..50u64 {
        let _ = search_roundtrip(&mut stream, &query_30, 30, i);
    }

    group.bench_function("warm-30char", |b| {
        let mut epoch = 50u64;
        b.iter(|| {
            epoch += 1;
            black_box(search_roundtrip(&mut stream, &query_30, 30, epoch))
        });
    });

    group.finish();
    drop(stream);
}

fn bench_bursty_10keystrokes_200ms(c: &mut Criterion) {
    let mut group = c.benchmark_group("bursty-10keystrokes-200ms");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(30));

    let mut stream = connect_system_daemon();

    for i in 0..10u64 {
        let _ = search_roundtrip(&mut stream, "rep", 30, i);
    }

    group.bench_function("bursty-10keystrokes-200ms", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            let queries: Vec<String> = (1..=10).map(|i| "a".repeat(i)).collect();
            let delay_per_keystroke = Duration::from_millis(20);

            for _ in 0..iters {
                let burst_start = Instant::now();
                for (i, q) in queries.iter().enumerate() {
                    let epoch = (i + 1) as u64;
                    let _ = search_roundtrip(&mut stream, q, 30, epoch);
                    if i < queries.len() - 1 {
                        std::thread::sleep(delay_per_keystroke);
                    }
                }
                total += burst_start.elapsed();
            }
            total
        });
    });

    group.finish();
    drop(stream);
}

fn send_search(stream: &mut UnixStream, q: &str, limit: u32, epoch: u64) {
    let req = encode_search(q, limit, epoch);
    stream
        .write_all(&req)
        .expect("bench: write_all to daemon socket");
    stream.flush().expect("bench: flush daemon socket");
}

fn drain_until_final_or_cancelled(stream: &mut UnixStream, target_epoch: u64) -> Duration {
    let start = Instant::now();
    loop {
        let Some(resp) = read_response(stream) else {
            break;
        };
        if let Some(c) = resp.get("SearchChunk") {
            let phase_final = c.get("phase").map(|p| p == "Final").unwrap_or(false);
            let ep = c.get("epoch").and_then(|v| v.as_u64()).unwrap_or(0);
            if phase_final && ep == target_epoch {
                break;
            }
        } else if resp.get("Cancelled").is_some() {
            continue;
        }
    }
    start.elapsed()
}

fn bench_bursty_with_cancel(c: &mut Criterion) {
    let mut group = c.benchmark_group("bursty-with-cancel");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(30));

    let mut stream = connect_system_daemon();

    for i in 0..10u64 {
        let _ = search_roundtrip(&mut stream, "rep", 30, i);
    }

    group.bench_function("bursty-with-cancel", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            let queries: Vec<String> = (1..=11).map(|i| "a".repeat(i)).collect();
            let inter_keystroke = Duration::from_millis(50);

            for _ in 0..iters {
                let burst_start = Instant::now();
                for (i, q) in queries.iter().enumerate().take(10) {
                    let epoch = (i + 1) as u64;
                    send_search(&mut stream, q, 30, epoch);
                    std::thread::sleep(inter_keystroke);
                }
                send_search(&mut stream, &queries[10], 30, 11);
                let _ = drain_until_final_or_cancelled(&mut stream, 11);
                total += burst_start.elapsed();
                black_box(&queries);
            }
            total
        });
    });

    group.finish();
    drop(stream);
}

criterion_group!(
    benches,
    bench_cold_3char,
    bench_warm_3char,
    bench_warm_30char,
    bench_bursty_10keystrokes_200ms,
    bench_bursty_with_cancel
);
criterion_main!(benches);
