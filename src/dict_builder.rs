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
/// At ~80 bytes per entry (key Box<str> + value u32 + HashMap overhead) this
/// caps the cache at roughly 320 MB, while covering all predicates and most
/// common objects in typical bio-RDF datasets.
const MAX_CACHE_ENTRIES: usize = 4_000_000;

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
    pub fn finish(mut self, out_path: &Path) -> io::Result<u32> {
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
        reader.read_exact(&mut buf)?;
        let remaining = u64::from_le_bytes(buf);
        Ok(Self { reader, remaining })
    }

    fn next_string(&mut self) -> io::Result<Option<String>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        let mut len_buf = [0u8; 4];
        self.reader.read_exact(&mut len_buf)?;
        let len = u32::from_le_bytes(len_buf) as usize;
        let mut bytes = vec![0u8; len];
        self.reader.read_exact(&mut bytes)?;
        self.remaining -= 1;
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

// ── k-way merge ───────────────────────────────────────────────────────────────

/// K-way merge of sorted string chunks → write `dict_sorted.bin`.
///
/// When `chunks.len() > MAX_FAN_IN` a **hierarchical merge** is performed
/// automatically: chunks are merged in batches into intermediate chunk files,
/// which are then merged in a final pass.  This bounds the number of
/// simultaneously open file descriptors to `MAX_FAN_IN + a few`, preventing
/// EMFILE (`Too many open files`) on large datasets such as UniProt.
///
/// Called by [`DictBuilder::finish`] for single-threaded loads, and directly
/// from the parallel loader after collecting chunks from all per-file threads.
pub(crate) fn merge_string_chunks(chunks: &[PathBuf], out_path: &Path) -> io::Result<u32> {
    if chunks.len() <= MAX_FAN_IN {
        return merge_string_chunks_direct(chunks, out_path);
    }

    // ── Hierarchical pass: merge groups of MAX_FAN_IN into intermediate chunks ──
    let mut intermediates: Vec<PathBuf> = Vec::new();
    for (i, batch) in chunks.chunks(MAX_FAN_IN).enumerate() {
        let tmp = append_suffix(out_path, &format!(".__merge_{:04}.tmp", i));
        merge_string_chunks_to_chunk(batch, &tmp)?;
        intermediates.push(tmp);
    }

    // ── Final pass: merge intermediate chunks → dict_sorted.bin ───────────────
    let result = if intermediates.len() <= MAX_FAN_IN {
        merge_string_chunks_direct(&intermediates, out_path)
    } else {
        // More than MAX_FAN_IN² input chunks (rare); recurse.
        merge_string_chunks(&intermediates, out_path)
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
/// removed.
fn merge_string_chunks_direct(chunks: &[PathBuf], out_path: &Path) -> io::Result<u32> {
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

    if count > u32::MAX as u64 {
        let _ = fs::remove_file(&strings_tmp);
        let _ = fs::remove_file(&offsets_tmp);
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "dictionary has {} unique terms, which exceeds the u32 limit ({})",
                count,
                u32::MAX
            ),
        ));
    }

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

    Ok(count as u32)
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
/// A bounded in-memory hot cache avoids repeated binary searches for
/// high-frequency terms such as predicates and `rdf:type` objects.
pub struct ReadonlyDict {
    mmap: Mmap,
    count: u64,
    /// Byte offset of the offsets section inside the mmap.
    offsets_start: usize,
    /// Bounded string→ID cache for hot terms.
    /// `RwLock` (not `RefCell`) makes `ReadonlyDict` `Sync` so it can be
    /// shared across query threads via `Arc<Store>`.
    cache: RwLock<FxHashMap<Box<str>, u32>>,
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
        })
    }

    /// Number of unique terms in the dictionary.
    #[inline]
    pub fn len(&self) -> u64 {
        self.count
    }

    /// Look up a string and return its ID, or `None` if not present.
    pub fn get_id(&self, s: &str) -> Option<u32> {
        // Fast path: hot cache (read lock, no exclusive contention with readers).
        {
            let cache = self.cache.read().unwrap();
            if let Some(&id) = cache.get(s) {
                return Some(id);
            }
        }
        // Slow path: binary search in mmap.
        let id = self.binary_search(s)?;
        // Cache result if the cache is not yet full.
        let mut cache = self.cache.write().unwrap();
        if cache.len() < MAX_CACHE_ENTRIES {
            cache.insert(s.into(), id);
        }
        Some(id)
    }

    /// Decode an ID to its string slice (zero-copy reference into the mmap).
    pub fn get_str(&self, id: u32) -> &str {
        let off = self.offset_of(id) as usize;
        let len = u32::from_le_bytes(self.mmap[off..off + 4].try_into().unwrap()) as usize;
        std::str::from_utf8(&self.mmap[off + 4..off + 4 + len])
            .expect("dict_sorted.bin contains invalid UTF-8")
    }

    // ── internal helpers ──────────────────────────────────────────────────────

    fn offset_of(&self, id: u32) -> u64 {
        let pos = self.offsets_start + id as usize * 8;
        u64::from_le_bytes(self.mmap[pos..pos + 8].try_into().unwrap())
    }

    fn binary_search(&self, target: &str) -> Option<u32> {
        if self.count == 0 {
            return None;
        }
        let mut lo: u64 = 0;
        let mut hi: u64 = self.count;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let s = self.get_str(mid as u32);
            match s.cmp(target) {
                std::cmp::Ordering::Equal => return Some(mid as u32),
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
        let tc = self.count as u32;
        w.write_all(&tc.to_le_bytes())?;

        for id in 0..tc {
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
        base_count: u32,
        /// Computed (expression-generated) terms: (vec of strings, string→id map).
        /// Single lock to avoid lock-ordering issues.
        computed: RwLock<(Vec<Box<str>>, FxHashMap<String, u32>)>,
    },
    Legacy {
        /// All terms: (id_to_str, str_to_id).
        data: RwLock<(Vec<Box<str>>, FxHashMap<String, u32>)>,
    },
}

impl QueryDict {
    /// Build a query dict backed by a memory-mapped `dict_sorted.bin`.
    pub fn from_mmap(base: ReadonlyDict) -> Self {
        let base_count = base.len() as u32;
        Self::Mmap {
            base,
            base_count,
            computed: RwLock::new((Vec::new(), FxHashMap::default())),
        }
    }

    /// Build a query dict from in-memory vectors (legacy / old-store path).
    pub fn from_legacy(id_to_str: Vec<Box<str>>, str_to_id: FxHashMap<String, u32>) -> Self {
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
    pub fn lookup(&self, s: &str) -> Option<u32> {
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
    pub fn decode(&self, id: u32) -> String {
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
    pub fn encode(&self, s: &str) -> u32 {
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
                let id = base_count + g.0.len() as u32;
                g.0.push(s.into());
                g.1.insert(s.to_string(), id);
                id
            }
            Self::Legacy { data } => {
                let mut g = data.write().unwrap();
                if let Some(&id) = g.1.get(s) {
                    return id;
                }
                let id = g.0.len() as u32;
                g.0.push(s.into());
                g.1.insert(s.to_string(), id);
                id
            }
        }
    }

    /// Pretty-print a term for human display (wraps IRIs in `<…>`).
    pub fn display(&self, id: u32) -> String {
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
