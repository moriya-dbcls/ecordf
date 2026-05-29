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

use memmap2::{Advice, Mmap};
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

// rayon::join is used in build_from_parallel_chunks to merge 3 indexes in parallel.
use rayon;

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

/// Skip index magic and header size.
/// File layout: magic(8) + stride(4) + entry_count(4) + total_count(8) + entries[u64…]
const SKIP_MAGIC: &[u8; 8] = b"ECOSKIP1";
const SKIP_HDR: usize = 24; // 8 + 4 + 4 + 8

/// One skip anchor every SKIP_STRIDE entries in c0.
///
/// At 3 B triples: ~5.86 M anchors × 8 B ≈ 46.9 MB per index (140 MB total).
/// After narrowing the range is exactly SKIP_STRIDE entries = 4 KB = 1 OS page.
/// The binary search after narrow() runs entirely within that single page —
/// 1 page fault per cold search vs ~31 without the skip index.
const SKIP_STRIDE: usize = 512;

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

/// Sparse in-memory index over the primary-key column (c0) of a columnar index.
///
/// Stores one anchor value per `SKIP_STRIDE` entries: `anchors[i] = c0[i × SKIP_STRIDE]`.
/// With SKIP_STRIDE = 512 the anchor array for 3 B triples is ~5.86 M entries × 8 B ≈ 47 MB,
/// which stays hot in CPU cache; `narrow()` costs ~23 cache-resident comparisons and
/// returns a ≤ 512-entry range — exactly 4 KB = 1 OS page of *contiguous* c0 data.
///
/// ## I/O improvement (3 B triples, 22 GiB c0 file, SKIP_STRIDE = 512)
///
/// Before: `lower_bound_0` does log₂(3 × 10⁹) ≈ 31 random page faults.
/// After:  `narrow()` in hot RAM → `prefetch_c0` fires async MADV_WILLNEED on 4 KB →
///         binary search over 512 entries fits in the 1 resident page →
///         **1 page fault per cold search** (physical minimum without loading c0 into RAM).
struct SkipIndex {
    /// `anchors[i] == c0[i * SKIP_STRIDE]`
    anchors: Vec<u64>,
    /// Total number of entries in the c0 column.
    count: usize,
}

impl SkipIndex {
    /// Build from an in-memory sorted slice of triples.
    /// Samples c0 (column 0) every `SKIP_STRIDE` rows.
    fn build_from_triples(triples: &[[u64; 3]]) -> Self {
        let count = triples.len();
        let anchors = (0..)
            .map(|i: usize| i * SKIP_STRIDE)
            .take_while(|&pos| pos < count)
            .map(|pos| triples[pos][0])
            .collect();
        SkipIndex { anchors, count }
    }

    /// Build by doing one sequential scan over an already-open c0 mmap.
    ///
    /// Called when an existing index was built before skip support was added.
    /// One sequential pass — OS sequential prefetcher makes this fast.
    fn build_from_mmap(mmap: &Mmap, count: usize) -> Self {
        let anchors = (0..)
            .map(|i: usize| i * SKIP_STRIDE)
            .take_while(|&pos| pos < count)
            .map(|pos| {
                let off = HEADER_SIZE + pos * COL_VALUE_BYTES;
                u64::from_le_bytes(mmap[off..off + 8].try_into().unwrap())
            })
            .collect();
        SkipIndex { anchors, count }
    }

    /// Save to a `.skip` file alongside the `.c0` column.
    fn save(&self, path: &Path) -> io::Result<()> {
        let f = File::create(path)?;
        let mut w = BufWriter::new(f);
        w.write_all(SKIP_MAGIC)?;
        w.write_all(&(SKIP_STRIDE as u32).to_le_bytes())?;
        w.write_all(&(self.anchors.len() as u32).to_le_bytes())?;
        w.write_all(&(self.count as u64).to_le_bytes())?;
        for &v in &self.anchors {
            w.write_all(&v.to_le_bytes())?;
        }
        w.flush()
    }

    /// Load from a `.skip` file.
    fn load(path: &Path) -> io::Result<Self> {
        let mut buf = Vec::new();
        File::open(path)?.read_to_end(&mut buf)?;
        if buf.len() < SKIP_HDR {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "skip file too small"));
        }
        if &buf[0..8] != SKIP_MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad skip magic"));
        }
        let stride = u32::from_le_bytes(buf[8..12].try_into().unwrap()) as usize;
        let n      = u32::from_le_bytes(buf[12..16].try_into().unwrap()) as usize;
        let count  = u64::from_le_bytes(buf[16..24].try_into().unwrap()) as usize;
        if stride != SKIP_STRIDE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("skip stride mismatch: file={} code={}", stride, SKIP_STRIDE),
            ));
        }
        if buf.len() != SKIP_HDR + n * 8 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "skip file size mismatch"));
        }
        let anchors = (0..n)
            .map(|i| {
                let off = SKIP_HDR + i * 8;
                u64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
            })
            .collect();
        Ok(SkipIndex { anchors, count })
    }

    /// Return the narrowed `[lo, hi)` range that is guaranteed to contain the
    /// position `lower_bound(key)` in c0.
    ///
    /// The range spans at most `SKIP_STRIDE + 1` contiguous entries — 4 KB (1 OS
    /// page) with SKIP_STRIDE = 512.  A single `prefetch_c0` hint on this range
    /// is enough to load the entire binary search window in one disk I/O.
    #[inline]
    fn narrow(&self, key: u64) -> (usize, usize) {
        if self.anchors.is_empty() {
            return (0, self.count);
        }
        // `slot` = first anchor index where anchors[slot] >= key.
        let slot = self.anchors.partition_point(|&a| a < key);
        // Everything before (slot-1)*SKIP_STRIDE is guaranteed < key (sorted).
        let lo = slot.saturating_sub(1) * SKIP_STRIDE;
        let hi = if slot < self.anchors.len() {
            // anchors[slot] >= key, so lower_bound is at most slot*SKIP_STRIDE.
            (slot * SKIP_STRIDE + 1).min(self.count)
        } else {
            // All anchors < key; lower_bound is somewhere in the tail.
            self.count
        };
        (lo, hi)
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
        if self.chunks.is_empty() {
            // ── In-memory path: data already in RAM, write directly to columns.
            self.triples.sort_unstable();
            write_columnar_from_sorted(&self.triples, path)?;
        } else {
            // ── External-sort path: flush remaining buffer, k-way merge → columns.
            self.flush_chunk()?;
            eprintln!(
                "  Merging {} sorted chunks → {:?} index (columnar)…",
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
/// Given `base_path = dir/spo.bin`, creates:
///   `dir/spo.c0`   — primary-key column (u64 values, count × 8 bytes)
///   `dir/spo.c1`   — secondary-key column
///   `dir/spo.c2`   — tertiary-key column
///   `dir/spo.skip` — sparse skip index over c0 (built in-memory, no extra I/O)
///
/// Each column file: `magic(8) + count(8) + data[u64 × count]`
pub(crate) fn write_columnar_from_sorted(triples: &[[u64; 3]], base_path: &Path) -> io::Result<()> {
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
    Ok(())
}

/// k-way merge of sorted triple chunk files into columnar index files.
///
/// Final output is written as three `.c0`/`.c1`/`.c2` column files derived
/// from `base_path` (e.g. `spo.bin` → `spo.c0`, `spo.c1`, `spo.c2`).
///
/// When `chunks.len() > MAX_FAN_IN` a **hierarchical merge** is performed
/// automatically using plain chunk format for intermediate passes.
/// Only the final merge writes columnar output.
///
/// Consecutive duplicate triples are dropped so the output is a set.
pub(crate) fn merge_triple_chunks(chunks: &[PathBuf], base_path: &Path) -> io::Result<()> {
    if chunks.len() <= MAX_FAN_IN {
        return merge_to_columnar_direct(chunks, base_path);
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
        merge_to_columnar_direct(&intermediates, base_path)
    } else {
        merge_triple_chunks(&intermediates, base_path)
    };

    for p in &intermediates {
        let _ = std::fs::remove_file(p);
    }

    result
}

/// Merge up to `MAX_FAN_IN` chunk files directly into three columnar files.
///
/// Writes `base_path`-derived `.c0`, `.c1`, `.c2` column files.
/// Count is back-patched in each column file after the merge.
fn merge_to_columnar_direct(chunks: &[PathBuf], base_path: &Path) -> io::Result<()> {
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

    // ── Merge with deduplication + skip anchor collection ────────────────────
    let mut count = 0u64;
    let mut prev: Option<[u64; 3]> = None;
    let mut skip_anchors: Vec<u64> = Vec::new();
    while let Some(Reverse((t, i))) = heap.pop() {
        if Some(t) != prev {
            // Record one anchor every SKIP_STRIDE deduplicated output rows.
            if (count as usize) % SKIP_STRIDE == 0 {
                skip_anchors.push(t[0]);
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
    for w in &mut writers { w.flush()?; }
    drop(writers);

    // ── Back-patch count field (offset 8) in each column file ────────────────
    for cpath in &cpaths {
        let mut f = OpenOptions::new().write(true).open(cpath)?;
        f.seek(SeekFrom::Start(8))?;
        f.write_all(&count.to_le_bytes())?;
    }

    // ── Write skip index (anchors collected during merge, no extra I/O) ───────
    let skip = SkipIndex { anchors: skip_anchors, count: count as usize };
    skip.save(&skip_path_from_c0(&cpaths[0]))?;

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
}

impl IndexFile {
    // ── Construction ──────────────────────────────────────────────────────────

    /// Open an index, preferring the columnar format if the `.c0` file exists,
    /// falling back to the legacy interleaved `.bin` file otherwise.
    pub fn open(path: &Path, kind: IndexKind) -> io::Result<Self> {
        let cpaths = col_paths(path);
        if cpaths[0].exists() {
            Self::open_columnar(&cpaths, kind)
        } else {
            Self::open_interleaved(path, kind)
        }
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
        Ok(Self { kind, count, storage: IndexStorage::Interleaved { _file: file, mmap }, skip: None })
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
        })
    }

    // ── Public size info ──────────────────────────────────────────────────────

    pub fn len(&self) -> usize { self.count }
    pub fn is_empty(&self) -> bool { self.count == 0 }

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
    /// Uses the skip index to narrow the binary search to exactly 1 OS page
    /// (SKIP_STRIDE = 512 entries = 4 KB), then fires a non-blocking prefetch
    /// so the kernel pipelines the disk read while the CPU works.
    fn lower_bound_0(&self, key: u64) -> usize {
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
    pub fn scan(&self, pat: &TriplePattern) -> TripleScan {
        let raw_pat = pattern_to_raw(*pat, self.kind);
        let (start, end) = self.range_for_pattern(&raw_pat);
        TripleScan { index: self, raw_pat, pos: start, end }
    }

    /// Compute the [start, end) range in the index that covers `raw`.
    ///
    /// With columnar storage the range-end scan reads only col0 or col01,
    /// which keeps the tertiary-key pages cold until triples are emitted.
    fn range_for_pattern(&self, raw: &[Option<u64>; 3]) -> (usize, usize) {
        match (raw[0], raw[1]) {
            (Some(k0), Some(k1)) => {
                let start = self.lower_bound_01(k0, k1);
                let mut end = start;
                while end < self.count {
                    let (c0, c1) = self.get_col01(end);
                    if c0 != k0 || c1 != k1 { break; }
                    end += 1;
                }
                (start, end)
            }
            (Some(k0), None) => {
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

pub struct TripleScan<'a> {
    index: &'a IndexFile,
    raw_pat: [Option<u64>; 3],
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
fn reorder(t: Triple, kind: IndexKind) -> [u64; 3] {
    match kind {
        IndexKind::Spo => [t.s, t.p, t.o],
        IndexKind::Pos => [t.p, t.o, t.s],
        IndexKind::Osp => [t.o, t.s, t.p],
    }
}

/// Convert index-ordered raw triple back to SPO.
#[inline]
fn reorder_back(raw: [u64; 3], kind: IndexKind) -> Triple {
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

    /// Helper: merge chunks into columnar files at `path`, or write empty columns if no chunks.
    fn merge_or_empty(chunks: &[PathBuf], path: &Path) -> io::Result<()> {
        if chunks.is_empty() {
            write_columnar_from_sorted(&[], path)
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
