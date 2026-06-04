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
use std::fs::File;
use std::io::{self, Read, Write, BufReader, BufWriter};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use crate::dict_builder::QueryDict;
use crate::index::TripleIndex;
use crate::predcache::PredCache;
use crate::rdf_config::{Cardinality, CompoundPath};
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
        pred_cache: &PredCache,
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

            // Materialise the path (uses pred_cache when available to avoid POS scans)
            let pairs = materialise_path(&path_ids, index, pred_cache);
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

    /// Save this cache to a binary file (format: ECPATH02).
    /// Logs a warning on error (non-fatal).
    pub fn save_to_file(&self, path: &Path) {
        let t0 = Instant::now();
        let mb = self.bytes_used / (1024 * 1024);
        let entries = &*self.entries;
        let ok = write_atomic(path, |w| {
            w.write_all(b"ECPATH02")?;
            w.write_all(&(entries.len() as u64).to_le_bytes())?;
            for (path_ids, pairs) in entries {
                w.write_all(&(path_ids.len() as u64).to_le_bytes())?;
                for &pred_id in path_ids {
                    w.write_all(&pred_id.to_le_bytes())?;
                }
                write_pairs(w, &**pairs)?;
            }
            Ok(())
        });
        if ok {
            tracing::info!(
                path = %path.display(),
                mb,
                elapsed_ms = t0.elapsed().as_millis(),
                "cache: saved to file"
            );
        }
    }

    /// Load a previously saved cache.
    /// Returns `None` if the file is missing, stale, or corrupt.
    pub fn load_from_file(path: &Path, reference: &Path) -> Option<Self> {
        if !is_fresh(path, reference) {
            return None;
        }
        let result = (|| -> io::Result<Self> {
            let file = File::open(path)?;
            let mut reader = BufReader::new(file);
            let mut magic = [0u8; 8];
            reader.read_exact(&mut magic)?;
            if &magic != b"ECPATH02" {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "path-cache magic mismatch"));
            }
            let mut buf8 = [0u8; 8];
            reader.read_exact(&mut buf8)?;
            let n = u64::from_le_bytes(buf8) as usize;
            let mut entries: HashMap<Vec<TermId>, PathPairs> = HashMap::with_capacity(n);
            let mut bytes_used = 0usize;
            for _ in 0..n {
                reader.read_exact(&mut buf8)?;
                let l = u64::from_le_bytes(buf8) as usize;
                let mut path_ids = Vec::with_capacity(l);
                for _ in 0..l {
                    reader.read_exact(&mut buf8)?;
                    path_ids.push(u64::from_le_bytes(buf8));
                }
                reader.read_exact(&mut buf8)?;
                let m = u64::from_le_bytes(buf8) as usize;
                let pairs = read_pairs(&mut reader, m)?;
                bytes_used += pairs.len() * 16;
                entries.insert(path_ids, Arc::new(pairs));
            }
            Ok(Self { entries: Arc::new(entries), bytes_used })
        })();
        match result {
            Ok(cache) => {
                tracing::info!(
                    path = %path.display(),
                    n_entries = cache.entries.len(),
                    mb = cache.bytes_used / (1024 * 1024),
                    "cache: loaded from file"
                );
                Some(cache)
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), err = %e, "path-cache: load failed, will rebuild");
                None
            }
        }
    }
}

// ── Persistence helpers ───────────────────────────────────────────────────────

fn is_fresh(cache_path: &Path, reference: &Path) -> bool {
    let cache_mtime = std::fs::metadata(cache_path).and_then(|m| m.modified()).ok();
    let ref_mtime   = std::fs::metadata(reference).and_then(|m| m.modified()).ok();
    match (cache_mtime, ref_mtime) {
        (Some(c), Some(r)) => c >= r,
        (Some(_), None)    => true,
        _                  => false,
    }
}

fn write_pairs(writer: &mut impl Write, pairs: &[(u64, u64)]) -> io::Result<()> {
    writer.write_all(&(pairs.len() as u64).to_le_bytes())?;
    const CHUNK: usize = 256 * 1024;
    for chunk in pairs.chunks(CHUNK) {
        let mut buf = Vec::with_capacity(chunk.len() * 16);
        for &(s, o) in chunk {
            buf.extend_from_slice(&s.to_le_bytes());
            buf.extend_from_slice(&o.to_le_bytes());
        }
        writer.write_all(&buf)?;
    }
    Ok(())
}

fn read_pairs(reader: &mut impl Read, n: usize) -> io::Result<Vec<(u64, u64)>> {
    let mut pairs = Vec::with_capacity(n);
    const CHUNK: usize = 256 * 1024;
    let mut buf = vec![0u8; CHUNK * 16];
    let mut remaining = n;
    while remaining > 0 {
        let batch = remaining.min(CHUNK);
        reader.read_exact(&mut buf[..batch * 16])?;
        for c in buf[..batch * 16].chunks_exact(16) {
            let s = u64::from_le_bytes(c[0..8].try_into().unwrap());
            let o = u64::from_le_bytes(c[8..16].try_into().unwrap());
            pairs.push((s, o));
        }
        remaining -= batch;
    }
    Ok(pairs)
}

fn write_atomic(path: &Path, write_fn: impl FnOnce(&mut BufWriter<File>) -> io::Result<()>) -> bool {
    let tmp = path.with_extension("tmp");
    let result = (|| -> io::Result<()> {
        let file = File::create(&tmp)?;
        let mut writer = BufWriter::new(file);
        write_fn(&mut writer)?;
        writer.flush()?;
        writer.into_inner()?.sync_all()?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if let Err(e) = result {
        tracing::warn!(path = %path.display(), err = %e, "cache save failed (non-fatal)");
        let _ = std::fs::remove_file(&tmp);
        false
    } else {
        true
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
fn resolve_path(path_iris: &[(String, Cardinality)], dict: &QueryDict) -> Option<Vec<TermId>> {
    path_iris.iter().map(|(iri, _)| {
        // IRIs from rdf_config come as `<…>` — strip angle brackets for lookup
        let key = iri.trim_matches(|c| c == '<' || c == '>');
        dict.lookup(key)
    }).collect()
}

// ── Path materialisation ──────────────────────────────────────────────────────

/// Retrieve all `(s, o)` pairs for `pred_id`, preferring the pred_cache.
///
/// pred_cache stores pairs sorted by `(s, o)` which is exactly the order
/// needed for merge-join.  When a cache hit occurs, we avoid:
///   - A full POS scan (sequential I/O, potentially millions of entries)
///   - An O(N log N) sort by subject
///
/// Falls back to a POS index scan + sort when the predicate is not cached.
fn get_pred_pairs(pred_id: TermId, index: &TripleIndex, pred_cache: &PredCache)
    -> Vec<(TermId, TermId)>
{
    if let Some(cached) = pred_cache.get(pred_id) {
        // pred_cache stores (S, O) sorted by (S, O) — already the right order.
        cached.to_vec()
    } else {
        let pat = TriplePattern::new(UNBOUND, pred_id, UNBOUND);
        let mut pairs: Vec<(TermId, TermId)> = index.scan(&pat)
            .map(|t| (t.s, t.o))
            .collect();
        pairs.sort_unstable_by_key(|&(s, _)| s);
        pairs
    }
}

/// Materialise all (start, end) pairs reachable through `path_ids`.
///
/// Uses `pred_cache` to avoid POS scans and sorts when predicates are cached.
/// Each hop fetches `(s, o)` pairs sorted by `s`, then performs a merge-join.
fn materialise_path(
    path_ids: &[TermId],
    index: &TripleIndex,
    pred_cache: &PredCache,
) -> Vec<(TermId, TermId)> {
    if path_ids.is_empty() {
        return Vec::new();
    }

    // First hop: get (s, o) pairs sorted by s.
    let mut current: Vec<(TermId, TermId)> = get_pred_pairs(path_ids[0], index, pred_cache);

    // Subsequent hops
    for &pred_id in &path_ids[1..] {
        if current.is_empty() {
            break;
        }

        // next_hop: (s, o) sorted by s — ready for merge-join on current.o == next_hop.s
        let next_hop = get_pred_pairs(pred_id, index, pred_cache);

        // current must be sorted by .1 (the "mid" value = object of previous hop)
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

    fn e(s: &str) -> (String, Cardinality) {
        (s.to_string(), Cardinality::ExactlyOne)
    }

    #[test]
    fn test_expand_subseqs_basic() {
        let paths = vec![
            vec![e("A"), e("B"), e("C")],
        ];
        let expanded = expand_subseqs(&paths);
        // Expected (length-2 first, then length-3):
        // ["A","B"], ["B","C"], ["A","B","C"]
        assert_eq!(expanded, vec![
            vec![e("A"), e("B")],
            vec![e("B"), e("C")],
            vec![e("A"), e("B"), e("C")],
        ]);
    }

    #[test]
    fn test_expand_subseqs_dedup() {
        // Two paths share a suffix — ["B","C"] should appear only once.
        let paths = vec![
            vec![e("A"), e("B"), e("C")],
            vec![e("X"), e("B"), e("C")],
        ];
        let expanded = expand_subseqs(&paths);
        let bc_count = expanded.iter()
            .filter(|p| p.as_slice() == [e("B"), e("C")])
            .count();
        assert_eq!(bc_count, 1, "['B','C'] should appear exactly once after dedup");
    }

    #[test]
    fn test_expand_subseqs_four_hop() {
        // 4-hop path → 3 length-2, 2 length-3, 1 length-4 = 6 sub-sequences
        let paths = vec![
            vec![e("A"), e("B"), e("C"), e("D")],
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
