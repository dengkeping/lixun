//! Query normalization for prefix/acronym matching and query-latch keys.

use unicode_normalization::UnicodeNormalization;

/// Normalize a query string for matching against document titles.
///
/// Steps:
/// 1. NFKD decomposition
/// 2. Strip combining marks (U+0300..=U+036F)
/// 3. Lowercase
/// 4. Trim
/// 5. Collapse internal whitespace to single ASCII space
///
/// Tantivy operators (`-`, `|`, `"`, `+`) are preserved.
#[must_use]
pub fn normalize_for_match(q: &str) -> String {
    let decomposed: String = q.nfkd().collect();

    let filtered: String = decomposed
        .chars()
        .filter(|&c| !is_combining_mark(c))
        .collect::<String>()
        .to_lowercase();

    let trimmed = filtered.trim();
    collapse_whitespace(trimmed)
}

/// Normalize a query string for use as a latch key.
///
/// Steps:
/// 1. Apply `normalize_for_match`
/// 2. Strip leading runs of `-`, `+`, `"`
/// 3. Strip trailing runs of `-`, `+`, `"`
/// 4. Remove any remaining internal `"` characters
///
/// Word order is preserved (not sorted).
#[must_use]
pub fn normalize_for_latch_key(q: &str) -> String {
    let normalized = normalize_for_match(q);

    let without_leading = normalized.trim_start_matches(['-', '+', '"']);
    let without_trailing = without_leading.trim_end_matches(['-', '+', '"']);

    without_trailing.replace('"', "")
}

/// Check if a character is a combining mark (U+0300..=U+036F).
#[inline]
fn is_combining_mark(c: char) -> bool {
    matches!(c, '\u{0300}'..='\u{036F}')
}

/// Collapse runs of whitespace to a single ASCII space.
fn collapse_whitespace(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut prev_was_whitespace = false;

    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_was_whitespace {
                result.push(' ');
                prev_was_whitespace = true;
            }
        } else {
            result.push(c);
            prev_was_whitespace = false;
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURES_MATCH: &[(&str, &str)] = &[
        ("", ""),
        ("   ", ""),
        ("Café", "cafe"),
        ("RÉSUMÉ", "resume"),
        ("Foo   Bar", "foo bar"),
        ("-foo", "-foo"),
        ("naïve", "naive"),
        ("日本語", "日本語"),
    ];

    const FIXTURES_LATCH_KEY: &[(&str, &str)] = &[
        ("", ""),
        ("-foo", "foo"),
        ("+foo", "foo"),
        ("\"foo bar\"", "foo bar"),
        ("foo bar", "foo bar"),
        ("bar foo", "bar foo"),
        ("Café", "cafe"),
    ];

    #[test]
    fn test_normalize_for_match() {
        for (input, expected) in FIXTURES_MATCH {
            let actual = normalize_for_match(input);
            assert_eq!(
                &actual, *expected,
                "normalize_for_match({:?}) expected {:?}, got {:?}",
                input, expected, actual
            );
        }
    }

    #[test]
    fn test_normalize_for_latch_key() {
        for (input, expected) in FIXTURES_LATCH_KEY {
            let actual = normalize_for_latch_key(input);
            assert_eq!(
                &actual, *expected,
                "normalize_for_latch_key({:?}) expected {:?}, got {:?}",
                input, expected, actual
            );
        }
    }
}
