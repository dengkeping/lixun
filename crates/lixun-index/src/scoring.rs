use crate::normalize::normalize_for_match;
use lixun_core::Category;

const SECONDS_PER_DAY: f32 = 86_400.0;

/// Applies the prefix-match multiplier if the normalized title starts with
/// the already-normalized query. Both inputs must already be run through
/// `normalize_for_match`. Returns `weight` on match, `1.0` otherwise.
#[must_use]
pub fn prefix_mult(title_norm: &str, q_norm: &str, weight: f32) -> f32 {
    if q_norm.is_empty() {
        return 1.0;
    }

    if title_norm.starts_with(q_norm) {
        weight
    } else {
        1.0
    }
}

/// Computes the acronym initials of `title` per D4 (VSCode-style), normalizes
/// them via `normalize_for_match`, and returns `weight` if the initials string
/// starts with `q_norm`. Empty query returns `1.0`.
#[must_use]
pub fn acronym_mult(title: &str, q_norm: &str, weight: f32) -> f32 {
    if q_norm.is_empty() {
        return 1.0;
    }

    let initials = normalize_for_match(&acronym_initials(title));
    if initials.starts_with(q_norm) {
        weight
    } else {
        1.0
    }
}

/// Returns `1.0 + weight * exp(-age_days / tau_days)` for File/Mail, else `1.0`.
/// Future-dated mtime (age<0) is treated as age=0. tau_days must be > 0.
#[must_use]
pub fn recency_mult(
    category: Category,
    mtime_secs: i64,
    now_secs: i64,
    weight: f32,
    tau_days: f32,
) -> f32 {
    if !matches!(category, Category::File | Category::Mail) {
        return 1.0;
    }

    assert!(tau_days > 0.0, "tau_days must be > 0");

    let age_days = (now_secs - mtime_secs).max(0) as f32 / SECONDS_PER_DAY;
    1.0 + weight * (-age_days / tau_days).exp()
}

/// Splits a title into initials per D4 rules. Lowercased, no separators.
/// Empty input → empty string. Used by `acronym_mult` and unit-tested
/// directly with the fixtures from the plan.
#[must_use]
pub fn acronym_initials(title: &str) -> String {
    let mut initials = String::new();
    let mut word_start = None;

    for (idx, ch) in title.char_indices() {
        if is_separator(ch) {
            if let Some(start) = word_start.take() {
                push_word_initials(&title[start..idx], &mut initials);
            }
            continue;
        }

        if word_start.is_none() {
            word_start = Some(idx);
        }
    }

    if let Some(start) = word_start {
        push_word_initials(&title[start..], &mut initials);
    }

    initials
}

fn is_separator(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '_' | '-' | '.' | '/')
}

fn push_word_initials(word: &str, initials: &mut String) {
    let chars: Vec<(usize, char)> = word.char_indices().collect();
    if chars.is_empty() {
        return;
    }

    for (idx, &(_, ch)) in chars.iter().enumerate() {
        if !ch.is_alphabetic() {
            continue;
        }

        let starts_subword = if idx == 0 {
            true
        } else {
            let prev = chars[idx - 1].1;
            let next = chars.get(idx + 1).map(|(_, next)| *next);

            (prev.is_alphabetic() && prev.is_lowercase() && ch.is_uppercase())
                || (prev.is_alphabetic()
                    && ch.is_uppercase()
                    && next.is_some_and(|next| next.is_alphabetic() && next.is_lowercase()))
        };

        if starts_subword {
            initials.extend(ch.to_lowercase());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LixunIndex;
    use lixun_core::{Action, DocId, Document, Query, RankingConfig};

    const DAY: i64 = 86_400;

    #[test]
    fn acronym_fixtures() {
        let cases = [
            ("JSONParser", "jp"),
            ("XMLHttpRequest", "xhr"),
            ("parseURL", "pu"),
            ("iPhone", "ip"),
            ("snake_case", "sc"),
            ("kebab-case", "kc"),
            ("Café Pro", "cp"),
            ("Firefox", "f"),
            ("Google Image Capture", "gic"),
            ("Visual Studio Code", "vsc"),
            ("", ""),
            ("  ", ""),
            ("A", "a"),
            ("ABC", "a"),
        ];

        for (title, expected) in cases {
            assert_eq!(acronym_initials(title), expected, "title: {title:?}");
        }
    }

    #[test]
    fn prefix_and_unicode() {
        assert_eq!(prefix_mult("firefox", "fire", 1.4), 1.4);
        assert_eq!(prefix_mult("campfire", "fire", 1.4), 1.0);
        assert_eq!(prefix_mult("café pro", "caf", 1.4), 1.4);
    }

    #[test]
    fn recency_orders_ties() {
        let now = chrono::Utc::now().timestamp();
        let ranking = RankingConfig {
            apps: 1.0,
            files: 1.0,
            mail: 1.0,
            attachments: 1.0,
            prefix_boost: 1.0,
            acronym_boost: 1.0,
            recency_weight: 0.2,
            recency_tau_days: 30.0,
            ..RankingConfig::default()
        };

        let file_docs = vec![
            sample_document("fs:/tmp/report-new.txt", Category::File, "report", now),
            sample_document(
                "fs:/tmp/report-old.txt",
                Category::File,
                "report",
                now - 60 * DAY,
            ),
        ];
        let file_results = search_titles(&file_docs, ranking.clone());
        assert_eq!(file_results.len(), 2);
        let newer = file_results
            .iter()
            .find(|hit| hit.id.0 == "fs:/tmp/report-new.txt")
            .unwrap();
        let older = file_results
            .iter()
            .find(|hit| hit.id.0 == "fs:/tmp/report-old.txt")
            .unwrap();
        assert!(newer.score > older.score);

        let app_order_a = search_titles(
            &[
                sample_document("app:new", Category::App, "report", now),
                sample_document("app:old", Category::App, "report", now - 60 * DAY),
            ],
            ranking.clone(),
        )
        .into_iter()
        .map(|hit| hit.id.0)
        .collect::<Vec<_>>();
        let app_order_b = search_titles(
            &[
                sample_document("app:new", Category::App, "report", now - 60 * DAY),
                sample_document("app:old", Category::App, "report", now),
            ],
            ranking,
        )
        .into_iter()
        .map(|hit| hit.id.0)
        .collect::<Vec<_>>();

        assert_eq!(app_order_a, app_order_b);
    }

    fn search_titles(docs: &[Document], ranking: RankingConfig) -> Vec<lixun_core::Hit> {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().to_str().unwrap();
        let mut index = LixunIndex::create_or_open(path, ranking).unwrap();
        let mut writer = index.writer(20_000_000).unwrap();

        for doc in docs {
            index.upsert(doc, &mut writer).unwrap();
        }

        index.commit(&mut writer).unwrap();

        index
            .search(&Query {
                text: "report".to_string(),
                limit: 10,
            })
            .unwrap()
    }

    fn sample_document(id: &str, category: Category, title: &str, mtime: i64) -> Document {
        Document {
            id: DocId(id.to_string()),
            category,
            title: title.to_string(),
            subtitle: id.to_string(),
            icon_name: None,
            kind_label: None,
            body: Some(title.to_string()),
            path: id.to_string(),
            mtime,
            size: 100,
            action: Action::OpenFile { path: id.into() },
            extract_fail: false,
            sender: None,
            recipients: None,
            source_instance: "test".into(),
            extra: Vec::new(),
        }
    }
}
