//! # Delta-encoded column files (ECOCOL02 / ECOCOL03 / ECOCOL04)
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
//! ## File formats
//!
//! ### ECOCOL02 (`encode_column`)
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
//! ### ECOCOL03 (`encode_column_pred_aligned`)
//!
//! Same as ECOCOL02 but blocks may be shorter at predicate boundaries.
//! Block index entries are 24 B: `(first_value, byte_offset, start_pos)`.
//!
//! ### ECOCOL04 (`encode_column_zstd`)
//!
//! Delta blocks (ECOCOL02 wire format) are grouped into Zstd-compressed
//! chunks of `ZSTD_BLOCKS_PER_CHUNK` blocks each.
//!
//! ```text
//! offset  0: magic              [u8; 8] = b"ECOCOL04"
//! offset  8: count              u64
//! offset 16: block_count        u64
//! offset 24: blocks_per_chunk   u64
//! offset 32: chunk_count        u64
//! offset 40: idx_offset         u64      byte offset of chunk index
//! offset 48: ── Zstd frames (variable length) ─────────────────────────────
//! at idx_offset:
//!   [(first_value: u64, byte_offset: u64, compressed_size: u32, n_blocks: u32)
//!    × chunk_count]
//! ```
//!
//! ## Usage
//!
//! **Build:** call [`encode_column`] / [`encode_column_zstd`].
//!
//! **Read:** open via [`DeltaColFile::open`].

use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use memmap2::Mmap;

// ── Constants ─────────────────────────────────────────────────────────────────

/// ECOCOL02: standard encoding — all blocks are exactly `DELTA_BLOCK_SIZE`
/// entries (except the last which may be smaller).  Block index entry = 16 B:
/// `(first_value: u64, byte_offset: u64)`.
pub const DELTA_MAGIC: &[u8; 8] = b"ECOCOL02";

/// ECOCOL03: pred-aligned encoding — blocks may be smaller than
/// `DELTA_BLOCK_SIZE` at predicate boundaries.  Block index entry = 24 B:
/// `(first_value: u64, byte_offset: u64, start_pos: u64)`.
pub const DELTA_MAGIC_V3: &[u8; 8] = b"ECOCOL03";

/// ECOCOL04: delta encoding + Zstd block compression.
pub const DELTA_MAGIC_V4: &[u8; 8] = b"ECOCOL04";

pub const DELTA_HDR: usize = 32; // magic(8)+count(8)+block_count(8)+idx_offset(8)
pub const DELTA_BLOCK_SIZE: usize = 256;

/// Number of delta blocks per Zstd chunk in ECOCOL04.
pub const ZSTD_BLOCKS_PER_CHUNK: usize = 64;
/// Zstd compression level for ECOCOL04.
pub const ZSTD_COMPRESSION_LEVEL: i32 = 3;

// Encoding tags stored in the first byte of each block.
const ENC_ALL_SAME: u8 = 0;
const ENC_U8:       u8 = 1;
const ENC_U16:      u8 = 2;
const ENC_U32:      u8 = 3;
const ENC_U64:      u8 = 4;

// ── Encoder (ECOCOL02) ────────────────────────────────────────────────────────

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

// ── ECOCOL04: block serialisation helper ──────────────────────────────────────

/// Serialize one delta block in ECOCOL02 wire format into `buf`.
///
/// Uses wrapping arithmetic so non-monotone blocks (c2 at group boundaries)
/// don't panic in debug builds.
fn append_block_bytes(buf: &mut Vec<u8>, chunk: &[u64]) {
    let base = chunk[0];
    let n = chunk.len();
    let max_delta = if n == 1 {
        0
    } else {
        chunk[1..].iter().fold(0u64, |acc, &v| acc.max(v.wrapping_sub(base)))
    };
    let enc = if max_delta == 0         { ENC_ALL_SAME }
              else if max_delta <= u8::MAX  as u64 { ENC_U8  }
              else if max_delta <= u16::MAX as u64 { ENC_U16 }
              else if max_delta <= u32::MAX as u64 { ENC_U32 }
              else                                 { ENC_U64 };

    buf.push(enc);
    buf.push((n - 1) as u8);
    buf.extend_from_slice(&base.to_le_bytes());
    match enc {
        ENC_ALL_SAME => {}
        ENC_U8  => {
            for &v in &chunk[1..] { buf.push(v.wrapping_sub(base) as u8); }
        }
        ENC_U16 => {
            for &v in &chunk[1..] {
                buf.extend_from_slice(&(v.wrapping_sub(base) as u16).to_le_bytes());
            }
        }
        ENC_U32 => {
            for &v in &chunk[1..] {
                buf.extend_from_slice(&(v.wrapping_sub(base) as u32).to_le_bytes());
            }
        }
        _ => {
            // ENC_U64: raw values (not relative deltas)
            for &v in &chunk[1..] { buf.extend_from_slice(&v.to_le_bytes()); }
        }
    }
}

// ── Decoders ─────────────────────────────────────────────────────────────────

/// Decode one compressed block starting at `data[offset..]`.
///
/// Clears `out` then writes decoded u64 values.  Returns bytes consumed.
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

/// Like `decode_block` but uses wrapping arithmetic and APPENDS to `out` (no clear).
///
/// Used by ECOCOL04 chunk decoder to accumulate multiple blocks into one Vec.
fn decode_block_wrapping(data: &[u8], offset: usize, out: &mut Vec<u64>) -> usize {
    let enc      = data[offset];
    let count_m1 = data[offset + 1] as usize;
    let n        = count_m1 + 1;
    let base     = u64::from_le_bytes(data[offset+2..offset+10].try_into().unwrap());
    let body     = &data[offset + 10..];

    match enc {
        ENC_ALL_SAME => {
            for _ in 0..n { out.push(base); }
            2 + 8
        }
        ENC_U8 => {
            out.push(base);
            for i in 0..count_m1 {
                out.push(base.wrapping_add(body[i] as u64));
            }
            2 + 8 + count_m1
        }
        ENC_U16 => {
            out.push(base);
            for i in 0..count_m1 {
                let delta = u16::from_le_bytes(body[i*2..i*2+2].try_into().unwrap()) as u64;
                out.push(base.wrapping_add(delta));
            }
            2 + 8 + count_m1 * 2
        }
        ENC_U32 => {
            out.push(base);
            for i in 0..count_m1 {
                let delta = u32::from_le_bytes(body[i*4..i*4+4].try_into().unwrap()) as u64;
                out.push(base.wrapping_add(delta));
            }
            2 + 8 + count_m1 * 4
        }
        _ => {
            // ENC_U64: raw values
            out.push(base);
            for i in 0..count_m1 {
                out.push(u64::from_le_bytes(body[i*8..i*8+8].try_into().unwrap()));
            }
            2 + 8 + count_m1 * 8
        }
    }
}

// ── Encoder (ECOCOL04) ────────────────────────────────────────────────────────

/// ECOCOL04: delta-encode + Zstd-compress `values` and write to `path`.
///
/// Groups delta blocks into chunks of `ZSTD_BLOCKS_PER_CHUNK` (= 64 × 256 = 16384
/// values) and compresses each chunk as a single Zstd frame.
pub fn encode_column_zstd(values: &[u64], path: &Path) -> io::Result<()> {
    let count = values.len();
    let block_count = count.div_ceil(DELTA_BLOCK_SIZE);
    let chunk_count = block_count.div_ceil(ZSTD_BLOCKS_PER_CHUNK);

    let f = File::create(path)?;
    let mut w = BufWriter::with_capacity(8 * 1024 * 1024, f);

    // Header — idx_offset is back-patched below.
    w.write_all(DELTA_MAGIC_V4)?;
    w.write_all(&(count as u64).to_le_bytes())?;
    w.write_all(&(block_count as u64).to_le_bytes())?;
    w.write_all(&(ZSTD_BLOCKS_PER_CHUNK as u64).to_le_bytes())?;
    w.write_all(&(chunk_count as u64).to_le_bytes())?;
    w.write_all(&0u64.to_le_bytes())?; // idx_offset placeholder

    // (first_value, byte_offset, compressed_size, n_blocks)
    let mut chunk_index: Vec<(u64, u64, u32, u32)> = Vec::with_capacity(chunk_count);
    let mut byte_pos: u64 = 48; // V4 header size

    let all_blocks: Vec<&[u64]> = values.chunks(DELTA_BLOCK_SIZE).collect();
    for chunk_blocks in all_blocks.chunks(ZSTD_BLOCKS_PER_CHUNK) {
        let first_value = chunk_blocks[0][0];
        let n_blocks = chunk_blocks.len() as u32;

        let mut raw_bytes: Vec<u8> = Vec::new();
        for block in chunk_blocks {
            append_block_bytes(&mut raw_bytes, block);
        }

        let compressed = zstd::encode_all(raw_bytes.as_slice(), ZSTD_COMPRESSION_LEVEL)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        let compressed_size = compressed.len() as u32;

        chunk_index.push((first_value, byte_pos, compressed_size, n_blocks));
        w.write_all(&compressed)?;
        byte_pos += compressed.len() as u64;
    }

    // Write chunk index.
    let idx_offset = byte_pos;
    for (fv, bo, cs, nb) in &chunk_index {
        w.write_all(&fv.to_le_bytes())?;
        w.write_all(&bo.to_le_bytes())?;
        w.write_all(&cs.to_le_bytes())?;
        w.write_all(&nb.to_le_bytes())?;
    }

    // Back-patch idx_offset at header offset 40.
    let mut f = w.into_inner().map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
    f.seek(SeekFrom::Start(40))?;
    f.write_all(&idx_offset.to_le_bytes())?;
    f.flush()?;

    Ok(())
}

// ── DeltaColFile ──────────────────────────────────────────────────────────────

/// A memory-mapped delta-encoded column file.
///
/// Supports three on-disk formats:
///
/// - **ECOCOL02** (`encode_column`): fixed-size blocks.  Block index entry = 16 B.
/// - **ECOCOL03** (`encode_column_pred_aligned`): variable-size blocks at predicate
///   boundaries.  Block index entry = 24 B.
/// - **ECOCOL04** (`encode_column_zstd`): delta blocks + Zstd chunk compression.
///   Chunk index stored in `zstd_index`; no per-block mmap index.
///
/// The block/chunk index lives at the end of the mmap.  For ECOCOL02/03 it is
/// **not** copied into a Vec — the OS page cache manages hot portions and binary
/// search touches only O(log N) pages.  For ECOCOL04 the chunk index (24 B per
/// chunk of 16384 values) is small enough to load into RAM.
pub struct DeltaColFile {
    _file: File,
    mmap:  Mmap,
    pub count: usize,
    block_count: usize,
    /// Byte offset within `mmap` where the block/chunk index section begins.
    idx_offset: usize,
    /// Bytes per block index entry: 16 for ECOCOL02, 24 for ECOCOL03, 0 for ECOCOL04.
    entry_bytes: usize,
    /// Logical start position of each block (ECOCOL03 only).
    start_positions: Vec<usize>,
    /// True when all blocks are exactly DELTA_BLOCK_SIZE (ECOCOL02 and ECOCOL04).
    fixed_size: bool,
    /// ECOCOL04 chunk index: (first_value, byte_offset, compressed_size, n_blocks).
    /// `None` for ECOCOL02/03.
    zstd_index: Option<Vec<(u64, u64, u32, u32)>>,
    /// Blocks per Zstd chunk (ECOCOL04 only).
    zstd_blocks_per_chunk: usize,
}

impl DeltaColFile {
    /// Open and memory-map a delta-encoded column file (ECOCOL02, ECOCOL03, or ECOCOL04).
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let mmap = unsafe { Mmap::map(&file)? };

        if mmap.len() < 8 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "delta col too small"));
        }

        let magic = &mmap[0..8];
        let is_v4 = magic == DELTA_MAGIC_V4;
        let is_v3 = magic == DELTA_MAGIC_V3;

        if !is_v4 && !is_v3 && magic != DELTA_MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData,
                format!("bad delta col magic: {:?}", &mmap[0..8])));
        }

        // ── ECOCOL04 ──────────────────────────────────────────────────────────
        if is_v4 {
            if mmap.len() < 48 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "delta col V4 too small"));
            }
            let count             = u64::from_le_bytes(mmap[8..16].try_into().unwrap()) as usize;
            let block_count       = u64::from_le_bytes(mmap[16..24].try_into().unwrap()) as usize;
            let blocks_per_chunk  = u64::from_le_bytes(mmap[24..32].try_into().unwrap()) as usize;
            let chunk_count       = u64::from_le_bytes(mmap[32..40].try_into().unwrap()) as usize;
            let idx_offset        = u64::from_le_bytes(mmap[40..48].try_into().unwrap()) as usize;

            if mmap.len() < idx_offset + chunk_count * 24 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "truncated V4 chunk index"));
            }

            let mut zstd_index = Vec::with_capacity(chunk_count);
            for i in 0..chunk_count {
                let off = idx_offset + i * 24;
                let first_value     = u64::from_le_bytes(mmap[off   ..off+ 8].try_into().unwrap());
                let byte_offset     = u64::from_le_bytes(mmap[off+ 8..off+16].try_into().unwrap());
                let compressed_size = u32::from_le_bytes(mmap[off+16..off+20].try_into().unwrap());
                let n_blocks        = u32::from_le_bytes(mmap[off+20..off+24].try_into().unwrap());
                zstd_index.push((first_value, byte_offset, compressed_size, n_blocks));
            }

            return Ok(Self {
                _file: file,
                mmap,
                count,
                block_count,
                idx_offset,
                entry_bytes: 0,
                start_positions: Vec::new(),
                fixed_size: true,
                zstd_index: Some(zstd_index),
                zstd_blocks_per_chunk: blocks_per_chunk,
            });
        }

        // ── ECOCOL02 / ECOCOL03 ───────────────────────────────────────────────
        if mmap.len() < DELTA_HDR {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "delta col too small"));
        }

        let count       = u64::from_le_bytes(mmap[8..16].try_into().unwrap()) as usize;
        let block_count = u64::from_le_bytes(mmap[16..24].try_into().unwrap()) as usize;
        let idx_offset  = u64::from_le_bytes(mmap[24..32].try_into().unwrap()) as usize;

        let entry_bytes = if is_v3 { 24 } else { 16 };
        if mmap.len() < idx_offset + block_count * entry_bytes {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "truncated block index"));
        }

        // ECOCOL03: load start_positions for random block lookup.
        let start_positions: Vec<usize> = if is_v3 {
            (0..block_count).map(|i| {
                let off = idx_offset + i * 24 + 16;
                u64::from_le_bytes(mmap[off..off+8].try_into().unwrap()) as usize
            }).collect()
        } else {
            Vec::new()
        };

        Ok(Self {
            _file: file,
            mmap,
            count,
            block_count,
            idx_offset,
            entry_bytes,
            start_positions,
            fixed_size: !is_v3,
            zstd_index: None,
            zstd_blocks_per_chunk: 0,
        })
    }

    #[inline]
    fn is_v4(&self) -> bool {
        self.zstd_index.is_some()
    }

    // ── Block index accessors (ECOCOL02/03 only) ──────────────────────────────

    /// `first_value` field of block `block_idx` from the mmap block index.
    #[inline]
    fn block_first_value(&self, block_idx: usize) -> u64 {
        let off = self.idx_offset + block_idx * self.entry_bytes;
        u64::from_le_bytes(self.mmap[off..off+8].try_into().unwrap())
    }

    /// `byte_offset` field of block `block_idx` from the mmap block index.
    #[inline]
    fn block_byte_off(&self, block_idx: usize) -> usize {
        let off = self.idx_offset + block_idx * self.entry_bytes;
        u64::from_le_bytes(self.mmap[off+8..off+16].try_into().unwrap()) as usize
    }

    /// Logical start position (entry index) of block `block_idx`.
    /// ECOCOL02/ECOCOL04: O(1) multiply.  ECOCOL03: Vec lookup.
    #[inline]
    fn block_start_pos(&self, block_idx: usize) -> usize {
        if self.fixed_size {
            block_idx * DELTA_BLOCK_SIZE
        } else {
            self.start_positions[block_idx]
        }
    }

    // ── Public API ────────────────────────────────────────────────────────────

    /// Decompress the block at `block_idx` into `out`.
    pub fn decompress_block(&self, block_idx: usize, out: &mut Vec<u64>) {
        if self.is_v4() {
            let bpc = self.zstd_blocks_per_chunk;
            let chunk_idx   = block_idx / bpc;
            let local_block = block_idx % bpc;
            let chunk_values = self.decode_zstd_chunk(chunk_idx)
                .expect("ECOCOL04 zstd decode failed");
            let start = local_block * DELTA_BLOCK_SIZE;
            let end   = (start + DELTA_BLOCK_SIZE).min(chunk_values.len());
            out.clear();
            out.extend_from_slice(&chunk_values[start..end]);
        } else {
            decode_block(&self.mmap, self.block_byte_off(block_idx), out);
        }
    }

    /// Decode Zstd chunk `chunk_idx` and return all its values as a flat Vec.
    fn decode_zstd_chunk(&self, chunk_idx: usize) -> io::Result<Vec<u64>> {
        let (_, byte_offset, comp_size, n_blocks) =
            self.zstd_index.as_ref().unwrap()[chunk_idx];
        let compressed = &self.mmap[byte_offset as usize
                                    .. byte_offset as usize + comp_size as usize];
        let raw_bytes = zstd::decode_all(compressed)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        let mut values: Vec<u64> = Vec::new();
        let mut offset = 0usize;
        for _ in 0..n_blocks as usize {
            offset += decode_block_wrapping(&raw_bytes, offset, &mut values);
        }
        Ok(values)
    }

    /// Find `(block_idx, offset_within_block)` for logical position `pos`.
    #[inline]
    fn block_for_pos(&self, pos: usize) -> (usize, usize) {
        if self.fixed_size {
            (pos / DELTA_BLOCK_SIZE, pos % DELTA_BLOCK_SIZE)
        } else {
            let block_idx = self.start_positions
                .partition_point(|&sp| sp <= pos)
                .saturating_sub(1);
            (block_idx, pos - self.start_positions[block_idx])
        }
    }

    /// Return the value at logical position `pos`.
    pub fn get(&self, pos: usize) -> u64 {
        let (block_idx, block_offset) = self.block_for_pos(pos);
        let mut buf = Vec::with_capacity(DELTA_BLOCK_SIZE);
        self.decompress_block(block_idx, &mut buf);
        buf[block_offset]
    }

    /// Return the index of the first block whose `first_value > target`.
    ///
    /// Reads O(log block_count) entries from the mmap block index region —
    /// all cache-resident after the first query that touches that key range.
    pub fn block_upper_bound(&self, target: u64) -> usize {
        if self.is_v4() {
            return self.block_upper_bound_v4(target);
        }
        let (mut lo, mut hi) = (0, self.block_count);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.block_first_value(mid) <= target { lo = mid + 1; } else { hi = mid; }
        }
        lo
    }

    /// ECOCOL04 implementation of `block_upper_bound`.
    fn block_upper_bound_v4(&self, target: u64) -> usize {
        let zi = self.zstd_index.as_ref().unwrap();
        if zi.is_empty() { return 0; }

        // Chunk-level upper bound: first chunk with first_value > target.
        let chunk_ub = {
            let (mut lo, mut hi) = (0, zi.len());
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                if zi[mid].0 <= target { lo = mid + 1; } else { hi = mid; }
            }
            lo
        };

        // Scan blocks inside the chunk just before chunk_ub.
        let search_chunk = chunk_ub.saturating_sub(1);
        let chunk_values = self.decode_zstd_chunk(search_chunk)
            .expect("ECOCOL04 zstd decode failed");

        let chunk_start_block = search_chunk * self.zstd_blocks_per_chunk;
        let n_blocks_in_chunk = zi[search_chunk].3 as usize;

        for local_block in 0..n_blocks_in_chunk {
            let block_start = local_block * DELTA_BLOCK_SIZE;
            if block_start >= chunk_values.len() { break; }
            if chunk_values[block_start] > target {
                return chunk_start_block + local_block;
            }
        }

        // All blocks in search_chunk have first_value <= target.
        (chunk_ub * self.zstd_blocks_per_chunk).min(self.block_count)
    }

    /// Find the position of the first value `>= target` (lower bound).
    ///
    /// O(log block_count) mmap reads to locate the block, then one block
    /// decompress + linear scan within the block (~256 entries).
    pub fn lower_bound(&self, target: u64) -> usize {
        if self.is_v4() {
            return self.lower_bound_v4(target);
        }
        if self.block_count == 0 {
            return 0;
        }
        // First block B where first_value[B] >= target.
        let blk_lb = {
            let (mut lo, mut hi) = (0, self.block_count);
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                if self.block_first_value(mid) < target { lo = mid + 1; } else { hi = mid; }
            }
            lo
        };
        let block_start = blk_lb.saturating_sub(1);

        let mut buf = Vec::with_capacity(DELTA_BLOCK_SIZE);
        self.decompress_block(block_start, &mut buf);

        let base_pos = self.block_start_pos(block_start);
        for (i, &v) in buf.iter().enumerate() {
            if v >= target {
                return base_pos + i;
            }
        }
        if blk_lb < self.block_count {
            self.block_start_pos(blk_lb)
        } else {
            self.count
        }
    }

    /// ECOCOL04 implementation of `lower_bound`.
    fn lower_bound_v4(&self, target: u64) -> usize {
        let zi = self.zstd_index.as_ref().unwrap();
        if zi.is_empty() { return 0; }

        // Chunk-level lower bound: first chunk with first_value >= target.
        let chunk_lb = {
            let (mut lo, mut hi) = (0, zi.len());
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                if zi[mid].0 < target { lo = mid + 1; } else { hi = mid; }
            }
            lo
        };

        // Scan values inside the chunk just before chunk_lb.
        let search_chunk = chunk_lb.saturating_sub(1);
        let chunk_values = self.decode_zstd_chunk(search_chunk)
            .expect("ECOCOL04 zstd decode failed");

        let chunk_start_pos =
            search_chunk * self.zstd_blocks_per_chunk * DELTA_BLOCK_SIZE;

        for (i, &v) in chunk_values.iter().enumerate() {
            if v >= target {
                return chunk_start_pos + i;
            }
        }

        // All values in search_chunk < target; answer is start of chunk_lb.
        if chunk_lb < zi.len() {
            (chunk_lb * self.zstd_blocks_per_chunk * DELTA_BLOCK_SIZE).min(self.count)
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

/// Iterator over values in a [`DeltaColFile`], decompressing one block/chunk at a time.
///
/// For ECOCOL04, `buf` holds all decoded values of the current Zstd chunk
/// (up to `ZSTD_BLOCKS_PER_CHUNK × DELTA_BLOCK_SIZE` values), advancing one chunk
/// at a time for efficient sequential access.
pub struct DeltaColIter<'a> {
    col:       &'a DeltaColFile,
    block_idx: usize,   // current block (ECOCOL02/03)
    chunk_idx: usize,   // current chunk (ECOCOL04)
    buf:       Vec<u64>,
    buf_pos:   usize,
    remaining: usize,
}

impl<'a> DeltaColIter<'a> {
    fn new(col: &'a DeltaColFile, start_pos: usize) -> Self {
        if col.is_v4() {
            let chunk_size = col.zstd_blocks_per_chunk * DELTA_BLOCK_SIZE;
            let chunk_count = col.zstd_index.as_ref().unwrap().len();
            let remaining = col.count.saturating_sub(start_pos);

            let (chunk_idx, pos_in_chunk) = if remaining > 0 && chunk_count > 0 {
                (start_pos / chunk_size, start_pos % chunk_size)
            } else {
                (chunk_count, 0)
            };

            let mut buf = Vec::new();
            if chunk_idx < chunk_count {
                buf = col.decode_zstd_chunk(chunk_idx).unwrap_or_default();
            }

            Self { col, block_idx: 0, chunk_idx, buf, buf_pos: pos_in_chunk, remaining }
        } else {
            let (block_idx, buf_pos) = if start_pos < col.count && col.block_count > 0 {
                col.block_for_pos(start_pos)
            } else {
                (col.block_count, 0)
            };

            let mut buf = Vec::with_capacity(DELTA_BLOCK_SIZE);
            if block_idx < col.block_count {
                col.decompress_block(block_idx, &mut buf);
            }

            let remaining = col.count.saturating_sub(start_pos);
            Self { col, block_idx, chunk_idx: 0, buf, buf_pos, remaining }
        }
    }
}

impl<'a> Iterator for DeltaColIter<'a> {
    type Item = u64;

    #[inline]
    fn next(&mut self) -> Option<u64> {
        if self.remaining == 0 {
            return None;
        }

        if self.col.is_v4() {
            if self.buf_pos >= self.buf.len() {
                self.chunk_idx += 1;
                let chunk_count = self.col.zstd_index.as_ref().unwrap().len();
                if self.chunk_idx >= chunk_count {
                    return None;
                }
                self.buf = self.col.decode_zstd_chunk(self.chunk_idx).unwrap_or_default();
                self.buf_pos = 0;
            }
        } else {
            if self.buf_pos >= self.buf.len() {
                self.block_idx += 1;
                if self.block_idx >= self.col.block_count {
                    return None;
                }
                self.col.decompress_block(self.block_idx, &mut self.buf);
                self.buf_pos = 0;
            }
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

    // ECOCOL03: write new magic so open() knows block index entries are 24 bytes.
    w.write_all(DELTA_MAGIC_V3)?;
    w.write_all(&(count as u64).to_le_bytes())?;
    w.write_all(&0u64.to_le_bytes())?; // block_count placeholder
    w.write_all(&0u64.to_le_bytes())?; // block_idx_offset placeholder

    // (first_value, byte_offset, start_pos) — 24 bytes per entry.
    let mut block_index: Vec<(u64, u64, u64)> = Vec::new();
    let mut byte_pos: u64 = DELTA_HDR as u64;
    let mut logical_pos: usize = 0; // cumulative entry count = start_pos of next block

    let mut pos = 0usize;
    let mut force_idx = 0usize;

    while pos < count {
        let base = values[pos];

        let natural_end = (pos + DELTA_BLOCK_SIZE).min(count);
        let forced_end = if force_idx < force_starts.len() && force_starts[force_idx] > pos {
            force_starts[force_idx].min(natural_end)
        } else {
            natural_end
        };
        while force_idx < force_starts.len() && force_starts[force_idx] <= forced_end {
            force_idx += 1;
        }

        let chunk = &values[pos..forced_end];
        let n = chunk.len();

        block_index.push((base, byte_pos, logical_pos as u64));

        let max_delta = if n == 1 { 0 } else {
            chunk[1..].iter().fold(0u64, |acc, &v| acc.max(v - base))
        };
        let enc = if max_delta == 0 { ENC_ALL_SAME }
                  else if max_delta <= u8::MAX  as u64 { ENC_U8  }
                  else if max_delta <= u16::MAX as u64 { ENC_U16 }
                  else if max_delta <= u32::MAX as u64 { ENC_U32 }
                  else                                  { ENC_U64 };

        let count_m1 = (n - 1) as u8;
        w.write_all(&[enc, count_m1])?;
        w.write_all(&base.to_le_bytes())?;

        match enc {
            ENC_ALL_SAME => { byte_pos += 2 + 8; }
            ENC_U8  => { for &v in &chunk[1..] { w.write_all(&[(v-base) as u8])?; }       byte_pos += 2+8+(n-1) as u64; }
            ENC_U16 => { for &v in &chunk[1..] { w.write_all(&((v-base) as u16).to_le_bytes())?; } byte_pos += 2+8+(n-1) as u64*2; }
            ENC_U32 => { for &v in &chunk[1..] { w.write_all(&((v-base) as u32).to_le_bytes())?; } byte_pos += 2+8+(n-1) as u64*4; }
            _       => { for &v in &chunk[1..] { w.write_all(&v.to_le_bytes())?; }         byte_pos += 2+8+(n-1) as u64*8; }
        }

        logical_pos += n;
        pos = forced_end;
    }

    // Write 24-byte block index entries: (first_value, byte_offset, start_pos).
    let idx_offset = byte_pos;
    let block_count = block_index.len();
    for (first_val, byte_off, start_pos) in &block_index {
        w.write_all(&first_val.to_le_bytes())?;
        w.write_all(&byte_off.to_le_bytes())?;
        w.write_all(&start_pos.to_le_bytes())?;
    }

    // Back-patch block_count (offset 16) and idx_offset (offset 24).
    let mut f = w.into_inner().map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
    f.seek(SeekFrom::Start(16))?;
    f.write_all(&(block_count as u64).to_le_bytes())?;
    f.write_all(&idx_offset.to_le_bytes())?;
    f.flush()?;

    Ok(())
}

// ── Suffix helpers ────────────────────────────────────────────────────────────

/// Return the `.dz` variant path: `spo.c0` → `spo.c0.dz`.
pub fn delta_path(col_path: &Path) -> PathBuf {
    let name = col_path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("col");
    col_path.with_file_name(format!("{}.dz", name))
}

/// Return the `.zst` variant path for ECOCOL04: `spo.c0` → `spo.c0.zst`.
pub fn zstd_path(col_path: &Path) -> PathBuf {
    let name = col_path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("col");
    col_path.with_file_name(format!("{}.zst", name))
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

    fn round_trip_v4(values: &[u64]) -> Vec<u64> {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        encode_column_zstd(values, tmp.path()).unwrap();
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

    // ── ECOCOL04 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_v4_all_same() {
        let v: Vec<u64> = vec![42u64; 512];
        assert_eq!(round_trip_v4(&v), v);
    }

    #[test]
    fn test_v4_u8_delta() {
        let v: Vec<u64> = (0u64..512).map(|i| 1000 + i).collect();
        assert_eq!(round_trip_v4(&v), v);
    }

    #[test]
    fn test_v4_u16_delta() {
        let v: Vec<u64> = (0u64..512).map(|i| 1000 + i * 300).collect();
        assert_eq!(round_trip_v4(&v), v);
    }

    #[test]
    fn test_v4_multi_chunk() {
        // Spans multiple Zstd chunks (ZSTD_BLOCKS_PER_CHUNK * DELTA_BLOCK_SIZE = 16384).
        let v: Vec<u64> = (0u64..40000).map(|i| i * 7).collect();
        assert_eq!(round_trip_v4(&v), v);
    }

    #[test]
    fn test_v4_lower_bound() {
        let v: Vec<u64> = (0u64..1024).map(|i| i * 2).collect();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        encode_column_zstd(&v, tmp.path()).unwrap();
        let col = DeltaColFile::open(tmp.path()).unwrap();
        assert_eq!(col.get(col.lower_bound(5)), 6);
        assert_eq!(col.lower_bound(0), 0);
    }

    #[test]
    fn test_v4_iter_from() {
        let v: Vec<u64> = (0u64..600).collect();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        encode_column_zstd(&v, tmp.path()).unwrap();
        let col = DeltaColFile::open(tmp.path()).unwrap();
        let result: Vec<u64> = col.iter_from(250).take(10).collect();
        assert_eq!(result, (250u64..260).collect::<Vec<_>>());
    }

    #[test]
    fn test_v4_iter_cross_chunk() {
        // Iter crosses a chunk boundary (chunk size = 16384).
        let v: Vec<u64> = (0u64..20000).collect();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        encode_column_zstd(&v, tmp.path()).unwrap();
        let col = DeltaColFile::open(tmp.path()).unwrap();
        let result: Vec<u64> = col.iter_from(16380).take(10).collect();
        assert_eq!(result, (16380u64..16390).collect::<Vec<_>>());
    }

    #[test]
    fn test_v4_wrapping_delta() {
        // Non-monotone data (c2-style: group resets) — tests wrapping arithmetic.
        let mut v: Vec<u64> = Vec::new();
        for group in 0u64..5 {
            for i in 0u64..512 {
                v.push(group * 1000 + i);
            }
        }
        // Introduce backward jumps by shuffling groups.
        let mut v2: Vec<u64> = Vec::new();
        for _ in 0..3 {
            v2.extend_from_slice(&v[..256]);
            v2.extend_from_slice(&v[512..768]); // jump backward
        }
        assert_eq!(round_trip_v4(&v2), v2);
    }

    #[test]
    fn test_v4_empty() {
        let v: Vec<u64> = vec![];
        let tmp = tempfile::NamedTempFile::new().unwrap();
        encode_column_zstd(&v, tmp.path()).unwrap();
        let col = DeltaColFile::open(tmp.path()).unwrap();
        assert_eq!(col.count, 0);
        assert_eq!(col.iter_from(0).count(), 0);
    }

    #[test]
    fn test_v4_single_value() {
        let v: Vec<u64> = vec![12345678u64];
        assert_eq!(round_trip_v4(&v), v);
    }

    #[test]
    fn test_zstd_path() {
        let p = std::path::Path::new("/store/spo.c2");
        assert_eq!(zstd_path(p), std::path::PathBuf::from("/store/spo.c2.zst"));
    }
}
