//! Idle-gated OCR worker tick (OCR-T6).
//!
//! The indexer hot path enqueues scan PDFs and images into the
//! persistent [`OcrQueue`] (DB-13). This module defines the worker
//! that drains that queue in the background, one job per tick,
//! only while the system is idle, and writes the OCR'd text back
//! through a [`WriterSink`].
//!
//! The tick body is split out into [`tick_once`] so tests can drive
//! it synchronously without spinning up a real timer, injecting a
//! mock sink and a stub for [`lixun_extract::ocr::run_ocr_job`].

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use lixun_extract::ExtractorCapabilities;
use lixun_extract::ocr_queue::OcrQueue;

use crate::writer_sink::WriterSink;

/// Composable "is the daemon idle right now?" probe.
///
/// Used by the OCR worker to throttle itself. The production gate
/// in `lixun-daemon` is driven by `stats.reindex_in_progress`; T6.5
/// will add a CPU PSI probe and a composite gate that `all`s them.
/// Kept synchronous so the trait remains `dyn`-compatible — the
/// daemon's stats gate resolves its tokio `RwLock` via non-blocking
/// `try_read` and treats contention as "busy".
pub trait IdleGate: Send + Sync + 'static {
    fn is_idle(&self) -> bool;
}

/// Target of the body mutation produced by each drained job.
/// Abstracted out so unit tests can capture calls without spinning
/// up a real `IndexWriter`. Production uses [`WriterSink`].
///
/// Async because [`tick_once`] runs on a tokio worker thread and
/// the production implementation writes to the writer_loop mpsc
/// with `tx.send(...).await`. A sync trait would force a nested
/// `block_on` which panics at runtime.
pub trait UpsertBodySink: Send + Sync + 'static {
    fn upsert_body<'a>(
        &'a self,
        doc_id: &'a str,
        body: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>>;
}

impl UpsertBodySink for WriterSink {
    fn upsert_body<'a>(
        &'a self,
        doc_id: &'a str,
        body: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(WriterSink::upsert_body(self, doc_id, body))
    }
}

/// Tunables resolved from `[ocr]` config plus probed caps.
#[derive(Debug, Clone)]
pub struct OcrWorkerCfg {
    pub interval: Duration,
    pub langs: Vec<String>,
    pub min_image_side_px: u32,
    pub max_pages_per_pdf: Option<usize>,
    pub max_attempts: u32,
    /// `Some((nice, ioprio_idle))` when `[ocr].adaptive_throttle =
    /// true`. Drives per-job `SystemRunner::with_low_priority` so
    /// tesseract/pdftoppm spawn with reduced CPU and (optionally)
    /// I/O priority (DB-15).
    pub throttle: Option<(i32, bool)>,
}

/// Outcome of a single [`tick_once`] call. Enum shape supports
/// deterministic assertions in unit tests; production code ignores
/// the return value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TickOutcome {
    NoIdle,
    Empty,
    Drained { doc_id: String },
    StaleSkipped { doc_id: String },
    Failed { doc_id: String, err: String },
    PermanentSkip { doc_id: String },
}

/// Run one OCR job from the queue if the gate says we're idle.
///
/// Returns an [`TickOutcome`] describing what happened. The real
/// OCR call is injected via `run_ocr` so tests can substitute a
/// closure. The production wrapper in [`spawn`] binds this to
/// [`lixun_extract::ocr::run_ocr_job`].
pub async fn tick_once<S, G, F>(
    queue: &OcrQueue,
    idle: &G,
    sink: &S,
    caps: &ExtractorCapabilities,
    cfg: &OcrWorkerCfg,
    run_ocr: F,
) -> TickOutcome
where
    S: UpsertBodySink + ?Sized,
    G: IdleGate + ?Sized,
    F: Fn(&Path, &str, &[String], &ExtractorCapabilities, u32, Option<usize>) -> Result<Option<String>>
        + Send,
{
    if !idle.is_idle() {
        tracing::trace!("ocr tick: not idle, skipping");
        return TickOutcome::NoIdle;
    }

    let row = match queue.peek_next(cfg.max_attempts) {
        Ok(Some(r)) => r,
        Ok(None) => {
            tracing::debug!("ocr tick: no pending jobs");
            return TickOutcome::Empty;
        }
        Err(e) => {
            tracing::warn!("ocr tick: peek_next failed: {e:#}");
            return TickOutcome::Empty;
        }
    };

    let doc_id = row.doc_id.clone();
    let path = PathBuf::from(&row.path);
    let expected_mtime = row.mtime;
    let expected_size = row.size;

    let caps_for_ocr = caps.clone();
    let langs = cfg.langs.clone();
    let min_side = cfg.min_image_side_px;
    let max_pages = cfg.max_pages_per_pdf;
    let ext = row.ext.clone();
    let path_for_ocr = path.clone();

    let ocr_result = run_ocr(
        &path_for_ocr,
        &ext,
        &langs,
        &caps_for_ocr,
        min_side,
        max_pages,
    );

    match ocr_result {
        Ok(Some(text)) => {
            if !idle.is_idle() {
                tracing::warn!(
                    "ocr completed but idle gate flipped; keeping row {doc_id}"
                );
                return TickOutcome::NoIdle;
            }
            // AM-6: detect watcher-vs-worker races by re-stating the
            // file. If mtime/size changed, the bytes we just OCR'd
            // are stale — drop the result and let the watcher's next
            // reindex re-enqueue with fresh metadata.
            match std::fs::metadata(&path) {
                Ok(md) => {
                    let cur_mtime = filetime_secs(&md);
                    let cur_size = md.len();
                    if cur_mtime == expected_mtime && cur_size == expected_size {
                        if let Err(e) = sink.upsert_body(&doc_id, &text).await {
                            tracing::warn!("ocr: upsert_body failed for {doc_id}: {e:#}");
                            return TickOutcome::Failed {
                                doc_id,
                                err: format!("upsert_body: {e}"),
                            };
                        }
                        if let Err(e) = queue.remove(&doc_id) {
                            tracing::warn!("ocr: queue.remove failed for {doc_id}: {e:#}");
                        }
                        tracing::info!(
                            "ocr ok: {} ({} chars)",
                            path.display(),
                            text.len()
                        );
                        TickOutcome::Drained { doc_id }
                    } else {
                        tracing::warn!(
                            "ocr stale: {} mtime/size changed mid-OCR; dropping result",
                            path.display()
                        );
                        if let Err(e) = queue.remove(&doc_id) {
                            tracing::warn!("ocr: queue.remove failed for {doc_id}: {e:#}");
                        }
                        TickOutcome::StaleSkipped { doc_id }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "ocr: file gone mid-OCR {}: {e}",
                        path.display()
                    );
                    if let Err(e2) = queue.remove(&doc_id) {
                        tracing::warn!("ocr: queue.remove failed for {doc_id}: {e2:#}");
                    }
                    TickOutcome::StaleSkipped { doc_id }
                }
            }
        }
        Ok(None) => {
            let msg = "ocr returned empty (e.g. below min_side_px)";
            if let Err(e) = queue.mark_failure(&doc_id, msg) {
                tracing::warn!("ocr: mark_failure failed for {doc_id}: {e:#}");
            }
            tracing::info!("ocr skipped permanently: {}", path.display());
            TickOutcome::PermanentSkip { doc_id }
        }
        Err(e) => {
            let err_str = format!("{e:#}");
            if let Err(e2) = queue.mark_failure(&doc_id, &err_str) {
                tracing::warn!("ocr: mark_failure failed for {doc_id}: {e2:#}");
            }
            tracing::warn!("ocr failed {}: {err_str}", path.display());
            TickOutcome::Failed {
                doc_id,
                err: err_str,
            }
        }
    }
}

#[cfg(unix)]
fn filetime_secs(md: &std::fs::Metadata) -> i64 {
    use std::os::unix::fs::MetadataExt;
    md.mtime()
}

#[cfg(not(unix))]
fn filetime_secs(md: &std::fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Spawn the production OCR worker. Ticks every `cfg.interval` and
/// calls [`tick_once`] with the real [`lixun_extract::ocr::run_ocr_job`].
pub fn spawn(
    queue: Arc<OcrQueue>,
    idle: Arc<dyn IdleGate>,
    sink: Arc<WriterSink>,
    caps: Arc<ExtractorCapabilities>,
    cfg: OcrWorkerCfg,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(cfg.interval);
        timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
        tracing::info!(
            "ocr worker: started, interval={:?}, langs={:?}, max_attempts={}",
            cfg.interval,
            cfg.langs,
            cfg.max_attempts
        );
        loop {
            timer.tick().await;

            let throttle = cfg.throttle;
            let run_ocr =
                |path: &Path,
                 ext: &str,
                 langs: &[String],
                 caps: &ExtractorCapabilities,
                 min_side: u32,
                 max_pages: Option<usize>|
                 -> Result<Option<String>> {
                    let caps_clone = caps.clone();
                    let langs_vec = langs.to_vec();
                    let path_owned = path.to_path_buf();
                    let ext_owned = ext.to_string();
                    tokio::task::block_in_place(|| {
                        if let Some((nice, ioprio_idle)) = throttle {
                            let runner = lixun_extract::shell::SystemRunner::new(
                                caps_clone.timeout.as_secs(),
                            )
                            .with_low_priority(nice, ioprio_idle);
                            lixun_extract::ocr::run_ocr_job_with(
                                &path_owned,
                                &ext_owned,
                                &langs_vec,
                                min_side,
                                max_pages,
                                &runner,
                            )
                        } else {
                            lixun_extract::ocr::run_ocr_job(
                                &path_owned,
                                &ext_owned,
                                &langs_vec,
                                &caps_clone,
                                min_side,
                                max_pages,
                            )
                        }
                    })
                };

            let outcome = tick_once(
                queue.as_ref(),
                idle.as_ref(),
                sink.as_ref(),
                caps.as_ref(),
                &cfg,
                run_ocr,
            )
            .await;

            tracing::trace!(?outcome, "ocr worker: tick complete");
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lixun_extract::ocr_queue::{OcrQueue, OcrQueueRow};
    use rusqlite::Connection;
    use std::sync::Mutex as StdMutex;

    struct FixedIdle(bool);

    impl IdleGate for FixedIdle {
        fn is_idle(&self) -> bool {
            self.0
        }
    }

    struct CapturingSink {
        calls: StdMutex<Vec<(String, String)>>,
    }

    impl CapturingSink {
        fn new() -> Self {
            Self {
                calls: StdMutex::new(Vec::new()),
            }
        }
        fn calls(&self) -> Vec<(String, String)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl UpsertBodySink for CapturingSink {
        fn upsert_body<'a>(
            &'a self,
            doc_id: &'a str,
            body: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
            let doc_id = doc_id.to_string();
            let body = body.to_string();
            Box::pin(async move {
                self.calls.lock().unwrap().push((doc_id, body));
                Ok(())
            })
        }
    }

    fn mem_queue() -> OcrQueue {
        let conn = Connection::open_in_memory().unwrap();
        OcrQueue::from_connection(conn).unwrap()
    }

    fn test_caps() -> ExtractorCapabilities {
        ExtractorCapabilities::all_available_no_timeout()
    }

    fn test_cfg() -> OcrWorkerCfg {
        OcrWorkerCfg {
            interval: Duration::from_secs(60),
            langs: vec!["eng".into()],
            min_image_side_px: 200,
            max_pages_per_pdf: None,
            max_attempts: 3,
            throttle: None,
        }
    }

    fn enqueue_for_existing_file(queue: &OcrQueue, doc_id: &str, path: &Path) -> OcrQueueRow {
        let md = std::fs::metadata(path).unwrap();
        use std::os::unix::fs::MetadataExt;
        let row = OcrQueueRow {
            doc_id: doc_id.to_string(),
            path: path.to_string_lossy().to_string(),
            mtime: md.mtime(),
            size: md.len(),
            ext: "png".into(),
            enqueued_at: 0,
            attempts: 0,
            last_error: None,
        };
        queue.enqueue(row.clone()).unwrap();
        row
    }

    #[tokio::test]
    async fn noop_when_not_idle() {
        let queue = mem_queue();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("scan.png");
        std::fs::write(&p, b"fake").unwrap();
        enqueue_for_existing_file(&queue, "fs:/scan", &p);

        let sink = CapturingSink::new();
        let idle = FixedIdle(false);
        let caps = test_caps();
        let cfg = test_cfg();
        let ocr_called = StdMutex::new(0usize);
        let run_ocr = |_p: &Path,
                       _e: &str,
                       _l: &[String],
                       _c: &ExtractorCapabilities,
                       _m: u32,
                       _mp: Option<usize>|
         -> Result<Option<String>> {
            *ocr_called.lock().unwrap() += 1;
            Ok(Some("should not run".into()))
        };

        let outcome = tick_once(&queue, &idle, &sink, &caps, &cfg, run_ocr).await;

        assert_eq!(outcome, TickOutcome::NoIdle);
        assert_eq!(*ocr_called.lock().unwrap(), 0);
        assert!(sink.calls().is_empty());
        assert_eq!(queue.len().unwrap(), 1);
    }

    #[tokio::test]
    async fn drain_one_when_idle() {
        let queue = mem_queue();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("scan.png");
        std::fs::write(&p, b"fake-png").unwrap();
        enqueue_for_existing_file(&queue, "fs:/scan", &p);

        let sink = CapturingSink::new();
        let idle = FixedIdle(true);
        let caps = test_caps();
        let cfg = test_cfg();
        let run_ocr = |_p: &Path,
                       _e: &str,
                       _l: &[String],
                       _c: &ExtractorCapabilities,
                       _m: u32,
                       _mp: Option<usize>|
         -> Result<Option<String>> { Ok(Some("MOCK_OCR_TEXT".into())) };

        let outcome = tick_once(&queue, &idle, &sink, &caps, &cfg, run_ocr).await;

        assert_eq!(
            outcome,
            TickOutcome::Drained {
                doc_id: "fs:/scan".into()
            }
        );
        assert_eq!(queue.len().unwrap(), 0);
        let calls = sink.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "fs:/scan");
        assert_eq!(calls[0].1, "MOCK_OCR_TEXT");
    }

    #[tokio::test]
    async fn attempts_increment_on_failure() {
        let queue = mem_queue();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("broken.png");
        std::fs::write(&p, b"bad").unwrap();
        enqueue_for_existing_file(&queue, "fs:/broken", &p);

        let sink = CapturingSink::new();
        let idle = FixedIdle(true);
        let caps = test_caps();
        let cfg = test_cfg();
        let run_ocr = |_p: &Path,
                       _e: &str,
                       _l: &[String],
                       _c: &ExtractorCapabilities,
                       _m: u32,
                       _mp: Option<usize>|
         -> Result<Option<String>> {
            Err(anyhow::anyhow!("tesseract exploded"))
        };

        let outcome = tick_once(&queue, &idle, &sink, &caps, &cfg, run_ocr).await;

        match outcome {
            TickOutcome::Failed { doc_id, err } => {
                assert_eq!(doc_id, "fs:/broken");
                assert!(err.contains("tesseract exploded"), "err was {err}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert_eq!(queue.len().unwrap(), 1);
        let row = queue.peek_next(10).unwrap().unwrap();
        assert_eq!(row.attempts, 1);
        assert!(row.last_error.as_deref().unwrap().contains("tesseract"));
        assert!(sink.calls().is_empty());
    }

    #[tokio::test]
    async fn stops_after_max_attempts() {
        let queue = mem_queue();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("dead.png");
        std::fs::write(&p, b"bad").unwrap();
        enqueue_for_existing_file(&queue, "fs:/dead", &p);
        queue.mark_failure("fs:/dead", "e1").unwrap();
        queue.mark_failure("fs:/dead", "e2").unwrap();
        queue.mark_failure("fs:/dead", "e3").unwrap();

        let sink = CapturingSink::new();
        let idle = FixedIdle(true);
        let caps = test_caps();
        let cfg = test_cfg();
        let called = StdMutex::new(false);
        let run_ocr = |_p: &Path,
                       _e: &str,
                       _l: &[String],
                       _c: &ExtractorCapabilities,
                       _m: u32,
                       _mp: Option<usize>|
         -> Result<Option<String>> {
            *called.lock().unwrap() = true;
            Ok(Some("x".into()))
        };

        let outcome = tick_once(&queue, &idle, &sink, &caps, &cfg, run_ocr).await;

        assert_eq!(outcome, TickOutcome::Empty);
        assert!(!*called.lock().unwrap(), "run_ocr must not be called");
        assert!(sink.calls().is_empty());
        assert_eq!(queue.len().unwrap(), 1);
    }

    #[tokio::test]
    async fn stale_file_dropped() {
        let queue = mem_queue();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("stale.png");
        std::fs::write(&p, b"original").unwrap();
        let row = enqueue_for_existing_file(&queue, "fs:/stale", &p);
        // Mutate on-disk size so the stat after OCR no longer matches
        // the enqueue snapshot. The tick must drop the result.
        std::fs::write(&p, b"different size because we wrote more bytes").unwrap();
        assert_ne!(std::fs::metadata(&p).unwrap().len(), row.size);

        let sink = CapturingSink::new();
        let idle = FixedIdle(true);
        let caps = test_caps();
        let cfg = test_cfg();
        let run_ocr = |_p: &Path,
                       _e: &str,
                       _l: &[String],
                       _c: &ExtractorCapabilities,
                       _m: u32,
                       _mp: Option<usize>|
         -> Result<Option<String>> { Ok(Some("SHOULD_BE_DROPPED".into())) };

        let outcome = tick_once(&queue, &idle, &sink, &caps, &cfg, run_ocr).await;

        assert_eq!(
            outcome,
            TickOutcome::StaleSkipped {
                doc_id: "fs:/stale".into()
            }
        );
        assert!(sink.calls().is_empty());
        assert_eq!(queue.len().unwrap(), 0);
    }
}
