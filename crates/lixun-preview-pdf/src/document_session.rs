//! Document session: orchestrates the main-thread Poppler `Document`,
//! the two render workers, the texture cache, and the epoch counter
//! that lets stale render results be dropped on path change.
//!
//! See plan §2.10 Q2: `poppler::Document` is `!Send`, so each owner
//! (main thread, worker A, worker B) opens the file independently
//! and they exchange only plain data + `gdk::MemoryTexture`s
//! (which are `Send`-shareable as opaque GObjects via refcount).

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use lru::LruCache;
use poppler::Document;

use crate::worker::{RenderJob, RenderResult, RenderWorker};

/// Texture cache cap (~64 MB total, 32-bit ARGB). `lru` evicts by
/// access order; we manually account bytes and pop entries until we
/// fit. Keys are `(page_index, zoom_bucket_q4)` where the zoom
/// bucket is encoded as `(zoom * 4.0).round() as u32` so it remains
/// hashable.
pub const CACHE_BUDGET_BYTES: usize = 64 * 1024 * 1024;

/// Hard cap on render bucket. Above zoom=4.0 (bucket=16) a letter-
/// sized page exceeds Cairo's `ImageSurface` allocation budget
/// (~220 MB ARGB32 at zoom=8, ~1 GB at zoom=16) and `ImageSurface::
/// create` returns Err. Capping here keeps memory bounded; the
/// canvas stretches the bucket-16 texture to the requested display
/// zoom via `snapshot.append_texture` — the picture gets visibly
/// soft past zoom=4 but never disappears.
pub const MAX_RENDER_BUCKET: u32 = 16;

/// Project a requested bucket onto the renderable range. Used at
/// every session-boundary call (submit, lookup) so the pending set
/// and texture cache use one canonical key per requested bucket.
pub fn effective_bucket(req: u32) -> u32 {
    req.min(MAX_RENDER_BUCKET)
}

pub const PAGE_GAP_PT: f64 = 12.0;
pub const POINTS_PER_INCH: f64 = 72.0;
pub const BASE_DPI: f64 = 96.0;

/// Round a continuous zoom factor to the nearest 25 % step. Zoom is
/// clamped to `[0.25, 16.0]` upstream; the bucket key encodes the
/// step as quarter-units so `(0.25 → 1, 1.0 → 4, 16.0 → 64)`.
///
/// Pulled out of the canvas so we can unit-test the rounding rule
/// without spinning up GTK.
pub fn zoom_bucket_q4(zoom: f64) -> u32 {
    let clamped = zoom.clamp(0.25, 16.0);
    (clamped * 4.0).round() as u32
}

/// Inverse of [`zoom_bucket_q4`] — the actual scale we render at.
#[allow(dead_code)]
pub fn bucket_to_zoom(bucket: u32) -> f64 {
    f64::from(bucket) / 4.0
}

#[derive(Clone)]
pub struct CachedTexture {
    pub texture: gdk::MemoryTexture,
    pub width: u32,
    pub height: u32,
    pub bytes: usize,
}

/// Page geometry computed on the main thread from the Poppler
/// `Document`. Workers do not need this — they re-derive size from
/// their own `Document` opened against the same file.
#[derive(Clone, Copy, Debug)]
pub struct PageSize {
    pub width_pt: f64,
    pub height_pt: f64,
}

pub struct DocumentSession {
    /// Current path. Changes whenever `replace_path` is called.
    path: RefCell<PathBuf>,
    /// Main-thread `Document`. Used for layout and page count.
    /// `RefCell` because `replace_path` swaps it out.
    document: RefCell<Option<Document>>,
    /// Per-page sizes in PostScript points, indexed by page number.
    page_sizes: RefCell<Vec<PageSize>>,
    /// Monotonic epoch. Bumped by `replace_path`. Workers stamp
    /// every render result; the main-thread receiver drops results
    /// whose epoch is older than `current_epoch`.
    epoch: Arc<AtomicU64>,
    /// Texture cache keyed by `(page_index, bucket_q4)`.
    cache: RefCell<LruCache<(u32, u32), CachedTexture>>,
    /// Bytes currently held in `cache`.
    cache_bytes: RefCell<usize>,
    /// In-flight render jobs, keyed by `(page_index, bucket_q4)`.
    /// `submit_visible` is a no-op when the key is already present;
    /// `clear_pending` is called by the canvas when a result lands.
    /// Cleared on `replace_path` together with the texture cache.
    pending: RefCell<HashSet<(u32, u32)>>,
    /// Two long-lived workers. Worker 0 = visible page,
    /// worker 1 = adjacent prefetch. Each owns its own `Document`.
    workers: [RenderWorker; 2],
}

impl DocumentSession {
    /// Open a new session for `path`. Returns `Err` if the main-
    /// thread `Document` cannot be opened — caller decides whether
    /// to render an "encrypted PDF" placeholder or a generic error.
    pub fn open(
        path: PathBuf,
        result_tx: async_channel::Sender<RenderResult>,
    ) -> anyhow::Result<Rc<Self>> {
        let uri = path_to_file_uri(&path)?;
        let document = Document::from_file(&uri, None)
            .map_err(|e| anyhow::anyhow!("poppler: open {:?}: {:?}", path, e))?;

        let n = document.n_pages();
        let mut sizes = Vec::with_capacity(n.max(0) as usize);
        for i in 0..n {
            if let Some(page) = document.page(i) {
                let (w, h) = page.size();
                sizes.push(PageSize {
                    width_pt: w,
                    height_pt: h,
                });
            } else {
                sizes.push(PageSize {
                    width_pt: 595.0,
                    height_pt: 842.0,
                });
            }
        }

        let epoch = Arc::new(AtomicU64::new(1));
        let workers = [
            RenderWorker::spawn("pdf-worker-visible", path.clone(), result_tx.clone())?,
            RenderWorker::spawn("pdf-worker-prefetch", path.clone(), result_tx)?,
        ];

        let cap = std::num::NonZeroUsize::new(512).expect("non-zero cache cap");
        Ok(Rc::new(Self {
            path: RefCell::new(path),
            document: RefCell::new(Some(document)),
            page_sizes: RefCell::new(sizes),
            epoch,
            cache: RefCell::new(LruCache::new(cap)),
            cache_bytes: RefCell::new(0),
            pending: RefCell::new(HashSet::new()),
            workers,
        }))
    }

    /// Bump epoch, reopen main-thread document, message both
    /// workers to reopen, clear cache. Called by `update()`.
    pub fn replace_path(&self, new_path: PathBuf) -> anyhow::Result<()> {
        let old_epoch = self.epoch.load(Ordering::SeqCst);
        let uri = path_to_file_uri(&new_path)?;
        let document = Document::from_file(&uri, None)
            .map_err(|e| anyhow::anyhow!("poppler: reopen {:?}: {:?}", new_path, e))?;

        let n = document.n_pages();
        let mut sizes = Vec::with_capacity(n.max(0) as usize);
        for i in 0..n {
            if let Some(page) = document.page(i) {
                let (w, h) = page.size();
                sizes.push(PageSize {
                    width_pt: w,
                    height_pt: h,
                });
            } else {
                sizes.push(PageSize {
                    width_pt: 595.0,
                    height_pt: 842.0,
                });
            }
        }

        // Bump epoch BEFORE swapping document so any in-flight
        // render result already has a stale stamp by the time the
        // main thread sees it.
        self.epoch.fetch_add(1, Ordering::SeqCst);

        *self.document.borrow_mut() = Some(document);
        *self.page_sizes.borrow_mut() = sizes;
        *self.path.borrow_mut() = new_path.clone();
        self.cache.borrow_mut().clear();
        *self.cache_bytes.borrow_mut() = 0;
        self.pending.borrow_mut().clear();

        for w in &self.workers {
            w.replace_path(new_path.clone());
        }

        let new_epoch = self.epoch.load(Ordering::SeqCst);
        tracing::info!(
            "session replace_path: old_epoch={} → new_epoch={} n_pages={} cache+pending cleared",
            old_epoch,
            new_epoch,
            self.page_sizes.borrow().len(),
        );
        Ok(())
    }

    pub fn path(&self) -> PathBuf {
        self.path.borrow().clone()
    }

    pub fn n_pages(&self) -> u32 {
        self.page_sizes.borrow().len() as u32
    }

    pub fn page_size(&self, index: u32) -> Option<PageSize> {
        self.page_sizes.borrow().get(index as usize).copied()
    }

    pub fn current_epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    pub fn epoch_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.epoch)
    }

    pub fn get_cached(&self, page: u32, bucket: u32) -> Option<CachedTexture> {
        let bucket = effective_bucket(bucket);
        self.cache.borrow_mut().get(&(page, bucket)).cloned()
    }

    /// Best-effort cache lookup: returns the exact bucket if cached,
    /// otherwise the cached entry for the same page whose bucket is
    /// closest to `target_bucket`. The caller stretches the texture
    /// to fit the target rect via `snapshot.append_texture`. Avoids
    /// the "everything goes grey on zoom change" flicker.
    pub fn get_best_cached(&self, page: u32, target_bucket: u32) -> Option<CachedTexture> {
        let target_bucket = effective_bucket(target_bucket);
        if let Some(exact) = self.cache.borrow_mut().get(&(page, target_bucket)).cloned() {
            return Some(exact);
        }
        let cache = self.cache.borrow();
        let mut best: Option<(u32, CachedTexture)> = None;
        for (&(p, b), entry) in cache.iter() {
            if p != page {
                continue;
            }
            let dist = b.abs_diff(target_bucket);
            match &best {
                Some((bd, _)) if *bd <= dist => {}
                _ => best = Some((dist, entry.clone())),
            }
        }
        best.map(|(_, e)| e)
    }

    pub fn insert_cached(&self, page: u32, bucket: u32, entry: CachedTexture) {
        let mut cache = self.cache.borrow_mut();
        let mut bytes = self.cache_bytes.borrow_mut();
        if let Some(old) = cache.put((page, bucket), entry.clone()) {
            *bytes = bytes.saturating_sub(old.bytes);
        }
        *bytes += entry.bytes;
        while *bytes > CACHE_BUDGET_BYTES {
            match cache.pop_lru() {
                Some((_, evicted)) => {
                    *bytes = bytes.saturating_sub(evicted.bytes);
                }
                None => break,
            }
        }
    }

    /// Submit a render job to worker A (visible). Worker drops
    /// jobs whose epoch < its `current_epoch`. Per-key dedup: a
    /// duplicate `(page, bucket)` while one is in flight is a
    /// no-op, preventing the "snapshot resubmits while the result
    /// is still rendering" feedback loop.
    pub fn submit_visible(&self, job: RenderJob) {
        let mut job = job;
        job.zoom_bucket = effective_bucket(job.zoom_bucket);
        let key = (job.page_index, job.zoom_bucket);
        if !self.pending.borrow_mut().insert(key) {
            return;
        }
        self.workers[0].submit(job);
    }

    /// Submit a render job to worker B (adjacent prefetch). Same
    /// dedup contract as `submit_visible`.
    pub fn submit_prefetch(&self, job: RenderJob) {
        let mut job = job;
        job.zoom_bucket = effective_bucket(job.zoom_bucket);
        let key = (job.page_index, job.zoom_bucket);
        if !self.pending.borrow_mut().insert(key) {
            return;
        }
        self.workers[1].submit(job);
    }

    /// Mark a `(page, bucket)` slot free so a later cache miss can
    /// resubmit. Called by the canvas's render-result handler.
    pub fn clear_pending(&self, page: u32, bucket: u32) {
        let bucket = effective_bucket(bucket);
        self.pending.borrow_mut().remove(&(page, bucket));
    }
}

/// Encode a filesystem path as a `file://` URI suitable for
/// `poppler::Document::from_file`. Percent-encodes anything
/// outside the unreserved set.
pub fn path_to_file_uri(path: &Path) -> anyhow::Result<String> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let canon = abs.canonicalize().unwrap_or(abs);
    let s = canon.to_string_lossy();
    let mut uri = String::from("file://");
    for b in s.as_bytes() {
        let c = *b;
        let safe = c.is_ascii_alphanumeric()
            || c == b'/'
            || c == b'-'
            || c == b'_'
            || c == b'.'
            || c == b'~';
        if safe {
            uri.push(c as char);
        } else {
            uri.push_str(&format!("%{:02X}", c));
        }
    }
    Ok(uri)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_bucket_rounds_to_quarter_steps() {
        assert_eq!(zoom_bucket_q4(1.0), 4);
        assert_eq!(zoom_bucket_q4(1.10), 4); // round down to 1.00
        assert_eq!(zoom_bucket_q4(1.13), 5); // 4.52 rounds to 5 → 1.25
        assert_eq!(zoom_bucket_q4(1.25), 5);
        assert_eq!(zoom_bucket_q4(0.25), 1);
        assert_eq!(zoom_bucket_q4(0.10), 1); // clamped to 0.25
        assert_eq!(zoom_bucket_q4(20.0), 64); // clamped to 16.0
    }

    #[test]
    fn bucket_round_trips_to_zoom() {
        for q in [1u32, 4, 5, 8, 16, 64] {
            let zoom = bucket_to_zoom(q);
            assert_eq!(zoom_bucket_q4(zoom), q);
        }
    }

    #[test]
    fn render_bucket_caps_at_max() {
        assert_eq!(effective_bucket(64), MAX_RENDER_BUCKET);
        assert_eq!(effective_bucket(MAX_RENDER_BUCKET + 1), MAX_RENDER_BUCKET);
        assert_eq!(effective_bucket(MAX_RENDER_BUCKET), MAX_RENDER_BUCKET);
        assert_eq!(effective_bucket(10), 10);
        assert_eq!(effective_bucket(1), 1);
        assert_eq!(effective_bucket(0), 0);
    }

    #[test]
    fn path_to_file_uri_escapes_spaces() {
        let uri = path_to_file_uri(Path::new("/tmp/hello world.pdf")).unwrap();
        assert!(uri.contains("hello%20world.pdf"));
        assert!(uri.starts_with("file://"));
    }

    #[test]
    fn path_to_file_uri_keeps_separators() {
        let uri = path_to_file_uri(Path::new("/a/b/c.pdf")).unwrap();
        assert!(!uri.contains("%2F"));
        assert!(uri.ends_with("/c.pdf"));
    }

    fn make_texture(w: i32, h: i32) -> CachedTexture {
        let stride = w * 4;
        let buf = vec![0u8; (stride * h) as usize];
        let bytes = glib::Bytes::from_owned(buf);
        let texture = gdk::MemoryTexture::new(
            w,
            h,
            gdk::MemoryFormat::B8g8r8a8Premultiplied,
            &bytes,
            stride as usize,
        );
        CachedTexture {
            texture,
            width: w as u32,
            height: h as u32,
            bytes: (stride * h) as usize,
        }
    }

    fn fresh_session() -> Rc<DocumentSession> {
        let (tx, _rx) = async_channel::unbounded::<RenderResult>();
        let cap = std::num::NonZeroUsize::new(64).unwrap();
        Rc::new(DocumentSession {
            path: RefCell::new(PathBuf::new()),
            document: RefCell::new(None),
            page_sizes: RefCell::new(vec![PageSize {
                width_pt: 595.0,
                height_pt: 842.0,
            }]),
            epoch: Arc::new(AtomicU64::new(1)),
            cache: RefCell::new(LruCache::new(cap)),
            cache_bytes: RefCell::new(0),
            pending: RefCell::new(HashSet::new()),
            workers: [
                RenderWorker::spawn("test-w0", PathBuf::new(), tx.clone()).unwrap(),
                RenderWorker::spawn("test-w1", PathBuf::new(), tx).unwrap(),
            ],
        })
    }

    #[test]
    fn pending_dedup_blocks_resubmit() {
        gtk::init().ok();
        let s = fresh_session();
        let job = RenderJob {
            page_index: 0,
            zoom_bucket: 4,
            epoch: 1,
        };
        assert!(s.pending.borrow().is_empty());
        s.submit_visible(job);
        assert_eq!(s.pending.borrow().len(), 1);
        s.submit_visible(job);
        assert_eq!(
            s.pending.borrow().len(),
            1,
            "second submit must not insert a duplicate"
        );
        s.clear_pending(0, 4);
        assert!(s.pending.borrow().is_empty());
        s.submit_visible(job);
        assert_eq!(
            s.pending.borrow().len(),
            1,
            "after clear_pending, the same key can be submitted again"
        );
    }

    #[test]
    fn get_best_cached_falls_back_to_nearest_bucket() {
        gtk::init().ok();
        let s = fresh_session();
        s.insert_cached(0, 4, make_texture(2, 2));
        assert!(s.get_best_cached(0, 4).is_some());
        let fb = s
            .get_best_cached(0, 8)
            .expect("expected fallback to bucket=4");
        assert_eq!(fb.width, 2);
        assert_eq!(fb.height, 2);
        assert!(
            s.get_best_cached(1, 4).is_none(),
            "fallback must not cross page boundaries"
        );
    }
}
