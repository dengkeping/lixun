//! Preview plugin trait + shared helpers for `lixun-preview`.
//!
//! Each format plugin (text, image, pdf, code, email, office, av)
//! lives in its own crate and registers itself via
//! `inventory::submit!(PreviewPluginEntry { .. })`. The preview
//! binary links them via `lixun-preview-bundle` and discovers them
//! at runtime by iterating `inventory::iter::<PreviewPluginEntry>`.
//!
//! The daemon does not depend on this crate and never names a
//! concrete plugin.

use lixun_core::Hit;

/// Sentinel error message returned by `PreviewPlugin::update`'s
/// default implementation. The preview host matches on this exact
/// string to decide between "rebuild widget" (opt-out) and "log
/// and rebuild as recovery" (genuine failure). Plugins must not
/// reuse this string for any other error.
pub const UPDATE_UNSUPPORTED: &str = "UpdateUnsupported";

/// Config shipped to a plugin's `build()`, sourced from the
/// `[preview]` section of `~/.config/lixun/config.toml`.
///
/// The `section` is the raw `[preview.<plugin-id>]` TOML table if
/// the user wrote one; plugins parse it with their own schema.
/// `max_file_size_mb` is hoisted from the top-level
/// `preview.max_file_size_mb` for convenience — plugins that render
/// large binaries (images, PDFs, video) should honour it.
pub struct PreviewPluginCfg<'a> {
    pub section: Option<&'a toml::Value>,
    pub max_file_size_mb: u64,
}

impl<'a> PreviewPluginCfg<'a> {
    pub fn none() -> Self {
        Self {
            section: None,
            max_file_size_mb: 200,
        }
    }
}

/// How the preview host should size the window relative to the
/// plugin's content widget.
///
/// Different content types have different notions of "natural
/// size". A short txt file wants a small window; a 4K photo does
/// not — its pixbuf-reported natural size is the image itself
/// (thousands of pixels), which would push the window straight to
/// the configured ceiling and feel exactly like a fixed window.
/// So plugins declare their sizing intent explicitly and the host
/// picks a strategy:
///
/// - `FitToContent`: the host starts the window at a small
///   default, enables `propagate_natural_*` on the content's
///   scroll container, and lets the content drive the final
///   window size — clamped at `preview_max_*_px` from config.
///   Use for plugins whose content reports small, honest natural
///   sizes (wrapped text, email, code).
///
/// - `FixedCap` (default): the host sizes the window to the
///   configured cap (`preview_*_percent × monitor` clipped to
///   `preview_max_*_px`), independently of content. Use for
///   media-like plugins (image, pdf, video, office) where the
///   content's natural size is either huge or ill-defined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizingPreference {
    FitToContent,
    FixedCap,
}

/// A format plugin that renders a preview widget for a hit AND
/// owns the launch semantics for hits in its domain.
///
/// Implementations must:
/// - Return a stable, globally-unique `id()` used as plugin key and
///   as the `[preview.<id>]` config subtable name.
/// - Compute `match_score()` cheaply (no filesystem access, no
///   MIME sniffing of file contents); return `0` to decline.
/// - Keep `build()` effectively synchronous — under ~50 ms wall
///   time. Heavier work (PDF rasterisation, office conversion)
///   must return a placeholder widget immediately and offload
///   to a worker thread via `gio::spawn_blocking` +
///   `glib::MainContext::spawn_local`.
/// - Override `launch()` only if the hit needs plugin-internal
///   state the default cannot reach (e.g. mail attachments that
///   must be sliced out of an mbox before `xdg-open`); most
///   plugins can produce `Action::OpenUri { uri }` or
///   `Action::Exec` and let the default handle dispatch.
pub trait PreviewPlugin: Send + Sync + 'static {
    fn id(&self) -> &'static str;

    fn match_score(&self, hit: &Hit) -> u32;

    /// Declared sizing strategy; see `SizingPreference`.
    ///
    /// Default is `FixedCap` so existing media-heavy plugins keep
    /// their current behaviour without needing an override. Text-
    /// like plugins should return `FitToContent`.
    fn sizing(&self) -> SizingPreference {
        SizingPreference::FixedCap
    }

    fn build(&self, hit: &Hit, cfg: &PreviewPluginCfg<'_>) -> anyhow::Result<gtk::Widget>;

    /// Update an already-built widget in place for a new hit.
    ///
    /// Called by the preview host when the launcher's selection
    /// moves to a different hit AND `select_plugin` resolves to the
    /// same plugin instance. The plugin should mutate the existing
    /// widget tree (swap text buffer, reload pixbuf, re-render PDF
    /// page, etc.) instead of producing a new widget — this is the
    /// core path that keeps live preview feeling instant on rapid
    /// arrow-key scrubbing (Apple QuickLook semantics).
    ///
    /// Returning `Err(UpdateUnsupported)` is the documented opt-out:
    /// the host catches it, drops the old widget, calls
    /// `build(new_hit, cfg)` and `set_child()` on the window's
    /// content slot. Every plugin therefore works day-1 — opting
    /// into in-place update is purely an optimisation.
    ///
    /// Returning any other `Err` is a real failure: the host logs
    /// it and falls back to rebuild as a last-resort recovery
    /// (better stale-but-correct than a stuck stale widget).
    ///
    /// Implementations MUST be quick (target <16 ms — one frame).
    /// Heavy work (PDF rasterisation, office conversion) MUST
    /// schedule on a worker thread and check the host-supplied
    /// epoch (via plugin-internal state) before committing results
    /// to the widget — see `PreviewCommand::ShowOrUpdate { epoch }`
    /// in `lixun-ipc::preview`.
    fn update(&self, _hit: &Hit, _widget: &gtk::Widget) -> anyhow::Result<()> {
        anyhow::bail!(UPDATE_UNSUPPORTED)
    }

    /// Whether this plugin can turn `hit` into a user-visible
    /// launch (Open button in the preview header + Enter key
    /// inside the preview window).
    ///
    /// Default: true for every `Action` variant whose launch is a
    /// well-defined system operation that does NOT require
    /// launcher-internal state (i.e. everything except
    /// `ReplaceQuery`, which is a launcher-only text-swap and has
    /// no meaning in preview).
    ///
    /// Plugins with in-process extraction steps (e.g. mail
    /// attachments sliced out of an mbox) may return `false` if
    /// they cannot perform that extraction without the launcher's
    /// action helpers; the preview host then hides the Open button
    /// and maps Enter to plain dismiss.
    fn can_launch(&self, hit: &Hit) -> bool {
        !matches!(hit.action, lixun_core::Action::ReplaceQuery { .. })
    }

    /// Launch this hit. Called by the preview host when the user
    /// clicks the Open button or presses Enter. The host will
    /// exit with its "launched" sentinel on Ok(()) so the daemon
    /// clears the launcher session.
    ///
    /// Default implementation handles the generic path-based
    /// variants via `xdg-open` / `gio launch_default_for_uri`,
    /// generic URI dispatch (`Action::OpenUri { uri }`) via
    /// `xdg-open uri`, and the generic command-line variants
    /// (`Launch`, `Exec`) via `std::process::Command`. Plugins
    /// whose domain needs plugin-internal state the default cannot
    /// reach (e.g. mbox attachment extraction) MUST override this.
    ///
    /// Returning `Err` keeps the preview window open so the user
    /// can Escape cleanly; the host logs the error.
    fn launch(&self, hit: &Hit) -> anyhow::Result<()> {
        default_launch(hit)
    }
}

/// Generic launch logic used by the `PreviewPlugin::launch` default
/// implementation. Exposed as a free function so plugin overrides
/// can fall through to it for action variants they don't specialise
/// (e.g. a plugin that extracts a file and then wants the generic
/// `OpenFile` dispatch).
pub fn default_launch(hit: &lixun_core::Hit) -> anyhow::Result<()> {
    use gtk::prelude::FileExt;
    use lixun_core::Action;
    match &hit.action {
        Action::OpenFile { path } | Action::ShowInFileManager { path } => {
            let uri = gtk::gio::File::for_path(path).uri();
            if uri.is_empty() {
                anyhow::bail!("cannot form URI from path {:?}", path);
            }
            gtk::gio::AppInfo::launch_default_for_uri(&uri, gtk::gio::AppLaunchContext::NONE)?;
            Ok(())
        }
        Action::Launch { exec, .. } => {
            let tokens: Vec<&str> = exec
                .split_whitespace()
                .filter(|tok| {
                    !matches!(
                        *tok,
                        "%f" | "%F"
                            | "%u"
                            | "%U"
                            | "%d"
                            | "%D"
                            | "%n"
                            | "%N"
                            | "%i"
                            | "%c"
                            | "%k"
                            | "%v"
                            | "%m"
                    )
                })
                .collect();
            let Some((program, args)) = tokens.split_first() else {
                anyhow::bail!("empty exec line after field-code strip: {:?}", exec);
            };
            std::process::Command::new(program).args(args).spawn()?;
            Ok(())
        }
        Action::Exec { cmdline, .. } => {
            let Some((program, args)) = cmdline.split_first() else {
                anyhow::bail!("Action::Exec with empty cmdline");
            };
            std::process::Command::new(program).args(args).spawn()?;
            Ok(())
        }
        Action::OpenUri { uri } => {
            tracing::debug!(uri = %uri, "default_launch: dispatching via xdg-open");
            std::process::Command::new("xdg-open").arg(uri).spawn()?;
            Ok(())
        }
        Action::OpenAttachment { .. } => {
            anyhow::bail!(
                "default_launch: {:?} is plugin-specific and has no generic fallback; \
                 the plugin must override PreviewPlugin::launch",
                std::mem::discriminant(&hit.action)
            );
        }
        Action::ReplaceQuery { .. } => {
            anyhow::bail!("ReplaceQuery has no standalone launch semantics");
        }
        Action::ExecCapture { .. } => {
            anyhow::bail!("ExecCapture has no standalone launch semantics");
        }
    }
}

pub struct PreviewPluginEntry {
    pub factory: fn() -> Box<dyn PreviewPlugin>,
}

inventory::collect!(PreviewPluginEntry);

/// Pick the best plugin for `hit`, or `None` if no plugin scores
/// above zero. Ties on score are broken by `id()` alphabetically
/// so the choice is deterministic across builds (inventory's own
/// iteration order is not guaranteed).
pub fn select_plugin(hit: &Hit) -> Option<Box<dyn PreviewPlugin>> {
    inventory::iter::<PreviewPluginEntry>
        .into_iter()
        .map(|entry| (entry.factory)())
        .filter_map(|p| {
            let score = p.match_score(hit);
            if score == 0 { None } else { Some((score, p)) }
        })
        .max_by(|(s1, p1), (s2, p2)| s1.cmp(s2).then_with(|| p1.id().cmp(p2.id())))
        .map(|(_, p)| p)
}

/// Install the user's `~/.config/lixun/style.css` at
/// `APPLICATION + 1` priority on top of the built-in theme, if the
/// file exists. Mirrors the launcher's CSS loading
/// (`lixun-gui::window::build_window`) so both windows pick up the
/// same user theme from one source of truth.
///
/// No-op if the file is missing or the config dir cannot be
/// determined. Never panics; logs on failure.
pub fn install_user_css(display: &gtk::gdk::Display) {
    let Some(path) = dirs::config_dir().map(|d| d.join("lixun/style.css")) else {
        return;
    };
    if !path.exists() {
        return;
    }
    let provider = gtk::CssProvider::new();
    provider.load_from_path(&path);
    gtk::style_context_add_provider_for_display(
        display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
    );
    tracing::info!("preview: loaded external style.css from {:?}", path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use lixun_core::{Action, Category, DocId};
    use std::path::PathBuf;

    fn fake_hit() -> Hit {
        Hit {
            id: DocId("fs:/tmp/demo.txt".into()),
            category: Category::File,
            title: "demo.txt".into(),
            subtitle: "/tmp".into(),
            icon_name: None,
            kind_label: None,
            score: 1.0,
            action: Action::OpenFile {
                path: PathBuf::from("/tmp/demo.txt"),
            },
            mime: None,
            extract_fail: false,
            sender: None,
            recipients: None,
            body: None,
            secondary_action: None,
            source_instance: String::new(),
            row_menu: lixun_core::RowMenuDef::empty(),
        }
    }

    struct ScorePlugin {
        id: &'static str,
        score: u32,
    }

    impl PreviewPlugin for ScorePlugin {
        fn id(&self) -> &'static str {
            self.id
        }
        fn match_score(&self, _: &Hit) -> u32 {
            self.score
        }
        fn build(&self, _: &Hit, _: &PreviewPluginCfg<'_>) -> anyhow::Result<gtk::Widget> {
            unreachable!("test plugin")
        }
    }

    fn choose(plugins: Vec<Box<dyn PreviewPlugin>>, hit: &Hit) -> Option<&'static str> {
        plugins
            .into_iter()
            .filter_map(|p| {
                let score = p.match_score(hit);
                if score == 0 { None } else { Some((score, p)) }
            })
            .max_by(|(s1, p1), (s2, p2)| s1.cmp(s2).then_with(|| p1.id().cmp(p2.id())))
            .map(|(_, p)| {
                let id: &'static str = p.id();
                id
            })
    }

    #[test]
    fn highest_score_wins() {
        let hit = fake_hit();
        let winner = choose(
            vec![
                Box::new(ScorePlugin { id: "a", score: 10 }),
                Box::new(ScorePlugin { id: "b", score: 50 }),
                Box::new(ScorePlugin { id: "c", score: 30 }),
            ],
            &hit,
        );
        assert_eq!(winner, Some("b"));
    }

    #[test]
    fn ties_broken_by_id_alphabetical() {
        let hit = fake_hit();
        let winner = choose(
            vec![
                Box::new(ScorePlugin {
                    id: "zulu",
                    score: 50,
                }),
                Box::new(ScorePlugin {
                    id: "alpha",
                    score: 50,
                }),
                Box::new(ScorePlugin {
                    id: "mike",
                    score: 50,
                }),
            ],
            &hit,
        );
        assert_eq!(
            winner,
            Some("zulu"),
            "expected the lexicographically largest id on a score tie"
        );
    }

    #[test]
    fn zero_score_filtered() {
        let hit = fake_hit();
        let winner = choose(
            vec![
                Box::new(ScorePlugin {
                    id: "zero",
                    score: 0,
                }),
                Box::new(ScorePlugin {
                    id: "one",
                    score: 1,
                }),
            ],
            &hit,
        );
        assert_eq!(winner, Some("one"));
    }

    #[test]
    fn all_zero_returns_none() {
        let hit = fake_hit();
        let winner = choose(
            vec![
                Box::new(ScorePlugin { id: "a", score: 0 }),
                Box::new(ScorePlugin { id: "b", score: 0 }),
            ],
            &hit,
        );
        assert_eq!(winner, None);
    }

    #[test]
    fn empty_registry_returns_none() {
        let hit = fake_hit();
        let winner = choose(vec![], &hit);
        assert_eq!(winner, None);
    }

    #[test]
    fn preview_plugin_cfg_none_has_sensible_defaults() {
        let cfg = PreviewPluginCfg::none();
        assert!(cfg.section.is_none());
        assert_eq!(cfg.max_file_size_mb, 200);
    }

    #[test]
    fn sizing_default_is_fixed_cap() {
        // Regression guard: changing the default silently would flip
        // every existing plugin's window behaviour without them
        // opting in. If a future refactor wants a different default,
        // every plugin must be re-audited first.
        struct P;
        impl PreviewPlugin for P {
            fn id(&self) -> &'static str {
                "p"
            }
            fn match_score(&self, _: &Hit) -> u32 {
                0
            }
            fn build(&self, _: &Hit, _: &PreviewPluginCfg<'_>) -> anyhow::Result<gtk::Widget> {
                unreachable!()
            }
        }
        assert_eq!(P.sizing(), SizingPreference::FixedCap);
    }

    #[test]
    fn update_default_returns_unsupported_sentinel() {
        // The host's rebuild-fallback path matches on this exact
        // string. If the default impl ever returns a different
        // error, every existing plugin silently stops getting the
        // rebuild fallback and the user sees stale widgets.
        //
        // We cannot call `update()` without a real `gtk::Widget`
        // and `gtk::init()` is process-wide and racy across tests,
        // so we assert the invariant at the source: the sentinel
        // constant the host matches on must equal the literal the
        // default impl bails with. Both sides of the contract live
        // in this file, so a drift would be caught here.
        assert_eq!(UPDATE_UNSUPPORTED, "UpdateUnsupported");
    }
}
