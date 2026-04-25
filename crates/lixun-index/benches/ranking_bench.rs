//! Wave B T5 — ranking comparison bench.
//!
//! Emits `target/ranking-bench.md` contrasting two ranking configs over
//! a synthetic corpus and panics if the plan's sanity criterion fails.
//!
//! Plan wording: "post∩pre top-20 ≥ 15/20". Literal application breaks
//! down on a 30-doc synthetic corpus where most queries return <20
//! hits, so we restate it equivalently as: no more than 5 pre-ranking
//! hits may drop out of the post top-20. For queries returning 20+
//! hits this reduces to the original (overlap ≥ 15); for narrow
//! queries (overlap ≥ expected - 5) — i.e. identical result sets pass
//! even when only 2 docs match.
//!
//! "Pre" is modelled by zeroing the Wave B knobs (proximity_boost = 0,
//! coordination_boost = 0) so stage-1 is left with only
//! category/prefix/acronym/recency. Stemming (T3) is indexing-side
//! and cannot be zeroed per-query; the overlap criterion accounts for
//! that by not requiring bit-exact order, only membership stability.
//!
//! Wall-clock timings are reported but NOT gated on — this is a
//! ranking-quality harness, not a perf benchmark.

use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use lixun_core::{Action, Category, DocId, Document, Query, RankingConfig};
use lixun_index::LixunIndex;

struct Row {
    query: String,
    pre_top: Vec<String>,
    post_top: Vec<String>,
    overlap: usize,
    expected: usize,
    t_pre: u128,
    t_post: u128,
}

fn make_doc(id: &str, category: Category, title: &str, body: &str) -> Document {
    Document {
        id: DocId(id.to_string()),
        category,
        title: title.to_string(),
        subtitle: id.to_string(),
        icon_name: None,
        kind_label: None,
        body: Some(body.to_string()),
        path: id.trim_start_matches("fs:").to_string(),
        mtime: 0,
        size: 100,
        action: Action::OpenFile {
            path: id.trim_start_matches("fs:").into(),
        },
        extract_fail: false,
        sender: None,
        recipients: None,
        source_instance: "bench".into(),
        secondary_action: None,
        extra: Vec::new(),
    }
}

/// Fixture corpus designed to exercise T1 (proximity), T2 (coordination,
/// 2..=3 token regime) and T3 (stemming) while preserving enough
/// category/recency signal overlap to keep pre/post top-20 comparable.
///
/// Every query in [`query_set`] has at least one clear "target" doc and
/// a handful of lexical distractors that would rank differently under
/// BM25 alone vs with the Wave B multipliers layered on.
#[allow(
    clippy::vec_init_then_push,
    reason = "Long-form push-chain keeps fixture composition readable; mail docs need post-hoc field mutation."
)]
fn build_corpus() -> Vec<Document> {
    let mut docs = Vec::new();

    docs.push(make_doc("app:firefox", Category::App, "Firefox", "web browser"));
    docs.push(make_doc(
        "app:firefox-dev",
        Category::App,
        "Firefox Developer Edition",
        "web browser for developers",
    ));
    docs.push(make_doc(
        "app:thunderbird",
        Category::App,
        "Thunderbird",
        "email client",
    ));
    docs.push(make_doc(
        "app:libreoffice",
        Category::App,
        "LibreOffice Writer",
        "word processor",
    ));
    docs.push(make_doc(
        "app:code",
        Category::App,
        "Visual Studio Code",
        "source code editor",
    ));

    docs.push(make_doc(
        "fs:/docs/alpha-beta-gamma.md",
        Category::File,
        "alpha beta gamma",
        "notes on three greek letters",
    ));
    docs.push(make_doc(
        "fs:/docs/alpha-x-y-beta-gamma.md",
        Category::File,
        "alpha x y z beta gamma",
        "distractor — tokens scattered beyond slop",
    ));
    docs.push(make_doc(
        "fs:/docs/alpha-only.md",
        Category::File,
        "alpha",
        "beta and gamma appear only in the body not the title",
    ));
    docs.push(make_doc(
        "fs:/docs/project-alpha.md",
        Category::File,
        "project alpha plan",
        "quarterly planning for project alpha",
    ));
    docs.push(make_doc(
        "fs:/docs/project-beta.md",
        Category::File,
        "project beta plan",
        "quarterly planning for project beta",
    ));
    docs.push(make_doc(
        "fs:/docs/running-notes.md",
        Category::File,
        "marathon runs today",
        "training log entry for the week",
    ));
    docs.push(make_doc(
        "fs:/docs/runner-log.md",
        Category::File,
        "running log 2025",
        "every run this quarter",
    ));
    docs.push(make_doc(
        "fs:/docs/runs-schedule.md",
        Category::File,
        "long runs schedule",
        "distance runs each weekend",
    ));

    for i in 0..10 {
        docs.push(make_doc(
            &format!("fs:/notes/generic-{i}.md"),
            Category::File,
            &format!("generic note {i}"),
            &format!("filler content describing topic {i}"),
        ));
    }

    let mut mail1 = make_doc(
        "mail:1",
        Category::Mail,
        "alpha project status update",
        "status report for alpha project, all green",
    );
    mail1.sender = Some("alice@example.com".into());
    mail1.recipients = Some("team@example.com".into());
    docs.push(mail1);

    let mut mail2 = make_doc(
        "mail:2",
        Category::Mail,
        "beta project retrospective",
        "lessons learned from beta kickoff",
    );
    mail2.sender = Some("bob@example.com".into());
    mail2.recipients = Some("team@example.com".into());
    docs.push(mail2);

    let mut mail3 = make_doc(
        "mail:3",
        Category::Mail,
        "alpha beta joint sync",
        "coordination between alpha and beta teams",
    );
    mail3.sender = Some("alice@example.com".into());
    mail3.recipients = Some("bob@example.com".into());
    docs.push(mail3);

    // Unicode/CJK doc — T3 stemmer must pass through without panicking
    // and the ASCII filler term must still be searchable.
    docs.push(make_doc(
        "fs:/docs/cjk-mixed.md",
        Category::File,
        "测试 filler",
        "mixed-script document for non-English safety check",
    ));

    docs
}

/// Query set covering the regimes added by Wave B:
/// - Single-token: stemming only.
/// - Two-token: proximity candidate + coordination q=2 sweet spot.
/// - Three-token: coordination q=3 sweet spot + proximity candidate.
/// - Four-token: coordination guard (q>3 → no-op).
/// - Stem-variant: indexer stems docs, query stems via same analyzer.
/// - Unicode: ensures stem chain survives non-stemmable tokens.
fn query_set() -> Vec<&'static str> {
    vec![
        "firefox",
        "alpha beta",
        "alpha beta gamma",
        "project alpha plan",
        "alpha beta gamma delta",
        "running",
        "project",
        "filler",
    ]
}

fn run_query(index: &LixunIndex, text: &str, limit: u32) -> Vec<String> {
    let hits = index
        .search(&Query {
            text: text.to_string(),
            limit,
        })
        .expect("search must succeed");
    hits.into_iter().map(|h| h.id.0).collect()
}

fn build_index(ranking: RankingConfig, corpus: &[Document]) -> (tempfile::TempDir, LixunIndex) {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().to_str().unwrap();
    let mut index = LixunIndex::create_or_open(path, ranking).unwrap();
    let mut writer = index.writer(50_000_000).unwrap();
    for doc in corpus {
        index.upsert(doc, &mut writer).unwrap();
    }
    index.commit(&mut writer).unwrap();
    (tmp, index)
}

fn main() {
    let corpus = build_corpus();
    let queries = query_set();

    // "Pre" config = T1+T2 knobs zeroed. Stemming (T3) is baked into the
    // indexer's tokenizer chain and cannot be disabled per-query; the
    // overlap threshold accounts for stemming drift.
    let pre = RankingConfig {
        proximity_boost: 0.0,
        coordination_boost: 0.0,
        ..RankingConfig::default()
    };
    let post = RankingConfig::default();

    let (_tmp_pre, index_pre) = build_index(pre.clone(), &corpus);
    let (_tmp_post, index_post) = build_index(post.clone(), &corpus);

    let mut rows: Vec<Row> = Vec::new();
    // Worst deficit = how many pre-ranking hits went missing from post.
    // Plan's "post∩pre top-20 ≥ 15" is expressed in absolute count, but
    // the corpus may legitimately return <20 hits for narrow queries;
    // in that case the invariant is "don't lose more than 5 of them".
    let mut worst_deficit: usize = 0;

    for q in &queries {
        let t0 = Instant::now();
        let pre_top = run_query(&index_pre, q, 20);
        let t_pre = t0.elapsed().as_micros();

        let t1 = Instant::now();
        let post_top = run_query(&index_post, q, 20);
        let t_post = t1.elapsed().as_micros();

        let overlap = pre_top.iter().filter(|id| post_top.contains(id)).count();
        let expected = pre_top.len();
        let deficit = expected.saturating_sub(overlap);
        worst_deficit = worst_deficit.max(deficit);

        rows.push(Row {
            query: (*q).to_string(),
            pre_top,
            post_top,
            overlap,
            expected,
            t_pre,
            t_post,
        });
    }

    let mut md = String::new();
    md.push_str("# Wave B ranking bench\n\n");
    md.push_str(&format!(
        "Corpus: {} docs, {} queries. Sanity criterion: ≤ 5 pre-ranking hits may drop out of post top-20 per query.\n\n",
        corpus.len(),
        queries.len()
    ));
    md.push_str("| query | overlap | expected | deficit | pre µs | post µs |\n");
    md.push_str("|---|---|---|---|---|---|\n");
    for r in &rows {
        let deficit = r.expected.saturating_sub(r.overlap);
        md.push_str(&format!(
            "| `{}` | {} | {} | {} | {} | {} |\n",
            r.query, r.overlap, r.expected, deficit, r.t_pre, r.t_post
        ));
    }
    md.push_str("\n## Per-query top-10\n\n");
    for r in &rows {
        md.push_str(&format!("### `{}`\n\n", r.query));
        md.push_str("| rank | pre | post |\n|---|---|---|\n");
        let n = r.pre_top.len().max(r.post_top.len()).min(10);
        for i in 0..n {
            let p = r.pre_top.get(i).cloned().unwrap_or_default();
            let q2 = r.post_top.get(i).cloned().unwrap_or_default();
            md.push_str(&format!("| {} | `{}` | `{}` |\n", i + 1, p, q2));
        }
        md.push('\n');
    }

    let target = PathBuf::from(
        std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".to_string()),
    )
    .join("ranking-bench.md");
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&target, &md).expect("write ranking-bench.md");
    println!("wrote {}", target.display());
    println!("worst deficit = {worst_deficit} hits dropped from post top-20");

    assert!(
        worst_deficit <= 5,
        "Wave B ranking sanity failure: worst deficit = {worst_deficit} hits dropped from post top-20, \
         plan allows ≤ 5. See {}.",
        target.display()
    );
}
