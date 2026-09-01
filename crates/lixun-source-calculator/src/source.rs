use crate::detect;
use anyhow::Result;
use lixun_core::{
    Action, Calculation, Category, DocId, Hit, PluginFieldSpec, RowMenuDef, RowMenuItem,
    RowMenuVerb,
};
use lixun_sources::{IndexerSource, MutationSink, QueryContext, SourceContext};

pub struct CalculatorSource;

/// The evaluated-result hit, shared by the `=`-prefixed and the bare
/// arithmetic paths. Primary action copies the result (the value IS
/// the answer); secondary chains it back into the query so Shift+Enter
/// continues the computation.
fn result_hit(calc: Calculation, ctx: &QueryContext) -> Hit {
    Hit {
        id: DocId(format!("calculator:{}", calc.expr)),
        category: Category::Calculator,
        title: calc.result.clone(),
        subtitle: format!("= {}", calc.expr),
        icon_name: Some("accessories-calculator".into()),
        kind_label: Some("Calculator".into()),
        score: 999.0,
        action: Action::CopyText {
            text: calc.result.clone(),
        },
        extract_fail: false,
        sender: None,
        recipients: None,
        body: None,
        secondary_action: Some(Box::new(Action::ReplaceQuery { q: calc.result })),
        source_instance: ctx.instance_id.to_string(),
        row_menu: RowMenuDef::empty(),
        mime: None,
    }
}

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

    fn on_query(&self, query: &str, ctx: &QueryContext) -> Vec<Hit> {
        let Some(rest) = query.strip_prefix('=') else {
            // Bare arithmetic ("2+2" without the `=` prefix): the
            // conservative detector rejects plain words and sentences,
            // so ordinary search queries never produce a calculator
            // hit. `claims_query` stays `=`-gated, so normal index
            // hits still appear alongside this one.
            return match detect::detect(query) {
                Some(calc) => vec![result_hit(calc, ctx)],
                None => Vec::new(),
            };
        };
        let expr = rest.trim();
        if expr.is_empty() {
            return vec![Hit {
                id: DocId("calculator:__placeholder__".into()),
                category: Category::Calculator,
                title: "Calculator".into(),
                subtitle: "Type an expression after =".into(),
                icon_name: Some("accessories-calculator".into()),
                kind_label: Some("Calculator".into()),
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
            }];
        }
        let Some(calc) = detect::detect(expr) else {
            return vec![Hit {
                id: DocId(format!("calculator:invalid:{expr}")),
                category: Category::Calculator,
                title: "Calculator".into(),
                subtitle: format!("Invalid expression: {expr}"),
                icon_name: Some("accessories-calculator".into()),
                kind_label: Some("Calculator".into()),
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
            }];
        };

        vec![result_hit(calc, ctx)]
    }

    fn excludes_from_query_log(&self, query: &str) -> bool {
        query.trim_start().starts_with('=')
    }

    fn claims_query(&self, query: &str) -> bool {
        query.strip_prefix('=').is_some()
    }

    fn claimed_prefix(&self) -> Option<&'static str> {
        Some("=")
    }

    fn row_menu(&self) -> RowMenuDef {
        RowMenuDef {
            items: vec![RowMenuItem {
                label: "Copy result".into(),
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

    fn ctx() -> QueryContext<'static> {
        QueryContext {
            cancel: None,
            instance_id: "calculator",
            state_dir: Path::new("/tmp/lixun-calc-test"),
        }
    }

    #[test]
    fn on_query_without_prefix_rejects_non_math() {
        let src = CalculatorSource;
        // Plain words/sentences must never become calculator hits —
        // the bare-arithmetic path relies on the conservative
        // detector, not on the `=` prefix.
        assert!(src.on_query("firefox", &ctx()).is_empty());
        assert!(src.on_query("hello world", &ctx()).is_empty());
        // `=` after leading whitespace is neither a claimed prefix
        // nor valid math.
        assert!(src.on_query("   = 2+2", &ctx()).is_empty());
        assert!(src.on_query("", &ctx()).is_empty());
    }

    #[test]
    fn on_query_bare_arithmetic_evaluates() {
        let src = CalculatorSource;
        let hits = src.on_query("2+2", &ctx());
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert_eq!(hit.title, "4");
        assert_eq!(hit.category, Category::Calculator);
        assert_eq!(hit.id.0, "calculator:2+2");
        assert!(matches!(&hit.action, Action::CopyText { text } if text == "4"));
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
        // Primary copies the result; secondary chains it back into
        // the query (Shift+Enter continues the computation).
        assert!(matches!(&hit.action, Action::CopyText { text } if text == "4"));
        match hit.secondary_action.as_deref() {
            Some(Action::ReplaceQuery { q }) => assert_eq!(q, "4"),
            other => panic!("expected secondary ReplaceQuery, got {:?}", other),
        }
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
