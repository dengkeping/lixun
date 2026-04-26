use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fastembed::{
    EmbeddingModel, ImageEmbedding, ImageEmbeddingModel, ImageInitOptions, InitOptions,
    TextEmbedding,
};

pub struct TextEmbedder {
    inner: TextEmbedding,
    dim: usize,
}

pub struct ImageEmbedder {
    inner: ImageEmbedding,
    dim: usize,
}

pub fn load_text_embedder(model_name: &str, cache_dir: &Path) -> Result<TextEmbedder> {
    let (model, dim) = resolve_text_model(model_name)?;
    let inner = TextEmbedding::try_new(
        InitOptions::new(model)
            .with_cache_dir(cache_dir.to_path_buf())
            .with_show_download_progress(false),
    )
    .with_context(|| format!("fastembed: text embedder init for '{model_name}'"))?;
    Ok(TextEmbedder { inner, dim })
}

pub fn load_image_embedder(model_name: &str, cache_dir: &Path) -> Result<ImageEmbedder> {
    let (model, dim) = resolve_image_model(model_name)?;
    let inner = ImageEmbedding::try_new(
        ImageInitOptions::new(model)
            .with_cache_dir(cache_dir.to_path_buf())
            .with_show_download_progress(false),
    )
    .with_context(|| format!("fastembed: image embedder init for '{model_name}'"))?;
    Ok(ImageEmbedder { inner, dim })
}

impl TextEmbedder {
    pub fn dim(&self) -> usize {
        self.dim
    }

    pub fn embed(&mut self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        self.inner
            .embed(texts, None)
            .context("fastembed: text embed batch")
    }
}

impl ImageEmbedder {
    pub fn dim(&self) -> usize {
        self.dim
    }

    pub fn embed(&mut self, paths: Vec<PathBuf>) -> Result<Vec<Vec<f32>>> {
        self.inner
            .embed(paths, None)
            .context("fastembed: image embed batch")
    }
}

fn resolve_text_model(name: &str) -> Result<(EmbeddingModel, usize)> {
    match name {
        "bge-small-en-v1.5" => Ok((EmbeddingModel::BGESmallENV15, 384)),
        "bge-m3" => Ok((EmbeddingModel::BGEM3, 1024)),
        other => anyhow::bail!(
            "semantic.text_model='{other}' not supported in v1 \
             (allowed: bge-small-en-v1.5, bge-m3)"
        ),
    }
}

fn resolve_image_model(name: &str) -> Result<(ImageEmbeddingModel, usize)> {
    match name {
        "clip-vit-b-32" => Ok((ImageEmbeddingModel::ClipVitB32, 512)),
        other => anyhow::bail!(
            "semantic.image_model='{other}' not supported in v1 \
             (allowed: clip-vit-b-32)"
        ),
    }
}
