//! A text-quote address is relative to a hit's stable document id.
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TextFragment {
    pub quote: String,
}

pub struct PassageMatcher {
    phrase: Option<Regex>,
    terms: Regex,
}

impl PassageMatcher {
    pub fn new(query: &str) -> Option<Self> {
        let query = query.trim();
        if query.is_empty() || query.len() > 4096 {
            return None;
        }
        let compile = |pattern: &str| {
            RegexBuilder::new(pattern)
                .case_insensitive(true)
                .build()
                .ok()
        };
        let phrase = (!crate::query_uses_operators(query) && query.contains(char::is_whitespace))
            .then(|| compile(&regex::escape(query)))
            .flatten();
        let mut terms = Vec::new();
        let mut include_quote = true;
        for (i, part) in query.split('"').enumerate() {
            if i % 2 == 1 {
                if include_quote && !part.is_empty() {
                    terms.push(regex::escape(part));
                }
            } else {
                let prefix = part.split_whitespace().last().unwrap_or("");
                include_quote = part.ends_with(char::is_whitespace)
                    || !(prefix.starts_with('-') || (prefix.ends_with(':') && prefix != "body:"));
                for term in part.split_whitespace() {
                    let term = term.strip_prefix("body:").unwrap_or(term);
                    if term.starts_with('-')
                        || term.contains(':')
                        || matches!(term, "AND" | "OR" | "|")
                    {
                        continue;
                    }
                    let term = term.trim_start_matches('+');
                    if !term.is_empty() {
                        terms.push(regex::escape(term));
                    }
                }
            }
        }
        if terms.is_empty() {
            return None;
        }
        Some(Self {
            phrase,
            terms: compile(&terms.join("|"))?,
        })
    }

    pub fn find<'a>(&self, body: &'a str) -> Option<(&'a str, TextFragment)> {
        let matched = self
            .phrase
            .as_ref()
            .and_then(|phrase| phrase.find(body))
            .or_else(|| self.terms.find(body))?;
        let before = &body[..matched.start()];
        let start = before
            .char_indices()
            .rev()
            .take(48)
            .last()
            .map_or(0, |(i, _)| i);
        let end = body[matched.end()..]
            .char_indices()
            .nth(92)
            .map_or(body.len(), |(i, _)| matched.end() + i);
        Some((
            &body[start..end],
            TextFragment {
                quote: matched.as_str().into(),
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prefers_whole_passage_and_respects_fields() {
        let body = "garden notes, followed by garden party";
        assert_eq!(
            PassageMatcher::new("garden party")
                .unwrap()
                .find(body)
                .unwrap()
                .1
                .quote,
            "garden party"
        );
        assert_eq!(
            PassageMatcher::new(r#"title:"garden" -body:"notes" body:party"#)
                .unwrap()
                .find(body)
                .unwrap()
                .1
                .quote,
            "party"
        );
    }

    #[test]
    fn quoted_unicode_passage_after_long_prefix() {
        let body = format!("{}На второй странице есть Жёлтая утка.", "界".repeat(9000));
        let (snippet, fragment) = PassageMatcher::new("\"жёлтая утка\"")
            .unwrap()
            .find(&body)
            .unwrap();
        assert_eq!(fragment.quote, "Жёлтая утка");
        assert!(snippet.contains("второй странице"));
        assert!(snippet.chars().count() < 100);
    }
}
