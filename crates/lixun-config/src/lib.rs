//! Configuration — ~/.config/lixun/config.toml

use anyhow::Result;
use lixun_core::{ImpactProfile, SystemImpact};
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

const KNOWN_TOP_LEVEL_KEYS: &[&str] = &[
    "core",
    "ranking",
    "keybindings",
    "preview",
    "gui",
    "extract",
    "ocr",
    "impact",
];

/// Top-level keys that used to live directly under the document root
/// but now belong under `[core]`. The loader detects each one at parse
/// time and emits a `tracing::warn!` so the user can migrate. The
/// legacy values are not honoured and are removed from the raw
/// document before the unknown-key sweep so they do not leak into
/// [`Config::plugin_sections`].
/// Known keys per built-in section, used by the warn-and-continue
/// unknown-key sweep in [`Config::from_toml_str`] (O2). `[preview]`
/// is absent on purpose: its unknown keys are per-plugin sections
/// preserved verbatim in `PreviewConfig::plugin_sections`.
const KNOWN_SECTION_KEYS: &[(&str, &[&str])] = &[
    (
        "core",
        &[
            "roots",
            "exclude",
            "exclude_regex",
            "max_file_size_mb",
            "extractor_timeout_secs",
            "max_results",
        ],
    ),
    (
        "gui",
        &[
            "width_percent",
            "height_percent",
            "max_width_px",
            "max_height_px",
            "preview_width_percent",
            "preview_height_percent",
            "preview_max_width_px",
            "preview_max_height_px",
            "preview_placement",
            "blur",
            "opacity",
            "show_recents",
            "theme",
            "matugen",
            "matugen_colors_path",
        ],
    ),
    (
        "ranking",
        &[
            "apps",
            "files",
            "mail",
            "attachments",
            "prefix_boost",
            "acronym_boost",
            "recency_weight",
            "recency_tau_days",
            "frecency_alpha",
            "latch_weight",
            "latch_cap",
            "total_multiplier_cap",
            "top_hit_min_confidence",
            "top_hit_min_margin",
            "strong_latch_threshold",
        ],
    ),
    (
        "keybindings",
        &[
            "close",
            "primary_action",
            "secondary_action",
            "copy",
            "quick_look",
            "quick_look_alt",
            "history_up",
            "next_result",
            "previous_result",
            "next_category",
            "previous_category",
            "filter_all",
            "filter_apps",
            "filter_files",
            "filter_mail",
            "filter_attachments",
            "global_toggle",
            "reset_gui_position",
        ],
    ),
    (
        "extract",
        &[
            "cache_max_mb",
            "cache_sweep_interval_secs",
            "extractor_max_decompress_mb",
        ],
    ),
    (
        "ocr",
        &[
            "enabled",
            "languages",
            "max_pages_per_pdf",
            "min_image_side_px",
            "timeout_secs",
            "worker_interval_secs",
            "jobs_per_tick",
            "adaptive_throttle",
            "max_cpu_pressure_avg10",
            "nice_level",
            "io_class_idle",
            "content_filter",
        ],
    ),
    ("impact", &["level", "follow_battery", "on_battery_level"]),
];

const LEGACY_TOP_LEVEL_KEYS: &[&str] = &[
    "roots",
    "exclude",
    "exclude_regex",
    "max_file_size_mb",
    "extractor_timeout_secs",
];

#[derive(Debug, Deserialize)]
struct ConfigToml {
    core: Option<CoreToml>,
    ranking: Option<RankingToml>,
    keybindings: Option<KeybindingsToml>,
    preview: Option<PreviewToml>,
    gui: Option<GuiToml>,
    extract: Option<ExtractToml>,
    ocr: Option<OcrToml>,
    impact: Option<ImpactToml>,
}

/// Wire-format mirror of the `[core]` table. Hosts the indexer's
/// root list, substring and regex excludes, the extraction file-size
/// cap, the extractor timeout, and the search result-count cap. Every
/// field is optional so an absent or partially-populated table falls
/// back to [`Config::default`] piecewise. Unknown keys inside `[core]`
/// are warned about (not hard errors) by the uniform unknown-key sweep
/// in [`Config::from_toml_str`] — a typo must never take the whole
/// daemon down (O2).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CoreToml {
    roots: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    exclude_regex: Option<Vec<String>>,
    max_file_size_mb: Option<u64>,
    extractor_timeout_secs: Option<u64>,
    max_results: Option<u32>,
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

/// Parse-side mirror of [`ExtractConfig`]. All knobs are optional so
/// defaults and impact-profile seeds survive when the operator has not
/// pinned explicit values.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ExtractToml {
    cache_max_mb: Option<u64>,
    cache_sweep_interval_secs: Option<u64>,
    extractor_max_decompress_mb: Option<u64>,
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
    /// How the preview window is placed: `"window"` (default) for a
    /// normal WM-managed toplevel, `"overlay"` for a compositor-
    /// overlay layer surface pinned beside the launcher. See
    /// [`PreviewPlacement`].
    preview_placement: Option<PreviewPlacement>,
    blur: Option<BlurToml>,
    /// Launcher surface alpha in the blur-on state, `0.0..=1.0`
    /// (clamped). Threaded into the style pipeline as the
    /// `--lixun-surface-alpha` custom property. Default 0.70 —
    /// the value the shipped stylesheet uses.
    opacity: Option<f64>,
    /// Show the frecency-derived "Recent" section when the launcher
    /// opens with an empty query (O4). Default true.
    show_recents: Option<bool>,
    theme: Option<String>,
    /// Matugen integration toggle. Mapped to [`GuiMatugenConfig::enabled`]
    /// on the resolved side. Absent or missing leaves the default
    /// (`false`) in place; matugen integration is opt-in because it
    /// requires the user to install matugen and configure
    /// `[templates.lixun]` externally.
    matugen: Option<bool>,
    /// Optional override of the path lixun watches for matugen output.
    /// Mapped to [`GuiMatugenConfig::colors_path`]. Tilde expansion is
    /// applied. Defaults to `${config_dir}/lixun/colors.css`.
    matugen_colors_path: Option<String>,
}

/// Text-extraction configuration. Cache entries live under
/// `~/.cache/lixun/extract/v1/`.
/// Cache sweep is a tick-scheduled LRU eviction keyed by file mtime.
/// `cache_max_mb = 0` disables the sweep tick (valid config, no warn).
#[derive(Debug, Clone, Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct ExtractConfig {
    #[serde(default = "default_cache_max_mb")]
    pub cache_max_mb: u64,
    #[serde(default = "default_cache_sweep_interval_secs")]
    pub cache_sweep_interval_secs: u64,
    #[serde(default = "default_extractor_max_decompress_mb")]
    pub extractor_max_decompress_mb: u64,
}

impl Default for ExtractConfig {
    fn default() -> Self {
        Self {
            cache_max_mb: default_cache_max_mb(),
            cache_sweep_interval_secs: default_cache_sweep_interval_secs(),
            extractor_max_decompress_mb: default_extractor_max_decompress_mb(),
        }
    }
}

fn default_cache_max_mb() -> u64 {
    500
}
fn default_cache_sweep_interval_secs() -> u64 {
    600
}
fn default_extractor_max_decompress_mb() -> u64 {
    64
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
    quick_look_alt: Option<String>,
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
    reset_gui_position: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Keybindings {
    pub close: String,
    pub primary_action: String,
    pub secondary_action: String,
    pub copy: String,
    pub quick_look: String,
    /// Quick Look chord that works while the search entry owns focus
    /// (bare `quick_look` Space must stay typeable there).
    pub quick_look_alt: String,
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
    pub reset_gui_position: String,
}

pub struct Config {
    pub roots: Vec<PathBuf>,
    pub exclude: Vec<String>,
    pub exclude_regex: Vec<regex::Regex>,
    pub max_file_size_mb: u64,
    pub extractor_timeout_secs: u64,
    pub max_results: u32,
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
}

/// Background-blur policy for the launcher surface (V2b).
///
/// * `Auto` — probe for a compositor blur protocol (KDE/Plasma blur,
///   Hyprland layerrule detection via `HYPRLAND_INSTANCE_SIGNATURE`);
///   fall back to the opaque no-blur skin when none is found.
/// * `Compositor` — the operator asserts compositor-side blur is
///   configured (e.g. a Hyprland `layerrule = blur`); the GUI keeps
///   the translucent skin and never forces the no-blur class, even
///   when it cannot detect a blur protocol itself.
/// * `Off` — no blur attach, opaque no-blur skin.
///
/// Wire back-compat: `blur = true` parses as `Auto`, `blur = false`
/// as `Off`, alongside the string forms `"auto" | "compositor" |
/// "off"`.
/// Shipped stylesheet surface alpha; `[gui] opacity` defaults to it.
pub const DEFAULT_SURFACE_OPACITY: f64 = 0.70;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BlurMode {
    #[default]
    Auto,
    Compositor,
    Off,
}

impl BlurMode {
    /// Whether any blur attach should be attempted at all.
    pub fn wants_blur(&self) -> bool {
        !matches!(self, BlurMode::Off)
    }
}

/// Parse-side mirror of [`BlurMode`]: accepts the legacy bool and
/// the tri-state string. Untagged so both spellings coexist.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(untagged)]
enum BlurToml {
    Legacy(bool),
    Mode(BlurModeStr),
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum BlurModeStr {
    Auto,
    Compositor,
    Off,
}

impl From<BlurToml> for BlurMode {
    fn from(t: BlurToml) -> Self {
        match t {
            BlurToml::Legacy(true) => BlurMode::Auto,
            BlurToml::Legacy(false) => BlurMode::Off,
            BlurToml::Mode(BlurModeStr::Auto) => BlurMode::Auto,
            BlurToml::Mode(BlurModeStr::Compositor) => BlurMode::Compositor,
            BlurToml::Mode(BlurModeStr::Off) => BlurMode::Off,
        }
    }
}

/// `[gui] preview_placement` — who owns the preview window's
/// position and stacking.
///
/// * `Window` (default): the preview is a normal WM-managed
///   xdg-toplevel — freely draggable, tileable, and targetable by
///   WM rules (app-id `app.lixun.preview`). Tradeoff: the launcher
///   is an overlay-layer surface and always stacks above the
///   preview wherever the two overlap; the launcher tucks against
///   the left screen edge during preview mode to minimise the
///   initial overlap.
/// * `Overlay`: the preview joins the launcher's compositor-overlay
///   layer, anchored beside it at the right screen edge. Deterministic
///   side-by-side layout; recommended on tiling compositors
///   (sway/Hyprland) where a normal window would get tiled on every
///   preview open. Requires layer-shell support at runtime; without
///   it the GUI behaves as `Window`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PreviewPlacement {
    #[default]
    Window,
    Overlay,
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
    /// Preview window placement mode. See [`PreviewPlacement`].
    pub preview_placement: PreviewPlacement,
    pub blur: BlurMode,
    /// Surface alpha for the blur-on state, `0.0..=1.0`. See
    /// `[gui] opacity`.
    pub opacity: f64,
    /// Render the "Recent" (frecency) section on an empty query.
    pub show_recents: bool,
    /// Active theme. Looked up as `${config_dir}/lixun/themes/<name>/style.css`.
    /// When `None` the GUI falls back to the built-in stylesheet embedded at
    /// compile time from `crates/lixun-gui/style.css` (plus the optional
    /// user-wide override at `${config_dir}/lixun/style.css`).
    pub theme: Option<String>,
    pub matugen: GuiMatugenConfig,
}

/// Matugen-driven color override layer. When `enabled` is `true`
/// the GUI registers an extra `gtk::CssProvider` at
/// `APPLICATION + 3` — the top of the cascade — loading
/// `colors_path` whenever it exists or is updated. The file is
/// expected to contain `--lixun-*` custom property declarations
/// rendered by matugen from the lixun-owned template at
/// `/usr/share/lixun/matugen/lixun-colors.css.tmpl` (or the in-tree
/// copy at `crates/lixun-gui/assets/matugen/lixun-colors.css.tmpl`).
///
/// Wire format (under `[gui]` in `config.toml`):
/// `matugen = true|false` and `matugen_colors_path = "..."`.
///
/// `enabled = false` makes the layer a true no-op: the provider is
/// never registered with GTK, the file is never loaded, and no
/// watcher event is emitted even if the file exists.
#[derive(Debug, Clone)]
pub struct GuiMatugenConfig {
    pub enabled: bool,
    pub colors_path: PathBuf,
}

impl Default for GuiMatugenConfig {
    fn default() -> Self {
        // Opt-in: enabling matugen requires the user to install matugen and
        // wire up `[templates.lixun]` in their matugen config first. Defaulting
        // to `false` leaves the launcher visually unchanged for users who
        // never set up the integration; setting `matugen = true` under `[gui]`
        // in `~/.config/lixun/config.toml` activates the recolour path.
        Self {
            enabled: false,
            colors_path: config_dir().join("lixun").join("colors.css"),
        }
    }
}

impl Default for GuiConfig {
    fn default() -> Self {
        Self {
            width_percent: 40,
            height_percent: 60,
            max_width_px: 900,
            max_height_px: 800,
            // Spotlight parity (P1): the preview opens beside the
            // launcher at roughly half the monitor width instead of
            // covering 80% of the screen, so launcher + results stay
            // visible and arrow-scrub is reachable during preview.
            preview_width_percent: 50,
            preview_height_percent: 70,
            preview_max_width_px: 1400,
            preview_max_height_px: 1200,
            preview_placement: PreviewPlacement::default(),
            blur: BlurMode::Auto,
            opacity: DEFAULT_SURFACE_OPACITY,
            show_recents: true,
            theme: None,
            matugen: GuiMatugenConfig::default(),
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
            max_results: 30,
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
            quick_look_alt: "<Ctrl>space".into(),
            history_up: "Up".into(),
            next_result: "Down".into(),
            previous_result: "Up".into(),
            next_category: "<Ctrl>Down".into(),
            previous_category: "<Ctrl>Up".into(),
            filter_all: "<Ctrl>grave".into(),
            filter_apps: "<Ctrl>1".into(),
            filter_files: "<Ctrl>2".into(),
            filter_mail: "<Ctrl>3".into(),
            filter_attachments: "<Ctrl>4".into(),
            global_toggle: "Super+space".into(),
            reset_gui_position: "<Ctrl>0".into(),
        }
    }
}

/// Modifier bitmask + lowercased keysym name for a GTK accelerator
/// string, used only for duplicate detection. Mirrors the subset of
/// `gtk::accelerator_parse` syntax the GUI dispatches (`<Ctrl>c`,
/// `<Shift>Return`, `<Ctrl><Shift>0`, bare keysym names) without
/// linking GTK — lixun-config is shared by the headless daemon and
/// CLI. Returns `None` for strings that are not GTK accelerators
/// (e.g. the XDG-spec `global_toggle` form `Super+space`).
fn normalize_accel(accel: &str) -> Option<(u8, String)> {
    let mut mods: u8 = 0;
    let mut rest = accel.trim();
    while let Some(stripped) = rest.strip_prefix('<') {
        let (name, tail) = stripped.split_once('>')?;
        mods |= match name.to_ascii_lowercase().as_str() {
            "shift" => 1,
            "ctrl" | "control" | "primary" => 2,
            "alt" => 4,
            "super" => 8,
            "meta" => 16,
            "hyper" => 32,
            _ => return None,
        };
        rest = tail;
    }
    if rest.is_empty() || rest.contains(['<', '>', '+', ' ']) {
        return None;
    }
    Some((mods, rest.to_ascii_lowercase()))
}

/// One warning line per pair of keybindings that resolve to the same
/// (modifiers, key) accelerator. The GUI dispatches accels in a fixed
/// order, so of two colliding actions one silently never fires — warn
/// naming both, never reject. The `previous_result` / `history_up`
/// pair is exempt: both default to `Up` on purpose and are dispatched
/// in mutually exclusive contexts (history only fires while the
/// search entry is empty). `global_toggle` is excluded entirely — it
/// uses XDG shortcut syntax and is bound by the compositor, not the
/// launcher window.
fn duplicate_accel_warnings(kb: &Keybindings) -> Vec<String> {
    const EXEMPT: [(&str, &str); 1] = [("history_up", "previous_result")];
    let actions: [(&str, &str); 17] = [
        ("close", &kb.close),
        ("primary_action", &kb.primary_action),
        ("secondary_action", &kb.secondary_action),
        ("copy", &kb.copy),
        ("quick_look", &kb.quick_look),
        ("quick_look_alt", &kb.quick_look_alt),
        ("history_up", &kb.history_up),
        ("next_result", &kb.next_result),
        ("previous_result", &kb.previous_result),
        ("next_category", &kb.next_category),
        ("previous_category", &kb.previous_category),
        ("filter_all", &kb.filter_all),
        ("filter_apps", &kb.filter_apps),
        ("filter_files", &kb.filter_files),
        ("filter_mail", &kb.filter_mail),
        ("filter_attachments", &kb.filter_attachments),
        ("reset_gui_position", &kb.reset_gui_position),
    ];
    type NormalizedAction<'a> = (&'a str, &'a str, Option<(u8, String)>);
    let normalized: Vec<NormalizedAction> = actions
        .iter()
        .map(|(name, accel)| (*name, *accel, normalize_accel(accel)))
        .collect();
    let mut warnings = Vec::new();
    for (i, (name_a, accel_a, norm_a)) in normalized.iter().enumerate() {
        let Some(norm_a) = norm_a else { continue };
        for (name_b, _, norm_b) in &normalized[i + 1..] {
            if norm_b.as_ref() != Some(norm_a) {
                continue;
            }
            if EXEMPT.contains(&(name_a, name_b)) || EXEMPT.contains(&(name_b, name_a)) {
                continue;
            }
            warnings.push(format!(
                "config: [keybindings] `{name_a}` and `{name_b}` both bind \"{accel_a}\"; \
                 dispatch order decides which one fires — rebind one of them"
            ));
        }
    }
    warnings
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

    /// Resolved path of the user config file, plus whether it exists
    /// on disk. Exposed so hosts (daemon status, CLI) can report which
    /// file is in effect vs. "defaults (no file)".
    pub fn user_config_path() -> (PathBuf, bool) {
        let path = config_dir().join("lixun/config.toml");
        let exists = path.exists();
        (path, exists)
    }

    /// Load the config, falling back to built-in defaults instead of
    /// failing when the file is unreadable or does not parse (O2): a
    /// config typo must degrade search defaults, never kill the
    /// daemon — and with it the global hotkey. Returns the config
    /// plus the load error string (if any) so the daemon can carry it
    /// into `Response::Status` for the CLI/GUI to surface.
    pub fn load_or_default() -> (Self, Option<String>) {
        match Self::load() {
            Ok(cfg) => (cfg, None),
            Err(e) => {
                let (path, _) = Self::user_config_path();
                let msg = format!("config: {} failed to load: {e:#}", path.display());
                tracing::error!("{msg}; continuing with built-in defaults");
                (Self::default(), Some(msg))
            }
        }
    }

    pub fn from_toml_str(content: &str) -> Result<Self> {
        let mut cfg = Self::default();
        let parsed: ConfigToml = toml::from_str(content)?;

        let user_set_max_file_size_mb = parsed
            .core
            .as_ref()
            .and_then(|c| c.max_file_size_mb)
            .is_some();
        if let Some(core) = parsed.core {
            if let Some(roots) = core.roots {
                cfg.roots = roots.iter().map(|s| expand_tilde(s)).collect();
            }
            if let Some(extra) = core.exclude {
                cfg.exclude.extend(extra);
            }
            if let Some(patterns) = core.exclude_regex {
                for pat in patterns {
                    match regex::Regex::new(&pat) {
                        Ok(r) => cfg.exclude_regex.push(r),
                        Err(e) => tracing::error!(
                            "config: skipping invalid [core].exclude_regex '{}': {}",
                            pat,
                            e
                        ),
                    }
                }
            }
            if let Some(max) = core.max_file_size_mb {
                cfg.max_file_size_mb = max;
            }
            if let Some(timeout) = core.extractor_timeout_secs {
                cfg.extractor_timeout_secs = timeout;
            }
            if let Some(n) = core.max_results {
                cfg.max_results = n;
            }
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
            if let Some(v) = bindings.quick_look_alt {
                cfg.keybindings.quick_look_alt = v;
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
            if let Some(v) = bindings.reset_gui_position {
                cfg.keybindings.reset_gui_position = v;
            }
        }
        // Warn-only duplicate scan over the resolved bindings: a
        // collision (e.g. filter_all = "<Ctrl>0" vs the default
        // reset_gui_position = "<Ctrl>0") means one action silently
        // never fires, decided by GUI dispatch order.
        for warning in duplicate_accel_warnings(&cfg.keybindings) {
            tracing::warn!("{warning}");
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
            if let Some(v) = gui.preview_placement {
                cfg.gui.preview_placement = v;
            }
            if let Some(v) = gui.blur {
                cfg.gui.blur = v.into();
            }
            if let Some(v) = gui.opacity {
                let clamped = v.clamp(0.0, 1.0);
                if (clamped - v).abs() > f64::EPSILON {
                    tracing::warn!(
                        "[gui].opacity = {} out of 0.0..=1.0, clamped to {}",
                        v,
                        clamped
                    );
                }
                cfg.gui.opacity = clamped;
            }
            if let Some(v) = gui.show_recents {
                cfg.gui.show_recents = v;
            }
            if let Some(theme) = gui.theme {
                let trimmed = theme.trim();
                cfg.gui.theme = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                };
            }
            if let Some(v) = gui.matugen {
                cfg.gui.matugen.enabled = v;
            }
            if let Some(path) = gui.matugen_colors_path {
                let trimmed = path.trim();
                if !trimmed.is_empty() {
                    cfg.gui.matugen.colors_path = expand_tilde(trimmed);
                }
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
            if let Some(v) = extract_toml.extractor_max_decompress_mb {
                cfg.extract.extractor_max_decompress_mb = v;
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
            // Uniform warn-and-continue unknown-key sweep (O2): a
            // misspelled key inside any built-in section is reported
            // once and ignored, matching the legacy-top-level-key
            // pattern below. Previously `[core]` hard-errored via
            // deny_unknown_fields while `[gui]`/`[ranking]` silently
            // dropped typos.
            for (section, known_keys) in KNOWN_SECTION_KEYS {
                if let Some(toml::Value::Table(table)) = top.get(*section) {
                    for key in table.keys() {
                        if !known_keys.contains(&key.as_str()) {
                            tracing::warn!(
                                "config: unknown key `{key}` in [{section}]; ignored. Typo?"
                            );
                        }
                    }
                }
            }
            if let Some(toml::Value::Table(preview_table)) = top.remove("preview") {
                for (key, value) in preview_table {
                    if known_preview.contains(key.as_str()) {
                        continue;
                    }
                    cfg.preview.plugin_sections.insert(key, value);
                }
            }
            // Must precede the `plugin_sections` sweep below;
            // otherwise legacy keys would be misdispatched to
            // non-existent plugin factories instead of warned about.
            for legacy_key in LEGACY_TOP_LEVEL_KEYS {
                if top.remove(*legacy_key).is_some() {
                    tracing::warn!(
                        field = *legacy_key,
                        "config: top-level `{}` is no longer supported; move it under [core] in ~/.config/lixun/config.toml. The legacy value has been ignored.",
                        legacy_key
                    );
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
        if !(1..=1000).contains(&self.max_results) {
            let clamped = self.max_results.clamp(1, 1000);
            tracing::warn!(
                "[core].max_results = {} out of 1..=1000, clamped to {}",
                self.max_results,
                clamped
            );
            self.max_results = clamped;
        }
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
    fn blur_tristate_parses_bool_and_strings() {
        // Legacy booleans keep working (V2b back-compat).
        let cfg = Config::from_toml_str("[gui]\nblur = true\n").unwrap();
        assert_eq!(cfg.gui.blur, BlurMode::Auto);
        let cfg = Config::from_toml_str("[gui]\nblur = false\n").unwrap();
        assert_eq!(cfg.gui.blur, BlurMode::Off);
        let cfg = Config::from_toml_str("[gui]\nblur = \"auto\"\n").unwrap();
        assert_eq!(cfg.gui.blur, BlurMode::Auto);
        let cfg = Config::from_toml_str("[gui]\nblur = \"compositor\"\n").unwrap();
        assert_eq!(cfg.gui.blur, BlurMode::Compositor);
        let cfg = Config::from_toml_str("[gui]\nblur = \"off\"\n").unwrap();
        assert_eq!(cfg.gui.blur, BlurMode::Off);
        assert!(BlurMode::Compositor.wants_blur());
        assert!(!BlurMode::Off.wants_blur());
    }

    #[test]
    fn gui_opacity_defaults_and_clamps() {
        let cfg = Config::default();
        assert!((cfg.gui.opacity - DEFAULT_SURFACE_OPACITY).abs() < 1e-9);
        let cfg = Config::from_toml_str("[gui]\nopacity = 0.5\n").unwrap();
        assert!((cfg.gui.opacity - 0.5).abs() < 1e-9);
        let cfg = Config::from_toml_str("[gui]\nopacity = 1.5\n").unwrap();
        assert!((cfg.gui.opacity - 1.0).abs() < 1e-9);
    }

    #[test]
    fn show_recents_defaults_true_and_parses() {
        assert!(Config::default().gui.show_recents);
        let cfg = Config::from_toml_str("[gui]\nshow_recents = false\n").unwrap();
        assert!(!cfg.gui.show_recents);
    }

    #[test]
    fn unknown_keys_warn_and_continue_everywhere() {
        // O2: a typo in ANY built-in section must not abort the parse
        // (the old [core] deny_unknown_fields hard-errored).
        let cfg = Config::from_toml_str(
            "[core]\nmax_resuls = 10\n[gui]\nwidht_percent = 30\n[ranking]\ntypo = 1\n",
        )
        .expect("typos must warn, not error");
        // The misspelled keys are ignored; defaults survive.
        assert_eq!(cfg.max_results, Config::default().max_results);
        assert_eq!(cfg.gui.width_percent, Config::default().gui.width_percent);
    }

    #[test]
    fn from_toml_str_still_rejects_syntax_errors() {
        // Structural TOML damage is a parse error (the daemon then
        // falls back to defaults via load_or_default).
        assert!(Config::from_toml_str("[core\nroots = 3").is_err());
    }

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
            extractor_max_decompress_mb: 128,
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
            "[extract]\ncache_max_mb = 1024\ncache_sweep_interval_secs = 120\nextractor_max_decompress_mb = 128\n",
        )
        .unwrap();
        assert_eq!(cfg.extract.cache_max_mb, 1024);
        assert_eq!(cfg.extract.cache_sweep_interval_secs, 120);
        assert_eq!(cfg.extract.extractor_max_decompress_mb, 128);
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
[core]
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

    #[test]
    fn gui_theme_parses_non_empty_string() {
        let cfg = Config::from_toml_str("[gui]\ntheme = \"midnight\"\n").unwrap();
        assert_eq!(cfg.gui.theme.as_deref(), Some("midnight"));
    }

    #[test]
    fn gui_theme_empty_string_becomes_none() {
        let cfg = Config::from_toml_str("[gui]\ntheme = \"\"\n").unwrap();
        assert!(cfg.gui.theme.is_none());
    }

    #[test]
    fn gui_theme_whitespace_only_becomes_none() {
        let cfg = Config::from_toml_str("[gui]\ntheme = \"   \"\n").unwrap();
        assert!(cfg.gui.theme.is_none());
    }

    #[test]
    fn gui_theme_absent_defaults_to_none() {
        let cfg = Config::from_toml_str("").unwrap();
        assert!(cfg.gui.theme.is_none());
    }

    #[test]
    fn gui_theme_is_trimmed() {
        let cfg = Config::from_toml_str("[gui]\ntheme = \"  midnight  \"\n").unwrap();
        assert_eq!(cfg.gui.theme.as_deref(), Some("midnight"));
    }

    #[test]
    fn default_keybindings_have_no_duplicate_accels() {
        // previous_result and history_up share "Up" by design (mutually
        // exclusive dispatch contexts) and must not be reported.
        let warnings = duplicate_accel_warnings(&Keybindings::default());
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    }

    #[test]
    fn duplicate_accel_names_both_actions() {
        let kb = Keybindings {
            filter_all: "<Ctrl>0".into(),
            ..Keybindings::default()
        };
        let warnings = duplicate_accel_warnings(&kb);
        assert_eq!(warnings.len(), 1, "warnings: {warnings:?}");
        assert!(warnings[0].contains("filter_all"));
        assert!(warnings[0].contains("reset_gui_position"));
        assert!(warnings[0].contains("<Ctrl>0"));
    }

    #[test]
    fn duplicate_accel_detects_modifier_aliases() {
        // <Control>c, <Primary>c, and <Ctrl>c all normalize to the same
        // (mods, key) pair the way gtk::accelerator_parse would.
        let kb = Keybindings {
            copy: "<Control>c".into(),
            quick_look: "<Primary>C".into(),
            ..Keybindings::default()
        };
        let warnings = duplicate_accel_warnings(&kb);
        assert_eq!(warnings.len(), 1, "warnings: {warnings:?}");
        assert!(warnings[0].contains("copy"));
        assert!(warnings[0].contains("quick_look"));
    }

    #[test]
    fn non_accel_strings_never_collide() {
        // XDG-spec shortcut syntax (`Super+space`) is not a GTK accel
        // and must not be reported against `quick_look = "space"`.
        assert_eq!(normalize_accel("Super+space"), None);
        assert_eq!(normalize_accel("<Bogus>x"), None);
        assert_eq!(normalize_accel(""), None);
        assert_eq!(
            normalize_accel("<Ctrl><Shift>Return"),
            Some((3, "return".into()))
        );
    }

    /// Drift guard: docs/config.example.toml is the single config
    /// reference, so it must always parse cleanly against the current
    /// schema — no legacy top-level keys, no unknown sections, no
    /// colliding keybindings.
    #[test]
    fn example_config_stays_in_sync_with_schema() {
        const EXAMPLE: &str = include_str!("../../../docs/config.example.toml");
        let cfg = Config::from_toml_str(EXAMPLE).expect("docs/config.example.toml must parse");

        let raw: toml::Value = toml::from_str(EXAMPLE).unwrap();
        let top = raw.as_table().unwrap();
        for key in LEGACY_TOP_LEVEL_KEYS {
            assert!(
                !top.contains_key(*key),
                "docs/config.example.toml uses removed legacy top-level key `{key}`"
            );
        }
        // Every top-level table must be a known host section or one of
        // the plugin stanzas the example currently documents. This list
        // mirrors the doc file, not host behaviour — the daemon still
        // discovers factories purely via inventory.
        let documented_plugin_sections = ["calculator", "shell", "semantic"];
        for key in top.keys() {
            assert!(
                KNOWN_TOP_LEVEL_KEYS.contains(&key.as_str())
                    || documented_plugin_sections.contains(&key.as_str()),
                "unknown top-level section [{key}] in docs/config.example.toml \
                 (the daemon would warn `no factory registered` at startup)"
            );
        }

        assert!(
            duplicate_accel_warnings(&cfg.keybindings).is_empty(),
            "docs/config.example.toml documents colliding keybindings"
        );
        // The example's keybindings must show the shipped defaults.
        assert_eq!(cfg.keybindings.filter_all, "<Ctrl>grave");
        assert_eq!(cfg.keybindings.reset_gui_position, "<Ctrl>0");
    }
}
