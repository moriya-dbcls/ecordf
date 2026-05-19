//! # EcoRDF Store — top-level facade
//!
//! Wraps dictionary + triple indexes and provides the main public API:
//! - `Store::load()` — build from RDF files
//! - `Store::open()` — reopen an existing store
//! - `Store::query()` — execute a SPARQL query string
//! - `Store::ask()` — ASK query → bool

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::dict::Dictionary;
use crate::index::{AllBuilders, TripleIndex};
use crate::loader::load_files;
use crate::sparql::{parse_query, Executor, ResultSet};
use crate::sparql::ast::QueryForm;

pub struct Store {
    pub dict: Dictionary,
    pub index: TripleIndex,
    pub dir: PathBuf,
}

impl Store {
    /// Build a new store from RDF input files.
    ///
    /// Creates `dir` if it doesn't exist, writes dict.bin + spo.bin/pos.bin/osp.bin.
    pub fn load(dir: &Path, input_files: &[&Path]) -> io::Result<Self> {
        fs::create_dir_all(dir)?;

        let t0 = Instant::now();
        eprintln!("=== EcoRDF: loading {} file(s) ===", input_files.len());

        let dict = Dictionary::new();
        let mut builders = AllBuilders::new();

        let stats = load_files(input_files, &dict, &mut builders)?;

        eprintln!(
            "Parsed {} triples in {:.1}s. Sorting and writing indexes...",
            stats.triples_loaded,
            t0.elapsed().as_secs_f64()
        );

        let t1 = Instant::now();
        let index = builders.build(dir)?;
        dict.save(&dir.join("dict.bin"))?;

        eprintln!(
            "Indexes written in {:.1}s. Total load time: {:.1}s",
            t1.elapsed().as_secs_f64(),
            t0.elapsed().as_secs_f64()
        );
        eprintln!(
            "Dictionary: {} terms | Triples: {}",
            dict.len(),
            index.triple_count()
        );

        Ok(Self { dict, index, dir: dir.to_path_buf() })
    }

    /// Reopen an existing store from its directory.
    pub fn open(dir: &Path) -> io::Result<Self> {
        let dict = Dictionary::load(&dir.join("dict.bin"))?;
        let index = TripleIndex::open(dir)?;
        Ok(Self { dict, index, dir: dir.to_path_buf() })
    }

    /// Execute a SPARQL query string and return the result set.
    pub fn query(&self, sparql: &str) -> Result<QueryResult, QueryError> {
        let t0 = Instant::now();
        let ast = parse_query(sparql).map_err(|e| QueryError::Parse(e.to_string()))?;

        let executor = Executor::new(&self.index, &self.dict);

        let result = match ast.form {
            QueryForm::Select(ref sq) => {
                let rs = executor.execute_select(sq);
                QueryResult::Select(rs)
            }
            QueryForm::Ask(ref aq) => {
                let b = executor.execute_ask(aq);
                QueryResult::Ask(b)
            }
            QueryForm::Construct(_) => {
                return Err(QueryError::Unsupported("CONSTRUCT not yet implemented".into()));
            }
        };

        let elapsed = t0.elapsed();
        tracing::debug!(
            elapsed_ms = elapsed.as_millis(),
            "query executed"
        );

        Ok(result)
    }

    /// Convenience: execute a SPARQL query and return rows as strings.
    pub fn query_to_table(&self, sparql: &str) -> Result<Vec<Vec<String>>, QueryError> {
        match self.query(sparql)? {
            QueryResult::Select(rs) => {
                if rs.overflow {
                    return Err(QueryError::Unsupported(format!(
                        "Query result exceeded memory limit ({} rows). \
                         Add LIMIT / tighter FILTER to reduce result size.",
                        rs.rows.len()
                    )));
                }
                let mut rows = Vec::new();
                // Header
                rows.push(rs.variables.clone());
                // Data rows
                for row in &rs.rows {
                    let str_row: Vec<String> = row.iter().map(|cell| {
                        match cell {
                            Some(id) => self.dict.display(*id),
                            None => String::new(),
                        }
                    }).collect();
                    rows.push(str_row);
                }
                Ok(rows)
            }
            QueryResult::Ask(b) => Ok(vec![vec![b.to_string()]]),
        }
    }

    /// Statistics about the store.
    pub fn stats(&self) -> StoreStats {
        StoreStats {
            triple_count: self.index.triple_count(),
            term_count: self.dict.len(),
            graph_count: self.index.graph_count(),
            dir: self.dir.clone(),
        }
    }
}

pub enum QueryResult {
    Select(ResultSet),
    Ask(bool),
}

#[derive(Debug)]
pub enum QueryError {
    Parse(String),
    Unsupported(String),
    Io(io::Error),
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QueryError::Parse(s) => write!(f, "Parse error: {}", s),
            QueryError::Unsupported(s) => write!(f, "Unsupported: {}", s),
            QueryError::Io(e) => write!(f, "I/O error: {}", e),
        }
    }
}

pub struct StoreStats {
    pub triple_count: usize,
    pub term_count: usize,
    pub graph_count: usize,
    pub dir: PathBuf,
}
