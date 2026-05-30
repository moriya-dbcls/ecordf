//! # Path Cache
//!
//! Pre-materialises multi-hop SPARQL property paths extracted from rdf-config
//! `model.yaml` files.  Each path is stored as a sorted `Vec<(TermId, TermId)>`
//! so the executor can answer `?s <path> ?o` queries with an O(N) linear scan
//! instead of a multi-step HDD traversal.
//!
//! ## Design
//!
//! A compound path `[p1, p2, …, pN]` is materialised by:
//!
//! 1. Scanning `?s p1 ?m1` from the POS index.
//! 2. For each intermediate node `m1`, scanning `?m1 p2 ?m2`.
//! 3. … repeat through the chain …
//! 4. Collecting all reachable `(start, end)` pairs.
//!
//! The result is a sorted `Vec<(TermId, TermId)>` wrapped in `Arc` so multiple
//! query threads can share it without copying.
//!
//! ## Memory
//!
//! Controlled by `model.path_cache_mb` in `ecordf.toml` (default: `0` = disabled).
//! Each entry uses 16 bytes (two u64 IDs).  A 1 M-pair path uses ~16 MB.
//!
//! ## Integration with the executor
//!
//! The executor's `eval_path` checks `PathCache::get(path_ids)` before falling
//! back to step-by-step POS scans.  A cache hit replaces the entire multi-hop
//! traversal with a single binary-search or sequential scan over a RAM-resident
//! sorted array.
//!
//! ## Relationship to PredCache
//!
//! [`crate::predcache::PredCache`] caches **single-predicate** (subject, object)
//! pairs and is used for individual POS scans.  PathCache caches **multi-hop**
//! paths and is used for property-path evaluation.  The two caches complement
//! each other: PathCache eliminates the blank-node traversal, PredCache eliminates
//! individual HDD scans within a single hop.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use crate::dict_builder::QueryDict;
use crate::index::TripleIndex;
use crate::rdf_config::CompoundPath;
use crate::triple::{TermId, TriplePattern, UNBOUND};

// ── Types ─────────────────────────────────────────────────────────────────────

/// Sorted (start, end) pairs for a materialised property path.
pub type PathPairs = Arc<Vec<(TermId, TermId)>>;

// ── PathCache ─────────────────────────────────────────────────────────────────

/// In-RAM materialised path cache.
///
/// Keyed by `Vec<TermId>` (the predicate IDs of the path in order).
/// Lookup is O(N_paths) where N_paths is the number of cached paths (typically < 100).
#[derive(Clone, Default)]
pub struct PathCache {
    /// Map from predicate-ID sequence to materialised (start, end) pairs.
    entries: Arc<HashMap<Vec<TermId>, PathPairs>>,
    /// Total bytes used by all cached pairs.
    bytes_used: usize,
}

impl PathCache {
    /// Empty cache (used when path_cache is disabled).
    pub fn empty() -> Self {
        Self::default()
    }

    /// Total bytes currently held in the cache.
    pub fn bytes_used(&self) -> usize {
        self.bytes_used
    }

    /// Number of paths cached.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Look up a path by its predicate-ID sequence.
    ///
    /// Returns `None` if this path is not cached.
    pub fn get(&self, path: &[TermId]) -> Option<PathPairs> {
        self.entries.get(path).cloned()
    }

    /// Build a PathCache by materialising all `compound_paths`.
    ///
    /// - Resolves each IRI string in each path to a `TermId` via `dict`.
    ///   Paths containing unknown IRIs are skipped (warn-logged).
    /// - Traverses the index hop-by-hop, collecting (start, end) pairs.
    /// - Sorts and deduplicates each path's pairs.
    /// - Respects `budget_bytes`: stops adding new paths once the budget is
    ///   exhausted.  Paths are processed in declaration order.
    pub fn build(
        compound_paths: &[CompoundPath],
        dict: &QueryDict,
        index: &TripleIndex,
        budget_bytes: usize,
    ) -> Self {
        let t0 = Instant::now();
        let mut entries: HashMap<Vec<TermId>, PathPairs> = HashMap::new();
        let mut total_bytes: usize = 0;

        tracing::info!(
            paths = compound_paths.len(),
            budget_mb = budget_bytes / (1024 * 1024),
            "path-cache: starting build"
        );

        for path_iris in compound_paths {
            if path_iris.len() < 2 {
                continue; // single-hop paths handled by PredCache
            }

            // Resolve IRI strings → TermIds
            let path_ids: Vec<TermId> = match resolve_path(path_iris, dict) {
                Some(ids) => ids,
                None => {
                    tracing::debug!(
                        path = ?path_iris,
                        "path-cache: skipping path with unknown IRI(s)"
                    );
                    continue;
                }
            };

            // Already cached (could be a duplicate from multiple rdf-config sources)
            if entries.contains_key(&path_ids) {
                continue;
            }

            // Materialise the path
            let pairs = materialise_path(&path_ids, index);
            if pairs.is_empty() {
                tracing::debug!(
                    path = ?path_iris,
                    "path-cache: empty result, not caching"
                );
                continue;
            }

            let pair_bytes = pairs.len() * 16; // 16 bytes per (u64, u64)
            if total_bytes + pair_bytes > budget_bytes {
                tracing::debug!(
                    path = ?path_iris,
                    pairs = pairs.len(),
                    mb = pair_bytes / (1024 * 1024),
                    "path-cache: budget exhausted, skipping remaining paths"
                );
                // Unlike PredCache we stop here — paths are ordered by importance
                // in the model.yaml (most common patterns first).
                break;
            }

            tracing::debug!(
                path = ?path_iris,
                pairs = pairs.len(),
                mb = pair_bytes / (1024 * 1024),
                total_mb = (total_bytes + pair_bytes) / (1024 * 1024),
                "path-cache: cached path"
            );

            total_bytes += pair_bytes;
            entries.insert(path_ids, Arc::new(pairs));
        }

        tracing::info!(
            paths_cached = entries.len(),
            total_mb = total_bytes / (1024 * 1024),
            elapsed_ms = t0.elapsed().as_millis(),
            "path-cache: build complete"
        );

        Self {
            entries: Arc::new(entries),
            bytes_used: total_bytes,
        }
    }
}

// ── Path resolution ───────────────────────────────────────────────────────────

/// Resolve a sequence of IRI strings to TermIds.
///
/// Returns `None` if any IRI is not in the dictionary (i.e. never appears in
/// the store — this path won't produce any results anyway).
fn resolve_path(path_iris: &[String], dict: &QueryDict) -> Option<Vec<TermId>> {
    path_iris.iter().map(|iri| {
        // IRIs from rdf_config come as `<…>` — strip angle brackets for lookup
        let key = iri.trim_matches(|c| c == '<' || c == '>');
        dict.lookup(key)
    }).collect()
}

// ── Path materialisation ──────────────────────────────────────────────────────

/// Materialise all (start, end) pairs reachable through `path_ids`.
///
/// Algorithm: iterative hop-by-hop expansion.
///
/// 1. Scan all `(s, o)` pairs for `path_ids[0]` from the POS index.
/// 2. Use the object set as the subject set for `path_ids[1]`.
/// 3. Repeat through the chain, carrying forward the full (start, mid…, end) chain
///    as `(original_start, current_end)` pairs.
/// 4. Return the sorted (start, end) pairs.
///
/// For long paths or large intermediate sets this can be memory-intensive, but
/// in practice model.yaml paths through blank nodes have small intermediate sets
/// (blank nodes are not shared between independent subjects).
fn materialise_path(path_ids: &[TermId], index: &TripleIndex) -> Vec<(TermId, TermId)> {
    if path_ids.is_empty() {
        return Vec::new();
    }

    // Start: scan the first predicate
    let first_pred = path_ids[0];
    let pat = TriplePattern::new(UNBOUND, first_pred, UNBOUND);
    // current = Vec<(start, mid)>
    let mut current: Vec<(TermId, TermId)> = index.scan(&pat)
        .map(|t| (t.s, t.o))
        .collect();

    // Subsequent hops
    for &pred_id in &path_ids[1..] {
        if current.is_empty() {
            break;
        }

        // Build a HashSet of the current "frontier" (mid values) for O(1) lookup
        // We need to join current.o with next-hop.s
        // Strategy: collect next hop pairs for this predicate, then merge-join.

        let hop_pat = TriplePattern::new(UNBOUND, pred_id, UNBOUND);
        // next_hop: Vec<(s, o)> sorted by s (POS scan gives P-O-S order;
        //           we need to sort by s for merge join)
        let mut next_hop: Vec<(TermId, TermId)> = index.scan(&hop_pat)
            .map(|t| (t.s, t.o))
            .collect();
        next_hop.sort_unstable_by_key(|&(s, _)| s);

        // Sort current by mid (second element) for merge join
        current.sort_unstable_by_key(|&(_, mid)| mid);

        // Merge join: current.mid == next_hop.s
        current = merge_hop(&current, &next_hop);
    }

    // Sort the final (start, end) pairs and deduplicate
    current.sort_unstable();
    current.dedup();
    current
}

/// Merge-join `current` (sorted by mid/second) with `next_hop` (sorted by s/first).
///
/// Produces `Vec<(original_start, new_end)>` where `current[i].1 == next_hop[j].0`.
fn merge_hop(
    current: &[(TermId, TermId)], // sorted by .1 (mid)
    next_hop: &[(TermId, TermId)], // sorted by .0 (s = mid in next hop)
) -> Vec<(TermId, TermId)> {
    let mut out = Vec::new();
    let mut ci = 0usize;
    let mut ni = 0usize;

    while ci < current.len() && ni < next_hop.len() {
        let mid   = current[ci].1;
        let nhkey = next_hop[ni].0;

        if mid < nhkey {
            // Advance past all current entries with this mid
            while ci < current.len() && current[ci].1 == mid { ci += 1; }
        } else if mid > nhkey {
            // Advance past all next_hop entries with this key
            let skip = next_hop[ni..].partition_point(|&(s, _)| s < mid);
            ni += skip;
        } else {
            // mid == nhkey: emit cross-product
            let cur_end = ci + current[ci..].partition_point(|&(_, m)| m == mid);
            let ni_end  = ni + next_hop[ni..].partition_point(|&(s, _)| s == mid);

            for &(start, _) in &current[ci..cur_end] {
                for &(_, end) in &next_hop[ni..ni_end] {
                    out.push((start, end));
                }
            }
            ci = cur_end;
            ni = ni_end;
        }
    }
    out
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_merge_hop_simple() {
        // current: (start, mid) sorted by mid
        let current = vec![(1u64, 10u64), (2u64, 10u64), (3u64, 20u64)];
        // next_hop: (mid, end) sorted by mid (acting as s)
        let next_hop = vec![(10u64, 100u64), (10u64, 101u64), (20u64, 200u64)];

        let result = merge_hop(&current, &next_hop);
        let mut expected = vec![
            (1u64, 100u64), (1u64, 101u64),
            (2u64, 100u64), (2u64, 101u64),
            (3u64, 200u64),
        ];
        expected.sort();
        let mut result_sorted = result.clone();
        result_sorted.sort();
        assert_eq!(result_sorted, expected);
    }

    #[test]
    fn test_merge_hop_no_match() {
        let current  = vec![(1u64, 99u64)];
        let next_hop = vec![(100u64, 200u64)]; // no key 99
        let result   = merge_hop(&current, &next_hop);
        assert!(result.is_empty());
    }
}
