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
                builders.push(Triple::new(si, pi, oi));
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
                    builders.push_quad(Quad::new(si, pi, oi, gi));
                } else {
                    builders.push(Triple::new(si, pi, oi));
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
                builders.push(crate::triple::Triple::new(si, pi, oi));
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
                    builders.push_quad(Quad::new(si, pi, oi, gi));
                } else {
                    builders.push(Triple::new(si, pi, oi));
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
                builders.push_quad(Quad::new(si, pi, oi, gi));
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
                builders.push_quad(Quad::new(si, pi, oi, gi));
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
