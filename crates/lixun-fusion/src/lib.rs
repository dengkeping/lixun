//! Hybrid BM25 × ANN retrieval layered over `SearchHandle`.
//!
//! WD-T0 scaffolding only — concrete types land in WD-T1 and WD-T7.

#![allow(dead_code)]

#[cfg(feature = "semantic")]
mod ann;
#[cfg(feature = "semantic")]
mod debug;
mod handle;
#[cfg(feature = "semantic")]
mod rrf;

#[cfg(feature = "semantic")]
pub use debug::FusionDebug;
pub use handle::HybridSearchHandle;
