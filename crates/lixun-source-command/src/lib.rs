//! Command source plugin (C5) — user-installable extensions without
//! recompiling.
//!
//! Each `[[command_source]]` entry in `config.toml` declares a local
//! script/program that answers a claimed query prefix (the
//! Ulauncher/rofi-script model):
//!
//! ```toml
//! [[command_source]]
//! name       = "Passwords"
//! prefix     = "pass "
//! argv       = ["/home/me/bin/lixun-pass.sh", "{query}"]
//! timeout_ms = 1500              # optional, default 1500
//! # cwd      = "~/bin"           # optional
//! ```
//!
//! On a claimed query the argv is spawned DIRECTLY (no shell
//! interpolation — `{query}` is substituted as a literal argv token),
//! stdout is parsed as a JSON array or JSON lines of
//! `{title, subtitle?, icon?, action}` objects, and each entry maps
//! onto the generic `Hit`/`Action` vocabulary (`open-uri`,
//! `open-file`, `copy-text`, `exec`). Output size and hit count are
//! bounded; the child is killed on timeout; parse/spawn failures
//! surface as a single error hit rather than silence.
//!
//! User-authored local scripts keep the no-cloud trust boundary: the
//! daemon never fetches anything itself, it only runs what the
//! operator configured.

pub mod factory;
pub mod runner;
pub mod source;
