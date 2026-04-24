//! Filesystem source — walk directories, extract text, produce Documents.
//!
//! Two-pass indexing:
//! 1. **Metadata pass** (sequential): collects path, filename, mtime, size.
//! 2. **Content pass** (rayon pool): extracts body text in parallel.

use anyhow::Result;
use lixun_core::{Action, Category, DocId, Document};
use rayon::prelude::*;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::manifest::Manifest;
use crate::mime_icons;

/// Filesystem source configuration.
pub struct FsSource {
    pub roots: Vec<PathBuf>,
    pub exclude: Vec<String>,
    pub exclude_regex: Vec<regex::Regex>,
    pub max_file_size_mb: u64,
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
        }
    }

    fn is_excluded(&self, path: &Path) -> bool {
        crate::exclude::path_excluded(path, &self.exclude, &self.exclude_regex)
    }

    /// Extract content from a file. Public for the watcher to reuse.
    pub fn extract_content(path: &Path) -> Result<Option<String>> {
        let text = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            lixun_extract::extract_path(path)
        }))
        .map_err(|_| anyhow::anyhow!("extractor panicked"))??;
        if text.is_empty() {
            return Ok(None);
        }
        Ok(Some(text))
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
            action: Action::OpenFile { path: meta.path.clone() },
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
        while !changed_metas.is_empty() {
            let take_n = Self::BATCH_SIZE.min(changed_metas.len());
            let chunk: Vec<FileMeta> = changed_metas.drain(..take_n).collect();
            let docs: Vec<Document> = pool.install(|| {
                chunk
                    .into_par_iter()
                    .map(|meta| {
                        // Directories: skip content extraction (no body
                        // to extract, only name is indexed for matching).
                        let (body, extract_fail) = if meta.is_dir {
                            (None, false)
                        } else if meta.size <= max_size {
                            match Self::extract_content(&meta.path) {
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
        let docs: Vec<Document> = pool.install(|| {
            metas
                .into_par_iter()
                .map(|meta| {
                    let (body, extract_fail) = if meta.is_dir {
                        (None, false)
                    } else if meta.size <= max_size {
                        match Self::extract_content(&meta.path) {
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

#[cfg(test)]
mod tests {
    use super::*;

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
