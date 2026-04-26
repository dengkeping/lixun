use anyhow::{Context, Result};
use lixun_sources::{
    PluginBuildContext, PluginFactory, PluginFactoryEntry, PluginInstance, inventory,
};
use std::sync::Arc;

use crate::config::SemanticConfig;
use crate::source::SemanticSource;

inventory::submit! {
    PluginFactoryEntry { new: || Box::new(SemanticFactory) as Box<dyn PluginFactory> }
}

pub struct SemanticFactory;

impl PluginFactory for SemanticFactory {
    fn section(&self) -> &'static str {
        "semantic"
    }

    fn build(&self, raw: &toml::Value, ctx: &PluginBuildContext) -> Result<Vec<PluginInstance>> {
        let config: SemanticConfig = raw
            .clone()
            .try_into()
            .context("parsing [semantic] config section")?;

        if !config.enabled {
            return Ok(Vec::new());
        }

        let state_dir = ctx.state_dir_root.join("semantic");
        let source = SemanticSource::new(config, state_dir);

        Ok(vec![PluginInstance {
            instance_id: "semantic".into(),
            source: Arc::new(source),
        }])
    }
}
