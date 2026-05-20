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

use crate::config::Config;
use crate::dict::Dictionary;
use crate::index::{AllBuilders, TripleIndex};
use crate::loader::load_files;
use crate::sparql::{parse_query, Executor, ResultSet};
use crate::sparql::ast::{Expression, Projection, QueryForm, SelectItem};

pub struct Store {
    pub dict: Dictionary,
    pub index: TripleIndex,
    pub dir: PathBuf,
    pub config: Config,
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

        // Config is loaded from the store dir (if ecordf.toml exists there).
        // At build time it may not exist yet — fall back to defaults in that case.
        let config = Config::resolve(None, dir).map_err(io::Error::from)?;
        Ok(Self { dict, index, dir: dir.to_path_buf(), config })
    }

    /// Reopen an existing store from its directory.
    pub fn open(dir: &Path) -> io::Result<Self> {
        let dict = Dictionary::load(&dir.join("dict.bin"))?;
        let index = TripleIndex::open(dir)?;
        let config = Config::resolve(None, dir).map_err(io::Error::from)?;
        Ok(Self { dict, index, dir: dir.to_path_buf(), config })
    }

    /// Reopen an existing store and apply an explicit config file (overrides
    /// the auto-detected `<store-dir>/ecordf.toml`).
    pub fn open_with_config(dir: &Path, config_path: Option<&std::path::Path>) -> io::Result<Self> {
        let dict = Dictionary::load(&dir.join("dict.bin"))?;
        let index = TripleIndex::open(dir)?;
        let config = Config::resolve(config_path, dir).map_err(io::Error::from)?;
        Ok(Self { dict, index, dir: dir.to_path_buf(), config })
    }

    /// Replace the active configuration without reopening indexes.
    pub fn set_config(&mut self, config: Config) {
        self.config = config;
    }

    /// Execute a SPARQL query string and return the result set.
    pub fn query(&self, sparql: &str) -> Result<QueryResult, QueryError> {
        let t0 = Instant::now();
        let ast = parse_query(sparql).map_err(|e| QueryError::Parse(e.to_string()))?;

        let executor = Executor::with_config(&self.index, &self.dict, self.config.query.clone());

        let result = match ast.form {
            QueryForm::Select(ref sq) => {
                // Validate GROUP BY consistency before executing.
                // SPARQL 1.1 leaves the value of non-aggregate, non-GROUP-BY SELECT variables
                // undefined when GROUP BY is present.  Rather than silently returning NULL or
                // a random sample, we reject the query with a helpful suggestion.
                if !sq.group_by.is_empty() {
                    if let Some(err) = validate_group_by(sq) {
                        return Err(QueryError::Parse(err));
                    }
                }
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

/// Check that every plain variable in SELECT is either a GROUP BY key or an aggregate alias.
///
/// Returns `Some(error_message)` when ungrouped variables are found, `None` when the query is valid.
///
/// Example error:
///   "Variable(s) ?c, ?c_label appear in SELECT but not in GROUP BY or an aggregate.
///    Add them to GROUP BY, e.g.: GROUP BY ?p ?c ?c_label"
fn validate_group_by(sq: &crate::sparql::ast::SelectQuery) -> Option<String> {
    // Collect all GROUP BY variable names
    let group_by_vars: std::collections::HashSet<&str> = sq.group_by.iter()
        .filter_map(|gc| {
            if let Expression::Variable(v) = &gc.expr { Some(v.as_str()) } else { None }
        })
        .collect();

    // Find SELECT variables that are neither a GROUP BY key nor an aggregate alias
    let mut ungrouped: Vec<&str> = Vec::new();
    if let Projection::Variables(items) = &sq.projection {
        for item in items {
            if let SelectItem::Variable(v) = item {
                if !group_by_vars.contains(v.as_str()) {
                    ungrouped.push(v.as_str());
                }
            }
        }
    }

    if ungrouped.is_empty() {
        return None;
    }

    // Build a suggested GROUP BY that includes both current keys and the missing variables,
    // preserving the original order of GROUP BY keys first.
    let mut suggested: Vec<&str> = sq.group_by.iter()
        .filter_map(|gc| if let Expression::Variable(v) = &gc.expr { Some(v.as_str()) } else { None })
        .collect();
    for v in &ungrouped {
        if !suggested.contains(v) {
            suggested.push(v);
        }
    }

    let ungrouped_display: Vec<String> = ungrouped.iter().map(|v| format!("?{}", v)).collect();
    let suggested_display: Vec<String> = suggested.iter().map(|v| format!("?{}", v)).collect();

    Some(format!(
        "Variable(s) {} appear in SELECT but not in GROUP BY or an aggregate function. \
         Add them to GROUP BY, e.g.: GROUP BY {}",
        ungrouped_display.join(", "),
        suggested_display.join(" "),
    ))
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
