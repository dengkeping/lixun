//! Reciprocal Rank Fusion (Cormack, Clarke, Büttcher 2009): combine
//! multiple ranked lists into one by summing `1 / (k + rank_i)` over
//! every list a doc appears in. Doc ids missing from a list
//! contribute zero to that list's term. k = 60 is the canonical
//! default from the original paper and matches Elastic / OpenSearch
//! / Qdrant behaviour.

use std::cmp::Ordering;
use std::collections::HashMap;

pub fn rrf_fuse(bm25: &[(String, f32)], ann: &[(String, f32)], k: f32) -> Vec<(String, f32)> {
    let mut fused: HashMap<&str, f32> = HashMap::with_capacity(bm25.len() + ann.len());
    for (pos, (doc_id, _)) in bm25.iter().enumerate() {
        let rank = (pos + 1) as f32; // RRF ranks are 1-based
        *fused.entry(doc_id.as_str()).or_insert(0.0) += 1.0 / (k + rank);
    }
    for (pos, (doc_id, _)) in ann.iter().enumerate() {
        let rank = (pos + 1) as f32;
        *fused.entry(doc_id.as_str()).or_insert(0.0) += 1.0 / (k + rank);
    }
    let mut out: Vec<(String, f32)> = fused
        .into_iter()
        .map(|(id, score)| (id.to_string(), score))
        .collect();
    out.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    out
}

pub fn rrf_fuse_3way(
    bm25: &[(String, f32)],
    text_ann: &[(String, f32)],
    image_ann: &[(String, f32)],
    k: f32,
) -> Vec<(String, f32)> {
    rrf_fuse_3way_weighted(bm25, text_ann, image_ann, k, (1.0, 1.0, 1.0))
}

/// Weighted RRF: each list's reciprocal-rank contribution is scaled by
/// its weight `(w_bm25, w_text, w_image)`. Weights of 1.0 reduce to
/// canonical RRF. Used to lean the fusion toward the image channel when
/// the query router classifies the query as image-intent ("photos of
/// dogs") without ever zeroing out the other evidence streams.
pub fn rrf_fuse_3way_weighted(
    bm25: &[(String, f32)],
    text_ann: &[(String, f32)],
    image_ann: &[(String, f32)],
    k: f32,
    weights: (f32, f32, f32),
) -> Vec<(String, f32)> {
    let (w_bm25, w_text, w_image) = weights;
    let mut fused: HashMap<&str, f32> =
        HashMap::with_capacity(bm25.len() + text_ann.len() + image_ann.len());
    for (pos, (doc_id, _)) in bm25.iter().enumerate() {
        let rank = (pos + 1) as f32; // RRF ranks are 1-based
        *fused.entry(doc_id.as_str()).or_insert(0.0) += w_bm25 / (k + rank);
    }
    for (pos, (doc_id, _)) in text_ann.iter().enumerate() {
        let rank = (pos + 1) as f32;
        *fused.entry(doc_id.as_str()).or_insert(0.0) += w_text / (k + rank);
    }
    for (pos, (doc_id, _)) in image_ann.iter().enumerate() {
        let rank = (pos + 1) as f32;
        *fused.entry(doc_id.as_str()).or_insert(0.0) += w_image / (k + rank);
    }
    let mut out: Vec<(String, f32)> = fused
        .into_iter()
        .map(|(id, score)| (id.to_string(), score))
        .collect();
    out.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuses_disjoint_lists_by_reciprocal_rank() {
        let bm25 = vec![("a".into(), 0.0), ("b".into(), 0.0)];
        let ann = vec![("c".into(), 0.0), ("a".into(), 0.0)];
        let out = rrf_fuse(&bm25, &ann, 60.0);
        let scores: HashMap<String, f32> = out.iter().cloned().collect();
        let expect_a = 1.0 / 61.0 + 1.0 / 62.0;
        let expect_b = 1.0 / 62.0;
        let expect_c = 1.0 / 61.0;
        assert!((scores["a"] - expect_a).abs() < 1e-6);
        assert!((scores["b"] - expect_b).abs() < 1e-6);
        assert!((scores["c"] - expect_c).abs() < 1e-6);
        assert_eq!(out[0].0, "a");
    }

    #[test]
    fn deterministic_tiebreak_by_doc_id_ascending() {
        let bm25 = vec![("zebra".into(), 0.0)];
        let ann = vec![("apple".into(), 0.0)];
        let out = rrf_fuse(&bm25, &ann, 60.0);
        assert_eq!(out[0].0, "apple");
        assert_eq!(out[1].0, "zebra");
    }

    #[test]
    fn empty_inputs_yield_empty_output() {
        let out = rrf_fuse(&[], &[], 60.0);
        assert!(out.is_empty());
    }

    #[test]
    fn fuse_3way_all_agree_dominates() {
        // When all three streams rank the same doc first, it accumulates
        // three rank-1 contributions and must dominate every other doc.
        let bm25 = vec![("hero".into(), 0.0), ("b".into(), 0.0)];
        let text = vec![("hero".into(), 0.0), ("c".into(), 0.0)];
        let image = vec![("hero".into(), 0.0), ("d".into(), 0.0)];
        let out = rrf_fuse_3way(&bm25, &text, &image, 60.0);
        assert_eq!(out[0].0, "hero");
        let scores: HashMap<String, f32> = out.iter().cloned().collect();
        let expect_hero = 3.0 * (1.0 / 61.0);
        assert!((scores["hero"] - expect_hero).abs() < 1e-6);
        // Hero strictly outranks any single-stream rank-2 doc.
        assert!(scores["hero"] > scores["b"]);
        assert!(scores["hero"] > scores["c"]);
        assert!(scores["hero"] > scores["d"]);
    }

    #[test]
    fn fuse_3way_partial_presence_surfaces() {
        // A doc present in only one stream (and missing from the other
        // two) still surfaces with that stream's reciprocal-rank score.
        let bm25 = vec![("a".into(), 0.0), ("b".into(), 0.0)];
        let text = vec![("a".into(), 0.0)];
        let image = vec![("z".into(), 0.0)];
        let out = rrf_fuse_3way(&bm25, &text, &image, 60.0);
        let scores: HashMap<String, f32> = out.iter().cloned().collect();
        // a: rank1 in bm25 + rank1 in text
        let expect_a = 1.0 / 61.0 + 1.0 / 61.0;
        // b: rank2 in bm25 only
        let expect_b = 1.0 / 62.0;
        // z: rank1 in image only — still present in the fused output
        let expect_z = 1.0 / 61.0;
        assert!((scores["a"] - expect_a).abs() < 1e-6);
        assert!((scores["b"] - expect_b).abs() < 1e-6);
        assert!(scores.contains_key("z"));
        assert!((scores["z"] - expect_z).abs() < 1e-6);
        assert_eq!(out[0].0, "a");
    }

    #[test]
    fn fuse_3way_empty_image_equals_2way() {
        // With an empty image stream, 3-way fusion is identical to the
        // 2-way lexical+text fusion (the empty stream contributes zero).
        let bm25 = vec![("a".into(), 0.0), ("b".into(), 0.0)];
        let text = vec![("c".into(), 0.0), ("a".into(), 0.0)];
        let three = rrf_fuse_3way(&bm25, &text, &[], 60.0);
        let two = rrf_fuse(&bm25, &text, 60.0);
        assert_eq!(three, two);
    }
}
