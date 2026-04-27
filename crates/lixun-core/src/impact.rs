//! Global system impact preset (CPU/RAM/I-O footprint level).

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use std::time::Duration;

const MB: usize = 1024 * 1024;

/// Operator-chosen footprint level. Seeds defaults for every tunable
/// in the codebase; explicit per-knob config keys still override.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SystemImpact {
    Unlimited,
    High,
    Medium,
    Low,
}

impl SystemImpact {
    /// Every level, in canonical CLI listing order.
    pub const ALL: &'static [SystemImpact] = &[
        SystemImpact::Unlimited,
        SystemImpact::High,
        SystemImpact::Medium,
        SystemImpact::Low,
    ];

    fn as_str(self) -> &'static str {
        match self {
            SystemImpact::Unlimited => "unlimited",
            SystemImpact::High => "high",
            SystemImpact::Medium => "medium",
            SystemImpact::Low => "low",
        }
    }
}

impl fmt::Display for SystemImpact {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SystemImpact {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "unlimited" => Ok(SystemImpact::Unlimited),
            "high" => Ok(SystemImpact::High),
            "medium" => Ok(SystemImpact::Medium),
            "low" => Ok(SystemImpact::Low),
            _ => Err(format!(
                "invalid level \"{s}\"; expected one of: unlimited, high, medium, low"
            )),
        }
    }
}

/// Resolved per-level knob values. Built once at startup via
/// [`ImpactProfile::from_level`]; consumers read the fields they
/// care about. Plugin-agnostic by construction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImpactProfile {
    pub level: SystemImpact,
    pub tokio_worker_threads: usize,
    pub onnx_intra_threads: usize,
    pub onnx_inter_threads: usize,
    pub rayon_threads: usize,
    pub tantivy_heap_bytes: usize,
    pub tantivy_num_threads: usize,
    pub embed_batch_hint: usize,
    pub embed_concurrency_hint: Option<usize>,
    pub ocr_jobs_per_tick: usize,
    pub ocr_adaptive_throttle: bool,
    pub ocr_nice_level: i32,
    pub ocr_io_class_idle: bool,
    pub ocr_worker_interval: Duration,
    pub extract_cache_max_bytes: usize,
    pub max_file_size_bytes: u64,
    pub gloda_batch_size: usize,
    pub daemon_nice: i32,
    pub daemon_sched_idle: bool,
}

impl ImpactProfile {
    /// Materialise the per-level table. `num_cpus` is supplied by the
    /// caller (this crate stays dep-light and does not link `num_cpus`).
    ///
    /// The Medium row's `tokio_worker_threads` uses `max(num_cpus/2, 2)`:
    /// the floor of 2 keeps the runtime usable on single-core hosts where
    /// `num_cpus/2` would otherwise round to 0 or 1.
    pub fn from_level(level: SystemImpact, num_cpus: usize) -> Self {
        match level {
            SystemImpact::Unlimited => Self {
                level,
                tokio_worker_threads: num_cpus,
                onnx_intra_threads: num_cpus,
                onnx_inter_threads: num_cpus,
                rayon_threads: num_cpus,
                tantivy_heap_bytes: 200 * MB,
                tantivy_num_threads: num_cpus,
                embed_batch_hint: 64,
                embed_concurrency_hint: None,
                ocr_jobs_per_tick: 200,
                ocr_adaptive_throttle: false,
                ocr_nice_level: 0,
                ocr_io_class_idle: false,
                ocr_worker_interval: Duration::from_secs(1),
                extract_cache_max_bytes: 2000 * MB,
                max_file_size_bytes: 500u64 * 1024 * 1024,
                gloda_batch_size: 5000,
                daemon_nice: 0,
                daemon_sched_idle: false,
            },
            SystemImpact::High => Self {
                level,
                tokio_worker_threads: num_cpus,
                onnx_intra_threads: 4,
                onnx_inter_threads: 2,
                rayon_threads: num_cpus.min(4),
                tantivy_heap_bytes: 100 * MB,
                tantivy_num_threads: 4,
                embed_batch_hint: 32,
                embed_concurrency_hint: None,
                ocr_jobs_per_tick: 100,
                ocr_adaptive_throttle: false,
                ocr_nice_level: 5,
                ocr_io_class_idle: false,
                ocr_worker_interval: Duration::from_secs(1),
                extract_cache_max_bytes: 500 * MB,
                max_file_size_bytes: 50u64 * 1024 * 1024,
                gloda_batch_size: 2500,
                daemon_nice: 0,
                daemon_sched_idle: false,
            },
            SystemImpact::Medium => Self {
                level,
                tokio_worker_threads: (num_cpus / 2).max(2),
                onnx_intra_threads: 2,
                onnx_inter_threads: 1,
                rayon_threads: 2,
                tantivy_heap_bytes: 64 * MB,
                tantivy_num_threads: 2,
                embed_batch_hint: 16,
                embed_concurrency_hint: Some(1),
                ocr_jobs_per_tick: 20,
                ocr_adaptive_throttle: true,
                ocr_nice_level: 15,
                ocr_io_class_idle: true,
                ocr_worker_interval: Duration::from_secs(5),
                extract_cache_max_bytes: 200 * MB,
                max_file_size_bytes: 20u64 * 1024 * 1024,
                gloda_batch_size: 1000,
                daemon_nice: 5,
                daemon_sched_idle: false,
            },
            SystemImpact::Low => Self {
                level,
                tokio_worker_threads: 2,
                onnx_intra_threads: 1,
                onnx_inter_threads: 1,
                rayon_threads: 1,
                tantivy_heap_bytes: 32 * MB,
                tantivy_num_threads: 1,
                embed_batch_hint: 8,
                embed_concurrency_hint: Some(1),
                ocr_jobs_per_tick: 5,
                ocr_adaptive_throttle: true,
                ocr_nice_level: 19,
                ocr_io_class_idle: true,
                ocr_worker_interval: Duration::from_secs(30),
                extract_cache_max_bytes: 100 * MB,
                max_file_size_bytes: 5u64 * 1024 * 1024,
                gloda_batch_size: 200,
                daemon_nice: 10,
                daemon_sched_idle: true,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_level_low_8() {
        let p = ImpactProfile::from_level(SystemImpact::Low, 8);
        assert_eq!(p.tokio_worker_threads, 2);
        assert_eq!(p.onnx_intra_threads, 1);
        assert_eq!(p.rayon_threads, 1);
        assert_eq!(p.tantivy_heap_bytes, 32 * 1024 * 1024);
        assert!(p.daemon_sched_idle);
        assert_eq!(p.embed_concurrency_hint, Some(1));
    }

    #[test]
    fn from_level_medium_8() {
        let p = ImpactProfile::from_level(SystemImpact::Medium, 8);
        assert_eq!(p.tokio_worker_threads, 4);
        assert_eq!(p.onnx_intra_threads, 2);
        assert_eq!(p.tantivy_heap_bytes, 64 * 1024 * 1024);
    }

    #[test]
    fn from_level_high_8() {
        let p = ImpactProfile::from_level(SystemImpact::High, 8);
        assert_eq!(p.tokio_worker_threads, 8);
        assert_eq!(p.onnx_intra_threads, 4);
        assert_eq!(p.tantivy_heap_bytes, 100 * 1024 * 1024);
        assert!(!p.daemon_sched_idle);
    }

    #[test]
    fn from_level_unlimited_8() {
        let p = ImpactProfile::from_level(SystemImpact::Unlimited, 8);
        assert_eq!(p.tokio_worker_threads, 8);
        assert_eq!(p.onnx_intra_threads, 8);
        assert_eq!(p.onnx_inter_threads, 8);
        assert_eq!(p.rayon_threads, 8);
        assert_eq!(p.tantivy_num_threads, 8);
        assert_eq!(p.embed_concurrency_hint, None);
        assert!(!p.ocr_adaptive_throttle);
    }

    #[test]
    fn from_level_medium_clamps_tokio_workers() {
        let p = ImpactProfile::from_level(SystemImpact::Medium, 1);
        assert_eq!(p.tokio_worker_threads, 2);
    }

    #[test]
    fn serde_roundtrip_low() {
        let s = serde_json::to_string(&SystemImpact::Low).unwrap();
        assert_eq!(s, "\"low\"");
        let back: SystemImpact = serde_json::from_str(&s).unwrap();
        assert_eq!(back, SystemImpact::Low);
    }

    #[test]
    fn serde_accepts_all_levels() {
        for (lit, want) in [
            ("\"unlimited\"", SystemImpact::Unlimited),
            ("\"high\"", SystemImpact::High),
            ("\"medium\"", SystemImpact::Medium),
            ("\"low\"", SystemImpact::Low),
        ] {
            let got: SystemImpact = serde_json::from_str(lit).unwrap();
            assert_eq!(got, want);
        }
    }

    #[test]
    fn serde_rejects_unknown() {
        let r = serde_json::from_str::<SystemImpact>("\"bogus\"");
        assert!(r.is_err());
    }

    #[test]
    fn fromstr_ok_and_err() {
        assert_eq!("medium".parse::<SystemImpact>().unwrap(), SystemImpact::Medium);
        let err = "BOGUS".parse::<SystemImpact>().unwrap_err();
        assert_eq!(
            err,
            "invalid level \"BOGUS\"; expected one of: unlimited, high, medium, low"
        );
    }

    #[test]
    fn display_lowercase() {
        assert_eq!(format!("{}", SystemImpact::Unlimited), "unlimited");
        assert_eq!(format!("{}", SystemImpact::High), "high");
        assert_eq!(format!("{}", SystemImpact::Medium), "medium");
        assert_eq!(format!("{}", SystemImpact::Low), "low");
    }

    #[test]
    fn high_4_byte_sizes_consistent() {
        let p = ImpactProfile::from_level(SystemImpact::High, 4);
        assert_eq!(p.tantivy_heap_bytes, 100 * 1024 * 1024);
        assert_eq!(p.extract_cache_max_bytes, 500 * 1024 * 1024);
    }

    #[test]
    fn all_lists_every_level() {
        assert_eq!(SystemImpact::ALL.len(), 4);
        assert!(SystemImpact::ALL.contains(&SystemImpact::Unlimited));
        assert!(SystemImpact::ALL.contains(&SystemImpact::Low));
    }
}
