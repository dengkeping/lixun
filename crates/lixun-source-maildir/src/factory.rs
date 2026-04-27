use crate::MaildirSource;
use anyhow::{Result, bail};
use lixun_sources::{PluginBuildContext, PluginFactory, PluginFactoryEntry, PluginInstance};
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;

lixun_sources::inventory::submit! {
    PluginFactoryEntry { new: || Box::new(MaildirFactory) }
}

#[derive(Debug, Deserialize)]
struct MaildirEntry {
    id: String,
    #[serde(default = "default_true")]
    enabled: bool,
    paths: Vec<String>,
    #[serde(default)]
    open_cmd: Vec<String>,
}

fn default_true() -> bool {
    true
}

pub struct MaildirFactory;

impl PluginFactory for MaildirFactory {
    fn section(&self) -> &'static str {
        "maildir"
    }

    fn build(&self, raw: &toml::Value, _ctx: &PluginBuildContext) -> Result<Vec<PluginInstance>> {
        let array = match raw {
            toml::Value::Array(arr) => arr.clone(),
            other => bail!(
                "config: [[maildir]] must be an array of tables, got {}",
                other.type_str()
            ),
        };

        let mut entries: Vec<MaildirEntry> = Vec::with_capacity(array.len());
        for (i, item) in array.into_iter().enumerate() {
            let entry: MaildirEntry = item
                .try_into()
                .map_err(|e| anyhow::anyhow!("config: [[maildir]] entry {}: {}", i, e))?;
            entries.push(entry);
        }

        let mut seen_ids = std::collections::HashSet::new();
        for entry in &entries {
            if !seen_ids.insert(entry.id.clone()) {
                bail!(
                    "config: duplicate maildir source id '{}'; each [[maildir]] entry must have a unique id",
                    entry.id
                );
            }
        }

        let mut instances = Vec::new();
        for entry in entries {
            if !entry.enabled {
                continue;
            }
            if entry.paths.is_empty() {
                tracing::warn!(
                    "config: skipping maildir source '{}': no paths configured",
                    entry.id
                );
                continue;
            }
            let roots: Vec<PathBuf> = entry.paths.iter().map(|p| expand_tilde(p)).collect();
            let source = Arc::new(MaildirSource::new(roots, entry.open_cmd));
            instances.push(PluginInstance {
                instance_id: entry.id,
                source,
            });
        }

        Ok(instances)
    }
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_default();
        PathBuf::from(home).join(rest)
    } else if path == "~" {
        PathBuf::from(std::env::var("HOME").unwrap_or_default())
    } else {
        PathBuf::from(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> PluginBuildContext {
        PluginBuildContext {
            max_file_size_mb: 50,
            state_dir_root: std::env::temp_dir(),
            impact: std::sync::Arc::new(lixun_core::ImpactProfile::from_level(
                lixun_core::SystemImpact::High,
                4,
            )),
        }
    }

    fn parse_as_array(inner: &str) -> toml::Value {
        let full = format!("maildir = [\n{}\n]\n", inner);
        let parsed: toml::Value = full.parse().unwrap();
        parsed.get("maildir").unwrap().clone()
    }

    #[test]
    fn valid_array_with_two_entries() {
        let raw = parse_as_array(
            r#"
            { id = "personal", paths = ["/tmp/m1"] },
            { id = "work", paths = ["/tmp/m2"], open_cmd = ["neomutt", "-f", "{folder}"] }
            "#,
        );
        let out = MaildirFactory.build(&raw, &ctx()).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].instance_id, "personal");
        assert_eq!(out[1].instance_id, "work");
    }

    #[test]
    fn duplicate_ids_rejected() {
        let raw = parse_as_array(
            r#"
            { id = "dup", paths = ["/tmp/m1"] },
            { id = "dup", paths = ["/tmp/m2"] }
            "#,
        );
        let result = MaildirFactory.build(&raw, &ctx());
        let err = match result {
            Ok(_) => panic!("expected error for duplicate ids"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("duplicate maildir source id"),
            "error must mention duplicate: {}",
            err
        );
    }

    #[test]
    fn empty_paths_skipped_not_fatal() {
        let raw = parse_as_array(
            r#"
            { id = "good", paths = ["/tmp/m1"] },
            { id = "bad", paths = [] }
            "#,
        );
        let out = MaildirFactory.build(&raw, &ctx()).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].instance_id, "good");
    }

    #[test]
    fn enabled_false_skipped() {
        let raw = parse_as_array(
            r#"
            { id = "off", enabled = false, paths = ["/tmp/m1"] }
            "#,
        );
        let out = MaildirFactory.build(&raw, &ctx()).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn tilde_expands_in_paths() {
        let raw = parse_as_array(
            r#"
            { id = "home", paths = ["~/Mail"] }
            "#,
        );
        let out = MaildirFactory.build(&raw, &ctx()).unwrap();
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn non_array_input_rejected() {
        let raw: toml::Value = "enabled = true".parse().unwrap();
        let result = MaildirFactory.build(&raw, &ctx());
        assert!(
            result.is_err(),
            "scalar table should be rejected with clear error"
        );
    }
}
