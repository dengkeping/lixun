//! Factory: builds one [`KeywordSource`] instance per valid
//! `[[search.keyword]]` entry.
//!
//! The factory's section is `search` — the same top-level table that
//! carries the host-owned `web_engine_url` key. The host config
//! layer deliberately keeps the whole `[search]` table available in
//! `plugin_sections` so this factory can read the `keyword` array;
//! unknown-to-us keys (like `web_engine_url`) are ignored here.

use crate::source::KeywordSource;
use anyhow::Result;
use lixun_sources::{PluginBuildContext, PluginFactory, PluginFactoryEntry, PluginInstance};
use serde::Deserialize;
use std::sync::Arc;

lixun_sources::inventory::submit! {
    PluginFactoryEntry { new: || Box::new(KeywordFactory) }
}

pub struct KeywordFactory;

#[derive(Debug, Deserialize)]
struct KeywordEntry {
    key: Option<String>,
    url: Option<String>,
    name: Option<String>,
}

impl PluginFactory for KeywordFactory {
    fn section(&self) -> &'static str {
        "search"
    }

    // Config-gated on purpose: without `[[search.keyword]]` entries
    // there is nothing to claim, so `default_enabled` stays false
    // and the factory only runs when a `[search]` section exists.

    fn build(&self, raw: &toml::Value, _ctx: &PluginBuildContext) -> Result<Vec<PluginInstance>> {
        let Some(entries) = raw.get("keyword") else {
            return Ok(Vec::new());
        };
        let Some(entries) = entries.as_array() else {
            anyhow::bail!("[search].keyword must be an array of tables ([[search.keyword]])");
        };

        let mut instances = Vec::new();
        let mut seen_keys: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (i, entry) in entries.iter().enumerate() {
            let parsed: KeywordEntry = entry.clone().try_into().map_err(|e| {
                anyhow::anyhow!("[[search.keyword]] entry {}: invalid shape: {e}", i + 1)
            })?;
            let Some(key) = parsed.key.as_deref().map(str::trim).filter(|k| !k.is_empty())
            else {
                tracing::warn!("[[search.keyword]] entry {}: missing `key`; skipped", i + 1);
                continue;
            };
            if key.contains(char::is_whitespace) {
                tracing::warn!(
                    "[[search.keyword]] key {key:?} contains whitespace; skipped"
                );
                continue;
            }
            let Some(url) = parsed.url.as_deref().map(str::trim).filter(|u| !u.is_empty())
            else {
                tracing::warn!("[[search.keyword]] key {key:?}: missing `url`; skipped");
                continue;
            };
            if !url.contains("{query}") {
                tracing::warn!(
                    "[[search.keyword]] key {key:?}: url lacks the {{query}} placeholder; skipped"
                );
                continue;
            }
            // The daemon aborts startup on duplicate claimed
            // prefixes across plugins; dedup within our own config
            // here so a copy-paste mistake degrades to a warning
            // instead.
            if !seen_keys.insert(key.to_string()) {
                tracing::warn!("[[search.keyword]] duplicate key {key:?}; skipped");
                continue;
            }
            let name = parsed
                .name
                .as_deref()
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .unwrap_or(key)
                .to_string();
            // Leak "<key> " once per configured entry: claimed_prefix
            // demands &'static str; the set is small and lives for
            // the daemon's lifetime anyway.
            let claimed: &'static str = Box::leak(format!("{key} ").into_boxed_str());
            instances.push(PluginInstance {
                instance_id: format!("keyword:{key}"),
                source: Arc::new(KeywordSource {
                    key: key.to_string(),
                    url: url.to_string(),
                    name,
                    claimed,
                }),
            });
        }
        Ok(instances)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lixun_core::ImpactProfile;

    fn ctx() -> PluginBuildContext {
        PluginBuildContext {
            max_file_size_mb: 50,
            state_dir_root: std::path::PathBuf::from("/tmp"),
            impact: Arc::new(ImpactProfile::from_level(
                lixun_core::SystemImpact::High,
                4,
            )),
        }
    }

    fn build(toml_str: &str) -> Vec<PluginInstance> {
        let raw: toml::Value = toml::from_str(toml_str).unwrap();
        KeywordFactory.build(&raw, &ctx()).unwrap()
    }

    #[test]
    fn builds_one_instance_per_valid_entry() {
        let instances = build(
            "web_engine_url = \"https://duckduckgo.com/?q={query}\"\n\
             [[keyword]]\nkey = \"gh\"\nurl = \"https://github.com/search?q={query}\"\nname = \"GitHub\"\n\
             [[keyword]]\nkey = \"so\"\nurl = \"https://stackoverflow.com/search?q={query}\"\n",
        );
        assert_eq!(instances.len(), 2);
        assert_eq!(instances[0].instance_id, "keyword:gh");
        assert_eq!(instances[1].instance_id, "keyword:so");
        assert_eq!(instances[0].source.claimed_prefix(), Some("gh "));
        assert_eq!(instances[1].source.claimed_prefix(), Some("so "));
    }

    #[test]
    fn no_keyword_array_builds_nothing() {
        let instances = build("web_engine_url = \"https://duckduckgo.com/?q={query}\"\n");
        assert!(instances.is_empty());
    }

    #[test]
    fn invalid_entries_are_skipped_not_fatal() {
        let instances = build(
            "[[keyword]]\nkey = \"gh\"\nurl = \"https://github.com/search?q={query}\"\n\
             [[keyword]]\nkey = \"bad key\"\nurl = \"https://example.com/{query}\"\n\
             [[keyword]]\nkey = \"noq\"\nurl = \"https://example.com/\"\n\
             [[keyword]]\nurl = \"https://example.com/{query}\"\n\
             [[keyword]]\nkey = \"gh\"\nurl = \"https://example.com/{query}\"\n",
        );
        assert_eq!(instances.len(), 1, "only the first gh entry survives");
        assert_eq!(instances[0].instance_id, "keyword:gh");
    }
}
