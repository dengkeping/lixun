use tantivy::{
    Index,
    tokenizer::{
        AsciiFoldingFilter, LowerCaser, RemoveLongFilter, SimpleTokenizer, Stemmer, TextAnalyzer,
    },
};

// Spotlight tokenizer chains. Two analyzers are registered:
//
// - "spotlight": NO stemmer. Used for title, title_terms,
//   title_initials, path, sender, recipients — fields dominated by
//   proper nouns, filenames and identifiers, where an English stemmer
//   corrupts terms ("parsing" → "pars") and causes both false matches
//   and misses. Also used to tokenize queries against those fields.
// - "spotlight_stem": the stemmed variant, used ONLY for `body`,
//   where natural-language morphology ("running" vs "runs") genuinely
//   helps recall.
//
// Order is load-bearing — tantivy's Stemmer documents that it expects
// lowercased input, so LowerCaser must precede it. AsciiFoldingFilter
// also runs before the stemmer so accented forms share a stem with
// their ASCII siblings (e.g. "naïve" → "naive" → "naiv"). Adding or
// reordering filters past this point MUST bump INDEX_VERSION — the
// on-disk posting lists depend on the exact output of these chains.
pub fn register_spotlight_tokenizer(index: &Index) {
    let unstemmed = TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(RemoveLongFilter::limit(40))
        .filter(LowerCaser)
        .filter(AsciiFoldingFilter)
        .build();
    index.tokenizers().register("spotlight", unstemmed);

    let stemmed = TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(RemoveLongFilter::limit(40))
        .filter(LowerCaser)
        .filter(AsciiFoldingFilter)
        .filter(Stemmer::default())
        .build();
    index.tokenizers().register("spotlight_stem", stemmed);
}

pub fn split_identifiers(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut output = String::with_capacity(input.len() + 8);

    for (index, ch) in chars.iter().copied().enumerate() {
        match ch {
            '_' | '-' => output.push(' '),
            _ if ch.is_whitespace() => output.push(' '),
            _ => {
                if ch.is_uppercase() && index > 0 {
                    let previous = chars[index - 1];
                    let next = chars.get(index + 1).copied();
                    let starts_new_word = previous.is_lowercase()
                        || previous.is_ascii_digit()
                        || (previous.is_uppercase()
                            && next.is_some_and(|next| next.is_lowercase()));

                    if starts_new_word && !output.ends_with(' ') {
                        output.push(' ');
                    }
                }

                output.push(ch);
            }
        }
    }

    output
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::split_identifiers;

    #[test]
    fn test_split_identifiers_examples() {
        let cases = [
            ("MyFileName", "my file name"),
            ("my_file_name", "my file name"),
            ("my-file-name", "my file name"),
            ("XMLHttpRequest", "xml http request"),
            ("simple", "simple"),
            ("Already Spaces", "already spaces"),
            ("snake_case_ID", "snake case id"),
        ];

        for (input, expected) in cases {
            assert_eq!(split_identifiers(input), expected, "input: {input}");
        }
    }
}
