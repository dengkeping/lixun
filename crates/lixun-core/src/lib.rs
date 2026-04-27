//! Lixun Core — shared types with no runtime dependencies.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub mod paths;

/// Categories of searchable items.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Category {
    App,
    File,
    Mail,
    Attachment,
    Calculator,
    Shell,
}

impl Category {
    pub fn as_str(&self) -> &'static str {
        match self {
            Category::App => "app",
            Category::File => "file",
            Category::Mail => "mail",
            Category::Attachment => "attachment",
            Category::Calculator => "calculator",
            Category::Shell => "shell",
        }
    }
}

/// Ranking configuration used by the index and daemon scoring layers.
///
/// Every field is a scalar weight with a neutral value (1.0 for
/// multipliers, 0.0 for alphas / recency weights) that disables the
/// corresponding signal. The host TOML parser in `lixun-daemon`
/// constructs this struct from `[ranking]` keys; callers without
/// config (tests, etc.) use [`RankingConfig::default`].
///
/// Category fields (`apps`, `files`, `mail`, `attachments`) are the
/// only fields used in Wave A Task 1. The remaining fields are
/// declared here ahead of their first use so that later Wave A
/// tasks (T3..T6) add scoring code without re-widening the struct or
/// breaking external callers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankingConfig {
    // Per-category multipliers — applied in `lixun-index::search`.
    pub apps: f32,
    pub files: f32,
    pub mail: f32,
    pub attachments: f32,

    // Forward-compatible stub fields. Defined now so the struct is
    // stable; first consumed by Wave A tasks T3..T6.
    pub prefix_boost: f32,
    pub acronym_boost: f32,
    pub recency_weight: f32,
    pub recency_tau_days: f32,
    pub frecency_alpha: f32,
    pub latch_weight: f32,
    pub latch_cap: f32,
    pub total_multiplier_cap: f32,
    pub top_hit_min_confidence: f32,
    pub top_hit_min_margin: f32,
    pub strong_latch_threshold: u32,

    // Proximity (Wave B T1) — PhraseQuery over title + title_terms.
    // `proximity_slop` is the max positional gap tolerated inside the
    // phrase; `proximity_boost` is the multiplicative weight applied to
    // each phrase sub-query via `BoostQuery`. Boost of `0.0` disables the
    // contribution without removing the phrase subquery from the tree.
    pub proximity_slop: u32,
    pub proximity_boost: f32,

    // Coordination (Wave B T2) — rewards docs where every query token
    // matches the title. Applied as a stage-1 multiplier alongside
    // prefix/acronym/recency. Formula:
    //   coord_mult = 1 + coordination_boost / q^coordination_delta
    // active only when 2 <= q <= 3 and v == q (v = title matches).
    // Guards (q<2, q>3, v<q, analyzer missing) collapse to 1.0 no-op.
    pub coordination_boost: f32,
    pub coordination_delta: f32,
}

impl Default for RankingConfig {
    fn default() -> Self {
        Self {
            apps: 1.3,
            files: 1.2,
            mail: 1.0,
            attachments: 0.9,
            prefix_boost: 1.4,
            acronym_boost: 1.25,
            recency_weight: 0.2,
            recency_tau_days: 30.0,
            frecency_alpha: 0.1,
            latch_weight: 0.5,
            latch_cap: 3.0,
            total_multiplier_cap: 6.0,
            top_hit_min_confidence: 0.6,
            top_hit_min_margin: 1.3,
            strong_latch_threshold: 3,
            proximity_slop: 2,
            proximity_boost: 1.8,
            coordination_boost: 1.2,
            coordination_delta: 0.5,
        }
    }
}

impl RankingConfig {
    /// Per-category multiplier applied on top of the Tantivy BM25
    /// score inside `LixunIndex::search`.
    pub fn multiplier_for(&self, category: Category) -> f32 {
        match category {
            Category::App => self.apps,
            Category::File => self.files,
            Category::Mail => self.mail,
            Category::Attachment => self.attachments,
            Category::Calculator | Category::Shell => 1.0,
        }
    }
}

/// Stable document ID.
/// Format: `fs:<abspath>`, `app:<desktop-id>`, `mail:<gloda-id>`, `att:<mail-id>#<n>`
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DocId(pub String);

/// Action to perform on a hit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Action {
    /// Launch an application.
    Launch {
        exec: String,
        terminal: bool,
        desktop_id: Option<String>,
        desktop_file: Option<PathBuf>,
        working_dir: Option<PathBuf>,
    },
    /// Open a file with the default handler.
    OpenFile { path: PathBuf },
    /// Show file in file manager.
    ShowInFileManager { path: PathBuf },
    /// Extract an attachment to a temp file and open it.
    OpenAttachment {
        mbox_path: PathBuf,
        byte_offset: u64,
        length: u64,
        mime: String,
        encoding: String,
        suggested_filename: String,
    },
    /// Replace the current search query with this text (used for recent-query hits).
    ReplaceQuery { q: String },
    /// Execute an arbitrary command. Generic escape hatch for plugin sources.
    ///
    /// `terminal: true` asks the host to wrap the spawn in a terminal
    /// emulator (freedesktop XDG Default Terminal Execution; falls
    /// back to `$TERMINAL -e …` then `xterm -e …`) so stdout/stderr
    /// land in a visible window. Plain CLI commands (e.g. `ls`,
    /// `echo`) need this; GUI commands (e.g. `firefox`) do not.
    /// Defaults to `false` for backward wire compatibility with
    /// older clients.
    Exec {
        cmdline: Vec<String>,
        working_dir: Option<PathBuf>,
        #[serde(default)]
        terminal: bool,
    },
    /// Open an arbitrary URI via the OS (xdg-open on Linux). Generic
    /// primitive for URI-dispatchable actions (e.g. `mid:<message-id>`,
    /// `mailto:`, `https:`). Plugin-agnostic: the host does not know
    /// which application will handle the scheme.
    OpenUri { uri: String },
}

/// A single row context-menu item, GTK-free.
///
/// Sources describe what items their right-click menu exposes via
/// [`RowMenuDef`]; the host translates this into whatever native
/// menu model fits the platform (on Linux: `gio::Menu` +
/// `gtk::PopoverMenu`). Keeping this type plain-data preserves the
/// project invariant that plugin-specific UI logic never leaks
/// into host binaries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowMenuItem {
    /// User-visible label. Owned so sources can localize later
    /// without a string-interning dance.
    pub label: String,
    /// Verb the host dispatches when the item is activated.
    pub verb: RowMenuVerb,
    /// Visibility / enablement predicate evaluated per-hit on the
    /// host side. Keeps the menu shape stable across hits so the
    /// host can cache the translated menu by `source_instance`.
    #[serde(default)]
    pub visibility: RowMenuVisibility,
}

/// Verbs the host wires to SimpleActions on each row. Fixed
/// vocabulary to keep dispatch generic: a source picks verbs, the
/// host implements them once.
///
/// `Secondary` replaces the legacy `row.reveal` / "Show in
/// folder" naming; for file hits the host still reveals in the
/// file manager, for attachment hits it opens the parent mail,
/// etc. — whichever `Action` the source put in
/// [`Hit::secondary_action`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RowMenuVerb {
    /// Primary action (`Hit::action`).
    Open,
    /// Secondary action (`Hit::secondary_action`), rendered
    /// generically — the label itself tells the user what it does.
    Secondary,
    /// Copy the hit's canonical textual representation to the
    /// clipboard.
    Copy,
    /// QuickLook-style transient preview.
    QuickLook,
    /// Detailed info popover.
    Info,
}

/// When a [`RowMenuItem`] should be enabled / visible.
///
/// Kept minimal on purpose: any new variant must be dispatchable
/// on the host without naming a specific plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum RowMenuVisibility {
    /// Always enabled.
    #[default]
    Always,
    /// Enabled only when the hit carries a non-`None`
    /// [`Hit::secondary_action`].
    RequiresSecondaryAction,
}

/// Row context-menu definition. Empty means "no menu" and the host
/// omits the gesture wiring entirely for those rows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowMenuDef {
    pub items: Vec<RowMenuItem>,
}

impl RowMenuDef {
    /// Empty menu — the default for sources that opt out of the
    /// contextual affordance.
    pub fn empty() -> Self {
        Self { items: Vec::new() }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// A search result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hit {
    pub id: DocId,
    pub category: Category,
    pub title: String,
    pub subtitle: String,
    pub icon_name: Option<String>,
    pub kind_label: Option<String>,
    pub score: f32,
    pub action: Action,
    pub extract_fail: bool,
    /// Email `From` header. `None` for non-mail hits and hits
    /// whose source did not populate it. Used by the email preview
    /// plugin to render a header grid for gloda hits which cannot
    /// be read back from disk (gloda messages live inside an mbox
    /// shard, not as individual files on a path the plugin can
    /// `fs::read`).
    #[serde(default)]
    pub sender: Option<String>,
    /// Email `To`/`Cc`/`Bcc` joined. Same rationale as `sender`.
    #[serde(default)]
    pub recipients: Option<String>,
    /// Stored body snippet — currently only populated for gloda
    /// mail hits where it's the only way the email preview plugin
    /// can show message content (see `sender` note). Capped by the
    /// source; do not assume the full message.
    #[serde(default)]
    pub body: Option<String>,
    /// Optional secondary action invoked via right-click or other
    /// non-primary affordances. `Box` keeps `Hit` size stable since
    /// `Action` is a sizable enum. `None` means no secondary action;
    /// the host hides the corresponding menu entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secondary_action: Option<Box<Action>>,
    /// Identifies which source produced this hit. The host uses it
    /// as the cache key for the translated row menu (see
    /// [`RowMenuDef`]) — every hit from the same source instance
    /// shares one menu-model instance, bypassing the GTK4
    /// popover-menu retention leak that per-hit rebuilding caused.
    /// Empty string for legacy / test hits that predate plugin
    /// menus; the host treats it as "no menu".
    #[serde(default)]
    pub source_instance: String,
    /// Row context-menu supplied by the source. Attached by the
    /// daemon (never by the plugin itself) from the source's
    /// `row_menu()` declaration. Empty when the source opts out.
    #[serde(default, skip_serializing_if = "RowMenuDef::is_empty")]
    pub row_menu: RowMenuDef,
}

/// Per-hit score breakdown (Wave B T6) — carries the raw multipliers
/// that compose `Hit.score`. Populated only when a caller asks for an
/// explanation via `search_with_breakdown`; never serialized on the
/// wire because the daemon derives a human-readable string from it
/// before shipping (see `Response::HitsWithExtrasV3.explanations`).
///
/// Invariant: when all fields are populated,
///   `final_score == tantivy * category_mult * prefix_mult
///                 * acronym_mult * recency_mult * coord_mult
///                 * stage2_clamped`
/// up to f32 rounding. Plugin-owned hits (no tantivy scoring) leave
/// stage-1 fields at 1.0 and set `tantivy = final_score` so the
/// invariant still holds trivially.
#[derive(Debug, Clone, Default)]
pub struct ScoreBreakdown {
    pub tantivy: f32,
    pub category_mult: f32,
    pub prefix_mult: f32,
    pub acronym_mult: f32,
    pub recency_mult: f32,
    pub coord_mult: f32,
    pub frecency_mult: f32,
    pub latch_mult: f32,
    pub stage2_clamped: f32,
    pub final_score: f32,
}

/// Inline calculator result (for Spotlight-style "2+2 = 4" display).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Calculation {
    pub expr: String,
    pub result: String,
}

/// A document to be indexed.
#[derive(Debug, Clone)]
pub struct Document {
    pub id: DocId,
    pub category: Category,
    pub title: String,
    pub subtitle: String,
    pub icon_name: Option<String>,
    pub kind_label: Option<String>,
    pub body: Option<String>,
    pub path: String,
    pub mtime: i64,
    pub size: u64,
    pub action: Action,
    pub extract_fail: bool,
    /// Email `From` header. `None` for non-mail documents.
    pub sender: Option<String>,
    /// Email `To` + `Cc` joined by `, `. `None` for non-mail documents.
    pub recipients: Option<String>,
    /// Framework-set (NOT plugin-set): identifier of the source instance.
    /// Used to purge all docs from a removed/disabled source instance.
    pub source_instance: String,
    pub extra: Vec<ExtraFieldValue>,
    pub secondary_action: Option<Action>,
}

#[derive(Debug, Clone)]
pub struct ExtraFieldValue {
    pub field: &'static str,
    pub value: PluginValue,
}

#[derive(Debug, Clone)]
pub enum PluginValue {
    Text(String),
    I64(i64),
    U64(u64),
    Bool(bool),
}

/// Declaration of a plugin-owned tantivy field.
/// Plugins return `&'static [PluginFieldSpec]` from `Source::extra_fields()`.
#[derive(Debug, Clone, Copy)]
pub struct PluginFieldSpec {
    /// Globally unique across all enabled kinds. Convention: `<kind>_<short>`.
    pub schema_name: &'static str,
    /// User-facing alias for `field:value` queries. `Some("folder")` lets `folder:Inbox` work.
    pub query_alias: Option<&'static str>,
    pub ty: PluginFieldType,
    pub stored: bool,
    /// If true, field is included in default (unqualified) QueryParser with `boost`.
    pub default_search: bool,
    pub boost: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginFieldType {
    Text { tokenizer: TextTokenizer },
    Keyword,
    I64,
    U64,
    Bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextTokenizer {
    Default,
    Raw,
    Spotlight,
}

/// Search query.
#[derive(Debug, Clone)]
pub struct Query {
    pub text: String,
    pub limit: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_category_as_str() {
        assert_eq!(Category::App.as_str(), "app");
        assert_eq!(Category::File.as_str(), "file");
        assert_eq!(Category::Mail.as_str(), "mail");
        assert_eq!(Category::Attachment.as_str(), "attachment");
        assert_eq!(Category::Calculator.as_str(), "calculator");
        assert_eq!(Category::Shell.as_str(), "shell");
    }

    #[test]
    fn test_category_serde_roundtrip() {
        let cat = Category::App;
        let json = serde_json::to_string(&cat).unwrap();
        let decoded: Category = serde_json::from_str(&json).unwrap();
        assert_eq!(cat, decoded);
    }

    #[test]
    fn test_doc_id_equality() {
        let a = DocId("fs:/tmp/test.txt".to_string());
        let b = DocId("fs:/tmp/test.txt".to_string());
        let c = DocId("fs:/tmp/other.txt".to_string());
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn test_action_serde_roundtrip() {
        let actions = vec![
            Action::Launch {
                exec: "firefox".to_string(),
                terminal: false,
                desktop_id: Some("firefox.desktop".to_string()),
                desktop_file: Some(PathBuf::from("/usr/share/applications/firefox.desktop")),
                working_dir: None,
            },
            Action::OpenFile {
                path: PathBuf::from("/tmp/test.txt"),
            },
            Action::ShowInFileManager {
                path: PathBuf::from("/tmp"),
            },
            Action::OpenAttachment {
                mbox_path: PathBuf::from("/tmp/mail.mbox"),
                byte_offset: 100,
                length: 500,
                mime: "application/pdf".to_string(),
                encoding: "base64".to_string(),
                suggested_filename: "test.pdf".to_string(),
            },
            Action::Exec {
                cmdline: vec!["neomutt".into(), "-f".into(), "/home/me/Mail".into()],
                working_dir: Some(PathBuf::from("/home/me")),
                terminal: true,
            },
            Action::OpenUri {
                uri: "mid:abc@example.org".to_string(),
            },
        ];

        for action in actions {
            let json = serde_json::to_string(&action).unwrap();
            let decoded: Action = serde_json::from_str(&json).unwrap();
            match (&action, &decoded) {
                (Action::Launch { exec: e1, .. }, Action::Launch { exec: e2, .. }) => {
                    assert_eq!(e1, e2);
                }
                (Action::OpenFile { path: p1 }, Action::OpenFile { path: p2 }) => {
                    assert_eq!(p1, p2);
                }
                (
                    Action::ShowInFileManager { path: p1 },
                    Action::ShowInFileManager { path: p2 },
                ) => {
                    assert_eq!(p1, p2);
                }
                (
                    Action::OpenAttachment {
                        mbox_path: mb1,
                        byte_offset: bo1,
                        length: l1,
                        mime: mi1,
                        encoding: en1,
                        suggested_filename: sf1,
                    },
                    Action::OpenAttachment {
                        mbox_path: mb2,
                        byte_offset: bo2,
                        length: l2,
                        mime: mi2,
                        encoding: en2,
                        suggested_filename: sf2,
                    },
                ) => {
                    assert_eq!(mb1, mb2);
                    assert_eq!(bo1, bo2);
                    assert_eq!(l1, l2);
                    assert_eq!(mi1, mi2);
                    assert_eq!(en1, en2);
                    assert_eq!(sf1, sf2);
                }
                (
                    Action::Exec {
                        cmdline: c1,
                        working_dir: wd1,
                        terminal: t1,
                    },
                    Action::Exec {
                        cmdline: c2,
                        working_dir: wd2,
                        terminal: t2,
                    },
                ) => {
                    assert_eq!(c1, c2);
                    assert_eq!(wd1, wd2);
                    assert_eq!(t1, t2);
                }
                (Action::OpenUri { uri: u1 }, Action::OpenUri { uri: u2 }) => {
                    assert_eq!(u1, u2);
                }
                _ => panic!("Action variant mismatch"),
            }
        }
    }

    #[test]
    fn test_action_exec_terminal_default_false_for_legacy_wire() {
        let legacy_json = r#"{"Exec":{"cmdline":["ls"],"working_dir":null}}"#;
        let decoded: Action = serde_json::from_str(legacy_json).unwrap();
        match decoded {
            Action::Exec { terminal, .. } => assert!(!terminal),
            _ => panic!("expected Action::Exec"),
        }
    }

    #[test]
    fn test_query_clone() {
        let q = Query {
            text: "hello world".to_string(),
            limit: 10,
        };
        let q2 = q.clone();
        assert_eq!(q.text, q2.text);
        assert_eq!(q.limit, q2.limit);
    }

    #[test]
    fn test_calculation_serde_roundtrip() {
        let calculation = Calculation {
            expr: "sqrt(16)+pi".to_string(),
            result: "7.141592654".to_string(),
        };

        let json = serde_json::to_string(&calculation).unwrap();
        let decoded: Calculation = serde_json::from_str(&json).unwrap();

        assert_eq!(calculation, decoded);
    }
}
