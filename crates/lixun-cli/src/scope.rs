//! Read-only preview using the filesystem source's configured path and size policy.
use anyhow::{Result, ensure};
use lixun_daemon::{config::Config, sources_glue::SourcesGlue};
use lixun_extract::ExtractorCapabilities;
use std::{path::Path, sync::Arc, time::Duration};

pub(super) fn preview(root: &Path) -> Result<()> {
    ensure!(root.is_dir(), "Choose an existing directory");
    let root = std::path::absolute(root)?;
    let mut cfg = Config::load()?;
    cfg.roots = vec![root.clone()];
    let mut caps = ExtractorCapabilities::probe(Duration::from_secs(cfg.extractor_timeout_secs));
    caps.ocr_enabled = cfg.ocr.enabled;
    lixun_extract::init_capabilities(caps.clone());
    let offline =
        lixun_sources::manifest::Manifest::load(&cfg.state_dir).offline_mounts(&cfg.roots);
    let glue = SourcesGlue::new(Arc::new(cfg));
    let fs = glue.build_fs_source()?;
    let policy = fs.path_policy();
    let mut walk = walkdir::WalkDir::new(&root).sort_by_file_name().into_iter();
    while let Some(result) = walk.next() {
        let entry = match result {
            Ok(entry) => entry,
            Err(e) => {
                eprintln!("{e}");
                continue;
            }
        };
        let path = entry.path();
        let reason = policy.reason(path).or_else(|| {
            offline
                .iter()
                .any(|p| path.starts_with(p))
                .then(|| "offline mount".into())
        });
        if let Some(reason) = reason {
            println!("excluded ({reason})\t{}", path.display());
            if entry.file_type().is_dir() {
                walk.skip_current_dir();
            }
            continue;
        }
        if entry.depth() == 0 {
            continue;
        }
        let kind = entry.file_type();
        let classification = if kind.is_dir() {
            "name (directory)"
        } else if !kind.is_file() {
            "excluded (non-regular entry)"
        } else if entry.metadata()?.len() > fs.max_file_size_mb.saturating_mul(1024 * 1024) {
            "name (size limit)"
        } else {
            match lixun_extract::extract_path(path) {
                Ok(text) if !text.trim().is_empty() => "content",
                Ok(_) if lixun_sources::fs::ocr_eligible(path, &caps, fs.min_image_side_px) => {
                    "content (pending OCR)"
                }
                Ok(_) => "name (no extractable text)",
                Err(_) => "name (extraction failed)",
            }
        };
        println!("{classification}\t{}", path.display());
    }
    Ok(())
}
