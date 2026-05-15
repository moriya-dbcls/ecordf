//! Histogram-based statistics for query optimization.
//!
//! The optimizer uses these statistics to estimate triple pattern cardinality.

use std::collections::HashMap;
use crate::triple::TermId;

/// Per-predicate statistics: subject count, object count, triple count.
#[derive(Default)]
pub struct PredicateStats {
    pub triple_count: u64,
    pub subject_count: u64,    // approx distinct subjects
    pub object_count: u64,     // approx distinct objects
}

pub struct StoreStatistics {
    pub total_triples: u64,
    pub predicate_stats: HashMap<TermId, PredicateStats>,
}

impl StoreStatistics {
    pub fn new() -> Self {
        Self {
            total_triples: 0,
            predicate_stats: HashMap::new(),
        }
    }

    /// Estimate the number of results for a triple pattern.
    /// Used by the optimizer for join ordering.
    pub fn estimate(&self, s: Option<TermId>, p: Option<TermId>, o: Option<TermId>) -> u64 {
        let n = self.total_triples.max(1);
        match (s, p, o) {
            (Some(_), Some(p_id), Some(_)) => {
                // SPO: at most 1 result
                1
            }
            (Some(_), Some(p_id), None) => {
                // SP: estimate by predicate object count
                let ps = self.predicate_stats.get(&p_id);
                ps.map(|s| (s.triple_count / s.subject_count.max(1)).max(1))
                  .unwrap_or(n / 100)
            }
            (None, Some(p_id), Some(_)) => {
                // PO
                let ps = self.predicate_stats.get(&p_id);
                ps.map(|s| (s.triple_count / s.object_count.max(1)).max(1))
                  .unwrap_or(n / 100)
            }
            (Some(_), None, None) => n / 1000,   // S pattern
            (None, Some(p_id), None) => {
                self.predicate_stats.get(&p_id)
                    .map(|s| s.triple_count)
                    .unwrap_or(n / 10)
            }
            (None, None, Some(_)) => n / 100,    // O pattern (rare)
            _ => n,                               // Full scan
        }
    }
}
