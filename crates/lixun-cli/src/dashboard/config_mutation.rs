//! Config file mutation utilities for dashboard toggles.
//!
//! Uses `toml_edit` to preserve comments and formatting when mutating
//! config.toml. Pattern copied from `lixun-daemon::config::persist_impact_level`.

use anyhow::Result;
use std::path::PathBuf;

/// Read semantic.enabled from config.toml.
///
/// Returns None if config doesn't exist or [semantic] section is missing.
pub fn read_semantic_enabled() -> Result<Option<bool>> {
    let path = dirs::config_dir()
        .ok_or_else(|| anyhow::anyhow!("No config dir"))?
        .join("lixun/config.toml");
    
    if !path.exists() {
        return Ok(None);
    }
    
    let raw = std::fs::read_to_string(&path)?;
    let doc: toml_edit::DocumentMut = raw.parse()?;
    
    let enabled = doc
        .get("semantic")
        .and_then(|item| item.as_table())
        .and_then(|table| table.get("enabled"))
        .and_then(|val| val.as_bool());
    
    Ok(enabled)
}

/// Read ocr.enabled from config.toml.
///
/// Returns None if config doesn't exist or [ocr] section is missing.
#[allow(dead_code)]
pub fn read_ocr_enabled() -> Result<Option<bool>> {
    let path = dirs::config_dir()
        .ok_or_else(|| anyhow::anyhow!("No config dir"))?
        .join("lixun/config.toml");
    
    if !path.exists() {
        return Ok(None);
    }
    
    let raw = std::fs::read_to_string(&path)?;
    let doc: toml_edit::DocumentMut = raw.parse()?;
    
    let enabled = doc
        .get("ocr")
        .and_then(|item| item.as_table())
        .and_then(|table| table.get("enabled"))
        .and_then(|val| val.as_bool());
    
    Ok(enabled)
}

/// Persist OCR enabled/disabled state to config.toml.
///
/// Mutates `[ocr].enabled = true/false` while preserving comments.
pub fn persist_ocr_enabled(enabled: bool) -> Result<PathBuf> {
    let path = dirs::config_dir()
        .ok_or_else(|| anyhow::anyhow!("No config dir"))?
        .join("lixun/config.toml");
    
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    
    let new_doc = if path.exists() {
        let raw = std::fs::read_to_string(&path)?;
        let mut doc: toml_edit::DocumentMut = raw.parse()?;
        let ocr_item = doc
            .entry("ocr")
            .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
        let table = ocr_item
            .as_table_mut()
            .ok_or_else(|| anyhow::anyhow!("[ocr] in {} is not a table", path.display()))?;
        table["enabled"] = toml_edit::value(enabled);
        doc.to_string()
    } else {
        format!("[ocr]\nenabled = {enabled}\n")
    };
    
    std::fs::write(&path, new_doc)?;
    Ok(path)
}

/// Persist semantic search enabled/disabled state to config.toml.
///
/// Mutates `[semantic].enabled = true/false` while preserving comments.
pub fn persist_semantic_enabled(enabled: bool) -> Result<PathBuf> {
    let path = dirs::config_dir()
        .ok_or_else(|| anyhow::anyhow!("No config dir"))?
        .join("lixun/config.toml");
    
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    
    let new_doc = if path.exists() {
        let raw = std::fs::read_to_string(&path)?;
        let mut doc: toml_edit::DocumentMut = raw.parse()?;
        let semantic_item = doc
            .entry("semantic")
            .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
        let table = semantic_item
            .as_table_mut()
            .ok_or_else(|| anyhow::anyhow!("[semantic] in {} is not a table", path.display()))?;
        table["enabled"] = toml_edit::value(enabled);
        doc.to_string()
    } else {
        format!("[semantic]\nenabled = {enabled}\n")
    };
    
    std::fs::write(&path, new_doc)?;
    Ok(path)
}
