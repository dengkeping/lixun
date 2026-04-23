//! Top Hit selection (Wave A, D5).
//!
//! Pure-function module: the Search handler builds the final
//! scored-and-sorted `Vec<Hit>` and hands it here together with
//! read-only borrows of the frecency / latch stores and the
//! configured thresholds. The module decides whether `hits[0]`
//! is confident enough to promote to the hero Top Hit slot.
//!
//! Design per the Spotlight Wave A ranking plan, "Top Hit
//! selection (D5)":
//!
//! - `confidence ∈ {0.0, 1.0}` is the max of three binary-ish signals
//!   (prefix/acronym match, strong latch, frecency dominance).
//! - `margin` is the score ratio between `hits[0]` and `hits[1]`,
//!   treated as `+INF` when there is no runner-up.
//! - Promotion requires BOTH `confidence >= min_confidence` AND
//!   `margin >= min_margin`; either gate failing returns `None`.
//!
//! The module owns no state, does no I/O, and knows nothing about
//! plugin categories — it looks at the candidate's title, frecency
//! buckets, and latch counters, which are uniform across every
//! source plugin.

use crate::frecency::FrecencyStore;
use crate::query_latch::QueryLatchStore;
use lixun_core::{DocId, Hit};
use lixun_index::{
    normalize::normalize_for_match,
    scoring::{acronym_mult, prefix_mult},
};

/// Guard against division by zero when computing the top-1/top-2
/// score ratio. Any runner-up whose score is below this epsilon is
/// treated as effectively zero for margin purposes.
const EPSILON: f32 = 1e-6;

/// Minimum frecency_raw for a candidate to be considered
/// "dominant" over the runner-up. Matches the D5 spec.
const FRECENCY_DOMINANCE_MIN_RAW: f32 = 3.0;

/// Multiplier the candidate's frecency_raw must exceed over the
/// runner-up's to count as dominant.
const FRECENCY_DOMINANCE_RATIO: f32 = 2.0;

/// Decide whether `hits[0]` should be promoted to Top Hit for query
/// `q_raw`. Returns `Some(hits[0].id.clone())` when both the
/// confidence and margin thresholds are satisfied, else `None`.
///
/// Edge cases:
/// - `hits.is_empty()` → `None`.
/// - `hits.len() == 1` → margin is treated as infinity; promotion
///   depends solely on confidence.
#[allow(clippy::too_many_arguments)]
pub fn select_top_hit(
    q_raw: &str,
    hits: &[Hit],
    frecency: &FrecencyStore,
    latch: &QueryLatchStore,
    now: i64,
    min_confidence: f32,
    min_margin: f32,
    strong_latch_threshold: u32,
) -> Option<DocId> {
    if hits.is_empty() {
        return None;
    }
    let candidate = &hits[0];
    let q_norm = normalize_for_match(q_raw);
    let title_norm = normalize_for_match(&candidate.title);

    // `prefix_mult`/`acronym_mult` return `weight` on match, `1.0`
    // otherwise. Any weight > 1.0 works as a "fired?" probe; the
    // actual prefix/acronym multiplier applied during search uses
    // the configured boost (independent from this check).
    let prefix_match = prefix_mult(&title_norm, &q_norm, 2.0) > 1.0;
    let acronym_match = acronym_mult(&candidate.title, &q_norm, 2.0) > 1.0;
    let has_strong_latch = latch.strong(q_raw, &candidate.id.0, strong_latch_threshold);
    let dominance = frecency_dominance(candidate, hits, frecency, now);

    let lexical_confidence: f32 = if prefix_match || acronym_match {
        1.0
    } else {
        0.0
    };
    let latch_confidence: f32 = if has_strong_latch { 1.0 } else { 0.0 };
    let confidence = lexical_confidence.max(latch_confidence).max(dominance);

    let margin = if hits.len() <= 1 {
        f32::INFINITY
    } else {
        hits[0].score / hits[1].score.max(EPSILON)
    };

    if confidence >= min_confidence && margin >= min_margin {
        Some(candidate.id.clone())
    } else {
        None
    }
}

/// `1.0` when the candidate dominates the runner-up in frecency
/// terms (sufficient absolute usage AND clear lead over #2), else
/// `0.0`. The single-hit case (`hits.len() < 2`) relies solely on
/// the absolute threshold — there is no one to out-rank.
fn frecency_dominance(
    candidate: &Hit,
    hits: &[Hit],
    frecency: &FrecencyStore,
    now: i64,
) -> f32 {
    let cand_raw = frecency.raw(&candidate.id.0, now);
    if hits.len() < 2 {
        return if cand_raw >= FRECENCY_DOMINANCE_MIN_RAW {
            1.0
        } else {
            0.0
        };
    }
    let runner_raw = frecency.raw(&hits[1].id.0, now);
    if cand_raw >= FRECENCY_DOMINANCE_MIN_RAW
        && cand_raw >= FRECENCY_DOMINANCE_RATIO * runner_raw.max(EPSILON)
    {
        1.0
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lixun_core::{Action, Category, DocId, Hit};
    use lixun_ipc::Response;

    fn make_hit(id: &str, title: &str, score: f32) -> Hit {
        Hit {
            id: DocId(id.into()),
            category: Category::App,
            title: title.into(),
            subtitle: String::new(),
            icon_name: None,
            kind_label: None,
            score,
            action: Action::Launch {
                exec: "true".into(),
                terminal: false,
                desktop_id: None,
                desktop_file: None,
                working_dir: None,
            },
            extract_fail: false,
            sender: None,
            recipients: None,
            body: None,
        }
    }

    /// D5 happy path: a clear prefix match with a wide score lead
    /// satisfies both gates and promotes hits[0] to Top Hit.
    #[test]
    fn prefix_match_sets_top_hit() {
        let hits = vec![
            make_hit("app:firefox", "Firefox", 10.0),
            make_hit("app:something", "Something", 3.0),
        ];
        let result = select_top_hit(
            "fire",
            &hits,
            &FrecencyStore::default(),
            &QueryLatchStore::default(),
            0,
            0.6,
            1.3,
            3,
        );
        assert_eq!(result, Some(hits[0].id.clone()));
    }

    /// Margin gate bites: two identically-titled docs both
    /// prefix-match, but 10.0/9.5 ≈ 1.05 fails the 1.3 threshold,
    /// so no Top Hit is emitted.
    #[test]
    fn ambiguous_returns_none() {
        let hits = vec![
            make_hit("fs:a", "Doc", 10.0),
            make_hit("fs:b", "Doc", 9.5),
        ];
        let result = select_top_hit(
            "doc",
            &hits,
            &FrecencyStore::default(),
            &QueryLatchStore::default(),
            0,
            0.6,
            1.3,
            3,
        );
        assert!(result.is_none());
    }

    /// Mirror of the `match negotiated_version { .. }` arm in
    /// `handle_client::Search`. Guards that v2 clients continue to
    /// receive the pre-v3 `HitsWithExtras` shape.
    fn dispatch_response(
        negotiated_version: u16,
        hits: Vec<Hit>,
        top_hit: Option<DocId>,
    ) -> Response {
        match negotiated_version {
            1 => Response::Hits(hits),
            2 => Response::HitsWithExtras {
                hits,
                calculation: None,
            },
            _ => Response::HitsWithExtrasV3 {
                hits,
                calculation: None,
                top_hit,
            },
        }
    }

    #[test]
    fn v2_response_shape_preserved() {
        let hits = vec![make_hit("app:firefox", "Firefox", 10.0)];
        let resp = dispatch_response(2, hits, Some(DocId("app:firefox".into())));
        match resp {
            Response::HitsWithExtras { .. } => {}
            other => panic!("expected HitsWithExtras for v2, got {:?}", other),
        }
    }

    #[test]
    fn v1_response_shape_preserved() {
        let hits = vec![make_hit("app:firefox", "Firefox", 10.0)];
        let resp = dispatch_response(1, hits, None);
        match resp {
            Response::Hits(_) => {}
            other => panic!("expected Hits for v1, got {:?}", other),
        }
    }
}
