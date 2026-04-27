use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use lixun_sources::{PluginBuildContext, PluginFactory, PluginFactoryEntry, PluginInstance};
use serde::Deserialize;

use crate::source::ShellSource;

lixun_sources::inventory::submit! {
    PluginFactoryEntry { new: || Box::new(ShellFactory) }
}

pub struct ShellFactory;

impl PluginFactory for ShellFactory {
    fn section(&self) -> &'static str {
        "shell"
    }

    fn build(&self, raw: &toml::Value, _ctx: &PluginBuildContext) -> Result<Vec<PluginInstance>> {
        #[derive(Deserialize)]
        struct ShellCfg {
            working_dir: Option<String>,
            strict_mode: Option<bool>,
        }
        let cfg: ShellCfg = raw.clone().try_into()?;
        let working_dir = cfg
            .working_dir
            .map(expand_tilde)
            .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")));
        Ok(vec![PluginInstance {
            instance_id: "shell".into(),
            source: Arc::new(ShellSource {
                working_dir,
                strict_mode: cfg.strict_mode.unwrap_or(false),
            }),
        }])
    }
}

fn expand_tilde(s: String) -> PathBuf {
    if s == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    }
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(s)
}
