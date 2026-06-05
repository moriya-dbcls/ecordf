//! # Blank-node semantic reordering
//!
//! Reassigns TermIds within the blank-node range so that nodes sharing the
//! same `rdf:type` (or, failing that, the same primary predicate as subject)
//! receive consecutive IDs.
//!
//! This improves cache efficiency for queries of the form
//! `?x a T . ?x :pred ?o` because bind-join probes for `?x` all fall
//! within a compact region of the SPO column index.
//!
//! ## Files written
//!
//! - `bnode_remap.bin` — mapping between old and new TermIds for blank nodes.
//!
//! ```text
//! [magic: "ECBNRM01" (8 bytes)]
//! [bnode_start: u64]
//! [count: u64]          ← number of blank-node entries
//! [new2old: count × u32]  ← new2old[i] = old offset from bnode_start
//! [old2new: count × u32]  ← old2new[i] = new offset from bnode_start
//! ```
//!
//! After writing `bnode_remap.bin`, all six column index files are rewritten
//! with the remapped TermIds and re-sorted.

use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::Path;

use memmap2::Mmap;
use rayon::prelude::*;

use crate::config::Config;
use crate::dict_builder::ReadonlyDict;
use crate::index::{AllBuilders, TripleIndex};
use crate::triple::{TermId, Triple, TriplePattern, UNBOUND};

const REMAP_MAGIC: &[u8; 8] = b"ECBNRM01";

// ── Public API ────────────────────────────────────────────────────────────────

/// Reorder blank-node TermIds in `store_dir` by primary predicate / rdf:type.
///
/// Rewrites all six column-index files and writes `bnode_remap.bin`.
/// The dictionary (`dict_sorted.bin`) is NOT changed; `bnode_remap.bin` is
/// loaded at query time to translate between old and new IDs transparently.
pub fn reorder_bnodes(store_dir: &Path) -> io::Result<()> {
    let dict_path = store_dir.join("dict_sorted.bin");
    let dict = ReadonlyDict::open(&dict_path)?;

    // Locate blank-node ID range via binary search.
    let (bnode_start, bnode_end) = find_bnode_range(&dict);
    if bnode_end < bnode_start {
        eprintln!("reorder-bnodes: no blank nodes found in dictionary.");
        return Ok(());
    }
    let n_bnodes = (bnode_end - bnode_start + 1) as usize;
    eprintln!("reorder-bnodes: {} blank nodes in ID range [{}, {}]",
              n_bnodes, bnode_start, bnode_end);

    let index = TripleIndex::open(store_dir)?;

    // Build sort key for each blank node: (primary_id, old_id).
    // primary_id = rdf:type object (preferred) or first predicate seen as subject.
    let sort_keys = build_sort_keys(&index, &dict, bnode_start, n_bnodes)?;
    eprintln!("reorder-bnodes: sort keys built. Sorting...");

    // Sort by (primary_id, old_bnode_id).
    // new2old[new_rank] = old_offset_from_bnode_start
    let mut ranked: Vec<(u64, u64)> = sort_keys.iter().enumerate()
        .map(|(offset, &pk)| (pk, offset as u64))
        .collect();
    ranked.par_sort_unstable();

    let mut new2old = vec![0u32; n_bnodes];
    let mut old2new = vec![0u32; n_bnodes];
    for (new_rank, &(_, old_offset)) in ranked.iter().enumerate() {
        new2old[new_rank]       = old_offset as u32;
        old2new[old_offset as usize] = new_rank as u32;
    }

    // Write bnode_remap.bin.
    write_remap(store_dir, bnode_start, &new2old, &old2new)?;
    eprintln!("reorder-bnodes: bnode_remap.bin written.");

    // Rewrite column index files.
    rewrite_indexes(store_dir, &index, bnode_start, &old2new)?;
    eprintln!("reorder-bnodes: done.");

    Ok(())
}

// ── BnodeRemap: loaded at runtime ────────────────────────────────────────────

/// In-memory representation of `bnode_remap.bin` used during query execution.
pub struct BnodeRemap {
    _mmap: Mmap,
    pub bnode_start: u64,
    pub count:       u64,
    new2old_ptr: *const u32,
    old2new_ptr: *const u32,
}

// Safety: the pointers point into the mmap which lives as long as BnodeRemap.
unsafe impl Send for BnodeRemap {}
unsafe impl Sync for BnodeRemap {}

impl BnodeRemap {
    pub fn open(store_dir: &Path) -> io::Result<Option<Self>> {
        let path = store_dir.join("bnode_remap.bin");
        if !path.exists() { return Ok(None); }
        let file = File::open(&path)?;
        let mmap = unsafe { Mmap::map(&file)? };
        if mmap.len() < 24 || &mmap[..8] != REMAP_MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad bnode_remap.bin magic"));
        }
        let bnode_start = u64::from_le_bytes(mmap[8..16].try_into().unwrap());
        let count       = u64::from_le_bytes(mmap[16..24].try_into().unwrap());
        let expected = 24 + count as usize * 8;
        if mmap.len() < expected {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bnode_remap.bin truncated"));
        }
        let new2old_ptr = mmap[24..].as_ptr() as *const u32;
        let old2new_ptr = unsafe { new2old_ptr.add(count as usize) };
        Ok(Some(Self { _mmap: mmap, bnode_start, count, new2old_ptr, old2new_ptr }))
    }

    #[inline]
    pub fn is_bnode(&self, id: u64) -> bool {
        id >= self.bnode_start && id < self.bnode_start + self.count
    }

    /// Translate new (reordered) TermId → old (dictionary) TermId.
    #[inline]
    pub fn new_to_old(&self, new_id: u64) -> u64 {
        let offset = (new_id - self.bnode_start) as usize;
        let old_offset = unsafe { *self.new2old_ptr.add(offset) } as u64;
        self.bnode_start + old_offset
    }

    /// Translate old (dictionary) TermId → new (index) TermId.
    #[inline]
    pub fn old_to_new(&self, old_id: u64) -> u64 {
        let offset = (old_id - self.bnode_start) as usize;
        let new_offset = unsafe { *self.old2new_ptr.add(offset) } as u64;
        self.bnode_start + new_offset
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn find_bnode_range(dict: &ReadonlyDict) -> (u64, u64) {
    let count = dict.len();
    if count == 0 { return (1, 0); }
    // Binary search for first ID whose string starts with "_:"
    let first = binary_search_first(dict, |s| !s.starts_with("_:"));
    let last  = binary_search_last(dict, |s| s.starts_with("_:"));
    (first, last)
}

fn binary_search_first(dict: &ReadonlyDict, pred_false: impl Fn(&str) -> bool) -> u64 {
    let (mut lo, mut hi) = (0u64, dict.len());
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if pred_false(dict.get_str(mid)) { lo = mid + 1; } else { hi = mid; }
    }
    lo
}

fn binary_search_last(dict: &ReadonlyDict, pred_true: impl Fn(&str) -> bool) -> u64 {
    let count = dict.len();
    let (mut lo, mut hi) = (0u64, count);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if pred_true(dict.get_str(mid)) { lo = mid + 1; } else { hi = mid; }
    }
    lo.saturating_sub(1)
}

/// Build a sort-key vector: sort_keys[old_offset] = primary_id.
/// primary_id = rdf:type object id (preferred), else first predicate id, else u64::MAX.
fn build_sort_keys(
    index: &TripleIndex,
    dict:  &ReadonlyDict,
    bnode_start: u64,
    n_bnodes:    usize,
) -> io::Result<Vec<u64>> {
    let mut sort_keys: Vec<u64> = vec![u64::MAX; n_bnodes];

    let rdf_type_str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

    // Phase A: use rdf:type → get (type_id, bnode_id) via POS index.
    if let Some(rdf_type_id) = dict.get_id(rdf_type_str) {
        let pat = TriplePattern { s: UNBOUND, p: rdf_type_id, o: UNBOUND };
        let mut typed = 0usize;
        for t in index.scan(&pat) {
            if t.s >= bnode_start && (t.s - bnode_start) < n_bnodes as u64 {
                let offset = (t.s - bnode_start) as usize;
                // Keep first type seen (index order).
                if sort_keys[offset] == u64::MAX {
                    sort_keys[offset] = t.o;  // object = rdf:type class
                    typed += 1;
                }
            }
        }
        eprintln!("reorder-bnodes: {}/{} blank nodes have rdf:type.", typed, n_bnodes);
    } else {
        eprintln!("reorder-bnodes: rdf:type not found in dictionary; using predicate-based keys.");
    }

    // Phase B: for untyped BNs, assign first predicate found in SPO.
    let mut fallback = 0usize;
    let pat_all = TriplePattern { s: UNBOUND, p: UNBOUND, o: UNBOUND };
    for t in index.spo.scan(&pat_all) {
        if t.s >= bnode_start && (t.s - bnode_start) < n_bnodes as u64 {
            let offset = (t.s - bnode_start) as usize;
            if sort_keys[offset] == u64::MAX {
                // Use predicate ID as sort key, offset into a range above typed keys
                // so typed BNs always come first.
                sort_keys[offset] = t.p | (1u64 << 62);
                fallback += 1;
            }
        }
    }
    if fallback > 0 {
        eprintln!("reorder-bnodes: {} blank nodes got predicate-based fallback key.", fallback);
    }

    Ok(sort_keys)
}

/// Move all column-index files produced by AllBuilders::build from `from` to `to`.
/// Only moves recognisable index files; leaves dict_sorted.bin and other files alone.
fn move_index_files(from: &Path, to: &Path) -> io::Result<()> {
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        let s = name.to_string_lossy();
        let is_index = s.ends_with(".c0")   || s.ends_with(".c1")   || s.ends_with(".c2")
            || s.ends_with(".c0.zst") || s.ends_with(".c1.zst") || s.ends_with(".c2.zst")
            || s.ends_with(".c0.dz")  || s.ends_with(".c1.dz")  || s.ends_with(".c2.dz")
            || s.ends_with(".c0.skip") || s.ends_with(".skip")
            || s.ends_with(".pidx")
            || s == "gspo.bin" || s == "stats.bin";
        if is_index {
            let dst = to.join(&name);
            fs::rename(entry.path(), &dst)?;
            eprintln!("  moved: {}", s);
        }
    }
    Ok(())
}

fn write_remap(store_dir: &Path, bnode_start: u64, new2old: &[u32], old2new: &[u32]) -> io::Result<()> {
    let path = store_dir.join("bnode_remap.bin");
    let tmp  = store_dir.join("bnode_remap.bin.tmp");
    let mut w = BufWriter::new(File::create(&tmp)?);
    w.write_all(REMAP_MAGIC)?;
    w.write_all(&bnode_start.to_le_bytes())?;
    w.write_all(&(new2old.len() as u64).to_le_bytes())?;
    for &v in new2old { w.write_all(&v.to_le_bytes())?; }
    for &v in old2new { w.write_all(&v.to_le_bytes())?; }
    w.flush()?;
    drop(w);
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// Re-sort all six column-index files with remapped blank-node IDs.
fn rewrite_indexes(
    store_dir: &Path,
    old_index: &TripleIndex,
    bnode_start: u64,
    old2new:     &[u32],
) -> io::Result<()> {
    let n = old2new.len() as u64;
    let remap = |id: TermId| -> TermId {
        if id >= bnode_start && id < bnode_start + n {
            bnode_start + old2new[(id - bnode_start) as usize] as u64
        } else {
            id
        }
    };

    let chunk_size = Config::load_or_default(store_dir)
        .map(|c| c.build.chunk_size)
        .unwrap_or(0)
        .max(5_000_000);

    let tmp_dir = store_dir.join("_ecordf_bnode_reorder_tmp");
    let out_dir = store_dir.join("_ecordf_bnode_reorder_out");
    fs::create_dir_all(&tmp_dir)?;
    fs::create_dir_all(&out_dir)?;

    let mut builders = AllBuilders::new_streaming_in(&tmp_dir, chunk_size)?;

    eprintln!("reorder-bnodes: scanning existing index and translating IDs...");
    for t in old_index.spo_scan_all() {
        let new_t = Triple::new(remap(t.s), remap(t.p), remap(t.o));
        builders.push(new_t)?;
    }

    // GSPO (named-graph) index: scan each graph and re-push as quads.
    if let Some(ref gspo) = old_index.gspo {
        use crate::triple::Quad;
        for g_id in gspo.graphs() {
            let new_g = remap(g_id);
            for t in gspo.scan_graph(g_id, &TriplePattern { s: UNBOUND, p: UNBOUND, o: UNBOUND }) {
                let new_q = Quad::new(remap(t.s), remap(t.p), remap(t.o), new_g);
                builders.push_quad(new_q)?;
            }
        }
    }

    eprintln!("reorder-bnodes: sorting and writing new index files...");
    builders.build(&out_dir)?;

    // Move all column/index files from out_dir to store_dir.
    // AllBuilders::build writes col files as "{stem}.c0(.zst)", skip as "{stem}.c0.skip",
    // predicate index as "{stem}.pidx" — all without the ".bin" infix.
    move_index_files(&out_dir, store_dir)?;

    // Cleanup temp dirs.
    let _ = fs::remove_dir_all(&tmp_dir);
    let _ = fs::remove_dir_all(&out_dir);

    Ok(())
}
