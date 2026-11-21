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

pub const MULTILINGUAL_CLIP: &str = "clip-vit-b-32-multilingual";

pub struct ClipTextEmbedder {
    projection: Option<Vec<f32>>,
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
    if model == EmbeddingModel::BGEM3 {
        prepare_external_model_files(cache_dir, &model)?;
    }
    let inner = TextEmbedding::try_new(
        InitOptions::new(model)
            .with_cache_dir(cache_dir.to_path_buf())
            .with_show_download_progress(false),
    )
    .with_context(|| format!("fastembed: text embedder init for '{model_name}'"))?;
    Ok(TextEmbedder { inner, dim })
}

fn model_repository(cache_dir: &Path, repo_id: &str) -> Result<hf_hub::api::sync::ApiRepo> {
    let cache_dir = std::env::var_os("HF_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| cache_dir.to_owned());
    let api = hf_hub::api::sync::ApiBuilder::from_env()
        .with_cache_dir(cache_dir)
        .with_progress(false)
        .build()?;
    Ok(api.model(repo_id.to_owned()))
}

// ONNX Runtime requires external tensors beside the model. Standard hub
// snapshot symlinks resolve outside that directory. Replace just those
// directory entries with atomic hard links to the same immutable blobs.
fn prepare_external_model_files(cache_dir: &Path, model: &EmbeddingModel) -> Result<()> {
    let info = TextEmbedding::get_model_info(model)?;
    let repo = model_repository(cache_dir, &info.model_code)?;
    for name in std::iter::once(&info.model_file).chain(&info.additional_files) {
        let path = repo.get(name)?;
        if path.is_symlink() {
            let parent = path.parent().context("missing snapshot directory")?;
            let staging = tempfile::tempdir_in(parent)?;
            let link = staging.path().join("tensor");
            std::fs::hard_link(path.canonicalize()?, &link)?;
            std::fs::rename(link, path)?;
        }
    }
    Ok(())
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
    load_clip_query_encoder(
        "clip-vit-b-32",
        cache_dir,
        onnx_intra_threads,
        onnx_inter_threads,
    )
}

pub fn load_clip_query_encoder(
    model_name: &str,
    cache_dir: &Path,
    onnx_intra_threads: usize,
    onnx_inter_threads: usize,
) -> Result<ClipTextEmbedder> {
    if model_name == MULTILINGUAL_CLIP {
        apply_onnx_thread_env(onnx_intra_threads, onnx_inter_threads);
        return multilingual_clip(cache_dir);
    }
    let model = EmbeddingModel::ClipVitB32;
    let dim = 512;
    apply_onnx_thread_env(onnx_intra_threads, onnx_inter_threads);
    let inner = TextEmbedding::try_new(
        InitOptions::new(model)
            .with_cache_dir(cache_dir.to_path_buf())
            .with_show_download_progress(false),
    )
    .context("fastembed: CLIP text embedder init")?;
    Ok(ClipTextEmbedder {
        inner,
        dim,
        projection: None,
    })
}

// The distilled multilingual query tower projects mean-pooled tokens into the
// same normalized 512-dimensional space as the existing CLIP vision tower.
fn multilingual_clip(cache_dir: &Path) -> Result<ClipTextEmbedder> {
    use fastembed::{InitOptionsUserDefined, Pooling, TokenizerFiles, UserDefinedEmbeddingModel};
    let repo = model_repository(
        cache_dir,
        "sentence-transformers/clip-ViT-B-32-multilingual-v1",
    )?;
    let read = |name: &str| -> Result<Vec<u8>> { Ok(std::fs::read(repo.get(name)?)?) };
    let tokenizer = TokenizerFiles {
        tokenizer_file: read("tokenizer.json")?,
        config_file: read("config.json")?,
        special_tokens_map_file: read("special_tokens_map.json")?,
        tokenizer_config_file: read("tokenizer_config.json")?,
    };
    let model = UserDefinedEmbeddingModel::new(read("onnx/model_quint8_avx2.onnx")?, tokenizer)
        .with_pooling(Pooling::Mean);
    let inner = TextEmbedding::try_new_from_user_defined(
        model,
        InitOptionsUserDefined::default().with_max_length(128),
    )?;
    let bytes = read("2_Dense/model.safetensors")?;
    let weights = safetensors::SafeTensors::deserialize(&bytes)?;
    let matrix = weights.tensor("linear.weight")?;
    anyhow::ensure!(
        matrix.shape() == [512, 768] && matrix.dtype() == safetensors::Dtype::F32,
        "incompatible CLIP projection"
    );
    let projection = matrix
        .data()
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect();
    Ok(ClipTextEmbedder {
        inner,
        dim: 512,
        projection: Some(projection),
    })
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
        let mut embeddings = self
            .inner
            .embed(texts, None)
            .context("fastembed: CLIP text embed batch")?;
        if let Some(matrix) = &self.projection {
            for embedding in &mut embeddings {
                anyhow::ensure!(embedding.len() == 768, "incompatible CLIP hidden dimension");
                let mut projected: Vec<f32> = matrix
                    .as_chunks::<768>()
                    .0
                    .iter()
                    .map(|row| row.iter().zip(embedding.iter()).map(|(w, x)| w * x).sum())
                    .collect();
                let norm = projected
                    .iter()
                    .map(|x| x * x)
                    .sum::<f32>()
                    .sqrt()
                    .max(f32::EPSILON);
                projected.iter_mut().for_each(|x| *x /= norm);
                *embedding = projected;
            }
        }
        Ok(embeddings)
    }
}

/// True when `name` is a text-model id this build can load. Lets
/// config sanitization degrade an unknown id (e.g. a typo in a
/// daemon-pushed config) to the default model instead of aborting
/// the worker.
pub fn is_supported_text_model(name: &str) -> bool {
    resolve_text_model(name).is_ok()
}

/// Image-model counterpart of [`is_supported_text_model`].
pub fn is_supported_image_model(name: &str) -> bool {
    resolve_image_model(name).is_ok()
}

fn resolve_text_model(name: &str) -> Result<(EmbeddingModel, usize)> {
    match name {
        // Both builds emit 384 dimensions. Keep separate vector stores for
        // the int8 and fp32 document encoders when switching builds.
        "bge-small-en-v1.5" => {
            #[cfg(feature = "quantized-embedders")]
            {
                Ok((EmbeddingModel::BGESmallENV15Q, 384))
            }
            #[cfg(not(feature = "quantized-embedders"))]
            {
                Ok((EmbeddingModel::BGESmallENV15, 384))
            }
        }
        // BAAI bge-m3 has no statically-quantized variant in fastembed
        // 5.13.3, so this arm is the same in both feature states.
        "bge-m3" => Ok((EmbeddingModel::BGEM3, 1024)),
        other => anyhow::bail!(
            "semantic.text_model='{other}' not supported in v1 \
             (allowed: bge-small-en-v1.5, bge-m3)"
        ),
    }
}

fn resolve_image_model(name: &str) -> Result<(ImageEmbeddingModel, usize)> {
    match name {
        // CLIP ViT-B/32 vision encoder. fastembed 5.13.3 does NOT ship
        // a statically pre-quantized variant of either
        // `ImageEmbeddingModel::ClipVitB32` or
        // `EmbeddingModel::ClipVitB32` (the matching text tower), so
        // there is no `ClipVitB32Q` to switch to when the
        // `quantized-embedders` feature is on. The image and CLIP-text
        // sides therefore stay on fp32 (~150 MB per branch) regardless
        // of the feature state; B2 only quantizes the BGE text path
        // and the CLIP int8 gap is tracked in
        // `.workspace-local/notepads/launcher-perf-and-semantic-memory/decisions.md`
        // under "B2 partial: only BGE int8 available".
        "clip-vit-b-32" | MULTILINGUAL_CLIP => Ok((ImageEmbeddingModel::ClipVitB32, 512)),
        other => anyhow::bail!(
            "semantic.image_model='{other}' not supported in v1 \
             (allowed: clip-vit-b-32, clip-vit-b-32-multilingual)"
        ),
    }
}
