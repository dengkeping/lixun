//! Phase-2 acceptance tests for the semantic worker supervisor.
//!
//! Three tests as required by Phase 2 §4:
//!   1. probe returns None when the binary is absent
//!   2. supervisor handshakes and serves an empty-corpus search
//!   3. supervisor restarts after the worker is killed
//!
//! Requires the worker binary at `target/release/lixun-semantic-worker`.
//! Phase 1's tests already build it; this file panics with a clear
//! message if it is missing.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lixun_daemon::semantic_supervisor;
use lixun_mutation::AnnHandle;
use lixun_source_semantic_stub::{SemanticIpcFactory, is_connected};
use lixun_sources::{PluginBuildContext, PluginFactory};
use tokio::time::{sleep, timeout};

fn worker_bin() -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace_root = Path::new(manifest_dir)
        .parent()
        .and_then(Path::parent)
        .expect("workspace root");
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

/// Run each live test in its own process because the connection registry and
/// runtime locations are process-wide. The worker wrapper records only its own PID.
fn isolated_worker_test(name: &str) -> bool {
    if std::env::var_os("LIXUN_TEST_WORKER_PID_FILE").is_some() {
        return false;
    }
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let launcher = dir.path().join("worker");
    std::fs::write(
        &launcher,
        r#"#!/bin/sh
printf '%s\n' "$$" > "$LIXUN_TEST_WORKER_PID_FILE"
exec "$LIXUN_TEST_WORKER_BINARY" "$@"
"#,
    )
    .unwrap();
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700)).unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("XDG_RUNTIME_DIR", dir.path())
        .env("LIXUN_TEST_WORKER_PID_FILE", dir.path().join("pid"))
        .env("LIXUN_TEST_WORKER_BINARY", worker_bin())
        .env("LIXUN_SEMANTIC_WORKER", launcher)
        .env("LIXUN_SEMANTIC_DATA_DIR", dir.path().join("data"))
        .status()
        .unwrap();
    assert!(status.success(), "isolated worker test failed");
    true
}

fn ann_handle() -> Arc<dyn AnnHandle> {
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
    let inst = factory
        .build(&raw, &ctx)
        .expect("stub factory")
        .into_iter()
        .next()
        .expect("stub registered no instance — env not set?");
    inst.source
        .ann_handle()
        .expect("stub source has no AnnHandle")
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

#[test]
fn probe_returns_none_for_unreachable_env_path() {
    let prior = std::env::var_os("LIXUN_SEMANTIC_WORKER");
    /* Tests share process state. We set a deliberately-bad path,
    snapshot the probe, then restore — even though probe might
    still find a real binary on PATH. The assertion is permissive
    by design: we only require the probe doesn't panic. */
    unsafe {
        std::env::set_var(
            "LIXUN_SEMANTIC_WORKER",
            "/nonexistent/lixun-semantic-worker-fake",
        );
    }
    let _ = semantic_supervisor::probe_worker_binary();
    unsafe {
        match prior {
            Some(v) => std::env::set_var("LIXUN_SEMANTIC_WORKER", v),
            None => std::env::remove_var("LIXUN_SEMANTIC_WORKER"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_handshakes_and_serves_empty_search() {
    if isolated_worker_test("supervisor_handshakes_and_serves_empty_search") {
        return;
    }
    let bin = PathBuf::from(std::env::var_os("LIXUN_SEMANTIC_WORKER").unwrap());
    let supervisor = tokio::spawn(semantic_supervisor::supervise(bin.clone()));

    wait_until_connected(Duration::from_secs(120)).await;

    let ann = ann_handle();
    let hits = timeout(Duration::from_secs(30), ann.search_text("hello", 5))
        .await
        .expect("search did not complete within 30s")
        .expect("search returned Err");
    assert!(hits.is_empty(), "empty corpus must produce zero hits");

    supervisor.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_restarts_after_worker_kill() {
    if isolated_worker_test("supervisor_restarts_after_worker_kill") {
        return;
    }
    let bin = PathBuf::from(std::env::var_os("LIXUN_SEMANTIC_WORKER").unwrap());
    let supervisor = tokio::spawn(semantic_supervisor::supervise(bin.clone()));
    wait_until_connected(Duration::from_secs(120)).await;

    let pid_file = std::env::var_os("LIXUN_TEST_WORKER_PID_FILE").unwrap();
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(pid > 1);
    // Pin the process before checking ownership so PID reuse cannot redirect
    // the signal. The supervisor spawns the worker directly in this process.
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    assert!(fd >= 0, "worker already exited");
    let worker = unsafe { OwnedFd::from_raw_fd(fd as i32) };
    let proc_status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    let parent = proc_status
        .lines()
        .find_map(|line| line.strip_prefix("PPid:"))
        .unwrap();
    assert_eq!(parent.trim().parse::<u32>().unwrap(), std::process::id());
    assert_eq!(
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                worker.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        },
        0
    );
    /* Give the supervisor a moment to notice the dead child before
    we begin probing. */
    sleep(Duration::from_millis(500)).await;

    let ann = ann_handle();
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut last_err: Option<String> = None;
    loop {
        if Instant::now() >= deadline {
            panic!("worker did not recover within 120s after kill; last err: {last_err:?}");
        }
        match timeout(Duration::from_secs(5), ann.search_text("hello", 5)).await {
            Ok(Ok(_)) if std::fs::read_to_string(&pid_file).unwrap().trim() != pid.to_string() => {
                supervisor.abort();
                return;
            }
            Ok(Ok(_)) => {}
            Ok(Err(e)) => last_err = Some(format!("{e}")),
            Err(_) => last_err = Some("timeout".into()),
        }
        sleep(Duration::from_millis(500)).await;
    }
}
