//! # Index Layer: Memory-mapped sorted triple arrays
//!
//! ## Index storage: memory-mapped files
//!
//! Each index file is mapped into the virtual address space via `memmap2::Mmap`.
//! Actual RAM pages are allocated by the OS only on first access and evicted
//! automatically under memory pressure (OS page cache).
//!
//! Result: working-set memory = only the triples your queries actually touch.
//! For typical SPARQL workloads over large bio datasets (UniProt ~1B triples),
//! only a small fraction of the dataset is touched per query session.
//!
//! ## Index structure
//!
//! Six sorted indexes over integer-encoded triples (all permutations):
//!
//! ```text
//!   SPO: sorted (S,P,O) → s bound; s+p (upper_hint bounds secondary search to deg(s))
//!   POS: sorted (P,O,S) → p bound; p+o (pred_idx: O(1) predicate range)
//!   OSP: sorted (O,S,P) → o bound; o+s (upper_hint bounds secondary search to deg(o))
//!   PSO: sorted (P,S,O) → p+s (pred_idx + binary for S within predicate range)
//!   SOP: sorted (S,O,P) → s+o (skip for S, binary for O within S's range)
//!   OPS: sorted (O,P,S) → o+p (skip for O, binary for P within O's range)
//! ```
//!
//! Older 3-index stores open without PSO/SOP/OPS; queries fall back to the
//! nearest existing index automatically.
//!
//! Each index is a flat binary file of packed u32 triples:
//! `[count: u64][s0,p0,o0, s1,p1,o1, ...]` in the index's sort order.
//!
//! ## File format
//!
//! ```text
//! offset 0:  magic   [u8; 8]   = b"ECOI0001"
//! offset 8:  count   u64       = number of triples
//! offset 16: data    [u32; count*3]  in sorted order
//! ```

use memmap2::{Advice, Mmap};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

// rayon::join is used in build_from_parallel_chunks to merge 3 indexes in parallel.
use rayon;

#[cfg(unix)]
extern crate libc;

use crate::col_delta::{delta_path, DeltaColFile};
use crate::triple::{IndexKind, Quad, TermId, Triple, TriplePattern, UNBOUND};

/// Index file format version 2: term IDs are u64 (8 bytes each).
/// Version 1 used u32 (4 bytes each) and is no longer written.
const INDEX_MAGIC: &[u8; 8] = b"ECOI0002";
const HEADER_SIZE: usize = 16; // magic(8) + count(8)
const TRIPLE_BYTES: usize = 24; // 3 × u64

/// Columnar format: one file per column (`spo.c0`, `spo.c1`, `spo.c2`).
/// Each file: magic(8) + count(8) + data[u64 × count].
const COL_MAGIC: &[u8; 8] = b"ECOCOL01";
const COL_VALUE_BYTES: usize = 8; // one u64 per entry

/// Skip index v1 magic (legacy, single-level).
const SKIP_MAGIC_V1: &[u8; 8] = b"ECOSKIP1";
/// Skip index v2 magic (two-level: L1 stride=512, L2 stride=512²=262144).
///
/// ## File layout (v2)
/// ```text
/// magic(8) = "ECOSKIP2"
/// stride_l1: u32   = 512
/// stride_l2: u32   = 262144
/// l1_count:  u64   number of L1 anchors
/// l2_count:  u64   number of L2 anchors
/// total_count: u64 total entries in c0
/// l1_data: [u64; l1_count]
/// l2_data: [u64; l2_count]
/// ```
const SKIP_MAGIC: &[u8; 8] = b"ECOSKIP2";
const SKIP_HDR: usize = 40; // magic(8)+stride_l1(4)+stride_l2(4)+l1_count(8)+l2_count(8)+total(8)

/// L1 skip anchor: one every SKIP_STRIDE entries.
///
/// At 3 B triples: ~5.86 M anchors × 8 B ≈ 46.9 MB per index.
/// After L1 narrow() the range is exactly SKIP_STRIDE entries = 4 KB = 1 OS page.
const SKIP_STRIDE: usize = 512;

/// L2 skip anchor: one every SKIP_STRIDE² = 262 144 entries.
///
/// At 3 B triples: ~11 444 anchors × 8 B ≈ 91 KB per index — **fits in L2 CPU cache**.
/// Binary search in anchors_l2 costs ~14 L2-cache comparisons (zero page faults),
/// narrowing the L1 search to exactly SKIP_STRIDE anchors (4 KB).
/// Combined: 14 L2-cache hits → 14 L1-cache hits → 1 c0 page fault.
const SKIP_STRIDE_L2: usize = SKIP_STRIDE * SKIP_STRIDE; // 262 144

// ── Helper: derive the three column paths from a base index path ──────────────
//
// `col_paths(dir/spo.bin)` → `[dir/spo.c0, dir/spo.c1, dir/spo.c2]`
//
fn col_paths(base: &Path) -> [PathBuf; 3] {
    let stem = base
        .file_stem()
        .expect("index path must have a filename")
        .to_str()
        .expect("index path must be valid UTF-8");
    let dir = base.parent().unwrap_or_else(|| Path::new("."));
    [
        dir.join(format!("{}.c0", stem)),
        dir.join(format!("{}.c1", stem)),
        dir.join(format!("{}.c2", stem)),
    ]
}

/// Derive the skip-index path from the c0 column path.
/// `dir/spo.c0` → `dir/spo.skip`
fn skip_path_from_c0(c0: &Path) -> PathBuf {
    let stem = c0
        .file_stem()
        .expect("c0 path must have a filename")
        .to_str()
        .expect("c0 path must be valid UTF-8");
    let dir = c0.parent().unwrap_or_else(|| Path::new("."));
    dir.join(format!("{}.skip", stem))
}

// ── Skip index ────────────────────────────────────────────────────────────────

/// Two-level sparse in-memory index over the primary-key column (c0).
///
/// ## Level structure
///
/// ```text
/// L2 anchors: anchors_l2[i] = c0[i × SKIP_STRIDE_L2]    (~11 444 entries for 3 B triples → 91 KB)
/// L1 anchors: anchors[i]    = c0[i × SKIP_STRIDE]        (~5.86 M entries for 3 B triples → 47 MB)
/// ```
///
/// ## I/O improvement (3 B triples, 22 GiB c0 file)
///
/// Before (1-level): `narrow()` binary-searches 47 MB L1 → ~23 cache misses, 1 page fault
/// After  (2-level): L2 binary-search in 91 KB (L2-cache-resident) → 14 L2 hits → narrows to
///   512 L1 anchors (4 KB window) → 14 L1 hits → 1 c0 page fault.
///   Total: zero cold page faults on anchors. Same 1 c0 page fault (irreducible).
struct SkipIndex {
    /// L1: `anchors[i] == c0[i * SKIP_STRIDE]`
    anchors: Vec<u64>,
    /// L2: `anchors_l2[i] == c0[i * SKIP_STRIDE_L2]` where `SKIP_STRIDE_L2 = SKIP_STRIDE²`.
    /// Also equals `anchors[i * SKIP_STRIDE]`.
    /// At 3 B triples: ~11 444 entries × 8 B ≈ 91 KB (fits entirely in L2 CPU cache).
    anchors_l2: Vec<u64>,
    /// Total number of entries in the c0 column.
    count: usize,
}

impl SkipIndex {
    /// Derive L2 anchors from an already-built L1 anchor slice.
    fn build_l2(anchors: &[u64]) -> Vec<u64> {
        (0..)
            .map(|i: usize| i * SKIP_STRIDE)
            .take_while(|&pos| pos < anchors.len())
            .map(|pos| anchors[pos])
            .collect()
    }

    /// Build from an in-memory sorted slice of triples.
    /// Samples c0 (column 0) every `SKIP_STRIDE` rows (L1), then derives L2.
    fn build_from_triples(triples: &[[u64; 3]]) -> Self {
        let count = triples.len();
        let anchors: Vec<u64> = (0..)
            .map(|i: usize| i * SKIP_STRIDE)
            .take_while(|&pos| pos < count)
            .map(|pos| triples[pos][0])
            .collect();
        let anchors_l2 = Self::build_l2(&anchors);
        SkipIndex { anchors, anchors_l2, count }
    }

    /// Build from a delta-encoded column file.
    fn build_from_delta(col: &crate::col_delta::DeltaColFile, count: usize) -> Self {
        let anchors: Vec<u64> = (0..)
            .map(|i: usize| i * SKIP_STRIDE)
            .take_while(|&pos| pos < count)
            .map(|pos| col.get(pos))
            .collect();
        let anchors_l2 = Self::build_l2(&anchors);
        SkipIndex { anchors, anchors_l2, count }
    }

    /// Build by doing one sequential scan over an already-open c0 mmap.
    ///
    /// Called when an existing index was built before skip support was added.
    /// One sequential pass — OS sequential prefetcher makes this fast.
    fn build_from_mmap(mmap: &Mmap, count: usize) -> Self {
        let anchors: Vec<u64> = (0..)
            .map(|i: usize| i * SKIP_STRIDE)
            .take_while(|&pos| pos < count)
            .map(|pos| {
                let off = HEADER_SIZE + pos * COL_VALUE_BYTES;
                u64::from_le_bytes(mmap[off..off + 8].try_into().unwrap())
            })
            .collect();
        let anchors_l2 = Self::build_l2(&anchors);
        SkipIndex { anchors, anchors_l2, count }
    }

    /// Save to a `.skip` file in v2 format.
    ///
    /// Format: `magic(8) + stride_l1(4) + stride_l2(4) + l1_count(8) + l2_count(8) + total(8)
    ///          + l1_data[u64 × l1_count] + l2_data[u64 × l2_count]`
    fn save(&self, path: &Path) -> io::Result<()> {
        let f = File::create(path)?;
        let mut w = BufWriter::new(f);
        w.write_all(SKIP_MAGIC)?;                                         // 8
        w.write_all(&(SKIP_STRIDE as u32).to_le_bytes())?;               // 4
        w.write_all(&(SKIP_STRIDE_L2 as u32).to_le_bytes())?;            // 4
        w.write_all(&(self.anchors.len() as u64).to_le_bytes())?;        // 8
        w.write_all(&(self.anchors_l2.len() as u64).to_le_bytes())?;     // 8
        w.write_all(&(self.count as u64).to_le_bytes())?;                // 8 → total HDR = 40
        for &v in &self.anchors    { w.write_all(&v.to_le_bytes())?; }
        for &v in &self.anchors_l2 { w.write_all(&v.to_le_bytes())?; }
        w.flush()
    }

    /// Load from a `.skip` file (v1 or v2).
    ///
    /// v1 (`ECOSKIP1`): reads L1, derives L2 in-memory (no disk re-write).
    /// v2 (`ECOSKIP2`): reads both L1 and L2 from disk.
    fn load(path: &Path) -> io::Result<Self> {
        let mut buf = Vec::new();
        File::open(path)?.read_to_end(&mut buf)?;
        if buf.len() < 8 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "skip file too small"));
        }

        // ── v1 (ECOSKIP1): legacy single-level ───────────────────────────────
        if &buf[0..8] == SKIP_MAGIC_V1 {
            // Old header: magic(8) + stride(4) + l1_count(4) + total_count(8) = 24 bytes
            const V1_HDR: usize = 24;
            if buf.len() < V1_HDR {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "v1 skip file too small"));
            }
            let stride = u32::from_le_bytes(buf[8..12].try_into().unwrap()) as usize;
            let n      = u32::from_le_bytes(buf[12..16].try_into().unwrap()) as usize;
            let count  = u64::from_le_bytes(buf[16..24].try_into().unwrap()) as usize;
            if stride != SKIP_STRIDE {
                return Err(io::Error::new(io::ErrorKind::InvalidData,
                    format!("v1 skip stride mismatch: {}", stride)));
            }
            if buf.len() < V1_HDR + n * 8 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "v1 skip file truncated"));
            }
            let anchors: Vec<u64> = (0..n)
                .map(|i| u64::from_le_bytes(buf[V1_HDR + i*8 .. V1_HDR + i*8+8].try_into().unwrap()))
                .collect();
            let anchors_l2 = Self::build_l2(&anchors);
            return Ok(SkipIndex { anchors, anchors_l2, count });
        }

        // ── v2 (ECOSKIP2): two-level ─────────────────────────────────────────
        if &buf[0..8] != SKIP_MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad skip magic"));
        }
        if buf.len() < SKIP_HDR {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "v2 skip file too small"));
        }
        let stride_l1 = u32::from_le_bytes(buf[8..12].try_into().unwrap()) as usize;
        let stride_l2 = u32::from_le_bytes(buf[12..16].try_into().unwrap()) as usize;
        let l1_count  = u64::from_le_bytes(buf[16..24].try_into().unwrap()) as usize;
        let l2_count  = u64::from_le_bytes(buf[24..32].try_into().unwrap()) as usize;
        let count     = u64::from_le_bytes(buf[32..40].try_into().unwrap()) as usize;
        if stride_l1 != SKIP_STRIDE || stride_l2 != SKIP_STRIDE_L2 {
            return Err(io::Error::new(io::ErrorKind::InvalidData,
                format!("v2 skip stride mismatch: L1={} L2={}", stride_l1, stride_l2)));
        }
        let expected = SKIP_HDR + (l1_count + l2_count) * 8;
        if buf.len() < expected {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "v2 skip file truncated"));
        }
        let mut off = SKIP_HDR;
        let anchors: Vec<u64> = (0..l1_count)
            .map(|_| { let v = u64::from_le_bytes(buf[off..off+8].try_into().unwrap()); off += 8; v })
            .collect();
        // Recompute L2 from L1 to avoid any serialisation drift, OR load from file:
        // Loading from file is safe because we wrote them ourselves.
        let anchors_l2: Vec<u64> = (0..l2_count)
            .map(|_| { let v = u64::from_le_bytes(buf[off..off+8].try_into().unwrap()); off += 8; v })
            .collect();
        Ok(SkipIndex { anchors, anchors_l2, count })
    }

    /// Return the narrowed `[lo, hi)` range that is guaranteed to contain the
    /// position `lower_bound(key)` in c0.
    ///
    /// ## Two-level search
    ///
    /// 1. L2 binary search in `anchors_l2` (~91 KB, L2-cache-resident for 3 B triples):
    ///    narrows to a window of ≤ `SKIP_STRIDE` L1 anchors (4 KB).
    /// 2. L1 binary search in that 4 KB window:
    ///    narrows to ≤ `SKIP_STRIDE` c0 entries (4 KB = 1 OS page).
    ///
    /// Total anchor work: ~14 L2-cache hits + ~14 L1-cache hits.
    /// c0 disk I/O: exactly 1 OS page (irreducible minimum).
    #[inline]
    fn narrow(&self, key: u64) -> (usize, usize) {
        if self.anchors.is_empty() {
            return (0, self.count);
        }

        // ── Level 2: narrow to a SKIP_STRIDE-wide window of L1 anchors ───────
        let (l1_lo, l1_hi) = if self.anchors_l2.is_empty() {
            (0, self.anchors.len())
        } else {
            let slot_l2 = self.anchors_l2.partition_point(|&a| a < key);
            let lo = slot_l2.saturating_sub(1) * SKIP_STRIDE;
            let hi = if slot_l2 < self.anchors_l2.len() {
                (slot_l2 * SKIP_STRIDE + 1).min(self.anchors.len())
            } else {
                self.anchors.len()
            };
            (lo, hi)
        };

        // ── Level 1: narrow to a SKIP_STRIDE-wide window of c0 entries ───────
        let slot_l1 = l1_lo + self.anchors[l1_lo..l1_hi].partition_point(|&a| a < key);
        let lo = slot_l1.saturating_sub(1) * SKIP_STRIDE;
        let hi = if slot_l1 < self.anchors.len() {
            (slot_l1 * SKIP_STRIDE + 1).min(self.count)
        } else {
            self.count
        };
        (lo, hi)
    }

    /// Return a tight **upper bound** (exclusive) for entries with c0 == `key`.
    ///
    /// Uses L2 then L1 to narrow the search, same two-level strategy as `narrow()`.
    #[inline]
    pub fn upper_hint(&self, key: u64) -> usize {
        if self.anchors.is_empty() {
            return self.count;
        }

        // L2 narrow: find the L1 window
        let (l1_lo, l1_hi) = if self.anchors_l2.is_empty() {
            (0, self.anchors.len())
        } else {
            let slot_l2 = self.anchors_l2.partition_point(|&a| a <= key);
            let lo = slot_l2.saturating_sub(1) * SKIP_STRIDE;
            let hi = if slot_l2 < self.anchors_l2.len() {
                (slot_l2 * SKIP_STRIDE + 1).min(self.anchors.len())
            } else {
                self.anchors.len()
            };
            (lo, hi)
        };

        // L1 narrow: first anchor > key gives the upper bound
        let slot_l1 = l1_lo + self.anchors[l1_lo..l1_hi].partition_point(|&a| a <= key);
        (slot_l1 * SKIP_STRIDE).min(self.count)
    }
}

// ── Predicate secondary index ─────────────────────────────────────────────────

/// Magic for the predicate index file format.
/// File layout: magic(8) + pred_count(8) + entries[(pred:u64, lo:u64, hi:u64) × pred_count]
const PIDX_MAGIC: &[u8; 8] = b"ECOPIDX1";
const PIDX_HDR: usize = 16; // magic(8) + count(8)
const PIDX_ENTRY: usize = 24; // pred(8) + lo(8) + hi(8)

/// Derive the predicate-index path from the c0 column path.
/// `dir/pos.c0` → `dir/pos.pidx`
fn pidx_path_from_c0(c0: &Path) -> PathBuf {
    let stem = c0
        .file_stem()
        .expect("c0 path must have a filename")
        .to_str()
        .expect("c0 path must be valid UTF-8");
    let dir = c0.parent().unwrap_or_else(|| Path::new("."));
    dir.join(format!("{}.pidx", stem))
}

/// Dense in-memory secondary index mapping each predicate TermId to its exact
/// `[lo, hi)` range in the POS columnar arrays.
///
/// ## What this replaces
///
/// Previously, `range_for_pattern({p=P, o=*, s=*})` did:
///   1. `lower_bound_0(P)` — SkipIndex lookup + binary search on c0 → 1 page fault
///   2. Linear scan forward until c0 changes → reading all of P's range
///
/// Step 1 is replaced by a single `HashMap::get` (~10 ns, pure RAM, no I/O).
/// Step 2 is still needed to read the data, but now we jump *directly* to `lo`
/// without any binary search overhead.
///
/// ## Memory
///
/// 24 bytes per unique predicate.  UniProt has ~200 K predicates → ~5 MB.
/// Typical SPARQL datasets: < 10 K predicates → < 240 KB.
///
/// ## Build cost
///
/// One sequential scan of the POS c0 column.  Performed once at index-build
/// time (written alongside `.c0`, `.c1`, `.c2`, `.skip`) or lazily on first
/// open.  Subsequent opens load the `.pidx` file in microseconds.
struct PredicateIndex {
    /// predicate_id → (start_pos, end_pos) in the POS c0/c1/c2 column arrays.
    ranges: HashMap<TermId, (usize, usize)>,
}

impl PredicateIndex {
    /// Build by one sequential scan over sorted POS triples already in RAM.
    /// Called from `write_columnar_from_sorted` at build time — zero extra I/O.
    fn build_from_sorted(triples: &[[u64; 3]]) -> Self {
        let mut ranges: HashMap<TermId, (usize, usize)> = HashMap::new();
        if triples.is_empty() {
            return Self { ranges };
        }
        let mut cur_pred = triples[0][0]; // c0 = predicate in POS ordering
        let mut run_start = 0usize;
        for (i, t) in triples.iter().enumerate().skip(1) {
            if t[0] != cur_pred {
                ranges.insert(cur_pred, (run_start, i));
                cur_pred = t[0];
                run_start = i;
            }
        }
        ranges.insert(cur_pred, (run_start, triples.len()));
        Self { ranges }
    }

    /// Build by one sequential scan over the POS c0 mmap.
    /// Called on first open when the `.pidx` file is absent.
    fn build_from_pos_c0(mmap: &Mmap, count: usize) -> Self {
        let mut ranges: HashMap<TermId, (usize, usize)> = HashMap::new();
        if count == 0 {
            return Self { ranges };
        }
        let read = |pos: usize| -> u64 {
            let off = HEADER_SIZE + pos * COL_VALUE_BYTES;
            u64::from_le_bytes(mmap[off..off + 8].try_into().unwrap())
        };
        let mut cur_pred = read(0);
        let mut run_start = 0usize;
        for i in 1..count {
            let p = read(i);
            if p != cur_pred {
                ranges.insert(cur_pred, (run_start, i));
                cur_pred = p;
                run_start = i;
            }
        }
        ranges.insert(cur_pred, (run_start, count));
        Self { ranges }
    }

    /// Build by one sequential scan over a delta-encoded POS c0 column.
    ///
    /// Called when the `.pidx` file is absent but `.c0.dz` exists (e.g. first
    /// open after `compress-cols`).  Uses `DeltaColIter` for sequential
    /// decompression — no random access, no page faults.
    fn build_from_pos_c0_delta(col0: &crate::col_delta::DeltaColFile, count: usize) -> Self {
        let mut ranges: HashMap<TermId, (usize, usize)> = HashMap::new();
        if count == 0 {
            return Self { ranges };
        }
        let mut iter = col0.iter_from(0);
        let mut cur_pred = match iter.next() {
            Some(v) => v,
            None => return Self { ranges },
        };
        let mut run_start = 0usize;
        for i in 1..count {
            let p = match iter.next() {
                Some(v) => v,
                None => break,
            };
            if p != cur_pred {
                ranges.insert(cur_pred, (run_start, i));
                cur_pred = p;
                run_start = i;
            }
        }
        ranges.insert(cur_pred, (run_start, count));
        Self { ranges }
    }

    /// Build from skip anchors collected during a k-way merge (no extra I/O).
    /// `pred_runs` is a Vec of `(pred, lo, hi)` tuples collected during the merge loop.
    fn build_from_runs(pred_runs: Vec<(TermId, usize, usize)>) -> Self {
        let ranges = pred_runs.into_iter()
            .map(|(p, lo, hi)| (p, (lo, hi)))
            .collect();
        Self { ranges }
    }

    /// Save to a `.pidx` file alongside the POS `.c0` column.
    fn save(&self, path: &Path) -> io::Result<()> {
        let f = File::create(path)?;
        let mut w = BufWriter::new(f);
        w.write_all(PIDX_MAGIC)?;
        w.write_all(&(self.ranges.len() as u64).to_le_bytes())?;
        let mut entries: Vec<_> = self.ranges.iter().collect();
        entries.sort_by_key(|(&p, _)| p);
        for (&pred, &(lo, hi)) in &entries {
            w.write_all(&pred.to_le_bytes())?;
            w.write_all(&(lo as u64).to_le_bytes())?;
            w.write_all(&(hi as u64).to_le_bytes())?;
        }
        w.flush()
    }

    /// Load from a `.pidx` file.
    fn load(path: &Path) -> io::Result<Self> {
        let mut buf = Vec::new();
        File::open(path)?.read_to_end(&mut buf)?;
        if buf.len() < PIDX_HDR {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "pidx file too small"));
        }
        if &buf[0..8] != PIDX_MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad pidx magic"));
        }
        let n = u64::from_le_bytes(buf[8..16].try_into().unwrap()) as usize;
        if buf.len() != PIDX_HDR + n * PIDX_ENTRY {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "pidx file size mismatch"));
        }
        let mut ranges = HashMap::with_capacity(n);
        for i in 0..n {
            let off = PIDX_HDR + i * PIDX_ENTRY;
            let pred = u64::from_le_bytes(buf[off..off + 8].try_into().unwrap());
            let lo   = u64::from_le_bytes(buf[off + 8..off + 16].try_into().unwrap()) as usize;
            let hi   = u64::from_le_bytes(buf[off + 16..off + 24].try_into().unwrap()) as usize;
            ranges.insert(pred, (lo, hi));
        }
        Ok(Self { ranges })
    }

    /// O(1) range lookup for a predicate.  Returns `None` if predicate has no triples.
    #[inline]
    fn get(&self, pred: TermId) -> Option<(usize, usize)> {
        self.ranges.get(&pred).copied()
    }

    /// Number of distinct predicates tracked.
    #[allow(dead_code)]
    fn predicate_count(&self) -> usize {
        self.ranges.len()
    }
}

// ── GSPO quad index constants ─────────────────────────────────────────────────
const GSPO_MAGIC: &[u8; 8] = b"ECOG0002";
const QUAD_BYTES: usize = 32; // 4 × u64 : (g, s, p, o)

/// Maximum number of chunk files opened simultaneously in a single k-way merge pass.
///
/// Prevents EMFILE (`Too many open files`) on systems with low fd limits.
/// When chunk count exceeds this, a hierarchical merge is used automatically.
/// With three indexes merging in parallel the peak fd usage is 3 × MAX_FAN_IN.
const MAX_FAN_IN: usize = 64;

// ── In-memory index (during build) ──────────────────────────────────────────

/// Mutable index used during the load phase.
///
/// ## External-sort mode
///
/// When `chunk_size > 0` the builder uses **external sort**:
///
/// 1. Incoming triples are accumulated in a `Vec` up to `chunk_size` entries.
/// 2. When the buffer is full it is sorted and flushed to a numbered `.tmp`
///    file inside `chunk_dir` — the buffer is then cleared.
/// 3. On `build()` any remaining buffered triples are flushed as a final chunk,
///    and all chunks are merged via a k-way merge (binary heap) into the final
///    index file.  Consecutive duplicate triples are dropped during the merge
///    so the final index is a true set.
/// 4. Temp files are removed after a successful merge; the entire `_ecordf_tmp`
///    directory is cleaned up by `AllBuilders::build`.
///
/// Peak memory = `chunk_size × 12 bytes` per builder.
///
/// When `chunk_size == 0` the old behaviour is used: all triples stay in RAM
/// and are sorted in a single pass.
pub struct IndexBuilder {
    pub kind: IndexKind,
    triples: Vec<[u64; 3]>,
    /// 0 = unbounded (in-memory); > 0 = external-sort chunk threshold.
    chunk_size: usize,
    /// Directory for temporary chunk files (set when chunk_size > 0).
    chunk_dir: Option<PathBuf>,
    /// Paths of flushed sorted chunk files.
    chunks: Vec<PathBuf>,
}

impl IndexBuilder {
    /// Create an unbounded in-memory builder (old behaviour).
    pub fn new(kind: IndexKind) -> Self {
        Self {
            kind,
            triples: Vec::new(),
            chunk_size: 0,
            chunk_dir: None,
            chunks: Vec::new(),
        }
    }

    /// Create a streaming builder that flushes sorted chunks to `chunk_dir`.
    fn new_streaming(kind: IndexKind, chunk_dir: PathBuf, chunk_size: usize) -> Self {
        let cap = chunk_size.min(4_000_000);
        Self {
            kind,
            triples: Vec::with_capacity(cap),
            chunk_size,
            chunk_dir: Some(chunk_dir),
            chunks: Vec::new(),
        }
    }

    /// Append a triple.  Returns `Err` only in streaming mode when a chunk
    /// flush fails.
    pub fn push(&mut self, t: Triple) -> io::Result<()> {
        self.triples.push(reorder(t, self.kind));
        if self.chunk_size > 0 && self.triples.len() >= self.chunk_size {
            self.flush_chunk()?;
        }
        Ok(())
    }

    // ── Internal chunk management ─────────────────────────────────────────────

    fn kind_str(&self) -> &'static str {
        match self.kind {
            IndexKind::Spo => "spo",
            IndexKind::Pos => "pos",
            IndexKind::Osp => "osp",
            IndexKind::Pso => "pso",
            IndexKind::Sop => "sop",
            IndexKind::Ops => "ops",
        }
    }

    fn flush_chunk(&mut self) -> io::Result<()> {
        if self.triples.is_empty() {
            return Ok(());
        }
        let chunk_dir = self.chunk_dir.as_ref().expect("chunk_dir must be set");
        let chunk_path = chunk_dir.join(
            format!("{}_chunk_{:06}.tmp", self.kind_str(), self.chunks.len())
        );
        self.triples.sort_unstable();
        write_triple_chunk(&self.triples, &chunk_path)?;
        self.chunks.push(chunk_path);
        self.triples.clear();
        Ok(())
    }

    // ── parallel support ──────────────────────────────────────────────────────

    /// Flush remaining buffer and return all chunk file paths without merging.
    ///
    /// Used by the parallel loader: each worker thread calls this on its own
    /// `IndexBuilder` and returns the paths to the main thread, which then
    /// calls [`merge_triple_chunks`] once over all gathered paths.
    ///
    /// **Only valid in streaming mode** (`chunk_size > 0`).
    pub(crate) fn flush_and_return_chunks(mut self) -> io::Result<Vec<PathBuf>> {
        debug_assert!(self.chunk_size > 0, "flush_and_return_chunks called on in-memory builder");
        self.flush_chunk()?;
        Ok(self.chunks)
    }

    // ── build ─────────────────────────────────────────────────────────────────

    /// Sort and write to disk in columnar format, then return a read-only `IndexFile`.
    pub fn build(mut self, path: &Path) -> io::Result<IndexFile> {
        let kind = self.kind;
        if self.chunks.is_empty() {
            // ── In-memory path: data already in RAM, write directly to columns.
            self.triples.sort_unstable();
            write_columnar_from_sorted(&self.triples, path, kind)?;
        } else {
            // ── External-sort path: flush remaining buffer, k-way merge → columns.
            self.flush_chunk()?;
            eprintln!(
                "  Merging {} sorted chunks → {:?} index (columnar)…",
                self.chunks.len(), kind
            );
            merge_triple_chunks(&self.chunks, path, kind)?;
            // Remove individual chunk files (the _ecordf_tmp dir is removed by
            // AllBuilders::build once all indexes are written).
            for chunk in &self.chunks {
                let _ = std::fs::remove_file(chunk);
            }
        }
        IndexFile::open(path, kind)
    }
}

// ── Triple chunk helpers ──────────────────────────────────────────────────────

/// Write a pre-sorted slice of raw triples to a binary chunk file.
///
/// Format: `[count: u64][a0, b0, c0, a1, b1, c1, …  : u64 each]`
pub(crate) fn write_triple_chunk(triples: &[[u64; 3]], path: &Path) -> io::Result<()> {
    let file = File::create(path)?;
    let mut w = BufWriter::with_capacity(4 * 1024 * 1024, file);
    w.write_all(&(triples.len() as u64).to_le_bytes())?;
    for t in triples {
        w.write_all(&t[0].to_le_bytes())?;
        w.write_all(&t[1].to_le_bytes())?;
        w.write_all(&t[2].to_le_bytes())?;
    }
    w.flush()
}

/// Write a sorted slice of raw triples as a legacy interleaved index file (with header).
/// Kept for testing and potential external tooling; new builds use `write_columnar_from_sorted`.
#[allow(dead_code)]
pub(crate) fn write_index_from_sorted(triples: &[[u64; 3]], path: &Path) -> io::Result<()> {
    let file = File::create(path)?;
    let mut w = BufWriter::with_capacity(4 * 1024 * 1024, file);
    w.write_all(INDEX_MAGIC)?;
    w.write_all(&(triples.len() as u64).to_le_bytes())?;
    for t in triples {
        w.write_all(&t[0].to_le_bytes())?;
        w.write_all(&t[1].to_le_bytes())?;
        w.write_all(&t[2].to_le_bytes())?;
    }
    w.flush()
}

/// Write a sorted slice of triples as three columnar files plus a skip index.
///
/// Given `base_path = dir/pos.bin` and `kind = Pos`, creates:
///   `dir/pos.c0`   — primary-key column (u64 values, count × 8 bytes)
///   `dir/pos.c1`   — secondary-key column
///   `dir/pos.c2`   — tertiary-key column
///   `dir/pos.skip` — sparse skip index over c0 (built in-memory, no extra I/O)
///   `dir/pos.pidx` — predicate secondary index (only for Pos; built in-memory)
///
/// Each column file: `magic(8) + count(8) + data[u64 × count]`
pub(crate) fn write_columnar_from_sorted(
    triples: &[[u64; 3]],
    base_path: &Path,
    kind: IndexKind,
) -> io::Result<()> {
    let paths = col_paths(base_path);
    let count = triples.len() as u64;
    for (ci, path) in paths.iter().enumerate() {
        let f = File::create(path)?;
        let mut w = BufWriter::with_capacity(4 * 1024 * 1024, f);
        w.write_all(COL_MAGIC)?;
        w.write_all(&count.to_le_bytes())?;
        for t in triples {
            w.write_all(&t[ci].to_le_bytes())?;
        }
        w.flush()?;
    }
    // Build and save the skip index from in-memory c0 data — zero extra I/O.
    let skip = SkipIndex::build_from_triples(triples);
    skip.save(&skip_path_from_c0(&paths[0]))?;
    // Build and save the predicate secondary index for POS — zero extra I/O.
    if kind == IndexKind::Pos || kind == IndexKind::Pso {
        let pidx = PredicateIndex::build_from_sorted(triples);
        pidx.save(&pidx_path_from_c0(&paths[0]))?;
    }
    Ok(())
}

/// k-way merge of sorted triple chunk files into columnar index files.
///
/// Final output is written as three `.c0`/`.c1`/`.c2` column files derived
/// from `base_path` (e.g. `pos.bin` → `pos.c0`, `pos.c1`, `pos.c2`).
/// For `kind = Pos` a `.pidx` predicate secondary index is also emitted.
///
/// When `chunks.len() > MAX_FAN_IN` a **hierarchical merge** is performed
/// automatically using plain chunk format for intermediate passes.
/// Only the final merge writes columnar output.
///
/// Consecutive duplicate triples are dropped so the output is a set.
pub(crate) fn merge_triple_chunks(
    chunks: &[PathBuf],
    base_path: &Path,
    kind: IndexKind,
) -> io::Result<()> {
    if chunks.len() <= MAX_FAN_IN {
        return merge_to_columnar_direct(chunks, base_path, kind);
    }

    // ── Hierarchical pass: merge batches → intermediate plain chunk files ─────
    let mut intermediates: Vec<PathBuf> = Vec::new();
    for (i, batch) in chunks.chunks(MAX_FAN_IN).enumerate() {
        let tmp = append_path_suffix(base_path, &format!(".__merge_{:04}.tmp", i));
        merge_triple_chunks_to_chunk(batch, &tmp)?;
        intermediates.push(tmp);
    }

    // ── Final pass → columnar output ──────────────────────────────────────────
    let result = if intermediates.len() <= MAX_FAN_IN {
        merge_to_columnar_direct(&intermediates, base_path, kind)
    } else {
        merge_triple_chunks(&intermediates, base_path, kind)
    };

    for p in &intermediates {
        let _ = std::fs::remove_file(p);
    }

    result
}

/// Merge up to `MAX_FAN_IN` chunk files directly into three columnar files.
///
/// Writes `base_path`-derived `.c0`, `.c1`, `.c2` column files plus `.skip`.
/// For `kind = Pos` also writes a `.pidx` predicate secondary index.
/// Count is back-patched in each column file after the merge.
fn merge_to_columnar_direct(
    chunks: &[PathBuf],
    base_path: &Path,
    kind: IndexKind,
) -> io::Result<()> {
    let cpaths = col_paths(base_path);

    let mut readers: Vec<TripleChunkReader> = chunks.iter()
        .map(|p| TripleChunkReader::open(p))
        .collect::<io::Result<Vec<_>>>()?;

    // Open one writer per column file.
    let mut writers: [BufWriter<File>; 3] = [
        BufWriter::with_capacity(8 * 1024 * 1024,
            OpenOptions::new().create(true).write(true).truncate(true).open(&cpaths[0])?),
        BufWriter::with_capacity(8 * 1024 * 1024,
            OpenOptions::new().create(true).write(true).truncate(true).open(&cpaths[1])?),
        BufWriter::with_capacity(8 * 1024 * 1024,
            OpenOptions::new().create(true).write(true).truncate(true).open(&cpaths[2])?),
    ];

    // Write column headers with count placeholder.
    for w in &mut writers {
        w.write_all(COL_MAGIC)?;
        w.write_all(&0u64.to_le_bytes())?; // back-patched below
    }

    // ── Seed heap ─────────────────────────────────────────────────────────────
    let mut heap: BinaryHeap<Reverse<([u64; 3], usize)>> = BinaryHeap::new();
    for (i, reader) in readers.iter_mut().enumerate() {
        if let Some(t) = reader.next()? {
            heap.push(Reverse((t, i)));
        }
    }

    // ── Merge with deduplication + skip + predicate-run collection ────────────
    let mut count = 0u64;
    let mut prev: Option<[u64; 3]> = None;
    let mut skip_anchors: Vec<u64> = Vec::new();
    // For Pos: track (pred, run_start, run_end) as c0 transitions.
    let mut pred_runs: Vec<(TermId, usize, usize)> = if kind == IndexKind::Pos || kind == IndexKind::Pso {
        Vec::new()
    } else {
        Vec::with_capacity(0)
    };
    let mut cur_pred: Option<TermId> = None;
    let mut cur_run_start: usize = 0;

    while let Some(Reverse((t, i))) = heap.pop() {
        if Some(t) != prev {
            let pos = count as usize;
            // Record one skip anchor every SKIP_STRIDE deduplicated output rows.
            if pos % SKIP_STRIDE == 0 {
                skip_anchors.push(t[0]);
            }
            // Track predicate-run boundaries for the Pos predicate index.
            if kind == IndexKind::Pos || kind == IndexKind::Pso {
                match cur_pred {
                    None => {
                        cur_pred = Some(t[0]);
                        cur_run_start = pos;
                    }
                    Some(p) if p != t[0] => {
                        pred_runs.push((p, cur_run_start, pos));
                        cur_pred = Some(t[0]);
                        cur_run_start = pos;
                    }
                    _ => {}
                }
            }
            writers[0].write_all(&t[0].to_le_bytes())?;
            writers[1].write_all(&t[1].to_le_bytes())?;
            writers[2].write_all(&t[2].to_le_bytes())?;
            count += 1;
            prev = Some(t);
        }
        if let Some(next) = readers[i].next()? {
            heap.push(Reverse((next, i)));
        }
    }
    // Close the final predicate run.
    if kind == IndexKind::Pos || kind == IndexKind::Pso {
        if let Some(p) = cur_pred {
            pred_runs.push((p, cur_run_start, count as usize));
        }
    }

    for w in &mut writers { w.flush()?; }
    drop(writers);

    // ── Back-patch count field (offset 8) in each column file ────────────────
    for cpath in &cpaths {
        let mut f = OpenOptions::new().write(true).open(cpath)?;
        f.seek(SeekFrom::Start(8))?;
        f.write_all(&count.to_le_bytes())?;
    }

    // ── Write skip index (anchors collected during merge, no extra I/O) ───────
    let anchors_l2 = SkipIndex::build_l2(&skip_anchors);
    let skip = SkipIndex { anchors: skip_anchors, anchors_l2, count: count as usize };
    skip.save(&skip_path_from_c0(&cpaths[0]))?;

    // ── Write predicate secondary index for Pos (no extra I/O) ───────────────
    if kind == IndexKind::Pos || kind == IndexKind::Pso {
        let pidx = PredicateIndex::build_from_runs(pred_runs);
        pidx.save(&pidx_path_from_c0(&cpaths[0]))?;
    }

    Ok(())
}

/// Merge up to `MAX_FAN_IN` chunk files into a **new chunk file** at `path`.
///
/// Output format: `[count: u64][triple data…]` (no index magic header),
/// identical to the input chunk format so it can be fed into another pass.
/// The count field is back-patched after writing all triples.
fn merge_triple_chunks_to_chunk(chunks: &[PathBuf], path: &Path) -> io::Result<()> {
    let mut readers: Vec<TripleChunkReader> = chunks.iter()
        .map(|p| TripleChunkReader::open(p))
        .collect::<io::Result<Vec<_>>>()?;

    let out_file = OpenOptions::new().create(true).write(true).truncate(true).open(path)?;
    let mut w = BufWriter::with_capacity(8 * 1024 * 1024, out_file);
    w.write_all(&0u64.to_le_bytes())?; // count placeholder

    let mut heap: BinaryHeap<Reverse<([u64; 3], usize)>> = BinaryHeap::new();
    for (i, reader) in readers.iter_mut().enumerate() {
        if let Some(t) = reader.next()? {
            heap.push(Reverse((t, i)));
        }
    }

    let mut count = 0u64;
    let mut prev: Option<[u64; 3]> = None;
    while let Some(Reverse((t, i))) = heap.pop() {
        if Some(t) != prev {
            w.write_all(&t[0].to_le_bytes())?;
            w.write_all(&t[1].to_le_bytes())?;
            w.write_all(&t[2].to_le_bytes())?;
            count += 1;
            prev = Some(t);
        }
        if let Some(next) = readers[i].next()? {
            heap.push(Reverse((next, i)));
        }
    }
    w.flush()?;
    drop(w);

    // Back-patch count at offset 0.
    let mut f = OpenOptions::new().write(true).open(path)?;
    f.seek(SeekFrom::Start(0))?;
    f.write_all(&count.to_le_bytes())?;

    Ok(())
}

/// Append a suffix to a path's filename (e.g. `spo.bin` → `spo.bin.__merge_0000.tmp`).
fn append_path_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

/// Streaming reader for a binary triple chunk file.
struct TripleChunkReader {
    reader: BufReader<File>,
    remaining: u64,
}

impl TripleChunkReader {
    fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let mut reader = BufReader::with_capacity(4 * 1024 * 1024, file);
        let mut count_buf = [0u8; 8];
        reader.read_exact(&mut count_buf)?;
        let remaining = u64::from_le_bytes(count_buf);
        Ok(Self { reader, remaining })
    }

    fn next(&mut self) -> io::Result<Option<[u64; 3]>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        let mut buf = [0u8; 24];
        self.reader.read_exact(&mut buf)?;
        self.remaining -= 1;
        Ok(Some([
            u64::from_le_bytes(buf[0..8].try_into().unwrap()),
            u64::from_le_bytes(buf[8..16].try_into().unwrap()),
            u64::from_le_bytes(buf[16..24].try_into().unwrap()),
        ]))
    }
}

// ── Read-only mmap-backed index ──────────────────────────────────────────────

/// Internal storage backing for `IndexFile`.
///
/// **Interleaved** (legacy): one file, triples packed as `[a0,b0,c0, a1,b1,c1, …]`.
/// **Columnar** (current):   three files, each a contiguous `[u64; count]` array.
///
/// The columnar layout gives much better CPU-cache behaviour for binary search
/// and range-detection: searching the primary key (`raw[0]`) only touches the
/// first column file; the other two pages stay cold until triples are emitted.
enum IndexStorage {
    Interleaved {
        _file: File,
        mmap:  Mmap,
    },
    Columnar {
        /// One open `File` per column to keep the OS file handle alive.
        _files: [File; 3],
        /// Memory-mapped views of `.c0`, `.c1`, `.c2`.
        mmaps:  [Mmap; 3],
    },
    /// Delta-encoded columnar format (ECOCOL02).
    ///
    /// Column files are `<name>.c0.dz`, `<name>.c1.dz`, `<name>.c2.dz`.
    /// The block index for each column is loaded into RAM; compressed data
    /// stays mmap'd and decompressed lazily per block.
    DeltaColumnar {
        cols: [DeltaColFile; 3],
    },
}

/// A read-only sorted index, backed by either the legacy interleaved file or
/// three separate columnar mmap files.
pub struct IndexFile {
    pub kind: IndexKind,
    count:    usize,
    storage:  IndexStorage,
    /// Sparse in-memory index over c0: narrows binary search from ~31 random
    /// I/Os to a ≤32 KB contiguous range (1–2 sequential I/Os).
    /// `None` for legacy interleaved indexes (skip not supported there).
    skip:     Option<SkipIndex>,
    /// Dense predicate secondary index: predicate → exact [lo, hi) range in POS.
    /// Only populated for the POS index (kind == Pos).
    /// Turns `range_for_pattern({p=P, o=*})` from binary search → O(1) HashMap lookup.
    pred_idx: Option<PredicateIndex>,
}

impl IndexFile {
    // ── Construction ──────────────────────────────────────────────────────────

    /// Open an index, auto-detecting the format:
    /// 1. Delta-encoded `.c0.dz` / `.c1.dz` / `.c2.dz` (ECOCOL02) — preferred.
    /// 2. Uncompressed columnar `.c0` / `.c1` / `.c2` (ECOCOL01).
    /// 3. Legacy interleaved `.bin` (ECOI0002) — fallback.
    pub fn open(path: &Path, kind: IndexKind) -> io::Result<Self> {
        let cpaths = col_paths(path);
        let dzpaths: [PathBuf; 3] = [
            delta_path(&cpaths[0]),
            delta_path(&cpaths[1]),
            delta_path(&cpaths[2]),
        ];
        if dzpaths[0].exists() && dzpaths[1].exists() && dzpaths[2].exists() {
            Self::open_delta(&dzpaths, kind)
        } else if cpaths[0].exists() {
            Self::open_columnar(&cpaths, kind)
        } else {
            Self::open_interleaved(path, kind)
        }
    }

    /// Open three delta-encoded column files (`.c0.dz`, `.c1.dz`, `.c2.dz`).
    fn open_delta(paths: &[PathBuf; 3], kind: IndexKind) -> io::Result<Self> {
        let col0 = DeltaColFile::open(&paths[0])?;
        let col1 = DeltaColFile::open(&paths[1])?;
        let col2 = DeltaColFile::open(&paths[2])?;

        if col0.count != col1.count || col0.count != col2.count {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "delta col count mismatch"));
        }
        let count = col0.count;

        // The .skip and .pidx files are stored alongside the RAW column files
        // (e.g. `pos.skip`, `pos.pidx`), not alongside the `.dz` files
        // (e.g. `pos.c0.dz`).  Strip the `.dz` extension to get the base c0 path.
        //
        // delta_path(pos.c0) = pos.c0.dz  →  pos.c0.dz.with_extension("") = pos.c0
        let c0_base = paths[0].with_extension("");

        // Build SkipIndex and PredicateIndex from the delta col (sequential c0 scan).
        let skip_p = skip_path_from_c0(&c0_base);
        let skip = if skip_p.exists() {
            SkipIndex::load(&skip_p).ok().filter(|s| s.count == count)
        } else {
            None
        };
        // Build SkipIndex from the delta column if not cached.
        let skip = skip.unwrap_or_else(|| {
            eprintln!("  [{:?}] Building skip index from delta column…", kind);
            let s = SkipIndex::build_from_delta(&col0, count);
            let _ = s.save(&skip_p);
            s
        });

        // Build PredicateIndex for POS/PSO.
        let pred_idx = if kind == IndexKind::Pos || kind == IndexKind::Pso {
            let pidx_p = pidx_path_from_c0(&c0_base);
            if pidx_p.exists() {
                PredicateIndex::load(&pidx_p).ok()
            } else {
                // pidx not found — rebuild from delta column c0 (one sequential scan).
                eprintln!("  [Pos/Pso] Building predicate index from delta column…");
                let pi = PredicateIndex::build_from_pos_c0_delta(&col0, count);
                let _ = pi.save(&pidx_p);
                Some(pi)
            }
        } else {
            None
        };

        Ok(Self {
            kind,
            count,
            storage: IndexStorage::DeltaColumnar { cols: [col0, col1, col2] },
            skip: Some(skip),
            pred_idx,
        })
    }

    /// Open a legacy interleaved index file.
    fn open_interleaved(path: &Path, kind: IndexKind) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).open(path)?;
        // Safety: we never modify the file while it is mapped.
        let mmap = unsafe { Mmap::map(&file)? };

        if mmap.len() < HEADER_SIZE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "index too small"));
        }
        if &mmap[0..8] != INDEX_MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad index magic"));
        }
        let count = u64::from_le_bytes(mmap[8..16].try_into().unwrap()) as usize;
        let expected = HEADER_SIZE + count * TRIPLE_BYTES;
        if mmap.len() != expected {
            return Err(io::Error::new(io::ErrorKind::InvalidData,
                format!("index size mismatch: {} vs {}", mmap.len(), expected)));
        }
        Ok(Self { kind, count, storage: IndexStorage::Interleaved { _file: file, mmap }, skip: None, pred_idx: None })
    }

    /// Open three columnar column files (`.c0`, `.c1`, `.c2`).
    ///
    /// Also loads (or builds-and-saves) the `.skip` file alongside c0.
    fn open_columnar(paths: &[PathBuf; 3], kind: IndexKind) -> io::Result<Self> {
        let mut count_opt: Option<usize> = None;
        let mut files: Vec<File> = Vec::with_capacity(3);
        let mut mmaps: Vec<Mmap> = Vec::with_capacity(3);

        for path in paths {
            let file = OpenOptions::new().read(true).open(path)?;
            let mmap = unsafe { Mmap::map(&file)? };

            if mmap.len() < HEADER_SIZE {
                return Err(io::Error::new(io::ErrorKind::InvalidData,
                    format!("column file {:?} too small", path)));
            }
            if &mmap[0..8] != COL_MAGIC {
                return Err(io::Error::new(io::ErrorKind::InvalidData,
                    format!("bad column magic in {:?}", path)));
            }
            let n = u64::from_le_bytes(mmap[8..16].try_into().unwrap()) as usize;
            let expected = HEADER_SIZE + n * COL_VALUE_BYTES;
            if mmap.len() != expected {
                return Err(io::Error::new(io::ErrorKind::InvalidData,
                    format!("column {:?} size mismatch: {} vs {}", path, mmap.len(), expected)));
            }
            match count_opt {
                None => count_opt = Some(n),
                Some(c) if c != n => return Err(io::Error::new(
                    io::ErrorKind::InvalidData, "column count mismatch")),
                _ => {}
            }
            files.push(file);
            mmaps.push(mmap);
        }

        let count = count_opt.unwrap_or(0);

        // ── Load or build the skip index while we still own mmaps as a Vec ─────
        // (mmaps[0] is borrowed here; it will be consumed into the array below.)
        let skip_p = skip_path_from_c0(&paths[0]);
        let skip = if skip_p.exists() {
            match SkipIndex::load(&skip_p) {
                Ok(s) if s.count == count => {
                    Some(s) // fast path: valid cached skip file
                }
                _ => {
                    // Stale or corrupt skip file — rebuild from a sequential scan.
                    let s = SkipIndex::build_from_mmap(&mmaps[0], count);
                    let _ = s.save(&skip_p); // best-effort; ignore write errors
                    Some(s)
                }
            }
        } else {
            // First open after migration from interleaved or pre-skip build.
            // One sequential pass over c0 — fast due to OS sequential prefetch.
            eprintln!("  [{:?}] Building skip index (one-time scan of c0)…", kind);
            let s = SkipIndex::build_from_mmap(&mmaps[0], count);
            let _ = s.save(&skip_p);
            Some(s)
        };

        // ── Load or build the predicate secondary index (Pos only) ─────────────
        //
        // For the POS index, each unique predicate occupies a contiguous run of
        // entries in c0.  The predicate index maps pred → exact (lo, hi) so that
        // range_for_pattern({p=P, o=*}) can skip the binary search entirely and
        // jump straight to the right range with a single HashMap::get call.
        //
        // Build cost: one sequential c0 scan (same as SkipIndex).  Saved to
        // `pos.pidx` so subsequent opens are instant (microseconds to read).
        let pred_idx = if kind == IndexKind::Pos || kind == IndexKind::Pso {
            let pidx_p = pidx_path_from_c0(&paths[0]);
            if pidx_p.exists() {
                match PredicateIndex::load(&pidx_p) {
                    Ok(pi) => Some(pi),
                    Err(_) => {
                        // Corrupt file — rebuild.
                        eprintln!("  [Pos] Rebuilding predicate index (one-time scan of c0)…");
                        let pi = PredicateIndex::build_from_pos_c0(&mmaps[0], count);
                        let _ = pi.save(&pidx_p);
                        Some(pi)
                    }
                }
            } else {
                // First open after upgrade — build and cache.
                eprintln!("  [Pos] Building predicate index (one-time scan of c0)…");
                let pi = PredicateIndex::build_from_pos_c0(&mmaps[0], count);
                let _ = pi.save(&pidx_p);
                Some(pi)
            }
        } else {
            None
        };

        // Convert Vec<T> into [T; 3] — safe because we pushed exactly 3 elements.
        let [f0, f1, f2]: [File; 3] = files.try_into()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "column count != 3"))?;
        let [m0, m1, m2]: [Mmap; 3] = mmaps.try_into()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "column mmap count != 3"))?;

        Ok(Self {
            kind,
            count,
            storage: IndexStorage::Columnar {
                _files: [f0, f1, f2],
                mmaps:  [m0, m1, m2],
            },
            skip,
            pred_idx,
        })
    }

    // ── Public size info ──────────────────────────────────────────────────────

    pub fn len(&self) -> usize { self.count }
    pub fn is_empty(&self) -> bool { self.count == 0 }

    // ── Page-cache release ────────────────────────────────────────────────────

    /// Advise the OS to release page-cache pages for all three column files
    /// (`madvise(MADV_DONTNEED)`).
    ///
    /// Called after large sequential scans to prevent EcoRDF from monopolising
    /// the OS page cache when sharing a host with other services.  The kernel
    /// may ignore the hint or delay acting on it; subsequent accesses will
    /// page-fault again (cold cache).
    ///
    /// Only effective for `Columnar` storage (mmap-backed).  `Interleaved` and
    /// `DeltaColumnar` variants are no-ops because:
    ///   - Interleaved: less common; MADV_DONTNEED still works but offers less
    ///     benefit (data is interleaved, freeing partial ranges is harder).
    ///   - DeltaColumnar: decompressed data lives on the heap (Vec), not mmap;
    ///     the OS cannot reclaim heap pages via madvise.
    ///
    /// Returns approximate bytes released.
    pub fn advise_dontneed(&self) -> usize {
        if let IndexStorage::Columnar { mmaps, .. } = &self.storage {
            let bytes = self.count * COL_VALUE_BYTES;
            for mmap in mmaps.iter() {
                // memmap2 0.9 does not expose MADV_DONTNEED in its Advice enum,
                // so we call libc::madvise directly.
                #[cfg(unix)]
                unsafe {
                    let ptr = mmap.as_ptr().add(HEADER_SIZE) as *mut libc::c_void;
                    libc::madvise(ptr, bytes, libc::MADV_DONTNEED);
                }
                let _ = mmap; // suppress unused warning on non-unix
            }
            bytes * 3
        } else {
            0
        }
    }

    // ── Async prefetch ────────────────────────────────────────────────────────

    /// Non-blocking hint to the OS to prefetch the c0 pages covering entry
    /// range `[lo, hi)` into the page cache.
    ///
    /// Called immediately after `narrow()` so that while the CPU performs the
    /// binary search on already-hot skip-index anchors, the kernel can pipeline
    /// the disk read for the target 4 KB page.
    ///
    /// No-op for interleaved format, empty ranges, and already-resident pages
    /// (the OS just notes the hint and returns immediately in those cases).
    /// On non-Unix platforms `Advice::WillNeed` is a no-op in memmap2.
    #[inline]
    fn prefetch_c0(&self, lo: usize, hi: usize) {
        if lo >= hi {
            return;
        }
        if let IndexStorage::Columnar { mmaps, .. } = &self.storage {
            let byte_start = HEADER_SIZE + lo * COL_VALUE_BYTES;
            let byte_end   = HEADER_SIZE + hi * COL_VALUE_BYTES;
            // Page-align downward so madvise gets a valid aligned address.
            const PAGE: usize = 4096;
            let aligned_start = (byte_start / PAGE) * PAGE;
            let len = byte_end.saturating_sub(aligned_start);
            // Ignore errors — this is a best-effort performance hint.
            let _ = mmaps[0].advise_range(Advice::WillNeed, aligned_start, len);
        }
    }

    /// Fire `madvise(MADV_WILLNEED)` on **all three** column files for entry
    /// range `[lo, hi)`.  Returns immediately; the kernel reads the pages
    /// asynchronously in the background.
    ///
    /// Use this before a full triple read (`get_raw`), which touches all three
    /// columns.  Firing hints for many ranges in a tight loop lets the OS
    /// pipeline their I/Os in parallel (up to the SSD's native queue depth),
    /// converting O(N × serial_latency) to O(⌈N/queue_depth⌉ × latency).
    pub fn prefetch_all_cols(&self, lo: usize, hi: usize) {
        if lo >= hi { return; }
        if let IndexStorage::Columnar { mmaps, .. } = &self.storage {
            let byte_start = HEADER_SIZE + lo * COL_VALUE_BYTES;
            let byte_end   = HEADER_SIZE + hi * COL_VALUE_BYTES;
            const PAGE: usize = 4096;
            let aligned_start = (byte_start / PAGE) * PAGE;
            let len = byte_end.saturating_sub(aligned_start);
            if len == 0 { return; }
            for mmap in mmaps.iter() {
                let _ = mmap.advise_range(Advice::WillNeed, aligned_start, len);
            }
        }
    }

    /// Return the skip-index narrow range for primary key `k0` using **only
    /// in-RAM data** (no disk access).
    ///
    /// The returned `(lo, hi)` guarantees `lower_bound_0(k0) ∈ [lo, hi)`.
    /// With `SKIP_STRIDE = 512` the range covers at most 513 entries — one 4 KB
    /// page — so a single `prefetch_all_cols(lo, hi)` call loads exactly the
    /// pages the binary search will touch.
    #[inline]
    pub fn skip_narrow(&self, k0: u64) -> (usize, usize) {
        match &self.skip {
            Some(s) => s.narrow(k0),
            None    => (0, self.count),
        }
    }

    /// Prefetch (non-blocking) the single 4 KB page that contains the binary
    /// search target for primary key `k0`.  Uses `skip_narrow` (in-RAM) to
    /// locate the page, then fires `prefetch_all_cols`.
    #[inline]
    pub fn prefetch_for_key(&self, k0: u64) {
        let (lo, hi) = self.skip_narrow(k0);
        self.prefetch_all_cols(lo, hi);
    }

    // ── Column accessors ──────────────────────────────────────────────────────
    //
    // Three granularities let callers load only the columns they need:
    //
    //  `get_col0`  — primary key only   (binary search, range-end scan)
    //  `get_col01` — primary + secondary (two-key binary search, (k0,k1) range-end)
    //  `get_raw`   — all three          (emitting full triples to the caller)
    //
    // With columnar storage `get_col0` touches only the `.c0` mmap page; the
    // `.c1` and `.c2` pages remain cold.  For a binary search over 1 B triples
    // (log₂ ≈ 30 steps × 8 B) this is roughly 4 cache misses vs 12 for interleaved.

    /// Read only column 0 — primary sort key.
    #[inline]
    fn get_col0(&self, pos: usize) -> u64 {
        debug_assert!(pos < self.count);
        match &self.storage {
            IndexStorage::Interleaved { mmap, .. } => {
                let off = HEADER_SIZE + pos * TRIPLE_BYTES;
                u64::from_le_bytes(mmap[off..off + 8].try_into().unwrap())
            }
            IndexStorage::Columnar { mmaps, .. } => {
                let off = HEADER_SIZE + pos * COL_VALUE_BYTES;
                u64::from_le_bytes(mmaps[0][off..off + 8].try_into().unwrap())
            }
            IndexStorage::DeltaColumnar { cols } => cols[0].get(pos),
        }
    }

    /// Read columns 0 and 1 — primary and secondary sort keys.
    #[inline]
    fn get_col01(&self, pos: usize) -> (u64, u64) {
        debug_assert!(pos < self.count);
        match &self.storage {
            IndexStorage::Interleaved { mmap, .. } => {
                let off = HEADER_SIZE + pos * TRIPLE_BYTES;
                (
                    u64::from_le_bytes(mmap[off..off + 8].try_into().unwrap()),
                    u64::from_le_bytes(mmap[off + 8..off + 16].try_into().unwrap()),
                )
            }
            IndexStorage::Columnar { mmaps, .. } => {
                let off = HEADER_SIZE + pos * COL_VALUE_BYTES;
                (
                    u64::from_le_bytes(mmaps[0][off..off + 8].try_into().unwrap()),
                    u64::from_le_bytes(mmaps[1][off..off + 8].try_into().unwrap()),
                )
            }
            IndexStorage::DeltaColumnar { cols } => (cols[0].get(pos), cols[1].get(pos)),
        }
    }

    /// Read all three columns — needed when emitting a full triple.
    #[inline]
    fn get_raw(&self, pos: usize) -> [u64; 3] {
        debug_assert!(pos < self.count);
        match &self.storage {
            IndexStorage::Interleaved { mmap, .. } => {
                let off = HEADER_SIZE + pos * TRIPLE_BYTES;
                [
                    u64::from_le_bytes(mmap[off..off + 8].try_into().unwrap()),
                    u64::from_le_bytes(mmap[off + 8..off + 16].try_into().unwrap()),
                    u64::from_le_bytes(mmap[off + 16..off + 24].try_into().unwrap()),
                ]
            }
            IndexStorage::Columnar { mmaps, .. } => {
                let off = HEADER_SIZE + pos * COL_VALUE_BYTES;
                [
                    u64::from_le_bytes(mmaps[0][off..off + 8].try_into().unwrap()),
                    u64::from_le_bytes(mmaps[1][off..off + 8].try_into().unwrap()),
                    u64::from_le_bytes(mmaps[2][off..off + 8].try_into().unwrap()),
                ]
            }
            IndexStorage::DeltaColumnar { cols } => {
                [cols[0].get(pos), cols[1].get(pos), cols[2].get(pos)]
            }
        }
    }

    /// Convert a raw (index-reordered) triple back to SPO order.
    #[inline]
    fn to_triple(&self, raw: [u64; 3]) -> Triple {
        reorder_back(raw, self.kind)
    }

    // ── Binary search (primary key only) ─────────────────────────────────────

    /// First position where `col0 >= key`.  Reads only column 0.
    ///
    /// **DeltaColumnar fast path**: uses `DeltaColFile::lower_bound` which
    /// binary-searches the block index (all in RAM / page-cached) and decompresses
    /// exactly one block — instead of decompressing a full block per binary search
    /// probe (log₂(512) ≈ 9 probes × 256 entries = 2,304 ops with the skip path).
    ///
    /// **Columnar / Interleaved**: uses the skip index to narrow the binary search
    /// to ≤ SKIP_STRIDE entries (1 OS page), then fires a non-blocking prefetch.
    /// First position where `col0 >= key`.  Reads only column 0.
    ///
    /// **DeltaColumnar fast path**: uses `DeltaColFile::lower_bound` which
    /// binary-searches the block index (all in RAM / page-cached) and decompresses
    /// exactly one block — instead of decompressing a full block per binary search
    /// probe (log₂(512) ≈ 9 probes × 256 entries = 2,304 ops with the skip path).
    ///
    /// **Columnar / Interleaved**: uses the skip index to narrow the binary search
    /// to ≤ SKIP_STRIDE entries (1 OS page), then fires a non-blocking prefetch.
    fn lower_bound_0(&self, key: u64) -> usize {
        if let IndexStorage::DeltaColumnar { cols } = &self.storage {
            return cols[0].lower_bound(key);
        }
        let (mut lo, mut hi) = match &self.skip {
            Some(s) => s.narrow(key),
            None    => (0, self.count),
        };
        self.prefetch_c0(lo, hi);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.get_col0(mid) < key { lo = mid + 1; } else { hi = mid; }
        }
        lo
    }

    /// First position where `(col0, col1) >= (k0, k1)`.  Reads columns 0 and 1.
    ///
    /// ## Why narrow(k0).hi cannot be used here
    ///
    /// `narrow(k0)` returns a 512-entry window around the **first** occurrence
    /// of k0 in c0.  When k0 is a common value (e.g. `rdf:type` spanning
    /// billions of entries across thousands of skip blocks), the answer for
    /// `(k0, k1)` with a large k1 lies deep inside k0's range — far beyond
    /// the narrow window.  Clamping the binary search to that window would
    /// cause the search to return `hi` (not found) and produce empty results.
    ///
    /// ## Two-phase approach
    ///
    /// Phase 1 — `lower_bound_0(k0)`: skip-optimised single-key search on c0.
    ///   Touches exactly 1 OS page via skip + prefetch_c0.
    ///   Result `lo` = first position where c0 >= k0.
    ///
    /// Phase 2 — binary search `[lo, count)` with `(c0, c1)` comparator.
    ///   `lo` is tight (nothing before it can satisfy the predicate), so this
    ///   is correct.  The range is `count - lo` which equals the size of the
    ///   k0 range for exact predicate matches, giving O(log(|k0 range|)) steps.
    fn lower_bound_01(&self, k0: u64, k1: u64) -> usize {
        // Phase 1: find the exact start of k0's range, skip-optimised.
        let lo = self.lower_bound_0(k0);
        // Phase 2: binary search within [lo, count) with the two-key comparator.
        let (mut a, mut b) = (lo, self.count);
        while a < b {
            let mid = a + (b - a) / 2;
            let (c0, c1) = self.get_col01(mid);
            if (c0, c1) < (k0, k1) { a = mid + 1; } else { b = mid; }
        }
        a
    }

    // ── Scan API ──────────────────────────────────────────────────────────────

    /// Scan the index for all triples matching `pat`. Yields triples in SPO order.
    ///
    /// For DeltaColumnar storage, builds a `DeltaScanState` that uses three
    /// `DeltaColIter`s so each entry costs O(1) amortised instead of O(BLOCK_SIZE).
    pub fn scan(&self, pat: &TriplePattern) -> TripleScan {
        let raw_pat = pattern_to_raw(*pat, self.kind);
        let (start, end) = self.range_for_pattern(&raw_pat);
        let delta = if let IndexStorage::DeltaColumnar { cols } = &self.storage {
            let remaining = end.saturating_sub(start);
            Some(DeltaScanState {
                iter0: cols[0].iter_from(start),
                iter1: cols[1].iter_from(start),
                iter2: cols[2].iter_from(start),
                remaining,
            })
        } else {
            None
        };
        TripleScan { index: self, raw_pat, pos: start, end, delta }
    }

    /// Compute the [start, end) range in the index that covers `raw`.
    ///
    /// With columnar storage the range-end scan reads only col0 or col01,
    /// which keeps the tertiary-key pages cold until triples are emitted.
    ///
    /// For the POS index with k0 = predicate and k1 = None (full predicate
    /// scan), the predicate secondary index gives the answer in O(1) with a
    /// single HashMap lookup — no binary search, no c0 page fault.
    fn range_for_pattern(&self, raw: &[Option<u64>; 3]) -> (usize, usize) {
        match (raw[0], raw[1]) {
            (Some(k0), Some(k1)) => {
                // Determine the [lo, hi) range within which k0 entries live.
                //
                // Priority 1: pred_idx gives exact O(1) range for POS and PSO
                //   (predicate is always the primary key for those indexes).
                // Priority 2: SkipIndex.upper_hint gives a tight upper bound for
                //   the k0 range, bounding the secondary binary search to
                //   O(log degree(k0)) instead of O(log total_count).
                //   Example: for SPO with s=X, the search is bounded to the
                //   number of triples with subject X, not the entire index.
                let (k0_lo, k0_hi) = if let Some(pi) = &self.pred_idx {
                    // O(1) exact range (POS / PSO indexes).
                    pi.get(k0).unwrap_or((0, 0))
                } else if let Some(ref skip) = self.skip {
                    // SkipIndex tight bound: lower_bound uses narrow() for 1-page fault;
                    // upper_hint() narrows from self.count down to degree(k0) entries.
                    let lo = self.lower_bound_0(k0);
                    let hi = skip.upper_hint(k0);
                    (lo, hi)
                } else {
                    // Legacy interleaved: no skip index, fall back to full range.
                    (self.lower_bound_0(k0), self.count)
                };
                // Binary search within the tight k0 range for the first (k0, k1) entry.
                let (mut a, mut b) = (k0_lo, k0_hi);
                while a < b {
                    let mid = a + (b - a) / 2;
                    let (c0, c1) = self.get_col01(mid);
                    if (c0, c1) < (k0, k1) { a = mid + 1; } else { b = mid; }
                }
                let start = a;
                // Binary search for exclusive upper bound: first pos where (c0,c1) > (k0,k1).
                // Reduces O(degree(k0,k1)) sequential scan to O(log(degree(k0,k1))).
                let (mut a, mut b) = (start, k0_hi);
                while a < b {
                    let mid = a + (b - a) / 2;
                    let (c0, c1) = self.get_col01(mid);
                    if (c0, c1) <= (k0, k1) { a = mid + 1; } else { b = mid; }
                }
                (start, a)
            }
            (Some(k0), None) => {
                // Fast path: predicate secondary index gives exact range in O(1).
                if let Some(pi) = &self.pred_idx {
                    return pi.get(k0).unwrap_or((0, 0));
                }
                // Fallback: binary search + forward scan (legacy / SPO / OSP).
                let start = self.lower_bound_0(k0);
                let mut end = start;
                while end < self.count {
                    if self.get_col0(end) != k0 { break; }
                    end += 1;
                }
                (start, end)
            }
            _ => (0, self.count), // Full scan
        }
    }

    /// Estimate cardinality for a triple pattern (used by the query optimizer).
    pub fn estimate_cardinality(&self, pat: &TriplePattern) -> u64 {
        let raw_pat = pattern_to_raw(*pat, self.kind);
        let (start, end) = self.range_for_pattern(&raw_pat);
        if raw_pat[2].is_some() {
            // Assume ~50% selectivity for the unindexed tertiary component.
            ((end - start) as u64).saturating_add(1) / 2
        } else {
            (end - start) as u64
        }
    }

    // ── Leapfrog Triejoin support ─────────────────────────────────────────────

    /// Seek to the first position where `col0 >= target`.
    /// Returns `(pos, col0_value)`, or `(count, u64::MAX)` when exhausted.
    ///
    /// ## Search strategy
    ///
    /// **With skip index** (columnar format): `narrow(target)` narrows the
    /// range to ≤ `SKIP_STRIDE` contiguous entries in one cache-resident step,
    /// then binary-searches that 32 KB slice.  Handles both small advances
    /// (same skip block — pages already warm) and large jumps (different block —
    /// touches only one contiguous 32 KB region instead of ~31 random pages).
    ///
    /// **Without skip index** (interleaved legacy): galloping search —
    /// O(log k) where k is the distance from `from` to the result.
    pub fn seek_0(&self, from: usize, target: u64) -> (usize, u64) {
        if from >= self.count {
            return (self.count, u64::MAX);
        }
        let cur = self.get_col0(from);
        if cur >= target {
            return (from, cur);
        }

        if let Some(ref s) = self.skip {
            // ── Skip-index path ───────────────────────────────────────────────
            let (slo, shi) = s.narrow(target);
            // Fire a non-blocking prefetch on the target c0 page so the kernel
            // can pipeline the disk read while we finish the anchor search.
            self.prefetch_c0(slo, shi);
            // Never regress below `from + 1` (current position is already < target).
            let (mut a, mut b) = (slo.max(from + 1), shi);
            while a < b {
                let mid = a + (b - a) / 2;
                if self.get_col0(mid) < target { a = mid + 1; } else { b = mid; }
            }
            return if a < self.count {
                (a, self.get_col0(a))
            } else {
                (self.count, u64::MAX)
            };
        }

        // ── Galloping path (legacy interleaved, no skip) ──────────────────────
        let mut lo = from;
        let mut step = 1usize;
        loop {
            let probe = lo + step;
            if probe >= self.count || self.get_col0(probe) >= target {
                let hi = probe.min(self.count);
                let (mut a, mut b) = (lo + 1, hi);
                while a < b {
                    let mid = a + (b - a) / 2;
                    if self.get_col0(mid) < target { a = mid + 1; } else { b = mid; }
                }
                return if a < self.count {
                    (a, self.get_col0(a))
                } else {
                    (self.count, u64::MAX)
                };
            }
            lo = probe;
            step = step.saturating_mul(2);
        }
    }
}

// ── Iterator over matching triples ───────────────────────────────────────────

/// Internal state for delta-columnar scans.
///
/// Holds three `DeltaColIter`s that advance in lockstep, decompressing one
/// 256-entry block per iterator step instead of one entry per step.
///
/// Without this, `TripleScan::next` would call `DeltaColFile::get(pos)` for
/// each position which decompresses the entire block (256 entries) and throws
/// away 255 — making sequential scans O(N × BLOCK_SIZE) instead of O(N).
struct DeltaScanState<'a> {
    iter0: crate::col_delta::DeltaColIter<'a>,
    iter1: crate::col_delta::DeltaColIter<'a>,
    iter2: crate::col_delta::DeltaColIter<'a>,
    remaining: usize,
}

pub struct TripleScan<'a> {
    index:   &'a IndexFile,
    raw_pat: [Option<u64>; 3],
    pos:     usize,
    end:     usize,
    /// Set for DeltaColumnar storage; `None` for Columnar/Interleaved.
    delta:   Option<DeltaScanState<'a>>,
}

impl<'a> Iterator for TripleScan<'a> {
    type Item = Triple;

    fn next(&mut self) -> Option<Triple> {
        // ── Delta-columnar path: use DeltaColIter for O(N) amortised cost ──────
        if let Some(ref mut ds) = self.delta {
            while ds.remaining > 0 {
                let v0 = ds.iter0.next()?;
                let v1 = ds.iter1.next()?;
                let v2 = ds.iter2.next()?;
                ds.remaining -= 1;
                if let Some(k2) = self.raw_pat[2] {
                    if v2 != k2 { continue; }
                }
                return Some(self.index.to_triple([v0, v1, v2]));
            }
            return None;
        }

        // ── Columnar / interleaved path (unchanged) ───────────────────────────
        while self.pos < self.end {
            let raw = self.index.get_raw(self.pos);
            self.pos += 1;
            if let Some(k2) = self.raw_pat[2] {
                if raw[2] != k2 { continue; }
            }
            return Some(self.index.to_triple(raw));
        }
        None
    }
}

// ── Reorder helpers ───────────────────────────────────────────────────────────

/// Reorder SPO triple into the index's natural sort key.
///
/// ```text
///   SPO → [s, p, o]    PSO → [p, s, o]
///   POS → [p, o, s]    SOP → [s, o, p]
///   OSP → [o, s, p]    OPS → [o, p, s]
/// ```
#[inline]
fn reorder(t: Triple, kind: IndexKind) -> [u64; 3] {
    match kind {
        IndexKind::Spo => [t.s, t.p, t.o],
        IndexKind::Pos => [t.p, t.o, t.s],
        IndexKind::Osp => [t.o, t.s, t.p],
        IndexKind::Pso => [t.p, t.s, t.o],
        IndexKind::Sop => [t.s, t.o, t.p],
        IndexKind::Ops => [t.o, t.p, t.s],
    }
}

/// Convert index-ordered raw triple back to SPO.
///
/// Inverse of `reorder`: given raw = [c0, c1, c2] in the index's sort order,
/// recover (s, p, o).
#[inline]
fn reorder_back(raw: [u64; 3], kind: IndexKind) -> Triple {
    match kind {
        // SPO: raw=[s,p,o]         → s=raw[0], p=raw[1], o=raw[2]
        IndexKind::Spo => Triple::new(raw[0], raw[1], raw[2]),
        // POS: raw=[p,o,s]         → s=raw[2], p=raw[0], o=raw[1]
        IndexKind::Pos => Triple::new(raw[2], raw[0], raw[1]),
        // OSP: raw=[o,s,p]         → s=raw[1], p=raw[2], o=raw[0]
        IndexKind::Osp => Triple::new(raw[1], raw[2], raw[0]),
        // PSO: raw=[p,s,o]         → s=raw[1], p=raw[0], o=raw[2]
        IndexKind::Pso => Triple::new(raw[1], raw[0], raw[2]),
        // SOP: raw=[s,o,p]         → s=raw[0], p=raw[2], o=raw[1]
        IndexKind::Sop => Triple::new(raw[0], raw[2], raw[1]),
        // OPS: raw=[o,p,s]         → s=raw[2], p=raw[1], o=raw[0]
        IndexKind::Ops => Triple::new(raw[2], raw[1], raw[0]),
    }
}

/// Convert a TriplePattern to the index-reordered form for scanning.
fn pattern_to_raw(pat: TriplePattern, kind: IndexKind) -> [Option<TermId>; 3] {
    let b = |v: TermId| if v == UNBOUND { None } else { Some(v) };
    match kind {
        IndexKind::Spo => [b(pat.s), b(pat.p), b(pat.o)],
        IndexKind::Pos => [b(pat.p), b(pat.o), b(pat.s)],
        IndexKind::Osp => [b(pat.o), b(pat.s), b(pat.p)],
        IndexKind::Pso => [b(pat.p), b(pat.s), b(pat.o)],
        IndexKind::Sop => [b(pat.s), b(pat.o), b(pat.p)],
        IndexKind::Ops => [b(pat.o), b(pat.p), b(pat.s)],
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// GSPO quad index — sorted by (graph, subject, predicate, object)
// Used for GRAPH-scoped queries in SPARQL 1.1.
//
// Binary format:
//   offset 0:  magic  [u8; 8]  = b"ECOG0001"
//   offset 8:  count  u64      = number of quads
//   offset 16: data   [u32; count*4]  — (g,s,p,o) in sorted order
// ══════════════════════════════════════════════════════════════════════════════

/// Build-phase GSPO quad index.
///
/// Supports the same external-sort pattern as [`IndexBuilder`]: when
/// `chunk_size > 0` quads are flushed to sorted temp files and k-way
/// merged on `build()`.
pub struct GspoBuilder {
    quads: Vec<[u64; 4]>, // (g, s, p, o)
    chunk_size: usize,
    chunk_dir: Option<PathBuf>,
    chunks: Vec<PathBuf>,
}

impl GspoBuilder {
    pub fn new() -> Self {
        Self { quads: Vec::new(), chunk_size: 0, chunk_dir: None, chunks: Vec::new() }
    }

    fn new_streaming(chunk_dir: PathBuf, chunk_size: usize) -> Self {
        let cap = chunk_size.min(4_000_000);
        Self {
            quads: Vec::with_capacity(cap),
            chunk_size,
            chunk_dir: Some(chunk_dir),
            chunks: Vec::new(),
        }
    }

    pub fn push(&mut self, q: Quad) -> io::Result<()> {
        self.quads.push([q.g, q.s, q.p, q.o]);
        if self.chunk_size > 0 && self.quads.len() >= self.chunk_size {
            self.flush_chunk()?;
        }
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.quads.is_empty() && self.chunks.is_empty()
    }

    fn flush_chunk(&mut self) -> io::Result<()> {
        if self.quads.is_empty() { return Ok(()); }
        let chunk_dir = self.chunk_dir.as_ref().expect("chunk_dir must be set");
        let chunk_path = chunk_dir.join(format!("gspo_chunk_{:06}.tmp", self.chunks.len()));
        self.quads.sort_unstable();
        write_quad_chunk(&self.quads, &chunk_path)?;
        self.chunks.push(chunk_path);
        self.quads.clear();
        Ok(())
    }

    /// Flush remaining buffer and return all chunk file paths without merging.
    pub(crate) fn flush_and_return_chunks(mut self) -> io::Result<Vec<PathBuf>> {
        self.flush_chunk()?;
        Ok(self.chunks)
    }

    /// Sort and write to disk, returning a read-only GspoIndexFile.
    pub fn build(mut self, path: &Path) -> io::Result<GspoIndexFile> {
        if self.chunks.is_empty() {
            self.quads.sort_unstable();
            write_gspo_index_from_sorted(&self.quads, path)?;
        } else {
            self.flush_chunk()?;
            eprintln!("  Merging {} sorted chunks → GSPO index…", self.chunks.len());
            merge_quad_chunks(&self.chunks, path)?;
            for chunk in &self.chunks {
                let _ = std::fs::remove_file(chunk);
            }
        }
        GspoIndexFile::open(path)
    }
}

impl Default for GspoBuilder {
    fn default() -> Self { Self::new() }
}

// ── Quad chunk helpers ────────────────────────────────────────────────────────

fn write_quad_chunk(quads: &[[u64; 4]], path: &Path) -> io::Result<()> {
    let file = File::create(path)?;
    let mut w = BufWriter::with_capacity(4 * 1024 * 1024, file);
    w.write_all(&(quads.len() as u64).to_le_bytes())?;
    for q in quads {
        for v in q { w.write_all(&v.to_le_bytes())?; }
    }
    w.flush()
}

fn write_gspo_index_from_sorted(quads: &[[u64; 4]], path: &Path) -> io::Result<()> {
    let file = File::create(path)?;
    let mut w = BufWriter::with_capacity(4 * 1024 * 1024, file);
    w.write_all(GSPO_MAGIC)?;
    w.write_all(&(quads.len() as u64).to_le_bytes())?;
    for q in quads {
        for v in q { w.write_all(&v.to_le_bytes())?; }
    }
    w.flush()
}

/// k-way merge of sorted quad chunk files into a single GSPO index file.
///
/// Hierarchical merge is applied automatically when `chunks.len() > MAX_FAN_IN`.
pub(crate) fn merge_quad_chunks(chunks: &[PathBuf], path: &Path) -> io::Result<()> {
    if chunks.len() <= MAX_FAN_IN {
        return merge_quad_chunks_direct(chunks, path);
    }

    let mut intermediates: Vec<PathBuf> = Vec::new();
    for (i, batch) in chunks.chunks(MAX_FAN_IN).enumerate() {
        let tmp = append_path_suffix(path, &format!(".__merge_{:04}.tmp", i));
        merge_quad_chunks_to_chunk(batch, &tmp)?;
        intermediates.push(tmp);
    }

    let result = if intermediates.len() <= MAX_FAN_IN {
        merge_quad_chunks_direct(&intermediates, path)
    } else {
        merge_quad_chunks(&intermediates, path)
    };

    for p in &intermediates {
        let _ = std::fs::remove_file(p);
    }

    result
}

fn merge_quad_chunks_direct(chunks: &[PathBuf], path: &Path) -> io::Result<()> {
    let mut readers: Vec<QuadChunkReader> = chunks.iter()
        .map(|p| QuadChunkReader::open(p))
        .collect::<io::Result<Vec<_>>>()?;

    let out_file = OpenOptions::new().create(true).write(true).truncate(true).open(path)?;
    let mut w = BufWriter::with_capacity(8 * 1024 * 1024, out_file);
    w.write_all(GSPO_MAGIC)?;
    w.write_all(&0u64.to_le_bytes())?; // placeholder

    let mut heap: BinaryHeap<Reverse<([u64; 4], usize)>> = BinaryHeap::new();
    for (i, reader) in readers.iter_mut().enumerate() {
        if let Some(q) = reader.next()? {
            heap.push(Reverse((q, i)));
        }
    }

    let mut count = 0u64;
    let mut prev: Option<[u64; 4]> = None;
    while let Some(Reverse((q, i))) = heap.pop() {
        if Some(q) != prev {
            for v in &q { w.write_all(&v.to_le_bytes())?; }
            count += 1;
            prev = Some(q);
        }
        if let Some(next) = readers[i].next()? {
            heap.push(Reverse((next, i)));
        }
    }
    w.flush()?;

    drop(w);
    let mut f = OpenOptions::new().write(true).open(path)?;
    f.seek(SeekFrom::Start(8))?;
    f.write_all(&count.to_le_bytes())?;

    Ok(())
}

/// Merge up to `MAX_FAN_IN` quad chunk files into a new chunk file at `path`.
///
/// Output: `[count: u64][quad data…]` (no GSPO magic header).
fn merge_quad_chunks_to_chunk(chunks: &[PathBuf], path: &Path) -> io::Result<()> {
    let mut readers: Vec<QuadChunkReader> = chunks.iter()
        .map(|p| QuadChunkReader::open(p))
        .collect::<io::Result<Vec<_>>>()?;

    let out_file = OpenOptions::new().create(true).write(true).truncate(true).open(path)?;
    let mut w = BufWriter::with_capacity(8 * 1024 * 1024, out_file);
    w.write_all(&0u64.to_le_bytes())?; // count placeholder

    let mut heap: BinaryHeap<Reverse<([u64; 4], usize)>> = BinaryHeap::new();
    for (i, reader) in readers.iter_mut().enumerate() {
        if let Some(q) = reader.next()? {
            heap.push(Reverse((q, i)));
        }
    }

    let mut count = 0u64;
    let mut prev: Option<[u64; 4]> = None;
    while let Some(Reverse((q, i))) = heap.pop() {
        if Some(q) != prev {
            for v in &q { w.write_all(&v.to_le_bytes())?; }
            count += 1;
            prev = Some(q);
        }
        if let Some(next) = readers[i].next()? {
            heap.push(Reverse((next, i)));
        }
    }
    w.flush()?;
    drop(w);

    let mut f = OpenOptions::new().write(true).open(path)?;
    f.seek(SeekFrom::Start(0))?;
    f.write_all(&count.to_le_bytes())?;

    Ok(())
}

struct QuadChunkReader {
    reader: BufReader<File>,
    remaining: u64,
}

impl QuadChunkReader {
    fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let mut reader = BufReader::with_capacity(4 * 1024 * 1024, file);
        let mut count_buf = [0u8; 8];
        reader.read_exact(&mut count_buf)?;
        Ok(Self { reader, remaining: u64::from_le_bytes(count_buf) })
    }

    fn next(&mut self) -> io::Result<Option<[u64; 4]>> {
        if self.remaining == 0 { return Ok(None); }
        let mut buf = [0u8; 32];
        self.reader.read_exact(&mut buf)?;
        self.remaining -= 1;
        Ok(Some([
            u64::from_le_bytes(buf[0..8].try_into().unwrap()),
            u64::from_le_bytes(buf[8..16].try_into().unwrap()),
            u64::from_le_bytes(buf[16..24].try_into().unwrap()),
            u64::from_le_bytes(buf[24..32].try_into().unwrap()),
        ]))
    }
}

/// Read-only mmap-backed GSPO quad index.
pub struct GspoIndexFile {
    _file: File,
    mmap: Mmap,
    count: usize,
}

impl GspoIndexFile {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).open(path)?;
        let mmap = unsafe { Mmap::map(&file)? };

        if mmap.len() < HEADER_SIZE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "gspo index too small"));
        }
        if &mmap[0..8] != GSPO_MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad gspo magic"));
        }
        let count = u64::from_le_bytes(mmap[8..16].try_into().unwrap()) as usize;
        let expected = HEADER_SIZE + count * QUAD_BYTES;
        if mmap.len() != expected {
            return Err(io::Error::new(io::ErrorKind::InvalidData,
                format!("gspo size mismatch: {} vs {}", mmap.len(), expected)));
        }
        Ok(Self { _file: file, mmap, count })
    }

    pub fn quad_count(&self) -> usize { self.count }

    #[inline]
    fn get_raw(&self, pos: usize) -> [u64; 4] {
        debug_assert!(pos < self.count);
        let off = HEADER_SIZE + pos * QUAD_BYTES;
        [
            u64::from_le_bytes(self.mmap[off..off+8].try_into().unwrap()),
            u64::from_le_bytes(self.mmap[off+8..off+16].try_into().unwrap()),
            u64::from_le_bytes(self.mmap[off+16..off+24].try_into().unwrap()),
            u64::from_le_bytes(self.mmap[off+24..off+32].try_into().unwrap()),
        ]
    }

    /// Binary search: first position where raw[0] >= key.
    fn lower_bound_g(&self, g: u64) -> usize {
        let (mut lo, mut hi) = (0, self.count);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.get_raw(mid)[0] < g { lo = mid + 1; } else { hi = mid; }
        }
        lo
    }

    /// Binary search: first position after all entries with raw[0] == g.
    fn upper_bound_g(&self, g: u64) -> usize {
        let (mut lo, mut hi) = (0, self.count);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.get_raw(mid)[0] <= g { lo = mid + 1; } else { hi = mid; }
        }
        lo
    }

    /// All distinct graph IDs in the index.
    pub fn graphs(&self) -> Vec<TermId> {
        let mut result = Vec::new();
        let mut pos = 0;
        while pos < self.count {
            let g = self.get_raw(pos)[0];
            result.push(g);
            while pos < self.count && self.get_raw(pos)[0] == g {
                pos += 1;
            }
        }
        result
    }

    /// Scan triples in graph `g_id` that match the triple pattern.
    pub fn scan_graph<'a>(&'a self, g_id: TermId, pat: &TriplePattern) -> GspoScan<'a> {
        let start = self.lower_bound_g(g_id);
        let end   = self.upper_bound_g(g_id);
        GspoScan { index: self, pat: *pat, pos: start, end }
    }
}

/// Iterator over quads in a single graph matching a triple pattern.
pub struct GspoScan<'a> {
    index: &'a GspoIndexFile,
    pat: TriplePattern,
    pos: usize,
    end: usize,
}

impl<'a> Iterator for GspoScan<'a> {
    type Item = Triple;

    fn next(&mut self) -> Option<Triple> {
        while self.pos < self.end {
            let raw = self.index.get_raw(self.pos);
            self.pos += 1;
            // raw = [g, s, p, o] — g is already fixed by the scan range
            let t = Triple::new(raw[1], raw[2], raw[3]);
            if self.pat.matches(&t) {
                return Some(t);
            }
        }
        None
    }
}

// ── Three-index set + optional GSPO ──────────────────────────────────────────

/// Holds all triple indexes.
///
/// Always present: SPO, POS, OSP (3-index build).
/// Optional (6-index build): PSO, SOP, OPS — `None` for stores built before
/// 6-index support was added.  Queries fall back to the closest 3-index when
/// the extra indexes are absent.
pub struct TripleIndex {
    pub spo: IndexFile,
    pub pos: IndexFile,
    pub osp: IndexFile,
    /// PSO: sorted (P,S,O) — pred_idx for O(1) predicate range, binary search for S.
    /// Present only in 6-index stores.
    pub pso: Option<IndexFile>,
    /// SOP: sorted (S,O,P) — SkipIndex for S, binary search for O within S's range.
    /// Enables efficient (s=bound, o=bound) patterns. Present only in 6-index stores.
    pub sop: Option<IndexFile>,
    /// OPS: sorted (O,P,S) — SkipIndex for O, binary search for P within O's range.
    /// Enables efficient (o=bound, p=bound) patterns. Present only in 6-index stores.
    pub ops: Option<IndexFile>,
    /// Present only when named-graph data was loaded (.nq files).
    pub gspo: Option<GspoIndexFile>,
}

/// Try to open an optional index file (PSO / SOP / OPS).
/// Returns `None` if neither the columnar nor legacy file exists.
fn try_open_index(path: &Path, kind: IndexKind) -> io::Result<Option<IndexFile>> {
    let cpaths = col_paths(path);
    if cpaths[0].exists() || path.exists() {
        Ok(Some(IndexFile::open(path, kind)?))
    } else {
        Ok(None)
    }
}

/// Denominator for computing the OPS routing threshold relative to dataset size.
///
/// The actual threshold is `total_triples / OPS_ROUTING_DIVISOR`, clamped to
/// `[OPS_ROUTING_MIN, OPS_ROUTING_MAX]`.
///
/// ## Why relative, not fixed
///
/// OPS wins over POS for `(p+o bound)` when `|pred| >> deg_o`, where `deg_o` is
/// the object's degree in the OPS index (how many predicates point to it).
/// Both `|pred|` and `deg_o` scale proportionally with dataset size, so the
/// crossover point is naturally expressed as a fraction of total triples.
///
/// **Example: rdf:type across dataset sizes**
///
/// | Dataset size | rdf:type | threshold (÷1000) | POS cost        | OPS cost      |
/// |-------------|----------|-------------------|-----------------|---------------|
/// |  545 M triples | 500 M    |  545 K            | log₂(500M) = 29 | log₂(~10K) ≈ 13 |
/// |    5 B triples |   5 B    |    5 M            | log₂(5B)   = 32 | log₂(~100K) ≈ 17 |
/// |   50 B triples |  50 B    |   50 M            | log₂(50B)  = 36 | log₂(~1M)  ≈ 20 |
///
/// OPS consistently wins for hub predicates regardless of scale.
///
/// **Example: selective predicate (jpo:hasPeptide, 263 triples)**
///
/// | Dataset size | threshold | POS cost         | OPS cost        |
/// |-------------|-----------|------------------|-----------------|
/// |  545 M      |  545 K    | log₂(263) ≈ 8   | 1 + log₂(~1) ≈ 1 |
///
/// → POS wins (threshold 545K >> 263 → stays in POS).  Correct.
///
/// **Semantics of the divisor = 1000:**
/// A predicate is routed to OPS if it carries > 0.1 % of all triples.
/// In biological RDF graphs, hub predicates (rdf:type, rdfs:label, …) typically
/// account for 5–30 % of triples; relationship predicates account for ≪ 0.1 %.
const OPS_ROUTING_DIVISOR: usize = 1_000;
const OPS_ROUTING_MIN:     usize = 10_000;      // never route tiny predicates to OPS
const OPS_ROUTING_MAX:     usize = 50_000_000;  // cap so huge stores don't set threshold too high

impl TripleIndex {
    pub fn open(dir: &Path) -> io::Result<Self> {
        let gspo_path = dir.join("gspo.bin");
        let gspo = if gspo_path.exists() {
            Some(GspoIndexFile::open(&gspo_path)?)
        } else {
            None
        };
        Ok(Self {
            spo: IndexFile::open(&dir.join("spo.bin"), IndexKind::Spo)?,
            pos: IndexFile::open(&dir.join("pos.bin"), IndexKind::Pos)?,
            osp: IndexFile::open(&dir.join("osp.bin"), IndexKind::Osp)?,
            pso: try_open_index(&dir.join("pso.bin"), IndexKind::Pso)?,
            sop: try_open_index(&dir.join("sop.bin"), IndexKind::Sop)?,
            ops: try_open_index(&dir.join("ops.bin"), IndexKind::Ops)?,
            gspo,
        })
    }

    /// Choose the optimal `IndexKind` for `pat`, incorporating predicate-size
    /// statistics that `TriplePattern::best_index()` cannot access.
    ///
    /// ## Statistics-aware rule: (p+o bound) → OPS
    ///
    /// `TriplePattern::best_index()` always returns `Pos` for `(s=free, p=bound,
    /// o=bound)`.  This is correct for small predicates, but wrong for large ones:
    ///
    /// - **POS** binary-searches for `o` within the predicate's range →
    ///   O(log |pred|) page faults, e.g. log₂(500 M) ≈ 29 for `rdf:type`.
    /// - **OPS** starts from `o` (SkipIndex: 1 page fault) then binary-searches
    ///   for `p` within the object's degree → O(log deg(o)), typically ≪ 29.
    ///
    /// We switch to OPS when the predicate's row count exceeds
    /// `OPS_ROUTING_THRESHOLD` (1 M) and the OPS index is present.
    ///
    /// All other patterns delegate to `TriplePattern::best_index()`.
    pub fn best_kind(&self, pat: &TriplePattern) -> IndexKind {
        // (p+o bound, s=free): candidate for OPS routing.
        if pat.s == UNBOUND && pat.p != UNBOUND && pat.o != UNBOUND
            && self.ops.is_some()
        {
            // Relative threshold: OPS_ROUTING_DIVISOR-th fraction of total triples.
            // Scales automatically with dataset size — 10× more data → 10× higher
            // threshold — so the same fraction of "hub predicates" is routed to OPS
            // regardless of how many triples the store contains.
            let total = self.triple_count();
            let threshold = (total / OPS_ROUTING_DIVISOR)
                .max(OPS_ROUTING_MIN)
                .min(OPS_ROUTING_MAX);

            let pred_size = self.pos.pred_idx.as_ref()
                .and_then(|pi| pi.get(pat.p))
                .map(|(lo, hi)| hi - lo)
                .unwrap_or(0);

            if pred_size > threshold {
                tracing::trace!(
                    pred = pat.p,
                    pred_size,
                    threshold,
                    total_triples = total,
                    "index routing: (p+o bound) → OPS (large predicate)"
                );
                return IndexKind::Ops;
            }
        }
        pat.best_index()
    }

    /// Select the best index for a pattern and return matching triples.
    ///
    /// Uses `best_kind()` (statistics-aware) rather than `pat.best_index()`
    /// (statistics-free), so hub predicates in `(p+o)` patterns are routed
    /// to OPS instead of POS based on a relative threshold that scales with
    /// dataset size.
    ///
    /// When the optimal 6-index is absent (3-index store), falls back to the
    /// nearest existing index:
    ///   SOP missing → SPO   (s is primary key in both)
    ///   PSO missing → POS   (p is primary key in both)
    ///   OPS missing → OSP   (o is primary key in both)
    pub fn scan(&self, pat: &TriplePattern) -> TripleScan {
        match self.best_kind(pat) {
            IndexKind::Sop => self.sop.as_ref().map(|i| i.scan(pat))
                .unwrap_or_else(|| self.spo.scan(pat)),
            IndexKind::Pso => self.pso.as_ref().map(|i| i.scan(pat))
                .unwrap_or_else(|| self.pos.scan(pat)),
            IndexKind::Ops => self.ops.as_ref().map(|i| i.scan(pat))
                .unwrap_or_else(|| self.osp.scan(pat)),
            IndexKind::Spo => self.spo.scan(pat),
            IndexKind::Pos => self.pos.scan(pat),
            IndexKind::Osp => self.osp.scan(pat),
        }
    }

    /// Estimate cardinality using the statistics-aware best index.
    pub fn estimate(&self, pat: &TriplePattern) -> u64 {
        match self.best_kind(pat) {
            IndexKind::Sop => self.sop.as_ref().map(|i| i.estimate_cardinality(pat))
                .unwrap_or_else(|| self.spo.estimate_cardinality(pat)),
            IndexKind::Pso => self.pso.as_ref().map(|i| i.estimate_cardinality(pat))
                .unwrap_or_else(|| self.pos.estimate_cardinality(pat)),
            IndexKind::Ops => self.ops.as_ref().map(|i| i.estimate_cardinality(pat))
                .unwrap_or_else(|| self.osp.estimate_cardinality(pat)),
            IndexKind::Spo => self.spo.estimate_cardinality(pat),
            IndexKind::Pos => self.pos.estimate_cardinality(pat),
            IndexKind::Osp => self.osp.estimate_cardinality(pat),
        }
    }

    /// Scan all triples in natural SPO order.
    ///
    /// Used by `StoreStatistics::build_from_index` (pass 2) to count distinct
    /// `(subject, predicate)` pairs per predicate.
    pub fn spo_scan_all(&self) -> TripleScan {
        let pat = TriplePattern { s: UNBOUND, p: UNBOUND, o: UNBOUND };
        self.spo.scan(&pat)
    }

    /// Scan all triples in natural POS order (P, O, S).
    ///
    /// Used by `StoreStatistics::build_from_index` (pass 1) to count triples
    /// and distinct objects per predicate without allocating a hash set.
    pub fn pos_scan_all(&self) -> TripleScan {
        let pat = TriplePattern { s: UNBOUND, p: UNBOUND, o: UNBOUND };
        self.pos.scan(&pat)
    }

    /// Fire a non-blocking prefetch hint for the pages that `scan(pat)` will
    /// read, using only the in-RAM skip index (no disk I/O).
    ///
    /// Fire hints for many patterns in a tight loop before doing any reads so
    /// the OS can pipeline the physical I/Os in parallel:
    ///
    /// ```text
    /// // Phase 1: submit all hints (pure CPU + madvise syscalls, ~1 µs each)
    /// for pat in patterns { index.prefetch_pattern(pat); }
    ///
    /// // Phase 2: read data — pages are warm (OS loaded them concurrently)
    /// for pat in patterns { index.scan(pat).collect::<Vec<_>>(); }
    /// ```
    pub fn prefetch_pattern(&self, pat: &TriplePattern) {
        // Use best_kind() so large (p+o)-bound patterns prefetch OPS pages,
        // not POS pages — matching the index that scan() will actually use.
        let (idx_opt, k0) = match self.best_kind(pat) {
            IndexKind::Spo => (Some(&self.spo), if pat.s != UNBOUND { Some(pat.s) } else { None }),
            IndexKind::Pos => (Some(&self.pos), if pat.p != UNBOUND { Some(pat.p) } else { None }),
            IndexKind::Osp => (Some(&self.osp), if pat.o != UNBOUND { Some(pat.o) } else { None }),
            IndexKind::Pso => (self.pso.as_ref().or(Some(&self.pos)),
                               if pat.p != UNBOUND { Some(pat.p) } else { None }),
            IndexKind::Sop => (self.sop.as_ref().or(Some(&self.spo)),
                               if pat.s != UNBOUND { Some(pat.s) } else { None }),
            IndexKind::Ops => (self.ops.as_ref().or(Some(&self.osp)),
                               if pat.o != UNBOUND { Some(pat.o) } else { None }),
        };
        if let (Some(idx), Some(k)) = (idx_opt, k0) {
            idx.prefetch_for_key(k);
        }
    }

    pub fn triple_count(&self) -> usize { self.spo.len() }

    /// Number of named graphs (0 if no GSPO index).
    pub fn graph_count(&self) -> usize {
        self.gspo.as_ref().map(|g| g.graphs().len()).unwrap_or(0)
    }

    /// Look up the exact `[lo, hi)` range for a predicate in the POS index.
    ///
    /// Returns `Some((lo, hi))` when the predicate secondary index is available
    /// (always the case for columnar-format stores built or opened with this version).
    /// Returns `None` when the predicate is absent or the index is not loaded.
    ///
    /// This is a pure in-RAM operation — no disk I/O, no binary search.
    pub fn pos_predicate_range(&self, pred: TermId) -> Option<(usize, usize)> {
        self.pos.pred_idx.as_ref()?.get(pred)
    }

    /// Returns true when the PSO index is available.
    pub fn has_pso(&self) -> bool {
        self.pso.is_some()
    }

    /// Scan PSO for `pred` and collect up to `subject_limit` unique subjects
    /// with their objects.
    ///
    /// ## Why this beats POS + full-scan for LIMIT queries
    ///
    /// POS is `(P, O, S)` — subjects for a given predicate are scattered
    /// across the sort.  Collecting K unique subjects requires reading O(|pred|)
    /// entries even when K ≪ |pred|.
    ///
    /// PSO is `(P, S, O)` — subjects are contiguous within the predicate range.
    /// Collecting the first K unique subjects requires reading only
    /// O(K × avg_objects_per_subject) entries.
    ///
    /// Example: `dct:identifier` has 10 M entries (1 per PSM).
    ///   POS scan:           reads all 10 M entries → 15 s
    ///   PSO early-exit:     reads 1 000 entries    → < 1 ms
    ///
    /// ## Subject filter
    ///
    /// `subject_filter`: when non-empty, only subjects present in this set are
    /// collected.  When empty, all subjects in the predicate range are accepted.
    ///
    /// ## Return value
    ///
    /// `(s_to_objects, exhausted)` where:
    ///   - `s_to_objects`: subject → objects mapping (sorted by object)
    ///   - `exhausted`: true when the predicate range was fully scanned
    ///     (subject_limit was not reached; the result is complete)
    pub fn scan_pso_subject_limit(
        &self,
        pred: TermId,
        subject_limit: usize,
        subject_filter: &HashSet<TermId>,
    ) -> (HashMap<TermId, Vec<TermId>>, bool) {
        let Some(pso) = &self.pso else {
            return (HashMap::new(), false);
        };

        // PSO is (P,S,O); pattern with P bound and S,O free.
        let pat = TriplePattern { s: UNBOUND, p: pred, o: UNBOUND };
        // The raw pattern for PSO reorders to [P, S, O].
        // Use scan() which calls range_for_pattern → pred_idx gives O(1) range.
        let mut s_to_objects: HashMap<TermId, Vec<TermId>> = HashMap::new();
        let mut unique_subjects = 0usize;

        for triple in pso.scan(&pat) {
            let s = triple.s;
            if !subject_filter.is_empty() && !subject_filter.contains(&s) {
                continue;
            }
            let is_new = !s_to_objects.contains_key(&s);
            s_to_objects.entry(s).or_default().push(triple.o);
            if is_new {
                unique_subjects += 1;
                if unique_subjects >= subject_limit {
                    return (s_to_objects, false); // limit reached, not exhausted
                }
            }
        }

        (s_to_objects, true) // fully exhausted
    }

    /// Number of distinct predicates in the store (from the predicate index).
    pub fn predicate_count(&self) -> usize {
        self.pos.pred_idx.as_ref()
            .map(|pi| pi.predicate_count())
            .unwrap_or(0)
    }

    /// Return all (predicate_id, cardinality) pairs, sorted by cardinality ascending.
    /// Used by PredCache at startup to decide which predicates to cache.
    pub fn predicate_sizes(&self) -> Vec<(TermId, usize)> {
        let Some(pi) = &self.pos.pred_idx else { return Vec::new(); };
        let mut v: Vec<(TermId, usize)> = pi.ranges.iter()
            .map(|(&pred, &(lo, hi))| (pred, hi - lo))
            .collect();
        v.sort_unstable_by_key(|&(_, size)| size);
        v
    }

    /// Advise the OS to release page-cache pages for the given index after a
    /// large sequential scan, preventing EcoRDF from monopolising the page cache
    /// when sharing a host with other services.
    ///
    /// Calls `madvise(MADV_DONTNEED)` on all three column files of `kind`.
    /// Only effective for `Columnar` (mmap) storage; no-op for interleaved or
    /// delta-encoded columns (they decompress into heap-allocated Vecs).
    ///
    /// Returns the number of bytes released (approximate; actual kernel behaviour
    /// may differ) so the caller can log it.
    pub fn advise_dontneed(&self, kind: IndexKind) -> usize {
        let idx = match kind {
            IndexKind::Spo => Some(&self.spo),
            IndexKind::Pos => Some(&self.pos),
            IndexKind::Osp => Some(&self.osp),
            IndexKind::Pso => self.pso.as_ref(),
            IndexKind::Sop => self.sop.as_ref(),
            IndexKind::Ops => self.ops.as_ref(),
        };
        let Some(idx) = idx else { return 0 };
        idx.advise_dontneed()
    }

    /// Compress all column files under `store_dir` into delta-encoded `.dz` files.
    ///
    /// For each `*.c0` / `*.c1` / `*.c2` file under `store_dir`, reads the raw
    /// u64 values and writes a compressed `*.c0.dz` / `*.c1.dz` / `*.c2.dz`
    /// file next to it.
    ///
    /// - `force = false`: skip columns whose `.dz` already exists.
    /// - `force = true`:  overwrite existing `.dz` files.
    ///
    /// **Predicate-boundary alignment**: for `pos.c1` and `pso.c1` (object
    /// column of predicate-sorted indexes), a new delta block is forced at each
    /// predicate boundary.  This keeps all object IDs in a block within one
    /// predicate's range, minimising maximum delta and enabling U8/U16 encoding.
    ///
    /// Returns the number of column files compressed.
    pub fn compress_columns(store_dir: &std::path::Path, force: bool) -> io::Result<usize> {
        use crate::col_delta::{encode_column, encode_column_pred_aligned};

        /// Load predicate run boundaries (lo positions > 0) from the .pidx alongside c0.
        fn load_pred_boundaries(c0_path: &std::path::Path) -> Vec<usize> {
            let pidx_path = pidx_path_from_c0(c0_path);
            let Ok(pidx) = PredicateIndex::load(&pidx_path) else { return Vec::new(); };
            let mut boundaries: Vec<usize> = pidx.ranges.values().map(|&(lo, _)| lo).collect();
            boundaries.sort_unstable();
            boundaries.retain(|&b| b > 0);
            boundaries
        }

        let mut compressed = 0usize;
        for entry in std::fs::read_dir(store_dir)?.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
            // Match *.c0, *.c1, *.c2 (but not *.c0.dz, etc.)
            if !( (name.ends_with(".c0") || name.ends_with(".c1") || name.ends_with(".c2"))
                  && !name.ends_with(".dz") )
            {
                continue;
            }

            let dz_path = delta_path(&path);
            if !force && dz_path.exists() {
                tracing::debug!(?dz_path, "compress-cols: already exists, skipping");
                continue;
            }

            // Read the raw column.
            let mmap_file = File::open(&path)?;
            let mmap = unsafe { Mmap::map(&mmap_file)? };

            if mmap.len() < 16 {
                tracing::warn!(?path, "compress-cols: file too small, skipping");
                continue;
            }
            if &mmap[0..8] != COL_MAGIC {
                tracing::debug!(?path, "compress-cols: not a ECOCOL01 file, skipping");
                continue;
            }
            let count = u64::from_le_bytes(mmap[8..16].try_into().unwrap()) as usize;
            let expected = 16 + count * 8;
            if mmap.len() != expected {
                tracing::warn!(?path, count, "compress-cols: size mismatch, skipping");
                continue;
            }

            // Decode raw u64 values.
            let values: Vec<u64> = (0..count)
                .map(|i| {
                    let off = 16 + i * 8;
                    u64::from_le_bytes(mmap[off..off+8].try_into().unwrap())
                })
                .collect();

            let t = std::time::Instant::now();

            // For pos.c1 / pso.c1 (object column of predicate-sorted index),
            // use predicate-boundary-aligned encoding for better compression.
            let use_pred_align = name == "pos.c1" || name == "pso.c1";
            if use_pred_align {
                let c0_name = name.replace(".c1", ".c0");
                let c0_path = path.with_file_name(&c0_name);
                let boundaries = load_pred_boundaries(&c0_path);
                if !boundaries.is_empty() {
                    tracing::debug!(
                        col = name.as_str(),
                        boundaries = boundaries.len(),
                        "compress-cols: using pred-aligned encoding"
                    );
                    encode_column_pred_aligned(&values, &boundaries, &dz_path)?;
                } else {
                    encode_column(&values, &dz_path)?;
                }
            } else {
                encode_column(&values, &dz_path)?;
            }
            let orig_mb = mmap.len() as f64 / (1024.0 * 1024.0);
            let dz_size = std::fs::metadata(&dz_path)?.len();
            let dz_mb   = dz_size as f64 / (1024.0 * 1024.0);
            let ratio   = orig_mb / dz_mb;
            tracing::info!(
                col = name.as_str(),
                orig_mb = format!("{:.1}", orig_mb),
                dz_mb   = format!("{:.1}", dz_mb),
                ratio   = format!("{:.1}×", ratio),
                elapsed_ms = t.elapsed().as_millis(),
                "compress-cols: compressed"
            );
            eprintln!(
                "  {} → {:.1} MB → {:.1} MB ({:.1}×) in {}ms",
                name, orig_mb, dz_mb, ratio, t.elapsed().as_millis()
            );
            compressed += 1;
        }

        tracing::info!(compressed, ?store_dir, "compress-cols: done");
        Ok(compressed)
    }
}

// ── Parallel chunk collection ─────────────────────────────────────────────────

/// Sorted chunk files produced by one worker thread during parallel Phase 2.
///
/// Each field is a list of chunk files for the corresponding index type.
/// All `Vec`s may be empty if no triples (or no quads) were encountered.
pub struct ParallelChunks {
    pub spo:  Vec<PathBuf>,
    pub pos:  Vec<PathBuf>,
    pub osp:  Vec<PathBuf>,
    pub pso:  Vec<PathBuf>,
    pub sop:  Vec<PathBuf>,
    pub ops:  Vec<PathBuf>,
    pub gspo: Vec<PathBuf>,
}

// ── Builder convenience ───────────────────────────────────────────────────────

pub struct AllBuilders {
    pub spo: IndexBuilder,
    pub pos: IndexBuilder,
    pub osp: IndexBuilder,
    /// PSO: sorted (P,S,O) — enables efficient (p+s)-bound lookups.
    pub pso: IndexBuilder,
    /// SOP: sorted (S,O,P) — enables efficient (s+o)-bound lookups.
    pub sop: IndexBuilder,
    /// OPS: sorted (O,P,S) — enables efficient (o+p)-bound lookups.
    pub ops: IndexBuilder,
    /// GSPO quad index — only built when quads are pushed.
    pub gspo: GspoBuilder,
    /// Temp directory for external-sort chunk files (`<store-dir>/_ecordf_tmp`).
    /// `None` when running in unbounded in-memory mode.
    tmp_dir: Option<PathBuf>,
}

impl AllBuilders {
    /// Unbounded in-memory builder (old behaviour, no chunking).
    pub fn new() -> Self {
        Self {
            spo:     IndexBuilder::new(IndexKind::Spo),
            pos:     IndexBuilder::new(IndexKind::Pos),
            osp:     IndexBuilder::new(IndexKind::Osp),
            pso:     IndexBuilder::new(IndexKind::Pso),
            sop:     IndexBuilder::new(IndexKind::Sop),
            ops:     IndexBuilder::new(IndexKind::Ops),
            gspo:    GspoBuilder::new(),
            tmp_dir: None,
        }
    }

    /// External-sort builder: flushes sorted chunks of `chunk_size` triples to
    /// `<dir>/_ecordf_tmp/` and k-way merges them on `build()`.
    ///
    /// Creates the temp directory immediately; returns an error if it cannot be
    /// created.  The caller is responsible for deleting the directory on
    /// failure; on success `build()` removes it automatically.
    pub fn new_streaming(dir: &Path, chunk_size: usize) -> io::Result<Self> {
        if chunk_size == 0 {
            return Ok(Self::new());
        }
        let tmp_dir = dir.join("_ecordf_tmp");
        Self::new_streaming_in(&tmp_dir, chunk_size)
    }

    /// Like [`new_streaming`] but writes chunks directly to the given
    /// `chunk_dir` rather than `<dir>/_ecordf_tmp`.
    ///
    /// Used by the parallel loader so that each worker thread can write to its
    /// own private subdirectory, avoiding file-name collisions.
    pub fn new_streaming_in(chunk_dir: &Path, chunk_size: usize) -> io::Result<Self> {
        if chunk_size == 0 {
            return Ok(Self::new());
        }
        std::fs::create_dir_all(chunk_dir)?;
        let cd = chunk_dir.to_path_buf();
        Ok(Self {
            spo:  IndexBuilder::new_streaming(IndexKind::Spo, cd.clone(), chunk_size),
            pos:  IndexBuilder::new_streaming(IndexKind::Pos, cd.clone(), chunk_size),
            osp:  IndexBuilder::new_streaming(IndexKind::Osp, cd.clone(), chunk_size),
            pso:  IndexBuilder::new_streaming(IndexKind::Pso, cd.clone(), chunk_size),
            sop:  IndexBuilder::new_streaming(IndexKind::Sop, cd.clone(), chunk_size),
            ops:  IndexBuilder::new_streaming(IndexKind::Ops, cd.clone(), chunk_size),
            gspo: GspoBuilder::new_streaming(cd.clone(), chunk_size),
            tmp_dir: Some(cd),
        })
    }

    /// Push a plain triple (no named graph → union graph only, not GSPO).
    pub fn push(&mut self, t: Triple) -> io::Result<()> {
        self.spo.push(t)?;
        self.pos.push(t)?;
        self.osp.push(t)?;
        self.pso.push(t)?;
        self.sop.push(t)?;
        self.ops.push(t)?;
        Ok(())
    }

    /// Push a quad (triple + named graph).
    /// The triple is also added to all 6 triple indexes for union-graph queries.
    pub fn push_quad(&mut self, q: Quad) -> io::Result<()> {
        let t = q.to_triple();
        self.spo.push(t)?;
        self.pos.push(t)?;
        self.osp.push(t)?;
        self.pso.push(t)?;
        self.sop.push(t)?;
        self.ops.push(t)?;
        self.gspo.push(q)?;
        Ok(())
    }

    // ── parallel support ──────────────────────────────────────────────────────

    /// Flush all remaining buffers and return the chunk paths for all indexes
    /// without doing the final merge.
    ///
    /// Used by the parallel loader: each worker thread calls this, then the
    /// main thread gathers all [`ParallelChunks`] and passes them to
    /// [`AllBuilders::build_from_parallel_chunks`].
    ///
    /// **Only valid in streaming mode** (`chunk_size > 0`).
    pub(crate) fn flush_and_return_chunks(self) -> io::Result<ParallelChunks> {
        Ok(ParallelChunks {
            spo:  self.spo.flush_and_return_chunks()?,
            pos:  self.pos.flush_and_return_chunks()?,
            osp:  self.osp.flush_and_return_chunks()?,
            pso:  self.pso.flush_and_return_chunks()?,
            sop:  self.sop.flush_and_return_chunks()?,
            ops:  self.ops.flush_and_return_chunks()?,
            gspo: self.gspo.flush_and_return_chunks()?,
        })
    }

    /// Merge all chunk files gathered from parallel worker threads and open
    /// the resulting triple indexes.
    ///
    /// SPO, POS, and OSP merges run in parallel via `rayon::join` since they
    /// each write a different file.  The GSPO merge (if any named-graph data
    /// was loaded) runs sequentially after the triple merges complete.
    ///
    /// After a successful merge the individual chunk files are deleted.
    /// The caller is responsible for cleaning up the per-thread temp
    /// directories (the top-level `_ecordf_tmp` is removed by `store.rs`).
    pub fn build_from_parallel_chunks(
        all: Vec<ParallelChunks>,
        dir: &Path,
    ) -> io::Result<TripleIndex> {
        // Collect chunks by index type
        let spo_chunks: Vec<PathBuf> = all.iter().flat_map(|c| c.spo.iter().cloned()).collect();
        let pos_chunks: Vec<PathBuf> = all.iter().flat_map(|c| c.pos.iter().cloned()).collect();
        let osp_chunks: Vec<PathBuf> = all.iter().flat_map(|c| c.osp.iter().cloned()).collect();
        let pso_chunks: Vec<PathBuf> = all.iter().flat_map(|c| c.pso.iter().cloned()).collect();
        let sop_chunks: Vec<PathBuf> = all.iter().flat_map(|c| c.sop.iter().cloned()).collect();
        let ops_chunks: Vec<PathBuf> = all.iter().flat_map(|c| c.ops.iter().cloned()).collect();
        let gspo_chunks: Vec<PathBuf> = all.iter().flat_map(|c| c.gspo.iter().cloned()).collect();

        let spo_path  = dir.join("spo.bin");
        let pos_path  = dir.join("pos.bin");
        let osp_path  = dir.join("osp.bin");
        let pso_path  = dir.join("pso.bin");
        let sop_path  = dir.join("sop.bin");
        let ops_path  = dir.join("ops.bin");
        let gspo_path = dir.join("gspo.bin");

        eprintln!(
            "  Merging 6 indexes in parallel: {} SPO+POS+OSP+PSO+SOP+OPS chunks each",
            spo_chunks.len()
        );

        // Run all six triple-index merges in parallel (each writes a different file).
        let merge_spo = || Self::merge_or_empty(&spo_chunks, &spo_path, IndexKind::Spo);
        let merge_pos = || Self::merge_or_empty(&pos_chunks, &pos_path, IndexKind::Pos);
        let merge_osp = || Self::merge_or_empty(&osp_chunks, &osp_path, IndexKind::Osp);
        let merge_pso = || Self::merge_or_empty(&pso_chunks, &pso_path, IndexKind::Pso);
        let merge_sop = || Self::merge_or_empty(&sop_chunks, &sop_path, IndexKind::Sop);
        let merge_ops = || Self::merge_or_empty(&ops_chunks, &ops_path, IndexKind::Ops);

        // rayon::join is binary; nest calls to run all 6 in parallel.
        let (r_spo, (r_pos, (r_osp, (r_pso, (r_sop, r_ops))))) = rayon::join(
            merge_spo,
            || rayon::join(
                merge_pos,
                || rayon::join(
                    merge_osp,
                    || rayon::join(
                        merge_pso,
                        || rayon::join(merge_sop, merge_ops),
                    ),
                ),
            ),
        );
        r_spo?; r_pos?; r_osp?; r_pso?; r_sop?; r_ops?;

        // Remove chunk files (dirs cleaned up by store.rs)
        for c in spo_chunks.iter()
            .chain(pos_chunks.iter()).chain(osp_chunks.iter())
            .chain(pso_chunks.iter()).chain(sop_chunks.iter()).chain(ops_chunks.iter())
        {
            let _ = std::fs::remove_file(c);
        }

        // GSPO quad index (sequential; only present when named-graph data was loaded)
        let gspo = if gspo_chunks.is_empty() {
            None
        } else {
            eprintln!("  Merging {} GSPO chunks…", gspo_chunks.len());
            merge_quad_chunks(&gspo_chunks, &gspo_path)?;
            for c in &gspo_chunks {
                let _ = std::fs::remove_file(c);
            }
            Some(GspoIndexFile::open(&gspo_path)?)
        };

        Ok(TripleIndex {
            spo: IndexFile::open(&spo_path, IndexKind::Spo)?,
            pos: IndexFile::open(&pos_path, IndexKind::Pos)?,
            osp: IndexFile::open(&osp_path, IndexKind::Osp)?,
            pso: Some(IndexFile::open(&pso_path, IndexKind::Pso)?),
            sop: Some(IndexFile::open(&sop_path, IndexKind::Sop)?),
            ops: Some(IndexFile::open(&ops_path, IndexKind::Ops)?),
            gspo,
        })
    }

    /// Helper: merge chunks into columnar files at `path`, or write empty columns if no chunks.
    fn merge_or_empty(chunks: &[PathBuf], path: &Path, kind: IndexKind) -> io::Result<()> {
        if chunks.is_empty() {
            write_columnar_from_sorted(&[], path, kind)
        } else {
            merge_triple_chunks(chunks, path, kind)
        }
    }

    // ── standard build ────────────────────────────────────────────────────────

    pub fn build(self, dir: &Path) -> io::Result<TripleIndex> {
        let tmp_dir = self.tmp_dir.clone();
        let result = self.build_internal(dir);
        // Always clean up the temp directory (success or failure).
        if let Some(td) = tmp_dir {
            let _ = std::fs::remove_dir_all(&td);
        }
        result
    }

    fn build_internal(self, dir: &Path) -> io::Result<TripleIndex> {
        let spo = self.spo.build(&dir.join("spo.bin"))?;
        let pos = self.pos.build(&dir.join("pos.bin"))?;
        let osp = self.osp.build(&dir.join("osp.bin"))?;
        let pso = self.pso.build(&dir.join("pso.bin"))?;
        let sop = self.sop.build(&dir.join("sop.bin"))?;
        let ops = self.ops.build(&dir.join("ops.bin"))?;
        let gspo = if !self.gspo.is_empty() {
            Some(self.gspo.build(&dir.join("gspo.bin"))?)
        } else {
            None
        };
        Ok(TripleIndex { spo, pos, osp, pso: Some(pso), sop: Some(sop), ops: Some(ops), gspo })
    }
}

impl Default for AllBuilders {
    fn default() -> Self { Self::new() }
}
