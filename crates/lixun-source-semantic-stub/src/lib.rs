//! Daemon-side stub for the semantic-search plugin.
//!
//! Exposes an `IndexerSource` that forwards every broadcast and ANN
//! query over the IPC channel established by the daemon's
//! `semantic_supervisor`. The supervisor calls
//! [`install_connection`] once a successful handshake completes;
//! until then (or if the worker binary is absent) the stub returns
//! no-op handles, which is the documented "no semantic" path.
//!
//! Phase 2 coexistence rule
//! ------------------------
//! The legacy in-process plugin in `lixun-source-semantic` and this
//! stub both submit a `PluginFactoryEntry` whose `section()` returns
//! `"semantic"`. To prevent a double-build for the same config
//! section, the stub's factory short-circuits to a fully no-op source
//! whenever the env var `LIXUN_SEMANTIC_WORKER` is unset — that is
//! the operator's signal that the in-process plugin should remain
//! authoritative. Phase 4 deletes the in-process plugin and this
//! gate (search for `LIXUN_SEMANTIC_WORKER` here on cleanup).

#![allow(dead_code)]

mod factory;
mod source;
mod transport;

pub use factory::SemanticIpcFactory;
pub use transport::{
    BackfillStats, SemanticConnection, SemanticIpcError, current_doc_store, install_connection,
    install_doc_store, is_connected,
};
