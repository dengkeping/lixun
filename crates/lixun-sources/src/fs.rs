//! Filesystem source — walk directories, extract text, produce Documents.
//!
//! Two-pass indexing:
//! 1. **Metadata pass** (sequential): collects path, filename, mtime, size.
//! 2. **Content pass** (rayon pool): extracts body text in parallel.

use anyhow::Result;
use lixun_core::{Action, Category, DocId, Document};
use lixun_extract::ExtractorCapabilities;
use rayon::prelude::*;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::has_body::HasBody;
use crate::manifest::Manifest;
use crate::mime_icons;
use crate::ocr_enqueue::OcrEnqueue;

/// Filesystem source configuration.
pub struct FsSource {
    pub roots: Vec<PathBuf>,
    pub exclude: Vec<String>,
    pub exclude_regex: Vec<regex::Regex>,
    pub max_file_size_mb: u64,
    pub caps: Arc<ExtractorCapabilities>,
    pub ocr_enqueue: Option<Arc<dyn OcrEnqueue>>,
    pub body_checker: Option<Arc<dyn HasBody>>,
    /// Enqueue-side OCR dimension filter threshold (pixels). `0` keeps
    /// the v1.1 behaviour where every OCR-candidate image reaches the
    /// queue; any positive value skips images whose header decodes and
    /// whose both axes fall below the threshold. See
    /// `maybe_enqueue_ocr` for full semantics (PDFs are always
    /// enqueued; decode errors fail open).
    pub min_image_side_px: u32,
}

/// Intermediate metadata collected during the first pass.
struct FileMeta {
    path: PathBuf,
    path_str: String,
    filename: String,
    mtime: i64,
    size: u64,
    is_dir: bool,
}

impl FsSource {
    pub fn new(roots: Vec<PathBuf>, exclude: Vec<String>, max_file_size_mb: u64) -> Self {
        Self {
            roots,
            exclude,
            exclude_regex: Vec::new(),
            max_file_size_mb,
            caps: Arc::new(ExtractorCapabilities::all_available_no_timeout()),
            ocr_enqueue: None,
            body_checker: None,
            min_image_side_px: 0,
        }
    }

    pub fn with_regex(
        roots: Vec<PathBuf>,
        exclude: Vec<String>,
        exclude_regex: Vec<regex::Regex>,
        max_file_size_mb: u64,
    ) -> Self {
        Self {
            roots,
            exclude,
            exclude_regex,
            max_file_size_mb,
            caps: Arc::new(ExtractorCapabilities::all_available_no_timeout()),
            ocr_enqueue: None,
            body_checker: None,
            min_image_side_px: 0,
        }
    }

    pub fn new_with_ocr(
        roots: Vec<PathBuf>,
        exclude: Vec<String>,
        max_file_size_mb: u64,
        caps: Arc<ExtractorCapabilities>,
        ocr_enqueue: Option<Arc<dyn OcrEnqueue>>,
    ) -> Self {
        Self {
            roots,
            exclude,
            exclude_regex: Vec::new(),
            max_file_size_mb,
            caps,
            ocr_enqueue,
            body_checker: None,
            min_image_side_px: 0,
        }
    }

    pub fn with_regex_and_ocr(
        roots: Vec<PathBuf>,
        exclude: Vec<String>,
        exclude_regex: Vec<regex::Regex>,
        max_file_size_mb: u64,
        caps: Arc<ExtractorCapabilities>,
        ocr_enqueue: Option<Arc<dyn OcrEnqueue>>,
    ) -> Self {
        Self {
            roots,
            exclude,
            exclude_regex,
            max_file_size_mb,
            caps,
            ocr_enqueue,
            body_checker: None,
            min_image_side_px: 0,
        }
    }

    /// Attach a `HasBody` checker so the OCR enqueue path can
    /// short-circuit re-enqueue of docs whose body was already
    /// recovered in a prior pass (DB-16). Chainable off any
    /// constructor; `None` preserves T7 behaviour (always enqueue).
    pub fn with_body_checker(mut self, checker: Option<Arc<dyn HasBody>>) -> Self {
        self.body_checker = checker;
        self
    }

    /// Set the enqueue-side OCR dimension filter threshold. Chainable
    /// off any constructor; `0` keeps the v1.1 behaviour (no filter).
    /// The worker-side dim check in `ocr_image_with` stays as
    /// defence-in-depth regardless of this setting.
    pub fn with_min_image_side_px(mut self, min_side_px: u32) -> Self {
        self.min_image_side_px = min_side_px;
        self
    }

    fn is_excluded(&self, path: &Path) -> bool {
        crate::exclude::path_excluded(path, &self.exclude, &self.exclude_regex)
    }

    /// Extract content from a file, going through the T1 bytes cache so
    /// unchanged files skip the extractor subprocess on the second and
    /// later runs. On an empty result for an OCR-candidate extension,
    /// emits an enqueue request on `enqueue` (if supplied) per DB-13,
    /// but skips the enqueue entirely when `body_checker` reports the
    /// doc already has an indexed body (DB-16 short-circuit).
    ///
    /// `min_image_side_px` applies the enqueue-side dimension filter:
    /// images whose header decodes and whose both axes fall below this
    /// threshold are skipped without hitting the OCR queue. `0` disables
    /// the filter (v1.1 behaviour). See `maybe_enqueue_ocr` for details.
    ///
    /// Enqueue failures log-and-swallow: a queue write error must never
    /// fail the extraction path.
    pub fn extract_content(
        path: &Path,
        caps: &ExtractorCapabilities,
        enqueue: Option<&dyn OcrEnqueue>,
        body_checker: Option<&dyn HasBody>,
        min_image_side_px: u32,
    ) -> Result<Option<String>> {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            lixun_extract::cache::cached_extract_path(path, caps)
        }))
        .map_err(|_| anyhow::anyhow!("extractor panicked"))??;

        if result.is_none() {
            maybe_enqueue_ocr(path, caps, enqueue, body_checker, min_image_side_px);
        }
        Ok(result)
    }

    /// Build a rayon thread pool sized to min(num_cpus, 4).
    fn build_pool() -> rayon::ThreadPool {
        let num_threads = std::cmp::min(num_cpus::get(), 4);
        rayon::ThreadPoolBuilder::new()
            .num_threads(num_threads)
            .build()
            .expect("failed to build rayon pool")
    }

    pub fn metadata_for_path(path: &Path) -> (String, String) {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        (
            mime_icons::mime_to_icon_name(&mime),
            mime_icons::human_kind(&mime),
        )
    }

    fn build_document(meta: FileMeta, body: Option<String>, extract_fail: bool) -> Document {
        // Directories: fixed folder icon + "Folder" kind, no body/extract.
        // Files: mime-based icon + kind via mime_icons.
        let (icon_name, kind_label) = if meta.is_dir {
            ("folder".to_string(), "Folder".to_string())
        } else {
            Self::metadata_for_path(&meta.path)
        };

        Document {
            id: DocId(format!("fs:{}", meta.path_str)),
            category: Category::File,
            title: meta.filename,
            subtitle: meta.path_str.clone(),
            icon_name: Some(icon_name),
            kind_label: Some(kind_label),
            body,
            path: meta.path_str,
            mtime: meta.mtime,
            size: meta.size,
            // xdg-open and gio::AppInfo::launch_default_for_uri both
            // route directories to the user's default file manager
            // (nautilus/dolphin/nemo/...), so the same OpenFile
            // variant works for both files and dirs without needing
            // a separate OpenFolder action.
            action: Action::OpenFile {
                path: meta.path.clone(),
            },
            secondary_action: Some(Action::ShowInFileManager { path: meta.path }),
            sender: None,
            recipients: None,
            extract_fail,
            source_instance: "builtin:fs".into(),
            extra: Vec::new(),
        }
    }

    const BATCH_SIZE: usize = 2000;

    pub fn index_incremental(
        &self,
        manifest: &mut Manifest,
        indexed_ids: &HashSet<String>,
    ) -> Result<(Vec<Document>, Vec<String>)> {
        let mut collected: Vec<Document> = Vec::new();
        let deleted = self.index_incremental_batched(manifest, indexed_ids, |batch| {
            collected.extend(batch);
            Ok(())
        })?;
        Ok((collected, deleted))
    }

    pub fn index_incremental_batched<F>(
        &self,
        manifest: &mut Manifest,
        indexed_ids: &HashSet<String>,
        mut on_batch: F,
    ) -> Result<Vec<String>>
    where
        F: FnMut(Vec<Document>) -> Result<()>,
    {
        let max_size = self.max_file_size_mb * 1024 * 1024;

        let mut current_files: HashSet<String> = HashSet::new();
        let mut changed_metas: Vec<FileMeta> = Vec::new();
        let mut resurrected: u64 = 0;

        for root in &self.roots {
            for entry in walkdir::WalkDir::new(root)
                .into_iter()
                .filter_entry(|e| !self.is_excluded(e.path()))
                .filter_map(|e| e.ok())
            {
                let ft = entry.file_type();
                if !ft.is_file() && !ft.is_dir() {
                    continue;
                }
                // Skip the walk root itself — a user searching for "/"
                // or the root dir adds no value, only clutter.
                if entry.depth() == 0 {
                    continue;
                }

                let path = entry.path();
                let Ok(metadata) = std::fs::metadata(path) else {
                    continue;
                };

                let mtime = metadata
                    .modified()
                    .map(|t| {
                        t.duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0)
                    })
                    .unwrap_or(0);

                let path_str = path.to_string_lossy().to_string();
                current_files.insert(path_str.clone());

                let doc_id = format!("fs:{}", path_str);
                let in_index = indexed_ids.contains(&doc_id);
                if manifest.is_unchanged(&path_str, mtime) && in_index {
                    continue;
                }
                if !in_index && manifest.is_unchanged(&path_str, mtime) {
                    resurrected += 1;
                }

                changed_metas.push(FileMeta {
                    path: path.to_path_buf(),
                    path_str,
                    filename: path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default(),
                    mtime,
                    size: metadata.len(),
                    is_dir: ft.is_dir(),
                });
            }
        }

        if resurrected > 0 {
            tracing::warn!(
                "Filesystem: {} files were in manifest but missing from index, re-indexing",
                resurrected
            );
        }

        let deleted_ids: Vec<String> = manifest
            .known_paths()
            .filter(|p| !current_files.contains(*p))
            .map(|p| format!("fs:{}", p))
            .collect();

        for path in &deleted_ids {
            manifest.remove(path.strip_prefix("fs:").unwrap());
        }

        if changed_metas.is_empty() {
            if !deleted_ids.is_empty() {
                tracing::info!(
                    "Filesystem: {} deleted, nothing to extract",
                    deleted_ids.len()
                );
            } else {
                tracing::info!("Filesystem: no changes detected");
            }
            return Ok(deleted_ids);
        }

        tracing::info!(
            "Filesystem: {} changed/new files, {} deleted, extracting in parallel (batch={})...",
            changed_metas.len(),
            deleted_ids.len(),
            Self::BATCH_SIZE,
        );

        let pool = Self::build_pool();
        let total = changed_metas.len();
        let mut extract_fails: u64 = 0;
        let mut processed: usize = 0;
        let caps = Arc::clone(&self.caps);
        let enqueue = self.ocr_enqueue.clone();
        let body_checker = self.body_checker.clone();
        let min_image_side_px = self.min_image_side_px;
        while !changed_metas.is_empty() {
            let take_n = Self::BATCH_SIZE.min(changed_metas.len());
            let chunk: Vec<FileMeta> = changed_metas.drain(..take_n).collect();
            let caps_loop = Arc::clone(&caps);
            let enqueue_loop = enqueue.clone();
            let body_checker_loop = body_checker.clone();
            let docs: Vec<Document> = pool.install(|| {
                chunk
                    .into_par_iter()
                    .map(|meta| {
                        // Directories: skip content extraction (no body
                        // to extract, only name is indexed for matching).
                        let (body, extract_fail) = if meta.is_dir {
                            (None, false)
                        } else if meta.size <= max_size {
                            let enq_ref =
                                enqueue_loop.as_ref().map(|a| a.as_ref() as &dyn OcrEnqueue);
                            let body_ref = body_checker_loop
                                .as_ref()
                                .map(|a| a.as_ref() as &dyn HasBody);
                            match Self::extract_content(
                                &meta.path,
                                &caps_loop,
                                enq_ref,
                                body_ref,
                                min_image_side_px,
                            ) {
                                Ok(Some(text)) => (Some(text), false),
                                Ok(None) => (None, false),
                                Err(_) => (None, true),
                            }
                        } else {
                            (None, false)
                        };

                        Self::build_document(meta, body, extract_fail)
                    })
                    .collect()
            });

            for doc in &docs {
                manifest.update(doc.path.clone(), doc.mtime);
                if doc.extract_fail {
                    extract_fails += 1;
                }
            }
            processed += docs.len();
            on_batch(docs)?;
            tracing::info!(
                "Filesystem incremental: flushed batch, {}/{} files processed",
                processed,
                total,
            );
        }

        tracing::info!(
            "Filesystem incremental: {} docs indexed across batches, {} deleted ({} extract failures)",
            processed,
            deleted_ids.len(),
            extract_fails,
        );
        Ok(deleted_ids)
    }
}

impl FsSource {
    pub fn index_all(&self) -> Result<Vec<Document>> {
        let max_size = self.max_file_size_mb * 1024 * 1024;

        let mut metas: Vec<FileMeta> = Vec::new();

        for root in &self.roots {
            for entry in walkdir::WalkDir::new(root)
                .into_iter()
                .filter_entry(|e| !self.is_excluded(e.path()))
                .filter_map(|e| e.ok())
            {
                let ft = entry.file_type();
                if !ft.is_file() && !ft.is_dir() {
                    continue;
                }
                // Skip the walk root itself — indexing "/" or the
                // home dir as a hit adds no value, only clutter.
                if entry.depth() == 0 {
                    continue;
                }

                let path = entry.path();
                let Ok(metadata) = std::fs::metadata(path) else {
                    continue;
                };

                let mtime = metadata
                    .modified()
                    .map(|t| {
                        t.duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0)
                    })
                    .unwrap_or(0);

                metas.push(FileMeta {
                    path: path.to_path_buf(),
                    path_str: path.to_string_lossy().to_string(),
                    filename: path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default(),
                    mtime,
                    size: metadata.len(),
                    is_dir: ft.is_dir(),
                });
            }
        }

        tracing::info!(
            "Filesystem: {} files found, extracting content in parallel...",
            metas.len()
        );

        let pool = Self::build_pool();
        let caps = Arc::clone(&self.caps);
        let enqueue = self.ocr_enqueue.clone();
        let body_checker = self.body_checker.clone();
        let min_image_side_px = self.min_image_side_px;
        let docs: Vec<Document> = pool.install(|| {
            metas
                .into_par_iter()
                .map(|meta| {
                    let (body, extract_fail) = if meta.is_dir {
                        (None, false)
                    } else if meta.size <= max_size {
                        let enq_ref = enqueue.as_ref().map(|a| a.as_ref() as &dyn OcrEnqueue);
                        let body_ref =
                            body_checker.as_ref().map(|a| a.as_ref() as &dyn HasBody);
                        match Self::extract_content(
                            &meta.path,
                            &caps,
                            enq_ref,
                            body_ref,
                            min_image_side_px,
                        ) {
                            Ok(Some(text)) => (Some(text), false),
                            Ok(None) => (None, false),
                            Err(_) => (None, true),
                        }
                    } else {
                        (None, false)
                    };

                    Self::build_document(meta, body, extract_fail)
                })
                .collect()
        });

        let extract_fails = docs.iter().filter(|doc| doc.extract_fail).count();
        tracing::info!(
            "Filesystem: indexed {} documents ({} extract failures)",
            docs.len(),
            extract_fails
        );
        Ok(docs)
    }
}

impl crate::source::IndexerSource for FsSource {
    fn kind(&self) -> &'static str {
        "fs"
    }

    fn watch_paths(
        &self,
        _ctx: &crate::source::SourceContext,
    ) -> Result<Vec<crate::source::WatchSpec>> {
        Ok(self
            .roots
            .iter()
            .map(|p| crate::source::WatchSpec {
                path: p.clone(),
                recursive: true,
            })
            .collect())
    }

    fn reindex_full(
        &self,
        ctx: &crate::source::SourceContext,
        sink: &dyn crate::source::MutationSink,
    ) -> Result<()> {
        let manifest_path = ctx.state_dir.join("manifest.json");
        let _ = std::fs::remove_file(&manifest_path);

        let mut manifest = Manifest::default();
        let empty_ids: HashSet<String> = HashSet::new();

        let deleted = self.index_incremental_batched(&mut manifest, &empty_ids, |batch| {
            for mut doc in batch {
                doc.source_instance = ctx.instance_id.to_string();
                sink.emit(crate::source::Mutation::Upsert(Box::new(doc)))?;
            }
            Ok(())
        })?;

        for id in deleted {
            sink.emit(crate::source::Mutation::Delete { doc_id: id })?;
        }

        manifest.save(ctx.state_dir);
        Ok(())
    }
}

/// Files strictly smaller than this are skipped by the enqueue-side
/// dimension filter without opening them — a valid PNG/JPEG/etc. header
/// is at least ~30 bytes and any real image larger than `min_side_px`
/// on either axis is orders of magnitude above this threshold.
const MIN_IMAGE_BYTES_FOR_PROBE: u64 = 64;

fn maybe_enqueue_ocr(
    path: &Path,
    caps: &ExtractorCapabilities,
    enqueue: Option<&dyn OcrEnqueue>,
    body_checker: Option<&dyn HasBody>,
    min_image_side_px: u32,
) {
    let Some(enqueuer) = enqueue else { return };
    if !caps.has_tesseract || !caps.ocr_enabled {
        return;
    }
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if !lixun_extract::ocr::is_ocr_candidate(&ext) {
        return;
    }
    let doc_id = format!("fs:{}", path.to_string_lossy());
    // DB-16: skip enqueue when a prior OCR pass already populated the
    // body in Tantivy. Probe errors fall through to enqueue — the
    // OcrQueue's INSERT OR IGNORE still dedups at the DB layer, so
    // "re-enqueue on probe failure" is the safe default.
    if let Some(checker) = body_checker
        && checker.has_body(&doc_id).unwrap_or(false)
    {
        tracing::debug!("ocr enqueue: skipping {doc_id}, body already indexed");
        return;
    }
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("ocr enqueue: stat {} failed: {e}", path.display());
            return;
        }
    };
    // v1.2 Finding 3: drop icon-sized rasters before they reach the
    // OCR queue. PDFs never dimension-filter here — pdftoppm rasterises
    // pages at its own DPI so the file size on disk tells nothing about
    // the usable page bitmap. All failure modes fail open: the OCR
    // worker keeps its own dim check as defence-in-depth.
    if min_image_side_px > 0 && ext != "pdf" {
        if meta.len() < MIN_IMAGE_BYTES_FOR_PROBE {
            tracing::debug!(
                "ocr enqueue: skipping {doc_id}, file {} bytes below probe threshold",
                meta.len()
            );
            return;
        }
        if let Ok(bytes) = std::fs::read(path)
            && lixun_extract::ocr::image_too_small(&bytes, min_image_side_px)
        {
            tracing::debug!(
                "ocr enqueue: skipping {doc_id}, both axes below {min_image_side_px}px"
            );
            return;
        }
    }
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    if let Err(e) = enqueuer.enqueue(&doc_id, path, mtime, meta.len(), &ext) {
        tracing::warn!("ocr enqueue for {} failed: {e}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    static HOME_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn with_isolated_cache<F, R>(f: F) -> R
    where
        F: FnOnce() -> R,
    {
        let lock = HOME_LOCK.get_or_init(|| Mutex::new(()));
        let _g = lock.lock().unwrap();
        let td = tempfile::TempDir::new().unwrap();
        let old_xdg = std::env::var_os("XDG_CACHE_HOME");
        let old_home = std::env::var_os("HOME");
        // SAFETY: env is process-global; HOME_LOCK serializes every test
        // in this module that touches the cache and no other code in the
        // crate reads these vars during the test window.
        unsafe {
            std::env::set_var("XDG_CACHE_HOME", td.path());
            std::env::set_var("HOME", td.path());
        }
        let out = f();
        unsafe {
            match old_xdg {
                Some(v) => std::env::set_var("XDG_CACHE_HOME", v),
                None => std::env::remove_var("XDG_CACHE_HOME"),
            }
            match old_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
        drop(td);
        out
    }

    type EnqueueCall = (String, PathBuf, i64, u64, String);

    #[derive(Default)]
    struct MockEnqueue {
        calls: Mutex<Vec<EnqueueCall>>,
    }

    impl OcrEnqueue for MockEnqueue {
        fn enqueue(
            &self,
            doc_id: &str,
            path: &Path,
            mtime: i64,
            size: u64,
            ext: &str,
        ) -> Result<()> {
            self.calls.lock().unwrap().push((
                doc_id.to_string(),
                path.to_path_buf(),
                mtime,
                size,
                ext.to_string(),
            ));
            Ok(())
        }
    }

    #[test]
    fn extract_content_enqueues_on_empty_ocr_candidate() {
        with_isolated_cache(|| {
            let tmp = tempfile::tempdir().unwrap();
            let png = tmp.path().join("scan.png");
            // Non-empty bytes that utf8-sniff will treat as binary,
            // so cached_extract_path returns Ok(None).
            std::fs::write(&png, b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR").unwrap();

            let caps = Arc::new(ExtractorCapabilities::all_available_no_timeout());
            let mock: Arc<MockEnqueue> = Arc::new(MockEnqueue::default());
            let sink: Arc<dyn OcrEnqueue> = mock.clone();

            let result =
                FsSource::extract_content(&png, &caps, Some(sink.as_ref()), None, 0).unwrap();
            assert!(result.is_none(), "png has no text-layer body");

            let calls = mock.calls.lock().unwrap();
            assert_eq!(calls.len(), 1, "exactly one enqueue expected");
            let (doc_id, path, _mtime, size, ext) = &calls[0];
            assert_eq!(doc_id, &format!("fs:{}", png.to_string_lossy()));
            assert_eq!(path, &png);
            assert_eq!(ext, "png");
            assert!(*size > 0);
        });
    }

    #[test]
    fn extract_content_skips_enqueue_when_caps_off() {
        with_isolated_cache(|| {
            let tmp = tempfile::tempdir().unwrap();
            let png = tmp.path().join("scan.png");
            std::fs::write(&png, b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR").unwrap();

            let mut caps_plain = ExtractorCapabilities::all_available_no_timeout();
            caps_plain.ocr_enabled = false;
            let caps = Arc::new(caps_plain);
            let mock: Arc<MockEnqueue> = Arc::new(MockEnqueue::default());
            let sink: Arc<dyn OcrEnqueue> = mock.clone();

            let _ =
                FsSource::extract_content(&png, &caps, Some(sink.as_ref()), None, 0).unwrap();
            assert!(
                mock.calls.lock().unwrap().is_empty(),
                "no enqueue when ocr_enabled=false"
            );

            let mut caps_no_tess = ExtractorCapabilities::all_available_no_timeout();
            caps_no_tess.has_tesseract = false;
            let caps = Arc::new(caps_no_tess);
            let _ =
                FsSource::extract_content(&png, &caps, Some(sink.as_ref()), None, 0).unwrap();
            assert!(
                mock.calls.lock().unwrap().is_empty(),
                "no enqueue when has_tesseract=false"
            );
        });
    }

    #[test]
    fn extract_content_skips_enqueue_on_non_candidate_ext() {
        with_isolated_cache(|| {
            let tmp = tempfile::tempdir().unwrap();
            let bin = tmp.path().join("blob.docx");
            // Empty/invalid docx — zip parsing will fail, extractor
            // falls through returning empty. Not an OCR candidate ext.
            std::fs::write(&bin, b"not a real docx").unwrap();

            let caps = Arc::new(ExtractorCapabilities::all_available_no_timeout());
            let mock: Arc<MockEnqueue> = Arc::new(MockEnqueue::default());
            let sink: Arc<dyn OcrEnqueue> = mock.clone();

            let _ = FsSource::extract_content(&bin, &caps, Some(sink.as_ref()), None, 0);
            assert!(
                mock.calls.lock().unwrap().is_empty(),
                "non-candidate ext must not enqueue even on empty extraction"
            );
        });
    }

    #[derive(Default)]
    struct MockHasBody {
        has_body_for: std::collections::HashSet<String>,
    }

    impl HasBody for MockHasBody {
        fn has_body(&self, doc_id: &str) -> Result<bool> {
            Ok(self.has_body_for.contains(doc_id))
        }
    }

    #[test]
    fn extract_content_skips_enqueue_when_body_already_indexed() {
        with_isolated_cache(|| {
            let tmp = tempfile::tempdir().unwrap();
            let png = tmp.path().join("scan.png");
            std::fs::write(&png, b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR").unwrap();

            let caps = Arc::new(ExtractorCapabilities::all_available_no_timeout());
            let mock_enq: Arc<MockEnqueue> = Arc::new(MockEnqueue::default());
            let sink: Arc<dyn OcrEnqueue> = mock_enq.clone();

            let mut body = MockHasBody::default();
            body.has_body_for
                .insert(format!("fs:{}", png.to_string_lossy()));
            let body: Arc<dyn HasBody> = Arc::new(body);

            let _ = FsSource::extract_content(
                &png,
                &caps,
                Some(sink.as_ref()),
                Some(body.as_ref()),
                0,
            )
            .unwrap();
            assert!(
                mock_enq.calls.lock().unwrap().is_empty(),
                "body already indexed must short-circuit the enqueue",
            );

            let other_body: Arc<dyn HasBody> = Arc::new(MockHasBody::default());
            let _ = FsSource::extract_content(
                &png,
                &caps,
                Some(sink.as_ref()),
                Some(other_body.as_ref()),
                0,
            )
            .unwrap();
            assert_eq!(
                mock_enq.calls.lock().unwrap().len(),
                1,
                "checker returning false must let the enqueue proceed",
            );
        });
    }

    #[test]
    fn test_index_all_sets_pdf_icon_and_kind() {
        let tmp = tempfile::tempdir().unwrap();
        let pdf_path = tmp.path().join("report.pdf");
        std::fs::write(&pdf_path, b"%PDF-1.4\n%").unwrap();

        let source = FsSource::new(vec![tmp.path().to_path_buf()], Vec::new(), 1);
        let docs = source.index_all().unwrap();
        let doc = docs.iter().find(|doc| doc.title == "report.pdf").unwrap();

        assert_eq!(doc.icon_name.as_deref(), Some("application-pdf"));
        assert_eq!(doc.kind_label.as_deref(), Some("PDF Document"));
    }

    #[test]
    fn test_index_all_sets_fallback_icon_for_unknown_extension() {
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("mystery.unknownext");
        std::fs::write(&file_path, b"hello").unwrap();

        let source = FsSource::new(vec![tmp.path().to_path_buf()], Vec::new(), 1);
        let docs = source.index_all().unwrap();
        let doc = docs
            .iter()
            .find(|doc| doc.title == "mystery.unknownext")
            .unwrap();

        assert_eq!(doc.icon_name.as_deref(), Some("text-x-generic"));
        assert_eq!(doc.kind_label.as_deref(), Some("Octet Stream"));
    }

    fn write_png(path: &Path, w: u32, h: u32) {
        let img = image::RgbImage::new(w, h);
        img.save_with_format(path, image::ImageFormat::Png).unwrap();
    }

    #[test]
    fn maybe_enqueue_ocr_skips_tiny_png() {
        let tmp = tempfile::tempdir().unwrap();
        let png = tmp.path().join("icon.png");
        write_png(&png, 32, 32);

        let caps = ExtractorCapabilities::all_available_no_timeout();
        let mock: Arc<MockEnqueue> = Arc::new(MockEnqueue::default());
        let sink: Arc<dyn OcrEnqueue> = mock.clone();

        super::maybe_enqueue_ocr(&png, &caps, Some(sink.as_ref()), None, 256);
        assert!(
            mock.calls.lock().unwrap().is_empty(),
            "32x32 PNG must be filtered out at min=256"
        );
    }

    #[test]
    fn maybe_enqueue_ocr_enqueues_large_png() {
        let tmp = tempfile::tempdir().unwrap();
        let png = tmp.path().join("scan.png");
        write_png(&png, 512, 512);

        let caps = ExtractorCapabilities::all_available_no_timeout();
        let mock: Arc<MockEnqueue> = Arc::new(MockEnqueue::default());
        let sink: Arc<dyn OcrEnqueue> = mock.clone();

        super::maybe_enqueue_ocr(&png, &caps, Some(sink.as_ref()), None, 256);
        assert_eq!(
            mock.calls.lock().unwrap().len(),
            1,
            "512x512 PNG must enqueue at min=256"
        );
    }

    #[test]
    fn maybe_enqueue_ocr_enqueues_pdf_regardless() {
        let tmp = tempfile::tempdir().unwrap();
        let pdf = tmp.path().join("tiny.pdf");
        std::fs::write(&pdf, vec![b'%'; 512]).unwrap();

        let caps = ExtractorCapabilities::all_available_no_timeout();
        let mock: Arc<MockEnqueue> = Arc::new(MockEnqueue::default());
        let sink: Arc<dyn OcrEnqueue> = mock.clone();

        super::maybe_enqueue_ocr(&pdf, &caps, Some(sink.as_ref()), None, 2048);
        assert_eq!(
            mock.calls.lock().unwrap().len(),
            1,
            "PDFs must bypass the enqueue-side dimension pre-filter"
        );
    }

    #[test]
    fn maybe_enqueue_ocr_enqueues_on_unreadable_header() {
        let tmp = tempfile::tempdir().unwrap();
        let png = tmp.path().join("junk.png");
        std::fs::write(&png, vec![0xFFu8; 4096]).unwrap();

        let caps = ExtractorCapabilities::all_available_no_timeout();
        let mock: Arc<MockEnqueue> = Arc::new(MockEnqueue::default());
        let sink: Arc<dyn OcrEnqueue> = mock.clone();

        super::maybe_enqueue_ocr(&png, &caps, Some(sink.as_ref()), None, 256);
        assert_eq!(
            mock.calls.lock().unwrap().len(),
            1,
            "decode error must fail-open (worker will dim-check)",
        );
    }

    #[test]
    fn maybe_enqueue_ocr_with_min_zero_preserves_v11_behaviour() {
        let tmp = tempfile::tempdir().unwrap();
        let png = tmp.path().join("icon.png");
        write_png(&png, 32, 32);

        let caps = ExtractorCapabilities::all_available_no_timeout();
        let mock: Arc<MockEnqueue> = Arc::new(MockEnqueue::default());
        let sink: Arc<dyn OcrEnqueue> = mock.clone();

        super::maybe_enqueue_ocr(&png, &caps, Some(sink.as_ref()), None, 0);
        assert_eq!(
            mock.calls.lock().unwrap().len(),
            1,
            "min=0 must preserve v1.1 unconditional-enqueue behaviour",
        );
    }

    #[test]
    fn incremental_reindexes_when_manifest_says_unchanged_but_index_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path_a = tmp.path().join("a.txt");
        let path_b = tmp.path().join("b.txt");
        std::fs::write(&path_a, b"alpha").unwrap();
        std::fs::write(&path_b, b"beta").unwrap();

        let source = FsSource::new(vec![tmp.path().to_path_buf()], Vec::new(), 1);

        let mut manifest = Manifest::default();
        let all_ids: HashSet<String> = HashSet::new();
        let (docs, _deleted) = source.index_incremental(&mut manifest, &all_ids).unwrap();
        assert_eq!(docs.len(), 2, "first pass indexes both files");
        assert_eq!(manifest.len(), 2);

        let (docs_second, _) = source.index_incremental(&mut manifest, &all_ids).unwrap();
        assert_eq!(
            docs_second.len(),
            2,
            "index was empty so both files must be re-surfaced even with a populated manifest",
        );

        let indexed_ids: HashSet<String> = docs.iter().map(|d| d.id.0.clone()).collect();
        let (docs_third, _) = source
            .index_incremental(&mut manifest, &indexed_ids)
            .unwrap();
        assert_eq!(
            docs_third.len(),
            0,
            "once the index has the docs and nothing changed, nothing is re-indexed",
        );
    }
}
