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

/// All inputs and outputs from a Top Hit selection. The gate
/// decision is `id`; the other fields are the probes that went
/// into it, returned so the caller can log them regardless of the
/// outcome. Logging both pass and fail paths is essential for
/// Phase 3 retune — without the probe values we can't measure
/// where the gate is failing on live queries.
#[derive(Debug, Clone)]
pub struct TopHitDecision {
    pub id: Option<DocId>,
    pub confidence: f32,
    pub margin: f32,
    pub prefix_match: bool,
    pub acronym_match: bool,
    pub has_strong_latch: bool,
    pub dominance: f32,
}

/// Decide whether `hits[0]` should be promoted to Top Hit for query
/// `q_raw`. Returns a `TopHitDecision` whose `.id` is `Some` when
/// both gates pass and `None` otherwise; the other fields carry
/// the probes used to compute the decision.
///
/// Edge cases:
/// - `hits.is_empty()` → id None, probes zeroed, margin = -inf.
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
) -> TopHitDecision {
    if hits.is_empty() {
        return TopHitDecision {
            id: None,
            confidence: 0.0,
            margin: f32::NEG_INFINITY,
            prefix_match: false,
            acronym_match: false,
            has_strong_latch: false,
            dominance: 0.0,
        };
    }
    let candidate = &hits[0];
    let q_norm = normalize_for_match(q_raw);
    let title_norm = normalize_for_match(&candidate.title);

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

    let id = if confidence >= min_confidence && margin >= min_margin {
        Some(candidate.id.clone())
    } else {
        None
    };

    TopHitDecision {
        id,
        confidence,
        margin,
        prefix_match,
        acronym_match,
        has_strong_latch,
        dominance,
    }
}

/// `1.0` when the candidate dominates the runner-up in frecency
/// terms (sufficient absolute usage AND clear lead over #2), else
/// `0.0`. The single-hit case (`hits.len() < 2`) relies solely on
/// the absolute threshold — there is no one to out-rank.
fn frecency_dominance(candidate: &Hit, hits: &[Hit], frecency: &FrecencyStore, now: i64) -> f32 {
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
            secondary_action: None,
            source_instance: String::new(),
            row_menu: lixun_core::RowMenuDef::empty(),
            mime: None,
        }
    }

    #[test]
    fn prefix_match_sets_top_hit() {
        let hits = vec![
            make_hit("app:firefox", "Firefox", 10.0),
            make_hit("app:something", "Something", 3.0),
        ];
        let decision = select_top_hit(
            "fire",
            &hits,
            &FrecencyStore::default(),
            &QueryLatchStore::default(),
            0,
            0.6,
            1.3,
            3,
        );
        assert_eq!(decision.id, Some(hits[0].id.clone()));
        assert!(decision.prefix_match);
        assert!(decision.confidence >= 0.6);
        assert!(decision.margin >= 1.3);
    }

    /// Margin gate bites: two identically-titled docs both
    /// prefix-match, but 10.0/9.5 ≈ 1.05 fails the 1.3 threshold,
    /// so no Top Hit is emitted — but the probes are still filled.
    #[test]
    fn ambiguous_returns_none() {
        let hits = vec![make_hit("fs:a", "Doc", 10.0), make_hit("fs:b", "Doc", 9.5)];
        let decision = select_top_hit(
            "doc",
            &hits,
            &FrecencyStore::default(),
            &QueryLatchStore::default(),
            0,
            0.6,
            1.3,
            3,
        );
        assert!(decision.id.is_none());
        assert!(
            decision.prefix_match,
            "probe still fires even when gate rejects"
        );
        assert!(
            (decision.margin - (10.0_f32 / 9.5)).abs() < 0.001,
            "margin value populated regardless of gate outcome"
        );
    }

    /// All probes must populate on empty input so callers can log
    /// uniformly. The gate returns None trivially.
    #[test]
    fn empty_hits_returns_zeroed_decision() {
        let decision = select_top_hit(
            "anything",
            &[],
            &FrecencyStore::default(),
            &QueryLatchStore::default(),
            0,
            0.6,
            1.3,
            3,
        );
        assert!(decision.id.is_none());
        assert_eq!(decision.confidence, 0.0);
        assert!(!decision.prefix_match);
        assert!(!decision.acronym_match);
        assert!(!decision.has_strong_latch);
        assert_eq!(decision.dominance, 0.0);
    }

    #[test]
    fn v4_protocol_only() {
        assert_eq!(lixun_ipc::MIN_PROTOCOL_VERSION, 4);
        assert_eq!(lixun_ipc::PROTOCOL_VERSION, 4);
    }
}
