//! Process-level scheduling knobs applied at startup from the
//! resolved [`lixun_core::ImpactProfile`].
//!
//! Both calls are best-effort: missing `CAP_SYS_NICE`, kernels that
//! refuse `SCHED_IDLE`, and non-Linux hosts all degrade gracefully
//! with a single warn log and no error propagation.

/// Apply the daemon process priority knobs encoded in `profile`.
///
/// Always issues `setpriority(PRIO_PROCESS, 0, daemon_nice)` —
/// `daemon_nice == 0` is a no-op but keeps the call site uniform.
/// When `daemon_sched_idle` is true, additionally requests
/// `SCHED_IDLE`; on `EPERM` (typical for unprivileged users) we
/// log a warn and continue with plain `nice`.
pub fn apply_profile(profile: &lixun_core::ImpactProfile) {
    apply_nice(profile.daemon_nice);
    if profile.daemon_sched_idle {
        apply_sched_idle();
    }
}

fn apply_nice(nice: i32) {
    let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, nice) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        tracing::warn!("setpriority({}) failed: {}", nice, err);
    }
}

/// Re-apply just the daemon nice value without touching the
/// scheduler class. Used by the runtime hot-reload path
/// (`Request::ImpactSet`) — switching scheduler classes mid-run
/// would race against any in-flight `sched_setscheduler` call and
/// has no upside for the level transitions Wave E supports
/// (only the `Low` level requests `SCHED_IDLE`, which is set at
/// startup; downgrading back to `SCHED_OTHER` requires a restart
/// and is reported via `requires_restart`).
pub fn apply_nice_only(nice: i32) {
    apply_nice(nice);
}

fn apply_sched_idle() {
    let param = libc::sched_param { sched_priority: 0 };
    let rc = unsafe { libc::sched_setscheduler(0, libc::SCHED_IDLE, &param) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        tracing::warn!("sched_setscheduler EPERM; falling back to nice ({})", err);
    }
}
