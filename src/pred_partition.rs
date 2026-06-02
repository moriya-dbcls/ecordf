//! # PredPartition — on-disk per-predicate sorted (S, O) index files
//!
//! A **predicate partition** is a binary file for one predicate containing all
//! its `(subject, object)` pairs sorted by `(S, O)`.  Files are built once
//! during `ecordf build` (or `ecordf build-pred-parts`) and memory-mapped at
//! serve time.
//!
//! ## Motivation
//!
//! `pred_cache` loads predicate pairs **into RAM** with a configurable budget.
//! When a predicate exceeds the budget it is silently skipped, so large
//! predicates like `sio:SIO_000216` (1265 MB) or `dct:identifier` are never
//! cached.
//!
//! Predicate partitions remove that constraint:
//! - Files live on disk; the OS page cache manages which pages are in RAM.
//! - No RAM budget parameter needed; every predicate is always "cached".
//! - Access pattern is identical to `pred_cache` (sorted `(S, O)` → binary
//!   search) but backed by mmap instead of a `Vec`.
//!
//! ## File format
//!
//! ```text
//! offset 0:  magic       [u8; 8]  = b"ECPP0001"
//! offset 8:  count       u64      = number of (S, O) pairs
//! offset 16: data        [(u64, u64); count]  sorted by (S, O)
//! ```
//!
//! Files are stored in `{store_dir}/pred_parts/pp_{pred_id}.bin`.
//!
//! ## Build
//!
//! Call [`build_pred_partitions`] after the main indexes are built:
//!
//! ```bash
//! ecordf build-pred-parts --dir ./store
//! ```
//!
//! Only predicates with at least 1 triple are written.  Existing files are
//! skipped (re-run with `--force` to overwrite).

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use memmap2::Mmap;

use crate::index::TripleIndex;
use crate::triple::TermId;


// ── Constants ─────────────────────────────────────────────────────────────────

const PP_MAGIC: &[u8; 8] = b"ECPP0001";
const PP_HEADER: usize = 16; // magic(8) + count(8)

// ── Per-predicate file ────────────────────────────────────────────────────────

/// A single memory-mapped per-predicate `(S, O)` sorted file.
pub struct PredPartFile {
    /// Keep mmap alive for the lifetime of this struct.
    _mmap: Mmap,
    /// Slice into the mmap data (zero-copy, sorted by (S, O)).
    pairs: &'static [(TermId, TermId)],
}

impl PredPartFile {
    fn open(path: &Path) -> std::io::Result<Self> {
        let file = File::open(path)?;
        let mmap = unsafe { Mmap::map(&file)? };

        if mmap.len() < PP_HEADER {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("pred-part {:?}: file too small", path),
            ));
        }
        if &mmap[0..8] != PP_MAGIC {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("pred-part {:?}: bad magic", path),
            ));
        }

        let count = u64::from_le_bytes(mmap[8..16].try_into().unwrap()) as usize;
        let expected = PP_HEADER + count * 16;
        if mmap.len() < expected {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("pred-part {:?}: truncated ({} < {})", path, mmap.len(), expected),
            ));
        }

        // SAFETY: the mmap is read-only and lives as long as `_mmap`.
        // The 'static lifetime is a white lie: the slice is actually valid for
        // the lifetime of `_mmap`, which is stored in the same struct.
        let pairs: &'static [(TermId, TermId)] = unsafe {
            let ptr = mmap.as_ptr().add(PP_HEADER) as *const (TermId, TermId);
            std::slice::from_raw_parts(ptr, count)
        };

        Ok(Self { _mmap: mmap, pairs })
    }

    /// Number of (S, O) pairs.
    #[inline]
    pub fn len(&self) -> usize {
        self.pairs.len()
    }

    /// All sorted `(S, O)` pairs — used for predicate-scan joins.
    #[inline]
    pub fn pairs(&self) -> &[(TermId, TermId)] {
        self.pairs
    }

    /// Return all objects for `target_s` (O values in ascending order).
    #[inline]
    pub fn get_objects(&self, target_s: TermId) -> &[(TermId, TermId)] {
        let lo = self.pairs.partition_point(|&(s, _)| s < target_s);
        let hi = lo + self.pairs[lo..].partition_point(|&(s, _)| s == target_s);
        &self.pairs[lo..hi]
    }

    /// O(log N) existence check for the pair `(s, o)`.
    #[inline]
    pub fn contains(&self, s: TermId, o: TermId) -> bool {
        self.pairs.binary_search(&(s, o)).is_ok()
    }

    /// O(log N) lookup for the unique object of a **functional predicate**.
    ///
    /// For predicates where each subject has at most one object, this returns
    /// `Some(o)` when subject `s` is found, and `None` when `s` is absent.
    ///
    /// ## Why functional predicates benefit (改善6)
    ///
    /// `get_objects(s)` works correctly for all predicates but allocates no
    /// result for subjects absent in the partition.  For functional predicates
    /// (one object per subject) the slice always has length 0 or 1, so this
    /// specialisation avoids the slice overhead entirely.
    ///
    /// Callers should confirm the predicate is functional via
    /// `StoreStatistics::is_functional()` before using this method.
    #[inline]
    pub fn get_single_object(&self, s: TermId) -> Option<TermId> {
        let lo = self.pairs.partition_point(|&(ps, _)| ps < s);
        self.pairs.get(lo).filter(|&&(ps, _)| ps == s).map(|&(_, o)| o)
    }
}

// ── Collection ────────────────────────────────────────────────────────────────

/// Collection of all available per-predicate partition files for one store.
///
/// Cheaply cloneable — the underlying `Arc<PredPartFile>` entries are shared.
#[derive(Clone)]
pub struct PredPartitions {
    parts: HashMap<TermId, Arc<PredPartFile>>,
}

impl PredPartitions {
    /// Empty collection (no partition files present or `pred_parts_dir` absent).
    pub fn empty() -> Self {
        Self { parts: HashMap::new() }
    }

    /// Open all `pp_*.bin` files under `pred_parts_dir`.
    ///
    /// Files that fail to open are logged at WARN and skipped.
    pub fn open(pred_parts_dir: &Path) -> Self {
        if !pred_parts_dir.exists() {
            return Self::empty();
        }

        let entries = match fs::read_dir(pred_parts_dir) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(?pred_parts_dir, %e, "pred-parts: cannot read directory");
                return Self::empty();
            }
        };

        let mut parts: HashMap<TermId, Arc<PredPartFile>> = HashMap::new();

        for entry in entries.flatten() {
            let path = entry.path();
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if !stem.starts_with("pp_") {
                continue;
            }
            let pred_id: TermId = match stem[3..].parse() {
                Ok(id) => id,
                Err(_) => continue,
            };
            match PredPartFile::open(&path) {
                Ok(f) => {
                    tracing::trace!(pred = pred_id, pairs = f.len(), "pred-parts: opened");
                    parts.insert(pred_id, Arc::new(f));
                }
                Err(e) => {
                    tracing::warn!(?path, %e, "pred-parts: failed to open, skipping");
                }
            }
        }

        tracing::info!(
            loaded = parts.len(),
            ?pred_parts_dir,
            "pred-parts: opened partition files"
        );

        Self { parts }
    }

    /// Look up the partition for `pred_id`.
    ///
    /// Returns `None` when no partition file exists for this predicate
    /// (caller falls back to pred_cache or index scan).
    #[inline]
    pub fn get(&self, pred_id: TermId) -> Option<&PredPartFile> {
        self.parts.get(&pred_id).map(|arc| arc.as_ref())
    }

    /// Number of loaded partition files.
    pub fn len(&self) -> usize {
        self.parts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }
}

// ── Build ─────────────────────────────────────────────────────────────────────

/// Canonical path for the pred-parts subdirectory under `store_dir`.
pub fn pred_parts_dir(store_dir: &Path) -> PathBuf {
    store_dir.join("pred_parts")
}

/// Write one predicate's `(S, O)` pairs to `path`.
fn write_pred_part(path: &Path, pairs: &mut Vec<(TermId, TermId)>) -> std::io::Result<()> {
    pairs.sort_unstable();
    pairs.dedup();

    let file = File::create(path)?;
    let mut w = BufWriter::new(file);

    w.write_all(PP_MAGIC)?;
    w.write_all(&(pairs.len() as u64).to_le_bytes())?;

    for &(s, o) in pairs.iter() {
        w.write_all(&s.to_le_bytes())?;
        w.write_all(&o.to_le_bytes())?;
    }
    w.flush()?;
    Ok(())
}

/// Build per-predicate partition files from `index` into `{store_dir}/pred_parts/`.
///
/// - `force`: overwrite existing files.  When `false`, existing files are kept.
/// - Progress is logged at INFO level.
///
/// Returns the number of partition files written.
pub fn build_pred_partitions(
    store_dir: &Path,
    index: &TripleIndex,
    force: bool,
) -> std::io::Result<usize> {
    let dir = pred_parts_dir(store_dir);
    fs::create_dir_all(&dir)?;

    let t_total = std::time::Instant::now();

    // Scan POS to collect (S, O) per predicate.
    // We process one predicate at a time to keep memory bounded:
    // read all triples in POS order (sorted by P), flush when P changes.
    let mut current_pred: TermId = TermId::MAX;
    let mut current_pairs: Vec<(TermId, TermId)> = Vec::new();
    let mut files_written: usize = 0;

    // We use the POS-ordered scan to naturally group by predicate.
    // index.scan(all_pat) uses best_kind() which for all-unbound returns SPO.
    // We need POS order, so we scan POS directly.
    // Use pos_scan_all() for predicate-grouped ordering (P, O, S).
    for triple in index.pos_scan_all() {
        if triple.p != current_pred {
            // Flush previous predicate.
            if current_pred != TermId::MAX && !current_pairs.is_empty() {
                let path = dir.join(format!("pp_{}.bin", current_pred));
                if force || !path.exists() {
                    write_pred_part(&path, &mut current_pairs)?;
                    files_written += 1;
                    tracing::debug!(
                        pred = current_pred,
                        pairs = current_pairs.len(),
                        ?path,
                        "pred-parts: written"
                    );
                }
                current_pairs.clear();
            }
            current_pred = triple.p;
        }
        current_pairs.push((triple.s, triple.o));
    }

    // Flush last predicate.
    if current_pred != TermId::MAX && !current_pairs.is_empty() {
        let path = dir.join(format!("pp_{}.bin", current_pred));
        if force || !path.exists() {
            write_pred_part(&path, &mut current_pairs)?;
            files_written += 1;
        }
    }

    tracing::info!(
        files_written,
        elapsed_ms = t_total.elapsed().as_millis(),
        ?dir,
        "pred-parts: build complete"
    );

    Ok(files_written)
}
