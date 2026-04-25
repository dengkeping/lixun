//! CPU pressure idle gate (DB-15, OCR-T6.5).
//!
//! [`CpuPsiGate`] reads `/proc/pressure/cpu` and reports idle when
//! the `some avg10` value is at or below a configured threshold.
//! [`CompositeIdleGate`] composes several [`IdleGate`]s so callers
//! can `all`-combine (reindex-in-progress gate, PSI gate, future
//! battery gate, ...).
//!
//! Both gates are OFF by default at the daemon level — the daemon
//! only constructs a `CpuPsiGate` when `[ocr].adaptive_throttle =
//! true`. On any I/O failure (missing kernel file, permission
//! denied, parse error) the gate self-disables and reports idle
//! from then on: throttling must not block the worker on a
//! kernel-support hiccup.
//!
//! Non-Linux builds compile the struct for API compatibility but
//! the gate is permanently disabled (always idle) — PSI is a
//! Linux-only kernel interface.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::ocr_tick::IdleGate;

/// Default PSI source path. Exposed via [`CpuPsiGate::with_path`]
/// for tests; production always uses this path.
const DEFAULT_PSI_CPU_PATH: &str = "/proc/pressure/cpu";

/// Idle gate driven by `/proc/pressure/cpu` `some avg10` reading.
///
/// Fails open: if the kernel file is missing, unreadable, or
/// unparseable, the gate self-disables (via its internal
/// [`AtomicBool`]) and returns `true` (idle) on every subsequent
/// call. This is intentional — the worker should keep making
/// progress on systems without PSI support.
pub struct CpuPsiGate {
    threshold: f32,
    enabled: AtomicBool,
    path: PathBuf,
}

impl CpuPsiGate {
    /// Build a gate pointing at the production kernel path
    /// (`/proc/pressure/cpu`). On non-Linux targets the gate is
    /// born disabled.
    pub fn new(threshold: f32) -> Self {
        #[cfg(target_os = "linux")]
        let enabled = AtomicBool::new(true);
        #[cfg(not(target_os = "linux"))]
        let enabled = AtomicBool::new(false);
        Self {
            threshold,
            enabled,
            path: PathBuf::from(DEFAULT_PSI_CPU_PATH),
        }
    }

    /// Test-only constructor: point the gate at any file. Behaves
    /// identically to [`CpuPsiGate::new`] otherwise.
    pub fn with_path(threshold: f32, path: PathBuf) -> Self {
        #[cfg(target_os = "linux")]
        let enabled = AtomicBool::new(true);
        #[cfg(not(target_os = "linux"))]
        let enabled = AtomicBool::new(false);
        Self {
            threshold,
            enabled,
            path,
        }
    }

    /// Whether the gate is currently live (has not self-disabled).
    /// Primarily useful for tests that want to assert the
    /// fail-open path flipped the flag.
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    fn read_some_avg10(path: &Path) -> std::io::Result<f32> {
        let s = std::fs::read_to_string(path)?;
        for line in s.lines() {
            if let Some(rest) = line.strip_prefix("some ") {
                for kv in rest.split_whitespace() {
                    if let Some(v) = kv.strip_prefix("avg10=") {
                        return v.parse::<f32>().map_err(std::io::Error::other);
                    }
                }
            }
        }
        Err(std::io::Error::other("no 'some avg10' line"))
    }
}

impl IdleGate for CpuPsiGate {
    fn is_idle(&self) -> bool {
        if !self.enabled.load(Ordering::Relaxed) {
            return true;
        }
        match Self::read_some_avg10(&self.path) {
            Ok(v) => v <= self.threshold,
            Err(e) => {
                self.enabled.store(false, Ordering::Relaxed);
                tracing::warn!("PSI unavailable ({e}); adaptive throttle disabled");
                true
            }
        }
    }
}

/// Composite gate: idle iff every sub-gate reports idle.
///
/// Short-circuits on the first busy gate so the most selective
/// probe (typically the reindex-in-progress flag) should be listed
/// first.
pub struct CompositeIdleGate {
    pub gates: Vec<std::sync::Arc<dyn IdleGate>>,
}

impl IdleGate for CompositeIdleGate {
    fn is_idle(&self) -> bool {
        self.gates.iter().all(|g| g.is_idle())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Arc;

    fn write_psi_file(body: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(body.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    struct FixedIdle(bool);
    impl IdleGate for FixedIdle {
        fn is_idle(&self) -> bool {
            self.0
        }
    }

    #[test]
    fn psi_gate_under_threshold_reports_idle() {
        let f = write_psi_file(
            "some avg10=0.12 avg60=0.00 avg300=0.00 total=1234\n\
             full avg10=0.00 avg60=0.00 avg300=0.00 total=0\n",
        );
        let gate = CpuPsiGate::with_path(10.0, f.path().to_path_buf());
        // Force-enable on non-Linux so the parsing path is exercised
        // regardless of build target.
        gate.enabled.store(true, Ordering::Relaxed);
        assert!(gate.is_idle());
        assert!(gate.is_enabled());
    }

    #[test]
    fn psi_gate_over_threshold_reports_not_idle() {
        let f = write_psi_file(
            "some avg10=25.00 avg60=0.00 avg300=0.00 total=1234\n\
             full avg10=0.00 avg60=0.00 avg300=0.00 total=0\n",
        );
        let gate = CpuPsiGate::with_path(10.0, f.path().to_path_buf());
        gate.enabled.store(true, Ordering::Relaxed);
        assert!(!gate.is_idle());
        assert!(gate.is_enabled());
    }

    #[test]
    fn psi_gate_missing_file_auto_disables() {
        let gate = CpuPsiGate::with_path(
            10.0,
            PathBuf::from("/nonexistent/path/proc-pressure-cpu"),
        );
        gate.enabled.store(true, Ordering::Relaxed);
        // First call fails open and flips the flag off.
        assert!(gate.is_idle());
        assert!(!gate.is_enabled());
        // Subsequent calls stay idle and never touch disk.
        assert!(gate.is_idle());
        assert!(!gate.is_enabled());
    }

    #[test]
    fn composite_all_idle_is_idle() {
        let a: Arc<dyn IdleGate> = Arc::new(FixedIdle(true));
        let b: Arc<dyn IdleGate> = Arc::new(FixedIdle(true));
        let composite = CompositeIdleGate { gates: vec![a, b] };
        assert!(composite.is_idle());
    }

    #[test]
    fn composite_any_busy_is_not_idle() {
        let a: Arc<dyn IdleGate> = Arc::new(FixedIdle(true));
        let b: Arc<dyn IdleGate> = Arc::new(FixedIdle(false));
        let composite = CompositeIdleGate { gates: vec![a, b] };
        assert!(!composite.is_idle());
    }
}
