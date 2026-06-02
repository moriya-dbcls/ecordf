//! # Delta-encoded column files (ECOCOL02)
//!
//! Companion to the raw columnar format (`ECOCOL01`).  Divides the u64 value
//! stream into blocks of 256 and stores each block as a base value plus
//! minimum-width deltas, achieving significant compression for sorted data:
//!
//! | Column                 | Typical delta     | Encoding  | Compression |
//! |------------------------|-------------------|-----------|-------------|
//! | POS c0 (predicate IDs) | 0 within predicate | ALL_SAME  |  256×       |
//! | SPO c0 (subject IDs)   | 1–100             | U8_DELTA  |    8×       |
//! | POS c1 (object IDs)    | 1–1000            | U16_DELTA |    4×       |
//! | c2 (random IDs)        | large             | U64_RAW   |    1×       |
//!
//! ## File format
//!
//! ```text
//! offset  0: magic             [u8; 8]  = b"ECOCOL02"
//! offset  8: count             u64      total number of u64 values
//! offset 16: block_count       u64      number of compressed blocks
//! offset 24: block_idx_offset  u64      byte offset of block index section
//! offset 32: ── compressed blocks ────────────────────────────────────────
//!   each block:
//!     encoding: u8     (0=ALL_SAME, 1=U8, 2=U16, 3=U32, 4=U64)
//!     count_m1: u8     (number of values − 1; so 0=1, 255=256)
//!     base_val: u64    (minimum/reference value; all other values = base+delta)
//!     deltas[0..count_m1] × (0|1|2|4|8) bytes depending on encoding
//!        (absent for ALL_SAME)
//!   ──────────────────────────────────────────────────────────────────────
//! at block_idx_offset:
//!   [(first_value: u64, byte_offset: u64) × block_count]  ← block index
//! ```
//!
//! ## Usage
//!
//! **Build:** call [`encode_column`] during `ecordf build --delta-encode` or
//! the standalone `ecordf compress-cols --dir <store>` command.
//!
//! **Read:** open via [`DeltaColFile::open`].  The block index (~16 bytes ×
//! block_count) is loaded into RAM; compressed data stays in the mmap.

use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

use memmap2::Mmap;

// ── Constants ─────────────────────────────────────────────────────────────────

pub const DELTA_MAGIC: &[u8; 8] = b"ECOCOL02";
pub const DELTA_HDR: usize = 32; // magic(8)+count(8)+block_count(8)+idx_offset(8)
pub const DELTA_BLOCK_SIZE: usize = 256;

// Encoding tags stored in the first byte of each block.
const ENC_ALL_SAME: u8 = 0;
const ENC_U8:       u8 = 1;
const ENC_U16:      u8 = 2;
const ENC_U32:      u8 = 3;
const ENC_U64:      u8 = 4;

// ── Encoder ───────────────────────────────────────────────────────────────────

/// Encode a sorted slice of u64 values and write them to `path` in ECOCOL02 format.
///
/// `values` must be a sorted (non-strictly) sequence.
pub fn encode_column(values: &[u64], path: &Path) -> io::Result<()> {
    let count = values.len();
    let block_count = count.div_ceil(DELTA_BLOCK_SIZE);

    let f = File::create(path)?;
    let mut w = BufWriter::with_capacity(8 * 1024 * 1024, f);

    // Placeholder header — filled in below.
    w.write_all(DELTA_MAGIC)?;
    w.write_all(&(count as u64).to_le_bytes())?;
    w.write_all(&(block_count as u64).to_le_bytes())?;
    w.write_all(&0u64.to_le_bytes())?; // block_idx_offset placeholder

    // Track (first_value, byte_offset) for each block.
    let mut block_index: Vec<(u64, u64)> = Vec::with_capacity(block_count);
    let mut byte_pos: u64 = DELTA_HDR as u64;

    for chunk in values.chunks(DELTA_BLOCK_SIZE) {
        let base = chunk[0];
        let n = chunk.len();

        block_index.push((base, byte_pos));

        // Compute max delta to choose encoding width.
        let max_delta = if n == 1 { 0 } else {
            chunk[1..].iter().fold(0u64, |acc, &v| acc.max(v - base))
        };

        let enc = if max_delta == 0 {
            ENC_ALL_SAME
        } else if max_delta <= u8::MAX as u64 {
            ENC_U8
        } else if max_delta <= u16::MAX as u64 {
            ENC_U16
        } else if max_delta <= u32::MAX as u64 {
            ENC_U32
        } else {
            ENC_U64
        };

        let count_m1 = (n - 1) as u8;

        w.write_all(&[enc, count_m1])?;
        w.write_all(&base.to_le_bytes())?;

        match enc {
            ENC_ALL_SAME => {
                // No deltas needed.
                byte_pos += 2 + 8;
            }
            ENC_U8 => {
                for &v in &chunk[1..] {
                    w.write_all(&[(v - base) as u8])?;
                }
                byte_pos += 2 + 8 + (n - 1) as u64;
            }
            ENC_U16 => {
                for &v in &chunk[1..] {
                    w.write_all(&((v - base) as u16).to_le_bytes())?;
                }
                byte_pos += 2 + 8 + (n - 1) as u64 * 2;
            }
            ENC_U32 => {
                for &v in &chunk[1..] {
                    w.write_all(&((v - base) as u32).to_le_bytes())?;
                }
                byte_pos += 2 + 8 + (n - 1) as u64 * 4;
            }
            _ => {
                // ENC_U64: raw values
                for &v in &chunk[1..] {
                    w.write_all(&v.to_le_bytes())?;
                }
                byte_pos += 2 + 8 + (n - 1) as u64 * 8;
            }
        }
    }

    // Write the block index.
    let idx_offset = byte_pos;
    for (first_val, offset) in &block_index {
        w.write_all(&first_val.to_le_bytes())?;
        w.write_all(&offset.to_le_bytes())?;
    }

    // Back-patch block_idx_offset in the header.
    let mut f = w.into_inner().map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
    f.seek(SeekFrom::Start(24))?;
    f.write_all(&idx_offset.to_le_bytes())?;
    f.flush()?;

    Ok(())
}

/// Decode one compressed block starting at `data[offset..]`.
///
/// Writes decoded u64 values into `out` and returns the number of bytes consumed.
fn decode_block(data: &[u8], offset: usize, out: &mut Vec<u64>) -> usize {
    let enc      = data[offset];
    let count_m1 = data[offset + 1] as usize;
    let n        = count_m1 + 1;
    let base     = u64::from_le_bytes(data[offset+2..offset+10].try_into().unwrap());
    let body     = &data[offset + 10..];

    out.clear();

    match enc {
        ENC_ALL_SAME => {
            out.resize(n, base);
            2 + 8
        }
        ENC_U8 => {
            out.push(base);
            for i in 0..count_m1 {
                out.push(base + body[i] as u64);
            }
            2 + 8 + count_m1
        }
        ENC_U16 => {
            out.push(base);
            for i in 0..count_m1 {
                let delta = u16::from_le_bytes(body[i*2..i*2+2].try_into().unwrap()) as u64;
                out.push(base + delta);
            }
            2 + 8 + count_m1 * 2
        }
        ENC_U32 => {
            out.push(base);
            for i in 0..count_m1 {
                let delta = u32::from_le_bytes(body[i*4..i*4+4].try_into().unwrap()) as u64;
                out.push(base + delta);
            }
            2 + 8 + count_m1 * 4
        }
        _ => {
            // ENC_U64: first value is base, rest are raw u64
            out.push(base);
            for i in 0..count_m1 {
                out.push(u64::from_le_bytes(body[i*8..i*8+8].try_into().unwrap()));
            }
            2 + 8 + count_m1 * 8
        }
    }
}

// ── DeltaColFile ──────────────────────────────────────────────────────────────

/// A memory-mapped delta-encoded column file with a loaded block index.
///
/// Provides O(log block_count) access to any value and efficient sequential
/// iteration via [`DeltaColIter`].
///
/// ## Variable-sized blocks
///
/// [`encode_column_pred_aligned`] may produce blocks smaller than
/// `DELTA_BLOCK_SIZE` at predicate boundaries.  `start_positions[i]` holds the
/// cumulative logical start position of block `i`, computed once at open time
/// by scanning each block's `count_m1` byte.  All random-access methods
/// (`get`, `lower_bound`, `iter_from`) use a binary search on `start_positions`
/// instead of the old `pos / DELTA_BLOCK_SIZE` division, so they work correctly
/// regardless of block size.
pub struct DeltaColFile {
    /// Keep both the `File` handle and `Mmap` alive together.
    _file: File,
    mmap:  Mmap,
    /// Total number of u64 values in the column.
    pub count: usize,
    /// `(first_value, byte_offset_of_block)` for each block.
    block_index: Vec<(u64, u64)>,
    /// `start_positions[i]` = logical index of the first entry in block `i`.
    /// Derived from `count_m1` bytes at open time; supports variable block sizes.
    start_positions: Vec<usize>,
}

impl DeltaColFile {
    /// Open and memory-map a delta-encoded column file.
    ///
    /// Loads the block index and derives `start_positions` (one sequential scan
    /// of block headers, ≈ 2 bytes per block).
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let mmap = unsafe { Mmap::map(&file)? };

        if mmap.len() < DELTA_HDR {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "delta col too small"));
        }
        if &mmap[0..8] != DELTA_MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad delta col magic"));
        }

        let count       = u64::from_le_bytes(mmap[8..16].try_into().unwrap()) as usize;
        let block_count = u64::from_le_bytes(mmap[16..24].try_into().unwrap()) as usize;
        let idx_offset  = u64::from_le_bytes(mmap[24..32].try_into().unwrap()) as usize;

        if mmap.len() < idx_offset + block_count * 16 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "truncated block index"));
        }

        let mut block_index = Vec::with_capacity(block_count);
        for i in 0..block_count {
            let base_off = idx_offset + i * 16;
            let first_val = u64::from_le_bytes(mmap[base_off..base_off+8].try_into().unwrap());
            let byte_off  = u64::from_le_bytes(mmap[base_off+8..base_off+16].try_into().unwrap());
            block_index.push((first_val, byte_off));
        }

        // Derive start_positions from count_m1 byte of each block header.
        // Block header layout: enc(1) + count_m1(1) + base_val(8) + deltas…
        // count_m1 = (entries_in_block - 1), so entries = count_m1 + 1.
        let mut start_positions = Vec::with_capacity(block_count);
        let mut pos = 0usize;
        for &(_, byte_off) in &block_index {
            start_positions.push(pos);
            let count_m1 = mmap[byte_off as usize + 1] as usize;
            pos += count_m1 + 1;
        }

        Ok(Self { _file: file, mmap, count, block_index, start_positions })
    }

    /// Decompress the block at `block_idx` into `out`.
    pub fn decompress_block(&self, block_idx: usize, out: &mut Vec<u64>) {
        let offset = self.block_index[block_idx].1 as usize;
        decode_block(&self.mmap, offset, out);
    }

    /// Find the block index that contains logical position `pos`.
    ///
    /// Uses binary search on `start_positions` — works for both fixed-size
    /// (standard `encode_column`) and variable-size (pred-aligned) blocks.
    #[inline]
    fn block_for_pos(&self, pos: usize) -> (usize, usize) {
        // block_idx = last i where start_positions[i] <= pos
        let block_idx = self.start_positions.partition_point(|&sp| sp <= pos)
            .saturating_sub(1);
        let block_offset = pos - self.start_positions[block_idx];
        (block_idx, block_offset)
    }

    /// Return the value at logical position `pos`.
    ///
    /// O(BLOCK_SIZE) decompression work per call.  Use [`DeltaColIter`] for
    /// sequential scanning to amortise decompression across the block.
    pub fn get(&self, pos: usize) -> u64 {
        let (block_idx, block_offset) = self.block_for_pos(pos);
        let mut buf = Vec::with_capacity(DELTA_BLOCK_SIZE);
        self.decompress_block(block_idx, &mut buf);
        buf[block_offset]
    }

    /// Return the index of the first block whose `first_value > target`.
    /// Used to narrow the search range for `lower_bound`.
    pub fn block_upper_bound(&self, target: u64) -> usize {
        self.block_index.partition_point(|&(fv, _)| fv <= target)
    }

    /// Find the position of the first value `>= target` (lower bound).
    ///
    /// Uses the block index for O(log block_count) narrowing, then linear
    /// scan within at most one block.
    pub fn lower_bound(&self, target: u64) -> usize {
        if self.block_index.is_empty() {
            return 0;
        }
        // Last block whose first_value <= target.
        let bub = self.block_upper_bound(target);
        let block_start = if bub == 0 { 0 } else { bub - 1 };

        let mut buf = Vec::with_capacity(DELTA_BLOCK_SIZE);
        self.decompress_block(block_start, &mut buf);

        // base_pos: logical start of this block (works for variable-size blocks)
        let base_pos = self.start_positions[block_start];
        for (i, &v) in buf.iter().enumerate() {
            if v >= target {
                return base_pos + i;
            }
        }
        // Target is in the next block or beyond the end.
        if block_start + 1 < self.start_positions.len() {
            self.start_positions[block_start + 1]
        } else {
            self.count
        }
    }

    /// Create a sequential iterator starting at logical position `start_pos`.
    pub fn iter_from(&self, start_pos: usize) -> DeltaColIter<'_> {
        DeltaColIter::new(self, start_pos)
    }
}

// ── Sequential iterator ────────────────────────────────────────────────────────

/// Iterator over values in a [`DeltaColFile`], decompressing one block at a time.
pub struct DeltaColIter<'a> {
    col:       &'a DeltaColFile,
    block_idx: usize,
    buf:       Vec<u64>,
    buf_pos:   usize,
    remaining: usize,
}

impl<'a> DeltaColIter<'a> {
    fn new(col: &'a DeltaColFile, start_pos: usize) -> Self {
        // Use block_for_pos so variable-size blocks are handled correctly.
        let (block_idx, buf_pos) = if start_pos < col.count && !col.start_positions.is_empty() {
            col.block_for_pos(start_pos)
        } else {
            (col.block_index.len(), 0)
        };

        let mut buf = Vec::with_capacity(DELTA_BLOCK_SIZE);
        if block_idx < col.block_index.len() {
            col.decompress_block(block_idx, &mut buf);
        }

        let remaining = col.count.saturating_sub(start_pos);
        Self { col, block_idx, buf, buf_pos, remaining }
    }
}

impl<'a> Iterator for DeltaColIter<'a> {
    type Item = u64;

    #[inline]
    fn next(&mut self) -> Option<u64> {
        if self.remaining == 0 {
            return None;
        }
        if self.buf_pos >= self.buf.len() {
            self.block_idx += 1;
            if self.block_idx >= self.col.block_index.len() {
                return None;
            }
            self.col.decompress_block(self.block_idx, &mut self.buf);
            self.buf_pos = 0;
        }
        let v = self.buf[self.buf_pos];
        self.buf_pos += 1;
        self.remaining -= 1;
        Some(v)
    }
}

/// Encode a sorted slice of u64 values with **predicate-boundary-aligned blocks**.
///
/// Identical to [`encode_column`] except that a new delta block is forced at
/// each index listed in `boundaries`.  This guarantees that no compressed block
/// straddles two predicates, maximising the compression ratio for POS c1 (objects):
///
/// ```text
/// Without alignment: block may span pred_A tail + pred_B head
///   → max_delta covers both predicates' object ranges → wider delta → larger blocks
///
/// With alignment: every block is entirely within one predicate's object range
///   → max_delta is the object range of that predicate only → smaller delta → U8/U16 encoding
/// ```
///
/// `boundaries`: sorted list of entry indices where a new block must start.
///   Typically the `(lo, hi)` values from the `PredicateIndex` (each `lo > 0` is a boundary).
///   Entry 0 is implicit and need not be included.
///
/// The compression gain is most significant for POS c1 (objects ordered within
/// each predicate's range): without alignment the occasional large jump when the
/// predicate changes forces U64 raw encoding for those blocks.
pub fn encode_column_pred_aligned(
    values: &[u64],
    boundaries: &[usize],
    path: &Path,
) -> io::Result<()> {
    let count = values.len();
    if count == 0 {
        return encode_column(values, path);
    }

    // Build a sorted, deduped set of forced block-start positions.
    let mut force_starts: Vec<usize> = boundaries.iter()
        .copied()
        .filter(|&b| b > 0 && b < count)
        .collect();
    force_starts.sort_unstable();
    force_starts.dedup();

    // We'll accumulate (block_first_value, byte_offset) as we write.
    // Count of blocks is unknown upfront; reserve a placeholder for block_idx_offset.
    let f = File::create(path)?;
    let mut w = BufWriter::with_capacity(8 * 1024 * 1024, f);

    w.write_all(DELTA_MAGIC)?;
    w.write_all(&(count as u64).to_le_bytes())?;
    w.write_all(&0u64.to_le_bytes())?; // block_count placeholder
    w.write_all(&0u64.to_le_bytes())?; // block_idx_offset placeholder

    let mut block_index: Vec<(u64, u64)> = Vec::new();
    let mut byte_pos: u64 = DELTA_HDR as u64;

    let mut pos = 0usize;
    let mut force_idx = 0usize; // index into force_starts

    while pos < count {
        let base = values[pos];

        // Determine the end of this block: min(pos + DELTA_BLOCK_SIZE, next_boundary, count)
        let natural_end = (pos + DELTA_BLOCK_SIZE).min(count);
        let forced_end = if force_idx < force_starts.len() && force_starts[force_idx] > pos {
            force_starts[force_idx].min(natural_end)
        } else {
            natural_end
        };
        // Advance force_idx past any boundaries we are consuming.
        while force_idx < force_starts.len() && force_starts[force_idx] <= forced_end {
            force_idx += 1;
        }

        let chunk = &values[pos..forced_end];
        let n = chunk.len();

        block_index.push((base, byte_pos));

        let max_delta = if n == 1 { 0 } else {
            chunk[1..].iter().fold(0u64, |acc, &v| acc.max(v - base))
        };

        let enc = if max_delta == 0 {
            ENC_ALL_SAME
        } else if max_delta <= u8::MAX as u64 {
            ENC_U8
        } else if max_delta <= u16::MAX as u64 {
            ENC_U16
        } else if max_delta <= u32::MAX as u64 {
            ENC_U32
        } else {
            ENC_U64
        };

        let count_m1 = (n - 1) as u8;
        w.write_all(&[enc, count_m1])?;
        w.write_all(&base.to_le_bytes())?;

        match enc {
            ENC_ALL_SAME => { byte_pos += 2 + 8; }
            ENC_U8 => {
                for &v in &chunk[1..] { w.write_all(&[(v - base) as u8])?; }
                byte_pos += 2 + 8 + (n - 1) as u64;
            }
            ENC_U16 => {
                for &v in &chunk[1..] { w.write_all(&((v - base) as u16).to_le_bytes())?; }
                byte_pos += 2 + 8 + (n - 1) as u64 * 2;
            }
            ENC_U32 => {
                for &v in &chunk[1..] { w.write_all(&((v - base) as u32).to_le_bytes())?; }
                byte_pos += 2 + 8 + (n - 1) as u64 * 4;
            }
            _ => {
                for &v in &chunk[1..] { w.write_all(&v.to_le_bytes())?; }
                byte_pos += 2 + 8 + (n - 1) as u64 * 8;
            }
        }

        pos = forced_end;
    }

    // Write block index.
    let idx_offset = byte_pos;
    let block_count = block_index.len();
    for (first_val, offset) in &block_index {
        w.write_all(&first_val.to_le_bytes())?;
        w.write_all(&offset.to_le_bytes())?;
    }

    // Back-patch header: block_count at offset 16, block_idx_offset at offset 24.
    let mut f = w.into_inner().map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
    f.seek(SeekFrom::Start(16))?;
    f.write_all(&(block_count as u64).to_le_bytes())?;
    f.write_all(&idx_offset.to_le_bytes())?;
    f.flush()?;

    Ok(())
}

// ── Suffix helper ─────────────────────────────────────────────────────────────

/// Return the `.dz` variant path: `spo.c0` → `spo.c0.dz`.
pub fn delta_path(col_path: &Path) -> std::path::PathBuf {
    let name = col_path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("col");
    col_path.with_file_name(format!("{}.dz", name))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn round_trip(values: &[u64]) -> Vec<u64> {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        encode_column(values, tmp.path()).unwrap();
        let col = DeltaColFile::open(tmp.path()).unwrap();
        assert_eq!(col.count, values.len());
        (0..values.len()).map(|i| col.get(i)).collect()
    }

    #[test]
    fn test_all_same() {
        let v: Vec<u64> = vec![42u64; 512];
        assert_eq!(round_trip(&v), v);
    }

    #[test]
    fn test_u8_delta() {
        let v: Vec<u64> = (0u64..512).map(|i| 1000 + i).collect();
        assert_eq!(round_trip(&v), v);
    }

    #[test]
    fn test_u16_delta() {
        let v: Vec<u64> = (0u64..512).map(|i| 1000 + i * 300).collect();
        assert_eq!(round_trip(&v), v);
    }

    #[test]
    fn test_mixed() {
        // Simulate POS c0: long runs of same predicate ID, then jumps.
        let mut v = Vec::new();
        for pred in [100u64, 200, 300] {
            for _ in 0..300 {
                v.push(pred);
            }
        }
        assert_eq!(round_trip(&v), v);
    }

    #[test]
    fn test_lower_bound() {
        let v: Vec<u64> = (0u64..1024).map(|i| i * 2).collect(); // even numbers
        let tmp = tempfile::NamedTempFile::new().unwrap();
        encode_column(&v, tmp.path()).unwrap();
        let col = DeltaColFile::open(tmp.path()).unwrap();
        // lower_bound(5) → first value >= 5 → value 6 at position 3
        assert_eq!(col.get(col.lower_bound(5)), 6);
        // lower_bound(0) → 0 at position 0
        assert_eq!(col.lower_bound(0), 0);
    }

    #[test]
    fn test_iter_from() {
        let v: Vec<u64> = (0u64..600).collect();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        encode_column(&v, tmp.path()).unwrap();
        let col = DeltaColFile::open(tmp.path()).unwrap();
        let result: Vec<u64> = col.iter_from(250).take(10).collect();
        assert_eq!(result, (250u64..260).collect::<Vec<_>>());
    }
}
