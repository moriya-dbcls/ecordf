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

use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};

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
