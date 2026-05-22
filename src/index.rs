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
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

// rayon::join is used in build_from_parallel_chunks to merge 3 indexes in parallel.
use rayon;

use crate::triple::{IndexKind, Quad, TermId, Triple, TriplePattern, UNBOUND};

const INDEX_MAGIC: &[u8; 8] = b"ECOI0001";
const HEADER_SIZE: usize = 16; // magic(8) + count(8)
const TRIPLE_BYTES: usize = 12; // 3 × u32

// ── GSPO quad index constants ─────────────────────────────────────────────────
const GSPO_MAGIC: &[u8; 8] = b"ECOG0001";
const QUAD_BYTES: usize = 16; // 4 × u32 : (g, s, p, o)

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
    triples: Vec<[u32; 3]>,
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

    /// Sort and write to disk, then return a read-only `IndexFile`.
    pub fn build(mut self, path: &Path) -> io::Result<IndexFile> {
        if self.chunks.is_empty() {
            // ── In-memory path (chunk_size == 0 or dataset fits in one chunk)
            self.triples.sort_unstable();
            write_index_from_sorted(&self.triples, path)?;
        } else {
            // ── External-sort path: flush remaining buffer, k-way merge
            self.flush_chunk()?;
            eprintln!(
                "  Merging {} sorted chunks → {:?} index…",
                self.chunks.len(), self.kind
            );
            merge_triple_chunks(&self.chunks, path)?;
            // Remove individual chunk files (the _ecordf_tmp dir is removed by
            // AllBuilders::build once all indexes are written).
            for chunk in &self.chunks {
                let _ = std::fs::remove_file(chunk);
            }
        }
        IndexFile::open(path, self.kind)
    }
}

// ── Triple chunk helpers ──────────────────────────────────────────────────────

/// Write a pre-sorted slice of raw triples to a binary chunk file.
///
/// Format: `[count: u64][a0, b0, c0, a1, b1, c1, …  : u32 each]`
pub(crate) fn write_triple_chunk(triples: &[[u32; 3]], path: &Path) -> io::Result<()> {
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

/// Write a sorted slice of raw triples as a final index file (with header).
pub(crate) fn write_index_from_sorted(triples: &[[u32; 3]], path: &Path) -> io::Result<()> {
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

/// k-way merge of sorted triple chunk files into a single index file.
///
/// When `chunks.len() > MAX_FAN_IN` a **hierarchical merge** is performed
/// automatically to keep open-file-descriptor usage bounded (EMFILE-safe).
///
/// Consecutive duplicate triples are dropped so the output is a set.
pub(crate) fn merge_triple_chunks(chunks: &[PathBuf], path: &Path) -> io::Result<()> {
    if chunks.len() <= MAX_FAN_IN {
        return merge_triple_chunks_direct(chunks, path);
    }

    // ── Hierarchical pass: merge batches → intermediate chunk files ───────────
    let mut intermediates: Vec<PathBuf> = Vec::new();
    for (i, batch) in chunks.chunks(MAX_FAN_IN).enumerate() {
        let tmp = append_path_suffix(path, &format!(".__merge_{:04}.tmp", i));
        merge_triple_chunks_to_chunk(batch, &tmp)?;
        intermediates.push(tmp);
    }

    // ── Final pass ────────────────────────────────────────────────────────────
    let result = if intermediates.len() <= MAX_FAN_IN {
        merge_triple_chunks_direct(&intermediates, path)
    } else {
        merge_triple_chunks(&intermediates, path)
    };

    for p in &intermediates {
        let _ = std::fs::remove_file(p);
    }

    result
}

/// Merge up to `MAX_FAN_IN` chunk files directly into a final index file.
///
/// The output has the full index-file header (`INDEX_MAGIC` + count).
/// The count is back-patched after writing all triples.
fn merge_triple_chunks_direct(chunks: &[PathBuf], path: &Path) -> io::Result<()> {
    // ── Open all chunk readers ────────────────────────────────────────────────
    let mut readers: Vec<TripleChunkReader> = chunks.iter()
        .map(|p| TripleChunkReader::open(p))
        .collect::<io::Result<Vec<_>>>()?;

    // ── Write output header (count placeholder, back-patched later) ───────────
    let out_file = OpenOptions::new().create(true).write(true).truncate(true).open(path)?;
    let mut w = BufWriter::with_capacity(8 * 1024 * 1024, out_file);
    w.write_all(INDEX_MAGIC)?;
    w.write_all(&0u64.to_le_bytes())?; // placeholder

    // ── Seed heap ─────────────────────────────────────────────────────────────
    let mut heap: BinaryHeap<Reverse<([u32; 3], usize)>> = BinaryHeap::new();
    for (i, reader) in readers.iter_mut().enumerate() {
        if let Some(t) = reader.next()? {
            heap.push(Reverse((t, i)));
        }
    }

    // ── Merge with deduplication ──────────────────────────────────────────────
    let mut count = 0u64;
    let mut prev: Option<[u32; 3]> = None;
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

    // ── Back-patch the count field (offset 8, after magic) ───────────────────
    drop(w);
    let mut f = OpenOptions::new().write(true).open(path)?;
    f.seek(SeekFrom::Start(8))?;
    f.write_all(&count.to_le_bytes())?;

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

    let mut heap: BinaryHeap<Reverse<([u32; 3], usize)>> = BinaryHeap::new();
    for (i, reader) in readers.iter_mut().enumerate() {
        if let Some(t) = reader.next()? {
            heap.push(Reverse((t, i)));
        }
    }

    let mut count = 0u64;
    let mut prev: Option<[u32; 3]> = None;
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

    fn next(&mut self) -> io::Result<Option<[u32; 3]>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        let mut buf = [0u8; 12];
        self.reader.read_exact(&mut buf)?;
        self.remaining -= 1;
        Ok(Some([
            u32::from_le_bytes(buf[0..4].try_into().unwrap()),
            u32::from_le_bytes(buf[4..8].try_into().unwrap()),
            u32::from_le_bytes(buf[8..12].try_into().unwrap()),
        ]))
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
    ///
    /// Uses **galloping search** (exponential probe + binary search in the
    /// narrowed range): O(log k) where k = distance from `from` to the result.
    /// This is significantly faster than O(log n) binary search over the entire
    /// index when the target is close — the common case in Leapfrog Triejoin
    /// because successive `seek` calls advance by small amounts.
    pub fn seek_0(&self, from: usize, target: u32) -> (usize, u32) {
        if from >= self.count {
            return (self.count, u32::MAX);
        }
        // If already at or past target, return immediately.
        let cur = self.get_raw(from)[0];
        if cur >= target {
            return (from, cur);
        }

        // ── Galloping phase ───────────────────────────────────────────────────
        // `lo` always satisfies get_raw(lo)[0] < target.
        // We probe at lo+step, doubling step each time until we overshoot or
        // run off the end of the index.
        let mut lo = from;
        let mut step = 1usize;
        loop {
            let probe = lo + step;
            if probe >= self.count || self.get_raw(probe)[0] >= target {
                // Binary search in the range (lo, min(probe, count))
                let hi = probe.min(self.count);
                let (mut a, mut b) = (lo + 1, hi); // a > lo so a-1 is guaranteed < target
                while a < b {
                    let mid = a + (b - a) / 2;
                    if self.get_raw(mid)[0] < target {
                        a = mid + 1;
                    } else {
                        b = mid;
                    }
                }
                return if a < self.count {
                    (a, self.get_raw(a)[0])
                } else {
                    (self.count, u32::MAX)
                };
            }
            lo = probe;
            step = step.saturating_mul(2);
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
///
/// Supports the same external-sort pattern as [`IndexBuilder`]: when
/// `chunk_size > 0` quads are flushed to sorted temp files and k-way
/// merged on `build()`.
pub struct GspoBuilder {
    quads: Vec<[u32; 4]>, // (g, s, p, o)
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

fn write_quad_chunk(quads: &[[u32; 4]], path: &Path) -> io::Result<()> {
    let file = File::create(path)?;
    let mut w = BufWriter::with_capacity(4 * 1024 * 1024, file);
    w.write_all(&(quads.len() as u64).to_le_bytes())?;
    for q in quads {
        for v in q { w.write_all(&v.to_le_bytes())?; }
    }
    w.flush()
}

fn write_gspo_index_from_sorted(quads: &[[u32; 4]], path: &Path) -> io::Result<()> {
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

    let mut heap: BinaryHeap<Reverse<([u32; 4], usize)>> = BinaryHeap::new();
    for (i, reader) in readers.iter_mut().enumerate() {
        if let Some(q) = reader.next()? {
            heap.push(Reverse((q, i)));
        }
    }

    let mut count = 0u64;
    let mut prev: Option<[u32; 4]> = None;
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

    let mut heap: BinaryHeap<Reverse<([u32; 4], usize)>> = BinaryHeap::new();
    for (i, reader) in readers.iter_mut().enumerate() {
        if let Some(q) = reader.next()? {
            heap.push(Reverse((q, i)));
        }
    }

    let mut count = 0u64;
    let mut prev: Option<[u32; 4]> = None;
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

    fn next(&mut self) -> io::Result<Option<[u32; 4]>> {
        if self.remaining == 0 { return Ok(None); }
        let mut buf = [0u8; 16];
        self.reader.read_exact(&mut buf)?;
        self.remaining -= 1;
        Ok(Some([
            u32::from_le_bytes(buf[0..4].try_into().unwrap()),
            u32::from_le_bytes(buf[4..8].try_into().unwrap()),
            u32::from_le_bytes(buf[8..12].try_into().unwrap()),
            u32::from_le_bytes(buf[12..16].try_into().unwrap()),
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

    pub fn triple_count(&self) -> usize { self.spo.len() }

    /// Number of named graphs (0 if no GSPO index).
    pub fn graph_count(&self) -> usize {
        self.gspo.as_ref().map(|g| g.graphs().len()).unwrap_or(0)
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
    pub gspo: Vec<PathBuf>,
}

// ── Builder convenience ───────────────────────────────────────────────────────

pub struct AllBuilders {
    pub spo: IndexBuilder,
    pub pos: IndexBuilder,
    pub osp: IndexBuilder,
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
            gspo: GspoBuilder::new_streaming(cd.clone(), chunk_size),
            tmp_dir: Some(cd),
        })
    }

    /// Push a plain triple (no named graph → union graph only, not GSPO).
    pub fn push(&mut self, t: Triple) -> io::Result<()> {
        self.spo.push(t)?;
        self.pos.push(t)?;
        self.osp.push(t)?;
        Ok(())
    }

    /// Push a quad (triple + named graph).
    /// The triple is also added to SPO/POS/OSP for union-graph queries.
    pub fn push_quad(&mut self, q: Quad) -> io::Result<()> {
        self.spo.push(q.to_triple())?;
        self.pos.push(q.to_triple())?;
        self.osp.push(q.to_triple())?;
        self.gspo.push(q)?;
        Ok(())
    }

    // ── parallel support ──────────────────────────────────────────────────────

    /// Flush all remaining buffers and return the chunk paths for all four
    /// indexes without doing the final merge.
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
        let gspo_chunks: Vec<PathBuf> = all.iter().flat_map(|c| c.gspo.iter().cloned()).collect();

        let spo_path  = dir.join("spo.bin");
        let pos_path  = dir.join("pos.bin");
        let osp_path  = dir.join("osp.bin");
        let gspo_path = dir.join("gspo.bin");

        eprintln!(
            "  Merging indexes in parallel: {} SPO + {} POS + {} OSP chunks",
            spo_chunks.len(), pos_chunks.len(), osp_chunks.len()
        );

        // Run the three triple-index merges in parallel (each writes a different file).
        let merge_spo = || Self::merge_or_empty(&spo_chunks, &spo_path);
        let merge_pos = || Self::merge_or_empty(&pos_chunks, &pos_path);
        let merge_osp = || Self::merge_or_empty(&osp_chunks, &osp_path);

        let (r_spo, (r_pos, r_osp)) = rayon::join(
            merge_spo,
            || rayon::join(merge_pos, merge_osp),
        );
        r_spo?;
        r_pos?;
        r_osp?;

        // Remove chunk files (dirs cleaned up by store.rs)
        for c in spo_chunks.iter().chain(pos_chunks.iter()).chain(osp_chunks.iter()) {
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
            gspo,
        })
    }

    /// Helper: merge chunks into `path`, or write an empty index if no chunks.
    fn merge_or_empty(chunks: &[PathBuf], path: &Path) -> io::Result<()> {
        if chunks.is_empty() {
            write_index_from_sorted(&[], path)
        } else {
            merge_triple_chunks(chunks, path)
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
