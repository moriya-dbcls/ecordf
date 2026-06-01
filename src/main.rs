//! EcoRDF CLI — build, serve, query from command line.

use clap::{Parser, Subcommand};
use std::io::Read;
use std::path::{Path, PathBuf};
use tracing_subscriber::EnvFilter;

use ecordf::{Config, InputSpec, Store};

#[derive(Parser)]
#[command(
    name = "ecordf",
    version,
    about = "EcoRDF: Cost-efficient RDF triple store\n\
             Low memory (vs Qlever) + fast queries (vs Virtuoso)\n\
             via memmap2 indexes + Leapfrog Triejoin"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Build a new store from RDF input files.
    ///
    /// Accepts N-Triples (.nt), N-Quads (.nq), and gzip-compressed
    /// variants (.nt.gz, .nq.gz) when built with --features gzip.
    ///
    /// Examples:
    ///
    ///   # Direct file list
    ///   ecordf build --dir ./store a.nt b.nq c.nt.gz
    ///
    ///   # Read paths from a file (one path per line, # = comment)
    ///   ecordf build --dir ./store --from-file inputs.txt
    ///
    ///   # Pipe a path list from find
    ///   find /data -name '*.nt.gz' | ecordf build --dir ./store --from-file -
    ///
    ///   # Mix: explicit files + a list file
    ///   ecordf build --dir ./store --from-file batch.txt extra.nt
    Build {
        /// Directory to store indexes (created if missing)
        #[arg(short, long, default_value = "./ecordf-data")]
        dir: PathBuf,

            /// Input RDF files (.nt, .nq, .nt.gz, .nq.gz).
        /// May be omitted when --from-file is used.
        /// These files are loaded into the union graph (no named graph).
        #[arg(value_name = "FILE")]
        files: Vec<PathBuf>,

        /// Read file paths (and optional graph IRIs) from a text file.
        ///
        /// Format — one entry per line:
        ///
        ///   # comment (ignored)
        ///   /path/to/file.nt
        ///   /path/to/file.nt.gz  <http://graph.example.com/uniprot>
        ///   /path/to/file.nt.gz  http://graph.example.com/pdb
        ///
        /// Lines starting with '#' and blank lines are ignored.
        /// The graph IRI (second token) may be given with or without <…>.
        /// When omitted, the file is loaded into the union graph only.
        ///
        /// Use '-' to read from stdin. May be specified multiple times.
        #[arg(long, value_name = "LIST_FILE", action = clap::ArgAction::Append)]
        from_file: Vec<PathBuf>,

        /// Phase 1 (文字列収集) をスキップして中断した処理を再開する。
        ///
        /// 以下の2つの状況に自動対応します:
        ///
        ///   A) `<--dir>/_ecordf_tmp/dict_sorted.bin` が存在する場合:
        ///      Phase 1 を完全スキップして Phase 2 へ
        ///
        ///   B) dict_sorted.bin はないが `_ecordf_tmp/p1_*/` チャンクが残っている場合:
        ///      マージだけやり直してから Phase 2 へ (EMFILE で止まった場合など)
        ///
        /// 例:
        ///
        ///   ecordf build --dir ./store --resume-phase2 --from-file inputs.txt
        #[arg(long, default_value_t = false)]
        resume_phase2: bool,
    },

    /// Start the SPARQL 1.1 HTTP endpoint
    Serve {
        /// Store directory
        #[arg(short, long, default_value = "./ecordf-data")]
        dir: PathBuf,
        /// Bind host (overrides server.host in config file)
        #[arg(long)]
        host: Option<String>,
        /// Bind port (overrides server.port in config file)
        #[arg(short, long)]
        port: Option<u16>,
        /// Allow cross-origin requests (overrides server.cors_origins in config file).
        /// Use '*' to allow all origins, or a comma-separated list of specific origins.
        /// Examples:
        ///   --cors '*'
        ///   --cors 'https://app.example.com'
        ///   --cors 'https://a.example.com,https://b.example.com'
        #[arg(long, value_name = "ORIGINS")]
        cors: Option<String>,
        /// Path to the config file (default: <store-dir>/ecordf.toml)
        #[arg(long, value_name = "PATH")]
        config: Option<PathBuf>,
        /// Pre-populate the OS page cache by reading this many MB of index data
        /// in the background after startup.  The budget is spread across SPO/POS/OSP
        /// and the dictionary (POS gets a 2× share as the most-used index).
        /// When 0 (default), falls back to [server] warmup_mb in ecordf.toml.
        /// For UniProt-scale stores, 4096–16384 is useful.
        #[arg(long, default_value_t = 0, value_name = "MB")]
        warmup_mb: u64,

        /// RAM budget (MiB) for the in-RAM predicate cache.
        /// Predicates are loaded largest-first so expensive predicates like
        /// faldo:position (11.8 M entries = 188 MB) are cached before small ones.
        /// When 0 (default), falls back to [server] pred_cache_mb in ecordf.toml.
        /// Per-predicate cap = pred_cache_mb / 2.  1024 covers faldo on JPostDB.
        #[arg(long, default_value_t = 0, value_name = "MB")]
        pred_cache_mb: u64,

        /// Per-predicate size cap (MiB) for the predicate cache.
        /// When 0 (default), falls back to [server] pred_cache_per_pred_cap_mb in ecordf.toml,
        /// which itself defaults to 0 (= pred_cache_mb / 2).
        /// Use this when two large predicates dominate the budget and crowd out smaller ones.
        /// Example: --pred-cache-per-pred-cap-mb 200  skips predicates > 200 MB,
        /// freeing space for faldo:begin/position (178 MB each).
        #[arg(long, default_value_t = 0, value_name = "MB")]
        pred_cache_per_pred_cap_mb: u64,

        /// rdf-config directories to load for path-cache materialisation.
        /// Each entry is either a local directory path containing prefix.yaml + model.yaml,
        /// or a GitHub tree URL such as:
        ///   https://github.com/dbcls/rdf-config/tree/master/config/uniprot
        ///
        /// Compound property paths (multi-hop predicate chains through blank nodes)
        /// found in the model.yaml files are pre-materialised in RAM so that SPARQL
        /// property-path queries avoid repeated HDD scans.
        ///
        /// May be specified multiple times:
        ///   --rdf-config https://github.com/dbcls/rdf-config/tree/master/config/uniprot
        ///   --rdf-config https://github.com/dbcls/rdf-config/tree/master/config/jpostdb
        ///
        /// When omitted, falls back to [model] rdf_configs in ecordf.toml.
        #[arg(long, value_name = "URL_OR_DIR", action = clap::ArgAction::Append)]
        rdf_config: Vec<String>,

        /// RAM budget (MiB) for the path cache built from rdf-config compound paths.
        /// When 0 (default), falls back to [model] path_cache_mb in ecordf.toml.
        #[arg(long, default_value_t = 0, value_name = "MB")]
        path_cache_mb: u64,

        /// RAM budget (MiB) for the TypeCache (`?x a SomeClass` membership lookups).
        /// Builds a per-class sorted subject list from rdf:type at startup.
        /// When 0, falls back to [server] type_cache_mb in ecordf.toml.
        #[arg(long, default_value_t = 0, value_name = "MB")]
        type_cache_mb: u64,

        /// Per-query wall-clock timeout in seconds.  0 = no timeout (default).
        /// When exceeded, the executor is cancelled and the client receives 408.
        /// When 0, falls back to [server] query_timeout_secs in ecordf.toml.
        #[arg(long, default_value_t = 0, value_name = "SECS")]
        query_timeout_secs: u64,

        /// Release index pages from page cache after sequential scans larger
        /// than this threshold (MiB).  0 = disabled (default).
        /// Reduces impact on co-located services.
        /// When 0, falls back to [server] scan_dontneed_mb in ecordf.toml.
        #[arg(long, default_value_t = 0, value_name = "MB")]
        scan_dontneed_mb: u64,
    },

    /// Build per-predicate (S,O) partition files for fast predicate access.
    ///
    /// Scans the POS index and writes one `pred_parts/pp_<id>.bin` file per
    /// predicate.  These files are automatically loaded at `ecordf serve` time
    /// and supplement (then replace) the pred_cache for uncached predicates.
    ///
    /// Run once after `ecordf build`:
    ///
    ///   ecordf build-pred-parts --dir ./store
    ///
    /// Re-run with --force to overwrite existing partition files.
    BuildPredParts {
        #[arg(short, long, default_value = "./ecordf-data")]
        dir: PathBuf,
        /// Overwrite existing partition files.
        #[arg(long, default_value_t = false)]
        force: bool,
    },

    /// Compress column files with delta encoding (ECOCOL02).
    ///
    /// For each `*.c0`, `*.c1`, `*.c2` column file under the store directory,
    /// writes a compressed `*.c0.dz`, `*.c1.dz`, `*.c2.dz` file.
    ///
    /// The server automatically detects and uses `.dz` files when present.
    ///
    ///   ecordf compress-cols --dir ./store
    ///
    /// Expected compression ratios (JPostDB):
    ///   pos.c0 (6 GB) → ~30 MB (200×)  — predicate IDs repeat extensively
    ///   spo.c0 (6 GB) → ~750 MB (8×)   — ascending subject IDs, small deltas
    ///   pos.c1 (6 GB) → ~1.5 GB (4×)   — object IDs sorted within predicates
    CompressCols {
        #[arg(short, long, default_value = "./ecordf-data")]
        dir: PathBuf,
        /// Overwrite existing .dz files.
        #[arg(long, default_value_t = false)]
        force: bool,
    },

    /// Execute a SPARQL query from command line
    Query {
        /// Store directory
        #[arg(short, long, default_value = "./ecordf-data")]
        dir: PathBuf,
        /// SPARQL query string (or - to read from stdin)
        query: String,
        /// Output format: json (default), tsv, csv, table
        #[arg(short, long, default_value = "table")]
        format: String,
        /// Path to the config file (default: <store-dir>/ecordf.toml)
        #[arg(long, value_name = "PATH")]
        config: Option<PathBuf>,
    },

    /// Show store statistics
    Stats {
        #[arg(short, long, default_value = "./ecordf-data")]
        dir: PathBuf,

        /// Show the top N predicates by triple count, with estimated pred-cache size in MB.
        ///
        /// Use this to choose the right pred_cache_per_pred_cap_mb value:
        ///
        ///   ecordf stats --top-predicates 30
        ///
        /// Then set pred_cache_per_pred_cap_mb to just above the size of the
        /// largest predicate you actually want cached, so that larger predicates
        /// that are never queried by property paths do not consume the budget.
        ///
        /// Example (JPostDB): if faldo:location is 223 MB and a large unused
        /// predicate is 478 MB, set pred_cache_per_pred_cap_mb = 250 to skip
        /// the 478 MB predicate and guarantee faldo:location is cached.
        #[arg(long, default_value_t = 0, value_name = "N")]
        top_predicates: usize,
    },
}

// ── Input file resolution ─────────────────────────────────────────────────────

/// Combine direct file arguments with entries read from `--from-file` list files.
///
/// Returns a `Vec<InputSpec>` where each spec carries a file path and an
/// optional named graph IRI.
///
/// **List file format** (one entry per line):
/// ```text
/// # comment — ignored
/// /path/to/file.nt
/// /path/to/file.nt.gz  <http://graph.example.com/uniprot>
/// /path/to/file.nt.gz  http://graph.example.com/pdb
/// ```
/// The graph IRI (second whitespace-separated token) is optional; files
/// without one are loaded into the union graph only.
/// Angle brackets around the IRI are accepted but not required.
///
/// The special list path `-` reads from stdin.
fn resolve_input_files(
    direct: Vec<PathBuf>,
    list_files: Vec<PathBuf>,
) -> anyhow::Result<Vec<InputSpec>> {
    // Direct positional arguments → union graph (no named graph)
    let mut all: Vec<InputSpec> = direct.into_iter()
        .map(InputSpec::plain)
        .collect();

    for list_path in list_files {
        let content = if list_path == Path::new("-") {
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s)?;
            s
        } else {
            std::fs::read_to_string(&list_path)
                .map_err(|e| anyhow::anyhow!("cannot read list file {:?}: {}", list_path, e))?
        };

        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // Split into at most two tokens: path and optional graph IRI
            let mut tokens = line.splitn(2, |c: char| c.is_ascii_whitespace());
            let path = PathBuf::from(tokens.next().unwrap());
            let graph = tokens.next().map(|g| g.trim().to_string());

            all.push(match graph {
                Some(g) if !g.is_empty() => InputSpec::with_graph(path, g),
                _                        => InputSpec::plain(path),
            });
        }
    }

    Ok(all)
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize logging.
    // Priority: RUST_LOG env var → fallback to "ecordf=info".
    // Examples:
    //   RUST_LOG=ecordf=debug   → DEBUG + INFO for all ecordf modules
    //   RUST_LOG=ecordf=trace   → TRACE + DEBUG + INFO
    //   RUST_LOG=info           → INFO for everything
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("ecordf=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Build { dir, files, from_file, resume_phase2 } => {
            let inputs = resolve_input_files(files, from_file)?;
            if inputs.is_empty() {
                anyhow::bail!(
                    "no input files specified.\n\
                     Pass files directly, or use --from-file <list> (or --from-file - to read from stdin)."
                );
            }
            let store = if resume_phase2 {
                Store::load_with_graphs_resume_phase2(&dir, &inputs)?
            } else {
                Store::load_with_graphs(&dir, &inputs)?
            };
            let stats = store.stats();
            if stats.graph_count > 0 {
                println!(
                    "Built store: {} triples, {} terms, {} named graphs → {:?}",
                    stats.triple_count, stats.term_count, stats.graph_count, stats.dir
                );
            } else {
                println!(
                    "Built store: {} triples, {} terms → {:?}",
                    stats.triple_count, stats.term_count, stats.dir
                );
            }
        }

        Command::Serve { dir, host, port, cors, config, warmup_mb, pred_cache_mb, pred_cache_per_pred_cap_mb, rdf_config, path_cache_mb, type_cache_mb, query_timeout_secs, scan_dontneed_mb } => {
            let mut store = Store::open_with_config(&dir, config.as_deref())?;
            let stats = store.stats();
            eprintln!(
                "Opened store: {} triples, {} terms",
                stats.triple_count, stats.term_count
            );
            // CLI flags override config file values
            let effective_host = host.unwrap_or_else(|| store.config.server.host.clone());
            let effective_port = port.unwrap_or(store.config.server.port);
            let effective_cors = cors.or_else(|| {
                let s = &store.config.server.cors_origins;
                if s.is_empty() { None } else { Some(s.clone()) }
            });
            eprintln!(
                "Config: max_intermediate_rows={}, bind_join_threshold={}",
                store.config.query.max_intermediate_rows,
                store.config.query.bind_join_threshold,
            );
            // CLI --warmup-mb takes precedence; fall back to config file value.
            let effective_warmup_mb = if warmup_mb > 0 { warmup_mb } else { store.config.server.warmup_mb };
            if effective_warmup_mb > 0 {
                eprintln!("Warming up {} MB of indexes in background...", effective_warmup_mb);
                store.warmup_background(effective_warmup_mb);
            }
            // Resolve rdf-config specs first — needed both for the pred_cache
            // priority pass and for the path_cache build later.
            // CLI --rdf-config flags override [model] rdf_configs in ecordf.toml.
            let effective_rdf_configs: Vec<String> = if !rdf_config.is_empty() {
                rdf_config
            } else {
                store.config.model.rdf_configs.clone()
            };

            // Load compound paths once — used for both pred_cache priority and
            // path_cache materialisation below.  Network I/O (GitHub fetches)
            // happens here; skipped if no rdf_configs are configured.
            let compound_paths: Vec<Vec<String>> = if !effective_rdf_configs.is_empty() {
                eprintln!("Loading rdf-config from {} spec(s)...", effective_rdf_configs.len());
                let paths = ecordf::rdf_config::load_compound_paths(&effective_rdf_configs);
                eprintln!("  {} compound path(s) found.", paths.len());
                paths
            } else {
                Vec::new()
            };

            // Collect all unique predicate IRIs appearing in any compound path.
            // These are the predicates that drive property-path SPARQL queries, so
            // caching them first avoids "budget exhausted by non-query predicates"
            // without requiring any explicit user configuration.
            let priority_iris: Vec<String> = {
                let mut seen = std::collections::HashSet::new();
                let mut iris = Vec::new();
                for path in &compound_paths {
                    for iri in path {
                        if seen.insert(iri.as_str()) {
                            iris.push(iri.clone());
                        }
                    }
                }
                iris
            };

            // Build predicate cache synchronously before serving queries.
            // CLI --pred-cache-mb takes precedence; fall back to config file value.
            let effective_pred_cache_mb = if pred_cache_mb > 0 { pred_cache_mb } else { store.config.server.pred_cache_mb };
            let effective_per_pred_cap_mb = if pred_cache_per_pred_cap_mb > 0 {
                pred_cache_per_pred_cap_mb
            } else {
                store.config.server.pred_cache_per_pred_cap_mb
            };
            if effective_pred_cache_mb > 0 {
                let cap_desc = if effective_per_pred_cap_mb > 0 {
                    format!("{} MB", effective_per_pred_cap_mb)
                } else {
                    format!("{} MB (default 50%)", effective_pred_cache_mb / 2)
                };
                if priority_iris.is_empty() {
                    eprintln!(
                        "Building predicate cache ({} MB, per-predicate cap = {})...",
                        effective_pred_cache_mb, cap_desc,
                    );
                    if effective_rdf_configs.is_empty() {
                        eprintln!(
                            "  Note: no rdf-config specs configured — priority-predicate pass disabled.\n  \
                             To enable, set [model] rdf_configs in ecordf.toml or use --rdf-config.\n  \
                             Predicates will be cached in size-descending order (largest first).\n  \
                             Tip: run `ecordf stats --top-predicates 30` to see predicate sizes and\n  \
                             choose an appropriate pred_cache_per_pred_cap_mb."
                        );
                    }
                } else {
                    eprintln!(
                        "Building predicate cache ({} MB, per-predicate cap = {}, {} rdf-config predicate(s) prioritised)...",
                        effective_pred_cache_mb, cap_desc, priority_iris.len(),
                    );
                }
                store.build_pred_cache_sync(effective_pred_cache_mb, effective_per_pred_cap_mb, &priority_iris);
                eprintln!("Predicate cache ready ({} MB used).",
                    store.pred_cache.bytes_used() / (1024 * 1024));
            }

            // Build path cache from the already-loaded compound paths (no second YAML fetch).
            // If --rdf-config is given but --path-cache-mb is omitted, default to 512 MB.
            const DEFAULT_PATH_CACHE_MB: u64 = 512;
            let effective_path_cache_mb = if path_cache_mb > 0 {
                path_cache_mb
            } else if store.config.model.path_cache_mb > 0 {
                store.config.model.path_cache_mb
            } else if !effective_rdf_configs.is_empty() {
                DEFAULT_PATH_CACHE_MB // implicit default when --rdf-config is used
            } else {
                0
            };
            if !compound_paths.is_empty() && effective_path_cache_mb > 0 {
                eprintln!(
                    "Building path cache ({} MB) from {} compound path(s)...",
                    effective_path_cache_mb, compound_paths.len()
                );
                store.build_path_cache_from_compounds(&compound_paths, effective_path_cache_mb);
                eprintln!(
                    "Path cache ready: {} path(s), {} MB used.",
                    store.path_cache.len(),
                    store.path_cache.bytes_used() / (1024 * 1024)
                );
            }
            // Apply CLI overrides for timeout and MADV_DONTNEED threshold.
            if query_timeout_secs > 0 {
                store.config.server.query_timeout_secs = query_timeout_secs;
            }
            if scan_dontneed_mb > 0 {
                store.config.server.scan_dontneed_mb = scan_dontneed_mb;
            }
            if store.config.server.query_timeout_secs > 0 {
                eprintln!("Query timeout: {}s", store.config.server.query_timeout_secs);
            }
            if store.config.server.scan_dontneed_mb > 0 {
                eprintln!("Scan MADV_DONTNEED threshold: {} MB", store.config.server.scan_dontneed_mb);
            }

            // Build TypeCache (rdf:type membership lookups).
            let effective_type_cache_mb = if type_cache_mb > 0 { type_cache_mb } else { store.config.server.type_cache_mb };
            if effective_type_cache_mb > 0 {
                eprintln!("Building type cache ({} MB)...", effective_type_cache_mb);
                store.build_type_cache(effective_type_cache_mb);
            }

            tracing::info!(
                host = %effective_host,
                port = effective_port,
                triples = stats.triple_count,
                terms   = stats.term_count,
                pred_cache_mb = store.pred_cache.bytes_used() / (1024 * 1024),
                path_cache_paths = store.path_cache.len(),
                type_cache_classes = store.type_cache.len(),
                pred_partitions = store.pred_partitions.len(),
                "Server ready"
            );
            ecordf::server::serve(store, &effective_host, effective_port, effective_cors.as_deref()).await?;
        }

        Command::Query { dir, query, format, config } => {
            let store = Store::open_with_config(&dir, config.as_deref())?;
            let sparql = if query == "-" {
                let mut s = String::new();
                std::io::stdin().read_to_string(&mut s)?;
                s
            } else {
                query
            };

            match store.query_to_table(&sparql) {
                Ok(rows) => {
                    match format.as_str() {
                        "tsv" => {
                            for row in &rows {
                                println!("{}", row.join("\t"));
                            }
                        }
                        "csv" => {
                            for row in &rows {
                                println!("{}", row.iter().map(|c| {
                                    if c.contains(',') || c.contains('"') {
                                        format!("\"{}\"", c.replace('"', "\"\""))
                                    } else { c.clone() }
                                }).collect::<Vec<_>>().join(","));
                            }
                        }
                        _ => {
                            // Pretty table
                            if rows.is_empty() {
                                println!("(no results)");
                                return Ok(());
                            }
                            let headers = &rows[0];
                            let data = &rows[1..];

                            // Calculate column widths
                            let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
                            for row in data {
                                for (i, cell) in row.iter().enumerate() {
                                    if i < widths.len() {
                                        widths[i] = widths[i].max(cell.len().min(80));
                                    }
                                }
                            }

                            let separator: String = widths.iter()
                                .map(|w| "-".repeat(w + 2))
                                .collect::<Vec<_>>()
                                .join("+");
                            println!("+{}+", separator);

                            // Header
                            let header_row: String = headers.iter().enumerate()
                                .map(|(i, h)| format!(" {:width$} ", h, width = widths.get(i).copied().unwrap_or(10)))
                                .collect::<Vec<_>>()
                                .join("|");
                            println!("|{}|", header_row);
                            println!("+{}+", separator);

                            // Data
                            for row in data {
                                let data_row: String = (0..widths.len())
                                    .map(|i| {
                                        let cell = row.get(i).map(|s| s.as_str()).unwrap_or("");
                                        let truncated = if cell.len() > 80 {
                                            format!("{}...", &cell[..77])
                                        } else {
                                            cell.to_string()
                                        };
                                        format!(" {:width$} ", truncated, width = widths[i])
                                    })
                                    .collect::<Vec<_>>()
                                    .join("|");
                                println!("|{}|", data_row);
                            }
                            println!("+{}+", separator);
                            println!("{} row(s)", data.len());
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            }
        }

        Command::Stats { dir, top_predicates } => {
            let store = Store::open(&dir)?;
            let stats = store.stats();
            println!("Directory:     {:?}", stats.dir);
            println!("Triples:       {}", stats.triple_count);
            println!("Terms:         {}", stats.term_count);
            let six_index = store.index.sop.is_some();
            let index_label = if six_index { "6-index (SPO+POS+OSP+PSO+SOP+OPS)" } else { "3-index (SPO+POS+OSP)" };
            let index_mb = if six_index {
                (6 * stats.triple_count * 12) / (1024 * 1024)
            } else {
                (3 * stats.triple_count * 12) / (1024 * 1024)
            };
            if stats.graph_count > 0 {
                println!("Named graphs:  {}", stats.graph_count);
                println!("Index layout:  {} + GSPO (~{} MB)", index_label,
                    index_mb + (stats.triple_count * 16) / (1024 * 1024));
            } else {
                println!("Index layout:  {} (~{} MB)", index_label, index_mb);
            }

            if top_predicates > 0 {
                let mut sizes = store.index.predicate_sizes();
                sizes.sort_unstable_by(|a, b| b.1.cmp(&a.1));
                let total_pred_cache_mb: usize =
                    sizes.iter().map(|(_, c)| (c * 16 + (1024 * 1024 - 1)) / (1024 * 1024)).sum();
                println!();
                println!("Top {} predicates by triple count:", top_predicates.min(sizes.len()));
                println!(
                    "  {:>6}  {:>12}  {:>8}  {}",
                    "Rank", "Triples", "~MB", "Predicate IRI"
                );
                println!("  {}+{}+{}+{}", "-".repeat(6), "-".repeat(14), "-".repeat(10), "-".repeat(60));
                for (rank, (pred_id, count)) in sizes.iter().take(top_predicates).enumerate() {
                    let iri = store.dict.decode(*pred_id);
                    let mb = (count * 16) / (1024 * 1024);
                    let display = if iri.len() > 90 {
                        format!("{}…", &iri[..89])
                    } else {
                        iri
                    };
                    println!("  {:>6}  {:>12}  {:>8}  {}", rank + 1, count, mb, display);
                }
                println!();
                println!(
                    "  Total pred-cache requirement for all {} predicates: ~{} MB",
                    sizes.len(), total_pred_cache_mb
                );
                println!();
                println!("  Tuning tips:");
                println!("    pred_cache_mb              — total RAM budget for the cache");
                println!("    pred_cache_per_pred_cap_mb — skip predicates larger than this");
                println!();
                println!("  Set pred_cache_per_pred_cap_mb just above the size of the largest");
                println!("  predicate you actually query, so that huge low-query predicates");
                println!("  don't crowd out the ones you care about.");
                if let Some((_, largest_count)) = sizes.first() {
                    let largest_mb = (largest_count * 16) / (1024 * 1024);
                    println!("  Largest predicate: ~{} MB — this is the minimum cap to cache it.", largest_mb);
                }
            }
        }

        Command::BuildPredParts { dir, force } => {
            let store = Store::open(&dir)?;
            eprintln!("Building predicate partition files for {:?}...", dir);
            let n = ecordf::pred_partition::build_pred_partitions(&dir, &*store.index, force)?;
            eprintln!("Done: {} partition file(s) written.", n);
        }

        Command::CompressCols { dir, force } => {
            eprintln!("Compressing column files in {:?}...", dir);
            let n = ecordf::index::TripleIndex::compress_columns(&dir, force)?;
            eprintln!("Done: {} column file(s) compressed.", n);
        }
    }

    Ok(())
}
