//! Bounded child-process execution: spawn argv directly, capture
//! stdout up to a byte cap, kill on timeout or cancellation.

use anyhow::{Context, Result};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Hard cap on captured stdout. Anything beyond it is drained and
/// discarded so the child never blocks on a full pipe.
pub const MAX_STDOUT_BYTES: usize = 256 * 1024;

/// Hard cap on captured stderr (used only for error reporting).
const MAX_STDERR_BYTES: usize = 4 * 1024;

pub struct RunOutcome {
    pub stdout: Vec<u8>,
    /// True when the stdout cap was hit (output truncated).
    pub truncated: bool,
    /// First bytes of stderr, lossily decoded, for error hits.
    pub stderr_head: String,
    pub status: RunStatus,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RunStatus {
    Success,
    /// Non-zero exit; carries the code when the OS reports one.
    Failed(Option<i32>),
    TimedOut,
    Cancelled,
}

/// Read from `r` into a bounded buffer; keep draining (discarding)
/// past the cap so the writer never stalls on pipe backpressure.
fn read_bounded<R: Read>(mut r: R, cap: usize) -> (Vec<u8>, bool) {
    let mut buf = Vec::new();
    let mut scratch = [0u8; 8192];
    let mut truncated = false;
    loop {
        match r.read(&mut scratch) {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() < cap {
                    let take = n.min(cap - buf.len());
                    buf.extend_from_slice(&scratch[..take]);
                    if take < n {
                        truncated = true;
                    }
                } else {
                    truncated = true;
                }
            }
            Err(_) => break,
        }
    }
    (buf, truncated)
}

/// Spawn `argv` directly (no shell), wait up to `timeout`, polling
/// `is_cancelled` between waits. The child is killed and reaped on
/// timeout/cancel so no zombie survives the call.
pub fn run_bounded(
    argv: &[String],
    cwd: Option<&Path>,
    timeout: Duration,
    is_cancelled: &dyn Fn() -> bool,
) -> Result<RunOutcome> {
    let (program, args) = argv
        .split_first()
        .context("command source argv is empty")?;
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn {program:?}"))?;

    // Concurrent bounded readers: without them a chatty child fills
    // the ~64 KiB pipe buffer and deadlocks against our wait loop.
    let stdout_pipe = child.stdout.take().context("child stdout missing")?;
    let stderr_pipe = child.stderr.take().context("child stderr missing")?;
    let stdout_reader =
        std::thread::spawn(move || read_bounded(stdout_pipe, MAX_STDOUT_BYTES));
    let stderr_reader =
        std::thread::spawn(move || read_bounded(stderr_pipe, MAX_STDERR_BYTES));

    let start = Instant::now();
    let status = loop {
        if is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            break RunStatus::Cancelled;
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            break RunStatus::TimedOut;
        }
        match child.try_wait() {
            Ok(Some(st)) if st.success() => break RunStatus::Success,
            Ok(Some(st)) => break RunStatus::Failed(st.code()),
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e).context("waiting on command source child");
            }
        }
    };

    let (stdout, truncated) = stdout_reader.join().unwrap_or_default();
    let (stderr, _) = stderr_reader.join().unwrap_or_default();
    Ok(RunOutcome {
        stdout,
        truncated,
        stderr_head: String::from_utf8_lossy(&stderr).trim().to_string(),
        status,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_cancel() -> impl Fn() -> bool {
        || false
    }

    #[test]
    fn captures_stdout_of_quick_command() {
        let out = run_bounded(
            &["echo".into(), "hello".into()],
            None,
            Duration::from_millis(2000),
            &no_cancel(),
        )
        .expect("spawn echo");
        assert_eq!(out.status, RunStatus::Success);
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hello");
        assert!(!out.truncated);
    }

    #[test]
    fn kills_on_timeout() {
        let start = Instant::now();
        let out = run_bounded(
            &["sleep".into(), "5".into()],
            None,
            Duration::from_millis(120),
            &no_cancel(),
        )
        .expect("spawn sleep");
        assert_eq!(out.status, RunStatus::TimedOut);
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "timeout must not wait out the child's own sleep"
        );
    }

    #[test]
    fn spawn_failure_is_error() {
        let err = run_bounded(
            &["/nonexistent/lixun-test-binary".into()],
            None,
            Duration::from_millis(200),
            &no_cancel(),
        );
        assert!(err.is_err());
    }

    #[test]
    fn nonzero_exit_reported() {
        let out = run_bounded(
            &["sh".into(), "-c".into(), "echo oops >&2; exit 3".into()],
            None,
            Duration::from_millis(2000),
            &no_cancel(),
        )
        .expect("spawn sh");
        assert_eq!(out.status, RunStatus::Failed(Some(3)));
        assert_eq!(out.stderr_head, "oops");
    }

    #[test]
    fn stdout_is_capped_without_deadlock() {
        // 1 MiB of output against the 256 KiB cap: must terminate
        // promptly and mark truncation.
        let out = run_bounded(
            &[
                "sh".into(),
                "-c".into(),
                "head -c 1048576 /dev/zero | tr '\\0' 'x'".into(),
            ],
            None,
            Duration::from_millis(5000),
            &no_cancel(),
        )
        .expect("spawn sh");
        assert_eq!(out.status, RunStatus::Success);
        assert_eq!(out.stdout.len(), MAX_STDOUT_BYTES);
        assert!(out.truncated);
    }
}
