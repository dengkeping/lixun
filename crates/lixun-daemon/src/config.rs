//! Configuration — ~/.config/lixun/config.toml

use anyhow::Result;
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

const KNOWN_TOP_LEVEL_KEYS: &[&str] = &[
    "roots",
    "exclude",
    "exclude_regex",
    "max_file_size_mb",
    "extractor_timeout_secs",
    "ranking",
    "keybindings",
    "preview",
    "gui",
];

#[derive(Debug, Deserialize)]
struct ConfigToml {
    roots: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    exclude_regex: Option<Vec<String>>,
    max_file_size_mb: Option<u64>,
    extractor_timeout_secs: Option<u64>,
    ranking: Option<RankingToml>,
    keybindings: Option<KeybindingsToml>,
    preview: Option<PreviewToml>,
    gui: Option<GuiToml>,
}

#[derive(Debug, Deserialize)]
struct GuiToml {
    width_percent: Option<u8>,
    height_percent: Option<u8>,
    max_width_px: Option<i32>,
    max_height_px: Option<i32>,
    preview_width_percent: Option<u8>,
    preview_height_percent: Option<u8>,
    preview_max_width_px: Option<i32>,
    preview_max_height_px: Option<i32>,
}

#[derive(Debug, Deserialize)]
struct RankingToml {
    apps: Option<f32>,
    files: Option<f32>,
    mail: Option<f32>,
    attachments: Option<f32>,
    prefix_boost: Option<f32>,
    acronym_boost: Option<f32>,
    recency_weight: Option<f32>,
    recency_tau_days: Option<f32>,
    frecency_alpha: Option<f32>,
    latch_weight: Option<f32>,
    latch_cap: Option<f32>,
    total_multiplier_cap: Option<f32>,
    top_hit_min_confidence: Option<f32>,
    top_hit_min_margin: Option<f32>,
}

#[derive(Debug, Deserialize)]
struct PreviewToml {
    enabled: Option<bool>,
    default_format: Option<String>,
    max_file_size_mb: Option<u64>,
    cache_dir: Option<String>,
}

const KNOWN_PREVIEW_KEYS: &[&str] = &["enabled", "default_format", "max_file_size_mb", "cache_dir"];

#[derive(Debug, Clone)]
pub struct PreviewConfig {
    /// Master switch for the preview subsystem. Consumed by G2.8+;
    /// no code path reads it yet.
    pub enabled: bool,
    /// Plugin id to force, or `"auto"` to dispatch by MIME / extension.
    /// Unknown ids are validated by the preview process at open time,
    /// not by the daemon — daemon stores the string verbatim.
    pub default_format: String,
    /// Upper bound on file size (MiB) the preview layer will attempt
    /// to render. Files larger than this get a "too large" placeholder.
    /// Intentionally independent from and larger than the top-level
    /// `max_file_size_mb` (which gates text extraction, not rendering).
    pub max_file_size_mb: u64,
    /// Directory for rendered thumbnails and cached preview artefacts.
    /// Tilde is expanded at parse time. Not created by the config
    /// loader; preview writers are responsible for `create_dir_all`.
    pub cache_dir: PathBuf,
    /// Raw per-plugin config tables under `[preview.<plugin>]`, e.g.
    /// `[preview.code] theme = "..."`. Preserved verbatim so preview
    /// plugins can parse their own shape without the daemon knowing
    /// any specific format. Mirrors the top-level `plugin_sections`
    /// pattern used for source plugins.
    pub plugin_sections: BTreeMap<String, toml::Value>,
}

impl Default for PreviewConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            default_format: "auto".into(),
            max_file_size_mb: 200,
            cache_dir: default_preview_cache_dir(),
            plugin_sections: BTreeMap::new(),
        }
    }
}

fn default_preview_cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".cache"))
        .join("lixun/preview")
}

#[derive(Debug, Deserialize)]
struct KeybindingsToml {
    close: Option<String>,
    primary_action: Option<String>,
    secondary_action: Option<String>,
    copy: Option<String>,
    quick_look: Option<String>,
    history_up: Option<String>,
    next_result: Option<String>,
    previous_result: Option<String>,
    next_category: Option<String>,
    previous_category: Option<String>,
    filter_all: Option<String>,
    filter_apps: Option<String>,
    filter_files: Option<String>,
    filter_mail: Option<String>,
    filter_attachments: Option<String>,
    global_toggle: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Keybindings {
    pub close: String,
    pub primary_action: String,
    pub secondary_action: String,
    pub copy: String,
    pub quick_look: String,
    pub history_up: String,
    pub next_result: String,
    pub previous_result: String,
    pub next_category: String,
    pub previous_category: String,
    pub filter_all: String,
    pub filter_apps: String,
    pub filter_files: String,
    pub filter_mail: String,
    pub filter_attachments: String,
    pub global_toggle: String,
}

pub struct Config {
    pub roots: Vec<PathBuf>,
    pub exclude: Vec<String>,
    pub exclude_regex: Vec<regex::Regex>,
    pub max_file_size_mb: u64,
    pub extractor_timeout_secs: u64,
    pub ranking_apps: f32,
    pub ranking_files: f32,
    pub ranking_mail: f32,
    pub ranking_attachments: f32,
    pub ranking_prefix_boost: f32,
    pub ranking_acronym_boost: f32,
    pub ranking_recency_weight: f32,
    pub ranking_recency_tau_days: f32,
    pub ranking_frecency_alpha: f32,
    pub ranking_latch_weight: f32,
    pub ranking_latch_cap: f32,
    pub ranking_total_multiplier_cap: f32,
    pub ranking_top_hit_min_confidence: f32,
    pub ranking_top_hit_min_margin: f32,
    pub keybindings: Keybindings,
    pub preview: PreviewConfig,
    pub gui: GuiConfig,
    pub state_dir: PathBuf,
    pub plugin_sections: BTreeMap<String, toml::Value>,
}

/// Launcher + preview window sizing policy. Percentages are of the
/// monitor the window opens on (resolved at window-build time).
/// Percent values outside 10-95 are clamped.
///
/// Pixel caps (`max_*_px`) impose an absolute ceiling regardless of
/// monitor size — this matches Spotlight on macOS, where the
/// launcher stays around 680 pt and the Quick Look pane at around
/// 1800×1200 pt even on a 6K display, avoiding windows that feel
/// oversized on large monitors.
///
/// Effective size is `min(percent * monitor, max_px)`.
#[derive(Debug, Clone)]
pub struct GuiConfig {
    pub width_percent: u8,
    pub height_percent: u8,
    pub max_width_px: i32,
    pub max_height_px: i32,
    pub preview_width_percent: u8,
    pub preview_height_percent: u8,
    pub preview_max_width_px: i32,
    pub preview_max_height_px: i32,
}

impl Default for GuiConfig {
    fn default() -> Self {
        Self {
            width_percent: 40,
            height_percent: 60,
            max_width_px: 900,
            max_height_px: 800,
            preview_width_percent: 80,
            preview_height_percent: 80,
            preview_max_width_px: 2000,
            preview_max_height_px: 1400,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home".into());
        Self {
            roots: vec![PathBuf::from(&home)],
            exclude: default_excludes(),
            exclude_regex: Vec::new(),
            max_file_size_mb: 50,
            extractor_timeout_secs: 15,
            ranking_apps: 1.3,
            ranking_files: 1.2,
            ranking_mail: 1.0,
            ranking_attachments: 0.9,
            ranking_prefix_boost: 1.4,
            ranking_acronym_boost: 1.25,
            ranking_recency_weight: 0.2,
            ranking_recency_tau_days: 30.0,
            ranking_frecency_alpha: 0.1,
            ranking_latch_weight: 0.5,
            ranking_latch_cap: 3.0,
            ranking_total_multiplier_cap: 6.0,
            ranking_top_hit_min_confidence: 0.6,
            ranking_top_hit_min_margin: 1.3,
            keybindings: Keybindings::default(),
            preview: PreviewConfig::default(),
            gui: GuiConfig::default(),
            state_dir: state_dir(),
            plugin_sections: BTreeMap::new(),
        }
    }
}

fn default_excludes() -> Vec<String> {
    vec![
        ".cache".into(),
        ".local/share/Trash".into(),
        ".steam".into(),
        ".var/app".into(),
        "node_modules".into(),
        "target".into(),
        ".git".into(),
        ".venv".into(),
        "__pycache__".into(),
        ".thunderbird".into(),
        ".swp".into(),
        ".swo".into(),
        ".swx".into(),
    ]
}

impl Default for Keybindings {
    fn default() -> Self {
        Self {
            close: "Escape".into(),
            primary_action: "Return".into(),
            secondary_action: "<Shift>Return".into(),
            copy: "<Ctrl>c".into(),
            quick_look: "space".into(),
            history_up: "Up".into(),
            next_result: "Down".into(),
            previous_result: "Up".into(),
            next_category: "<Ctrl>Down".into(),
            previous_category: "<Ctrl>Up".into(),
            filter_all: "<Ctrl>0".into(),
            filter_apps: "<Ctrl>1".into(),
            filter_files: "<Ctrl>2".into(),
            filter_mail: "<Ctrl>3".into(),
            filter_attachments: "<Ctrl>4".into(),
            global_toggle: "Super+space".into(),
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let config_path = config_dir().join("lixun/config.toml");
        if !config_path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(&config_path)?;
        Self::from_toml_str(&content)
    }

    pub fn from_toml_str(content: &str) -> Result<Self> {
        let mut cfg = Self::default();
        let parsed: ConfigToml = toml::from_str(content)?;

        if let Some(roots) = parsed.roots {
            cfg.roots = roots.iter().map(|s| expand_tilde(s)).collect();
        }
        if let Some(extra) = parsed.exclude {
            cfg.exclude.extend(extra);
        }
        if let Some(patterns) = parsed.exclude_regex {
            for pat in patterns {
                match regex::Regex::new(&pat) {
                    Ok(r) => cfg.exclude_regex.push(r),
                    Err(e) => {
                        tracing::error!("config: skipping invalid exclude_regex '{}': {}", pat, e)
                    }
                }
            }
        }
        if let Some(max) = parsed.max_file_size_mb {
            cfg.max_file_size_mb = max;
        }
        if let Some(timeout) = parsed.extractor_timeout_secs {
            cfg.extractor_timeout_secs = timeout;
        }
        if let Some(ranking) = parsed.ranking {
            cfg.ranking_apps = ranking.apps.unwrap_or(1.3);
            cfg.ranking_files = ranking.files.unwrap_or(1.2);
            cfg.ranking_mail = ranking.mail.unwrap_or(1.0);
            cfg.ranking_attachments = ranking.attachments.unwrap_or(0.9);
            cfg.ranking_prefix_boost = ranking.prefix_boost.unwrap_or(1.4);
            cfg.ranking_acronym_boost = ranking.acronym_boost.unwrap_or(1.25);
            cfg.ranking_recency_weight = ranking.recency_weight.unwrap_or(0.2);
            cfg.ranking_recency_tau_days = ranking.recency_tau_days.unwrap_or(30.0);
            cfg.ranking_frecency_alpha = ranking.frecency_alpha.unwrap_or(0.1);
            cfg.ranking_latch_weight = ranking.latch_weight.unwrap_or(0.5);
            cfg.ranking_latch_cap = ranking.latch_cap.unwrap_or(3.0);
            cfg.ranking_total_multiplier_cap =
                ranking.total_multiplier_cap.unwrap_or(6.0);
            cfg.ranking_top_hit_min_confidence =
                ranking.top_hit_min_confidence.unwrap_or(0.6);
            cfg.ranking_top_hit_min_margin = ranking.top_hit_min_margin.unwrap_or(1.3);
        }
        if let Some(bindings) = parsed.keybindings {
            if let Some(v) = bindings.close {
                cfg.keybindings.close = v;
            }
            if let Some(v) = bindings.primary_action {
                cfg.keybindings.primary_action = v;
            }
            if let Some(v) = bindings.secondary_action {
                cfg.keybindings.secondary_action = v;
            }
            if let Some(v) = bindings.copy {
                cfg.keybindings.copy = v;
            }
            if let Some(v) = bindings.quick_look {
                cfg.keybindings.quick_look = v;
            }
            if let Some(v) = bindings.history_up {
                cfg.keybindings.history_up = v;
            }
            if let Some(v) = bindings.next_result {
                cfg.keybindings.next_result = v;
            }
            if let Some(v) = bindings.previous_result {
                cfg.keybindings.previous_result = v;
            }
            if let Some(v) = bindings.next_category {
                cfg.keybindings.next_category = v;
            }
            if let Some(v) = bindings.previous_category {
                cfg.keybindings.previous_category = v;
            }
            if let Some(v) = bindings.filter_all {
                cfg.keybindings.filter_all = v;
            }
            if let Some(v) = bindings.filter_apps {
                cfg.keybindings.filter_apps = v;
            }
            if let Some(v) = bindings.filter_files {
                cfg.keybindings.filter_files = v;
            }
            if let Some(v) = bindings.filter_mail {
                cfg.keybindings.filter_mail = v;
            }
            if let Some(v) = bindings.filter_attachments {
                cfg.keybindings.filter_attachments = v;
            }
            if let Some(v) = bindings.global_toggle {
                cfg.keybindings.global_toggle = v;
            }
        }
        if let Some(preview) = parsed.preview {
            if let Some(v) = preview.enabled {
                cfg.preview.enabled = v;
            }
            if let Some(v) = preview.default_format {
                cfg.preview.default_format = v;
            }
            if let Some(v) = preview.max_file_size_mb {
                cfg.preview.max_file_size_mb = v;
            }
            if let Some(v) = preview.cache_dir {
                cfg.preview.cache_dir = expand_tilde(&v);
            }
        }
        if let Some(gui) = parsed.gui {
            if let Some(v) = gui.width_percent {
                cfg.gui.width_percent = v.clamp(10, 95);
            }
            if let Some(v) = gui.height_percent {
                cfg.gui.height_percent = v.clamp(10, 95);
            }
            if let Some(v) = gui.max_width_px {
                cfg.gui.max_width_px = v.max(200);
            }
            if let Some(v) = gui.max_height_px {
                cfg.gui.max_height_px = v.max(200);
            }
            if let Some(v) = gui.preview_width_percent {
                cfg.gui.preview_width_percent = v.clamp(10, 95);
            }
            if let Some(v) = gui.preview_height_percent {
                cfg.gui.preview_height_percent = v.clamp(10, 95);
            }
            if let Some(v) = gui.preview_max_width_px {
                cfg.gui.preview_max_width_px = v.max(400);
            }
            if let Some(v) = gui.preview_max_height_px {
                cfg.gui.preview_max_height_px = v.max(400);
            }
        }

        let known: HashSet<&'static str> = KNOWN_TOP_LEVEL_KEYS.iter().copied().collect();
        let known_preview: HashSet<&'static str> = KNOWN_PREVIEW_KEYS.iter().copied().collect();
        let raw: toml::Value = toml::from_str(content)?;
        if let toml::Value::Table(mut top) = raw {
            if let Some(toml::Value::Table(preview_table)) = top.remove("preview") {
                for (key, value) in preview_table {
                    if known_preview.contains(key.as_str()) {
                        continue;
                    }
                    cfg.preview.plugin_sections.insert(key, value);
                }
            }
            for (key, value) in top {
                if known.contains(key.as_str()) {
                    continue;
                }
                cfg.plugin_sections.insert(key, value);
            }
        }

        Ok(cfg)
    }

    pub fn build_fs_source(&self) -> Result<lixun_sources::fs::FsSource> {
        Ok(lixun_sources::fs::FsSource::with_regex(
            self.roots.clone(),
            self.exclude.clone(),
            self.exclude_regex.clone(),
            self.max_file_size_mb,
        ))
    }

    pub fn ranking_config(&self) -> lixun_core::RankingConfig {
        lixun_core::RankingConfig {
            apps: self.ranking_apps,
            files: self.ranking_files,
            mail: self.ranking_mail,
            attachments: self.ranking_attachments,
            prefix_boost: self.ranking_prefix_boost,
            acronym_boost: self.ranking_acronym_boost,
            recency_weight: self.ranking_recency_weight,
            recency_tau_days: self.ranking_recency_tau_days,
            frecency_alpha: self.ranking_frecency_alpha,
            latch_weight: self.ranking_latch_weight,
            latch_cap: self.ranking_latch_cap,
            total_multiplier_cap: self.ranking_total_multiplier_cap,
            top_hit_min_confidence: self.ranking_top_hit_min_confidence,
            top_hit_min_margin: self.ranking_top_hit_min_margin,
        }
    }
}

impl lixun_indexer::IndexerSources for Config {
    fn build_fs_source(&self) -> Result<lixun_sources::fs::FsSource> {
        Config::build_fs_source(self)
    }
    fn exclude(&self) -> &[String] {
        &self.exclude
    }
    fn max_file_size_mb(&self) -> u64 {
        self.max_file_size_mb
    }
}

pub fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_default();
        PathBuf::from(home).join(rest)
    } else if path == "~" {
        PathBuf::from(std::env::var("HOME").unwrap_or_default())
    } else {
        PathBuf::from(path)
    }
}

fn config_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from("/tmp"))
}

fn state_dir() -> PathBuf {
    dirs::state_dir()
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/state")
        })
        .join("lixun")
}
