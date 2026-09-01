use crate::source::CalculatorSource;
use anyhow::Result;
use lixun_sources::{PluginBuildContext, PluginFactory, PluginFactoryEntry, PluginInstance};
use std::sync::Arc;

lixun_sources::inventory::submit! {
    PluginFactoryEntry { new: || Box::new(CalculatorFactory) }
}

pub struct CalculatorFactory;

impl PluginFactory for CalculatorFactory {
    fn section(&self) -> &'static str {
        "calculator"
    }

    /// Zero-config: the daemon registers the calculator even without a
    /// `[calculator]` section. `enabled = false` in an explicit section
    /// opts out.
    fn default_enabled(&self) -> bool {
        true
    }

    fn build(&self, raw: &toml::Value, _ctx: &PluginBuildContext) -> Result<Vec<PluginInstance>> {
        if raw.get("enabled").and_then(|v| v.as_bool()) == Some(false) {
            return Ok(Vec::new());
        }
        Ok(vec![PluginInstance {
            instance_id: "calculator".into(),
            source: Arc::new(CalculatorSource),
        }])
    }
}
