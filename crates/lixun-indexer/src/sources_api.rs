use anyhow::Result;
use std::sync::Arc;

pub trait IndexerSources: Send + Sync {
    fn build_fs_source(&self) -> Result<lixun_sources::fs::FsSource>;
    fn exclude(&self) -> &[String];
    fn max_file_size_mb(&self) -> u64;
    fn caps(&self) -> Arc<lixun_extract::ExtractorCapabilities>;
    fn ocr_enqueue(&self) -> Option<Arc<dyn lixun_sources::OcrEnqueue>>;
    fn body_checker(&self) -> Option<Arc<dyn lixun_sources::HasBody>> {
        None
    }
    /// Minimum image-side threshold (pixels) for the enqueue-side OCR
    /// pre-filter. `0` disables it so every OCR-candidate image still
    /// reaches the queue (v1.1 behaviour). See
    /// `FsSource::extract_content` for the exact semantics.
    fn min_image_side_px(&self) -> u32 {
        0
    }
}
