//! Text search: dedicated worker thread + main-thread query state.
//!
//! Q7 decisions (plan §2.10):
//! - Flags: `FindFlags::empty()` — case-insensitive, no diacritic
//!   folding, no whole-word restriction. Matches Papers' default
//!   (`PPS_FIND_DEFAULT = 0`) and user expectation for a launcher
//!   quick-look.
//! - Worker: long-lived dedicated [`SearchWorker`] thread, owns
//!   its own `poppler::Document` (Q2 per-owner pattern).
//! - Cold, progressive streaming: every `Start` bumps the local
//!   generation, scans every page in order, emits one
//!   [`PageSearchResult`] per page. A newer `Start` invalidates
//!   the in-flight scan between pages.
//! - Stale-result drop: main thread compares
//!   `result.generation` against [`SearchQueryState::generation`]
//!   and ignores mismatches.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;

use crate::document_session::DocumentSession;

/// Per-page hits keyed by page index. `BTreeMap` so iteration is
/// document order; `Vec<Rectangle>` preserves the per-page order
/// poppler returned.
pub type SearchResults = BTreeMap<u32, Vec<poppler::Rectangle>>;

/// A reference to a single match, used by Next/Prev navigation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchMatchRef {
    pub page_idx: u32,
    pub match_idx_in_page: usize,
}

/// Main-thread state for an in-flight or completed query.
#[derive(Clone, Debug)]
pub struct SearchQueryState {
    pub query: String,
    pub generation: u64,
    pub results: SearchResults,
    pub total_matches: usize,
    pub finished_pages: usize,
    pub total_pages: usize,
    pub current: Option<SearchMatchRef>,
}

impl SearchQueryState {
    pub fn empty() -> Self {
        Self {
            query: String::new(),
            generation: 0,
            results: BTreeMap::new(),
            total_matches: 0,
            finished_pages: 0,
            total_pages: 0,
            current: None,
        }
    }

    /// Merge a page result. Drops anything whose generation does
    /// not match the current state. Returns `true` if the result
    /// was accepted.
    pub fn merge_page_result(&mut self, result: PageSearchResult) -> bool {
        if result.generation != self.generation {
            return false;
        }
        self.total_matches = self
            .total_matches
            .saturating_sub(self.results.get(&result.page_idx).map(Vec::len).unwrap_or(0));
        self.total_matches += result.rects.len();
        if result.rects.is_empty() {
            self.results.remove(&result.page_idx);
        } else {
            self.results.insert(result.page_idx, result.rects);
        }
        if result.done_for_page {
            self.finished_pages = self.finished_pages.saturating_add(1);
        }
        if self.current.is_none() {
            self.current = first_match(&self.results);
        }
        true
    }

    pub fn match_count(&self) -> usize {
        self.total_matches
    }

    pub fn current_index(&self) -> Option<usize> {
        let cur = self.current?;
        let mut idx = 0usize;
        for (&page, rects) in &self.results {
            if page == cur.page_idx {
                return Some(idx + cur.match_idx_in_page);
            }
            idx = idx.saturating_add(rects.len());
        }
        None
    }

    pub fn advance(&mut self, forward: bool) {
        if self.results.is_empty() {
            self.current = None;
            return;
        }
        let flat = flatten(&self.results);
        if flat.is_empty() {
            self.current = None;
            return;
        }
        let cur = self
            .current
            .and_then(|c| flat.iter().position(|r| *r == c))
            .unwrap_or(0);
        let next = if forward {
            (cur + 1) % flat.len()
        } else {
            (cur + flat.len() - 1) % flat.len()
        };
        self.current = Some(flat[next]);
    }

    pub fn current_rect(&self) -> Option<(u32, poppler::Rectangle)> {
        let cur = self.current?;
        let rects = self.results.get(&cur.page_idx)?;
        let r = rects.get(cur.match_idx_in_page)?;
        Some((cur.page_idx, clone_rect(r)))
    }
}

fn clone_rect(r: &poppler::Rectangle) -> poppler::Rectangle {
    let mut out = poppler::Rectangle::default();
    out.set_x1(r.x1());
    out.set_y1(r.y1());
    out.set_x2(r.x2());
    out.set_y2(r.y2());
    out
}

fn first_match(results: &SearchResults) -> Option<SearchMatchRef> {
    for (&page, rects) in results {
        if !rects.is_empty() {
            return Some(SearchMatchRef {
                page_idx: page,
                match_idx_in_page: 0,
            });
        }
    }
    None
}

fn flatten(results: &SearchResults) -> Vec<SearchMatchRef> {
    let mut out = Vec::new();
    for (&page, rects) in results {
        for i in 0..rects.len() {
            out.push(SearchMatchRef {
                page_idx: page,
                match_idx_in_page: i,
            });
        }
    }
    out
}

/// Streamed from the worker. One per scanned page per query.
pub struct PageSearchResult {
    pub generation: u64,
    pub page_idx: u32,
    pub rects: Vec<poppler::Rectangle>,
    pub done_for_page: bool,
}

pub struct SearchWorker {
    session: Rc<DocumentSession>,
    result_tx: async_channel::Sender<PageSearchResult>,
}

impl SearchWorker {
    pub fn new(
        session: Rc<DocumentSession>,
        result_tx: async_channel::Sender<PageSearchResult>,
    ) -> Self {
        Self { session, result_tx }
    }

    pub fn replace_path(&self, _path: PathBuf) {}

    pub fn start(&self, query: String, generation: u64, n_pages: u32) {
        self.session
            .start_search(generation, query, n_pages, self.result_tx.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64) -> poppler::Rectangle {
        let mut r = poppler::Rectangle::default();
        r.set_x1(x);
        r.set_x2(x + 10.0);
        r.set_y1(100.0);
        r.set_y2(120.0);
        r
    }

    #[test]
    fn stale_generation_is_ignored() {
        let mut state = SearchQueryState {
            generation: 5,
            ..SearchQueryState::empty()
        };
        let accepted = state.merge_page_result(PageSearchResult {
            generation: 4,
            page_idx: 0,
            rects: vec![rect(0.0)],
            done_for_page: true,
        });
        assert!(!accepted, "older generation must not land");
        assert_eq!(state.total_matches, 0);
        assert_eq!(state.finished_pages, 0);
    }

    #[test]
    fn empty_results_do_not_leave_empty_vec_in_map() {
        let mut state = SearchQueryState {
            generation: 1,
            ..SearchQueryState::empty()
        };
        state.merge_page_result(PageSearchResult {
            generation: 1,
            page_idx: 0,
            rects: vec![],
            done_for_page: true,
        });
        assert!(state.results.get(&0).is_none());
        assert_eq!(state.total_matches, 0);
        assert_eq!(state.finished_pages, 1);
        assert!(state.current.is_none());
    }

    #[test]
    fn progressive_accumulation_counts_matches() {
        let mut state = SearchQueryState {
            generation: 1,
            ..SearchQueryState::empty()
        };
        state.merge_page_result(PageSearchResult {
            generation: 1,
            page_idx: 0,
            rects: vec![rect(0.0), rect(50.0)],
            done_for_page: true,
        });
        state.merge_page_result(PageSearchResult {
            generation: 1,
            page_idx: 2,
            rects: vec![rect(20.0)],
            done_for_page: true,
        });
        assert_eq!(state.total_matches, 3);
        assert_eq!(state.finished_pages, 2);
        assert_eq!(
            state.current,
            Some(SearchMatchRef {
                page_idx: 0,
                match_idx_in_page: 0,
            })
        );
    }

    #[test]
    fn advance_wraps_around_flattened_order() {
        let mut state = SearchQueryState {
            generation: 1,
            ..SearchQueryState::empty()
        };
        state.merge_page_result(PageSearchResult {
            generation: 1,
            page_idx: 0,
            rects: vec![rect(0.0), rect(50.0)],
            done_for_page: true,
        });
        state.merge_page_result(PageSearchResult {
            generation: 1,
            page_idx: 1,
            rects: vec![rect(0.0)],
            done_for_page: true,
        });
        assert_eq!(state.current_index(), Some(0));
        state.advance(true);
        assert_eq!(state.current_index(), Some(1));
        state.advance(true);
        assert_eq!(state.current_index(), Some(2));
        state.advance(true);
        assert_eq!(state.current_index(), Some(0), "should wrap");
        state.advance(false);
        assert_eq!(state.current_index(), Some(2), "backwards wraps too");
    }

    #[test]
    fn second_page_result_for_same_page_replaces_rects() {
        let mut state = SearchQueryState {
            generation: 1,
            ..SearchQueryState::empty()
        };
        state.merge_page_result(PageSearchResult {
            generation: 1,
            page_idx: 0,
            rects: vec![rect(0.0), rect(50.0), rect(100.0)],
            done_for_page: true,
        });
        state.merge_page_result(PageSearchResult {
            generation: 1,
            page_idx: 0,
            rects: vec![rect(0.0)],
            done_for_page: true,
        });
        assert_eq!(state.total_matches, 1);
    }
}
