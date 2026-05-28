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
use std::collections::BinaryHeap;
use std::fs;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};

use crate::dict::Dictionary;
use crate::dict_builder::{DictBuilder, DictScanner, LocalDict, LocalDictBuilder};
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
// Streaming Phase 2 — disk-based string collection + LocalDict join
// ══════════════════════════════════════════════════════════════════════════════
//
// OOM problem with the original approach:
//   collect_strings_for_file_sorted used HashSet<String>  → 313M strings × ~100B = ~31 GB
//   join_batch_with_dict built FxHashMap<Box<str>, u64>   → 313M × ~80B  = ~25 GB
//   Both coexist during the join step → OOM before first triple is written.
//
// New approach: all intermediate string state lives on disk.
//
// Phase 2a — Collect sorted unique strings per file to disk (parallel):
//   Use DictBuilder (external sort, bounded RAM) → write ESRT0001 p2a file.
//   Peak RAM per thread = P2A_BUF_BYTES (64 MB).  No HashSet in RAM.
//
// Join — Sequential dict scan, build LocalDict files (sequential, per batch):
//   Open one DictScanner per p2a file in the batch.
//   K-way merge p2a scanners against main dict_sorted.bin.
//   For each match: write (global_id, string) to LocalDictBuilder.
//   At most (max_batch × 3 + 1) file descriptors open simultaneously.
//   Peak RAM: k-way heap (k strings at a time) — negligible.
//
// Phase 2b — Load triples using LocalDict binary search (parallel):
//   LocalDict is mmap-backed: O(log N) per lookup.
//   Much smaller than full HashSet: only strings actually in this file.
//
// Batch sizing — by fd limit, not RAM:
//   max_batch = (fd_soft_limit() - 64) / 3
//   (Each file in batch needs 3 fds during join: p2a scanner + 2 ld builder tmps)
//
// ══════════════════════════════════════════════════════════════════════════════

/// Collect all unique RDF terms from `input` and write them as a sorted,
/// deduplicated ESRT0001 file to `out_path` using an external sort.
///
/// Uses [`DictBuilder`] with `budget_bytes` RAM so no `HashSet` is allocated.
/// Returns the count of unique strings written.
fn collect_strings_for_file_to_disk(
    input: &InputSpec,
    tmp_dir: &Path,
    out_path: &Path,
    budget_bytes: usize,
) -> io::Result<u64> {
    // Per-file sub-tmp dir so chunk names don't collide across parallel calls.
    let file_tmp = tmp_dir.join(format!(
        "p2a_{}",
        out_path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("x")
    ));
    let mut builder = DictBuilder::new(&file_tmp, budget_bytes)?;

    if let Some(g) = input.graph.as_deref() {
        builder.add(g)?;
    }
    let path = &input.path;
    match classify_extension(path) {
        FileKind::NTriples | FileKind::Unknown(_) => {
            visit_nt_file(path, |s, p, o| {
                builder.add(s)?;
                builder.add(p)?;
                builder.add(o)
            })?;
        }
        FileKind::NTriplesGz => {
            visit_nt_file_gz(path, |s, p, o| {
                builder.add(s)?;
                builder.add(p)?;
                builder.add(o)
            })?;
        }
        FileKind::NQuads => {
            visit_nq_file(path, |s, p, o, g| {
                builder.add(s)?;
                builder.add(p)?;
                builder.add(o)?;
                if let Some(g) = g { builder.add(g)?; }
                Ok(())
            })?;
        }
        FileKind::NQuadsGz => {
            visit_nq_file_gz(path, |s, p, o, g| {
                builder.add(s)?;
                builder.add(p)?;
                builder.add(o)?;
                if let Some(g) = g { builder.add(g)?; }
                Ok(())
            })?;
        }
    }
    let count = builder.finish(out_path)?;
    // Clean up the per-file chunk sub-dir.
    let _ = fs::remove_dir_all(&file_tmp);
    Ok(count)
}

/// Scan `dict_sorted.bin` once to resolve all query strings in `batch_p2a`
/// (one ESRT0001 p2a file per input file) to their global dictionary IDs.
///
/// Performs a k-way merge of the p2a [`DictScanner`] readers against the
/// main dict scanner.  On each match writes `(global_id, string)` to the
/// corresponding [`LocalDictBuilder`].  Produces one ELOC0001 `local_dict.bin`
/// per input file.
///
/// Returns the paths of the produced LocalDict files (same order as `batch_p2a`).
///
/// Time: O(N_dict + M_total × log k) — one sequential pass over `dict_sorted.bin`.
fn join_batch_with_dict_to_disk(
    dict_path: &Path,
    batch_p2a: &[(PathBuf, u64)],
    tmp_dir: &Path,
    batch_idx: usize,
) -> io::Result<Vec<PathBuf>> {
    let k = batch_p2a.len();
    if k == 0 {
        return Ok(vec![]);
    }

    // Output paths for LocalDict files.
    let ld_paths: Vec<PathBuf> = (0..k)
        .map(|i| tmp_dir.join(format!("ld_B{:04}_{:06}.bin", batch_idx, i)))
        .collect();

    // Open DictScanner for each p2a file.
    let mut p2a_scanners: Vec<DictScanner> = batch_p2a.iter()
        .map(|(p, _)| DictScanner::open(p))
        .collect::<io::Result<_>>()?;

    // Create LocalDictBuilder for each file.
    let mut ld_builders: Vec<LocalDictBuilder> = (0..k)
        .map(|i| LocalDictBuilder::new(tmp_dir, batch_idx * 100_000 + i))
        .collect::<io::Result<_>>()?;

    // Min-heap: (current_string_from_p2a, file_idx).
    let mut heap: BinaryHeap<Reverse<(String, usize)>> = BinaryHeap::new();
    for (i, scanner) in p2a_scanners.iter_mut().enumerate() {
        if let Some((_id, s)) = scanner.next_entry()? {
            heap.push(Reverse((s, i)));
        }
    }

    if heap.is_empty() {
        // All p2a files empty — finish with empty LocalDicts.
        for (i, builder) in ld_builders.into_iter().enumerate() {
            builder.finish(&ld_paths[i])?;
        }
        return Ok(ld_paths);
    }

    let mut main_scanner = DictScanner::open(dict_path)?;
    let mut buffered: Option<(u64, String)> = None;
    let mut unresolved: u64 = 0;

    loop {
        if heap.is_empty() {
            break;
        }

        // Fetch next main-dict entry (reuse buffered if available).
        let (dict_id, dict_str) = match buffered.take() {
            Some(e) => e,
            None => match main_scanner.next_entry()? {
                Some(e) => e,
                None => {
                    // Dict exhausted: remaining query strings are unresolved.
                    unresolved += heap.len() as u64;
                    break;
                }
            },
        };

        // Minimum query string across all p2a files.
        let min_query: String = heap.peek().unwrap().0.0.clone();

        match dict_str.as_str().cmp(min_query.as_str()) {
            std::cmp::Ordering::Less => {
                // Main dict is behind the minimum query: skip this dict entry.
            }
            std::cmp::Ordering::Equal => {
                // Resolve every p2a file whose current minimum equals dict_str.
                loop {
                    let top_matches = match heap.peek() {
                        Some(Reverse((s, _))) => s.as_str() == dict_str.as_str(),
                        None => false,
                    };
                    if !top_matches { break; }
                    let Reverse((s, fi)) = heap.pop().unwrap();
                    ld_builders[fi].add(dict_id, &s)?;
                    // Advance this p2a scanner.
                    if let Some((_id, next_s)) = p2a_scanners[fi].next_entry()? {
                        heap.push(Reverse((next_s, fi)));
                    }
                }
            }
            std::cmp::Ordering::Greater => {
                // Main dict jumped past min_query: Phase 1/2 mismatch.
                loop {
                    let top_matches = match heap.peek() {
                        Some(Reverse((s, _))) => s.as_str() == min_query.as_str(),
                        None => false,
                    };
                    if !top_matches { break; }
                    let Reverse((_s, fi)) = heap.pop().unwrap();
                    unresolved += 1;
                    if let Some((_id, next_s)) = p2a_scanners[fi].next_entry()? {
                        heap.push(Reverse((next_s, fi)));
                    }
                }
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

    drop(p2a_scanners);

    // Finish all LocalDictBuilders → ELOC0001 files.
    for (i, builder) in ld_builders.into_iter().enumerate() {
        builder.finish(&ld_paths[i])?;
    }

    Ok(ld_paths)
}

/// Look up a string in a [`LocalDict`]; return an error if absent.
#[inline]
fn lookup_ld(dict: &LocalDict, s: &str) -> io::Result<u64> {
    dict.get_id(s).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "term missing from LocalDict (Phase 1/2 mismatch?): {:?}",
                &s[..s.len().min(80)]
            ),
        )
    })
}

/// Load triples from one file using a mmap-backed [`LocalDict`].
///
/// Called by [`load_triples_streaming`] in Phase 2b.  O(log N) binary-search
/// lookups replace the former hash-table approach; the LocalDict is small
/// (only this file's strings) so it fits in OS page cache easily.
fn load_triple_from_one_input_local_dict(
    input: &InputSpec,
    local_dict: &LocalDict,
    builders: &mut AllBuilders,
) -> io::Result<LoadStats> {
    let path = &input.path;
    match (classify_extension(path), input.graph.as_deref()) {
        // ── N-Triples, no named graph ─────────────────────────────────────────
        (FileKind::NTriples | FileKind::Unknown(_), None) => {
            visit_nt_file(path, |s, p, o| {
                let si = lookup_ld(local_dict, s)?;
                let pi = lookup_ld(local_dict, p)?;
                let oi = lookup_ld(local_dict, o)?;
                builders.push(Triple::new(si, pi, oi))
            })
        }
        // ── N-Triples, named graph ────────────────────────────────────────────
        (FileKind::NTriples | FileKind::Unknown(_), Some(graph_iri)) => {
            let gi = lookup_ld(local_dict, graph_iri)?;
            visit_nt_file(path, |s, p, o| {
                let si = lookup_ld(local_dict, s)?;
                let pi = lookup_ld(local_dict, p)?;
                let oi = lookup_ld(local_dict, o)?;
                builders.push_quad(Quad::new(si, pi, oi, gi))
            })
        }
        // ── gzip N-Triples, no named graph ────────────────────────────────────
        (FileKind::NTriplesGz, None) => {
            visit_nt_file_gz(path, |s, p, o| {
                let si = lookup_ld(local_dict, s)?;
                let pi = lookup_ld(local_dict, p)?;
                let oi = lookup_ld(local_dict, o)?;
                builders.push(Triple::new(si, pi, oi))
            })
        }
        // ── gzip N-Triples, named graph ───────────────────────────────────────
        (FileKind::NTriplesGz, Some(graph_iri)) => {
            let gi = lookup_ld(local_dict, graph_iri)?;
            visit_nt_file_gz(path, |s, p, o| {
                let si = lookup_ld(local_dict, s)?;
                let pi = lookup_ld(local_dict, p)?;
                let oi = lookup_ld(local_dict, o)?;
                builders.push_quad(Quad::new(si, pi, oi, gi))
            })
        }
        // ── N-Quads ───────────────────────────────────────────────────────────
        (FileKind::NQuads, _) => {
            visit_nq_file(path, |s, p, o, g| {
                let si = lookup_ld(local_dict, s)?;
                let pi = lookup_ld(local_dict, p)?;
                let oi = lookup_ld(local_dict, o)?;
                if let Some(g) = g {
                    let gi = lookup_ld(local_dict, g)?;
                    builders.push_quad(Quad::new(si, pi, oi, gi))
                } else {
                    builders.push(Triple::new(si, pi, oi))
                }
            })
        }
        // ── gzip N-Quads ──────────────────────────────────────────────────────
        (FileKind::NQuadsGz, _) => {
            visit_nq_file_gz(path, |s, p, o, g| {
                let si = lookup_ld(local_dict, s)?;
                let pi = lookup_ld(local_dict, p)?;
                let oi = lookup_ld(local_dict, o)?;
                if let Some(g) = g {
                    let gi = lookup_ld(local_dict, g)?;
                    builders.push_quad(Quad::new(si, pi, oi, gi))
                } else {
                    builders.push(Triple::new(si, pi, oi))
                }
            })
        }
    }
}

/// Phase 2 **disk-based streaming** variant for dictionaries larger than RAM.
///
/// ## Algorithm
///
/// ```text
/// Phase 2a (parallel, all files at once):
///   parse each file → DictBuilder → p2a_N.esrt on disk (no HashSet in RAM)
///
/// For each batch of max_batch files:
///   Join (sequential):
///     k-way merge of batch's p2a DictScanners vs. main dict scanner
///     → write LocalDict files (ELOC0001) on disk
///   Phase 2b (parallel):
///     mmap LocalDict, load triples with O(log N) binary-search lookup
///     delete p2a + LocalDict files for this batch
/// ```
///
/// ## Memory
///
/// Phase 2a: `num_threads × 64 MB` (external-sort buffer per thread).
/// Join: k-way heap of at most `max_batch` strings — negligible.
/// Phase 2b: OS page cache for LocalDict files (evicted under pressure).
///
/// ## Batch sizing
///
/// `max_batch = (fd_soft_limit() - 64) / 3`
///
/// Each file in a join batch needs 3 fds:
///   - 1 for the p2a `DictScanner`
///   - 2 for the `LocalDictBuilder` temporary files (strings + offsets)
///
/// The main dict scanner adds 1 more, and 64 is reserved for system overhead.
pub fn load_triples_streaming(
    inputs: &[InputSpec],
    dict_path: &Path,
    _term_count: u64,
    tmp_dir: &Path,
    chunk_size: usize,
    num_threads: usize,
    _ram_budget_bytes: usize,
) -> io::Result<(Vec<crate::index::ParallelChunks>, LoadStats)> {
    use rayon::prelude::*;
    use crate::dict_builder::fd_soft_limit;

    let n = num_threads.max(1);
    let per_thread_chunk_size = (chunk_size / n).max(100_000);

    // Per-file external-sort buffer for Phase 2a (64 MB per file/thread).
    const P2A_BUF_BYTES: usize = 64 * 1024 * 1024;

    // fd-based batch size: each file uses 3 fds during the join.
    let fds = fd_soft_limit();
    let max_batch = ((fds.saturating_sub(64)) / 3).max(1).min(inputs.len());

    let p2a_dir = tmp_dir.join("p2a");
    fs::create_dir_all(&p2a_dir)?;

    let num_batches = (inputs.len() + max_batch - 1) / max_batch;
    eprintln!(
        "=== Streaming Phase 2 (disk-based): {} files / max_batch={} / {} batch(es) ===",
        inputs.len(),
        max_batch,
        num_batches,
    );
    eprintln!(
        "  fd_soft_limit={}  P2A_BUF={}MB",
        fds,
        P2A_BUF_BYTES / (1024 * 1024),
    );

    // ── Phase 2a: ALL files → p2a files on disk (parallel) ───────────────────
    let t_2a = std::time::Instant::now();
    eprintln!("  Phase 2a: collecting strings to disk ({} threads)...", n);

    let p2a_results: Vec<io::Result<(PathBuf, u64)>> = inputs
        .par_iter()
        .enumerate()
        .map(|(i, input)| {
            let out = p2a_dir.join(format!("p2a_{:06}.esrt", i));
            let count =
                collect_strings_for_file_to_disk(input, &p2a_dir, &out, P2A_BUF_BYTES)?;
            Ok((out, count))
        })
        .collect();

    let p2a_files: Vec<(PathBuf, u64)> =
        p2a_results.into_iter().collect::<io::Result<_>>()?;

    let total_query_strings: u64 = p2a_files.iter().map(|(_, c)| c).sum();
    eprintln!(
        "  Phase 2a done: {} total query strings ({:.1}s)",
        total_query_strings,
        t_2a.elapsed().as_secs_f64()
    );

    // ── Batched join + Phase 2b ────────────────────────────────────────────────
    let mut all_chunks: Vec<crate::index::ParallelChunks> = Vec::new();
    let mut total = LoadStats { triples_loaded: 0, lines_processed: 0, errors: 0 };

    for (batch_idx, (batch_inputs, batch_p2a)) in inputs
        .chunks(max_batch)
        .zip(p2a_files.chunks(max_batch))
        .enumerate()
    {
        let t_batch = std::time::Instant::now();
        eprintln!(
            "--- Batch {}/{}: {} files ---",
            batch_idx + 1,
            num_batches,
            batch_inputs.len()
        );

        // ── Join: scan dict_sorted.bin → LocalDict files on disk ──────────────
        let t_join = std::time::Instant::now();
        eprintln!(
            "  Join: scanning dict_sorted.bin → {} LocalDict files...",
            batch_p2a.len()
        );
        let ld_paths =
            join_batch_with_dict_to_disk(dict_path, batch_p2a, tmp_dir, batch_idx)?;
        eprintln!("  Join done ({:.1}s)", t_join.elapsed().as_secs_f64());

        // ── Phase 2b: load triples via LocalDict (parallel) ───────────────────
        let t_2b = std::time::Instant::now();
        eprintln!("  Phase 2b: loading triples...");

        let results: Vec<io::Result<(crate::index::ParallelChunks, LoadStats)>> = batch_inputs
            .par_iter()
            .zip(ld_paths.par_iter())
            .enumerate()
            .map(|(i, (input, ld_path))| {
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
                let local_dict = LocalDict::open(ld_path)?;
                let mut builders =
                    AllBuilders::new_streaming_in(&thread_tmp, per_thread_chunk_size)?;
                let stats =
                    load_triple_from_one_input_local_dict(input, &local_dict, &mut builders)?;
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

        // Clean up this batch's p2a and LocalDict temp files.
        for (p2a_path, _) in batch_p2a {
            let _ = fs::remove_file(p2a_path);
        }
        for ld_path in &ld_paths {
            let _ = fs::remove_file(ld_path);
        }

        eprintln!(
            "  Phase 2b done ({:.1}s)  |  Batch total {:.1}s",
            t_2b.elapsed().as_secs_f64(),
            t_batch.elapsed().as_secs_f64()
        );
    }

    // Clean up Phase 2a directory.
    let _ = fs::remove_dir_all(&p2a_dir);

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
