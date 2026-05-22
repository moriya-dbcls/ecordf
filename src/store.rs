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

use rayon;

use crate::config::Config;
use crate::dict::Dictionary;
use crate::dict_builder::{QueryDict, ReadonlyDict};
use crate::index::{AllBuilders, TripleIndex};
use crate::loader::{collect_strings_from_inputs, collect_strings_parallel,
                    load_files, load_files_with_graphs,
                    load_triples_with_readonly_dict, load_triples_parallel, InputSpec};
use crate::sparql::{parse_query, Executor, ResultSet};
use crate::sparql::ast::{Expression, Projection, QueryForm, SelectItem};
use crate::stats::StoreStatistics;

pub struct Store {
    pub dict: QueryDict,
    pub index: TripleIndex,
    pub dir: PathBuf,
    pub config: Config,
    /// Per-predicate statistics for join ordering.
    /// Loaded from `stats.bin` or built on first open.
    pub stats: StoreStatistics,
}

impl Store {
    /// Build a new store from RDF input files.
    ///
    /// Creates `dir` if it doesn't exist, writes dict.bin + spo.bin/pos.bin/osp.bin.
    ///
    /// When `config.build.chunk_size > 0` (the default) a **two-pass** strategy
    /// is used to keep peak RAM bounded regardless of dataset size:
    ///
    /// - **Phase 1** — stream every file once to collect unique strings into
    ///   an external-sort dictionary builder.  Peak RAM = `dict_chunk_mb` MB.
    /// - **Phase 2** — stream every file again, resolving string IDs via a
    ///   mmap-based binary search, and build the triple indexes with external sort.
    ///   Peak RAM = `dict_chunk_mb` + triple index buffers (`chunk_size × 12 B × 3`).
    ///
    /// Set `chunk_size = 0` in `ecordf.toml` to revert to the legacy one-pass
    /// in-memory approach (requires all unique strings in RAM simultaneously).
    pub fn load(dir: &Path, input_files: &[&Path]) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let config = Config::resolve(None, dir).map_err(io::Error::from)?;

        let inputs: Vec<InputSpec> = input_files.iter()
            .map(|p| InputSpec::plain(p.to_path_buf()))
            .collect();

        if config.build.chunk_size > 0 {
            Self::load_two_pass_internal(dir, &inputs, &config, false)
        } else {
            Self::load_one_pass_internal(dir, input_files, &config)
        }
    }

    // ── internal: one-pass (legacy) ───────────────────────────────────────────

    fn load_one_pass_internal(dir: &Path, input_files: &[&Path], config: &Config) -> io::Result<Self> {
        let t0 = Instant::now();
        eprintln!("=== EcoRDF: loading {} file(s) [one-pass / in-memory dict] ===", input_files.len());

        let raw_dict = Dictionary::new();
        // chunk_size == 0 → in-memory sort (AllBuilders::new)
        let mut builders = AllBuilders::new_streaming(dir, 0)?;
        let stats = load_files(input_files, &raw_dict, &mut builders)?;

        eprintln!("Parsed {} triples in {:.1}s. Sorting and writing indexes...",
            stats.triples_loaded, t0.elapsed().as_secs_f64());

        let t1 = Instant::now();
        let index = builders.build(dir)?;
        raw_dict.save(&dir.join("dict.bin"))?;

        eprintln!("Indexes written in {:.1}s. Total: {:.1}s",
            t1.elapsed().as_secs_f64(), t0.elapsed().as_secs_f64());
        eprintln!("Dictionary: {} terms | Triples: {}", raw_dict.len(), index.triple_count());

        let (id_to_str, str_to_id) = raw_dict.into_parts();
        let dict = QueryDict::from_legacy(id_to_str, str_to_id);
        let store_stats = StoreStatistics::load_or_build(&dir.join("stats.bin"), &index)?;
        Ok(Self { dict, index, dir: dir.to_path_buf(), config: config.clone(), stats: store_stats })
    }

    // ── internal: two-pass (external-sort dict) ───────────────────────────────

    /// Two-pass external-sort index build.
    ///
    /// When `resume_phase2 = true`, Phase 1 (string collection) is skipped and
    /// an existing `_ecordf_tmp/dict_sorted.bin` is used directly.  This lets
    /// you continue a build that was interrupted after Phase 1 completed.
    fn load_two_pass_internal(
        dir: &Path,
        inputs: &[InputSpec],
        config: &Config,
        resume_phase2: bool,
    ) -> io::Result<Self> {
        let tmp_dir = dir.join("_ecordf_tmp");
        let dict_sorted_path = tmp_dir.join("dict_sorted.bin");

        let dict_chunk_bytes = config.build.dict_chunk_mb * 1024 * 1024;
        let t0 = Instant::now();

        // Determine actual thread count.
        let num_threads = if config.build.parallel_threads > 0 {
            config.build.parallel_threads.min(inputs.len()).max(1)
        } else {
            rayon::current_num_threads().min(inputs.len()).max(1)
        };

        eprintln!(
            "=== EcoRDF: loading {} file(s) [two-pass / external-sort dict / {} thread(s)] ===",
            inputs.len(), num_threads
        );
        eprintln!(
            "  dict_chunk_mb={} MB  |  triple chunk_size={} (~{} MB/index)  |  threads={}",
            config.build.dict_chunk_mb,
            config.build.chunk_size,
            config.build.chunk_size * 12 / (1024 * 1024),
            num_threads,
        );

        // Build a local rayon thread pool so the thread count is honoured even
        // when the caller has already configured the global pool differently.
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(num_threads)
            .build()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

        fs::create_dir_all(&tmp_dir)?;

        // ── Phase 1: collect all unique strings (or skip if resuming) ─────────
        let term_count: u32;

        if resume_phase2 {
            if dict_sorted_path.exists() {
                // ── ケース A: dict_sorted.bin が存在 → Phase 1 を完全スキップ ──
                let rdict = ReadonlyDict::open(&dict_sorted_path)?;
                term_count = rdict.len() as u32;
                eprintln!(
                    "=== Phase 1 をスキップ (--resume-phase2): 既存の辞書を使用 ===\n\
                     Dictionary: {} terms  ({:?})",
                    term_count, dict_sorted_path
                );
                // 中断した Phase 1 が残した p1_ ディレクトリを掃除する
                for entry in fs::read_dir(&tmp_dir).into_iter().flatten().flatten() {
                    let name = entry.file_name();
                    if name.to_string_lossy().starts_with("p1_") {
                        let _ = fs::remove_dir_all(entry.path());
                    }
                }
            } else {
                // ── ケース B: dict_sorted.bin がない → 残存チャンクからマージ ──
                // Phase 1 の文字列収集は終わっているが merge_string_chunks が
                // EMFILE などで失敗した場合にここに来る。

                // 前回の失敗したマージが残した .__merge_*.tmp 中間ファイルを削除する。
                // これらは今回の新しいマージで別の名前（L0/L1… レベル付き）が使われるので
                // 衝突しないが、念のため清掃しておく。
                for entry in fs::read_dir(&tmp_dir).into_iter().flatten().flatten() {
                    let name = entry.file_name();
                    let name_str = name.to_string_lossy();
                    if name_str.contains(".__merge_") && name_str.ends_with(".tmp") {
                        let _ = fs::remove_file(entry.path());
                    }
                }

                let existing_chunks = collect_p1_chunks(&tmp_dir)?;
                if existing_chunks.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!(
                            "--resume-phase2: dict_sorted.bin も p1_* チャンクファイルも見つかりません。\n\
                             再開できる状態ではありません。--resume-phase2 を外して最初からやり直してください。\n\
                             (探した場所: {:?})",
                            tmp_dir
                        ),
                    ));
                }
                eprintln!(
                    "=== Phase 1 マージを再実行 (--resume-phase2): {} チャンクファイルを使用 ===",
                    existing_chunks.len()
                );
                let t_merge = Instant::now();
                term_count = crate::dict_builder::merge_string_chunks(&existing_chunks, &dict_sorted_path)?;
                // p1_ ディレクトリを掃除する
                for entry in fs::read_dir(&tmp_dir).into_iter().flatten().flatten() {
                    let name = entry.file_name();
                    if name.to_string_lossy().starts_with("p1_") {
                        let _ = fs::remove_dir_all(entry.path());
                    }
                }
                eprintln!(
                    "Dictionary: {} unique terms  (merge {:.1}s  |  total {:.1}s)",
                    term_count,
                    t_merge.elapsed().as_secs_f64(),
                    t0.elapsed().as_secs_f64()
                );
            }
        } else {
            eprintln!("=== Phase 1: collecting unique terms ({} threads) ===", num_threads);

            let (all_string_chunks, p1_stats) = pool.install(|| {
                collect_strings_parallel(inputs, &tmp_dir, dict_chunk_bytes, num_threads)
            })?;

            eprintln!(
                "Phase 1 done: {} lines processed, {} string chunks  ({:.1}s)",
                p1_stats.lines_processed,
                all_string_chunks.len(),
                t0.elapsed().as_secs_f64()
            );

            // k-way merge of all per-thread/per-file string chunks
            let t_merge = Instant::now();
            term_count = crate::dict_builder::merge_string_chunks(&all_string_chunks, &dict_sorted_path)?;
            // Clean up per-file Phase 1 subdirectories
            for i in 0..inputs.len() {
                let _ = fs::remove_dir_all(tmp_dir.join(format!("p1_{:06}", i)));
            }
            eprintln!(
                "Dictionary: {} unique terms  (merge {:.1}s  |  total {:.1}s)",
                term_count,
                t_merge.elapsed().as_secs_f64(),
                t0.elapsed().as_secs_f64()
            );
        }

        // ── Phase 2: load triples in parallel ─────────────────────────────────
        eprintln!("=== Phase 2: loading triples ({} threads) ===", num_threads);
        let t2 = Instant::now();

        let (all_index_chunks, p2_stats) = pool.install(|| {
            load_triples_parallel(
                inputs,
                &dict_sorted_path,
                &tmp_dir,
                config.build.chunk_size,
                num_threads,
            )
        })?;

        eprintln!(
            "Parsed {} triples in {:.1}s. Merging indexes...",
            p2_stats.triples_loaded,
            t2.elapsed().as_secs_f64()
        );

        let t3 = Instant::now();
        let index = AllBuilders::build_from_parallel_chunks(all_index_chunks, dir)?;

        eprintln!("Indexes written in {:.1}s.", t3.elapsed().as_secs_f64());

        // ── Persist dict_sorted.bin in the store root for query-time mmap ─────
        let store_dict_sorted = dir.join("dict_sorted.bin");
        fs::copy(&dict_sorted_path, &store_dict_sorted)?;

        // ── Write legacy dict.bin for backward compatibility ──────────────────
        let readonly_dict = ReadonlyDict::open(&dict_sorted_path)?;
        readonly_dict.write_legacy_dict(&dir.join("dict.bin"))?;

        eprintln!(
            "Total load time: {:.1}s  |  Dictionary: {} terms  |  Triples: {}",
            t0.elapsed().as_secs_f64(),
            term_count,
            index.triple_count()
        );

        // Cleanup entire tmp dir (dict_sorted.bin + any remaining chunk dirs)
        let _ = fs::remove_dir_all(&tmp_dir);

        // Open query-time dictionary from the persisted mmap file.
        let dict = QueryDict::from_mmap(ReadonlyDict::open(&store_dict_sorted)?);
        let store_stats = StoreStatistics::load_or_build(&dir.join("stats.bin"), &index)?;
        Ok(Self { dict, index, dir: dir.to_path_buf(), config: config.clone(), stats: store_stats })
    }

    /// Build a new store from RDF input files with optional per-file named graph assignment.
    ///
    /// Each [`InputSpec`] carries a file path and an optional graph IRI.
    /// N-Triples files with a graph IRI are loaded into both the union graph and
    /// the named graph (GSPO index).  N-Quads files ignore the graph field.
    ///
    /// Uses the same two-pass / one-pass logic as [`Store::load`].
    pub fn load_with_graphs(dir: &Path, inputs: &[InputSpec]) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let config = Config::resolve(None, dir).map_err(io::Error::from)?;

        if config.build.chunk_size > 0 {
            Self::load_two_pass_internal(dir, inputs, &config, false)
        } else {
            let t0 = Instant::now();
            eprintln!("=== EcoRDF: loading {} file(s) [one-pass] ===", inputs.len());
            let raw_dict = Dictionary::new();
            let mut builders = AllBuilders::new_streaming(dir, 0)?;
            let stats = load_files_with_graphs(inputs, &raw_dict, &mut builders)?;
            eprintln!("Parsed {} triples in {:.1}s. Writing indexes...",
                stats.triples_loaded, t0.elapsed().as_secs_f64());
            let index = builders.build(dir)?;
            raw_dict.save(&dir.join("dict.bin"))?;
            let (id_to_str, str_to_id) = raw_dict.into_parts();
            let dict = QueryDict::from_legacy(id_to_str, str_to_id);
            let store_stats = StoreStatistics::load_or_build(&dir.join("stats.bin"), &index)?;
            Ok(Self { dict, index, dir: dir.to_path_buf(), config, stats: store_stats })
        }
    }

    /// [`load_with_graphs`] と同じだが Phase 1 (文字列収集) をスキップする。
    ///
    /// `<dir>/_ecordf_tmp/dict_sorted.bin` が存在している必要があります。
    /// Phase 1 が完了した後に Phase 2 が失敗した場合の再実行用。
    ///
    /// `chunk_size == 0` の場合は通常の one-pass ビルドにフォールバックします。
    pub fn load_with_graphs_resume_phase2(dir: &Path, inputs: &[InputSpec]) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let config = Config::resolve(None, dir).map_err(io::Error::from)?;

        if config.build.chunk_size > 0 {
            Self::load_two_pass_internal(dir, inputs, &config, true)
        } else {
            // chunk_size == 0 はもともと one-pass なので resume の概念がない
            eprintln!("警告: chunk_size=0 のため --resume-phase2 は無視し通常ビルドを実行します");
            let t0 = Instant::now();
            let raw_dict = Dictionary::new();
            let mut builders = AllBuilders::new_streaming(dir, 0)?;
            let stats = load_files_with_graphs(inputs, &raw_dict, &mut builders)?;
            eprintln!("Parsed {} triples in {:.1}s. Writing indexes...",
                stats.triples_loaded, t0.elapsed().as_secs_f64());
            let index = builders.build(dir)?;
            raw_dict.save(&dir.join("dict.bin"))?;
            let (id_to_str, str_to_id) = raw_dict.into_parts();
            let dict = QueryDict::from_legacy(id_to_str, str_to_id);
            let store_stats = StoreStatistics::load_or_build(&dir.join("stats.bin"), &index)?;
            Ok(Self { dict, index, dir: dir.to_path_buf(), config, stats: store_stats })
        }
    }

    /// Reopen an existing store from its directory.
    pub fn open(dir: &Path) -> io::Result<Self> {
        let dict = open_query_dict(dir)?;
        let index = TripleIndex::open(dir)?;
        let config = Config::resolve(None, dir).map_err(io::Error::from)?;
        let stats = StoreStatistics::load_or_build(&dir.join("stats.bin"), &index)?;
        Ok(Self { dict, index, dir: dir.to_path_buf(), config, stats })
    }

    /// Reopen an existing store and apply an explicit config file (overrides
    /// the auto-detected `<store-dir>/ecordf.toml`).
    pub fn open_with_config(dir: &Path, config_path: Option<&std::path::Path>) -> io::Result<Self> {
        let dict = open_query_dict(dir)?;
        let index = TripleIndex::open(dir)?;
        let config = Config::resolve(config_path, dir).map_err(io::Error::from)?;
        let stats = StoreStatistics::load_or_build(&dir.join("stats.bin"), &index)?;
        Ok(Self { dict, index, dir: dir.to_path_buf(), config, stats })
    }

    /// Replace the active configuration without reopening indexes.
    pub fn set_config(&mut self, config: Config) {
        self.config = config;
    }

    /// Execute a SPARQL query string and return the result set.
    pub fn query(&self, sparql: &str) -> Result<QueryResult, QueryError> {
        let t0 = Instant::now();
        let ast = parse_query(sparql).map_err(|e| QueryError::Parse(e.to_string()))?;

        let executor = Executor::with_config_and_stats(
            &self.index,
            &self.dict,
            self.config.query.clone(),
            Some(&self.stats),
        );

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

/// Open the query-time dictionary for an existing store.
///
/// Prefers `dict_sorted.bin` (mmap-backed, low RAM) when present.
/// Falls back to the legacy `dict.bin` (full in-memory) for stores built
/// before this feature was added.
fn open_query_dict(dir: &Path) -> io::Result<QueryDict> {
    let mmap_path = dir.join("dict_sorted.bin");
    if mmap_path.exists() {
        let base = ReadonlyDict::open(&mmap_path)?;
        return Ok(QueryDict::from_mmap(base));
    }
    // Legacy fallback: load dict.bin into memory.
    let legacy = Dictionary::load(&dir.join("dict.bin"))?;
    let (id_to_str, str_to_id) = legacy.into_parts();
    Ok(QueryDict::from_legacy(id_to_str, str_to_id))
}

/// `_ecordf_tmp/p1_NNNNNN/` ディレクトリ内の文字列チャンクファイルを収集する。
///
/// Phase 1 の文字列収集が完了したが `merge_string_chunks` が失敗した場合（EMFILE など）に
/// 残存するチャンクファイルを再マージするために使用する。
fn collect_p1_chunks(tmp_dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut chunks: Vec<PathBuf> = Vec::new();
    if !tmp_dir.exists() {
        return Ok(chunks);
    }
    let mut entries: Vec<_> = fs::read_dir(tmp_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name().to_string_lossy().starts_with("p1_")
                && e.file_type().map(|t| t.is_dir()).unwrap_or(false)
        })
        .collect();
    // p1_ ディレクトリを名前順にソートして決定的な順序にする
    entries.sort_by_key(|e| e.file_name());

    for dir_entry in entries {
        let mut files: Vec<PathBuf> = fs::read_dir(dir_entry.path())?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map(|ext| ext == "bin").unwrap_or(false))
            .collect();
        files.sort(); // sdc_000000.bin, sdc_000001.bin … の順
        chunks.extend(files);
    }
    Ok(chunks)
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
