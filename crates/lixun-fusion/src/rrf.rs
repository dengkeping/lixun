//! Reciprocal Rank Fusion (Cormack, Clarke, Büttcher 2009): combine
//! multiple ranked lists into one by summing `1 / (k + rank_i)` over
//! every list a doc appears in. Doc ids missing from a list
//! contribute zero to that list's term. k = 60 is the canonical
//! default from the original paper and matches Elastic / OpenSearch
//! / Qdrant behaviour.

use std::cmp::Ordering;
use std::collections::HashMap;

pub fn rrf_fuse(
    bm25: &[(String, f32)],
    ann: &[(String, f32)],
    k: f32,
) -> Vec<(String, f32)> {
    let mut fused: HashMap<&str, f32> = HashMap::with_capacity(bm25.len() + ann.len());
    for (rank0, (doc_id, _)) in bm25.iter().enumerate() {
        let rank1 = (rank0 + 1) as f32;
        *fused.entry(doc_id.as_str()).or_insert(0.0) += 1.0 / (k + rank1);
    }
    for (rank0, (doc_id, _)) in ann.iter().enumerate() {
        let rank1 = (rank0 + 1) as f32;
        *fused.entry(doc_id.as_str()).or_insert(0.0) += 1.0 / (k + rank1);
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
}
