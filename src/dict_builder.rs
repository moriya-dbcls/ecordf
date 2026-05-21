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
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use memmap2::Mmap;
use rustc_hash::FxHashMap;

use crate::dict::KNOWN_PREFIXES;

// ── constants ─────────────────────────────────────────────────────────────────

const SORTED_MAGIC: &[u8; 8] = b"ESRT0001";

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
/// Uses two temporary files to avoid holding all offsets in RAM:
/// - `*.strings.tmp`: string bytes in sorted order
/// - `*.offsets.tmp`: u64 file-offset of each string (written incrementally)
///
/// After the merge both temporaries are concatenated into the final file and
/// removed.
fn merge_string_chunks(chunks: &[PathBuf], out_path: &Path) -> io::Result<u32> {
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
    cache: std::cell::RefCell<FxHashMap<Box<str>, u32>>,
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
            cache: std::cell::RefCell::new(FxHashMap::default()),
        })
    }

    /// Number of unique terms in the dictionary.
    #[inline]
    pub fn len(&self) -> u64 {
        self.count
    }

    /// Look up a string and return its ID, or `None` if not present.
    pub fn get_id(&self, s: &str) -> Option<u32> {
        // Fast path: hot cache.
        {
            let cache = self.cache.borrow();
            if let Some(&id) = cache.get(s) {
                return Some(id);
            }
        }
        // Slow path: binary search in mmap.
        let id = self.binary_search(s)?;
        // Cache result if the cache is not yet full.
        let mut cache = self.cache.borrow_mut();
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
