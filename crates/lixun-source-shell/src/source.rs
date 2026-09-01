use std::path::PathBuf;

use anyhow::Result;
use lixun_core::{Action, Category, DocId, Hit, RowMenuDef, RowMenuItem, RowMenuVerb};
use lixun_sources::{IndexerSource, MutationSink, QueryContext, SourceContext};

/// Programs allowed to run in the default (argv) shell mode. The list is
/// intentionally conservative: read-only inspection and common developer
/// tooling. Anything outside this set is treated as non-whitelisted and
/// is either suppressed (strict mode) or surfaced with a warning badge
/// (lenient mode). The whitelist is matched against the basename of the
/// program (argv[0]) so an absolute path like `/usr/bin/ls` still passes.
const WHITELIST: &[&str] = &[
    "ls", "echo", "pwd", "cat", "grep", "rg", "fd", "find", "git", "cargo", "make", "head", "tail",
    "wc", "tree", "stat", "du", "df", "free", "ps", "top", "htop", "btop", "uname", "whoami", "id",
    "date", "uptime", "which", "type", "env",
];

/// Return true when the program (argv[0]) is on the whitelist. Matches on
/// the final path segment so both `ls` and `/usr/bin/ls` are accepted.
fn is_whitelisted(program: &str) -> bool {
    let base = program.rsplit('/').next().unwrap_or(program);
    WHITELIST.contains(&base)
}

/// Wrap the user's shell command in a hold-after-exit sentinel so the
/// terminal window stays open until the user presses Enter. Without
/// this, terminals close the moment the wrapped command exits and the
/// user never sees stdout/stderr (or the exit code) for short-running
/// commands like `> echo hello`. Universal across xdg-terminal-exec /
/// `$TERMINAL` / xterm fallbacks (no terminal-specific flag needed).
/// Only used by the opt-in legacy `sh -c` mode.
fn wrap_with_hold(cmd: &str) -> String {
    format!(
        "{cmd}\nec=$?\nprintf '\\n[exit %s \u{2014} press Enter to close] ' \"$ec\"\nread -r _\n"
    )
}

pub struct ShellSource {
    pub working_dir: PathBuf,
    pub strict_mode: bool,
    /// When false (default), commands are parsed into argv and executed
    /// directly without a shell, so metacharacters carry no special
    /// meaning and command injection is impossible. When true, the
    /// command is handed verbatim to `sh -c`, restoring full shell
    /// semantics (pipes, redirects, globbing) for power users who
    /// accept the risk.
    ///
    /// `shell_mode = true` means exactly that: anything typed after
    /// the prefix runs in a POSIX shell with the user's privileges —
    /// including substitutions like `$(...)`. No escaping is applied,
    /// by design. Leave it off unless every keystroke in the launcher
    /// should be treated as a shell command line.
    pub shell_mode: bool,
}

impl ShellSource {
    fn placeholder_hit(&self, query: &str, ctx: &QueryContext) -> Hit {
        Hit {
            id: DocId("shell:__placeholder__".into()),
            category: Category::Shell,
            title: "Run a shell command".into(),
            subtitle: "Type a command after >".into(),
            icon_name: Some("utilities-terminal".into()),
            kind_label: Some("Shell".into()),
            score: 900.0,
            action: Action::ReplaceQuery { q: query.into() },
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

    /// Default safe path: parse `cmd` into argv with `shell_words`, validate
    /// argv[0] against the whitelist, and emit `Action::Exec` running the
    /// program directly (no shell). Returns an empty vec when parsing fails
    /// or when a non-whitelisted program is rejected under strict mode.
    fn argv_hit(&self, cmd: &str, ctx: &QueryContext) -> Vec<Hit> {
        let argv = match shell_words::split(cmd) {
            Ok(parts) => parts,
            Err(_) => return Vec::new(),
        };
        if argv.is_empty() {
            return Vec::new();
        }
        let whitelisted = is_whitelisted(&argv[0]);
        if self.strict_mode && !whitelisted {
            return Vec::new();
        }
        let title = if whitelisted {
            format!("Run: {cmd}")
        } else {
            tracing::warn!(cmd = %cmd, program = %argv[0], "shell plugin: non-whitelisted program");
            format!("Run: {cmd} \u{26A0}")
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
                cmdline: argv.clone(),
                working_dir: Some(self.working_dir.clone()),
                terminal: true,
            },
            extract_fail: false,
            sender: None,
            recipients: None,
            body: None,
            secondary_action: Some(Box::new(Action::ExecCapture {
                cmdline: argv,
                working_dir: Some(self.working_dir.clone()),
            })),
            source_instance: ctx.instance_id.to_string(),
            row_menu: RowMenuDef::empty(),
            mime: None,
            timestamp: None,
            size: None,
        }]
    }

    /// Opt-in legacy path: hand the raw command to `sh -c`, restoring full
    /// shell semantics. Gated behind `shell_mode = true` in config.
    fn legacy_sh_c_hit(&self, cmd: &str, ctx: &QueryContext) -> Vec<Hit> {
        vec![Hit {
            id: DocId(format!("shell:{cmd}")),
            category: Category::Shell,
            title: format!("Run: {cmd}"),
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
            secondary_action: Some(Box::new(Action::ExecCapture {
                cmdline: vec!["sh".into(), "-c".into(), cmd.into()],
                working_dir: Some(self.working_dir.clone()),
            })),
            source_instance: ctx.instance_id.to_string(),
            row_menu: RowMenuDef::empty(),
            mime: None,
            timestamp: None,
            size: None,
        }]
    }
}

impl IndexerSource for ShellSource {
    fn kind(&self) -> &'static str {
        "shell"
    }

    fn reindex_full(&self, _ctx: &SourceContext, _sink: &dyn MutationSink) -> Result<()> {
        Ok(())
    }

    fn on_query(&self, query: &str, ctx: &QueryContext) -> Vec<Hit> {
        let body = match query.strip_prefix('>') {
            Some(rest) => rest.trim_start(),
            None => return Vec::new(),
        };
        if body.is_empty() {
            return vec![self.placeholder_hit(query, ctx)];
        }
        if self.shell_mode {
            self.legacy_sh_c_hit(body, ctx)
        } else {
            self.argv_hit(body, ctx)
        }
    }

    fn excludes_from_query_log(&self, query: &str) -> bool {
        query.trim_start().starts_with('>')
    }

    fn claims_query(&self, query: &str) -> bool {
        query.trim_start().starts_with('>')
    }

    fn claimed_prefix(&self) -> Option<&'static str> {
        Some(">")
    }

    fn row_menu(&self) -> RowMenuDef {
        RowMenuDef {
            items: vec![RowMenuItem {
                label: "Execute".into(),
                verb: RowMenuVerb::Open,
                visibility: Default::default(),
            }],
        }
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
            shell_mode: false,
        }
    }

    fn src_shell_mode() -> ShellSource {
        ShellSource {
            working_dir: PathBuf::from("/tmp/lixun-shell-test"),
            strict_mode: false,
            shell_mode: true,
        }
    }

    fn ctx() -> QueryContext<'static> {
        QueryContext {
            cancel: None,
            instance_id: "shell",
            state_dir: Path::new("/tmp/lixun-shell-test"),
        }
    }

    fn exec_cmdline(hit: &Hit) -> &[String] {
        let Action::Exec { cmdline, .. } = &hit.action else {
            panic!("expected Action::Exec");
        };
        cmdline
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
    }

    #[test]
    fn bare_prefix_shows_placeholder() {
        for q in [">", ">   "] {
            let hits = src(false).on_query(q, &ctx());
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].id.0, "shell:__placeholder__");
        }
    }

    #[test]
    fn excludes_from_query_log_matches_trigger() {
        let s = src(false);
        assert!(s.excludes_from_query_log("> ls"));
        assert!(s.excludes_from_query_log(" > ls"));
        assert!(!s.excludes_from_query_log("ls"));
        assert!(!s.excludes_from_query_log(""));
    }

    // --- P0-B argv-mode (default safe) behaviour ---

    #[test]
    fn argv_mode_emits_argv_not_sh_c() {
        let hits = src(false).on_query("> ls -la", &ctx());
        assert_eq!(hits.len(), 1);
        let cmdline = exec_cmdline(&hits[0]);
        assert_eq!(cmdline, &["ls".to_string(), "-la".to_string()]);
        // No shell wrapper anywhere.
        assert!(!cmdline.iter().any(|a| a == "sh"));
        assert!(!cmdline.iter().any(|a| a == "-c"));
    }

    #[test]
    fn argv_mode_rejects_metacharacters_via_shell_words() {
        // `echo` is whitelisted; the `;` and following tokens become
        // literal argv elements, never a shell separator. This proves
        // no shell evaluation occurs: `rm -rf ~` cannot fork.
        let hits = src(false).on_query("> echo x; rm -rf ~", &ctx());
        assert_eq!(hits.len(), 1);
        let cmdline = exec_cmdline(&hits[0]);
        assert_eq!(cmdline[0], "echo");
        assert!(cmdline.contains(&"x;".to_string()));
        // The dangerous tokens are inert literal args, not a new command.
        assert!(cmdline.contains(&"rm".to_string()));
        assert!(cmdline.contains(&"-rf".to_string()));
        assert!(!cmdline.iter().any(|a| a == "sh"));
    }

    #[test]
    fn argv_mode_whitelist_under_strict_blocks_non_whitelisted() {
        let hits = src(true).on_query("> sudo ls", &ctx());
        assert!(hits.is_empty());
    }

    #[test]
    fn argv_mode_non_whitelisted_warns_under_lenient() {
        let hits = src(false).on_query("> sudo ls", &ctx());
        assert_eq!(hits.len(), 1);
        assert!(hits[0].title.ends_with('\u{26A0}'));
        // Still emitted as argv, not sh -c.
        let cmdline = exec_cmdline(&hits[0]);
        assert_eq!(cmdline[0], "sudo");
        assert!(!cmdline.iter().any(|a| a == "-c"));
    }

    #[test]
    fn argv_mode_quoted_args_preserved() {
        let hits = src(false).on_query("> grep \"hello world\" file.txt", &ctx());
        assert_eq!(hits.len(), 1);
        let cmdline = exec_cmdline(&hits[0]);
        assert_eq!(
            cmdline,
            &[
                "grep".to_string(),
                "hello world".to_string(),
                "file.txt".to_string()
            ]
        );
    }

    #[test]
    fn argv_mode_parse_error_yields_no_hit() {
        // Unbalanced quote -> shell_words::split returns Err -> no hit.
        let hits = src(false).on_query("> \"unclosed", &ctx());
        assert!(hits.is_empty());
    }

    // --- opt-in legacy sh -c mode ---

    #[test]
    fn legacy_mode_keeps_sh_c_behavior() {
        let hits = src_shell_mode().on_query("> echo hello", &ctx());
        assert_eq!(hits.len(), 1);
        let cmdline = exec_cmdline(&hits[0]);
        assert_eq!(cmdline[0], "sh");
        assert_eq!(cmdline[1], "-c");
        assert!(cmdline[2].starts_with("echo hello\n"));
        assert!(cmdline[2].contains("read -r _"));
    }

    #[test]
    fn wrap_with_hold_includes_command_exit_capture_and_read() {
        let wrapped = wrap_with_hold("echo hello");
        assert!(wrapped.starts_with("echo hello\n"));
        assert!(wrapped.contains("ec=$?"));
        assert!(wrapped.contains("press Enter to close"));
        assert!(wrapped.trim_end().ends_with("read -r _"));
    }
}
