//! The command source: claims its prefix, runs the configured argv,
//! maps JSON stdout onto generic Hits.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use lixun_core::{Action, Category, DocId, Hit, RowMenuDef, RowMenuItem, RowMenuVerb};
use lixun_sources::{IndexerSource, MutationSink, QueryContext, SourceContext};
use serde::Deserialize;

use crate::runner::{RunStatus, run_bounded};

/// Upper bound on hits mapped from one invocation.
pub const MAX_HITS: usize = 20;

pub struct CommandSource {
    /// Display name (`"Passwords"`); doubles as the kind label.
    pub name: String,
    /// Claimed query prefix, verbatim from config (`"pass "`).
    /// Leaked to `'static` at build for `claimed_prefix`.
    pub prefix: &'static str,
    /// Argv template; `{query}` in any token is replaced with the
    /// text after the prefix. Spawned directly — no shell.
    pub argv: Vec<String>,
    pub timeout: Duration,
    pub cwd: Option<PathBuf>,
}

/// One stdout entry. `action` uses an externally-visible tag so
/// scripts write `{"type": "open-uri", "uri": "..."}` etc.
#[derive(Debug, Deserialize)]
pub struct ScriptHit {
    pub title: String,
    #[serde(default)]
    pub subtitle: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    pub action: ScriptAction,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ScriptAction {
    OpenUri {
        uri: String,
    },
    OpenFile {
        path: PathBuf,
    },
    CopyText {
        text: String,
    },
    Exec {
        argv: Vec<String>,
        #[serde(default)]
        cwd: Option<PathBuf>,
        #[serde(default)]
        terminal: bool,
    },
}

impl ScriptAction {
    fn into_action(self) -> Action {
        match self {
            ScriptAction::OpenUri { uri } => Action::OpenUri { uri },
            ScriptAction::OpenFile { path } => Action::OpenFile { path },
            ScriptAction::CopyText { text } => Action::CopyText { text },
            ScriptAction::Exec {
                argv,
                cwd,
                terminal,
            } => Action::Exec {
                cmdline: argv,
                working_dir: cwd,
                terminal,
            },
        }
    }
}

/// Substitute `{query}` in every argv token. Literal substitution
/// into a single token — never re-split, never shell-expanded.
pub fn substitute_argv(template: &[String], query: &str) -> Vec<String> {
    template
        .iter()
        .map(|tok| tok.replace("{query}", query))
        .collect()
}

/// Parse stdout as a JSON array of [`ScriptHit`] or, failing that, as
/// JSON lines (one object per non-empty line). Returns an error only
/// when NEITHER shape parses.
pub fn parse_script_hits(stdout: &str) -> Result<Vec<ScriptHit>> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    if trimmed.starts_with('[') {
        return serde_json::from_str::<Vec<ScriptHit>>(trimmed)
            .map_err(|e| anyhow::anyhow!("invalid JSON array: {e}"));
    }
    let mut out = Vec::new();
    for (n, line) in trimmed.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let hit: ScriptHit = serde_json::from_str(line)
            .map_err(|e| anyhow::anyhow!("invalid JSON on line {}: {e}", n + 1))?;
        out.push(hit);
    }
    Ok(out)
}

impl CommandSource {
    fn error_hit(&self, detail: &str, ctx: &QueryContext) -> Hit {
        Hit {
            id: DocId(format!("command:{}:__error__", self.name)),
            category: Category::Shell,
            title: format!("{}: command failed", self.name),
            subtitle: detail.to_string(),
            icon_name: Some("dialog-error".into()),
            kind_label: Some(self.name.clone()),
            score: 999.0,
            action: Action::CopyText {
                text: detail.to_string(),
            },
            extract_fail: false,
            sender: None,
            recipients: None,
            body: None,
            secondary_action: None,
            source_instance: ctx.instance_id.to_string(),
            row_menu: RowMenuDef::empty(),
            mime: None,
            timestamp: None,
            size: None,
        }
    }

    fn map_hits(&self, script_hits: Vec<ScriptHit>, ctx: &QueryContext) -> Vec<Hit> {
        script_hits
            .into_iter()
            .take(MAX_HITS)
            .enumerate()
            .map(|(i, sh)| Hit {
                id: DocId(format!("command:{}:{}", self.name, i)),
                category: Category::Shell,
                title: sh.title,
                subtitle: sh.subtitle.unwrap_or_default(),
                icon_name: sh.icon.or_else(|| Some("system-run".into())),
                kind_label: Some(self.name.clone()),
                // Descending scores preserve the script's own order
                // through the daemon's score sort.
                score: 999.0 - i as f32,
                action: sh.action.into_action(),
                extract_fail: false,
                sender: None,
                recipients: None,
                body: None,
                secondary_action: None,
                source_instance: ctx.instance_id.to_string(),
                row_menu: RowMenuDef::empty(),
                mime: None,
                timestamp: None,
                size: None,
            })
            .collect()
    }
}

impl IndexerSource for CommandSource {
    fn kind(&self) -> &'static str {
        "command"
    }

    fn reindex_full(&self, _ctx: &SourceContext, _sink: &dyn MutationSink) -> Result<()> {
        Ok(())
    }

    fn on_query(&self, query: &str, ctx: &QueryContext) -> Vec<Hit> {
        let Some(rest) = query.strip_prefix(self.prefix) else {
            return Vec::new();
        };
        let argv = substitute_argv(&self.argv, rest.trim());
        let cancelled = || ctx.is_cancelled();
        let outcome = match run_bounded(&argv, self.cwd.as_deref(), self.timeout, &cancelled) {
            Ok(o) => o,
            Err(e) => return vec![self.error_hit(&format!("{e:#}"), ctx)],
        };
        match outcome.status {
            RunStatus::Cancelled => Vec::new(),
            RunStatus::TimedOut => vec![self.error_hit(
                &format!("timed out after {} ms", self.timeout.as_millis()),
                ctx,
            )],
            RunStatus::Failed(code) => {
                let mut detail = match code {
                    Some(c) => format!("exited with code {c}"),
                    None => "killed by a signal".to_string(),
                };
                if !outcome.stderr_head.is_empty() {
                    detail.push_str(": ");
                    detail.push_str(&outcome.stderr_head);
                }
                vec![self.error_hit(&detail, ctx)]
            }
            RunStatus::Success => {
                let stdout = String::from_utf8_lossy(&outcome.stdout);
                match parse_script_hits(&stdout) {
                    Ok(hits) => self.map_hits(hits, ctx),
                    Err(e) => vec![self.error_hit(&format!("{e:#}"), ctx)],
                }
            }
        }
    }

    /// Command arguments can be sensitive (password entry names,
    /// private notes); keep them out of the recent-query log.
    fn excludes_from_query_log(&self, query: &str) -> bool {
        query.starts_with(self.prefix)
    }

    fn claims_query(&self, query: &str) -> bool {
        query.starts_with(self.prefix)
    }

    fn claimed_prefix(&self) -> Option<&'static str> {
        Some(self.prefix)
    }

    fn row_menu(&self) -> RowMenuDef {
        RowMenuDef {
            items: vec![
                RowMenuItem {
                    label: "Open".into(),
                    verb: RowMenuVerb::Open,
                    visibility: Default::default(),
                },
                RowMenuItem {
                    label: "Copy".into(),
                    verb: RowMenuVerb::Copy,
                    visibility: Default::default(),
                },
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn ctx() -> QueryContext<'static> {
        QueryContext {
            cancel: None,
            instance_id: "command:Test",
            state_dir: Path::new("/tmp/lixun-command-test"),
        }
    }

    fn src(argv: Vec<String>) -> CommandSource {
        CommandSource {
            name: "Test".into(),
            prefix: "t ",
            argv,
            timeout: Duration::from_millis(2000),
            cwd: None,
        }
    }

    #[test]
    fn substitute_argv_is_literal_per_token() {
        let argv = substitute_argv(
            &["run.sh".into(), "--q={query}".into(), "{query}".into()],
            "a b; rm -rf /",
        );
        assert_eq!(
            argv,
            vec![
                "run.sh".to_string(),
                "--q=a b; rm -rf /".to_string(),
                "a b; rm -rf /".to_string(),
            ],
            "substitution must never re-split or shell-expand"
        );
    }

    #[test]
    fn parse_json_array_and_lines() {
        let array = r#"[{"title":"A","action":{"type":"copy-text","text":"x"}}]"#;
        let hits = parse_script_hits(array).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "A");

        let lines = "{\"title\":\"A\",\"action\":{\"type\":\"open-uri\",\"uri\":\"https://e/\"}}\n\
                     {\"title\":\"B\",\"subtitle\":\"s\",\"action\":{\"type\":\"open-file\",\"path\":\"/tmp/x\"}}\n";
        let hits = parse_script_hits(lines).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(matches!(hits[1].action, ScriptAction::OpenFile { .. }));

        assert!(parse_script_hits("").unwrap().is_empty());
        assert!(parse_script_hits("not json").is_err());
    }

    #[test]
    fn exec_action_maps_to_generic_exec() {
        let json = r#"{"title":"E","action":{"type":"exec","argv":["ls","-la"],"terminal":true}}"#;
        let hits = parse_script_hits(json).unwrap();
        match hits.into_iter().next().unwrap().action.into_action() {
            Action::Exec {
                cmdline, terminal, ..
            } => {
                assert_eq!(cmdline, vec!["ls".to_string(), "-la".to_string()]);
                assert!(terminal);
            }
            other => panic!("expected Exec, got {other:?}"),
        }
    }

    #[test]
    fn on_query_maps_script_output_to_hits() {
        let s = src(vec![
            "sh".into(),
            "-c".into(),
            r#"printf '{"title":"Hit for %s","action":{"type":"copy-text","text":"v"}}\n' "$1""#
                .into(),
            "sh".into(),
            "{query}".into(),
        ]);
        let hits = s.on_query("t hello", &ctx());
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Hit for hello");
        assert_eq!(hits[0].kind_label.as_deref(), Some("Test"));
        assert!(matches!(&hits[0].action, Action::CopyText { text } if text == "v"));
    }

    #[test]
    fn on_query_surfaces_parse_error_as_single_hit() {
        let s = src(vec!["echo".into(), "definitely not json".into()]);
        let hits = s.on_query("t x", &ctx());
        assert_eq!(hits.len(), 1);
        assert!(hits[0].title.contains("command failed"));
        assert!(matches!(&hits[0].action, Action::CopyText { .. }));
    }

    #[test]
    fn on_query_surfaces_nonzero_exit() {
        let s = src(vec!["sh".into(), "-c".into(), "echo bad >&2; exit 2".into()]);
        let hits = s.on_query("t x", &ctx());
        assert_eq!(hits.len(), 1);
        assert!(hits[0].subtitle.contains("code 2"));
        assert!(hits[0].subtitle.contains("bad"));
    }

    #[test]
    fn hit_count_is_bounded() {
        // 50 JSON lines in, MAX_HITS out.
        let script = r#"i=0; while [ $i -lt 50 ]; do printf '{"title":"h%s","action":{"type":"copy-text","text":"x"}}\n' $i; i=$((i+1)); done"#;
        let s = src(vec!["sh".into(), "-c".into(), script.into()]);
        let hits = s.on_query("t x", &ctx());
        assert_eq!(hits.len(), MAX_HITS);
        // Script order preserved through descending scores.
        assert!(hits[0].score > hits[1].score);
    }

    #[test]
    fn unclaimed_query_yields_nothing() {
        let s = src(vec!["echo".into(), "[]".into()]);
        assert!(s.on_query("hello", &ctx()).is_empty());
        assert!(s.claims_query("t x"));
        assert!(!s.claims_query("tx"));
        assert!(s.excludes_from_query_log("t secret"));
        assert!(!s.excludes_from_query_log("secret"));
    }
}
