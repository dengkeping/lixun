use crate::{GlodaSource, ThunderbirdAttachmentsSource};
use anyhow::{Result, bail};
use lixun_sources::{PluginBuildContext, PluginFactory, PluginFactoryEntry, PluginInstance};
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;

lixun_sources::inventory::submit! {
    PluginFactoryEntry { new: || Box::new(ThunderbirdFactory) }
}

#[derive(Debug, Deserialize)]
struct ThunderbirdSectionToml {
    #[serde(default = "default_true")]
    enabled: bool,
    profile: Option<String>,
    gloda_batch_size: Option<u32>,
    #[serde(default = "default_true")]
    attachments: bool,
}

fn default_true() -> bool {
    true
}

pub struct ThunderbirdFactory;

impl PluginFactory for ThunderbirdFactory {
    fn section(&self) -> &'static str {
        "thunderbird"
    }

    fn build(&self, raw: &toml::Value, ctx: &PluginBuildContext) -> Result<Vec<PluginInstance>> {
        let section: ThunderbirdSectionToml = raw.clone().try_into()?;

        if !section.enabled {
            return Ok(Vec::new());
        }

        let batch_size: u32 = match section.gloda_batch_size {
            Some(0) => {
                bail!("config: [thunderbird] gloda_batch_size must be > 0 (got 0)")
            }
            Some(n) => n,
            None => 250,
        };

        let profile = match section.profile {
            Some(override_path) => {
                let expanded = expand_tilde(&override_path);
                if !expanded.exists() {
                    tracing::warn!(
                        "thunderbird: configured profile {:?} does not exist; skipping gloda + attachments",
                        expanded
                    );
                    return Ok(Vec::new());
                }
                expanded
            }
            None => match crate::gloda::find_profile() {
                Some(p) => p,
                None => {
                    tracing::info!(
                        "thunderbird: no profile found under ~/.thunderbird; skipping gloda + attachments"
                    );
                    return Ok(Vec::new());
                }
            },
        };

        let _ = &ctx.state_dir_root;

        let mut instances = Vec::with_capacity(2);
        instances.push(PluginInstance {
            instance_id: "builtin:gloda".into(),
            source: Arc::new(GlodaSource::new(profile.clone(), 0, batch_size)),
        });

        if section.attachments {
            instances.push(PluginInstance {
                instance_id: "builtin:tb_attachments".into(),
                source: Arc::new(ThunderbirdAttachmentsSource::new(
                    profile,
                    ctx.max_file_size_mb * 1024 * 1024,
                )),
            });
        } else {
            tracing::info!("thunderbird: attachments disabled by config; registering gloda only");
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
        }
    }

    fn parse(toml_str: &str) -> toml::Value {
        toml_str.parse().unwrap()
    }

    #[test]
    fn enabled_false_yields_no_instances() {
        let raw = parse("enabled = false");
        let out = ThunderbirdFactory.build(&raw, &ctx()).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn no_profile_on_disk_yields_no_instances() {
        let raw = parse("enabled = true");
        // ThunderbirdFactory will call find_profile(); unless the test env
        // happens to have ~/.thunderbird, it returns None and we get empty.
        let out = ThunderbirdFactory.build(&raw, &ctx()).unwrap();
        if std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
            .join(".thunderbird")
            .exists()
        {
            assert!(
                out.len() <= 2,
                "real profile on disk, factory produced instances"
            );
        } else {
            assert!(out.is_empty());
        }
    }

    #[test]
    fn nonexistent_profile_override_yields_no_instances() {
        let raw = parse(
            r#"
            enabled = true
            profile = "/nonexistent/path/to/thunderbird/profile"
        "#,
        );
        let out = ThunderbirdFactory.build(&raw, &ctx()).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn valid_profile_override_produces_both_instances() {
        let tmp = tempfile::tempdir().unwrap();
        let profile_path = tmp.path().to_string_lossy().to_string();
        let raw = parse(&format!(
            r#"
                enabled = true
                profile = "{}"
            "#,
            profile_path
        ));
        let out = ThunderbirdFactory.build(&raw, &ctx()).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].instance_id, "builtin:gloda");
        assert_eq!(out[1].instance_id, "builtin:tb_attachments");
    }

    #[test]
    fn attachments_false_produces_gloda_only() {
        let tmp = tempfile::tempdir().unwrap();
        let profile_path = tmp.path().to_string_lossy().to_string();
        let raw = parse(&format!(
            r#"
                enabled = true
                attachments = false
                profile = "{}"
            "#,
            profile_path
        ));
        let out = ThunderbirdFactory.build(&raw, &ctx()).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].instance_id, "builtin:gloda");
    }

    #[test]
    fn gloda_batch_size_zero_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let profile_path = tmp.path().to_string_lossy().to_string();
        let raw = parse(&format!(
            r#"
                enabled = true
                gloda_batch_size = 0
                profile = "{}"
            "#,
            profile_path
        ));
        let result = ThunderbirdFactory.build(&raw, &ctx());
        let err = match result {
            Ok(_) => panic!("expected error for gloda_batch_size = 0"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("gloda_batch_size"),
            "error must mention field, got: {}",
            err
        );
    }

    #[test]
    fn custom_batch_size_is_accepted() {
        let tmp = tempfile::tempdir().unwrap();
        let profile_path = tmp.path().to_string_lossy().to_string();
        let raw = parse(&format!(
            r#"
                enabled = true
                gloda_batch_size = 1000
                profile = "{}"
            "#,
            profile_path
        ));
        let out = ThunderbirdFactory.build(&raw, &ctx()).unwrap();
        assert_eq!(out.len(), 2);
    }
}
