//! Bounded child-process execution: spawn argv directly, capture
//! stdout up to a byte cap, kill on timeout or cancellation.

use anyhow::{Context, Result};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
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

fn nonblocking(pipe: &impl AsRawFd) -> io::Result<()> {
    let fd = pipe.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

struct Output<R> {
    pipe: R,
    bytes: Vec<u8>,
    cap: usize,
    truncated: bool,
    eof: bool,
}

impl<R: Read + AsRawFd> Output<R> {
    fn new(pipe: R, cap: usize) -> io::Result<Self> {
        nonblocking(&pipe)?;
        Ok(Self {
            pipe,
            bytes: Vec::new(),
            cap,
            truncated: false,
            eof: false,
        })
    }

    // Bound work per iteration so a continuously writable pipe cannot starve cancellation.
    fn drain(&mut self) -> io::Result<()> {
        let mut buffer = [0; 8192];
        for _ in 0..16 {
            if self.eof {
                break;
            }
            match self.pipe.read(&mut buffer) {
                Ok(0) => self.eof = true,
                Ok(n) => {
                    let keep = n.min(self.cap - self.bytes.len());
                    self.bytes.extend_from_slice(&buffer[..keep]);
                    self.truncated |= keep < n;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
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
    let (program, args) = argv.split_first().context("command source argv is empty")?;
    let mut cmd = Command::new(program);
    cmd.process_group(0)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn {program:?}"))?;

    let group = child.id() as i32;
    let collected = (|| -> Result<RunOutcome> {
        let mut out = Output::new(
            child.stdout.take().context("missing stdout")?,
            MAX_STDOUT_BYTES,
        )?;
        let mut err = Output::new(
            child.stderr.take().context("missing stderr")?,
            MAX_STDERR_BYTES,
        )?;
        let start = Instant::now();
        let status = loop {
            if is_cancelled() {
                break RunStatus::Cancelled;
            }
            if start.elapsed() >= timeout {
                break RunStatus::TimedOut;
            }
            out.drain()?;
            err.drain()?;
            if out.eof
                && err.eof
                && let Some(status) = child.try_wait()?
            {
                break if status.success() {
                    RunStatus::Success
                } else {
                    RunStatus::Failed(status.code())
                };
            }
            std::thread::sleep(Duration::from_millis(2));
        };
        Ok(RunOutcome {
            stdout: out.bytes,
            truncated: out.truncated,
            stderr_head: String::from_utf8_lossy(&err.bytes).trim().to_string(),
            status,
        })
    })();
    if !matches!(&collected, Ok(out) if matches!(out.status, RunStatus::Success | RunStatus::Failed(_)))
    {
        // The child is the group leader and has not been reaped on this path.
        unsafe {
            libc::kill(-group, libc::SIGKILL);
        }
        let _ = child.wait();
    }
    collected
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
    fn inherited_pipes_obey_timeout_and_cancellation_after_parent_exit() {
        for cancel in [false, true] {
            let start = Instant::now();
            let out = run_bounded(
                &["sh".into(), "-c".into(), "sleep 8 & exit 0".into()],
                None,
                Duration::from_millis(400),
                &|| cancel && start.elapsed() >= Duration::from_millis(100),
            )
            .unwrap();
            assert_eq!(
                out.status,
                if cancel {
                    RunStatus::Cancelled
                } else {
                    RunStatus::TimedOut
                }
            );
            assert!(start.elapsed() < Duration::from_secs(3));
        }
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
