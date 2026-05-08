//! Render worker thread.
//!
//! Each worker owns its own `poppler::Document` (Q2: `!Send`).
//! Jobs arrive over `std::sync::mpsc`; results go back over an
//! `async_channel::Sender` shared with the main thread, where a
//! `glib::MainContext::spawn_local` future drains them.
//!
//! Stale-result drop happens twice: the worker checks
//! `job.epoch >= self.current_epoch` before rendering, and the
//! main thread re-checks against `DocumentSession::current_epoch`
//! before inserting into the cache (the worker may have already
//! started rendering when `replace_path` fires).

use std::path::PathBuf;
use std::sync::mpsc;

use cairo::{Context as CairoCtx, Format as CairoFormat, ImageSurface};
use poppler::Document;

use crate::document_session::{MAX_RENDER_BUCKET, path_to_file_uri};

const POINTS_PER_INCH: f64 = 72.0;
const BASE_DPI: f64 = 96.0;

#[derive(Debug, Clone, Copy)]
pub struct RenderJob {
    pub page_index: u32,
    pub zoom_bucket: u32,
    pub epoch: u64,
}

pub struct RenderResult {
    pub page_index: u32,
    pub zoom_bucket: u32,
    pub epoch: u64,
    pub outcome: RenderOutcome,
}

pub enum RenderOutcome {
    Ok {
        texture: gdk::MemoryTexture,
        width: u32,
        height: u32,
        bytes: usize,
    },
    Err(String),
}

enum WorkerMsg {
    Render(RenderJob),
    ReplacePath(PathBuf),
}

pub struct RenderWorker {
    tx: mpsc::Sender<WorkerMsg>,
}

impl RenderWorker {
    pub fn spawn(
        name: &str,
        path: PathBuf,
        result_tx: async_channel::Sender<RenderResult>,
    ) -> anyhow::Result<Self> {
        let (tx, rx) = mpsc::channel::<WorkerMsg>();
        let thread_name = name.to_string();
        std::thread::Builder::new()
            .name(thread_name.clone())
            .spawn(move || worker_loop(thread_name, path, rx, result_tx))
            .map_err(|e| anyhow::anyhow!("spawn worker: {}", e))?;
        Ok(Self { tx })
    }

    pub fn submit(&self, job: RenderJob) {
        let _ = self.tx.send(WorkerMsg::Render(job));
    }

    pub fn replace_path(&self, path: PathBuf) {
        let _ = self.tx.send(WorkerMsg::ReplacePath(path));
    }
}

fn worker_loop(
    name: String,
    initial_path: PathBuf,
    rx: mpsc::Receiver<WorkerMsg>,
    result_tx: async_channel::Sender<RenderResult>,
) {
    let mut current_path = initial_path;
    let mut current_epoch: u64 = 1;
    let mut document: Option<Document> = open_document_for_worker(&current_path);

    while let Ok(msg) = rx.recv() {
        match msg {
            WorkerMsg::ReplacePath(new_path) => {
                current_path = new_path;
                current_epoch += 1;
                document = open_document_for_worker(&current_path);
                tracing::info!(
                    "worker {}: ReplacePath → epoch={} doc_open={}",
                    name,
                    current_epoch,
                    document.is_some()
                );
            }
            WorkerMsg::Render(job) => {
                tracing::info!(
                    "worker {}: recv Render page={} bucket={} job_epoch={} worker_epoch={}",
                    name,
                    job.page_index,
                    job.zoom_bucket,
                    job.epoch,
                    current_epoch
                );
                if job.epoch < current_epoch {
                    tracing::info!(
                        "worker {}: drop stale job page={} job_epoch={} < worker_epoch={}",
                        name,
                        job.page_index,
                        job.epoch,
                        current_epoch
                    );
                    continue;
                }
                let Some(doc) = document.as_ref() else {
                    let _ = result_tx.send_blocking(RenderResult {
                        page_index: job.page_index,
                        zoom_bucket: job.zoom_bucket,
                        epoch: job.epoch,
                        outcome: RenderOutcome::Err(format!("worker {}: document not open", name)),
                    });
                    continue;
                };
                let result = render_one(doc, job);
                let outcome_label = match &result {
                    RenderOutcome::Ok { .. } => "ok",
                    RenderOutcome::Err(e) => {
                        tracing::warn!("worker {} render err: {}", name, e);
                        "err"
                    }
                };
                tracing::info!(
                    "worker {}: emit Result page={} bucket={} epoch={} outcome={}",
                    name,
                    job.page_index,
                    job.zoom_bucket,
                    job.epoch,
                    outcome_label
                );
                let _ = result_tx.send_blocking(RenderResult {
                    page_index: job.page_index,
                    zoom_bucket: job.zoom_bucket,
                    epoch: job.epoch,
                    outcome: result,
                });
            }
        }
    }
    tracing::debug!("pdf worker '{}' exited", name);
}

fn open_document_for_worker(path: &std::path::Path) -> Option<Document> {
    let uri = match path_to_file_uri(path) {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!("worker uri for {:?}: {}", path, e);
            return None;
        }
    };
    match Document::from_file(&uri, None) {
        Ok(d) => Some(d),
        Err(e) => {
            tracing::warn!("worker open {:?} failed: {:?}", path, e);
            None
        }
    }
}

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

    let surface = match ImageSurface::create(CairoFormat::ARgb32, px_w, px_h) {
        Ok(s) => s,
        Err(e) => {
            return RenderOutcome::Err(format!("surface {}x{}: {}", px_w, px_h, e));
        }
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
    let data = match surface.take_data() {
        Ok(d) => d,
        Err(e) => return RenderOutcome::Err(format!("take_data: {}", e)),
    };
    let bytes_vec = data.to_vec();
    let bytes_len = bytes_vec.len();
    let glib_bytes = glib::Bytes::from_owned(bytes_vec);

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
