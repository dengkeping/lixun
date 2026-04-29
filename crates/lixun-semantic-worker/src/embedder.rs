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

pub struct ClipTextEmbedder {
    inner: TextEmbedding,
    dim: usize,
}

/// Apply ONNX-runtime intra/inter op thread counts before constructing
/// an embedder.
///
/// fastembed 5.13.3 (the workspace-pinned version) does not expose
/// `with_intra_threads` / `with_inter_threads` builder methods on
/// `InitOptions` or `ImageInitOptions`, and the `ort` session
/// builder is not reachable through the public fastembed API. The
/// only programmatic knob the underlying ONNX runtime honours at
/// session-creation time is the pair of environment variables
/// `ORT_INTRA_OP_NUM_THREADS` and `ORT_INTER_OP_NUM_THREADS`, which
/// `ort` reads when it builds the session inside `try_new`. So we
/// set them immediately before each `try_new` call. Both values are
/// clamped to at least 1 because ORT treats 0 as "use all cores",
/// which would defeat the impact profile.
fn apply_onnx_thread_env(intra: usize, inter: usize) {
    let intra = intra.max(1);
    let inter = inter.max(1);
    // SAFETY-NOTE: fastembed/ort are only constructed from this
    // crate, on the dedicated init paths below. `set_var` here
    // races no other readers because the embedder construction
    // funnels through `load_*_embedder` and runs before any embed
    // session exists. Once the session is built, ORT has captured
    // the value and further mutation of these vars has no effect
    // on the live session.
    unsafe {
        std::env::set_var("ORT_INTRA_OP_NUM_THREADS", intra.to_string());
        std::env::set_var("ORT_INTER_OP_NUM_THREADS", inter.to_string());
    }
}

pub fn load_text_embedder(
    model_name: &str,
    cache_dir: &Path,
    onnx_intra_threads: usize,
    onnx_inter_threads: usize,
) -> Result<TextEmbedder> {
    let (model, dim) = resolve_text_model(model_name)?;
    apply_onnx_thread_env(onnx_intra_threads, onnx_inter_threads);
    let inner = TextEmbedding::try_new(
        InitOptions::new(model)
            .with_cache_dir(cache_dir.to_path_buf())
            .with_show_download_progress(false),
    )
    .with_context(|| format!("fastembed: text embedder init for '{model_name}'"))?;
    Ok(TextEmbedder { inner, dim })
}

pub fn load_image_embedder(
    model_name: &str,
    cache_dir: &Path,
    onnx_intra_threads: usize,
    onnx_inter_threads: usize,
) -> Result<ImageEmbedder> {
    let (model, dim) = resolve_image_model(model_name)?;
    apply_onnx_thread_env(onnx_intra_threads, onnx_inter_threads);
    let inner = ImageEmbedding::try_new(
        ImageInitOptions::new(model)
            .with_cache_dir(cache_dir.to_path_buf())
            .with_show_download_progress(false),
    )
    .with_context(|| format!("fastembed: image embedder init for '{model_name}'"))?;
    Ok(ImageEmbedder { inner, dim })
}

pub fn load_clip_text_embedder(
    cache_dir: &Path,
    onnx_intra_threads: usize,
    onnx_inter_threads: usize,
) -> Result<ClipTextEmbedder> {
    let model = EmbeddingModel::ClipVitB32;
    let dim = 512;
    apply_onnx_thread_env(onnx_intra_threads, onnx_inter_threads);
    let inner = TextEmbedding::try_new(
        InitOptions::new(model)
            .with_cache_dir(cache_dir.to_path_buf())
            .with_show_download_progress(false),
    )
    .context("fastembed: CLIP text embedder init")?;
    Ok(ClipTextEmbedder { inner, dim })
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

impl ClipTextEmbedder {
    pub fn dim(&self) -> usize {
        self.dim
    }

    pub fn embed(&mut self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        self.inner
            .embed(texts, None)
            .context("fastembed: CLIP text embed batch")
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
