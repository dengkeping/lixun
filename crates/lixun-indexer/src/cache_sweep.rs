//! Periodic extract-cache maintenance (OCR-T9).
//!
//! The extract cache at `${XDG_CACHE_HOME}/lixun/extract/v1` grows
//! monotonically as new documents are indexed. Three obligations
//! keep it bounded:
//!
//! 1. **LRU eviction** — keep total non-tmp bytes under
//!    `[extract].cache_max_mb`. When the cap is exceeded, delete
//!    oldest-mtime-first until the budget is satisfied (R14).
//! 2. **Stale tmp cleanup** — atomic writes land via `.tmp-<uuid>` +
//!    `rename`. A crash or disk-full mid-write can leave the tmp file
//!    behind. Any `.tmp-*` older than `tmp_max_age` is removed
//!    unconditionally (R18).
//! 3. **Zombie reap** — rows in the OCR queue that have exhausted the
//!    worker's retry budget linger for a 30-day grace window so
//!    operators can inspect `last_error`, then are deleted (R12).
//!
//! All three happen on the same tick to keep the daemon's
//! background-task count low. The sweep runs under
//! `spawn_blocking` because it's pure filesystem + SQLite I/O.
//!
//! `max_bytes == 0` disables the tick entirely (valid configuration,
//! no warning). Zombie reap still does not run in that case because
//! the whole sweep loop exits early.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use walkdir::WalkDir;

/// Tunables for a single [`sweep_once`] call and the [`spawn`]
/// driver that invokes it periodically.
#[derive(Debug, Clone)]
pub struct CacheSweepCfg {
    pub cache_root: PathBuf,
    pub max_bytes: u64,
    pub interval: Duration,
    pub tmp_max_age: Duration,
    pub zombie_max_attempts: u32,
    pub zombie_max_age: Duration,
}

/// Abstraction over the OCR queue's zombie-reap query. The indexer
/// crate has no business depending on `lixun-extract::ocr_queue` for
/// this one call, and more importantly the test suite here must be
/// able to stub the reaper without opening a real SQLite database.
/// The daemon wires the production implementation.
pub trait ZombieReaper: Send + Sync + 'static {
    fn reap(&self, max_attempts: u32, older_than_secs: i64) -> Result<u64>;
}

/// Aggregate result of a single sweep. Surfaced for tests and
/// potential status/metrics endpoints.
#[derive(Debug, Default, Clone)]
pub struct SweepOutcome {
    pub evicted_files: u64,
    pub evicted_bytes: u64,
    pub tmp_cleaned: u64,
    pub zombies_reaped: u64,
}

/// File record gathered during the walk. `is_tmp` short-circuits the
/// LRU math: tmp files never count toward the cap because they might
/// disappear under `rename` at any moment.
struct CacheFile {
    path: PathBuf,
    mtime_secs: i64,
    size: u64,
    is_tmp: bool,
}

/// Perform one pass: walk the cache, clean stale tmps, LRU-evict
/// until the total fits under `max_bytes`, then (if supplied) reap
/// zombies from the OCR queue.
pub fn sweep_once(cfg: &CacheSweepCfg, reaper: Option<&dyn ZombieReaper>) -> Result<SweepOutcome> {
    let mut outcome = SweepOutcome::default();

    if !cfg.cache_root.exists() {
        return Ok(outcome);
    }

    let now = SystemTime::now();
    let now_secs = now
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let mut files: Vec<CacheFile> = Vec::new();
    for entry in WalkDir::new(&cfg.cache_root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let path = entry.path().to_path_buf();
        let md = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let size = md.len();
        let mtime_secs = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let is_tmp = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with(".tmp-"))
            .unwrap_or(false);
        files.push(CacheFile {
            path,
            mtime_secs,
            size,
            is_tmp,
        });
    }

    let tmp_cutoff_secs = cfg.tmp_max_age.as_secs() as i64;
    for f in files.iter().filter(|f| f.is_tmp) {
        let age_secs = now_secs.saturating_sub(f.mtime_secs);
        if age_secs >= tmp_cutoff_secs {
            match std::fs::remove_file(&f.path) {
                Ok(()) => outcome.tmp_cleaned += 1,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!(
                    "cache sweep: failed to remove tmp {}: {e}",
                    f.path.display()
                ),
            }
        }
    }

    if cfg.max_bytes > 0 {
        let mut regular: Vec<&CacheFile> = files.iter().filter(|f| !f.is_tmp).collect();
        let total_bytes: u64 = regular.iter().map(|f| f.size).sum();

        if total_bytes > cfg.max_bytes {
            regular.sort_by_key(|f| f.mtime_secs);
            let mut remaining = total_bytes;
            for f in regular {
                if remaining <= cfg.max_bytes {
                    break;
                }
                match std::fs::remove_file(&f.path) {
                    Ok(()) => {
                        outcome.evicted_files += 1;
                        outcome.evicted_bytes += f.size;
                        remaining = remaining.saturating_sub(f.size);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        remaining = remaining.saturating_sub(f.size);
                    }
                    Err(e) => {
                        tracing::warn!("cache sweep: failed to evict {}: {e}", f.path.display())
                    }
                }
            }

            if outcome.evicted_files > 0 {
                let freed_mb = outcome.evicted_bytes / (1024 * 1024);
                let total_mb = total_bytes / (1024 * 1024);
                let cap_mb = cfg.max_bytes / (1024 * 1024);
                tracing::info!(
                    "cache sweep: evicted {} files, freed {} MB (was {} MB, cap {} MB)",
                    outcome.evicted_files,
                    freed_mb,
                    total_mb,
                    cap_mb
                );
            }
        }
    }

    if let Some(r) = reaper {
        let cutoff = now_secs.saturating_sub(cfg.zombie_max_age.as_secs() as i64);
        match r.reap(cfg.zombie_max_attempts, cutoff) {
            Ok(n) => {
                outcome.zombies_reaped = n;
                if n > 0 {
                    tracing::info!("ocr queue: reaped {} zombie row(s)", n);
                }
            }
            Err(e) => tracing::warn!("ocr queue: reap_zombies failed: {e:#}"),
        }
    }

    Ok(outcome)
}

/// Spawn the periodic sweep. If `cfg.max_bytes == 0` the task logs
/// once and exits without entering the tick loop — useful for users
/// who want unbounded cache growth and still want the daemon wiring
/// to remain uniform.
pub fn spawn(cfg: CacheSweepCfg, reaper: Option<Arc<dyn ZombieReaper>>) -> JoinHandle<()> {
    tokio::spawn(async move {
        if cfg.max_bytes == 0 {
            tracing::info!("cache sweep disabled (cache_max_mb=0)");
            return;
        }
        let mut ticker = tokio::time::interval(cfg.interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        tracing::info!(
            "cache sweep: started, interval={:?}, cap={} MB",
            cfg.interval,
            cfg.max_bytes / (1024 * 1024)
        );
        loop {
            ticker.tick().await;
            let cfg_clone = cfg.clone();
            let reaper_clone = reaper.clone();
            let result = tokio::task::spawn_blocking(move || {
                sweep_once(
                    &cfg_clone,
                    reaper_clone.as_deref().map(|r| r as &dyn ZombieReaper),
                )
            })
            .await;
            match result {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => tracing::warn!("cache sweep: failed: {e:#}"),
                Err(e) => tracing::warn!("cache sweep: task panicked: {e}"),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering};
    use std::time::UNIX_EPOCH;
    use tempfile::tempdir;

    fn write_with_mtime(path: &std::path::Path, content: &[u8], mtime_epoch_secs: i64) {
        fs::write(path, content).unwrap();
        let t = UNIX_EPOCH + Duration::from_secs(mtime_epoch_secs.max(0) as u64);
        let ft = std::fs::FileTimes::new().set_modified(t);
        let f = fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_times(ft).unwrap();
    }

    fn mk_cfg(root: PathBuf, max_bytes: u64) -> CacheSweepCfg {
        CacheSweepCfg {
            cache_root: root,
            max_bytes,
            interval: Duration::from_secs(600),
            tmp_max_age: Duration::from_secs(3600),
            zombie_max_attempts: 3,
            zombie_max_age: Duration::from_secs(30 * 86_400),
        }
    }

    #[test]
    fn sweep_under_cap_is_noop() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        for (name, age_offset) in [("a.txt.zst", 300), ("b.txt.zst", 200), ("c.txt.zst", 100)] {
            write_with_mtime(&root.join(name), &[0u8; 33], now - age_offset);
        }
        let cfg = mk_cfg(root.clone(), 1000);

        let out = sweep_once(&cfg, None).unwrap();

        assert_eq!(out.evicted_files, 0);
        assert_eq!(out.evicted_bytes, 0);
        assert_eq!(fs::read_dir(&root).unwrap().count(), 3);
    }

    #[test]
    fn sweep_over_cap_evicts_oldest_first() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        write_with_mtime(&root.join("a.txt.zst"), &vec![0u8; 500], now - 3000);
        write_with_mtime(&root.join("b.txt.zst"), &vec![0u8; 500], now - 2000);
        write_with_mtime(&root.join("c.txt.zst"), &vec![0u8; 500], now - 1000);
        let cfg = mk_cfg(root.clone(), 500);

        let out = sweep_once(&cfg, None).unwrap();

        assert!(
            out.evicted_files >= 2,
            "expected at least 2 evictions, got {}",
            out.evicted_files
        );
        assert!(!root.join("a.txt.zst").exists(), "oldest must go first");
        let remaining_bytes: u64 = fs::read_dir(&root)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
            .sum();
        assert!(
            remaining_bytes <= cfg.max_bytes,
            "remaining {} exceeds cap {}",
            remaining_bytes,
            cfg.max_bytes
        );
    }

    #[test]
    fn sweep_honors_disabled_zero_cap() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        write_with_mtime(&root.join("big.txt.zst"), &vec![0u8; 10_000], now - 100);
        let cfg = mk_cfg(root.clone(), 0);

        let out = sweep_once(&cfg, None).unwrap();

        assert_eq!(out.evicted_files, 0);
        assert!(root.join("big.txt.zst").exists());
    }

    #[test]
    fn sweep_cleans_tmp_files_older_than_limit() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        write_with_mtime(&root.join(".tmp-abc"), b"partial", now - 10);
        write_with_mtime(&root.join("keep.txt.zst"), &[0u8; 32], now);
        let mut cfg = mk_cfg(root.clone(), 1000);
        cfg.tmp_max_age = Duration::from_millis(0);

        let out = sweep_once(&cfg, None).unwrap();

        assert_eq!(out.tmp_cleaned, 1);
        assert!(!root.join(".tmp-abc").exists());
        assert!(root.join("keep.txt.zst").exists());
    }

    struct RecordingReaper {
        calls: AtomicU64,
        last_attempts: AtomicU32,
        last_cutoff: AtomicI64,
        returns: u64,
    }

    impl ZombieReaper for RecordingReaper {
        fn reap(&self, max_attempts: u32, older_than_secs: i64) -> Result<u64> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.last_attempts.store(max_attempts, Ordering::SeqCst);
            self.last_cutoff.store(older_than_secs, Ordering::SeqCst);
            Ok(self.returns)
        }
    }

    #[test]
    fn sweep_invokes_reaper_when_present() {
        let dir = tempdir().unwrap();
        let cfg = mk_cfg(dir.path().to_path_buf(), 1000);
        let reaper = RecordingReaper {
            calls: AtomicU64::new(0),
            last_attempts: AtomicU32::new(0),
            last_cutoff: AtomicI64::new(0),
            returns: 7,
        };
        let before = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let out = sweep_once(&cfg, Some(&reaper)).unwrap();

        let after = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert_eq!(reaper.calls.load(Ordering::SeqCst), 1);
        assert_eq!(reaper.last_attempts.load(Ordering::SeqCst), 3);
        let cutoff = reaper.last_cutoff.load(Ordering::SeqCst);
        let expected_min = before - cfg.zombie_max_age.as_secs() as i64;
        let expected_max = after - cfg.zombie_max_age.as_secs() as i64;
        assert!(
            cutoff >= expected_min && cutoff <= expected_max,
            "cutoff {} outside [{}, {}]",
            cutoff,
            expected_min,
            expected_max
        );
        assert_eq!(out.zombies_reaped, 7);
    }

    #[test]
    fn sweep_skips_reaper_when_none() {
        let dir = tempdir().unwrap();
        let cfg = mk_cfg(dir.path().to_path_buf(), 1000);

        let out = sweep_once(&cfg, None).unwrap();

        assert_eq!(out.zombies_reaped, 0);
    }
}
