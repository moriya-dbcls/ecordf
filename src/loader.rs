//! # Data Loader: N-Triples, N-Quads, and gzipped variants
//!
//! Loads RDF data into the store's dictionary and index builder.
//! Streams line by line — constant memory regardless of file size.
//!
//! Supported formats:
//!   .nt / .ntriples     — N-Triples  (triples → union graph only)
//!   .nq / .nquads       — N-Quads    (quads   → union graph + GSPO named-graph index)
//!   .nt.gz / .ntriples.gz — gzipped N-Triples (with gzip feature)
//!   .nq.gz / .nquads.gz   — gzipped N-Quads   (with gzip feature)
//!
//! Extension resolution: the full filename stem is checked for a double extension
//! (e.g. `foo.nt.gz`) before falling back to the last extension alone (`foo.gz`).

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashSet};
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};

use rustc_hash::FxHashMap;

use crate::dict::Dictionary;
use crate::index::AllBuilders;
use crate::triple::{Quad, Triple};

pub struct LoadStats {
    pub triples_loaded: u64,
    pub lines_processed: u64,
    pub errors: u64,
}

// ── Input specification ───────────────────────────────────────────────────────

/// One input file, optionally associated with a named graph.
///
/// When `graph` is `Some`, triples are loaded into both the union graph
/// (SPO/POS/OSP indexes) and the named graph (GSPO index).  When `None`,
/// triples go to the union graph only — the default for plain N-Triples.
///
/// N-Quads files embed a graph per quad; the `graph` field is ignored for
/// those formats (the per-quad graph always takes precedence).
pub struct InputSpec {
    /// Path to the RDF file.
    pub path: PathBuf,
    /// Named graph IRI, without angle brackets.
    pub graph: Option<String>,
}

impl InputSpec {
    /// Plain file with no named graph (union graph only).
    pub fn plain(path: PathBuf) -> Self {
        Self { path, graph: None }
    }

    /// File whose triples will be loaded into the given named graph.
    ///
    /// `graph_iri` may be supplied with or without surrounding `<…>`.
    pub fn with_graph(path: PathBuf, graph_iri: impl Into<String>) -> Self {
        Self { path, graph: Some(strip_angle_brackets(graph_iri.into())) }
    }
}

/// Remove surrounding `<` `>` from an IRI if present.
fn strip_angle_brackets(s: String) -> String {
    if s.starts_with('<') && s.ends_with('>') && s.len() >= 2 {
        s[1..s.len() - 1].to_string()
    } else {
        s
    }
}

/// Load an N-Triples file (.nt) into the dictionary and index builder.
///
/// Format: one triple per line: <subject> <predicate> <object> .
/// Comments start with #.
pub fn load_ntriples(
    path: &Path,
    dict: &Dictionary,
    builders: &mut AllBuilders,
) -> io::Result<LoadStats> {
    let file = File::open(path)?;
    let reader = BufReader::with_capacity(4 * 1024 * 1024, file);
    let mut stats = LoadStats { triples_loaded: 0, lines_processed: 0, errors: 0 };

    for line in reader.lines() {
        let line = line?;
        stats.lines_processed += 1;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        match parse_nt_line(line) {
            Some((s, p, o)) => {
                let si = dict.encode(&s);
                let pi = dict.encode(&p);
                let oi = dict.encode(&o);
                builders.push(Triple::new(si, pi, oi))?;
                stats.triples_loaded += 1;
                if stats.triples_loaded % 1_000_000 == 0 {
                    eprintln!("  loaded {}M triples...", stats.triples_loaded / 1_000_000);
                }
            }
            None => {
                stats.errors += 1;
                if stats.errors <= 10 {
                    eprintln!("  parse error on line {}: {:?}", stats.lines_processed, &line[..line.len().min(80)]);
                }
            }
        }
    }

    Ok(stats)
}

/// Parse a single N-Triples line.
/// Returns (subject, predicate, object) as canonical strings.
fn parse_nt_line(line: &str) -> Option<(String, String, String)> {
    let line = line.strip_suffix('.').unwrap_or(line).trim();
    let mut parts = Vec::new();
    let mut chars = line.chars().peekable();

    while parts.len() < 3 {
        // Skip whitespace
        while chars.peek() == Some(&' ') || chars.peek() == Some(&'\t') {
            chars.next();
        }
        if chars.peek().is_none() { break; }

        let term = match chars.peek()? {
            '<' => {
                chars.next(); // consume <
                let mut s = String::new();
                while let Some(c) = chars.next() {
                    if c == '>' { break; }
                    if c == '\\' {
                        if let Some(esc) = chars.next() {
                            s.push(unescape_char(esc));
                        }
                    } else {
                        s.push(c);
                    }
                }
                s // bare IRI (no angle brackets in dictionary)
            }
            '"' => {
                chars.next(); // consume opening "
                let mut s = String::from("\"");
                let mut in_string = true;
                while let Some(c) = chars.next() {
                    if in_string {
                        if c == '"' {
                            s.push('"');
                            in_string = false;
                        } else if c == '\\' {
                            if let Some(esc) = chars.next() {
                                s.push('\\');
                                s.push(esc);
                            }
                        } else {
                            s.push(c);
                        }
                    } else {
                        // After closing quote: datatype or lang tag
                        match c {
                            '@' => {
                                s.push('@');
                                while let Some(&nc) = chars.peek() {
                                    if nc == ' ' || nc == '\t' { break; }
                                    s.push(nc);
                                    chars.next();
                                }
                                break;
                            }
                            '^' => {
                                // ^^<datatype>
                                chars.next(); // second ^
                                s.push_str("^^<");
                                chars.next(); // <
                                while let Some(nc) = chars.next() {
                                    if nc == '>' { break; }
                                    s.push(nc);
                                }
                                s.push('>');
                                break;
                            }
                            ' ' | '\t' => break,
                            _ => break,
                        }
                    }
                }
                s
            }
            '_' => {
                // Blank node _:label
                let mut s = String::new();
                while let Some(&c) = chars.peek() {
                    if c == ' ' || c == '\t' { break; }
                    s.push(c);
                    chars.next();
                }
                s
            }
            _ => return None,
        };
        parts.push(term);
    }

    if parts.len() == 3 {
        Some((parts.remove(0), parts.remove(0), parts.remove(0)))
    } else {
        None
    }
}

// ── Generic parse visitors (shared by one-pass and two-pass paths) ────────────

/// Stream every triple from an N-Triples file through `f(subject, predicate, object)`.
fn visit_nt_file<F>(path: &Path, mut f: F) -> io::Result<LoadStats>
where
    F: FnMut(&str, &str, &str) -> io::Result<()>,
{
    let file = File::open(path)?;
    let reader = BufReader::with_capacity(4 * 1024 * 1024, file);
    let mut stats = LoadStats { triples_loaded: 0, lines_processed: 0, errors: 0 };
    for line in reader.lines() {
        let line = line?;
        stats.lines_processed += 1;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') { continue; }
        match parse_nt_line(line) {
            Some((s, p, o)) => { f(&s, &p, &o)?; stats.triples_loaded += 1; }
            None => {
                stats.errors += 1;
                if stats.errors <= 10 {
                    eprintln!("  parse error on line {}: {:?}", stats.lines_processed, &line[..line.len().min(80)]);
                }
            }
        }
    }
    Ok(stats)
}

/// Stream every quad from an N-Quads file through `f(s, p, o, graph_opt)`.
fn visit_nq_file<F>(path: &Path, mut f: F) -> io::Result<LoadStats>
where
    F: FnMut(&str, &str, &str, Option<&str>) -> io::Result<()>,
{
    let file = File::open(path)?;
    let reader = BufReader::with_capacity(4 * 1024 * 1024, file);
    let mut stats = LoadStats { triples_loaded: 0, lines_processed: 0, errors: 0 };
    for line in reader.lines() {
        let line = line?;
        stats.lines_processed += 1;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') { continue; }
        match parse_nq_line(line) {
            Some((s, p, o, g)) => {
                f(&s, &p, &o, g.as_deref())?;
                stats.triples_loaded += 1;
            }
            None => { stats.errors += 1; }
        }
    }
    Ok(stats)
}

#[cfg(feature = "gzip")]
fn visit_nt_file_gz<F>(path: &Path, mut f: F) -> io::Result<LoadStats>
where
    F: FnMut(&str, &str, &str) -> io::Result<()>,
{
    use flate2::read::GzDecoder;
    let file = File::open(path)?;
    let gz = GzDecoder::new(file);
    let reader = BufReader::new(gz);
    let mut stats = LoadStats { triples_loaded: 0, lines_processed: 0, errors: 0 };
    for line in reader.lines() {
        let line = line?;
        stats.lines_processed += 1;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') { continue; }
        match parse_nt_line(line) {
            Some((s, p, o)) => { f(&s, &p, &o)?; stats.triples_loaded += 1; }
            None => { stats.errors += 1; }
        }
    }
    Ok(stats)
}

#[cfg(not(feature = "gzip"))]
fn visit_nt_file_gz<F>(_path: &Path, _f: F) -> io::Result<LoadStats>
where F: FnMut(&str, &str, &str) -> io::Result<()>
{
    Err(io::Error::new(io::ErrorKind::Unsupported,
        "gzip support not compiled in — rebuild with: cargo build --features gzip"))
}

#[cfg(feature = "gzip")]
fn visit_nq_file_gz<F>(path: &Path, mut f: F) -> io::Result<LoadStats>
where
    F: FnMut(&str, &str, &str, Option<&str>) -> io::Result<()>,
{
    use flate2::read::GzDecoder;
    let file = File::open(path)?;
    let gz = GzDecoder::new(file);
    let reader = BufReader::new(gz);
    let mut stats = LoadStats { triples_loaded: 0, lines_processed: 0, errors: 0 };
    for line in reader.lines() {
        let line = line?;
        stats.lines_processed += 1;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') { continue; }
        match parse_nq_line(line) {
            Some((s, p, o, g)) => {
                f(&s, &p, &o, g.as_deref())?;
                stats.triples_loaded += 1;
            }
            None => { stats.errors += 1; }
        }
    }
    Ok(stats)
}

#[cfg(not(feature = "gzip"))]
fn visit_nq_file_gz<F>(_path: &Path, _f: F) -> io::Result<LoadStats>
where F: FnMut(&str, &str, &str, Option<&str>) -> io::Result<()>
{
    Err(io::Error::new(io::ErrorKind::Unsupported,
        "gzip support not compiled in — rebuild with: cargo build --features gzip"))
}

// ── Phase 1: string collection ────────────────────────────────────────────────

/// Collect every unique RDF term from a **single** input into `db`.
///
/// Low-level helper shared by the sequential and parallel callers.
fn collect_strings_from_one_input(
    input: &InputSpec,
    db: &mut crate::dict_builder::DictBuilder,
) -> io::Result<LoadStats> {
    let path = &input.path;
    let graph = input.graph.as_deref();
    match (classify_extension(path), graph) {
        (FileKind::NTriples,   Some(g)) => collect_nt_strings(path, Some(g), db),
        (FileKind::NTriples,   None)    => collect_nt_strings(path, None,    db),
        (FileKind::NTriplesGz, Some(g)) => collect_nt_strings_gz(path, Some(g), db),
        (FileKind::NTriplesGz, None)    => collect_nt_strings_gz(path, None,    db),
        (FileKind::NQuads,   _)         => collect_nq_strings(path, db),
        (FileKind::NQuadsGz, _)         => collect_nq_strings_gz(path, db),
        (FileKind::Unknown(_), Some(g)) => collect_nt_strings(path, Some(g), db),
        (FileKind::Unknown(_), None)    => collect_nt_strings(path, None,    db),
    }
}

/// Collect every unique RDF term from `inputs` into `db` (Phase 1, sequential).
///
/// Streams through each file without building any index or triple buffer.
/// Memory cost = `db`'s internal chunk buffer (bounded by `dict_chunk_mb`).
pub fn collect_strings_from_inputs(
    inputs: &[InputSpec],
    db: &mut crate::dict_builder::DictBuilder,
) -> io::Result<LoadStats> {
    let mut total = LoadStats { triples_loaded: 0, lines_processed: 0, errors: 0 };
    for input in inputs {
        eprintln!("  Phase 1: {:?}", input.path.file_name().unwrap_or_default());
        let stats = collect_strings_from_one_input(input, db)?;
        total.triples_loaded  += stats.triples_loaded;
        total.lines_processed += stats.lines_processed;
        total.errors          += stats.errors;
    }
    Ok(total)
}

/// Phase 1, **parallel** variant.
///
/// Each file is processed by its own rayon worker thread.  Per-thread
/// [`DictBuilder`] instances write sorted string chunks to private
/// subdirectories under `tmp_dir`; their chunk files are returned for a
/// single k-way merge by the caller.
///
/// `total_dict_chunk_bytes` is divided by the actual thread count so that
/// total peak RAM stays approximately constant regardless of parallelism.
///
/// Returns `(all_chunk_paths, accumulated_stats)`.
pub fn collect_strings_parallel(
    inputs: &[InputSpec],
    tmp_dir: &Path,
    total_dict_chunk_bytes: usize,
    num_threads: usize,
) -> io::Result<(Vec<PathBuf>, LoadStats)> {
    use rayon::prelude::*;

    let n = num_threads.max(1);
    // Divide budget by actual thread count; never drop below 8 MB.
    let per_thread_bytes = (total_dict_chunk_bytes / n).max(8 * 1024 * 1024);

    // Process all inputs in parallel; each returns (chunks, stats) or an error.
    let results: Vec<io::Result<(Vec<PathBuf>, LoadStats)>> = inputs
        .par_iter()
        .enumerate()
        .map(|(i, input)| {
            let thread_tmp = tmp_dir.join(format!("p1_{:06}", i));
            eprintln!("  Phase 1 [t{i}]: {:?}", input.path.file_name().unwrap_or_default());
            let mut db = crate::dict_builder::DictBuilder::new(&thread_tmp, per_thread_bytes)?;
            let stats = collect_strings_from_one_input(input, &mut db)?;
            let chunks = db.flush_and_return_chunks()?;
            Ok((chunks, stats))
        })
        .collect();

    // Fold results; propagate first error (rayon does not short-circuit).
    let mut all_chunks = Vec::new();
    let mut total = LoadStats { triples_loaded: 0, lines_processed: 0, errors: 0 };
    for r in results {
        let (chunks, stats) = r?;
        all_chunks.extend(chunks);
        total.triples_loaded  += stats.triples_loaded;
        total.lines_processed += stats.lines_processed;
        total.errors          += stats.errors;
    }
    Ok((all_chunks, total))
}

fn collect_nt_strings(path: &Path, graph: Option<&str>, db: &mut crate::dict_builder::DictBuilder) -> io::Result<LoadStats> {
    if let Some(g) = graph { db.add(g)?; }
    visit_nt_file(path, |s, p, o| { db.add(s)?; db.add(p)?; db.add(o) })
}

fn collect_nt_strings_gz(path: &Path, graph: Option<&str>, db: &mut crate::dict_builder::DictBuilder) -> io::Result<LoadStats> {
    if let Some(g) = graph { db.add(g)?; }
    visit_nt_file_gz(path, |s, p, o| { db.add(s)?; db.add(p)?; db.add(o) })
}

fn collect_nq_strings(path: &Path, db: &mut crate::dict_builder::DictBuilder) -> io::Result<LoadStats> {
    visit_nq_file(path, |s, p, o, g| {
        db.add(s)?; db.add(p)?; db.add(o)?;
        if let Some(g) = g { db.add(g)?; }
        Ok(())
    })
}

fn collect_nq_strings_gz(path: &Path, db: &mut crate::dict_builder::DictBuilder) -> io::Result<LoadStats> {
    visit_nq_file_gz(path, |s, p, o, g| {
        db.add(s)?; db.add(p)?; db.add(o)?;
        if let Some(g) = g { db.add(g)?; }
        Ok(())
    })
}

// ── Phase 2: triple loading with ReadonlyDict ─────────────────────────────────

/// Load triples from a **single** input using IDs from `dict`.
///
/// Low-level helper shared by the sequential and parallel callers.
fn load_triple_from_one_input(
    input: &InputSpec,
    dict: &crate::dict_builder::ReadonlyDict,
    builders: &mut AllBuilders,
) -> io::Result<LoadStats> {
    let path = &input.path;
    match (classify_extension(path), input.graph.as_deref()) {
        (FileKind::NTriples,   Some(g)) => load_nt_two_pass_graph(path, g, dict, builders),
        (FileKind::NTriples,   None)    => load_nt_two_pass(path, None, dict, builders),
        (FileKind::NTriplesGz, Some(g)) => load_nt_two_pass_gz_graph(path, g, dict, builders),
        (FileKind::NTriplesGz, None)    => load_nt_two_pass_gz(path, None, dict, builders),
        (FileKind::NQuads,   _)         => load_nq_two_pass(path, dict, builders),
        (FileKind::NQuadsGz, _)         => load_nq_two_pass_gz(path, dict, builders),
        (FileKind::Unknown(_), Some(g)) => load_nt_two_pass_graph(path, g, dict, builders),
        (FileKind::Unknown(_), None)    => load_nt_two_pass(path, None, dict, builders),
    }
}

/// Load triples from `inputs` using IDs from `dict` (Phase 2, sequential).
///
/// Every string is resolved via binary search on the mmap-ed `ReadonlyDict`.
/// Returns an error if a term is missing from the dictionary (indicates a
/// mismatch between Phase 1 and Phase 2 parsing).
pub fn load_triples_with_readonly_dict(
    inputs: &[InputSpec],
    dict: &crate::dict_builder::ReadonlyDict,
    builders: &mut AllBuilders,
) -> io::Result<LoadStats> {
    let mut total = LoadStats { triples_loaded: 0, lines_processed: 0, errors: 0 };
    for input in inputs {
        let graph_label = input.graph.as_deref()
            .map(|g| format!(" → <{}>", g))
            .unwrap_or_default();
        eprintln!("  Phase 2: {:?}{}", input.path.file_name().unwrap_or_default(), graph_label);
        let stats = load_triple_from_one_input(input, dict, builders)?;
        total.triples_loaded  += stats.triples_loaded;
        total.lines_processed += stats.lines_processed;
        total.errors          += stats.errors;
        eprintln!("    → {} triples ({} errors)", stats.triples_loaded, stats.errors);
    }
    Ok(total)
}

/// Phase 2, **parallel** variant.
///
/// Each file is processed by its own rayon worker thread.  Each thread opens
/// its own [`ReadonlyDict`] instance (the OS shares physical pages across
/// mmap-s of the same file), writes triple chunks to a private subdirectory
/// under `tmp_dir`, and returns a [`ParallelChunks`] struct.
///
/// `chunk_size` is divided by thread count so total peak triple-buffer RAM
/// stays approximately constant.
///
/// Returns `(per_file_chunks, accumulated_stats)`.
pub fn load_triples_parallel(
    inputs: &[InputSpec],
    dict_sorted_path: &Path,
    tmp_dir: &Path,
    chunk_size: usize,
    num_threads: usize,
) -> io::Result<(Vec<crate::index::ParallelChunks>, LoadStats)> {
    use rayon::prelude::*;

    let n = num_threads.max(1);
    // Divide chunk_size by thread count; never drop below 100_000 triples.
    let per_thread_chunk_size = (chunk_size / n).max(100_000);

    let results: Vec<io::Result<(crate::index::ParallelChunks, LoadStats)>> = inputs
        .par_iter()
        .enumerate()
        .map(|(i, input)| {
            // Each thread opens its own ReadonlyDict — OS shares physical pages.
            let dict = crate::dict_builder::ReadonlyDict::open(dict_sorted_path)?;
            let thread_tmp = tmp_dir.join(format!("p2_{:06}", i));
            let graph_label = input.graph.as_deref()
                .map(|g| format!(" → <{}>", g))
                .unwrap_or_default();
            eprintln!("  Phase 2 [t{i}]: {:?}{}", input.path.file_name().unwrap_or_default(), graph_label);
            let mut builders = AllBuilders::new_streaming_in(&thread_tmp, per_thread_chunk_size)?;
            let stats = load_triple_from_one_input(input, &dict, &mut builders)?;
            eprintln!("    → {} triples ({} errors)", stats.triples_loaded, stats.errors);
            let chunks = builders.flush_and_return_chunks()?;
            Ok((chunks, stats))
        })
        .collect();

    let mut all_chunks = Vec::new();
    let mut total = LoadStats { triples_loaded: 0, lines_processed: 0, errors: 0 };
    for r in results {
        let (chunks, stats) = r?;
        all_chunks.push(chunks);
        total.triples_loaded  += stats.triples_loaded;
        total.lines_processed += stats.lines_processed;
        total.errors          += stats.errors;
    }
    Ok((all_chunks, total))
}

// ══════════════════════════════════════════════════════════════════════════════
// Streaming Phase 2 — sequential dict scan instead of random binary search
// ══════════════════════════════════════════════════════════════════════════════
//
// Problem: dict_sorted.bin for UniProt is ~640 GB (13.4 B unique terms).
// That exceeds the 64 GB RAM, so binary search causes ~34 page faults per
// lookup.  With 32 threads each calling get_id() millions of times, Phase 2
// thrashes page cache and runs for days at ~5% CPU utilization.
//
// Solution: three-step algorithm that avoids random I/O entirely.
//
// Phase 2a — Collect sorted unique strings per file (parallel, fast):
//   Same parsing as Phase 1; result is a sorted Vec<String> per file.
//
// Join — Sequential dict scan (single-threaded, but sequential I/O):
//   Scan dict_sorted.bin once.  K-way merge of per-file sorted string
//   lists against the dict stream.  Each match writes string→id into the
//   file's LocalMap.  O(N_dict + M_queries × log k).
//
// Phase 2b — Load triples using LocalMap (parallel, O(1) hash lookups):
//   Same as the existing load_triples_parallel, but lookups hit RAM instead
//   of mmap random I/O.
//
// Batch processing keeps total LocalMap memory bounded:
//   batch_size × strings_per_file × ~80 bytes ≤ ram_budget
// This causes multiple dict scans (one per batch), but each scan is fully
// sequential, so total I/O is batch_count × 640 GB at disk bandwidth.
//
// ══════════════════════════════════════════════════════════════════════════════

/// Per-file string→ID map built during the streaming Phase 2 join.
///
/// Built by [`join_batch_with_dict`] from a single sequential scan of
/// `dict_sorted.bin`.  Looked up (O(1) hash) during Phase 2b triple loading.
type LocalMap = FxHashMap<Box<str>, u64>;

/// Look up a string in a per-file [`LocalMap`]; error if absent.
#[inline]
fn lookup_local(map: &LocalMap, s: &str) -> io::Result<u64> {
    map.get(s).copied().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "term missing from LocalMap (Phase 1/2 mismatch?): {:?}",
                &s[..s.len().min(80)]
            ),
        )
    })
}

/// Collect every unique RDF term from `input`, sorted lexicographically.
///
/// Equivalent to Phase 1 string collection for a single file, but returns a
/// sorted `Vec<String>` instead of writing to a `DictBuilder`.  Used by the
/// streaming Phase 2a step to prepare per-file query lists for the join.
fn collect_strings_for_file_sorted(input: &InputSpec) -> io::Result<Vec<String>> {
    let mut set: HashSet<String> = HashSet::new();
    if let Some(g) = input.graph.as_deref() {
        set.insert(g.to_string());
    }
    let path = &input.path;
    match classify_extension(path) {
        FileKind::NTriples | FileKind::Unknown(_) => {
            visit_nt_file(path, |s, p, o| {
                set.insert(s.to_string());
                set.insert(p.to_string());
                set.insert(o.to_string());
                Ok(())
            })?;
        }
        FileKind::NTriplesGz => {
            visit_nt_file_gz(path, |s, p, o| {
                set.insert(s.to_string());
                set.insert(p.to_string());
                set.insert(o.to_string());
                Ok(())
            })?;
        }
        FileKind::NQuads => {
            visit_nq_file(path, |s, p, o, g| {
                set.insert(s.to_string());
                set.insert(p.to_string());
                set.insert(o.to_string());
                if let Some(g) = g { set.insert(g.to_string()); }
                Ok(())
            })?;
        }
        FileKind::NQuadsGz => {
            visit_nq_file_gz(path, |s, p, o, g| {
                set.insert(s.to_string());
                set.insert(p.to_string());
                set.insert(o.to_string());
                if let Some(g) = g { set.insert(g.to_string()); }
                Ok(())
            })?;
        }
    }
    let mut v: Vec<String> = set.into_iter().collect();
    v.sort_unstable();
    Ok(v)
}

/// Scan `dict_sorted.bin` once sequentially to resolve all query strings in
/// `per_file_sorted` to their dictionary IDs.
///
/// Performs a k-way merge of `per_file_sorted[i]` (one sorted string list per
/// file) against the sequential dict output.  Both sides are in lexicographic
/// order, so a single linear pass suffices.  Produces one `LocalMap` per file.
///
/// Time: O(N_dict + M_total × log k) where k = `per_file_sorted.len()`.
/// Sequential I/O: one full pass over `dict_sorted.bin` (per batch).
fn join_batch_with_dict(
    dict_path: &Path,
    per_file_sorted: &[Vec<String>],
) -> io::Result<Vec<LocalMap>> {
    let k = per_file_sorted.len();
    let mut local_maps: Vec<LocalMap> = per_file_sorted
        .iter()
        .map(|v| FxHashMap::with_capacity_and_hasher(v.len(), Default::default()))
        .collect();

    if k == 0 {
        return Ok(local_maps);
    }

    // Cursor positions into each per-file sorted list.
    let mut positions: Vec<usize> = vec![0usize; k];

    // Min-heap: (current_string_for_file, file_index).
    // We clone Strings into the heap (one per file at a time → at most k entries).
    let mut heap: BinaryHeap<Reverse<(String, usize)>> = BinaryHeap::new();
    for (i, strings) in per_file_sorted.iter().enumerate() {
        if !strings.is_empty() {
            heap.push(Reverse((strings[0].clone(), i)));
            positions[i] = 1;
        }
    }

    if heap.is_empty() {
        return Ok(local_maps);
    }

    let mut scanner = crate::dict_builder::DictScanner::open(dict_path)?;
    let mut unresolved: u64 = 0;

    // Buffered dict entry: used when dict has advanced past a query string
    // (shouldn't happen under Phase 1/2 consistency, but handled gracefully).
    let mut buffered: Option<(u64, String)> = None;

    loop {
        if heap.is_empty() {
            break;
        }

        // Fetch next dict entry (reuse buffered if available).
        let (dict_id, dict_str) = match buffered.take() {
            Some(e) => e,
            None => match scanner.next_entry()? {
                Some(e) => e,
                None => {
                    // Dict exhausted: remaining query strings are unresolved.
                    unresolved += heap.len() as u64;
                    break;
                }
            },
        };

        // Current minimum query string across all files.
        // Clone to avoid holding a borrow on `heap` while we mutate it below.
        let min_query: String = heap.peek().unwrap().0 .0.clone();

        match dict_str.as_str().cmp(min_query.as_str()) {
            std::cmp::Ordering::Less => {
                // Dict is behind the minimum query string: skip this dict entry.
            }
            std::cmp::Ordering::Equal => {
                // Resolve ALL files that have this string as their current minimum.
                // Use a borrow-safe peek-then-pop pattern: peek only to compare,
                // then pop unconditionally, so the peek borrow is released before pop.
                loop {
                    let top_matches = match heap.peek() {
                        Some(Reverse((s, _))) => s.as_str() == dict_str.as_str(),
                        None => false,
                    };
                    if !top_matches { break; }
                    let Reverse((s, fi)) = heap.pop().unwrap();
                    local_maps[fi].insert(s.into_boxed_str(), dict_id);
                    // Advance this file's cursor.
                    let pos = positions[fi];
                    if pos < per_file_sorted[fi].len() {
                        heap.push(Reverse((per_file_sorted[fi][pos].clone(), fi)));
                        positions[fi] += 1;
                    }
                }
            }
            std::cmp::Ordering::Greater => {
                // Dict has jumped past the minimum query string.
                // This indicates a Phase 1/2 mismatch (shouldn't occur in practice).
                // `min_query` is already an owned clone, so use it directly.
                // Borrow-safe peek-then-pop pattern.
                loop {
                    let top_matches = match heap.peek() {
                        Some(Reverse((s, _))) => s.as_str() == min_query.as_str(),
                        None => false,
                    };
                    if !top_matches { break; }
                    let Reverse((_s, fi)) = heap.pop().unwrap();
                    unresolved += 1;
                    let pos = positions[fi];
                    if pos < per_file_sorted[fi].len() {
                        heap.push(Reverse((per_file_sorted[fi][pos].clone(), fi)));
                        positions[fi] += 1;
                    }
                }
                // Re-use the current dict entry for the next iteration.
                buffered = Some((dict_id, dict_str));
            }
        }
    }

    if unresolved > 0 {
        eprintln!(
            "  WARNING: {} query strings not resolved during dict scan \
             (Phase 1/2 parse mismatch?)",
            unresolved
        );
    }

    Ok(local_maps)
}

/// Load triples from one file using a pre-built [`LocalMap`] instead of the
/// mmap-backed `ReadonlyDict`.
///
/// Called by [`load_triples_streaming`] in Phase 2b.  Identical logic to
/// `load_triple_from_one_input` but lookups are O(1) hash-table hits in RAM.
fn load_triple_from_one_input_local(
    input: &InputSpec,
    local_map: &LocalMap,
    builders: &mut AllBuilders,
) -> io::Result<LoadStats> {
    let path = &input.path;
    match (classify_extension(path), input.graph.as_deref()) {
        // ── N-Triples, no named graph ─────────────────────────────────────────
        (FileKind::NTriples | FileKind::Unknown(_), None) => {
            visit_nt_file(path, |s, p, o| {
                let si = lookup_local(local_map, s)?;
                let pi = lookup_local(local_map, p)?;
                let oi = lookup_local(local_map, o)?;
                builders.push(Triple::new(si, pi, oi))
            })
        }
        // ── N-Triples, named graph ────────────────────────────────────────────
        (FileKind::NTriples | FileKind::Unknown(_), Some(graph_iri)) => {
            let gi = lookup_local(local_map, graph_iri)?;
            visit_nt_file(path, |s, p, o| {
                let si = lookup_local(local_map, s)?;
                let pi = lookup_local(local_map, p)?;
                let oi = lookup_local(local_map, o)?;
                builders.push_quad(Quad::new(si, pi, oi, gi))
            })
        }
        // ── gzip N-Triples, no named graph ────────────────────────────────────
        (FileKind::NTriplesGz, None) => {
            visit_nt_file_gz(path, |s, p, o| {
                let si = lookup_local(local_map, s)?;
                let pi = lookup_local(local_map, p)?;
                let oi = lookup_local(local_map, o)?;
                builders.push(Triple::new(si, pi, oi))
            })
        }
        // ── gzip N-Triples, named graph ───────────────────────────────────────
        (FileKind::NTriplesGz, Some(graph_iri)) => {
            let gi = lookup_local(local_map, graph_iri)?;
            visit_nt_file_gz(path, |s, p, o| {
                let si = lookup_local(local_map, s)?;
                let pi = lookup_local(local_map, p)?;
                let oi = lookup_local(local_map, o)?;
                builders.push_quad(Quad::new(si, pi, oi, gi))
            })
        }
        // ── N-Quads ───────────────────────────────────────────────────────────
        (FileKind::NQuads, _) => {
            visit_nq_file(path, |s, p, o, g| {
                let si = lookup_local(local_map, s)?;
                let pi = lookup_local(local_map, p)?;
                let oi = lookup_local(local_map, o)?;
                if let Some(g) = g {
                    let gi = lookup_local(local_map, g)?;
                    builders.push_quad(Quad::new(si, pi, oi, gi))
                } else {
                    builders.push(Triple::new(si, pi, oi))
                }
            })
        }
        // ── gzip N-Quads ──────────────────────────────────────────────────────
        (FileKind::NQuadsGz, _) => {
            visit_nq_file_gz(path, |s, p, o, g| {
                let si = lookup_local(local_map, s)?;
                let pi = lookup_local(local_map, p)?;
                let oi = lookup_local(local_map, o)?;
                if let Some(g) = g {
                    let gi = lookup_local(local_map, g)?;
                    builders.push_quad(Quad::new(si, pi, oi, gi))
                } else {
                    builders.push(Triple::new(si, pi, oi))
                }
            })
        }
    }
}

/// Phase 2, **streaming** variant for dictionaries larger than RAM.
///
/// Used when `term_count` is so large that `dict_sorted.bin` does not fit in
/// memory and the mmap binary-search approach causes severe page-fault thrash.
///
/// ## Algorithm
///
/// Files are processed in *batches* to bound peak RAM:
///
/// ```text
/// for each batch of `batch_size` files:
///   Phase 2a (parallel):  parse each file → sorted unique string list
///   Join    (sequential): scan dict_sorted.bin once → build LocalMaps
///   Phase 2b (parallel):  parse each file again → triple chunks (O(1) hash lookup)
/// ```
///
/// ## Memory
///
/// Peak RAM per batch ≈ `batch_size × strings_per_file_est × 80 bytes × 2`
/// (factor of 2: per_file_sorted + local_maps coexist during the join).
/// `batch_size` is chosen so this stays within `ram_budget_bytes`.
///
/// ## I/O
///
/// Each batch requires one sequential read of `dict_sorted.bin` (~640 GB for
/// UniProt).  At 1–3 GB/s: `batch_count × 640 GB / 2 GB/s` ≈ hours, not days.
pub fn load_triples_streaming(
    inputs: &[InputSpec],
    dict_path: &Path,
    term_count: u64,
    tmp_dir: &Path,
    chunk_size: usize,
    num_threads: usize,
    ram_budget_bytes: usize,
) -> io::Result<(Vec<crate::index::ParallelChunks>, LoadStats)> {
    use rayon::prelude::*;

    let n = num_threads.max(1);
    let per_thread_chunk_size = (chunk_size / n).max(100_000);

    // Estimate per-file string count (upper bound; cross-file sharing reduces it).
    let strings_per_file_est = (term_count / inputs.len().max(1) as u64).max(1);
    // ~80 bytes per LocalMap entry (Box<str> len + u64 + FxHashMap overhead).
    // × 2 because per_file_sorted and local_maps coexist during the join.
    const BYTES_PER_ENTRY: u64 = 80;
    let batch_size = ((ram_budget_bytes as u64)
        / (strings_per_file_est * BYTES_PER_ENTRY * 2))
        .max(1)
        .min(inputs.len() as u64) as usize;

    let num_batches = (inputs.len() + batch_size - 1) / batch_size;
    eprintln!(
        "=== Streaming Phase 2: {} files / batch_size={} / {} dict scan(s) ===",
        inputs.len(),
        batch_size,
        num_batches,
    );
    eprintln!(
        "  est. {:.0} MB/batch  |  {} MB ram_budget",
        strings_per_file_est as f64 * BYTES_PER_ENTRY as f64 * batch_size as f64 * 2.0
            / (1024.0 * 1024.0),
        ram_budget_bytes / (1024 * 1024),
    );

    let mut all_chunks: Vec<crate::index::ParallelChunks> = Vec::new();
    let mut total = LoadStats { triples_loaded: 0, lines_processed: 0, errors: 0 };

    for (batch_idx, batch) in inputs.chunks(batch_size).enumerate() {
        let t_batch = std::time::Instant::now();
        eprintln!(
            "--- Batch {}/{}: {} files ---",
            batch_idx + 1,
            num_batches,
            batch.len()
        );

        // ── Phase 2a: collect sorted unique strings per file (parallel) ────────
        let t_2a = std::time::Instant::now();
        eprintln!("  Phase 2a: collecting strings...");
        let per_file_sorted: Vec<io::Result<Vec<String>>> = batch
            .par_iter()
            .enumerate()
            .map(|(i, input)| {
                eprintln!(
                    "    [2a t{}]: {:?}",
                    i,
                    input.path.file_name().unwrap_or_default()
                );
                collect_strings_for_file_sorted(input)
            })
            .collect();

        let per_file_sorted: Vec<Vec<String>> =
            per_file_sorted.into_iter().collect::<io::Result<_>>()?;

        let total_queries: usize = per_file_sorted.iter().map(|v| v.len()).sum();
        eprintln!(
            "  Phase 2a done: {} query strings ({:.1}s)",
            total_queries,
            t_2a.elapsed().as_secs_f64()
        );

        // ── Join: scan dict_sorted.bin once, build LocalMaps ──────────────────
        let t_join = std::time::Instant::now();
        eprintln!("  Join: scanning dict_sorted.bin...");
        let local_maps = join_batch_with_dict(dict_path, &per_file_sorted)?;
        eprintln!("  Join done ({:.1}s)", t_join.elapsed().as_secs_f64());

        // per_file_sorted no longer needed — release memory before Phase 2b.
        drop(per_file_sorted);

        // ── Phase 2b: load triples using LocalMaps (parallel) ─────────────────
        let t_2b = std::time::Instant::now();
        eprintln!("  Phase 2b: loading triples...");
        let results: Vec<io::Result<(crate::index::ParallelChunks, LoadStats)>> = batch
            .par_iter()
            .zip(local_maps.into_par_iter())
            .enumerate()
            .map(|(i, (input, local_map))| {
                let thread_tmp =
                    tmp_dir.join(format!("p2s_B{:04}_{:06}", batch_idx, i));
                let graph_label = input
                    .graph
                    .as_deref()
                    .map(|g| format!(" → <{}>", g))
                    .unwrap_or_default();
                eprintln!(
                    "    [2b t{}]: {:?}{}",
                    i,
                    input.path.file_name().unwrap_or_default(),
                    graph_label
                );
                let mut builders =
                    AllBuilders::new_streaming_in(&thread_tmp, per_thread_chunk_size)?;
                let stats = load_triple_from_one_input_local(input, &local_map, &mut builders)?;
                eprintln!(
                    "      → {} triples ({} errors)",
                    stats.triples_loaded, stats.errors
                );
                let chunks = builders.flush_and_return_chunks()?;
                Ok((chunks, stats))
            })
            .collect();

        for r in results {
            let (chunks, stats) = r?;
            all_chunks.push(chunks);
            total.triples_loaded += stats.triples_loaded;
            total.lines_processed += stats.lines_processed;
            total.errors += stats.errors;
        }

        eprintln!(
            "  Phase 2b done ({:.1}s)  |  Batch total {:.1}s",
            t_2b.elapsed().as_secs_f64(),
            t_batch.elapsed().as_secs_f64()
        );
    }

    Ok((all_chunks, total))
}

// ── Phase 2 mmap binary-search helpers (original path) ───────────────────────

#[inline]
fn lookup(dict: &crate::dict_builder::ReadonlyDict, s: &str) -> io::Result<u64> {
    dict.get_id(s).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("term not found in dictionary (phase 1/2 mismatch?): {:?}", &s[..s.len().min(80)]),
        )
    })
}

fn load_nt_two_pass(
    path: &Path,
    _graph: Option<&str>,
    dict: &crate::dict_builder::ReadonlyDict,
    builders: &mut AllBuilders,
) -> io::Result<LoadStats> {
    visit_nt_file(path, |s, p, o| {
        let si = lookup(dict, s)?;
        let pi = lookup(dict, p)?;
        let oi = lookup(dict, o)?;
        builders.push(crate::triple::Triple::new(si, pi, oi))
    })
}

fn load_nt_two_pass_graph(
    path: &Path,
    graph_iri: &str,
    dict: &crate::dict_builder::ReadonlyDict,
    builders: &mut AllBuilders,
) -> io::Result<LoadStats> {
    let gi = lookup(dict, graph_iri)?;
    visit_nt_file(path, |s, p, o| {
        let si = lookup(dict, s)?;
        let pi = lookup(dict, p)?;
        let oi = lookup(dict, o)?;
        builders.push_quad(crate::triple::Quad::new(si, pi, oi, gi))
    })
}

fn load_nt_two_pass_gz(
    path: &Path,
    _graph: Option<&str>,
    dict: &crate::dict_builder::ReadonlyDict,
    builders: &mut AllBuilders,
) -> io::Result<LoadStats> {
    visit_nt_file_gz(path, |s, p, o| {
        let si = lookup(dict, s)?;
        let pi = lookup(dict, p)?;
        let oi = lookup(dict, o)?;
        builders.push(crate::triple::Triple::new(si, pi, oi))
    })
}

fn load_nt_two_pass_gz_graph(
    path: &Path,
    graph_iri: &str,
    dict: &crate::dict_builder::ReadonlyDict,
    builders: &mut AllBuilders,
) -> io::Result<LoadStats> {
    let gi = lookup(dict, graph_iri)?;
    visit_nt_file_gz(path, |s, p, o| {
        let si = lookup(dict, s)?;
        let pi = lookup(dict, p)?;
        let oi = lookup(dict, o)?;
        builders.push_quad(crate::triple::Quad::new(si, pi, oi, gi))
    })
}

fn load_nq_two_pass(
    path: &Path,
    dict: &crate::dict_builder::ReadonlyDict,
    builders: &mut AllBuilders,
) -> io::Result<LoadStats> {
    visit_nq_file(path, |s, p, o, g| {
        let si = lookup(dict, s)?;
        let pi = lookup(dict, p)?;
        let oi = lookup(dict, o)?;
        if let Some(g) = g {
            let gi = lookup(dict, g)?;
            builders.push_quad(crate::triple::Quad::new(si, pi, oi, gi))
        } else {
            builders.push(crate::triple::Triple::new(si, pi, oi))
        }
    })
}

fn load_nq_two_pass_gz(
    path: &Path,
    dict: &crate::dict_builder::ReadonlyDict,
    builders: &mut AllBuilders,
) -> io::Result<LoadStats> {
    visit_nq_file_gz(path, |s, p, o, g| {
        let si = lookup(dict, s)?;
        let pi = lookup(dict, p)?;
        let oi = lookup(dict, o)?;
        if let Some(g) = g {
            let gi = lookup(dict, g)?;
            builders.push_quad(crate::triple::Quad::new(si, pi, oi, gi))
        } else {
            builders.push(crate::triple::Triple::new(si, pi, oi))
        }
    })
}

fn unescape_char(c: char) -> char {
    match c {
        'n' => '\n', 't' => '\t', 'r' => '\r',
        '\\' => '\\', '"' => '"', '\'' => '\'',
        _ => c,
    }
}

// ── Extension classification ──────────────────────────────────────────────────

enum FileKind {
    NTriples,
    NQuads,
    NTriplesGz,
    NQuadsGz,
    Unknown(String),
}

/// Classify a file path by its extension(s).
///
/// Double extensions (`.nt.gz`, `.nq.gz`) are recognised first so that a file
/// named `data.nq.gz` routes to N-Quads gzip rather than plain N-Triples gzip.
fn classify_extension(path: &Path) -> FileKind {
    // Full filename as lowercase string for suffix matching
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    // ── Double extensions (.nt.gz / .nq.gz / .ntriples.gz / .nquads.gz) ──────
    if name.ends_with(".nt.gz") || name.ends_with(".ntriples.gz") {
        return FileKind::NTriplesGz;
    }
    if name.ends_with(".nq.gz") || name.ends_with(".nquads.gz") {
        return FileKind::NQuadsGz;
    }

    // ── Single extension ──────────────────────────────────────────────────────
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "nt" | "ntriples" => FileKind::NTriples,
        "nq" | "nquads"   => FileKind::NQuads,
        // Bare .gz without a recognised inner extension → assume N-Triples
        // (backward compatibility with old behaviour)
        "gz"              => FileKind::NTriplesGz,
        other             => FileKind::Unknown(other.to_string()),
    }
}

/// Load multiple files, auto-detecting format from extension.
///
/// Convenience wrapper around [`load_files_with_graphs`] for callers that
/// only need the union graph (no named graph assignment).
pub fn load_files(
    paths: &[&Path],
    dict: &Dictionary,
    builders: &mut AllBuilders,
) -> io::Result<LoadStats> {
    let inputs: Vec<InputSpec> = paths.iter()
        .map(|p| InputSpec::plain(p.to_path_buf()))
        .collect();
    load_files_with_graphs(&inputs, dict, builders)
}

/// Load multiple files with optional per-file named graph assignment.
///
/// For each [`InputSpec`]:
/// - N-Triples / N-Triples.gz with `graph = Some(iri)`:
///   triples → union graph **and** the named graph (GSPO index).
/// - N-Triples / N-Triples.gz with `graph = None`:
///   triples → union graph only.
/// - N-Quads / N-Quads.gz:
///   per-quad graph is used; `InputSpec::graph` is ignored with a warning.
pub fn load_files_with_graphs(
    inputs: &[InputSpec],
    dict: &Dictionary,
    builders: &mut AllBuilders,
) -> io::Result<LoadStats> {
    let mut total = LoadStats { triples_loaded: 0, lines_processed: 0, errors: 0 };
    for input in inputs {
        let path = &input.path;
        let graph_label = input.graph.as_deref()
            .map(|g| format!(" → <{}>", g))
            .unwrap_or_default();
        eprintln!("Loading {:?}{}...", path.file_name().unwrap_or_default(), graph_label);

        let stats = match (classify_extension(path), input.graph.as_deref()) {
            // N-Triples with a graph name
            (FileKind::NTriples,   Some(g)) => load_ntriples_into_graph(path, g, dict, builders)?,
            (FileKind::NTriplesGz, Some(g)) => load_ntriples_into_graph_gz(path, g, dict, builders)?,
            // N-Triples without a graph name (union graph only)
            (FileKind::NTriples,   None)    => load_ntriples(path, dict, builders)?,
            (FileKind::NTriplesGz, None)    => load_ntriples_gz(path, dict, builders)?,
            // N-Quads: per-quad graph wins; graph override is ignored
            (FileKind::NQuads,   Some(_)) => {
                eprintln!("  Note: N-Quads already carry per-quad graph names; --graph override ignored.");
                load_nquads(path, dict, builders)?
            }
            (FileKind::NQuadsGz, Some(_)) => {
                eprintln!("  Note: N-Quads already carry per-quad graph names; --graph override ignored.");
                load_nquads_gz(path, dict, builders)?
            }
            (FileKind::NQuads,   None)    => load_nquads(path, dict, builders)?,
            (FileKind::NQuadsGz, None)    => load_nquads_gz(path, dict, builders)?,
            // Unknown extension: fall back to N-Triples (with or without graph)
            (FileKind::Unknown(ext), g_opt) => {
                eprintln!("  Warning: unknown extension '{}', trying N-Triples", ext);
                match g_opt {
                    Some(g) => load_ntriples_into_graph(path, g, dict, builders)?,
                    None    => load_ntriples(path, dict, builders)?,
                }
            }
        };

        total.triples_loaded += stats.triples_loaded;
        total.lines_processed += stats.lines_processed;
        total.errors += stats.errors;
        eprintln!("  → {} triples ({} errors)", stats.triples_loaded, stats.errors);
    }
    Ok(total)
}

// ── N-Quads loader ────────────────────────────────────────────────────────────

/// Load an N-Quads file (.nq) into the dictionary and index builder.
///
/// Format: `<subject> <predicate> <object> [<graph>] .`
/// Triples with a graph IRI are stored in both the union (SPO/POS/OSP) and
/// the GSPO named-graph index.  Triples without a graph go to the union only.
pub fn load_nquads(
    path: &Path,
    dict: &Dictionary,
    builders: &mut AllBuilders,
) -> io::Result<LoadStats> {
    let file = File::open(path)?;
    let reader = BufReader::with_capacity(4 * 1024 * 1024, file);
    let mut stats = LoadStats { triples_loaded: 0, lines_processed: 0, errors: 0 };

    for line in reader.lines() {
        let line = line?;
        stats.lines_processed += 1;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') { continue; }

        match parse_nq_line(line) {
            Some((s, p, o, g_opt)) => {
                let si = dict.encode(&s);
                let pi = dict.encode(&p);
                let oi = dict.encode(&o);
                if let Some(g) = g_opt {
                    let gi = dict.encode(&g);
                    builders.push_quad(Quad::new(si, pi, oi, gi))?;
                } else {
                    builders.push(Triple::new(si, pi, oi))?;
                }
                stats.triples_loaded += 1;
                if stats.triples_loaded % 1_000_000 == 0 {
                    eprintln!("  loaded {}M quads...", stats.triples_loaded / 1_000_000);
                }
            }
            None => {
                stats.errors += 1;
                if stats.errors <= 10 {
                    eprintln!("  parse error on line {}: {:?}",
                        stats.lines_processed, &line[..line.len().min(80)]);
                }
            }
        }
    }
    Ok(stats)
}

/// Parse a single N-Quads line.
/// Returns (subject, predicate, object, Option<graph>) as canonical strings.
fn parse_nq_line(line: &str) -> Option<(String, String, String, Option<String>)> {
    let line = line.strip_suffix('.').unwrap_or(line).trim();
    let mut parts = Vec::new();
    let mut chars = line.chars().peekable();

    while parts.len() < 4 {
        while chars.peek() == Some(&' ') || chars.peek() == Some(&'\t') { chars.next(); }
        if chars.peek().is_none() { break; }

        let term = match chars.peek()? {
            '<' => {
                chars.next();
                let mut s = String::new();
                while let Some(c) = chars.next() {
                    if c == '>' { break; }
                    if c == '\\' { if let Some(e) = chars.next() { s.push(unescape_char(e)); } }
                    else { s.push(c); }
                }
                s
            }
            '"' => {
                chars.next();
                let mut s = String::from("\"");
                let mut in_string = true;
                while let Some(c) = chars.next() {
                    if in_string {
                        if c == '"' { s.push('"'); in_string = false; }
                        else if c == '\\' { if let Some(e) = chars.next() { s.push('\\'); s.push(e); } }
                        else { s.push(c); }
                    } else {
                        match c {
                            '@' => {
                                s.push('@');
                                while let Some(&nc) = chars.peek() {
                                    if nc == ' ' || nc == '\t' { break; }
                                    s.push(nc); chars.next();
                                }
                                break;
                            }
                            '^' => {
                                chars.next(); s.push_str("^^<"); chars.next();
                                while let Some(nc) = chars.next() {
                                    if nc == '>' { break; }
                                    s.push(nc);
                                }
                                s.push('>');
                                break;
                            }
                            _ => break,
                        }
                    }
                }
                s
            }
            '_' => {
                let mut s = String::new();
                while let Some(&c) = chars.peek() {
                    if c == ' ' || c == '\t' { break; }
                    s.push(c); chars.next();
                }
                s
            }
            _ => break,
        };
        parts.push(term);
    }

    if parts.len() >= 3 {
        let o = parts.remove(2);
        let p = parts.remove(1);
        let s = parts.remove(0);
        let g = if !parts.is_empty() { Some(parts.remove(0)) } else { None };
        Some((s, p, o, g))
    } else {
        None
    }
}

/// Load gzipped N-Triples (.nt.gz).
/// Enable with: `cargo build --features gzip`
#[cfg(feature = "gzip")]
pub fn load_ntriples_gz(
    path: &Path,
    dict: &Dictionary,
    builders: &mut AllBuilders,
) -> io::Result<LoadStats> {
    use flate2::read::GzDecoder;
    let file = File::open(path)?;
    let gz = GzDecoder::new(file);
    let reader = BufReader::new(gz);
    // Re-use the same line-by-line logic
    let mut stats = LoadStats { triples_loaded: 0, lines_processed: 0, errors: 0 };
    for line in reader.lines() {
        let line = line?;
        stats.lines_processed += 1;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') { continue; }
        match parse_nt_line(line) {
            Some((s, p, o)) => {
                let si = dict.encode(&s);
                let pi = dict.encode(&p);
                let oi = dict.encode(&o);
                builders.push(crate::triple::Triple::new(si, pi, oi))?;
                stats.triples_loaded += 1;
            }
            None => { stats.errors += 1; }
        }
    }
    Ok(stats)
}

#[cfg(not(feature = "gzip"))]
pub fn load_ntriples_gz(
    _path: &Path,
    _dict: &Dictionary,
    _builders: &mut AllBuilders,
) -> io::Result<LoadStats> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "gzip support not compiled in — rebuild with: cargo build --features gzip",
    ))
}

/// Load gzipped N-Quads (.nq.gz).
/// Enable with: `cargo build --features gzip`
#[cfg(feature = "gzip")]
pub fn load_nquads_gz(
    path: &Path,
    dict: &Dictionary,
    builders: &mut AllBuilders,
) -> io::Result<LoadStats> {
    use flate2::read::GzDecoder;
    let file = File::open(path)?;
    let gz = GzDecoder::new(file);
    let reader = BufReader::new(gz);
    let mut stats = LoadStats { triples_loaded: 0, lines_processed: 0, errors: 0 };

    for line in reader.lines() {
        let line = line?;
        stats.lines_processed += 1;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') { continue; }

        match parse_nq_line(line) {
            Some((s, p, o, g_opt)) => {
                let si = dict.encode(&s);
                let pi = dict.encode(&p);
                let oi = dict.encode(&o);
                if let Some(g) = g_opt {
                    let gi = dict.encode(&g);
                    builders.push_quad(Quad::new(si, pi, oi, gi))?;
                } else {
                    builders.push(Triple::new(si, pi, oi))?;
                }
                stats.triples_loaded += 1;
                if stats.triples_loaded % 1_000_000 == 0 {
                    eprintln!("  loaded {}M quads...", stats.triples_loaded / 1_000_000);
                }
            }
            None => { stats.errors += 1; }
        }
    }
    Ok(stats)
}

#[cfg(not(feature = "gzip"))]
pub fn load_nquads_gz(
    _path: &Path,
    _dict: &Dictionary,
    _builders: &mut AllBuilders,
) -> io::Result<LoadStats> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "gzip support not compiled in — rebuild with: cargo build --features gzip",
    ))
}

// ── N-Triples into named graph ────────────────────────────────────────────────

/// Load an N-Triples file into a specific named graph.
///
/// Each triple is added to both the union graph (SPO/POS/OSP indexes) and the
/// named graph (GSPO index) so it is visible from both union-graph queries and
/// `GRAPH <iri> { … }` patterns.
pub fn load_ntriples_into_graph(
    path: &Path,
    graph_iri: &str,
    dict: &Dictionary,
    builders: &mut AllBuilders,
) -> io::Result<LoadStats> {
    let file = File::open(path)?;
    let reader = BufReader::with_capacity(4 * 1024 * 1024, file);
    let gi = dict.encode(graph_iri);
    let mut stats = LoadStats { triples_loaded: 0, lines_processed: 0, errors: 0 };

    for line in reader.lines() {
        let line = line?;
        stats.lines_processed += 1;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') { continue; }

        match parse_nt_line(line) {
            Some((s, p, o)) => {
                let si = dict.encode(&s);
                let pi = dict.encode(&p);
                let oi = dict.encode(&o);
                builders.push_quad(Quad::new(si, pi, oi, gi))?;
                stats.triples_loaded += 1;
                if stats.triples_loaded % 1_000_000 == 0 {
                    eprintln!("  loaded {}M triples...", stats.triples_loaded / 1_000_000);
                }
            }
            None => {
                stats.errors += 1;
                if stats.errors <= 10 {
                    eprintln!("  parse error on line {}: {:?}",
                        stats.lines_processed, &line[..line.len().min(80)]);
                }
            }
        }
    }
    Ok(stats)
}

/// Load a gzip-compressed N-Triples file into a specific named graph.
/// Enable with: `cargo build --features gzip`
#[cfg(feature = "gzip")]
pub fn load_ntriples_into_graph_gz(
    path: &Path,
    graph_iri: &str,
    dict: &Dictionary,
    builders: &mut AllBuilders,
) -> io::Result<LoadStats> {
    use flate2::read::GzDecoder;
    let file = File::open(path)?;
    let gz = GzDecoder::new(file);
    let reader = BufReader::new(gz);
    let gi = dict.encode(graph_iri);
    let mut stats = LoadStats { triples_loaded: 0, lines_processed: 0, errors: 0 };

    for line in reader.lines() {
        let line = line?;
        stats.lines_processed += 1;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') { continue; }

        match parse_nt_line(line) {
            Some((s, p, o)) => {
                let si = dict.encode(&s);
                let pi = dict.encode(&p);
                let oi = dict.encode(&o);
                builders.push_quad(Quad::new(si, pi, oi, gi))?;
                stats.triples_loaded += 1;
                if stats.triples_loaded % 1_000_000 == 0 {
                    eprintln!("  loaded {}M triples...", stats.triples_loaded / 1_000_000);
                }
            }
            None => { stats.errors += 1; }
        }
    }
    Ok(stats)
}

#[cfg(not(feature = "gzip"))]
pub fn load_ntriples_into_graph_gz(
    _path: &Path,
    _graph_iri: &str,
    _dict: &Dictionary,
    _builders: &mut AllBuilders,
) -> io::Result<LoadStats> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "gzip support not compiled in — rebuild with: cargo build --features gzip",
    ))
}
