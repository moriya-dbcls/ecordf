//! Per-predicate statistics for query optimization.
//!
//! ## Two-tier cardinality estimation
//!
//! **Tier 1 – index probing** (`index.estimate()`):
//!   For patterns that have constant IRIs/literals in at least one position, the
//!   optimizer calls `TripleIndex::estimate()` which does a binary search over the
//!   sorted index to count the matching range.  This is exact for the two leading
//!   key positions and approximate (~50% discount) for the third.
//!   Cost: O(log N) per pattern, zero memory.
//!
//! **Tier 2 – predicate statistics** (`StoreStatistics::estimate()`):
//!   For patterns whose only bound position is the predicate, index probing
//!   returns the full predicate fanout (potentially millions of rows).  The
//!   predicate statistics file provides per-predicate subject/object counts so
//!   the optimizer can also reason about SP and PO fanout for variable positions.
//!   Cost: O(N) to build once at load time; O(1) thereafter.
//!
//! ## File format (`stats.bin`)
//!
//! ```text
//! offset  0: magic        [u8; 8]  = b"ECOSTAT1"
//! offset  8: total_triples  u64
//! offset 16: n_predicates   u64
//! offset 24: per-predicate records (28 bytes each):
//!            pred_id       u32
//!            triple_count  u64
//!            subject_count u64
//!            object_count  u64
//! ```

use std::collections::HashMap;
use std::io::{self, BufWriter, Write, Read};
use std::path::Path;

use crate::index::TripleIndex;
use crate::triple::{TermId, UNBOUND};

/// Format version 2: pred_id is u64 (was u32 in v1) to match TermId = u64.
const STATS_MAGIC: &[u8; 8] = b"ECOSTAT2";
const RECORD_BYTES: usize = 8 + 8 + 8 + 8; // pred_id(u64) + triple + subject + object

// ── Per-predicate statistics ──────────────────────────────────────────────────

/// Per-predicate statistics: triple count, distinct subject count, distinct
/// object count.  Collected by scanning the SPO and POS indexes in order.
#[derive(Default)]
pub struct PredicateStats {
    /// Total number of triples with this predicate.
    pub triple_count: u64,
    /// Approximate number of distinct subjects that appear with this predicate.
    pub subject_count: u64,
    /// Approximate number of distinct objects that appear with this predicate.
    pub object_count: u64,
}

// ── Store-wide statistics ─────────────────────────────────────────────────────

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

    // ── Construction ──────────────────────────────────────────────────────────

    /// Build statistics by making two O(N) passes over the in-memory indexes.
    ///
    /// Pass 1 — POS order (P, O, S): counts triples and distinct objects per
    /// predicate by detecting when the raw O key changes within a predicate group.
    ///
    /// Pass 2 — SPO order (S, P, O): counts distinct subjects per predicate by
    /// detecting when the (S, P) pair changes.  This is exact because SPO is
    /// sorted so identical (S, P) pairs are always consecutive.
    ///
    /// For large datasets (~1 B triples) each pass takes roughly 5–30 s
    /// depending on available I/O bandwidth.  The result is saved to `stats.bin`
    /// so subsequent `Store::open()` calls load it in milliseconds.
    pub fn build_from_index(index: &TripleIndex) -> Self {
        let total_triples = index.triple_count() as u64;
        let mut predicate_stats: HashMap<TermId, PredicateStats> = HashMap::new();

        // ── Pass 1: POS scan ──────────────────────────────────────────────────
        // The POS index is stored as (P, O, S) in sorted order.  Iterating it
        // with all-UNBOUND returns triples in that physical order, so triples with
        // the same P are consecutive, and within each P the O values are sorted.
        {
            let mut current_p: TermId = UNBOUND;
            let mut current_o: TermId = UNBOUND;

            for triple in index.pos_scan_all() {
                if triple.p != current_p {
                    current_p = triple.p;
                    current_o = UNBOUND;
                }
                // Count every triple (predicate changes or not)
                predicate_stats.entry(triple.p).or_default().triple_count += 1;
                // Count each distinct object value (O changes within P)
                if triple.o != current_o {
                    current_o = triple.o;
                    predicate_stats.entry(triple.p).or_default().object_count += 1;
                }
            }
        }

        // ── Pass 2: SPO scan ──────────────────────────────────────────────────
        // SPO is stored as (S, P, O).  Count distinct (S, P) transitions per
        // predicate to approximate the number of distinct subjects.
        {
            let mut current_s: TermId = UNBOUND;
            let mut current_p: TermId = UNBOUND;

            for triple in index.spo_scan_all() {
                if triple.s != current_s || triple.p != current_p {
                    current_s = triple.s;
                    current_p = triple.p;
                    if let Some(ps) = predicate_stats.get_mut(&triple.p) {
                        ps.subject_count += 1;
                    }
                }
            }
        }

        Self { total_triples, predicate_stats }
    }

    /// Load stats from `stats.bin` if it exists, otherwise build from the index
    /// and save the result so future calls are fast.
    pub fn load_or_build(path: &Path, index: &TripleIndex) -> io::Result<Self> {
        if path.exists() {
            match Self::load(path) {
                Ok(s) => return Ok(s),
                Err(e) => {
                    eprintln!("Warning: could not read stats.bin ({e}); rebuilding.");
                }
            }
        }

        eprintln!("Building predicate statistics (two index passes)…");
        let s = Self::build_from_index(index);
        eprintln!(
            "Statistics built: {} predicates over {} triples.",
            s.predicate_stats.len(),
            s.total_triples
        );

        // Save for next time — non-fatal if the write fails.
        if let Err(e) = s.save(path) {
            eprintln!("Warning: could not save stats.bin ({e}).");
        }

        Ok(s)
    }

    // ── Serialisation ─────────────────────────────────────────────────────────

    /// Write statistics to a binary file.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let f = std::fs::File::create(path)?;
        let mut w = BufWriter::new(f);

        w.write_all(STATS_MAGIC)?;
        w.write_all(&self.total_triples.to_le_bytes())?;
        w.write_all(&(self.predicate_stats.len() as u64).to_le_bytes())?;

        for (pred_id, ps) in &self.predicate_stats {
            w.write_all(&pred_id.to_le_bytes())?;
            w.write_all(&ps.triple_count.to_le_bytes())?;
            w.write_all(&ps.subject_count.to_le_bytes())?;
            w.write_all(&ps.object_count.to_le_bytes())?;
        }
        w.flush()
    }

    /// Read statistics from a binary file previously written by `save()`.
    pub fn load(path: &Path) -> io::Result<Self> {
        let mut data = Vec::new();
        std::fs::File::open(path)?.read_to_end(&mut data)?;

        if data.len() < 24 || &data[0..8] != STATS_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid or corrupt stats.bin",
            ));
        }

        let total_triples = u64::from_le_bytes(data[8..16].try_into().unwrap());
        let n = u64::from_le_bytes(data[16..24].try_into().unwrap()) as usize;

        if data.len() < 24 + n * RECORD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "stats.bin truncated",
            ));
        }

        let mut predicate_stats = HashMap::with_capacity(n);
        let mut off = 24usize;
        for _ in 0..n {
            let pred_id  = u64::from_le_bytes(data[off..off+ 8].try_into().unwrap());
            let tc       = u64::from_le_bytes(data[off+ 8..off+16].try_into().unwrap());
            let sc       = u64::from_le_bytes(data[off+16..off+24].try_into().unwrap());
            let oc       = u64::from_le_bytes(data[off+24..off+32].try_into().unwrap());
            predicate_stats.insert(pred_id, PredicateStats {
                triple_count:  tc,
                subject_count: sc,
                object_count:  oc,
            });
            off += RECORD_BYTES;
        }

        Ok(Self { total_triples, predicate_stats })
    }

    // ── Functional predicate detection (改善6) ───────────────────────────────

    /// Return `true` if `pred_id` is a **functional predicate** — one where
    /// each subject has at most one object.
    ///
    /// A predicate is considered functional when
    ///   `triple_count ≤ subject_count × FUNCTIONAL_RATIO_THRESHOLD`
    /// where `FUNCTIONAL_RATIO_THRESHOLD = 1.05` (5% tolerance for data anomalies).
    ///
    /// Functional predicates include: `dct:identifier`, `up:mnemonic`,
    /// `schema:name` for most entities, `xsd:value`, etc.  For these, the
    /// (S → O) mapping is a single value, so a lookup can return exactly one
    /// object rather than a range of multiple objects.
    ///
    /// ## Why this matters
    ///
    /// When a pattern `?x pred:functional ?value` is used in a JOIN where `?x`
    /// is already bound, the executor can treat each subject as having at most
    /// one object and avoid allocating a Vec for a multi-object scan output.
    ///
    /// This is detected statically from `stats.bin` so there is no runtime cost.
    #[inline]
    pub fn is_functional(&self, pred_id: TermId) -> bool {
        const FUNCTIONAL_RATIO_THRESHOLD: f64 = 1.05;
        self.predicate_stats.get(&pred_id).map_or(false, |ps| {
            ps.subject_count > 0
                && (ps.triple_count as f64) <= (ps.subject_count as f64) * FUNCTIONAL_RATIO_THRESHOLD
        })
    }

    /// Return a `Vec` of all functional predicate IDs.
    ///
    /// Used by the build step to decide which predicates warrant a dedicated
    /// `pp_{id}.bin` partition file (highest priority targets for functional
    /// lookups during query time).
    pub fn functional_predicate_ids(&self) -> Vec<TermId> {
        self.predicate_stats.iter()
            .filter(|(_, ps)| {
                ps.subject_count > 0 && ps.triple_count <= ((ps.subject_count as f64 * 1.05) as u64)
            })
            .map(|(&pred_id, _)| pred_id)
            .collect()
    }

    // ── Cardinality estimation ────────────────────────────────────────────────

    /// Estimate the number of results for a triple pattern.
    ///
    /// `None` for a position means the position is an unbound variable.
    /// `Some(id)` means the position is constrained (either a constant or a
    /// bound variable that will be probed per outer row).
    ///
    /// Uses predicate-level fanout statistics for SP and PO patterns where
    /// index-based probing would return the full predicate cardinality.
    pub fn estimate(&self, s: Option<TermId>, p: Option<TermId>, o: Option<TermId>) -> u64 {
        let n = self.total_triples.max(1);
        match (s, p, o) {
            // SPO: single triple lookup
            (Some(_), Some(_), Some(_)) => 1,

            // SP pattern: how many objects does this subject have for this predicate?
            (Some(_), Some(p_id), None) => self.predicate_stats
                .get(&p_id)
                .map(|ps| (ps.triple_count / ps.subject_count.max(1)).max(1))
                .unwrap_or(n / 100),

            // PO pattern: how many subjects share this predicate/object?
            (None, Some(p_id), Some(_)) => self.predicate_stats
                .get(&p_id)
                .map(|ps| (ps.triple_count / ps.object_count.max(1)).max(1))
                .unwrap_or(n / 100),

            // S only: coarse estimate
            (Some(_), None, None) => n / 1_000,

            // P only: exact triple count for this predicate
            (None, Some(p_id), None) => self.predicate_stats
                .get(&p_id)
                .map(|ps| ps.triple_count)
                .unwrap_or(n / 10),

            // O only: coarse estimate
            (None, None, Some(_)) => n / 100,

            // Full scan
            _ => n,
        }
    }
}
