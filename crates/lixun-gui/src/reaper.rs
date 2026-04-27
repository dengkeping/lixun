//! Background child-process reaper.
//!
//! The GUI fires short-lived helper processes (xdg-open, terminal
//! emulators, xdg-terminal-exec, file managers) when dispatching
//! user actions. Each `Command::spawn()` returns a `Child`; if we
//! drop that `Child` without calling `wait()`, the kernel keeps its
//! exit status around as a zombie (`<defunct>` in ps) until the
//! parent reaps it. Observed in the wild: `kitty <defunct>` entries
//! accumulating under the lixun-gui PID.
//!
//! Fix: every spawned `Child` is handed to a module-global reaper
//! singleton. A dedicated OS thread waits on a `Condvar` with a
//! short timeout and periodically calls `try_wait()` on each
//! tracked child. `try_wait()` invokes `waitpid(WNOHANG)` under the
//! hood; when it returns `Ok(Some(_))` the child has exited and its
//! zombie has been consumed — we then drop the `Child` to release
//! the FD/pidfd.
//!
//! Trade-offs vs. alternatives:
//!
//! * `signal(SIGCHLD, SIG_IGN)` — works but is process-global and
//!   affects every crate in the address space (GIO, glib, zbus).
//!   Rejected: too invasive.
//! * Double-fork — requires `unsafe fork` and manual FD juggling.
//!   Rejected: the fix should be boring.
//! * `nix::sys::wait::waitpid` in a SIGCHLD handler — async-signal
//!   safety constraints make it fragile.
//!
//! This module uses only `std`. No `unsafe`. No new dependencies.

use std::process::{Child, Command};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

/// Max time the reaper thread sleeps between sweeps when it has
/// live children. Small enough that zombies don't accumulate for
/// long; large enough that the thread is effectively idle.
const SWEEP_INTERVAL: Duration = Duration::from_secs(2);

struct ReaperState {
    children: Mutex<Vec<Child>>,
    cv: Condvar,
}

static REAPER: OnceLock<Arc<ReaperState>> = OnceLock::new();

fn reaper() -> &'static Arc<ReaperState> {
    REAPER.get_or_init(|| {
        let state = Arc::new(ReaperState {
            children: Mutex::new(Vec::new()),
            cv: Condvar::new(),
        });
        let worker = Arc::clone(&state);
        thread::Builder::new()
            .name("lixun-gui-reaper".into())
            .spawn(move || reaper_loop(worker))
            .expect("spawn reaper thread");
        state
    })
}

fn reaper_loop(state: Arc<ReaperState>) {
    loop {
        let mut guard = state.children.lock().expect("reaper mutex poisoned");

        // Sleep until a new child is pushed or the sweep interval
        // elapses. If the Vec is empty we park indefinitely on the
        // condvar — no point waking up to sweep nothing.
        guard = if guard.is_empty() {
            state.cv.wait(guard).expect("reaper condvar poisoned")
        } else {
            state
                .cv
                .wait_timeout(guard, SWEEP_INTERVAL)
                .expect("reaper condvar poisoned")
                .0
        };

        // retain_mut: keep children that are still running; drop
        // those that exited. `try_wait` consumes the zombie; the
        // subsequent drop only closes the pidfd/handle.
        guard.retain_mut(|child| match child.try_wait() {
            Ok(None) => true,           // still running
            Ok(Some(_status)) => false, // exited, reaped
            Err(e) => {
                // ECHILD or similar: child vanished. Drop it —
                // there's nothing to wait on anyway.
                tracing::debug!(pid = child.id(), error = %e, "reaper: try_wait failed, dropping");
                false
            }
        });
    }
}

/// Spawn `cmd` and hand the resulting `Child` to the reaper
/// singleton. Prefer this over `cmd.spawn()` for any process the
/// caller does not intend to wait on themselves.
///
/// The reaper thread is started lazily on first use.
pub(crate) fn spawn_reaped(cmd: &mut Command) -> std::io::Result<()> {
    let child = cmd.spawn()?;
    let state = reaper();
    {
        let mut guard = state.children.lock().expect("reaper mutex poisoned");
        guard.push(child);
    }
    state.cv.notify_one();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Poll the reaper's Vec length until it hits `target` or the
    /// deadline elapses. Returns the final length.
    fn wait_for_len(target: usize, timeout: Duration) -> usize {
        let state = reaper();
        let deadline = Instant::now() + timeout;
        loop {
            let len = state.children.lock().unwrap().len();
            if len == target || Instant::now() >= deadline {
                return len;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    // Tests in this module share the single REAPER singleton. A
    // mutex serialises them so snapshots of the live children Vec
    // are not racing against each other's spawns.
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn spawn_reaped_reaps_short_lived_child() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Use /bin/true so the child exits immediately.
        let mut cmd = Command::new("/bin/true");
        spawn_reaped(&mut cmd).expect("spawn /bin/true");

        // The reaper sleeps up to SWEEP_INTERVAL; give it a
        // generous margin. On a loaded CI runner this must still
        // complete well within the timeout.
        let final_len = wait_for_len(0, Duration::from_secs(5));
        assert_eq!(final_len, 0, "reaper did not collect /bin/true child");
    }

    #[test]
    fn spawn_reaped_handles_multiple_children() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Drain any leftovers from prior tests so the count is
        // deterministic.
        wait_for_len(0, Duration::from_secs(5));

        for _ in 0..5 {
            let mut cmd = Command::new("/bin/true");
            spawn_reaped(&mut cmd).expect("spawn /bin/true");
        }

        let final_len = wait_for_len(0, Duration::from_secs(5));
        assert_eq!(
            final_len, 0,
            "reaper did not collect all five /bin/true children (leaked {})",
            final_len
        );
    }

    #[test]
    fn reaper_singleton_reuses_thread() {
        // Two calls to reaper() must return the same Arc.
        let a = reaper() as *const _;
        let b = reaper() as *const _;
        assert_eq!(a, b, "reaper() returned distinct instances");
    }
}
