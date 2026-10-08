//! Keyword web-search source: one instance per configured key.

use anyhow::Result;
use lixun_core::{Action, Category, DocId, Hit, RowMenuDef, RowMenuItem, RowMenuVerb};
use lixun_sources::{IndexerSource, MutationSink, QueryContext, SourceContext};

pub struct KeywordSource {
    /// The bare shortcut key (`"gh"`).
    pub key: String,
    /// Search URL template containing a `{query}` placeholder.
    pub url: String,
    /// User-facing engine name (`"GitHub"`); defaults to the key.
    pub name: String,
    /// `"<key> "` leaked to `'static` once at build so
    /// [`IndexerSource::claimed_prefix`] can expose it to the GUI's
    /// spinner suppression. Bounded by the number of configured
    /// keyword entries, allocated once per daemon lifetime.
    pub claimed: &'static str,
}

/// Minimal percent-encoding for a query interpolated into a URL
/// query-string position (RFC 3986 unreserved set kept verbatim,
/// space as `+`). Mirrors the launcher-side encoder so both web
/// paths build identical URLs.
pub fn urlencode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

impl KeywordSource {
    fn render_url(&self, terms: &str) -> String {
        self.url.replace("{query}", &urlencode(terms))
    }

    fn search_hit(&self, terms: &str, ctx: &QueryContext) -> Hit {
        let uri = self.render_url(terms);
        Hit {
            id: DocId(format!("keyword:{}:{terms}", self.key)),
            category: Category::File,
            title: format!("Search {} for \u{201c}{terms}\u{201d}", self.name),
            subtitle: uri.clone(),
            icon_name: Some("web-browser".into()),
            kind_label: Some("Web Search".into()),
            score: 999.0,
            action: Action::OpenUri { uri },
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

    fn placeholder_hit(&self, query: &str, ctx: &QueryContext) -> Hit {
        Hit {
            id: DocId(format!("keyword:{}:__placeholder__", self.key)),
            category: Category::File,
            title: format!("Search {}", self.name),
            subtitle: format!("Type a search term after \u{201c}{}\u{201d}", self.key),
            icon_name: Some("web-browser".into()),
            kind_label: Some("Web Search".into()),
            score: 999.0,
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
}

impl IndexerSource for KeywordSource {
    fn kind(&self) -> &'static str {
        "keyword"
    }

    fn reindex_full(&self, _ctx: &SourceContext, _sink: &dyn MutationSink) -> Result<()> {
        Ok(())
    }

    fn on_query(&self, query: &str, ctx: &QueryContext) -> Vec<Hit> {
        // Claim boundary: the key followed by whitespace. A bare
        // `gh` (no space) is NOT claimed so ordinary searches that
        // happen to start with the key keep working.
        let Some(rest) = query.strip_prefix(self.claimed) else {
            return Vec::new();
        };
        let terms = rest.trim();
        if terms.is_empty() {
            return vec![self.placeholder_hit(query, ctx)];
        }
        vec![self.search_hit(terms, ctx)]
    }

    fn claims_query(&self, query: &str) -> bool {
        query.starts_with(self.claimed)
    }

    fn claimed_prefix(&self) -> Option<&'static str> {
        Some(self.claimed)
    }

    fn row_menu(&self) -> RowMenuDef {
        RowMenuDef {
            items: vec![RowMenuItem {
                label: "Copy URL".into(),
                verb: RowMenuVerb::Copy,
                visibility: Default::default(),
            }],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn src() -> KeywordSource {
        KeywordSource {
            key: "gh".into(),
            url: "https://github.com/search?q={query}".into(),
            name: "GitHub".into(),
            claimed: "gh ",
        }
    }

    fn ctx() -> QueryContext<'static> {
        QueryContext {
            cancel: None,
            instance_id: "keyword:gh",
            state_dir: Path::new("/tmp/lixun-keyword-test"),
        }
    }

    #[test]
    fn claims_only_key_plus_space() {
        let s = src();
        assert!(s.claims_query("gh rust"));
        assert!(s.claims_query("gh "));
        assert!(!s.claims_query("gh"), "bare key must stay a normal search");
        assert!(!s.claims_query("ghost in the shell"));
        assert!(!s.claims_query("hub gh"));
    }

    #[test]
    fn on_query_builds_encoded_url_hit() {
        let s = src();
        let hits = s.on_query("gh rust lifetimes & bounds", &ctx());
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert_eq!(
            hit.title,
            "Search GitHub for \u{201c}rust lifetimes & bounds\u{201d}"
        );
        match &hit.action {
            Action::OpenUri { uri } => {
                assert_eq!(
                    uri,
                    "https://github.com/search?q=rust+lifetimes+%26+bounds"
                );
            }
            other => panic!("expected OpenUri, got {other:?}"),
        }
        assert_eq!(hit.kind_label.as_deref(), Some("Web Search"));
    }

    #[test]
    fn empty_terms_yield_placeholder() {
        let s = src();
        let hits = s.on_query("gh ", &ctx());
        assert_eq!(hits.len(), 1);
        assert!(matches!(hits[0].action, Action::ReplaceQuery { .. }));
    }

    #[test]
    fn unclaimed_query_yields_nothing() {
        let s = src();
        assert!(s.on_query("firefox", &ctx()).is_empty());
        assert!(s.on_query("gh", &ctx()).is_empty());
    }

    #[test]
    fn urlencode_matches_expected_shapes() {
        assert_eq!(urlencode("hello world"), "hello+world");
        assert_eq!(urlencode("a&b=c"), "a%26b%3Dc");
        assert_eq!(urlencode("caf\u{e9}"), "caf%C3%A9");
    }
}
