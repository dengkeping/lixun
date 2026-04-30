//! Hybrid BM25 × ANN retrieval layered over `SearchHandle`.

#![allow(dead_code)]

mod ann;
mod debug;
mod handle;
mod rrf;

pub use debug::FusionDebug;
pub use handle::{FusionChunk, HybridSearchHandle, Phase};
