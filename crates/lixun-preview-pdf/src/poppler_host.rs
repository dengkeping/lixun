//! Single-threaded `poppler::Document` owner — `PopplerHost`.
//!
//! Shape C: a single dedicated thread owns the only Document instance;
//! all callers (main thread, render-result drain, search code) talk to
//! that thread via typed commands sent over an `mpsc::Sender`.
//!
//! `poppler::Document` is `!Send`. Instead of cloning the Document across
//! main / visible-render / prefetch-render / search threads, this module
//! introduces a single dedicated thread that owns the only Document
//! instance. All callers (main thread, render-result drain, search code)
//! talk to that thread via typed commands sent over an `mpsc::Sender`.
//!
//! Result transport is unchanged from the prior `worker` design:
//! `gdk::MemoryTexture` is `Send` (verified upstream in gdk4 crate), so
//! the host thread builds the texture and ships it across an
//! `async_channel::Sender<RenderResult>` for the main thread to drain.
//!
//! Lifecycle:
//! - `PopplerHost::spawn()` creates the thread; the Document is NOT
//!   opened yet (lazy open on first command after a `ReplacePath`).
//! - `replace_path(path, epoch)` drops the old Document (if any) and
//!   records the new path; the next command that needs the Document
//!   triggers the actual `Document::from_file` call.
//! - `submit(job)` enqueues a render command.
//! - Dropping the `PopplerHost` sends `Shutdown` and joins the thread.
//!
//! This module currently only carries the `Render` command. `Text`,
//! `Find`, and `PageSize` commands land in T4/T5/T6 as their call sites
//! migrate; the dispatch loop is structured so adding them is a single
//! `match` arm each.

use std::path::PathBuf;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cairo::{Context as CairoCtx, Format as CairoFormat, ImageSurface};
use poppler::Document;

use crate::document_session::{MAX_RENDER_BUCKET, path_to_file_uri};
use crate::search::PageSearchResult;
use crate::worker::{RenderJob, RenderOutcome, RenderResult};

const POINTS_PER_INCH: f64 = 72.0;
const BASE_DPI: f64 = 96.0;

/// How long the host stays idle (no commands, no inflight work) before
/// dropping its open `poppler::Document` to release poppler's internal
/// per-page object cache (`Catalog::cachePageTree` / `cacheSubTree`).
///
/// Poppler accumulates ~3-4 MB per accessed page in this cache and
/// exposes no API to evict individual entries — only dropping the
/// Document frees it. On a 314-page IC datasheet this is ~1 GB of
/// idle RSS; collapsing the Document on idle returns RSS to baseline.
///
/// The next command after a drop triggers `ensure_document_open` and
/// re-parses the cross-ref table (~200-500 ms one-time cost).
///
/// Test override: `LIXUN_PDF_IDLE_DROP_MS` (parsed once at host spawn).
const DEFAULT_IDLE_COOLDOWN: Duration = Duration::from_secs(2);
/// How often the host wakes up to check the idle clock when no commands
/// are arriving. Independent of the cooldown so a short cooldown still
/// gets evaluated promptly.
const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Commands the host thread accepts. New variants land here as call
/// sites migrate (T5 adds `Probe`, T6 adds `PageSize`).
enum Cmd {
    /// Render a page at a given quarter-zoom bucket. Stale jobs (job
    /// epoch below current host epoch) are silently dropped.
    Render(RenderJob),
    /// Start a streaming text search over `n_pages`. The host scans
    /// pages in order and ships one `PageSearchResult` per page over
    /// `reply`. A newer `FindStart` (higher generation) invalidates an
    /// in-flight scan between pages.
    FindStart {
        generation: u64,
        query: String,
        n_pages: u32,
        reply: async_channel::Sender<PageSearchResult>,
    },
    /// Drop the currently-open Document and remember a new path. The
    /// next `Render` or `FindStart` opens it.
    ReplacePath { path: PathBuf, epoch: u64 },
    /// Stop the loop and let the thread join.
    Shutdown,
    /// Test-only: report whether the host currently holds an open
    /// Document. Used to validate the idle-drop cooldown path.
    #[cfg(test)]
    Probe(mpsc::Sender<bool>),
}

pub struct PopplerHost {
    tx: mpsc::Sender<Cmd>,
    join: Option<JoinHandle<()>>,
}

impl PopplerHost {
    /// Spawn the host thread. Document is opened lazily on the first
    /// command that requires it, after a `replace_path` call.
    pub fn spawn(
        path: PathBuf,
        initial_epoch: u64,
        result_tx: async_channel::Sender<RenderResult>,
    ) -> anyhow::Result<Self> {
        let (tx, rx) = mpsc::channel::<Cmd>();
        let join = std::thread::Builder::new()
            .name("pdf-poppler-host".to_string())
            .spawn(move || host_loop(path, initial_epoch, rx, result_tx))
            .map_err(|e| anyhow::anyhow!("spawn poppler host: {}", e))?;
        Ok(Self {
            tx,
            join: Some(join),
        })
    }

    /// Submit a render job. Best-effort: a closed channel (host already
    /// shut down) silently drops the job.
    pub fn submit(&self, job: RenderJob) {
        let _ = self.tx.send(Cmd::Render(job));
    }

    /// Tell the host to drop its current Document and adopt a new path
    /// at the given epoch. The new Document opens lazily on the next
    /// `Render` command.
    pub fn replace_path(&self, path: PathBuf, epoch: u64) {
        let _ = self.tx.send(Cmd::ReplacePath { path, epoch });
    }

    /// Submit a streaming text search. Caller owns the receiver end of
    /// `reply`. A newer call (higher `generation`) preempts an
    /// in-flight scan between pages — the host checks the latest
    /// generation it has seen and aborts the loop early. Empty query
    /// is dropped silently.
    pub fn find_start(
        &self,
        generation: u64,
        query: String,
        n_pages: u32,
        reply: async_channel::Sender<PageSearchResult>,
    ) {
        let _ = self.tx.send(Cmd::FindStart {
            generation,
            query,
            n_pages,
            reply,
        });
    }
}

impl Drop for PopplerHost {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Shutdown);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

#[cfg(test)]
impl PopplerHost {
    /// Test-only: ask the host whether it currently holds an open
    /// Document. Returns `None` if the host has shut down.
    fn probe_has_document(&self) -> Option<bool> {
        let (tx, rx) = mpsc::channel::<bool>();
        self.tx.send(Cmd::Probe(tx)).ok()?;
        rx.recv_timeout(Duration::from_secs(2)).ok()
    }

    /// Test-only: spawn with an explicit idle-drop cooldown. Bypasses
    /// the `LIXUN_PDF_IDLE_DROP_MS` env var so parallel tests don't
    /// race on a shared environment.
    fn spawn_with_cooldown(
        path: PathBuf,
        initial_epoch: u64,
        result_tx: async_channel::Sender<RenderResult>,
        idle_cooldown: Duration,
    ) -> anyhow::Result<Self> {
        let (tx, rx) = mpsc::channel::<Cmd>();
        let join = std::thread::Builder::new()
            .name("pdf-poppler-host-test".to_string())
            .spawn(move || {
                host_loop_inner(path, initial_epoch, rx, result_tx, idle_cooldown)
            })
            .map_err(|e| anyhow::anyhow!("spawn poppler host: {}", e))?;
        Ok(Self {
            tx,
            join: Some(join),
        })
    }
}

fn host_loop(
    initial_path: PathBuf,
    initial_epoch: u64,
    rx: mpsc::Receiver<Cmd>,
    result_tx: async_channel::Sender<RenderResult>,
) {
    host_loop_inner(initial_path, initial_epoch, rx, result_tx, read_idle_cooldown());
}

fn host_loop_inner(
    initial_path: PathBuf,
    initial_epoch: u64,
    rx: mpsc::Receiver<Cmd>,
    result_tx: async_channel::Sender<RenderResult>,
    idle_cooldown: Duration,
) {
    let mut current_path: PathBuf = initial_path;
    let mut current_epoch: u64 = initial_epoch;
    // Highest find generation the host has accepted so far. An
    // in-flight scan checks this between pages and aborts early when
    // a newer generation has arrived (set by a later `FindStart`
    // peeked off the queue, or by `ReplacePath` invalidation).
    let mut current_find_generation: u64 = 0;
    let mut document: Option<Document> = None;
    let mut last_activity = Instant::now();

    loop {
        match rx.recv_timeout(IDLE_POLL_INTERVAL) {
            Ok(Cmd::ReplacePath { path, epoch }) => {
                document = None;
                current_path = path;
                current_epoch = epoch;
                current_find_generation = current_find_generation.saturating_add(1);
                last_activity = Instant::now();
                tracing::info!(
                    target = "lixun-preview-pdf",
                    epoch = current_epoch,
                    "poppler host: ReplacePath (lazy reopen on next render)"
                );
            }
            Ok(Cmd::Render(job)) => {
                if job.epoch < current_epoch {
                    tracing::trace!(
                        target = "lixun-preview-pdf",
                        page = job.page_index,
                        job_epoch = job.epoch,
                        host_epoch = current_epoch,
                        "poppler host: drop stale render"
                    );
                    continue;
                }
                ensure_document_open(&mut document, &current_path);
                let Some(doc) = document.as_ref() else {
                    let _ = result_tx.send_blocking(RenderResult {
                        page_index: job.page_index,
                        zoom_bucket: job.zoom_bucket,
                        epoch: job.epoch,
                        outcome: RenderOutcome::Err(format!(
                            "poppler host: document not open for {:?}",
                            current_path
                        )),
                    });
                    continue;
                };
                let outcome = render_one(doc, job);
                let _ = result_tx.send_blocking(RenderResult {
                    page_index: job.page_index,
                    zoom_bucket: job.zoom_bucket,
                    epoch: job.epoch,
                    outcome,
                });
                last_activity = Instant::now();
            }
            Ok(Cmd::FindStart {
                generation,
                query,
                n_pages,
                reply,
            }) => {
                if generation > current_find_generation {
                    current_find_generation = generation;
                }
                if query.is_empty() {
                    continue;
                }
                ensure_document_open(&mut document, &current_path);
                let Some(doc) = document.as_ref() else {
                    continue;
                };
                find_scan(
                    doc,
                    &query,
                    generation,
                    n_pages,
                    &reply,
                    &rx,
                    &mut current_find_generation,
                );
                last_activity = Instant::now();
            }
            Ok(Cmd::Shutdown) => break,
            #[cfg(test)]
            Ok(Cmd::Probe(reply)) => {
                let _ = reply.send(document.is_some());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if document.is_some() && last_activity.elapsed() >= idle_cooldown {
                    document = None;
                    tracing::debug!(
                        target = "lixun-preview-pdf",
                        idle_ms = u64::try_from(last_activity.elapsed().as_millis())
                            .unwrap_or(u64::MAX),
                        "poppler host: idle drop"
                    );
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    // `document` drops here when the function returns.
    tracing::debug!(target = "lixun-preview-pdf", "poppler host exited");
}

/// Read the idle-drop cooldown from `LIXUN_PDF_IDLE_DROP_MS` env, or
/// fall back to `DEFAULT_IDLE_COOLDOWN`. A value of `0` disables the
/// drop (useful for tests that exercise the no-drop path).
fn read_idle_cooldown() -> Duration {
    match std::env::var("LIXUN_PDF_IDLE_DROP_MS") {
        Ok(s) => match s.parse::<u64>() {
            Ok(0) => Duration::from_secs(60 * 60 * 24 * 365),
            Ok(ms) => Duration::from_millis(ms),
            Err(_) => DEFAULT_IDLE_COOLDOWN,
        },
        Err(_) => DEFAULT_IDLE_COOLDOWN,
    }
}

/// Open the Document if not already open. Failure is logged and leaves
/// `*slot = None`; the caller then emits a `RenderOutcome::Err`.
fn ensure_document_open(slot: &mut Option<Document>, path: &std::path::Path) {
    if slot.is_some() {
        return;
    }
    let uri = match path_to_file_uri(path) {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!(
                target = "lixun-preview-pdf",
                "poppler host: uri for {:?}: {}",
                path,
                e
            );
            return;
        }
    };
    match Document::from_file(&uri, None) {
        Ok(d) => {
            tracing::info!(
                target = "lixun-preview-pdf",
                "poppler host: opened {:?}",
                path
            );
            *slot = Some(d);
        }
        Err(e) => {
            tracing::warn!(
                target = "lixun-preview-pdf",
                "poppler host: open {:?} failed: {:?}",
                path,
                e
            );
        }
    }
}

/// Render a single page to a `gdk::MemoryTexture`. Logic identical to
/// the previous `worker::render_one`; preserved here so worker.rs can
/// be slimmed down in T3. The single-allocation pixel-buffer pattern
/// from the previous T1 is preserved.
fn render_one(doc: &Document, job: RenderJob) -> RenderOutcome {
    let Some(page) = doc.page(job.page_index as i32) else {
        return RenderOutcome::Err(format!("page {} missing", job.page_index));
    };
    let effective_bucket = job.zoom_bucket.min(MAX_RENDER_BUCKET);
    let (pt_w, pt_h) = page.size();
    let zoom = f64::from(effective_bucket) / 4.0;
    let scale = (BASE_DPI / POINTS_PER_INCH) * zoom;
    let px_w = (pt_w * scale).ceil().max(1.0) as i32;
    let px_h = (pt_h * scale).ceil().max(1.0) as i32;

    let mut surface = match ImageSurface::create(CairoFormat::ARgb32, px_w, px_h) {
        Ok(s) => s,
        Err(e) => return RenderOutcome::Err(format!("surface {}x{}: {}", px_w, px_h, e)),
    };
    {
        let ctx = match CairoCtx::new(&surface) {
            Ok(c) => c,
            Err(e) => return RenderOutcome::Err(format!("cairo ctx: {}", e)),
        };
        ctx.set_source_rgb(1.0, 1.0, 1.0);
        if let Err(e) = ctx.paint() {
            return RenderOutcome::Err(format!("paint bg: {}", e));
        }
        ctx.scale(scale, scale);
        page.render(&ctx);
        ctx.target().flush();
    }
    let stride = surface.stride();
    // Single owned allocation for the pixel buffer: borrow the cairo
    // surface data, copy into `buf`, then drop the surface so cairo's
    // internal buffer is freed before we construct `glib::Bytes` and the
    // texture. This avoids holding two full-page allocations live
    // simultaneously.
    let buf = {
        let data = match surface.data() {
            Ok(d) => d,
            Err(e) => return RenderOutcome::Err(format!("data borrow: {}", e)),
        };
        let mut buf = Vec::with_capacity(data.len());
        buf.extend_from_slice(&data);
        buf
    };
    drop(surface);
    let bytes_len = buf.len();
    let glib_bytes = glib::Bytes::from_owned(buf);

    let texture = gdk::MemoryTexture::new(
        px_w,
        px_h,
        gdk::MemoryFormat::B8g8r8a8Premultiplied,
        &glib_bytes,
        stride as usize,
    );

    RenderOutcome::Ok {
        texture,
        width: px_w as u32,
        height: px_h as u32,
        bytes: bytes_len,
    }
}

fn find_scan(
    doc: &Document,
    query: &str,
    generation: u64,
    n_pages: u32,
    reply: &async_channel::Sender<PageSearchResult>,
    rx: &mpsc::Receiver<Cmd>,
    current_find_generation: &mut u64,
) {
    for page_idx in 0..n_pages {
        if let Ok(cmd) = rx.try_recv() {
            match cmd {
                Cmd::ReplacePath { .. } => return,
                Cmd::FindStart { generation: g, .. } if g > generation => {
                    *current_find_generation = g;
                    return;
                }
                Cmd::Shutdown => return,
                _ => {}
            }
        }
        if *current_find_generation != generation {
            return;
        }
        let Some(page) = doc.page(page_idx as i32) else {
            continue;
        };
        let rects = page.find_text_with_options(query, poppler::FindFlags::empty());
        let _ = reply.send_blocking(PageSearchResult {
            generation,
            page_idx,
            rects,
            done_for_page: true,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without GTK initialized we can't actually build a `MemoryTexture`,
    /// so most behaviour we test here is the message-loop lifecycle:
    /// spawn → ReplacePath → Shutdown → join cleanly; submitting a job
    /// against a non-existent path produces a `RenderOutcome::Err`
    /// rather than a panic.

    fn fresh_host(path: PathBuf) -> (PopplerHost, async_channel::Receiver<RenderResult>) {
        let (tx, rx) = async_channel::unbounded::<RenderResult>();
        let host = PopplerHost::spawn(path, 1, tx).expect("spawn host");
        (host, rx)
    }

    #[test]
    fn spawn_and_shutdown_cleanly() {
        let (host, _rx) = fresh_host(PathBuf::from("/nonexistent/never-opened.pdf"));
        // Drop sends Shutdown and joins. If the thread is stuck this test hangs.
        drop(host);
    }

    #[test]
    fn replace_path_then_shutdown() {
        let (host, _rx) = fresh_host(PathBuf::from("/nonexistent/first.pdf"));
        host.replace_path(PathBuf::from("/nonexistent/second.pdf"), 2);
        drop(host);
    }

    #[test]
    fn missing_document_yields_render_error() {
        let (host, rx) = fresh_host(PathBuf::from("/nonexistent/missing.pdf"));
        host.submit(RenderJob {
            page_index: 0,
            zoom_bucket: 4,
            epoch: 1,
        });
        // Receive on a blocking helper: this is a sync test, the host
        // thread completes its loop iteration and sends a Result.
        let result = rx
            .recv_blocking()
            .expect("expected a RenderResult on missing-doc path");
        match result.outcome {
            RenderOutcome::Err(msg) => {
                assert!(
                    msg.contains("document not open"),
                    "unexpected err message: {msg}"
                );
            }
            RenderOutcome::Ok { .. } => panic!("expected Err for missing document"),
        }
        drop(host);
    }

    #[test]
    fn stale_epoch_render_is_dropped() {
        let (tx, rx) = async_channel::unbounded::<RenderResult>();
        // host starts at epoch 5; job at epoch 3 must be dropped silently.
        let host = PopplerHost::spawn(PathBuf::from("/nonexistent/x.pdf"), 5, tx)
            .expect("spawn host");
        host.submit(RenderJob {
            page_index: 0,
            zoom_bucket: 4,
            epoch: 3,
        });
        // Bound the wait — if a result arrives within 50 ms we fail.
        let waited = std::thread::spawn(move || rx.recv_blocking());
        std::thread::sleep(std::time::Duration::from_millis(50));
        // The reader thread is still blocked; the host dropped the job.
        // We drop `host` which causes Shutdown → the channel closes →
        // recv_blocking on the reader returns Err.
        drop(host);
        let result = waited.join().expect("reader thread");
        assert!(
            result.is_err(),
            "expected channel close, got a RenderResult — stale-epoch was not dropped"
        );
    }

    fn sample_pdf_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/multi-page.pdf")
    }

    #[test]
    fn idle_drop_releases_document_after_cooldown() {
        let pdf = sample_pdf_path();
        let (result_tx, _result_rx) = async_channel::unbounded::<RenderResult>();
        let host = PopplerHost::spawn_with_cooldown(
            pdf,
            1,
            result_tx,
            Duration::from_millis(150),
        )
        .expect("spawn host");

        // FindStart with a real query forces ensure_document_open.
        // Use an unbounded reply channel so find_scan's send_blocking
        // never blocks the host thread.
        let (find_tx, _find_rx) = async_channel::unbounded::<PageSearchResult>();
        host.find_start(1, "x".to_string(), 1, find_tx);

        // Poll up to 1 s for the document to be open. find_scan runs on
        // the host thread; once it returns, last_activity is bumped and
        // the document slot is Some.
        let mut opened = false;
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            if host.probe_has_document() == Some(true) {
                opened = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(opened, "document was never opened by FindStart");

        // Sleep past the cooldown + at least one poll interval, then
        // probe. The host should have observed the idle gap and dropped.
        std::thread::sleep(Duration::from_millis(150 + 300));
        assert_eq!(
            host.probe_has_document(),
            Some(false),
            "document was not dropped after idle cooldown"
        );

        drop(host);
    }

    #[test]
    fn idle_drop_reopens_on_next_command() {
        let pdf = sample_pdf_path();
        let (result_tx, _result_rx) = async_channel::unbounded::<RenderResult>();
        // 250 ms cooldown gives the test thread comfortable margin to
        // observe the open state at least once via the 20 ms polling
        // cadence before the host fires the drop. Lower values race
        // the poll loop and produce flakes.
        let host = PopplerHost::spawn_with_cooldown(
            pdf,
            1,
            result_tx,
            Duration::from_millis(250),
        )
        .expect("spawn host");

        let (find_tx, _find_rx) = async_channel::unbounded::<PageSearchResult>();
        host.find_start(1, "x".to_string(), 1, find_tx);

        // Synchronously wait for the host to report document=Some by
        // polling probe before sleeping past the cooldown. Doing the
        // open-confirmation as a hard precondition (rather than a race
        // against drop) eliminates the timing window.
        let mut ever_open = false;
        let open_deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < open_deadline {
            if host.probe_has_document() == Some(true) {
                ever_open = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(ever_open, "document never opened after FindStart");

        // Now wait past cooldown + one poll interval and confirm drop.
        std::thread::sleep(Duration::from_millis(250 + 300));
        assert_eq!(
            host.probe_has_document(),
            Some(false),
            "document was not dropped after idle cooldown"
        );

        // Send another FindStart. The host should lazily reopen.
        let (find_tx2, _find_rx2) = async_channel::unbounded::<PageSearchResult>();
        host.find_start(2, "x".to_string(), 1, find_tx2);

        let reopen_deadline = Instant::now() + Duration::from_secs(1);
        let mut reopened = false;
        while Instant::now() < reopen_deadline {
            if host.probe_has_document() == Some(true) {
                reopened = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(reopened, "document did not reopen after idle drop");

        drop(host);
    }
}
