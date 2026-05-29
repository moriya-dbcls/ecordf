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

        Command::Serve { dir, host, port, cors, config, warmup_mb } => {
            let store = Store::open_with_config(&dir, config.as_deref())?;
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

        Command::Stats { dir } => {
            let store = Store::open(&dir)?;
            let stats = store.stats();
            println!("Directory:     {:?}", stats.dir);
            println!("Triples:       {}", stats.triple_count);
            println!("Terms:         {}", stats.term_count);
            if stats.graph_count > 0 {
                println!("Named graphs:  {}", stats.graph_count);
                println!(
                    "Index size: ~{} MB (SPO/POS/OSP×12 + GSPO×16 bytes)",
                    (3 * stats.triple_count * 12 + stats.triple_count * 16) / (1024 * 1024),
                );
            } else {
                println!(
                    "Index size: ~{} MB (3 × {} triples × 12 bytes)",
                    (3 * stats.triple_count * 12) / (1024 * 1024),
                    stats.triple_count
                );
            }
        }
    }

    Ok(())
}
