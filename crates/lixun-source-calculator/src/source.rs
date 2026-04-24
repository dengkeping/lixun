use crate::detect;
use anyhow::Result;
use lixun_core::{Action, Category, DocId, Hit, PluginFieldSpec};
use lixun_sources::{IndexerSource, MutationSink, QueryContext, SourceContext};

pub struct CalculatorSource;

impl IndexerSource for CalculatorSource {
    fn kind(&self) -> &'static str {
        "calculator"
    }

    fn extra_fields(&self) -> &'static [PluginFieldSpec] {
        &[]
    }

    fn reindex_full(&self, _ctx: &SourceContext, _sink: &dyn MutationSink) -> Result<()> {
        Ok(())
    }

    fn on_query(&self, query: &str, _ctx: &QueryContext) -> Vec<Hit> {
        let Some(rest) = query.strip_prefix('=') else {
            return Vec::new();
        };
        let expr = rest.trim();
        let Some(calc) = detect::detect(expr) else {
            return Vec::new();
        };

        vec![Hit {
            id: DocId(format!("calculator:{}", calc.expr)),
            category: Category::Calculator,
            title: calc.result.clone(),
            subtitle: format!("= {}", calc.expr),
            icon_name: Some("accessories-calculator".into()),
            kind_label: Some("Calculator".into()),
            score: 999.0,
            action: Action::ReplaceQuery { q: calc.result },
            extract_fail: false,
            sender: None,
            recipients: None,
            body: None,
            secondary_action: None,
        }]
    }

    fn excludes_from_query_log(&self, query: &str) -> bool {
        query.trim_start().starts_with('=')
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn ctx() -> QueryContext<'static> {
        QueryContext {
            instance_id: "calculator",
            state_dir: Path::new("/tmp/lixun-calc-test"),
        }
    }

    #[test]
    fn on_query_without_prefix_returns_empty() {
        let src = CalculatorSource;
        assert!(src.on_query("2+2", &ctx()).is_empty());
        assert!(src.on_query("   = 2+2", &ctx()).is_empty());
        assert!(src.on_query("firefox", &ctx()).is_empty());
        assert!(src.on_query("", &ctx()).is_empty());
    }

    #[test]
    fn on_query_with_prefix_returns_hit() {
        let src = CalculatorSource;
        let hits = src.on_query("= 2+2", &ctx());
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert_eq!(hit.title, "4");
        assert_eq!(hit.category, Category::Calculator);
        assert_eq!(hit.score, 999.0);
        assert_eq!(hit.id.0, "calculator:2+2");
        assert_eq!(hit.subtitle, "= 2+2");
    }

    #[test]
    fn excludes_from_query_log_matches_trigger() {
        let src = CalculatorSource;
        assert!(src.excludes_from_query_log("= 2+2"));
        assert!(src.excludes_from_query_log("  = foo"));
        assert!(!src.excludes_from_query_log("hello"));
        assert!(!src.excludes_from_query_log(""));
    }
}
