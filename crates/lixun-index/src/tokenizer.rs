use tantivy::{
    tokenizer::{
        AsciiFoldingFilter, LowerCaser, RemoveLongFilter, SimpleTokenizer, Stemmer, TextAnalyzer,
    },
    Index,
};

// Spotlight tokenizer chain (Wave B T3): stemming is the TAIL step.
// Order is load-bearing — tantivy's Stemmer documents that it expects
// lowercased input, so LowerCaser must precede it. AsciiFoldingFilter
// also runs before the stemmer so accented forms share a stem with
// their ASCII siblings (e.g. "naïve" → "naive" → "naiv"). Adding or
// reordering filters past this point MUST bump INDEX_VERSION — the
// on-disk posting lists depend on the exact output of this chain.
pub fn register_spotlight_tokenizer(index: &Index) {
    let analyzer = TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(RemoveLongFilter::limit(40))
        .filter(LowerCaser)
        .filter(AsciiFoldingFilter)
        .filter(Stemmer::default())
        .build();

    index.tokenizers().register("spotlight", analyzer);
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
