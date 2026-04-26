//! Hybrid BM25 × ANN retrieval layered over `SearchHandle`.
//!
//! WD-T0 scaffolding only — concrete types land in WD-T1 and WD-T7.

#![allow(dead_code)]

mod handle;
#[cfg(feature = "semantic")]
mod ann;
#[cfg(feature = "semantic")]
mod debug;
#[cfg(feature = "semantic")]
mod rrf;

pub use handle::HybridSearchHandle;
#[cfg(feature = "semantic")]
pub use debug::FusionDebug;
