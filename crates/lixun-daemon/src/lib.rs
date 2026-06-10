/// Re-export of the shared config crate under the historical module
/// path so daemon-internal `crate::config::...` references keep
/// working. The schema lives in `lixun-config` so GUI hosts
/// (lixun-gui, lixun-preview-bin) can read config.toml without
/// depending on the daemon crate.
pub use lixun_config as config;
pub mod gui_control;
pub mod sources_glue;
pub mod hotkeys;
pub mod portal_identity;
pub mod preview_spawn;
pub mod semantic_supervisor;
pub mod session_env;
pub mod symlink_alias;

pub use lixun_indexer::index_service;
pub use lixun_indexer::indexer;

#[allow(unused_imports)]
use lixun_plugin_bundle as _;
