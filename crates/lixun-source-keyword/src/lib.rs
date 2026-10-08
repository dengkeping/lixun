//! Keyword web-search source plugin (C3b).
//!
//! `[[search.keyword]]` entries in `config.toml` declare shortcut
//! keys that route a query to a web search URL:
//!
//! ```toml
//! [[search.keyword]]
//! key  = "gh"
//! url  = "https://github.com/search?q={query}"
//! name = "GitHub"            # optional; defaults to the key
//! ```
//!
//! A query whose first token equals a configured key (`gh rust
//! lifetimes`) is claimed exclusively — the same mechanism the
//! calculator uses for `=` — and produces a single hit ("Search
//! GitHub for \u{201c}rust lifetimes\u{201d}") whose primary action
//! is `Action::OpenUri`. The browser opens only on explicit
//! activation; nothing is fetched at query time, preserving the
//! local-only ethos.
//!
//! Config-gated: without a `[search]` section carrying `keyword`
//! entries the factory registers nothing.

pub mod factory;
pub mod source;
