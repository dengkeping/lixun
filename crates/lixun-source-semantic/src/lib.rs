//! Semantic search source plugin (Wave D).
//!
//! WD-T4 skeleton: registers a [`PluginFactory`] gated on the
//! `[semantic]` config section and exposes capability hooks
//! (`broadcaster()` / `ann_handle()`) that future tasks (WD-T5 / WD-T6
//! / WD-T7) flesh out with the real LanceDB store, fastembed worker,
//! and backfill journal.
//!
//! [`PluginFactory`]: lixun_sources::PluginFactory

#![allow(dead_code)]

mod ann;
mod broadcaster;
mod config;
mod embedder;
mod factory;
mod journal;
mod source;
mod store;
mod worker;

pub use ann::LanceDbAnnHandle;
pub use broadcaster::SemanticBroadcasterAdapter;
pub use config::SemanticConfig;
pub use embedder::{ImageEmbedder, TextEmbedder, load_image_embedder, load_text_embedder};
pub use factory::SemanticFactory;
pub use journal::{BackfillJournal, default_journal_path};
pub use source::SemanticSource;
pub use store::VectorStore;
pub use worker::{CHANNEL_IMAGE, CHANNEL_TEXT, EmbedJob, WorkerHandle, spawn_worker, start_backfill};
