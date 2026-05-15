//! EcoRDF CLI — build, serve, query from command line.

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

use ecordf::{Store};

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
    /// Build a new store from RDF input files
    Build {
        /// Directory to store indexes (created if missing)
        #[arg(short, long, default_value = "./ecordf-data")]
        dir: PathBuf,
        /// Input RDF files (N-Triples .nt format)
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },

    /// Start the SPARQL 1.1 HTTP endpoint
    Serve {
        /// Store directory
        #[arg(short, long, default_value = "./ecordf-data")]
        dir: PathBuf,
        /// Bind host
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Bind port
        #[arg(short, long, default_value = "7878")]
        port: u16,
        /// Allow cross-origin requests (CORS).
        /// Use '*' to allow all origins, or a comma-separated list of specific origins.
        /// Examples:
        ///   --cors '*'
        ///   --cors 'https://app.example.com'
        ///   --cors 'https://a.example.com,https://b.example.com'
        #[arg(long, value_name = "ORIGINS")]
        cors: Option<String>,
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
    },

    /// Show store statistics
    Stats {
        #[arg(short, long, default_value = "./ecordf-data")]
        dir: PathBuf,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize logging (RUST_LOG=ecordf=debug for verbose output)
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env()
            .add_directive("ecordf=info".parse()?))
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Build { dir, files } => {
            let file_refs: Vec<&std::path::Path> = files.iter().map(|p| p.as_path()).collect();
            let store = Store::load(&dir, &file_refs)?;
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

        Command::Serve { dir, host, port, cors } => {
            let store = Store::open(&dir)?;
            let stats = store.stats();
            eprintln!(
                "Opened store: {} triples, {} terms",
                stats.triple_count, stats.term_count
            );
            ecordf::server::serve(store, &host, port, cors.as_deref()).await?;
        }

        Command::Query { dir, query, format } => {
            let store = Store::open(&dir)?;
            let sparql = if query == "-" {
                use std::io::Read;
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
