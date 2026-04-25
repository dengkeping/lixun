//! Lixun Sources — IndexerSource trait + core built-in implementations (fs, apps).
//!
//! Opt-in source plugins live in sibling crates: `lixun-source-thunderbird`
//! (gloda + mbox attachments), `lixun-source-maildir`.

pub mod apps;
pub mod exclude;
pub mod fs;
pub mod has_body;
pub mod manifest;
pub mod mime_icons;
pub mod ocr_enqueue;
pub mod source;

pub use has_body::HasBody;
pub use inventory;
pub use ocr_enqueue::OcrEnqueue;
pub use source::{
    IndexerSource, Mutation, MutationSink, PluginBuildContext, PluginFactory, PluginFactoryEntry,
    PluginInstance, QueryContext, SourceContext, SourceEvent, SourceEventKind, WatchSpec,
};
