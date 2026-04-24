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

    fn build(&self, _raw: &toml::Value, _ctx: &PluginBuildContext) -> Result<Vec<PluginInstance>> {
        Ok(vec![PluginInstance {
            instance_id: "calculator".into(),
            source: Arc::new(CalculatorSource),
        }])
    }
}
