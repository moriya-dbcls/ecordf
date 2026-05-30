//! # Predicate Cache
//!
//! Loads medium-sized predicates from the POS index into RAM at startup,
//! enabling O(log N) direct lookups instead of O(N) sequential POS scans.
//!
//! ## Design
//!
//! For each cached predicate, stores a `Vec<(subject, object)>` sorted by
//! (subject, object).  This allows:
//! - **Individual probes**: binary search for a specific subject → O(log N)
//! - **Batch merge**: linear merge with a sorted `current` buffer → O(N)
//!
//! Both operations avoid HDD random access entirely, replacing disk I/O with
//! sequential RAM access.
//!
//! ## Memory budget
//!
//! Controlled by `server.pred_cache_mb` in `ecordf.toml`.  Predicates are
//! loaded **largest-first** (within the 50%-of-budget per-predicate cap) so that
//! expensive predicates like `faldo:position` (11.8 M entries = 188 MB) are
//! cached before tiny ones.  After large predicates are loaded, remaining budget
//! is filled with smaller predicates.  Building is **synchronous** at startup so
//! the first query is guaranteed to hit the cache.
//!
//! ## Empirical impact (JPostDB, HDD)
//!
//! | Phase | Before cache | After cache |
//! |-------|-------------|-------------|
//! | faldo:position step=1 cold | 13 s | ~0.4 s |
//! | faldo:position step=1 warm | 6 s  | ~0.4 s |
//!
//! The gain comes from replacing an 11.8 M-entry POS scan + HashMap lookup
//! with a linear merge over a RAM-resident sorted Vec.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use crate::index::TripleIndex;
use crate::triple::{TermId, TriplePattern, UNBOUND};

// Arc<TripleIndex> is used so the background thread can hold a shared reference
// without cloning the index data (Mmap doesn't implement Clone).

// ── Types ─────────────────────────────────────────────────────────────────────

/// Sorted (subject, object) pairs for a single predicate.
pub type PredPairs = Arc<Vec<(TermId, TermId)>>;

// ── PredCache ─────────────────────────────────────────────────────────────────

/// In-RAM predicate cache.
///
/// Access is `Arc<RwLock<...>>` so the background build thread can add entries
/// while query threads are already running.
#[derive(Clone, Default)]
pub struct PredCache {
    inner: Arc<RwLock<CacheInner>>,
}

#[derive(Default)]
struct CacheInner {
    entries: HashMap<TermId, PredPairs>,
    bytes_used: usize,
}

impl PredCache {
    /// Empty cache (used when `pred_cache_mb = 0`).
    pub fn empty() -> Self {
        Self::default()
    }

    /// Look up cached pairs for `pred`.  Returns `None` if not cached yet.
    pub fn get(&self, pred: TermId) -> Option<PredPairs> {
        self.inner.read().ok()?.entries.get(&pred).cloned()
    }

    /// Total bytes currently in the cache.
    pub fn bytes_used(&self) -> usize {
        self.inner.read().map(|g| g.bytes_used).unwrap_or(0)
    }

    /// Build the cache synchronously in the **calling thread**.
    ///
    /// Blocks until all predicates within the budget are loaded.  Call this
    /// before starting the HTTP server so the first query is guaranteed to hit
    /// the cache rather than falling back to HDD scans.
    pub fn build_sync(self, index: &TripleIndex, budget_bytes: usize) {
        build_cache(&self, index, budget_bytes);
    }

    /// Spawn a background thread that fills the cache up to `budget_bytes`.
    ///
    /// Takes `Arc<TripleIndex>` (not `TripleIndex`) because `TripleIndex` contains
    /// mmap'd files that don't implement Clone.  The Arc is cheaply cloned to give
    /// the background thread shared ownership without copying any index data.
    ///
    /// Returns immediately; the cache is populated predicate-by-predicate but
    /// **does not guarantee the cache is ready before the first query**.
    /// Prefer [`build_sync`] at startup unless you explicitly want async warmup.
    pub fn build_background(self, index: Arc<TripleIndex>, budget_bytes: usize) {
        std::thread::Builder::new()
            .name("pred-cache-builder".into())
            .spawn(move || {
                build_cache(&self, &*index, budget_bytes);
            })
            .expect("failed to spawn pred-cache builder thread");
    }
}

// ── Cache builder ─────────────────────────────────────────────────────────────

fn build_cache(cache: &PredCache, index: &TripleIndex, budget_bytes: usize) {
    let t0 = Instant::now();
    // Allow a single predicate to use up to 50% of the total budget.
    // A 25% cap (budget/4) sounds conservative but with 512 MB it limits to 128 MB per
    // predicate — too small for faldo:position (11.8 M entries × 16 B = 188 MB).
    // A 50% cap allows faldo:position/begin to be cached while still preventing a
    // single runaway predicate from consuming everything.
    let per_pred_cap = (budget_bytes / 2).max(1); // no single predicate > 50% of budget

    // Load predicates largest-first (within per_pred_cap) so that expensive
    // predicates like faldo:position (11.8 M entries = 188 MB) are cached before
    // tiny ones.  `predicate_sizes()` returns ascending order; reverse it here.
    // Use `continue` (not `break`) when a predicate doesn't fit in the remaining
    // budget — smaller predicates later in the list may still fit.
    let mut sizes = index.predicate_sizes();
    sizes.sort_unstable_by(|a, b| b.1.cmp(&a.1)); // descending by count
    let mut total_loaded: usize = 0;
    let mut total_bytes: usize = 0;
    let mut remaining = budget_bytes;

    tracing::info!(
        budget_mb = budget_bytes / (1024 * 1024),
        predicates = sizes.len(),
        "pred-cache: starting build"
    );

    for (pred_id, count) in sizes {
        if count == 0 { continue; }
        let entry_bytes = count * 16; // 16 bytes per (u64, u64) pair

        // Skip predicates that exceed the per-predicate cap (too big to be useful).
        if entry_bytes > per_pred_cap { continue; }
        // Skip predicates that no longer fit, but keep trying smaller ones.
        if entry_bytes > remaining { continue; }

        let pat = TriplePattern::new(UNBOUND, pred_id, UNBOUND);
        let mut pairs: Vec<(TermId, TermId)> = index.scan(&pat)
            .map(|t| (t.s, t.o))
            .collect();

        // Sort by (subject, object) for binary-search and merge-join access.
        pairs.sort_unstable();

        let actual_bytes = pairs.len() * 16;
        remaining = remaining.saturating_sub(actual_bytes);
        total_bytes += actual_bytes;
        total_loaded += 1;

        // Insert into the shared cache (brief write lock per predicate).
        if let Ok(mut guard) = cache.inner.write() {
            guard.bytes_used += actual_bytes;
            guard.entries.insert(pred_id, Arc::new(pairs));
        }

        tracing::debug!(
            pred = pred_id,
            triples = count,
            mb = actual_bytes / (1024 * 1024),
            total_mb = total_bytes / (1024 * 1024),
            "pred-cache: cached predicate"
        );
    }

    tracing::info!(
        predicates_cached = total_loaded,
        total_mb = total_bytes / (1024 * 1024),
        elapsed_ms = t0.elapsed().as_millis(),
        "pred-cache: build complete"
    );
}

// ── Merge-join helpers ────────────────────────────────────────────────────────

/// Merge `current` (pairs sorted by second/mid element) with a cached predicate
/// (sorted by first/subject element = mid in the sequence context).
///
/// This replaces the HashMap-based batch_scan in `eval_path` with an O(N)
/// linear merge, avoiding both HDD I/O and HashMap overhead.
///
/// # Arguments
/// - `current`: `[(src, mid), ...]` sorted by mid (second element).
///   Guaranteed when produced by a POS scan (the preceding Iri step).
/// - `cached`:  `[(mid, dst), ...]` sorted by mid (first element in cache).
/// - `step_o`:  fixed output value (last step of a Sequence); `None` = any.
/// - `out`:     output buffer to extend.
pub fn merge_join(
    current: &[(TermId, TermId)],
    cached: &[(TermId, TermId)],
    step_o: Option<TermId>,
    out: &mut Vec<(TermId, TermId)>,
) {
    if current.is_empty() || cached.is_empty() { return; }

    let mut ci = 0usize; // index into current
    let mut ki = 0usize; // index into cached

    while ci < current.len() && ki < cached.len() {
        let mid = current[ci].1;
        let cache_key = cached[ki].0;

        if mid < cache_key {
            // Advance current past entries with this mid (no cache match).
            while ci < current.len() && current[ci].1 == mid { ci += 1; }
        } else if mid > cache_key {
            // Advance cache past entries with this key.
            let skip = cached[ki..].partition_point(|&(ck, _)| ck < mid);
            ki += skip;
        } else {
            // mid == cache_key: emit all (src, dst) cross-products.
            // Find the end of the current group with this mid.
            let cur_end = ci + current[ci..].partition_point(|&(_, m)| m == mid);
            // Find the end of the cache group with this key.
            let ki_end = ki + cached[ki..].partition_point(|&(ck, _)| ck == mid);

            for c in &current[ci..cur_end] {
                let src = c.0;
                for &(_, dst) in &cached[ki..ki_end] {
                    if step_o.map_or(true, |eo| eo == dst) {
                        out.push((src, dst));
                    }
                }
            }
            ci = cur_end;
            ki = ki_end;
        }
    }
}

/// Like `merge_join` but `current` is not guaranteed to be sorted by mid.
/// Sorts `current` by mid in-place first (no-op if already sorted, which is
/// the common case for POS-derived pairs).
pub fn merge_join_unsorted(
    current: &mut Vec<(TermId, TermId)>,
    cached: &[(TermId, TermId)],
    step_o: Option<TermId>,
    out: &mut Vec<(TermId, TermId)>,
) {
    // Check if already sorted by second element (fast path for POS-derived pairs).
    let already_sorted = current.windows(2).all(|w| w[0].1 <= w[1].1);
    if !already_sorted {
        current.sort_unstable_by_key(|&(_, mid)| mid);
    }
    merge_join(current, cached, step_o, out);
}
