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

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::Instant;
use std::fs::File;
use std::io::{self, Read, Write, BufReader, BufWriter};
use std::path::Path;

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
    ///
    /// `per_pred_cap_bytes = 0` → use the default 50%-of-budget cap.
    /// `priority_ids` — `TermId`s loaded first regardless of size ordering.
    ///   Resolve IRI strings via `dict.lookup(iri)` before calling this.
    pub fn build_sync(
        self,
        index: &TripleIndex,
        budget_bytes: usize,
        per_pred_cap_bytes: usize,
        priority_ids: &[TermId],
    ) {
        build_cache(&self, index, budget_bytes, per_pred_cap_bytes, priority_ids);
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
    ///
    /// `per_pred_cap_bytes = 0` → use the default 50%-of-budget cap.
    /// `priority_ids` — `TermId`s loaded first regardless of size ordering.
    ///   Resolve IRI strings via `dict.lookup(iri)` before calling this.
    pub fn build_background(
        self,
        index: Arc<TripleIndex>,
        budget_bytes: usize,
        per_pred_cap_bytes: usize,
        priority_ids: Vec<TermId>,
    ) {
        std::thread::Builder::new()
            .name("pred-cache-builder".into())
            .spawn(move || {
                build_cache(&self, &*index, budget_bytes, per_pred_cap_bytes, &priority_ids);
            })
            .expect("failed to spawn pred-cache builder thread");
    }

    /// Save this cache to a binary file (format: ECPRED02).
    /// Logs a warning on error (non-fatal).
    pub fn save_to_file(&self, path: &Path) {
        let t0 = Instant::now();
        let guard = match self.inner.read() {
            Ok(g) => g,
            Err(_) => return,
        };
        let mb = guard.bytes_used / (1024 * 1024);
        let ok = write_atomic(path, |w| {
            w.write_all(b"ECPRED02")?;
            w.write_all(&(guard.entries.len() as u64).to_le_bytes())?;
            for (&pred_id, pairs) in &guard.entries {
                w.write_all(&pred_id.to_le_bytes())?;
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
            if &magic != b"ECPRED02" {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "pred-cache magic mismatch"));
            }
            let mut buf8 = [0u8; 8];
            reader.read_exact(&mut buf8)?;
            let n = u64::from_le_bytes(buf8) as usize;
            let mut entries: HashMap<TermId, PredPairs> = HashMap::with_capacity(n);
            let mut bytes_used = 0usize;
            for _ in 0..n {
                reader.read_exact(&mut buf8)?;
                let pred_id = u64::from_le_bytes(buf8);
                reader.read_exact(&mut buf8)?;
                let m = u64::from_le_bytes(buf8) as usize;
                let pairs = read_pairs(&mut reader, m)?;
                bytes_used += pairs.len() * 16;
                entries.insert(pred_id, Arc::new(pairs));
            }
            let cache = PredCache::empty();
            if let Ok(mut inner) = cache.inner.write() {
                inner.entries = entries;
                inner.bytes_used = bytes_used;
            }
            Ok(cache)
        })();
        match result {
            Ok(cache) => {
                let n_entries = cache.inner.read().map(|g| g.entries.len()).unwrap_or(0);
                let mb = cache.bytes_used() / (1024 * 1024);
                tracing::info!(
                    path = %path.display(),
                    n_entries,
                    mb,
                    "cache: loaded from file"
                );
                Some(cache)
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), err = %e, "pred-cache: load failed, will rebuild");
                None
            }
        }
    }
}

// ── Cache builder ─────────────────────────────────────────────────────────────

fn build_cache(
    cache: &PredCache,
    index: &TripleIndex,
    budget_bytes: usize,
    per_pred_cap_bytes: usize,
    priority_ids: &[TermId],
) {
    let t0 = Instant::now();
    // Per-predicate size cap.
    //
    // When `per_pred_cap_bytes` is non-zero, it is used directly (set by
    // `pred_cache_per_pred_cap_mb` in config or `--pred-cache-per-pred-cap-mb` on CLI).
    //
    // When zero, fall back to 50% of the total budget.  A 50% cap allows
    // faldo:position/begin (≈188 MB) to be cached with a 512 MB budget while
    // still preventing a single runaway predicate from consuming everything.
    //
    // Motivation for the explicit cap: if two huge predicates (e.g. 479 MB each)
    // consume 957 MB of a 1024 MB budget, the remaining 67 MB is not enough for
    // faldo:begin/position (178 MB each).  Setting `pred_cache_per_pred_cap_mb=200`
    // causes those 479 MB predicates to be skipped, freeing space for faldo.
    let per_pred_cap = if per_pred_cap_bytes > 0 {
        per_pred_cap_bytes
    } else {
        (budget_bytes / 2).max(1) // default: no single predicate > 50% of budget
    };

    // Build size map: pred_id → triple count (from index).
    let mut sizes = index.predicate_sizes();
    sizes.sort_unstable_by(|a, b| b.1.cmp(&a.1)); // descending by count (largest-first)
    let size_map: HashMap<TermId, usize> = sizes.iter().cloned().collect();

    let mut total_loaded: usize = 0;
    let mut total_bytes: usize = 0;
    let mut remaining = budget_bytes;
    let mut skipped_cap: usize = 0;
    let mut skipped_budget: usize = 0;

    tracing::info!(
        budget_mb = budget_bytes / (1024 * 1024),
        per_pred_cap_mb = per_pred_cap / (1024 * 1024),
        predicates = sizes.len(),
        priority_count = priority_ids.len(),
        "pred-cache: starting build"
    );

    // ── Priority pass: load specified predicates first ────────────────────────
    //
    // Priority predicates (pre-resolved TermIds) are guaranteed to be cached
    // before the size-ordered pass, as long as they fit under the per-predicate
    // cap and within the remaining budget.
    let mut priority_loaded: HashSet<TermId> = HashSet::new();

    for &pred_id in priority_ids {
        let count = size_map.get(&pred_id).copied().unwrap_or(0);
        if count == 0 {
            tracing::trace!(pred = pred_id, "pred-cache: priority predicate has 0 triples, skipping");
            continue;
        }
        let entry_bytes = count * 16;
        if entry_bytes > per_pred_cap {
            tracing::debug!(
                pred = pred_id,
                mb = entry_bytes / (1024 * 1024),
                cap_mb = per_pred_cap / (1024 * 1024),
                "pred-cache: priority predicate exceeds per-pred cap, skipping"
            );
            continue;
        }
        if entry_bytes > remaining {
            tracing::debug!(
                pred = pred_id,
                mb = entry_bytes / (1024 * 1024),
                remaining_mb = remaining / (1024 * 1024),
                "pred-cache: priority predicate does not fit in remaining budget, skipping"
            );
            continue;
        }

        let pairs = load_pairs(index, pred_id);
        let actual_bytes = pairs.len() * 16;
        remaining = remaining.saturating_sub(actual_bytes);
        total_bytes += actual_bytes;
        total_loaded += 1;
        priority_loaded.insert(pred_id);

        if let Ok(mut guard) = cache.inner.write() {
            guard.bytes_used += actual_bytes;
            guard.entries.insert(pred_id, Arc::new(pairs));
        }

        tracing::info!(
            pred = pred_id,
            triples = count,
            mb = actual_bytes / (1024 * 1024),
            total_mb = total_bytes / (1024 * 1024),
            remaining_mb = remaining / (1024 * 1024),
            "pred-cache: priority predicate cached"
        );
    }

    // ── Size-ordered pass: fill remaining budget largest-first ────────────────
    //
    // Use `continue` (not `break`) when a predicate doesn't fit — smaller
    // predicates later in the sorted list may still fit.
    for (pred_id, count) in &sizes {
        let (pred_id, count) = (*pred_id, *count);
        if count == 0 { continue; }
        // Skip predicates already loaded in the priority pass.
        if priority_loaded.contains(&pred_id) { continue; }

        let entry_bytes = count * 16; // 16 bytes per (u64, u64) pair

        // Skip predicates that exceed the per-predicate cap (too big to be useful).
        if entry_bytes > per_pred_cap {
            skipped_cap += 1;
            tracing::trace!(
                pred = pred_id,
                mb = entry_bytes / (1024 * 1024),
                cap_mb = per_pred_cap / (1024 * 1024),
                "pred-cache: skipped (exceeds per-pred cap)"
            );
            continue;
        }
        // Skip predicates that no longer fit, but keep trying smaller ones.
        if entry_bytes > remaining {
            skipped_budget += 1;
            tracing::trace!(
                pred = pred_id,
                mb = entry_bytes / (1024 * 1024),
                remaining_mb = remaining / (1024 * 1024),
                "pred-cache: skipped (budget exhausted for this size)"
            );
            continue;
        }

        let pairs = load_pairs(index, pred_id);
        let actual_bytes = pairs.len() * 16;
        remaining = remaining.saturating_sub(actual_bytes);
        total_bytes += actual_bytes;
        total_loaded += 1;

        // Insert into the shared cache (brief write lock per predicate).
        if let Ok(mut guard) = cache.inner.write() {
            guard.bytes_used += actual_bytes;
            guard.entries.insert(pred_id, Arc::new(pairs));
        }

        tracing::info!(
            pred = pred_id,
            triples = count,
            mb = actual_bytes / (1024 * 1024),
            total_mb = total_bytes / (1024 * 1024),
            remaining_mb = remaining / (1024 * 1024),
            "pred-cache: cached predicate"
        );
    }

    tracing::info!(
        predicates_cached = total_loaded,
        predicates_skipped_cap = skipped_cap,
        predicates_skipped_budget = skipped_budget,
        total_mb = total_bytes / (1024 * 1024),
        remaining_mb = remaining / (1024 * 1024),
        elapsed_ms = t0.elapsed().as_millis(),
        "pred-cache: build complete"
    );
}

/// Load all (subject, object) pairs for `pred_id` from the POS index,
/// sorted by (subject, object) for binary-search and merge-join access.
fn load_pairs(index: &TripleIndex, pred_id: TermId) -> Vec<(TermId, TermId)> {
    let pat = TriplePattern::new(UNBOUND, pred_id, UNBOUND);
    let mut pairs: Vec<(TermId, TermId)> = index.scan(&pat)
        .map(|t| (t.s, t.o))
        .collect();
    pairs.sort_unstable();
    pairs
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
