use std::path::PathBuf;
use std::sync::LazyLock;

use anyhow::Result;
use lixun_core::{Action, Category, DocId, Hit};
use lixun_sources::{IndexerSource, MutationSink, QueryContext, SourceContext};
use regex::Regex;

static RISKY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:sudo\b|rm\s+-rf\b|mkfs\b|dd\s)").unwrap()
});

/// Wrap the user's shell command in a hold-after-exit sentinel so the
/// terminal window stays open until the user presses Enter. Without
/// this, terminals close the moment the wrapped command exits and the
/// user never sees stdout/stderr (or the exit code) for short-running
/// commands like `> echo hello`. Universal across xdg-terminal-exec /
/// `$TERMINAL` / xterm fallbacks (no terminal-specific flag needed).
fn wrap_with_hold(cmd: &str) -> String {
    format!(
        "{cmd}\nec=$?\nprintf '\\n[exit %s \u{2014} press Enter to close] ' \"$ec\"\nread -r _\n"
    )
}

pub struct ShellSource {
    pub working_dir: PathBuf,
    pub strict_mode: bool,
}

impl IndexerSource for ShellSource {
    fn kind(&self) -> &'static str {
        "shell"
    }

    fn reindex_full(
        &self,
        _ctx: &SourceContext,
        _sink: &dyn MutationSink,
    ) -> Result<()> {
        Ok(())
    }

    fn on_query(&self, query: &str, _ctx: &QueryContext) -> Vec<Hit> {
        let cmd = match query.strip_prefix('>').map(str::trim_start) {
            Some(s) if !s.is_empty() => s,
            _ => return Vec::new(),
        };
        let risky = RISKY_RE.is_match(cmd);
        if self.strict_mode && risky {
            return Vec::new();
        }
        let title = if risky {
            tracing::warn!(cmd = %cmd, "shell plugin: risky command");
            format!("Run: {cmd} \u{26A0}")
        } else {
            format!("Run: {cmd}")
        };
        vec![Hit {
            id: DocId(format!("shell:{cmd}")),
            category: Category::Shell,
            title,
            subtitle: "shell".into(),
            icon_name: Some("utilities-terminal".into()),
            kind_label: Some("Shell".into()),
            score: 900.0,
            action: Action::Exec {
                cmdline: vec!["sh".into(), "-c".into(), wrap_with_hold(cmd)],
                working_dir: Some(self.working_dir.clone()),
                terminal: true,
            },
            extract_fail: false,
            sender: None,
            recipients: None,
            body: None,
            secondary_action: None,
        }]
    }

    fn excludes_from_query_log(&self, query: &str) -> bool {
        query.trim_start().starts_with('>')
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn src(strict: bool) -> ShellSource {
        ShellSource {
            working_dir: PathBuf::from("/tmp/lixun-shell-test"),
            strict_mode: strict,
        }
    }

    fn ctx() -> QueryContext<'static> {
        QueryContext {
            instance_id: "shell",
            state_dir: Path::new("/tmp/lixun-shell-test"),
        }
    }

    #[test]
    fn triggers_on_prefix_with_space() {
        let hits = src(false).on_query("> ls", &ctx());
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Run: ls");
        assert_eq!(hits[0].category, Category::Shell);
        assert_eq!(hits[0].score, 900.0);
        assert_eq!(hits[0].id.0, "shell:ls");
        assert_eq!(hits[0].subtitle, "shell");
    }

    #[test]
    fn triggers_on_prefix_without_space() {
        let hits = src(false).on_query(">ls", &ctx());
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Run: ls");
    }

    #[test]
    fn no_trigger_without_prefix() {
        assert!(src(false).on_query("ls", &ctx()).is_empty());
        assert!(src(false).on_query("", &ctx()).is_empty());
        assert!(src(false).on_query(">", &ctx()).is_empty());
        assert!(src(false).on_query(">   ", &ctx()).is_empty());
    }

    #[test]
    fn risky_command_warns_but_returns_hit() {
        let hits = src(false).on_query("> sudo ls", &ctx());
        assert_eq!(hits.len(), 1);
        assert!(hits[0].title.ends_with('\u{26A0}'));
        assert!(hits[0].title.starts_with("Run: sudo ls "));
    }

    #[test]
    fn strict_mode_blocks_risky() {
        let hits = src(true).on_query("> rm -rf /", &ctx());
        assert!(hits.is_empty());
        let hits = src(true).on_query("> ls", &ctx());
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn excludes_from_query_log_matches_trigger() {
        let s = src(false);
        assert!(s.excludes_from_query_log("> ls"));
        assert!(s.excludes_from_query_log(" > ls"));
        assert!(!s.excludes_from_query_log("ls"));
        assert!(!s.excludes_from_query_log(""));
    }

    #[test]
    fn wrap_with_hold_includes_command_exit_capture_and_read() {
        let wrapped = wrap_with_hold("echo hello");
        assert!(wrapped.starts_with("echo hello\n"));
        assert!(wrapped.contains("ec=$?"));
        assert!(wrapped.contains("press Enter to close"));
        assert!(wrapped.trim_end().ends_with("read -r _"));
    }

    #[test]
    fn on_query_emits_wrapped_cmdline() {
        let hits = src(false).on_query("> echo hello", &ctx());
        assert_eq!(hits.len(), 1);
        let Action::Exec { cmdline, terminal, .. } = &hits[0].action else {
            panic!("expected Action::Exec");
        };
        assert!(*terminal);
        assert_eq!(cmdline[0], "sh");
        assert_eq!(cmdline[1], "-c");
        assert!(cmdline[2].starts_with("echo hello\n"));
        assert!(cmdline[2].contains("read -r _"));
    }
}
