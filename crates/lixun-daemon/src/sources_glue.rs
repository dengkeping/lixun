//! Daemon-side wiring between the shared config schema
//! (`lixun-config`) and the indexing runtime (`lixun-indexer`,
//! `lixun-sources`, `lixun-extract`).
//!
//! `lixun-config` is deliberately a pure schema/loader crate so GUI
//! hosts (lixun-gui, lixun-preview-bin) can read `config.toml`
//! without pulling in the indexing stack. The runtime hooks that
//! used to live as `OnceLock` fields on `Config` — extractor
//! capabilities, the OCR enqueue sink, the indexed-body checker —
//! belong to the daemon process only, so they live here, alongside
//! the `IndexerSources` impl the indexer entry points consume.

use crate::config;
use anyhow::Result;
use std::sync::{Arc, OnceLock};

pub struct SourcesGlue {
    pub config: Arc<config::Config>,
    pub extractor_caps: OnceLock<Arc<lixun_extract::ExtractorCapabilities>>,
    pub ocr_enqueue: OnceLock<Arc<dyn lixun_sources::OcrEnqueue>>,
    pub body_checker: OnceLock<Arc<dyn lixun_sources::HasBody>>,
}

impl SourcesGlue {
    pub fn new(config: Arc<config::Config>) -> Self {
        Self {
            config,
            extractor_caps: OnceLock::new(),
            ocr_enqueue: OnceLock::new(),
            body_checker: OnceLock::new(),
        }
    }

    pub fn caps_arc(&self) -> Arc<lixun_extract::ExtractorCapabilities> {
        self.extractor_caps.get().cloned().unwrap_or_else(|| {
            Arc::new(lixun_extract::ExtractorCapabilities::all_available_no_timeout())
        })
    }

    pub fn build_fs_source(&self) -> Result<lixun_sources::fs::FsSource> {
        // Always exclude lixun's own state, data, cache and config
        // directories. Without this guard, the fs source watches
        // LanceDB's `_transactions/*.txn` and `_versions/*.manifest`
        // rotations under $XDG_DATA_HOME/lixun/semantic/vectors/, the
        // SQLite WAL/SHM under $XDG_STATE_HOME/lixun/, and the
        // extract/fastembed caches — and re-injects them into the
        // index as user files, which then floods the semantic worker
        // with Delete events for its own internal storage. The
        // hardcoded prefix list is derived from XDG dirs so it
        // follows whatever the user has configured, and it is
        // applied unconditionally on top of the user-supplied
        // `exclude` list (cannot be turned off via config).
        let mut exclude = lixun_sources::exclude::lixun_self_excludes();
        exclude.extend(self.config.exclude.iter().cloned());

        Ok(lixun_sources::fs::FsSource::with_regex_and_ocr(
            self.config.roots.clone(),
            exclude,
            self.config.exclude_regex.clone(),
            self.config.max_file_size_mb,
            self.caps_arc(),
            self.ocr_enqueue.get().cloned(),
        )
        .with_body_checker(self.body_checker.get().cloned())
        .with_min_image_side_px(self.config.ocr.min_image_side_px))
    }
}

impl lixun_indexer::IndexerSources for SourcesGlue {
    fn build_fs_source(&self) -> Result<lixun_sources::fs::FsSource> {
        SourcesGlue::build_fs_source(self)
    }
    fn exclude(&self) -> &[String] {
        &self.config.exclude
    }
    fn max_file_size_mb(&self) -> u64 {
        self.config.max_file_size_mb
    }
    fn caps(&self) -> Arc<lixun_extract::ExtractorCapabilities> {
        self.caps_arc()
    }
    fn ocr_enqueue(&self) -> Option<Arc<dyn lixun_sources::OcrEnqueue>> {
        self.ocr_enqueue.get().cloned()
    }
    fn body_checker(&self) -> Option<Arc<dyn lixun_sources::HasBody>> {
        self.body_checker.get().cloned()
    }
    fn min_image_side_px(&self) -> u32 {
        self.config.ocr.min_image_side_px
    }
}
