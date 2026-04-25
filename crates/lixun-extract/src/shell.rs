use anyhow::{anyhow, Context, Result};
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;
use tempfile::NamedTempFile;
use wait_timeout::ChildExt;

pub trait CommandRunner {
    fn run(&self, cmd: &str, args: &[&str], input: Option<&[u8]>) -> Result<String>;
}

pub struct SystemRunner {
    pub timeout: Duration,
}

impl SystemRunner {
    pub fn new(timeout_secs: u64) -> Self {
        Self {
            timeout: Duration::from_secs(timeout_secs),
        }
    }
}

impl Default for SystemRunner {
    fn default() -> Self {
        Self::new(15)
    }
}

impl CommandRunner for SystemRunner {
    fn run(&self, cmd: &str, args: &[&str], _input: Option<&[u8]>) -> Result<String> {
        let mut child = Command::new(cmd)
            .args(args)
            .env("OMP_THREAD_LIMIT", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()?;

        // Invariant: both pipes were just configured as Stdio::piped() above,
        // so the Options MUST be Some. Drain concurrently in worker threads to
        // prevent the child from blocking on a full pipe buffer (~64KB on Linux)
        // while we wait for it to exit.
        let mut stdout_pipe = child.stdout.take().expect("piped stdout");
        let mut stderr_pipe = child.stderr.take().expect("piped stderr");

        let stdout_handle = thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stdout_pipe.read_to_end(&mut buf);
            buf
        });
        let stderr_handle = thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stderr_pipe.read_to_end(&mut buf);
            buf
        });

        let status = if self.timeout.is_zero() {
            child.wait()?
        } else {
            match child.wait_timeout(self.timeout)? {
                Some(s) => s,
                None => {
                    let _ = unsafe { libc::killpg(child.id() as i32, libc::SIGKILL) };
                    let _ = child.kill();
                    let _ = child.wait();
                    // Killing the child closes the pipes, so the worker threads
                    // will return from read_to_end. Join them to avoid leaks.
                    let _ = stdout_handle.join();
                    let _ = stderr_handle.join();
                    return Err(anyhow!(
                        "extractor '{}' timed out after {}s",
                        cmd,
                        self.timeout.as_secs()
                    ));
                }
            }
        };

        let stdout_bytes = stdout_handle
            .join()
            .map_err(|_| anyhow!("stdout reader thread panicked"))?;
        let stderr_bytes = stderr_handle
            .join()
            .map_err(|_| anyhow!("stderr reader thread panicked"))?;

        if status.success() {
            Ok(String::from_utf8_lossy(&stdout_bytes).to_string())
        } else {
            Err(anyhow!(
                "{} failed: {}",
                cmd,
                String::from_utf8_lossy(&stderr_bytes)
            ))
        }
    }
}

#[doc(hidden)]
pub fn command_exists(cmd: &str) -> bool {
    which::which(cmd).is_ok()
}

pub fn extract_doc(bytes: &[u8], runner: &dyn CommandRunner) -> Result<String> {
    let (path, _handle) = write_temp_named("lixun-shell", "doc", bytes)?;
    let path_str = path.to_str().context("non-UTF8 tempfile path")?;
    runner.run("antiword", &["-m", "UTF-8", path_str], None)
}

pub fn extract_xls(bytes: &[u8], runner: &dyn CommandRunner) -> Result<String> {
    let (path, _handle) = write_temp_named("lixun-shell", "xls", bytes)?;
    let path_str = path.to_str().context("non-UTF8 tempfile path")?;
    runner.run("catdoc", &[path_str], None)
}

pub fn extract_ppt(bytes: &[u8], runner: &dyn CommandRunner) -> Result<String> {
    let (path, _handle) = write_temp_named("lixun-shell", "ppt", bytes)?;
    let path_str = path.to_str().context("non-UTF8 tempfile path")?;
    runner.run("libreoffice", &["--headless", "--cat", path_str], None)
}

pub fn extract_pdf(bytes: &[u8], runner: &dyn CommandRunner) -> Result<String> {
    let (path, _handle) = write_temp_named("lixun-shell", "pdf", bytes)?;
    let path_str = path.to_str().context("non-UTF8 tempfile path")?;
    runner.run(
        "pdftotext",
        &["-layout", "-enc", "UTF-8", path_str, "-"],
        None,
    )
}

/// Write bytes to a securely-created temp file with the given extension.
///
/// Returns the path AND the `NamedTempFile` handle. The caller MUST keep the handle
/// alive until the path is no longer needed; the file is deleted when the handle drops.
///
/// Security: uses `tempfile::Builder` with an 8-byte random suffix to produce an
/// unpredictable filename, preventing symlink pre-planting attacks that were possible
/// with the previous PID-only scheme in shared `/tmp`.
pub fn write_temp_named(prefix: &str, ext: &str, bytes: &[u8]) -> Result<(PathBuf, NamedTempFile)> {
    let handle = tempfile::Builder::new()
        .prefix(&format!("{prefix}-"))
        .suffix(&format!(".{ext}"))
        .rand_bytes(8)
        .tempfile()?;
    std::fs::write(handle.path(), bytes)?;
    Ok((handle.path().to_path_buf(), handle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn timeout_kills_long_running() {
        let runner = SystemRunner::new(1);
        let start = Instant::now();
        let res = runner.run("sh", &["-c", "sleep 30"], None);
        assert!(res.is_err());
        assert!(start.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn fast_command_succeeds() {
        let runner = SystemRunner::new(5);
        let out = runner.run("sh", &["-c", "echo hello"], None).unwrap();
        assert!(out.contains("hello"));
    }

    #[test]
    fn zero_timeout_means_no_timeout() {
        let runner = SystemRunner::new(0);
        let out = runner.run("sh", &["-c", "echo ok"], None).unwrap();
        assert!(out.contains("ok"));
    }

    #[test]
    fn test_write_temp_returns_unique_paths() {
        let (p1, _h1) = write_temp_named("test-prefix", "txt", b"first").unwrap();
        let (p2, _h2) = write_temp_named("test-prefix", "txt", b"second").unwrap();
        assert_ne!(p1, p2);
        assert_eq!(std::fs::read(&p1).unwrap(), b"first");
        assert_eq!(std::fs::read(&p2).unwrap(), b"second");
        let name1 = p1.file_name().unwrap().to_string_lossy();
        assert!(name1.starts_with("test-prefix-"));
        assert!(name1.ends_with(".txt"));
    }

    #[test]
    fn system_runner_sets_omp_thread_limit_for_children() {
        let runner = SystemRunner::new(5);
        let out = runner
            .run(
                "sh",
                &["-c", "printf 'OMP_THREAD_LIMIT=%s\\n' \"${OMP_THREAD_LIMIT:-unset}\""],
                None,
            )
            .unwrap();
        assert!(
            out.contains("OMP_THREAD_LIMIT=1"),
            "SystemRunner must export OMP_THREAD_LIMIT=1 to children, got: {out:?}"
        );
    }

    #[test]
    fn test_large_output_no_deadlock() {
        // Without concurrent pipe drain this hangs: the child fills the stdout
        // pipe buffer (~64KB on Linux) and blocks forever, our wait_timeout
        // then kills a healthy process. ~512KB exceeds every reasonable pipe
        // buffer.
        let runner = SystemRunner::new(30);
        let big = "a".repeat(100);
        let cmd = format!("for i in $(seq 1 5000); do printf '%s\\n' '{}'; done", big);
        let start = Instant::now();
        let out = runner.run("sh", &["-c", &cmd], None).unwrap();
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "took too long: {:?}",
            start.elapsed()
        );
        assert!(
            out.len() > 100_000,
            "expected >100KB output, got {}",
            out.len()
        );
    }
}
