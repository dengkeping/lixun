use anyhow::Result;
use std::sync::Arc;

pub trait IndexerSources: Send + Sync {
    fn build_fs_source(&self) -> Result<lixun_sources::fs::FsSource>;
    fn exclude(&self) -> &[String];
    fn max_file_size_mb(&self) -> u64;
    fn caps(&self) -> Arc<lixun_extract::ExtractorCapabilities>;
    fn ocr_enqueue(&self) -> Option<Arc<dyn lixun_sources::OcrEnqueue>>;
}
