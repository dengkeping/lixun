//! Configuration — ~/.config/lixun/config.toml

use anyhow::Result;
use lixun_core::{ImpactProfile, SystemImpact};
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
    "extract",
    "ocr",
    "impact",
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
    extract: Option<ExtractToml>,
    ocr: Option<OcrToml>,
    impact: Option<ImpactToml>,
}

/// Parse-side mirror of [`OcrConfig`]. Every field is optional so the
/// resolved [`ImpactProfile`] can seed the five profile-controlled
/// knobs (`worker_interval_secs`, `jobs_per_tick`, `adaptive_throttle`,
/// `nice_level`, `io_class_idle`) when the operator has not pinned an
/// explicit value, while preserving the existing `OcrConfig` defaults
/// for the other knobs (per plan §5.3 precedence rule).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct OcrToml {
    enabled: Option<bool>,
    languages: Option<Vec<String>>,
    max_pages_per_pdf: Option<usize>,
    min_image_side_px: Option<u32>,
    timeout_secs: Option<u64>,
    worker_interval_secs: Option<u64>,
    jobs_per_tick: Option<u32>,
    adaptive_throttle: Option<bool>,
    max_cpu_pressure_avg10: Option<f32>,
    nice_level: Option<i32>,
    io_class_idle: Option<bool>,
}

/// Wire-format mirror of [`ImpactConfig`]. Every field is optional so
/// an absent `[impact]` table, or a partially-populated one, falls
/// back to [`ImpactConfig::default`] piecewise.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ImpactToml {
    level: Option<SystemImpact>,
    follow_battery: Option<bool>,
    on_battery_level: Option<SystemImpact>,
}

/// Resolved `[impact]` configuration. Defaults match Wave D behaviour:
/// `High` level, no battery-following.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImpactConfig {
    pub level: SystemImpact,
    pub follow_battery: bool,
    pub on_battery_level: SystemImpact,
}

impl Default for ImpactConfig {
    fn default() -> Self {
        Self {
            level: SystemImpact::High,
            follow_battery: false,
            on_battery_level: SystemImpact::Low,
        }
    }
}

/// Parse-side mirror of [`ExtractConfig`]. Both knobs are optional so
/// the impact profile can seed the default when the operator has not
/// pinned an explicit value (per plan §5.3 precedence rule).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ExtractToml {
    cache_max_mb: Option<u64>,
    cache_sweep_interval_secs: Option<u64>,
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

/// Text-extraction cache configuration. Shared by every extractor
/// (pdftotext, OOXML, OCR) — lives under `~/.cache/lixun/extract/v1/`.
/// Cache sweep is a tick-scheduled LRU eviction keyed by file mtime.
/// `cache_max_mb = 0` disables the sweep tick (valid config, no warn).
#[derive(Debug, Clone, Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct ExtractConfig {
    #[serde(default = "default_cache_max_mb")]
    pub cache_max_mb: u64,
    #[serde(default = "default_cache_sweep_interval_secs")]
    pub cache_sweep_interval_secs: u64,
}

impl Default for ExtractConfig {
    fn default() -> Self {
        Self {
            cache_max_mb: default_cache_max_mb(),
            cache_sweep_interval_secs: default_cache_sweep_interval_secs(),
        }
    }
}

fn default_cache_max_mb() -> u64 {
    500
}
fn default_cache_sweep_interval_secs() -> u64 {
    600
}

/// OCR configuration. Disabled by default. Enabling requires
/// `tesseract` + at least one language pack installed on the host.
/// OCR runs deferred on a tick worker that drains a persistent queue
/// at `~/.local/state/lixun/ocr-queue.db`. Adaptive throttle fields
/// (`adaptive_throttle`, `max_cpu_pressure_avg10`, `nice_level`,
/// `io_class_idle`) are Linux-only (DB-15); on other platforms they
/// are accepted but ignored.
#[derive(Debug, Clone, Deserialize, serde::Serialize, PartialEq)]
pub struct OcrConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub languages: Vec<String>,
    #[serde(default = "default_max_pages_per_pdf")]
    pub max_pages_per_pdf: Option<usize>,
    #[serde(default = "default_min_image_side_px")]
    pub min_image_side_px: u32,
    #[serde(default = "default_ocr_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default = "default_ocr_worker_interval_secs")]
    pub worker_interval_secs: u64,
    #[serde(default = "default_ocr_jobs_per_tick")]
    pub jobs_per_tick: u32,
    #[serde(default)]
    pub adaptive_throttle: bool,
    #[serde(default = "default_max_cpu_pressure_avg10")]
    pub max_cpu_pressure_avg10: f32,
    #[serde(default = "default_nice_level")]
    pub nice_level: i32,
    #[serde(default)]
    pub io_class_idle: bool,
    #[serde(default)]
    pub content_filter: ContentFilterConfig,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize, PartialEq)]
pub struct ContentFilterConfig {
    #[serde(default = "default_content_filter_enabled")]
    pub enabled: bool,
    #[serde(default = "default_min_text_components")]
    pub min_text_components: u32,
}

impl Default for ContentFilterConfig {
    fn default() -> Self {
        Self {
            enabled: default_content_filter_enabled(),
            min_text_components: default_min_text_components(),
        }
    }
}

fn default_content_filter_enabled() -> bool {
    true
}

fn default_min_text_components() -> u32 {
    30
}

impl Default for OcrConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            languages: Vec::new(),
            max_pages_per_pdf: default_max_pages_per_pdf(),
            min_image_side_px: default_min_image_side_px(),
            timeout_secs: default_ocr_timeout_secs(),
            worker_interval_secs: default_ocr_worker_interval_secs(),
            jobs_per_tick: default_ocr_jobs_per_tick(),
            adaptive_throttle: false,
            max_cpu_pressure_avg10: default_max_cpu_pressure_avg10(),
            nice_level: default_nice_level(),
            io_class_idle: false,
            content_filter: ContentFilterConfig::default(),
        }
    }
}

fn default_max_pages_per_pdf() -> Option<usize> {
    None
}
fn default_min_image_side_px() -> u32 {
    200
}
fn default_ocr_timeout_secs() -> u64 {
    30
}
fn default_ocr_worker_interval_secs() -> u64 {
    10
}
fn default_ocr_jobs_per_tick() -> u32 {
    10
}
fn default_max_cpu_pressure_avg10() -> f32 {
    10.0
}
fn default_nice_level() -> i32 {
    19
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
    strong_latch_threshold: Option<u32>,
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
    pub ranking_strong_latch_threshold: u32,
    pub keybindings: Keybindings,
    pub preview: PreviewConfig,
    pub gui: GuiConfig,
    pub extract: ExtractConfig,
    pub ocr: OcrConfig,
    pub impact: ImpactConfig,
    pub state_dir: PathBuf,
    pub plugin_sections: BTreeMap<String, toml::Value>,
    pub extractor_caps: std::sync::OnceLock<std::sync::Arc<lixun_extract::ExtractorCapabilities>>,
    pub ocr_enqueue: std::sync::OnceLock<std::sync::Arc<dyn lixun_sources::OcrEnqueue>>,
    pub body_checker: std::sync::OnceLock<std::sync::Arc<dyn lixun_sources::HasBody>>,
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
            ranking_strong_latch_threshold: 3,
            keybindings: Keybindings::default(),
            preview: PreviewConfig::default(),
            gui: GuiConfig::default(),
            extract: ExtractConfig::default(),
            ocr: OcrConfig::default(),
            impact: ImpactConfig::default(),
            state_dir: state_dir(),
            plugin_sections: BTreeMap::new(),
            extractor_caps: std::sync::OnceLock::new(),
            ocr_enqueue: std::sync::OnceLock::new(),
            body_checker: std::sync::OnceLock::new(),
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
        let user_set_max_file_size_mb = parsed.max_file_size_mb.is_some();
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
            cfg.ranking_total_multiplier_cap = ranking.total_multiplier_cap.unwrap_or(6.0);
            cfg.ranking_top_hit_min_confidence = ranking.top_hit_min_confidence.unwrap_or(0.6);
            cfg.ranking_top_hit_min_margin = ranking.top_hit_min_margin.unwrap_or(1.3);
            cfg.ranking_strong_latch_threshold = ranking.strong_latch_threshold.unwrap_or(3);
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
        if let Some(impact_toml) = parsed.impact {
            if let Some(level) = impact_toml.level {
                cfg.impact.level = level;
            }
            if let Some(fb) = impact_toml.follow_battery {
                cfg.impact.follow_battery = fb;
            }
            if let Some(obl) = impact_toml.on_battery_level {
                cfg.impact.on_battery_level = obl;
            }
        }
        // Seed extract knobs from the resolved impact profile, then let
        // explicit [extract] keys override (precedence rule per plan §5.3).
        let profile_seed = cfg.resolved_profile();
        cfg.extract.cache_max_mb = (profile_seed.extract_cache_max_bytes / (1024 * 1024)) as u64;
        if let Some(extract_toml) = parsed.extract {
            if let Some(v) = extract_toml.cache_max_mb {
                cfg.extract.cache_max_mb = v;
            }
            if let Some(v) = extract_toml.cache_sweep_interval_secs {
                cfg.extract.cache_sweep_interval_secs = v;
            }
        }
        if !user_set_max_file_size_mb {
            cfg.max_file_size_mb = profile_seed.max_file_size_bytes / (1024 * 1024);
        }
        // Five OCR knobs are seeded from the resolved impact profile;
        // explicit `[ocr]` keys override on a per-field basis (plan
        // §5.3). The remaining OcrConfig defaults stay untouched so
        // existing behaviour for `enabled`, `languages`, `timeout_secs`,
        // `min_image_side_px`, `max_pages_per_pdf`, and
        // `max_cpu_pressure_avg10` is preserved.
        cfg.ocr.worker_interval_secs = profile_seed.ocr_worker_interval.as_secs();
        cfg.ocr.jobs_per_tick = profile_seed.ocr_jobs_per_tick as u32;
        cfg.ocr.adaptive_throttle = profile_seed.ocr_adaptive_throttle;
        cfg.ocr.nice_level = profile_seed.ocr_nice_level;
        cfg.ocr.io_class_idle = profile_seed.ocr_io_class_idle;
        if let Some(ocr) = parsed.ocr {
            if let Some(v) = ocr.enabled {
                cfg.ocr.enabled = v;
            }
            if let Some(v) = ocr.languages {
                cfg.ocr.languages = v;
            }
            if let Some(v) = ocr.max_pages_per_pdf {
                cfg.ocr.max_pages_per_pdf = Some(v);
            }
            if let Some(v) = ocr.min_image_side_px {
                cfg.ocr.min_image_side_px = v;
            }
            if let Some(v) = ocr.timeout_secs {
                cfg.ocr.timeout_secs = v;
            }
            if let Some(v) = ocr.worker_interval_secs {
                cfg.ocr.worker_interval_secs = v;
            }
            if let Some(v) = ocr.jobs_per_tick {
                cfg.ocr.jobs_per_tick = v;
            }
            if let Some(v) = ocr.adaptive_throttle {
                cfg.ocr.adaptive_throttle = v;
            }
            if let Some(v) = ocr.max_cpu_pressure_avg10 {
                cfg.ocr.max_cpu_pressure_avg10 = v;
            }
            if let Some(v) = ocr.nice_level {
                cfg.ocr.nice_level = v;
            }
            if let Some(v) = ocr.io_class_idle {
                cfg.ocr.io_class_idle = v;
            }
        }
        cfg.validate_and_normalize();

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
        // Always exclude lixun's own state, data, cache and config
        // directories. Without this guard, the fs source watches
        // LanceDB's `_transactions/*.txn` and `_versions/*.manifest`
        // rotations under $XDG_DATA_HOME/lixun/semantic/vectors/, the
        // SQLite WAL/SHM under $XDG_STATE_HOME/lixun/, and the
        // extract/fastembed caches \u2014 and re-injects them into the
        // index as user files, which then floods the semantic worker
        // with Delete events for its own internal storage. The
        // hardcoded prefix list is derived from XDG dirs so it
        // follows whatever the user has configured, and it is
        // applied unconditionally on top of the user-supplied
        // `exclude` list (cannot be turned off via config).
        let mut exclude = lixun_sources::exclude::lixun_self_excludes();
        exclude.extend(self.exclude.iter().cloned());

        Ok(lixun_sources::fs::FsSource::with_regex_and_ocr(
            self.roots.clone(),
            exclude,
            self.exclude_regex.clone(),
            self.max_file_size_mb,
            self.caps_arc(),
            self.ocr_enqueue.get().cloned(),
        )
        .with_body_checker(self.body_checker.get().cloned())
        .with_min_image_side_px(self.ocr.min_image_side_px))
    }

    pub fn caps_arc(&self) -> std::sync::Arc<lixun_extract::ExtractorCapabilities> {
        self.extractor_caps.get().cloned().unwrap_or_else(|| {
            std::sync::Arc::new(lixun_extract::ExtractorCapabilities::all_available_no_timeout())
        })
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
            strong_latch_threshold: self.ranking_strong_latch_threshold,
            // Wave B knobs (proximity T1, coordination T2) use their
            // `RankingConfig::default()` values until the daemon config
            // schema gains dedicated fields. Plumbing the toml keys is
            // deferred to a follow-up so T6 stays focused on the
            // explain-surface; defaults match the plan spec.
            ..lixun_core::RankingConfig::default()
        }
    }

    pub fn resolved_profile(&self) -> ImpactProfile {
        ImpactProfile::from_level(self.impact.level, num_cpus::get())
    }

    /// Write `level = "<lowercase>"` into the `[impact]` table of
    /// `~/.config/lixun/config.toml`, preserving every comment and
    /// every other key verbatim by editing the file via
    /// [`toml_edit::DocumentMut`]. If the file does not exist a
    /// minimal `[impact] level = "..."` document is created.
    /// Returns the on-disk path that was written.
    pub fn persist_impact_level(level: SystemImpact) -> Result<PathBuf> {
        Self::persist_impact_level_at(config_dir().join("lixun/config.toml"), level)
    }

    /// Same as [`persist_impact_level`] but writes to an explicit path.
    /// Used by unit tests to avoid mutating process-wide environment
    /// state (`XDG_CONFIG_HOME`) which races between parallel tests.
    pub fn persist_impact_level_at(path: PathBuf, level: SystemImpact) -> Result<PathBuf> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let level_str = level.to_string();
        let new_doc = if path.exists() {
            let raw = std::fs::read_to_string(&path)?;
            let mut doc: toml_edit::DocumentMut = raw.parse()?;
            let impact_item = doc
                .entry("impact")
                .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
            let table = impact_item
                .as_table_mut()
                .ok_or_else(|| anyhow::anyhow!("[impact] in {} is not a table", path.display()))?;
            table["level"] = toml_edit::value(level_str.clone());
            doc.to_string()
        } else {
            format!("[impact]\nlevel = \"{level_str}\"\n")
        };
        std::fs::write(&path, new_doc)?;
        Ok(path)
    }

    fn validate_and_normalize(&mut self) {
        if self.ocr.max_pages_per_pdf == Some(0) {
            tracing::warn!("[ocr].max_pages_per_pdf = 0 interpreted as unlimited");
            self.ocr.max_pages_per_pdf = None;
        }
        if self.ocr.worker_interval_secs == 0 {
            tracing::warn!("[ocr].worker_interval_secs = 0 clamped to 1");
            self.ocr.worker_interval_secs = 1;
        }
        if self.ocr.jobs_per_tick == 0 {
            tracing::warn!("[ocr].jobs_per_tick = 0 clamped to 1");
            self.ocr.jobs_per_tick = 1;
        }
        if !(0..=19).contains(&self.ocr.nice_level) {
            let clamped = self.ocr.nice_level.clamp(0, 19);
            tracing::warn!(
                "[ocr].nice_level = {} out of 0..=19, clamped to {}",
                self.ocr.nice_level,
                clamped
            );
            self.ocr.nice_level = clamped;
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
    fn caps(&self) -> std::sync::Arc<lixun_extract::ExtractorCapabilities> {
        self.caps_arc()
    }
    fn ocr_enqueue(&self) -> Option<std::sync::Arc<dyn lixun_sources::OcrEnqueue>> {
        self.ocr_enqueue.get().cloned()
    }
    fn body_checker(&self) -> Option<std::sync::Arc<dyn lixun_sources::HasBody>> {
        self.body_checker.get().cloned()
    }
    fn min_image_side_px(&self) -> u32 {
        self.ocr.min_image_side_px
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strong_latch_threshold_defaults_to_three() {
        let cfg = Config::default();
        assert_eq!(cfg.ranking_strong_latch_threshold, 3);
        let ranking = cfg.ranking_config();
        assert_eq!(ranking.strong_latch_threshold, 3);
    }

    #[test]
    fn strong_latch_threshold_propagates_from_config_to_ranking() {
        let cfg = Config {
            ranking_strong_latch_threshold: 7,
            ..Config::default()
        };
        let ranking = cfg.ranking_config();
        assert_eq!(ranking.strong_latch_threshold, 7);
    }

    #[test]
    fn total_multiplier_cap_defaults_to_six() {
        let cfg = Config::default();
        assert_eq!(cfg.ranking_total_multiplier_cap, 6.0);
        let ranking = cfg.ranking_config();
        assert!((ranking.total_multiplier_cap - 6.0).abs() < f32::EPSILON);
    }

    #[test]
    fn extract_config_round_trip() {
        let ec = ExtractConfig {
            cache_max_mb: 1024,
            cache_sweep_interval_secs: 30,
        };
        let s = toml::to_string(&ec).unwrap();
        let parsed: ExtractConfig = toml::from_str(&s).unwrap();
        assert_eq!(ec, parsed);
    }

    #[test]
    fn ocr_config_round_trip() {
        let oc = OcrConfig {
            enabled: true,
            languages: vec!["eng".into(), "rus".into()],
            max_pages_per_pdf: Some(20),
            min_image_side_px: 300,
            timeout_secs: 45,
            worker_interval_secs: 90,
            jobs_per_tick: 25,
            adaptive_throttle: true,
            max_cpu_pressure_avg10: 25.0,
            nice_level: 10,
            io_class_idle: true,
            content_filter: ContentFilterConfig::default(),
        };
        let s = toml::to_string(&oc).unwrap();
        let parsed: OcrConfig = toml::from_str(&s).unwrap();
        assert_eq!(oc, parsed);
    }

    #[test]
    fn extract_config_defaults_match_plan() {
        let ec = ExtractConfig::default();
        assert_eq!(ec.cache_max_mb, 500);
        assert_eq!(ec.cache_sweep_interval_secs, 600);
    }

    #[test]
    fn ocr_config_defaults_apply_when_only_enabled_set() {
        // The five profile-seeded knobs (worker_interval_secs,
        // jobs_per_tick, adaptive_throttle, nice_level, io_class_idle)
        // come from the resolved ImpactProfile (default level = High).
        // The remaining six keep OcrConfig::default() values.
        let cfg = Config::from_toml_str("[ocr]\nenabled = true\n").expect("parse");
        assert!(cfg.ocr.enabled);
        assert!(cfg.ocr.languages.is_empty());
        assert_eq!(cfg.ocr.max_pages_per_pdf, None);
        assert_eq!(cfg.ocr.min_image_side_px, 200);
        assert_eq!(cfg.ocr.timeout_secs, 30);
        assert_eq!(cfg.ocr.worker_interval_secs, 1);
        assert_eq!(cfg.ocr.jobs_per_tick, 100);
        assert!(!cfg.ocr.adaptive_throttle);
        assert!((cfg.ocr.max_cpu_pressure_avg10 - 10.0).abs() < f32::EPSILON);
        assert_eq!(cfg.ocr.nice_level, 5);
        assert!(!cfg.ocr.io_class_idle);
    }

    #[test]
    fn ocr_config_max_pages_none_when_omitted() {
        let cfg = Config::from_toml_str("[ocr]\nenabled = true\n").unwrap();
        assert_eq!(cfg.ocr.max_pages_per_pdf, None);
        let cfg2 = Config::from_toml_str("[ocr]\nmax_pages_per_pdf = 5\n").unwrap();
        assert_eq!(cfg2.ocr.max_pages_per_pdf, Some(5));
    }

    #[test]
    fn ocr_config_max_pages_zero_normalized_to_none() {
        let cfg = Config::from_toml_str("[ocr]\nmax_pages_per_pdf = 0\n").unwrap();
        assert_eq!(cfg.ocr.max_pages_per_pdf, None);
    }

    #[test]
    fn ocr_config_worker_interval_zero_clamped_to_one() {
        let cfg = Config::from_toml_str("[ocr]\nworker_interval_secs = 0\n").unwrap();
        assert_eq!(cfg.ocr.worker_interval_secs, 1);
    }

    #[test]
    fn ocr_config_jobs_per_tick_zero_clamped_to_one() {
        let cfg = Config::from_toml_str("[ocr]\njobs_per_tick = 0\n").unwrap();
        assert_eq!(cfg.ocr.jobs_per_tick, 1);
    }

    #[test]
    fn ocr_config_nice_out_of_range_clamped() {
        let cfg_low = Config::from_toml_str("[ocr]\nnice_level = -5\n").unwrap();
        assert_eq!(cfg_low.ocr.nice_level, 0);
        let cfg_high = Config::from_toml_str("[ocr]\nnice_level = 25\n").unwrap();
        assert_eq!(cfg_high.ocr.nice_level, 19);
        let cfg_ok = Config::from_toml_str("[ocr]\nnice_level = 10\n").unwrap();
        assert_eq!(cfg_ok.ocr.nice_level, 10);
    }

    #[test]
    fn extract_config_parsed_from_toml() {
        let cfg = Config::from_toml_str(
            "[extract]\ncache_max_mb = 1024\ncache_sweep_interval_secs = 120\n",
        )
        .unwrap();
        assert_eq!(cfg.extract.cache_max_mb, 1024);
        assert_eq!(cfg.extract.cache_sweep_interval_secs, 120);
    }

    #[test]
    fn extract_and_ocr_sections_not_treated_as_plugin_sections() {
        let cfg = Config::from_toml_str("[extract]\ncache_max_mb = 100\n[ocr]\nenabled = true\n")
            .unwrap();
        assert!(!cfg.plugin_sections.contains_key("extract"));
        assert!(!cfg.plugin_sections.contains_key("ocr"));
    }

    #[test]
    fn persist_impact_level_preserves_comments_and_unrelated_keys() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg_path = tmp.path().join("lixun/config.toml");
        std::fs::create_dir_all(cfg_path.parent().unwrap()).unwrap();
        let fixture = "\
# top-of-file comment kept verbatim
max_file_size_mb = 50

[ranking]
# preserved comment in [ranking]
apps = 1.5

[impact]
level = \"high\"
follow_battery = false
";
        std::fs::write(&cfg_path, fixture).unwrap();

        let written =
            Config::persist_impact_level_at(cfg_path.clone(), SystemImpact::Low).expect("persist");
        assert_eq!(written, cfg_path);

        let after = std::fs::read_to_string(&cfg_path).unwrap();
        assert!(
            after.contains("# top-of-file comment kept verbatim"),
            "top-of-file comment must survive: {after}"
        );
        assert!(
            after.contains("# preserved comment in [ranking]"),
            "[ranking] comment must survive: {after}"
        );
        assert!(after.contains("apps = 1.5"));
        assert!(after.contains("max_file_size_mb = 50"));
        assert!(after.contains("level = \"low\""));
        assert!(
            after.contains("follow_battery = false"),
            "unrelated [impact] key must survive: {after}"
        );

        let parsed = Config::from_toml_str(&after).expect("parse after persist");
        assert_eq!(parsed.impact.level, SystemImpact::Low);
        assert_eq!(parsed.ranking_apps, 1.5);
    }

    #[test]
    fn persist_impact_level_creates_minimal_file_when_absent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg_path = tmp.path().join("lixun/config.toml");
        assert!(!cfg_path.exists());
        let written = Config::persist_impact_level_at(cfg_path.clone(), SystemImpact::Medium)
            .expect("persist");
        assert_eq!(written, cfg_path);
        let after = std::fs::read_to_string(&cfg_path).unwrap();
        assert!(after.contains("[impact]"));
        assert!(after.contains("level = \"medium\""));
    }

    #[test]
    fn impact_level_fromstr_rejects_bogus_value() {
        let err = "BOGUS".parse::<SystemImpact>().unwrap_err();
        assert!(err.contains("invalid level"));
        assert!(err.contains("unlimited, high, medium, low"));
    }

    #[test]
    fn arc_swap_observes_new_profile_after_store() {
        // Simulates the daemon-side hot reload: build an ArcSwap with the
        // High profile, swap it for Low, ensure the next load() observes
        // the new values without rebuilding the swap.
        use arc_swap::ArcSwap;
        use std::sync::Arc as StdArc;

        let initial = ImpactProfile::from_level(SystemImpact::High, 8);
        let swap: StdArc<ArcSwap<ImpactProfile>> =
            StdArc::new(ArcSwap::from_pointee(initial.clone()));
        assert_eq!(swap.load().level, SystemImpact::High);
        assert_eq!(swap.load().ocr_jobs_per_tick, 100);

        let new_profile = ImpactProfile::from_level(SystemImpact::Low, 8);
        swap.store(StdArc::new(new_profile.clone()));

        let observed = swap.load_full();
        assert_eq!(observed.level, SystemImpact::Low);
        assert_eq!(observed.ocr_jobs_per_tick, 5);
        assert_eq!(observed.daemon_nice, 10);
        assert!(observed.daemon_sched_idle);
        assert_eq!(observed.ocr_worker_interval.as_secs(), 30);
    }
}
