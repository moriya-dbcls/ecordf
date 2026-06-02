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

    /// Build a PathCache by materialising all `compound_paths` and their
    /// contiguous sub-sequences.
    ///
    /// ## Sub-sequence expansion
    ///
    /// `rdf_config` extracts paths starting from each class's root predicate
    /// (e.g. `[up:annotation, up:range, faldo:begin, faldo:position]`), but
    /// SPARQL queries often traverse only a *suffix* of that chain
    /// (e.g. `faldo:begin / faldo:position`).  This method expands every input
    /// path into **all contiguous sub-sequences of length ≥ 2** so that
    /// any suffix or middle segment is also cached.
    ///
    /// Example expansion of `[A, B, C]`:
    /// - `[A, B]`, `[B, C]` (length 2)
    /// - `[A, B, C]`        (length 3)
    ///
    /// Sub-sequences are materialised **shortest-first**: short paths tend to
    /// be queried more often and are cheaper to store, so they get priority
    /// when the budget is tight.
    ///
    /// ## Other behaviour
    ///
    /// - Paths containing unknown IRIs (not in the dictionary) are skipped.
    /// - Empty materialised results are not stored.
    /// - Budget (`budget_bytes`) is respected; materialisation stops once the
    ///   total exceeds the limit.
    pub fn build(
        compound_paths: &[CompoundPath],
        dict: &QueryDict,
        index: &TripleIndex,
        budget_bytes: usize,
    ) -> Self {
        let t0 = Instant::now();

        // Expand input paths into all contiguous sub-sequences of length ≥ 2.
        // Sort shortest-first so short (frequently-queried) sub-paths get
        // priority when budget is limited.
        let expanded = expand_subseqs(compound_paths);

        let mut entries: HashMap<Vec<TermId>, PathPairs> = HashMap::new();
        let mut total_bytes: usize = 0;

        tracing::info!(
            input_paths = compound_paths.len(),
            expanded_paths = expanded.len(),
            budget_mb = budget_bytes / (1024 * 1024),
            "path-cache: starting build (with sub-sequence expansion)"
        );

        let remaining_budget = |used: usize| -> usize {
            budget_bytes.saturating_sub(used)
        };

        for path_iris in &expanded {
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

            // Already cached (duplicate sub-sequence from different parent paths)
            if entries.contains_key(&path_ids) {
                continue;
            }

            // ── Pre-estimate: skip materialisation if the first predicate's POS
            // range already exceeds the remaining budget.
            //
            // materialise_path does a full POS scan + sort + merge-join and can
            // be very expensive.  If the first hop alone produces more pairs than
            // the budget allows, materialising the path is pointless — we'd spend
            // seconds scanning just to discard the result.
            //
            // The POS range is an upper bound on output pairs (each hop can only
            // shrink the set).  If even the upper bound doesn't fit, skip entirely.
            let budget_left = remaining_budget(total_bytes);
            if budget_left == 0 {
                // Paths are sorted shortest-first.  All remaining paths are at
                // least as long (and thus at least as large) as the current one,
                // so no further path will fit.  Stop immediately.
                tracing::debug!(
                    remaining = expanded.len(),
                    "path-cache: budget exhausted, stopping"
                );
                break;
            }

            let first_pred_est_pairs = index
                .pos_predicate_range(path_ids[0])
                .map(|(lo, hi)| hi - lo)
                .unwrap_or(0);
            let first_pred_est_bytes = first_pred_est_pairs * 16;

            if first_pred_est_bytes > budget_left {
                tracing::debug!(
                    path = ?path_iris,
                    est_mb = first_pred_est_bytes / (1024 * 1024),
                    budget_left_mb = budget_left / (1024 * 1024),
                    "path-cache: pre-estimate exceeds budget, skipping materialisation"
                );
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
                    "path-cache: materialised but over budget, skipping"
                );
                continue;
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

// ── Sub-sequence expansion ────────────────────────────────────────────────────

/// Expand compound paths into every contiguous sub-sequence of length ≥ 2.
///
/// Given paths from rdf-config (which are already prefix-closed — every prefix
/// of a full path is emitted), this additionally generates **internal** and
/// **suffix** sub-sequences that don't start from the root predicate.
///
/// Example: `["A", "B", "C"]` →
///   - length 2: `["A","B"]`, `["B","C"]`
///   - length 3: `["A","B","C"]`
///
/// The result is sorted **shortest-first** so that short, frequently-queried
/// paths are prioritised when the budget is limited.  Duplicates (the same
/// sub-sequence appearing in multiple parent paths) are removed.
fn expand_subseqs(paths: &[CompoundPath]) -> Vec<CompoundPath> {
    let mut result: Vec<CompoundPath> = Vec::new();
    for path in paths {
        let n = path.len();
        if n < 2 { continue; }
        for start in 0..n {
            for end in (start + 2)..=n {
                result.push(path[start..end].to_vec());
            }
        }
    }
    // Sort shortest-first, then lexicographically for deterministic ordering.
    result.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
    result.dedup();
    result
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
    fn test_expand_subseqs_basic() {
        let paths = vec![
            vec!["A".to_string(), "B".to_string(), "C".to_string()],
        ];
        let expanded = expand_subseqs(&paths);
        // Expected (length-2 first, then length-3):
        // ["A","B"], ["B","C"], ["A","B","C"]
        assert_eq!(expanded, vec![
            vec!["A".to_string(), "B".to_string()],
            vec!["B".to_string(), "C".to_string()],
            vec!["A".to_string(), "B".to_string(), "C".to_string()],
        ]);
    }

    #[test]
    fn test_expand_subseqs_dedup() {
        // Two paths share a suffix — ["B","C"] should appear only once.
        let paths = vec![
            vec!["A".to_string(), "B".to_string(), "C".to_string()],
            vec!["X".to_string(), "B".to_string(), "C".to_string()],
        ];
        let expanded = expand_subseqs(&paths);
        let bc_count = expanded.iter()
            .filter(|p| p.as_slice() == ["B".to_string(), "C".to_string()])
            .count();
        assert_eq!(bc_count, 1, "['B','C'] should appear exactly once after dedup");
    }

    #[test]
    fn test_expand_subseqs_four_hop() {
        // 4-hop path → 3 length-2, 2 length-3, 1 length-4 = 6 sub-sequences
        let paths = vec![
            vec!["A".to_string(), "B".to_string(), "C".to_string(), "D".to_string()],
        ];
        let expanded = expand_subseqs(&paths);
        assert_eq!(expanded.len(), 6);
        // Shortest come first
        assert!(expanded[0].len() == 2);
        assert!(expanded[expanded.len()-1].len() == 4);
    }

    #[test]
    fn test_merge_hop_no_match() {
        let current  = vec![(1u64, 99u64)];
        let next_hop = vec![(100u64, 200u64)]; // no key 99
        let result   = merge_hop(&current, &next_hop);
        assert!(result.is_empty());
    }
}
