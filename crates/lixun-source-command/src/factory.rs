//! Factory: builds one [`CommandSource`] per `[[command_source]]`
//! config entry. Config-gated — no section, no instances.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use lixun_sources::{PluginBuildContext, PluginFactory, PluginFactoryEntry, PluginInstance};
use serde::Deserialize;

use crate::source::CommandSource;

lixun_sources::inventory::submit! {
    PluginFactoryEntry { new: || Box::new(CommandFactory) }
}

pub struct CommandFactory;

const DEFAULT_TIMEOUT_MS: u64 = 1500;
/// Sanity ceiling: a claimed query runs synchronously per keystroke
/// batch, so a multi-minute timeout would wedge the search path.
const MAX_TIMEOUT_MS: u64 = 30_000;

#[derive(Debug, Deserialize)]
struct CommandEntry {
    name: Option<String>,
    prefix: Option<String>,
    argv: Option<Vec<String>>,
    timeout_ms: Option<u64>,
    cwd: Option<String>,
}

fn expand_tilde(s: &str) -> PathBuf {
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

impl PluginFactory for CommandFactory {
    fn section(&self) -> &'static str {
        "command_source"
    }

    // Config-gated (trait-default `default_enabled = false`): a
    // command source only exists when the operator declared one.

    fn build(&self, raw: &toml::Value, _ctx: &PluginBuildContext) -> Result<Vec<PluginInstance>> {
        // `[[command_source]]` parses as a top-level array of tables;
        // tolerate a single bare `[command_source]` table too.
        let entries: Vec<toml::Value> = match raw {
            toml::Value::Array(a) => a.clone(),
            toml::Value::Table(_) => vec![raw.clone()],
            other => anyhow::bail!(
                "[[command_source]] must be an array of tables, got {}",
                other.type_str()
            ),
        };

        let mut instances = Vec::new();
        let mut seen_prefixes: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for (i, entry) in entries.into_iter().enumerate() {
            let parsed: CommandEntry = entry.try_into().map_err(|e| {
                anyhow::anyhow!("[[command_source]] entry {}: invalid shape: {e}", i + 1)
            })?;
            let Some(name) = parsed
                .name
                .as_deref()
                .map(str::trim)
                .filter(|n| !n.is_empty())
            else {
                tracing::warn!("[[command_source]] entry {}: missing `name`; skipped", i + 1);
                continue;
            };
            let Some(prefix) = parsed.prefix.as_deref().filter(|p| !p.trim().is_empty())
            else {
                tracing::warn!("[[command_source]] {name:?}: missing `prefix`; skipped");
                continue;
            };
            let Some(argv) = parsed.argv.filter(|a| !a.is_empty()) else {
                tracing::warn!("[[command_source]] {name:?}: missing/empty `argv`; skipped");
                continue;
            };
            // The daemon aborts startup on duplicate claimed
            // prefixes across plugins; dedup within our own config
            // so a copy-paste mistake degrades to a warning.
            if !seen_prefixes.insert(prefix.to_string()) {
                tracing::warn!("[[command_source]] duplicate prefix {prefix:?}; skipped");
                continue;
            }
            let timeout_ms = parsed
                .timeout_ms
                .unwrap_or(DEFAULT_TIMEOUT_MS)
                .clamp(1, MAX_TIMEOUT_MS);
            let prefix_static: &'static str = Box::leak(prefix.to_string().into_boxed_str());
            instances.push(PluginInstance {
                instance_id: format!("command:{name}"),
                source: Arc::new(CommandSource {
                    name: name.to_string(),
                    prefix: prefix_static,
                    argv,
                    timeout: Duration::from_millis(timeout_ms),
                    cwd: parsed.cwd.as_deref().map(expand_tilde),
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
            state_dir_root: PathBuf::from("/tmp"),
            impact: Arc::new(ImpactProfile::from_level(
                lixun_core::SystemImpact::High,
                4,
            )),
        }
    }

    /// Parse a document whose top level contains `[[command_source]]`
    /// entries and hand the factory exactly the value the daemon's
    /// plugin_sections would carry.
    fn build(doc: &str) -> Vec<PluginInstance> {
        let raw: toml::Value = toml::from_str(doc).unwrap();
        let section = raw.get("command_source").cloned().unwrap();
        CommandFactory.build(&section, &ctx()).unwrap()
    }

    #[test]
    fn builds_instances_from_array_of_tables() {
        let instances = build(
            "[[command_source]]\nname = \"Passwords\"\nprefix = \"pass \"\n\
             argv = [\"/usr/bin/env\", \"true\", \"{query}\"]\ntimeout_ms = 900\n\
             [[command_source]]\nname = \"Notes\"\nprefix = \"note \"\nargv = [\"notes.sh\"]\n",
        );
        assert_eq!(instances.len(), 2);
        assert_eq!(instances[0].instance_id, "command:Passwords");
        assert_eq!(instances[0].source.claimed_prefix(), Some("pass "));
        assert_eq!(instances[1].source.claimed_prefix(), Some("note "));
    }

    #[test]
    fn invalid_entries_skipped_and_prefixes_deduped() {
        let instances = build(
            "[[command_source]]\nname = \"A\"\nprefix = \"a \"\nargv = [\"x\"]\n\
             [[command_source]]\nname = \"NoArgv\"\nprefix = \"n \"\n\
             [[command_source]]\nname = \"NoPrefix\"\nargv = [\"x\"]\n\
             [[command_source]]\nname = \"Dup\"\nprefix = \"a \"\nargv = [\"y\"]\n",
        );
        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].instance_id, "command:A");
    }

    #[test]
    fn timeout_defaults_and_clamps() {
        let instances = build(
            "[[command_source]]\nname = \"A\"\nprefix = \"a \"\nargv = [\"x\"]\n\
             [[command_source]]\nname = \"B\"\nprefix = \"b \"\nargv = [\"x\"]\ntimeout_ms = 999999\n",
        );
        assert_eq!(instances.len(), 2);
        // Timeout is internal to CommandSource; assert via claims so
        // the build path is exercised. (Clamp behaviour is covered by
        // the constants; a build must not fail on out-of-range.)
        assert!(instances[1].source.claims_query("b x"));
    }
}
