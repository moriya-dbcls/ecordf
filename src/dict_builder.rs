//! # Dictionary Builder — Two-Pass External-Sort Construction
//!
//! Solves the dictionary OOM problem for large datasets (billions of triples).
//! The in-memory `Dictionary` holds ALL unique strings simultaneously; for
//! UniProt-scale data (100 M+ unique IRIs/literals) that can exceed 10 GB.
//!
//! ## Two-phase approach
//!
//! **Phase 1 — [`DictBuilder`]**
//! Stream through input files collecting only strings.  Buffer them in a
//! memory-bounded chunk (default 200 MB), sort/dedup and flush to disk.
//! After all input: k-way merge of chunk files → `dict_sorted.bin`.
//! Peak RAM = one chunk buffer (≤ `dict_chunk_mb` MB).
//!
//! **Phase 2 — [`ReadonlyDict`]**
//! `dict_sorted.bin` is mmap-ed. ID lookups use binary search on the
//! offsets section. A bounded hot-cache (≤ 4 M entries) avoids repeated
//! searches for frequently occurring terms (predicates, common objects).
//! Peak RAM = hot cache (≤ ~400 MB) + OS page cache for mmap.
//!
//! ## dict_sorted.bin layout
//!
//! ```text
//! [magic: b"ESRT0001"  (8 bytes)]
//! [count: u64           (8 bytes)]   ← number of unique terms
//! [offsets_start: u64   (8 bytes)]   ← byte offset of offsets section
//! ── strings section (starts at byte 24) ──────────────────────────────
//! for each term in lexicographic order:
//!   [len: u32][bytes: len × u8]
//! ── offsets section (starts at offsets_start) ─────────────────────────
//! for each term i in 0..count:
//!   [u64]  absolute byte offset of term i's (len, bytes) in this file
//! ```
//!
//! IDs are assigned by sorted position: term 0 = lexicographically smallest.
//! The triple indexes built in Phase 2 use these same IDs, so the mapping
//! is fully consistent without ever loading all strings into a HashMap.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use memmap2::Mmap;
use rayon::prelude::*;
use rustc_hash::FxHashMap;

use crate::dict::KNOWN_PREFIXES;

// ── constants ─────────────────────────────────────────────────────────────────

const SORTED_MAGIC: &[u8; 8] = b"ESRT0001";

/// Maximum number of chunk files opened simultaneously in a single k-way merge pass.
///
/// When the total chunk count exceeds this, a **hierarchical merge** is used:
/// chunks are merged in batches of `MAX_FAN_IN` into intermediate chunk files,
/// which are then merged in a final pass.  This keeps open-file-descriptor
/// usage to at most `MAX_FAN_IN + handful` at any moment, avoiding EMFILE even
/// on systems with low fd limits (macOS default soft limit = 256).
const MAX_FAN_IN: usize = 64;

/// Strings in the hot cache after this many entries, new entries are dropped.
///
/// Memory cost per entry: ~88 bytes (Box<str> key + u64 value + FxHashMap overhead).
///
/// | Entries    | RAM    | Notes                                              |
/// |------------|--------|----------------------------------------------------|
/// | 4_000_000  | ~352 MB | original default                                  |
/// | 20_000_000 | ~1.7 GB | covers ~all predicates + hot subjects/objects      |
///
/// At 20 M entries, virtually all repeated IRI lookups during query execution
/// hit the cache, eliminating binary-search page faults in dict_sorted.bin for
/// hot terms.  The remaining misses (rare literals, long-tail IRIs) still fall
/// back to the mmap binary search.
const MAX_CACHE_ENTRIES: usize = 20_000_000;

// ══════════════════════════════════════════════════════════════════════════════
// DictBuilder — Phase 1
// ══════════════════════════════════════════════════════════════════════════════

/// External-sort based string collector for Phase 1 of two-pass loading.
///
/// Call [`DictBuilder::add`] for every RDF term encountered while streaming
/// through the input files.  When the in-memory buffer reaches
/// `max_buf_bytes`, it is sorted, deduped, and flushed to a temporary chunk
/// file on disk.  After all input call [`DictBuilder::finish`] to k-way merge
/// all chunks into a sorted `dict_sorted.bin` file.
pub struct DictBuilder {
    buf: Vec<String>,
    buf_bytes: usize,
    max_buf_bytes: usize,
    chunks: Vec<PathBuf>,
    tmp_dir: PathBuf,
}

impl DictBuilder {
    /// Create a new builder.
    ///
    /// `max_buf_bytes` — RAM budget for the in-memory string buffer before
    /// flushing a chunk to disk.  A value of `200 * 1024 * 1024` (200 MB) is
    /// a reasonable default for most machines.
    pub fn new(tmp_dir: &Path, max_buf_bytes: usize) -> io::Result<Self> {
        fs::create_dir_all(tmp_dir)?;
        Ok(Self {
            buf: Vec::new(),
            buf_bytes: 0,
            max_buf_bytes,
            chunks: Vec::new(),
            tmp_dir: tmp_dir.to_path_buf(),
        })
    }

    /// Add a string to the builder.
    ///
    /// Flushes a sorted, deduped chunk to disk automatically when the buffer
    /// exceeds `max_buf_bytes`.
    pub fn add(&mut self, s: &str) -> io::Result<()> {
        // 4-byte length prefix + string bytes (approximate)
        self.buf_bytes += 4 + s.len();
        self.buf.push(s.to_string());
        if self.buf_bytes >= self.max_buf_bytes {
            self.flush_chunk()?;
        }
        Ok(())
    }

    fn flush_chunk(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        self.buf.sort_unstable();
        self.buf.dedup();

        let path = self.tmp_dir.join(format!("sdc_{:06}.bin", self.chunks.len()));
        write_string_chunk(&self.buf, &path)?;
        self.chunks.push(path);
        self.buf.clear();
        self.buf_bytes = 0;
        Ok(())
    }

    /// Flush any remaining buffer and return all chunk file paths **without merging**.
    ///
    /// Used by the parallel loader: each thread calls this, then the main thread
    /// collects all chunk paths from all threads and passes them to
    /// [`merge_string_chunks`] for a single k-way merge.
    pub fn flush_and_return_chunks(mut self) -> io::Result<Vec<PathBuf>> {
        self.flush_chunk()?;
        Ok(self.chunks)
    }

    /// Finish: flush remainder, merge all chunks, write `dict_sorted.bin`.
    ///
    /// Returns the number of unique terms written.
    /// The caller is responsible for cleaning up the `tmp_dir`.
    pub fn finish(mut self, out_path: &Path) -> io::Result<u64> {
        self.flush_chunk()?;
        if self.chunks.is_empty() {
            write_empty_sorted_dict(out_path)?;
            return Ok(0);
        }
        merge_string_chunks(&self.chunks, out_path)
    }
}

// ── chunk file I/O ────────────────────────────────────────────────────────────

/// Write a sorted, deduped list of strings to a chunk file.
///
/// Format: `[count: u64]` followed by `count` × `[len: u32][bytes: len × u8]`.
fn write_string_chunk(strings: &[String], path: &Path) -> io::Result<()> {
    let file = File::create(path)?;
    let mut w = BufWriter::new(file);
    w.write_all(&(strings.len() as u64).to_le_bytes())?;
    for s in strings {
        let b = s.as_bytes();
        w.write_all(&(b.len() as u32).to_le_bytes())?;
        w.write_all(b)?;
    }
    w.flush()
}

struct StringChunkReader {
    reader: BufReader<File>,
    remaining: u64,
}

impl StringChunkReader {
    fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let mut reader = BufReader::new(file);
        let mut buf = [0u8; 8];
        // A chunk file that was being written when a previous run was interrupted
        // may be truncated.  Treat it as an empty chunk rather than propagating
        // the error — the valid strings in its prefix have already been written,
        // and the strings in the missing tail will be recovered from sibling
        // chunk files produced by other threads / prior runs.
        match reader.read_exact(&mut buf) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                return Ok(Self { reader, remaining: 0 });
            }
            Err(e) => return Err(e),
        }
        let remaining = u64::from_le_bytes(buf);
        Ok(Self { reader, remaining })
    }

    fn next_string(&mut self) -> io::Result<Option<String>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        let mut len_buf = [0u8; 4];
        // Truncated write: stop reading this chunk gracefully.
        match self.reader.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                self.remaining = 0;
                return Ok(None);
            }
            Err(e) => return Err(e),
        }
        let len = u32::from_le_bytes(len_buf) as usize;
        let mut bytes = vec![0u8; len];
        match self.reader.read_exact(&mut bytes) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                self.remaining = 0;
                return Ok(None);
            }
            Err(e) => return Err(e),
        }
        self.remaining -= 1;
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

// ── fd limit helpers ──────────────────────────────────────────────────────────

/// Read the process's soft limit on open file descriptors.
///
/// On Linux reads `/proc/self/limits` (no extra dependencies).
/// Falls back to 1024 on other platforms or if parsing fails.
pub(crate) fn fd_soft_limit() -> usize {
    #[cfg(target_os = "linux")]
    {
        if let Ok(text) = std::fs::read_to_string("/proc/self/limits") {
            // Line format: "Max open files          1024                 4096                 files"
            for line in text.lines() {
                if line.starts_with("Max open files") {
                    if let Some(n) = line.split_whitespace().nth(3).and_then(|s| s.parse().ok()) {
                        return n;
                    }
                }
            }
        }
    }
    1024 // conservative fallback for macOS and other platforms
}

/// Maximum number of merge batches to run concurrently, derived from the
/// process fd soft limit.
///
/// Each batch opens `MAX_FAN_IN` chunk readers plus a few output files.
/// We reserve 128 fds for mmap handles, logger, stdin/stdout/stderr, and
/// other internal use, then divide the remainder by the per-batch cost.
fn max_merge_concurrency() -> usize {
    const FDS_RESERVED: usize = 128;
    const FDS_PER_BATCH: usize = MAX_FAN_IN + 8; // readers + writer + BufWriter overhead
    let available = fd_soft_limit().saturating_sub(FDS_RESERVED);
    (available / FDS_PER_BATCH).max(1)
}

// ── k-way merge ───────────────────────────────────────────────────────────────

/// K-way merge of sorted string chunks → write `dict_sorted.bin`.
///
/// When `chunks.len() > MAX_FAN_IN` a **hierarchical merge** is performed
/// automatically: chunks are merged in batches into intermediate chunk files,
/// which are then merged in a final pass.
///
/// Each level's batches are processed **in parallel** using a dedicated Rayon
/// thread pool whose size is derived from the process fd soft limit, so the
/// total number of simultaneously open file descriptors stays within the OS
/// limit (EMFILE / `Too many open files` is avoided automatically).
///
/// Called by [`DictBuilder::finish`] for single-threaded loads, and directly
/// from the parallel loader after collecting chunks from all per-file threads.
/// Returns the number of unique terms written (u64; no upper-bound check).
pub(crate) fn merge_string_chunks(chunks: &[PathBuf], out_path: &Path) -> io::Result<u64> {
    let concurrency = max_merge_concurrency();
    eprintln!(
        "Merge: fd limit={}, batch concurrency={} (each batch opens {} files)",
        fd_soft_limit(), concurrency, MAX_FAN_IN,
    );
    merge_string_chunks_impl(chunks, out_path, 0, concurrency)
}

fn merge_string_chunks_impl(
    chunks: &[PathBuf],
    out_path: &Path,
    level: usize,
    concurrency: usize,
) -> io::Result<u64> {
    if chunks.len() <= MAX_FAN_IN {
        return merge_string_chunks_direct(chunks, out_path);
    }

    // ── Hierarchical pass: merge groups of MAX_FAN_IN into intermediate chunks ──
    //
    // Batches within a single level are independent (each writes to its own
    // unique temp file), so we process them in parallel.
    //
    // IMPORTANT: intermediate file names include the `level` counter so that
    // Level-0 files (`.__merge_L0_B…`) and Level-1 files (`.__merge_L1_B…`)
    // do not collide.
    let batches: Vec<(usize, &[PathBuf])> = chunks.chunks(MAX_FAN_IN).enumerate().collect();
    let n_batches = batches.len();

    // Pre-allocate the output paths in batch order so we can collect them
    // without any locking after the parallel work.
    let intermediates: Vec<PathBuf> = (0..n_batches)
        .map(|i| append_suffix(out_path, &format!(".__merge_L{}_B{:06}.tmp", level, i)))
        .collect();

    // Build a dedicated pool limited to `concurrency` threads so we never
    // open more than concurrency × MAX_FAN_IN file descriptors at once.
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(concurrency)
        .build()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

    let errors: Vec<io::Error> = pool.install(|| {
        batches
            .into_par_iter()
            .zip(intermediates.par_iter())
            .filter_map(|((_, batch), tmp)| {
                merge_string_chunks_to_chunk(batch, tmp).err()
            })
            .collect()
    });

    if let Some(e) = errors.into_iter().next() {
        // Clean up whatever was written before propagating.
        for p in &intermediates {
            let _ = fs::remove_file(p);
        }
        return Err(e);
    }

    // ── Next pass: merge intermediates (recurse if still too many) ────────────
    let result: io::Result<u64> = if intermediates.len() <= MAX_FAN_IN {
        merge_string_chunks_direct(&intermediates, out_path)
    } else {
        merge_string_chunks_impl(&intermediates, out_path, level + 1, concurrency)
    };

    for p in &intermediates {
        let _ = fs::remove_file(p);
    }

    result
}

/// Merge up to `MAX_FAN_IN` chunks directly into a `dict_sorted.bin` file.
///
/// Uses two temporary files to avoid holding all offsets in RAM:
/// - `*.strings.tmp`: string bytes in sorted order
/// - `*.offsets.tmp`: u64 file-offset of each string (written incrementally)
///
/// After the merge both temporaries are concatenated into the final file and
/// removed.  Returns the number of unique terms (u64; no upper-bound check).
fn merge_string_chunks_direct(chunks: &[PathBuf], out_path: &Path) -> io::Result<u64> {
    let mut readers: Vec<StringChunkReader> = chunks
        .iter()
        .map(|p| StringChunkReader::open(p))
        .collect::<io::Result<_>>()?;

    // Seed heap with the first string from each chunk.
    let mut heap: BinaryHeap<Reverse<(String, usize)>> = BinaryHeap::new();
    for (i, r) in readers.iter_mut().enumerate() {
        if let Some(s) = r.next_string()? {
            heap.push(Reverse((s, i)));
        }
    }

    // Temporary files: strings and offsets written in parallel.
    let strings_tmp = append_suffix(out_path, ".strings.tmp");
    let offsets_tmp = append_suffix(out_path, ".offsets.tmp");

    let mut strings_w = BufWriter::new(File::create(&strings_tmp)?);
    let mut offsets_w = BufWriter::new(File::create(&offsets_tmp)?);

    // Byte position in the *final* file. Header occupies bytes 0..24.
    let mut byte_pos: u64 = 24;
    let mut count: u64 = 0;
    let mut prev: Option<String> = None;

    while let Some(Reverse((s, i))) = heap.pop() {
        // Dedup across sorted chunks.
        let is_dup = prev.as_deref() == Some(s.as_str());

        // Always advance the chunk that supplied this string.
        if let Some(next) = readers[i].next_string()? {
            heap.push(Reverse((next, i)));
        }

        if is_dup {
            continue;
        }

        let b = s.as_bytes();
        offsets_w.write_all(&byte_pos.to_le_bytes())?;
        strings_w.write_all(&(b.len() as u32).to_le_bytes())?;
        strings_w.write_all(b)?;
        byte_pos += 4 + b.len() as u64;
        count += 1;

        prev = Some(s);
    }

    let offsets_start = byte_pos; // where the offsets section begins in final file
    strings_w.flush()?;
    offsets_w.flush()?;
    drop(strings_w);
    drop(offsets_w);

    // Assemble final file: header + strings + offsets.
    {
        let mut out = BufWriter::new(File::create(out_path)?);
        out.write_all(SORTED_MAGIC)?;
        out.write_all(&count.to_le_bytes())?;
        out.write_all(&offsets_start.to_le_bytes())?;

        // Append strings section.
        let mut sf = File::open(&strings_tmp)?;
        io::copy(&mut sf, &mut out)?;

        // Append offsets section.
        let mut of = File::open(&offsets_tmp)?;
        io::copy(&mut of, &mut out)?;

        out.flush()?;
    }

    let _ = fs::remove_file(&strings_tmp);
    let _ = fs::remove_file(&offsets_tmp);

    Ok(count)
}

fn write_empty_sorted_dict(out_path: &Path) -> io::Result<()> {
    let mut w = BufWriter::new(File::create(out_path)?);
    w.write_all(SORTED_MAGIC)?;
    w.write_all(&0u64.to_le_bytes())?;  // count = 0
    w.write_all(&24u64.to_le_bytes())?; // offsets_start = 24 (right after header)
    w.flush()
}

/// Merge up to `MAX_FAN_IN` chunk files into a **new chunk file** at `path`.
///
/// The output uses the same format as the input chunks
/// (`[count: u64][len: u32][bytes…]…`), sorted and deduped, so it can be fed
/// back into another merge pass.  The count field is back-patched after
/// writing all strings.
fn merge_string_chunks_to_chunk(chunks: &[PathBuf], path: &Path) -> io::Result<()> {
    let mut readers: Vec<StringChunkReader> = chunks
        .iter()
        .map(|p| StringChunkReader::open(p))
        .collect::<io::Result<_>>()?;

    let mut heap: BinaryHeap<Reverse<(String, usize)>> = BinaryHeap::new();
    for (i, r) in readers.iter_mut().enumerate() {
        if let Some(s) = r.next_string()? {
            heap.push(Reverse((s, i)));
        }
    }

    let file = File::create(path)?;
    let mut w = BufWriter::new(file);
    w.write_all(&0u64.to_le_bytes())?; // count placeholder — back-patched below

    let mut count: u64 = 0;
    let mut prev: Option<String> = None;

    while let Some(Reverse((s, i))) = heap.pop() {
        let is_dup = prev.as_deref() == Some(s.as_str());
        if let Some(next) = readers[i].next_string()? {
            heap.push(Reverse((next, i)));
        }
        if is_dup {
            continue;
        }
        let b = s.as_bytes();
        w.write_all(&(b.len() as u32).to_le_bytes())?;
        w.write_all(b)?;
        count += 1;
        prev = Some(s);
    }
    w.flush()?;
    drop(w);

    // Back-patch the count field at offset 0.
    let mut f = OpenOptions::new().write(true).open(path)?;
    f.seek(SeekFrom::Start(0))?;
    f.write_all(&count.to_le_bytes())?;

    Ok(())
}

/// Append a suffix to a path's file name (e.g. `foo.bin` → `foo.bin.strings.tmp`).
fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

// ══════════════════════════════════════════════════════════════════════════════
// ReadonlyDict — Phase 2
// ══════════════════════════════════════════════════════════════════════════════

/// Memory-mapped read-only dictionary backed by `dict_sorted.bin`.
///
/// Lookups use binary search on the mmap-ed offsets section.  The top
/// log₂(N) pivots accessed during binary search quickly become hot in the
/// OS page cache, so amortised lookup cost is low even for large dictionaries.
///
/// A bounded in-memory hot cache (up to `MAX_CACHE_ENTRIES` ≈ 20 M entries,
/// ~1.7 GB) avoids repeated binary searches for high-frequency terms such as
/// predicates, common subjects, and `rdf:type` objects.
pub struct ReadonlyDict {
    mmap: Mmap,
    count: u64,
    /// Byte offset of the offsets section inside the mmap.
    offsets_start: usize,
    /// Bounded string→ID cache for hot terms.
    /// `RwLock` (not `RefCell`) makes `ReadonlyDict` `Sync` so it can be
    /// shared across query threads via `Arc<Store>`.
    cache: RwLock<FxHashMap<Box<str>, u64>>,
    /// Optional blank-node TermId remapping (written by `ecordf reorder-bnodes`).
    /// When present, translates between old dictionary IDs and new sorted IDs.
    pub bnode_remap: Option<std::sync::Arc<crate::bnode_reorder::BnodeRemap>>,
}

impl ReadonlyDict {
    /// Open a `dict_sorted.bin` file.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let mmap = unsafe { Mmap::map(&file)? };

        if mmap.len() < 24 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "dict_sorted.bin is too small",
            ));
        }
        if &mmap[..8] != SORTED_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "dict_sorted.bin has invalid magic bytes",
            ));
        }
        let count = u64::from_le_bytes(mmap[8..16].try_into().unwrap());
        let offsets_start = u64::from_le_bytes(mmap[16..24].try_into().unwrap()) as usize;

        Ok(Self {
            mmap,
            count,
            offsets_start,
            cache: RwLock::new(FxHashMap::default()),
            bnode_remap: None,
        })
    }

    /// Number of unique terms in the dictionary.
    #[inline]
    pub fn len(&self) -> u64 {
        self.count
    }

    /// Look up a string and return its ID, or `None` if not present.
    ///
    /// If `bnode_remap` is loaded, blank-node IDs are translated from the
    /// dictionary's old position to the semantically reordered new ID.
    pub fn get_id(&self, s: &str) -> Option<u64> {
        // Fast path: hot cache (read lock, no exclusive contention with readers).
        {
            let cache = self.cache.read().unwrap();
            if let Some(&id) = cache.get(s) {
                return Some(id);
            }
        }
        // Slow path: binary search in mmap.
        let old_id = self.binary_search(s)?;
        // Apply blank-node remapping if active.
        let id = if let Some(ref remap) = self.bnode_remap {
            if remap.is_bnode(old_id) { remap.old_to_new(old_id) } else { old_id }
        } else {
            old_id
        };
        // Cache the final (possibly remapped) ID.
        let mut cache = self.cache.write().unwrap();
        if cache.len() < MAX_CACHE_ENTRIES {
            cache.insert(s.into(), id);
        }
        Some(id)
    }

    /// Decode an ID to its string slice (zero-copy reference into the mmap).
    ///
    /// If `bnode_remap` is loaded, translates the new (reordered) ID back to
    /// the dictionary's original position before lookup.
    pub fn get_str(&self, id: u64) -> &str {
        let dict_id = if let Some(ref remap) = self.bnode_remap {
            if remap.is_bnode(id) { remap.new_to_old(id) } else { id }
        } else {
            id
        };
        let off = self.offset_of(dict_id) as usize;
        let len = u32::from_le_bytes(self.mmap[off..off + 4].try_into().unwrap()) as usize;
        std::str::from_utf8(&self.mmap[off + 4..off + 4 + len])
            .expect("dict_sorted.bin contains invalid UTF-8")
    }

    // ── internal helpers ──────────────────────────────────────────────────────

    fn offset_of(&self, id: u64) -> u64 {
        let pos = self.offsets_start + id as usize * 8;
        u64::from_le_bytes(self.mmap[pos..pos + 8].try_into().unwrap())
    }

    fn binary_search(&self, target: &str) -> Option<u64> {
        if self.count == 0 {
            return None;
        }
        let mut lo: u64 = 0;
        let mut hi: u64 = self.count;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let s = self.get_str(mid);
            match s.cmp(target) {
                std::cmp::Ordering::Equal => return Some(mid),
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
            }
        }
        None
    }

    // ── dict.bin conversion ───────────────────────────────────────────────────

    /// Write `dict.bin` in the legacy [`Dictionary`] format.
    ///
    /// Streams through `dict_sorted.bin` sequentially — no full-RAM copy
    /// required.  The resulting `dict.bin` can be opened with
    /// [`Dictionary::load`] for query-time use.
    ///
    /// [`Dictionary`]: crate::dict::Dictionary
    pub fn write_legacy_dict(&self, dict_path: &Path) -> io::Result<()> {
        const NO_PREFIX: u16 = 0xFFFF;
        const MAGIC: &[u8; 8] = b"ECOD0001";

        let file = File::create(dict_path)?;
        let mut w = BufWriter::new(file);

        // Magic
        w.write_all(MAGIC)?;

        // Prefix table
        let pc = KNOWN_PREFIXES.len() as u32;
        w.write_all(&pc.to_le_bytes())?;
        for p in KNOWN_PREFIXES {
            let b = p.as_bytes();
            w.write_all(&(b.len() as u16).to_le_bytes())?;
            w.write_all(b)?;
        }

        // Terms — IDs are 0..count in sorted order, matching Phase 2 IDs.
        // Legacy dict.bin format stores term counts as u32; skip writing it
        // for dictionaries that exceed the u32 limit (>4.3 billion unique terms).
        if self.count > u32::MAX as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Cannot write legacy dict.bin: dictionary has {} unique terms, \
                     which exceeds the u32 limit ({}).  \
                     The store uses dict_sorted.bin for queries and does not require dict.bin.",
                    self.count, u32::MAX
                ),
            ));
        }
        let tc = self.count as u32;
        w.write_all(&tc.to_le_bytes())?;

        for id in 0..tc as u64 {
            let s = self.get_str(id);
            let mut found = false;
            for (i, p) in KNOWN_PREFIXES.iter().enumerate() {
                if s.starts_with(p) {
                    let local = &s[p.len()..];
                    let lb = local.as_bytes();
                    w.write_all(&(i as u16).to_le_bytes())?;
                    w.write_all(&(lb.len() as u32).to_le_bytes())?;
                    w.write_all(lb)?;
                    found = true;
                    break;
                }
            }
            if !found {
                w.write_all(&NO_PREFIX.to_le_bytes())?;
                let b = s.as_bytes();
                w.write_all(&(b.len() as u32).to_le_bytes())?;
                w.write_all(b)?;
            }
        }

        w.flush()
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// DictScanner — streaming sequential reader for dict_sorted.bin
// ══════════════════════════════════════════════════════════════════════════════

/// Sequential reader for the **strings section** of `dict_sorted.bin`.
///
/// Yields `(term_id, string)` pairs in ascending lexicographic order without
/// touching the random-access offsets section.  Used by the streaming Phase 2
/// join to build per-file `LocalMap`s in a single O(N) sequential pass over
/// the dictionary file, avoiding the random page-fault storm that binary-search
/// causes on a dictionary larger than available RAM.
///
/// # Usage
///
/// ```ignore
/// let mut scanner = DictScanner::open(&dict_sorted_path)?;
/// while let Some((id, s)) = scanner.next_entry()? {
///     // id is the lexicographic rank of s (0-based)
/// }
/// ```
pub struct DictScanner {
    reader: BufReader<File>,
    count: u64,
    next_id: u64,
}

impl DictScanner {
    /// Open `dict_sorted.bin` and position the reader at the first string.
    ///
    /// Uses an 8 MiB read buffer to amortise the cost of sequential I/O on
    /// a large dictionary file.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let mut reader = BufReader::with_capacity(8 * 1024 * 1024, file);
        let mut header = [0u8; 24];
        reader.read_exact(&mut header)?;
        if &header[..8] != SORTED_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "dict_sorted.bin: invalid magic bytes (expected ESRT0001)",
            ));
        }
        let count = u64::from_le_bytes(header[8..16].try_into().unwrap());
        // header[16..24] = offsets_start — unused here; strings begin at byte 24
        Ok(Self { reader, count, next_id: 0 })
    }

    /// Total number of unique terms in the dictionary.
    #[inline]
    pub fn count(&self) -> u64 {
        self.count
    }

    /// Read and return the next `(term_id, string)` pair, or `Ok(None)` at EOF.
    ///
    /// Reads exactly one `[len: u32][bytes: len × u8]` record from the strings
    /// section.  IDs are assigned in ascending lexicographic order, starting at 0.
    pub fn next_entry(&mut self) -> io::Result<Option<(u64, String)>> {
        if self.next_id >= self.count {
            return Ok(None);
        }
        let mut len_buf = [0u8; 4];
        self.reader.read_exact(&mut len_buf)?;
        let len = u32::from_le_bytes(len_buf) as usize;
        let mut bytes = vec![0u8; len];
        self.reader.read_exact(&mut bytes)?;
        let s = String::from_utf8(bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let id = self.next_id;
        self.next_id += 1;
        Ok(Some((id, s)))
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// LocalDictBuilder / LocalDict — per-file sub-dictionary for streaming Phase 2
// ══════════════════════════════════════════════════════════════════════════════

/// Magic bytes for the per-file local dictionary format.
const LOCAL_DICT_MAGIC: &[u8; 8] = b"ELOC0001";

/// Builds a `local_dict.bin` file during the streaming Phase 2 join.
///
/// Entries **must be added in lexicographic string order** (the same order as
/// the main dict scanner yields them).  After the join pass, call [`finish`]
/// to assemble and persist the final file.
///
/// ## File format (`ELOC0001`)
///
/// ```text
/// [magic:         u8×8  ]   = b"ELOC0001"
/// [count:         u64   ]   number of entries
/// [offsets_start: u64   ]   byte offset of offsets section
/// ── strings section (byte 24 …) ──────────────────────────────────────────
/// for each entry in lex order:
///   [global_id: u64]  [len: u32]  [bytes: len × u8]
/// ── offsets section (at offsets_start) ───────────────────────────────────
/// for each entry i: [u64]  byte offset of entry i's global_id field
/// ```
pub struct LocalDictBuilder {
    strings_w:   BufWriter<File>,
    offsets_w:   BufWriter<File>,
    strings_tmp: PathBuf,
    offsets_tmp: PathBuf,
    /// Current byte position in the assembled file (advances with each `add`).
    byte_pos: u64,
    count:    u64,
}

impl LocalDictBuilder {
    /// Create a new builder that writes temporary files under `tmp_dir`.
    ///
    /// `file_index` is used only to give the temp files unique names within a batch.
    pub fn new(tmp_dir: &Path, file_index: usize) -> io::Result<Self> {
        fs::create_dir_all(tmp_dir)?;
        let strings_tmp = tmp_dir.join(format!("ldstr_{:06}.tmp", file_index));
        let offsets_tmp = tmp_dir.join(format!("ldoff_{:06}.tmp", file_index));
        Ok(Self {
            strings_w:   BufWriter::new(File::create(&strings_tmp)?),
            offsets_w:   BufWriter::new(File::create(&offsets_tmp)?),
            strings_tmp,
            offsets_tmp,
            byte_pos: 24, // header occupies bytes 0..24
            count: 0,
        })
    }

    /// Record that `string` has global dictionary ID `global_id`.
    ///
    /// Must be called in lexicographic order (the same order the dict scanner
    /// yields strings, which is ascending lex order).
    #[inline]
    pub fn add(&mut self, global_id: u64, string: &str) -> io::Result<()> {
        let b = string.as_bytes();
        // Offsets file: one u64 per entry, pointing to entry's start in strings section.
        self.offsets_w.write_all(&self.byte_pos.to_le_bytes())?;
        // Strings file: [global_id: u64][len: u32][bytes].
        self.strings_w.write_all(&global_id.to_le_bytes())?;
        self.strings_w.write_all(&(b.len() as u32).to_le_bytes())?;
        self.strings_w.write_all(b)?;
        self.byte_pos += 8 + 4 + b.len() as u64;
        self.count += 1;
        Ok(())
    }

    /// Flush temporary files and assemble the final `local_dict.bin` at `out_path`.
    ///
    /// Returns the number of entries written.
    pub fn finish(mut self, out_path: &Path) -> io::Result<u64> {
        self.strings_w.flush()?;
        self.offsets_w.flush()?;
        drop(self.strings_w);
        drop(self.offsets_w);

        let offsets_start = self.byte_pos;
        let count = self.count;

        // Assemble: header + strings section + offsets section.
        {
            let mut out = BufWriter::new(File::create(out_path)?);
            out.write_all(LOCAL_DICT_MAGIC)?;
            out.write_all(&count.to_le_bytes())?;
            out.write_all(&offsets_start.to_le_bytes())?;
            let mut sf = File::open(&self.strings_tmp)?;
            io::copy(&mut sf, &mut out)?;
            let mut of = File::open(&self.offsets_tmp)?;
            io::copy(&mut of, &mut out)?;
            out.flush()?;
        }

        let _ = fs::remove_file(&self.strings_tmp);
        let _ = fs::remove_file(&self.offsets_tmp);
        Ok(count)
    }
}

/// Memory-mapped per-file sub-dictionary for streaming Phase 2b.
///
/// Supports O(log N) string → global-ID lookup via binary search over the
/// mmap-ed offsets section.  Built by [`LocalDictBuilder`] during the join step.
///
/// Unlike [`ReadonlyDict`] (which assigns IDs by sorted position), `LocalDict`
/// stores the actual global dictionary IDs inline in each entry.
pub struct LocalDict {
    mmap:          Mmap,
    count:         u64,
    offsets_start: usize,
}

impl LocalDict {
    /// Open a `local_dict.bin` file produced by [`LocalDictBuilder::finish`].
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let mmap = unsafe { Mmap::map(&file)? };
        if mmap.len() < 24 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData, "local_dict.bin too small"));
        }
        if &mmap[..8] != LOCAL_DICT_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "local_dict.bin: invalid magic bytes (expected ELOC0001)"));
        }
        let count         = u64::from_le_bytes(mmap[8..16].try_into().unwrap());
        let offsets_start = u64::from_le_bytes(mmap[16..24].try_into().unwrap()) as usize;
        Ok(Self { mmap, count, offsets_start })
    }

    /// Look up a string and return its global dictionary ID, or `None` if absent.
    pub fn get_id(&self, s: &str) -> Option<u64> {
        if self.count == 0 { return None; }
        let mut lo: u64 = 0;
        let mut hi: u64 = self.count;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let (global_id, entry_str) = self.entry_at(mid);
            match entry_str.cmp(s) {
                std::cmp::Ordering::Equal   => return Some(global_id),
                std::cmp::Ordering::Less    => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
            }
        }
        None
    }

    fn entry_at(&self, id: u64) -> (u64, &str) {
        let off_pos = self.offsets_start + id as usize * 8;
        let off = u64::from_le_bytes(
            self.mmap[off_pos..off_pos + 8].try_into().unwrap()
        ) as usize;
        let global_id = u64::from_le_bytes(self.mmap[off..off + 8].try_into().unwrap());
        let len       = u32::from_le_bytes(self.mmap[off + 8..off + 12].try_into().unwrap()) as usize;
        let s = std::str::from_utf8(&self.mmap[off + 12..off + 12 + len])
            .expect("local_dict.bin contains invalid UTF-8");
        (global_id, s)
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// QueryDict — query-time dictionary (mmap-backed or legacy in-memory)
// ══════════════════════════════════════════════════════════════════════════════

/// Unified query-time dictionary.
///
/// ## Mmap mode (new stores)
///
/// The immutable base terms are served directly from the mmap-ed
/// `dict_sorted.bin` via binary search + bounded hot cache.  Peak RAM =
/// cache (~400 MB) + OS page cache pages actually touched.
///
/// Expression-generated terms (e.g. from `CONCAT`, `UCASE`, `STR`) that do
/// not appear in the base dictionary receive IDs starting at `base_count`.
/// They live in the in-memory `computed` table and are ephemeral (discarded
/// when the `Store` is dropped).
///
/// ## Legacy mode (old stores without `dict_sorted.bin`)
///
/// All terms are held in a fully in-memory hash table, matching the old
/// `Dictionary` behaviour.  This path is taken automatically when `open()`
/// cannot find `dict_sorted.bin` in the store directory.
pub enum QueryDict {
    Mmap {
        base: ReadonlyDict,
        base_count: u64,
        /// Computed (expression-generated) terms: (vec of strings, string→id map).
        /// Single lock to avoid lock-ordering issues.
        computed: RwLock<(Vec<Box<str>>, FxHashMap<String, u64>)>,
    },
    Legacy {
        /// All terms: (id_to_str, str_to_id).
        data: RwLock<(Vec<Box<str>>, FxHashMap<String, u64>)>,
    },
}

impl QueryDict {
    /// Build a query dict backed by a memory-mapped `dict_sorted.bin`.
    pub fn from_mmap(base: ReadonlyDict) -> Self {
        let base_count = base.len();
        Self::Mmap {
            base,
            base_count,
            computed: RwLock::new((Vec::new(), FxHashMap::default())),
        }
    }

    /// Build a query dict from in-memory vectors (legacy / old-store path).
    pub fn from_legacy(id_to_str: Vec<Box<str>>, str_to_id: FxHashMap<String, u64>) -> Self {
        Self::Legacy {
            data: RwLock::new((id_to_str, str_to_id)),
        }
    }

    /// Number of base terms (not counting ephemeral computed terms).
    pub fn len(&self) -> usize {
        match self {
            Self::Mmap { base, .. } => base.len() as usize,
            Self::Legacy { data } => data.read().unwrap().0.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Look up a string without inserting. Returns `None` if absent.
    pub fn lookup(&self, s: &str) -> Option<u64> {
        match self {
            Self::Mmap { base, computed, .. } => {
                base.get_id(s)
                    .or_else(|| computed.read().unwrap().1.get(s).copied())
            }
            Self::Legacy { data } => {
                data.read().unwrap().1.get(s).copied()
            }
        }
    }

    /// Decode a term ID to its string representation.
    pub fn decode(&self, id: u64) -> String {
        match self {
            Self::Mmap { base, base_count, computed } => {
                if id < *base_count {
                    base.get_str(id).to_string()
                } else {
                    let idx = (id - base_count) as usize;
                    computed.read().unwrap().0
                        .get(idx)
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| format!("<unknown-term:{}>", id))
                }
            }
            Self::Legacy { data } => {
                data.read().unwrap().0[id as usize].to_string()
            }
        }
    }

    /// Get or assign an integer ID for the given string.
    ///
    /// Base-dictionary terms get the same ID they were assigned during build.
    /// New expression-generated terms (absent from the base dict) get IDs
    /// starting at `base_count`; these IDs are ephemeral and session-local.
    pub fn encode(&self, s: &str) -> u64 {
        // Fast path: already known.
        if let Some(id) = self.lookup(s) {
            return id;
        }
        // Slow path: insert into the computed / data table.
        match self {
            Self::Mmap { base_count, computed, .. } => {
                let mut g = computed.write().unwrap();
                // Re-check after acquiring write lock.
                if let Some(&id) = g.1.get(s) {
                    return id;
                }
                let id = base_count + g.0.len() as u64;
                g.0.push(s.into());
                g.1.insert(s.to_string(), id);
                id
            }
            Self::Legacy { data } => {
                let mut g = data.write().unwrap();
                if let Some(&id) = g.1.get(s) {
                    return id;
                }
                let id = g.0.len() as u64;
                g.0.push(s.into());
                g.1.insert(s.to_string(), id);
                id
            }
        }
    }

    /// Pretty-print a term for human display (wraps IRIs in `<…>`).
    pub fn display(&self, id: u64) -> String {
        let s = self.decode(id);
        if s.starts_with("http://") || s.starts_with("https://") {
            format!("<{}>", s)
        } else if s.starts_with('"') {
            s
        } else {
            s
        }
    }
}
