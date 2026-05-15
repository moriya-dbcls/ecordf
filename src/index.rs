//! # Index Layer: Memory-mapped sorted triple arrays
//!
//! ## Why this beats Qlever on memory
//!
//! Qlever loads the *entire* dataset into RAM at startup.
//! We use `memmap2::Mmap` instead: the file is mapped into the virtual address space
//! but actual RAM pages are only allocated by the OS on first access.
//! The OS page cache evicts cold pages under memory pressure automatically.
//!
//! Result: working-set memory = only the triples your queries actually touch.
//! For typical SPARQL workloads over large bio datasets (UniProt ~1B triples),
//! only ~2-5% of the dataset is touched per query session.
//!
//! ## Index structure
//!
//! Three sorted indexes over integer-encoded triples:
//!
//! ```text
//!   SPO: sorted by (S, P, O) → efficient for patterns with S bound
//!   POS: sorted by (P, O, S) → efficient for patterns with P bound
//!   OSP: sorted by (O, S, P) → efficient for patterns with O bound
//! ```
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

use memmap2::Mmap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::Path;

use crate::triple::{IndexKind, Quad, TermId, Triple, TriplePattern, UNBOUND};

const INDEX_MAGIC: &[u8; 8] = b"ECOI0001";
const HEADER_SIZE: usize = 16; // magic(8) + count(8)
const TRIPLE_BYTES: usize = 12; // 3 × u32

// ── GSPO quad index constants ─────────────────────────────────────────────────
const GSPO_MAGIC: &[u8; 8] = b"ECOG0001";
const QUAD_BYTES: usize = 16; // 4 × u32 : (g, s, p, o)

// ── In-memory index (during build) ──────────────────────────────────────────

/// Mutable index used during the load phase.
pub struct IndexBuilder {
    pub kind: IndexKind,
    triples: Vec<[u32; 3]>,
}

impl IndexBuilder {
    pub fn new(kind: IndexKind) -> Self {
        Self {
            kind,
            triples: Vec::new(),
        }
    }

    pub fn push(&mut self, t: Triple) {
        self.triples.push(reorder(t, self.kind));
    }

    /// Sort and write to disk, then return a read-only `IndexFile`.
    pub fn build(mut self, path: &Path) -> io::Result<IndexFile> {
        // Sort by the index key order
        self.triples.sort_unstable();

        // Write binary file
        {
            let file = File::create(path)?;
            let mut w = BufWriter::with_capacity(4 * 1024 * 1024, file);
            w.write_all(INDEX_MAGIC)?;
            let count = self.triples.len() as u64;
            w.write_all(&count.to_le_bytes())?;
            for t in &self.triples {
                w.write_all(&t[0].to_le_bytes())?;
                w.write_all(&t[1].to_le_bytes())?;
                w.write_all(&t[2].to_le_bytes())?;
            }
            w.flush()?;
        }

        IndexFile::open(path, self.kind)
    }
}

// ── Read-only mmap-backed index ──────────────────────────────────────────────

/// A read-only sorted index backed by a memory-mapped file.
///
/// The `Mmap` struct itself is just a pointer + length; actual RAM pages are
/// managed by the OS kernel. Cold pages are evicted automatically.
pub struct IndexFile {
    pub kind: IndexKind,
    _file: File, // keep file handle alive
    mmap: Mmap,
    count: usize,
}

impl IndexFile {
    /// Open an existing index file.
    pub fn open(path: &Path, kind: IndexKind) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).open(path)?;
        // Safety: we never modify the file while mapped.
        let mmap = unsafe { Mmap::map(&file)? };

        // Validate header
        if mmap.len() < HEADER_SIZE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "index too small"));
        }
        if &mmap[0..8] != INDEX_MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad index magic"));
        }
        let count = u64::from_le_bytes(mmap[8..16].try_into().unwrap()) as usize;

        let expected_size = HEADER_SIZE + count * TRIPLE_BYTES;
        if mmap.len() != expected_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("index size mismatch: {} vs {}", mmap.len(), expected_size),
            ));
        }

        Ok(Self {
            kind,
            _file: file,
            mmap,
            count,
        })
    }

    /// Number of triples in this index.
    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Read a triple by index position (in the index's sort order, not SPO).
    #[inline]
    fn get_raw(&self, pos: usize) -> [u32; 3] {
        debug_assert!(pos < self.count);
        let off = HEADER_SIZE + pos * TRIPLE_BYTES;
        let s = u32::from_le_bytes(self.mmap[off..off + 4].try_into().unwrap());
        let p = u32::from_le_bytes(self.mmap[off + 4..off + 8].try_into().unwrap());
        let o = u32::from_le_bytes(self.mmap[off + 8..off + 12].try_into().unwrap());
        [s, p, o]
    }

    /// Convert a raw (reordered) triple back to SPO order.
    #[inline]
    fn to_triple(&self, raw: [u32; 3]) -> Triple {
        reorder_back(raw, self.kind)
    }

    // ── Scan API ──────────────────────────────────────────────────────────────

    /// Binary search: find the first position where raw[0] >= key.
    fn lower_bound_0(&self, key: u32) -> usize {
        let (mut lo, mut hi) = (0, self.count);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.get_raw(mid)[0] < key {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// Binary search: find first pos where (raw[0], raw[1]) >= (k0, k1).
    fn lower_bound_01(&self, k0: u32, k1: u32) -> usize {
        let (mut lo, mut hi) = (0, self.count);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let r = self.get_raw(mid);
            if (r[0], r[1]) < (k0, k1) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// Scan the index for all triples matching the pattern.
    /// Yields triples in SPO order.
    pub fn scan(&self, pat: &TriplePattern) -> TripleScan {
        let raw_pat = pattern_to_raw(*pat, self.kind);
        let (start, end) = self.range_for_pattern(&raw_pat);
        TripleScan {
            index: self,
            raw_pat,
            pos: start,
            end,
        }
    }

    fn range_for_pattern(&self, raw: &[Option<u32>; 3]) -> (usize, usize) {
        match (raw[0], raw[1]) {
            (Some(k0), Some(k1)) => {
                // Seek to exact (k0, k1) range
                let start = self.lower_bound_01(k0, k1);
                // End is where k0 or k1 changes
                let mut end = start;
                while end < self.count {
                    let r = self.get_raw(end);
                    if r[0] != k0 || r[1] != k1 {
                        break;
                    }
                    end += 1;
                }
                (start, end)
            }
            (Some(k0), None) => {
                // Seek to k0 range
                let start = self.lower_bound_0(k0);
                let mut end = start;
                while end < self.count {
                    if self.get_raw(end)[0] != k0 {
                        break;
                    }
                    end += 1;
                }
                (start, end)
            }
            _ => (0, self.count), // Full scan
        }
    }

    /// Estimate cardinality for a triple pattern (used by the optimizer).
    pub fn estimate_cardinality(&self, pat: &TriplePattern) -> u64 {
        let raw_pat = pattern_to_raw(*pat, self.kind);
        let (start, end) = self.range_for_pattern(&raw_pat);
        // Filter estimate for the 3rd component if needed
        if raw_pat[2].is_some() {
            // Assume ~50% selectivity for the unindexed 3rd component
            ((end - start) as u64).saturating_add(1) / 2
        } else {
            (end - start) as u64
        }
    }

    // ── Leapfrog Triejoin support ─────────────────────────────────────────────

    /// Seek to the first position where raw[0] >= target.
    /// Returns the actual raw[0] at that position, or u32::MAX if exhausted.
    pub fn seek_0(&self, from: usize, target: u32) -> (usize, u32) {
        let pos = {
            let (mut lo, mut hi) = (from, self.count);
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                if self.get_raw(mid)[0] < target {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            lo
        };
        if pos < self.count {
            (pos, self.get_raw(pos)[0])
        } else {
            (self.count, u32::MAX)
        }
    }
}

// ── Iterator over matching triples ───────────────────────────────────────────

pub struct TripleScan<'a> {
    index: &'a IndexFile,
    raw_pat: [Option<u32>; 3],
    pos: usize,
    end: usize,
}

impl<'a> Iterator for TripleScan<'a> {
    type Item = Triple;

    fn next(&mut self) -> Option<Triple> {
        while self.pos < self.end {
            let raw = self.index.get_raw(self.pos);
            self.pos += 1;
            // Check 3rd component if bound
            if let Some(k2) = self.raw_pat[2] {
                if raw[2] != k2 {
                    continue;
                }
            }
            return Some(self.index.to_triple(raw));
        }
        None
    }
}

// ── Reorder helpers ───────────────────────────────────────────────────────────

/// Reorder SPO triple into the index's natural sort key.
#[inline]
fn reorder(t: Triple, kind: IndexKind) -> [u32; 3] {
    match kind {
        IndexKind::Spo => [t.s, t.p, t.o],
        IndexKind::Pos => [t.p, t.o, t.s],
        IndexKind::Osp => [t.o, t.s, t.p],
    }
}

/// Convert index-ordered raw triple back to SPO.
#[inline]
fn reorder_back(raw: [u32; 3], kind: IndexKind) -> Triple {
    match kind {
        IndexKind::Spo => Triple::new(raw[0], raw[1], raw[2]),
        IndexKind::Pos => Triple::new(raw[2], raw[0], raw[1]),
        IndexKind::Osp => Triple::new(raw[1], raw[2], raw[0]),
    }
}

/// Convert a TriplePattern to the index-reordered form for scanning.
fn pattern_to_raw(pat: TriplePattern, kind: IndexKind) -> [Option<TermId>; 3] {
    let b = |v: TermId| if v == UNBOUND { None } else { Some(v) };
    match kind {
        IndexKind::Spo => [b(pat.s), b(pat.p), b(pat.o)],
        IndexKind::Pos => [b(pat.p), b(pat.o), b(pat.s)],
        IndexKind::Osp => [b(pat.o), b(pat.s), b(pat.p)],
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
pub struct GspoBuilder {
    quads: Vec<[u32; 4]>, // (g, s, p, o)
}

impl GspoBuilder {
    pub fn new() -> Self {
        Self { quads: Vec::new() }
    }

    pub fn push(&mut self, q: Quad) {
        self.quads.push([q.g, q.s, q.p, q.o]);
    }

    pub fn is_empty(&self) -> bool {
        self.quads.is_empty()
    }

    /// Sort and write to disk, returning a read-only GspoIndexFile.
    pub fn build(mut self, path: &Path) -> io::Result<GspoIndexFile> {
        self.quads.sort_unstable();
        {
            let file = File::create(path)?;
            let mut w = BufWriter::with_capacity(4 * 1024 * 1024, file);
            w.write_all(GSPO_MAGIC)?;
            w.write_all(&(self.quads.len() as u64).to_le_bytes())?;
            for q in &self.quads {
                for v in q {
                    w.write_all(&v.to_le_bytes())?;
                }
            }
            w.flush()?;
        }
        GspoIndexFile::open(path)
    }
}

impl Default for GspoBuilder {
    fn default() -> Self { Self::new() }
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
    fn get_raw(&self, pos: usize) -> [u32; 4] {
        debug_assert!(pos < self.count);
        let off = HEADER_SIZE + pos * QUAD_BYTES;
        [
            u32::from_le_bytes(self.mmap[off..off+4].try_into().unwrap()),
            u32::from_le_bytes(self.mmap[off+4..off+8].try_into().unwrap()),
            u32::from_le_bytes(self.mmap[off+8..off+12].try_into().unwrap()),
            u32::from_le_bytes(self.mmap[off+12..off+16].try_into().unwrap()),
        ]
    }

    /// Binary search: first position where raw[0] >= key.
    fn lower_bound_g(&self, g: u32) -> usize {
        let (mut lo, mut hi) = (0, self.count);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.get_raw(mid)[0] < g { lo = mid + 1; } else { hi = mid; }
        }
        lo
    }

    /// Binary search: first position after all entries with raw[0] == g.
    fn upper_bound_g(&self, g: u32) -> usize {
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

/// Holds all three SPO/POS/OSP indexes plus an optional GSPO quad index.
pub struct TripleIndex {
    pub spo: IndexFile,
    pub pos: IndexFile,
    pub osp: IndexFile,
    /// Present only when named-graph data was loaded (.nq files).
    pub gspo: Option<GspoIndexFile>,
}

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
            gspo,
        })
    }

    /// Select the best index for a pattern and return matching triples.
    pub fn scan(&self, pat: &TriplePattern) -> TripleScan {
        match pat.best_index() {
            IndexKind::Spo => self.spo.scan(pat),
            IndexKind::Pos => self.pos.scan(pat),
            IndexKind::Osp => self.osp.scan(pat),
        }
    }

    /// Estimate cardinality using the best index.
    pub fn estimate(&self, pat: &TriplePattern) -> u64 {
        match pat.best_index() {
            IndexKind::Spo => self.spo.estimate_cardinality(pat),
            IndexKind::Pos => self.pos.estimate_cardinality(pat),
            IndexKind::Osp => self.osp.estimate_cardinality(pat),
        }
    }

    pub fn triple_count(&self) -> usize { self.spo.len() }

    /// Number of named graphs (0 if no GSPO index).
    pub fn graph_count(&self) -> usize {
        self.gspo.as_ref().map(|g| g.graphs().len()).unwrap_or(0)
    }
}

// ── Builder convenience ───────────────────────────────────────────────────────

pub struct AllBuilders {
    pub spo: IndexBuilder,
    pub pos: IndexBuilder,
    pub osp: IndexBuilder,
    /// GSPO quad index — only built when quads are pushed.
    pub gspo: GspoBuilder,
}

impl AllBuilders {
    pub fn new() -> Self {
        Self {
            spo: IndexBuilder::new(IndexKind::Spo),
            pos: IndexBuilder::new(IndexKind::Pos),
            osp: IndexBuilder::new(IndexKind::Osp),
            gspo: GspoBuilder::new(),
        }
    }

    /// Push a plain triple (no named graph → union graph only, not GSPO).
    pub fn push(&mut self, t: Triple) {
        self.spo.push(t);
        self.pos.push(t);
        self.osp.push(t);
    }

    /// Push a quad (triple + named graph).
    /// The triple is also added to SPO/POS/OSP for union-graph queries.
    pub fn push_quad(&mut self, q: Quad) {
        self.spo.push(q.to_triple());
        self.pos.push(q.to_triple());
        self.osp.push(q.to_triple());
        self.gspo.push(q);
    }

    pub fn build(self, dir: &Path) -> io::Result<TripleIndex> {
        let spo = self.spo.build(&dir.join("spo.bin"))?;
        let pos = self.pos.build(&dir.join("pos.bin"))?;
        let osp = self.osp.build(&dir.join("osp.bin"))?;
        let gspo = if !self.gspo.is_empty() {
            Some(self.gspo.build(&dir.join("gspo.bin"))?)
        } else {
            None
        };
        Ok(TripleIndex { spo, pos, osp, gspo })
    }
}

impl Default for AllBuilders {
    fn default() -> Self { Self::new() }
}
