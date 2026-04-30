use anyhow::Result;
use lixun_sources::{
    PluginBuildContext, PluginFactory, PluginFactoryEntry, PluginInstance, inventory,
};
use serde::Deserialize;
use std::sync::Arc;

use crate::source::SemanticIpcSource;

/// Daemon-side runtime off-switch for semantic search. Mirrors the
/// `enabled: bool` field the deleted in-process `lixun-source-semantic`
/// crate used to expose, so operators keep the same TOML contract:
/// `[semantic] enabled = true` to opt in. Default is false because
/// the legacy plugin defaulted false; opt-in is intentional and
/// load-bearing — semantic backfill downloads ~400 MB of models on
/// first use and operators must consent explicitly.
#[derive(Deserialize, Default)]
struct StubConfig {
    #[serde(default)]
    enabled: bool,
}

/// Decide whether the stub should register an instance for the given
/// raw `[semantic]` config block. Returns false when the field is
/// missing (default-false parity with the legacy plugin) and also
/// when the block is malformed — config-parse errors must NOT crash
/// daemon startup, so we log a warning and degrade to "disabled".
fn is_enabled(raw: &toml::Value) -> bool {
    match raw.clone().try_into::<StubConfig>() {
        Ok(cfg) => cfg.enabled,
        Err(e) => {
            tracing::warn!(
                "semantic stub: failed to parse [semantic] config ({e}); treating as disabled"
            );
            false
        }
    }
}

pub struct SemanticIpcFactory;

impl PluginFactory for SemanticIpcFactory {
    fn section(&self) -> &'static str {
        "semantic"
    }

    fn build(&self, raw: &toml::Value, _ctx: &PluginBuildContext) -> Result<Vec<PluginInstance>> {
        if !is_enabled(raw) {
            tracing::debug!("semantic stub disabled by config");
            return Ok(Vec::new());
        }

        Ok(vec![PluginInstance {
            instance_id: "semantic".into(),
            source: Arc::new(SemanticIpcSource::new()),
        }])
    }
}

inventory::submit! {
    PluginFactoryEntry { new: || Box::new(SemanticIpcFactory) as Box<dyn PluginFactory> }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn val(s: &str) -> toml::Value {
        toml::from_str(s).expect("test fixture parses")
    }

    #[test]
    fn explicit_enabled_true_returns_true() {
        assert!(is_enabled(&val("enabled = true")));
    }

    #[test]
    fn explicit_enabled_false_returns_false() {
        assert!(!is_enabled(&val("enabled = false")));
    }

    #[test]
    fn empty_table_defaults_to_false() {
        assert!(!is_enabled(&val("")));
    }

    #[test]
    fn malformed_enabled_degrades_to_false() {
        assert!(!is_enabled(&val(r#"enabled = "yes""#)));
    }

    #[test]
    fn unknown_extra_fields_are_ignored() {
        assert!(is_enabled(&val(r#"
            enabled = true
            text_model = "bge-small-en-v1.5"
            batch_size = 32
            "#)));
    }
}
