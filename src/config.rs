//! # EcoRDF Configuration
//!
//! Runtime tunables loaded from an optional `ecordf.toml` file.
//!
//! ## File search order
//!
//! 1. Path given by `--config <path>` on the command line (highest priority).
//! 2. `<store-dir>/ecordf.toml` — config co-located with the data directory.
//! 3. Built-in defaults — used when no config file is found.
//!
//! ## Example
//!
//! ```toml
//! [build]
//! # Flush triples to disk in sorted chunks of this size during index build.
//! # Keeps peak memory to roughly: chunk_size × 40 bytes (3 indexes × 12 B + dict overhead).
//! # Default: 5_000_000 (≈ 180 MB peak across SPO/POS/OSP builders).
//! # Set to 0 to disable external sort (old in-memory behaviour; requires all triples in RAM).
//! chunk_size = 5_000_000
//!
//! [query]
//! max_intermediate_rows = 5_000_000
//! bind_join_threshold   = 10_000
//!
//! [server]
//! host        = "127.0.0.1"
//! port        = 7878
//! cors_origins = ""
//! # Pre-populate the OS page cache on startup (0 = disabled).
//! # For UniProt-scale stores, 4096–16384 MB is useful.
//! warmup_mb   = 0
//! # In-RAM predicate cache — loads small-to-medium predicates into a sorted Vec
//! # at startup so the first query is as fast as subsequent page-cached ones.
//! # Per-predicate cap = pred_cache_mb / 2.  Default 1024 covers faldo:position
//! # (11.8 M entries = 188 MB) on JPostDB.  Set 0 to disable.
//! pred_cache_mb = 1024
//! # Per-predicate cap (MiB).  0 = use pred_cache_mb / 2 (default behaviour).
//! # Set to e.g. 200 to prevent two 479 MB predicates from consuming 957 MB of
//! # a 1024 MB budget, crowding out faldo:begin/position (178 MB each).
//! pred_cache_per_pred_cap_mb = 0
//!
//! [model]
//! # rdf-config directories (local paths or GitHub tree URLs) to load at startup.
//! # Compound property paths found in model.yaml files are pre-materialised in RAM.
//! # rdf_configs = [
//! #   "https://github.com/dbcls/rdf-config/tree/master/config/uniprot",
//! #   "https://github.com/dbcls/rdf-config/tree/master/config/jpostdb",
//! # ]
//! path_cache_mb = 0   # disabled by default; set to e.g. 512 to enable
//! ```

use serde::{Deserialize, Serialize};
use std::path::Path;

// ── Top-level config ──────────────────────────────────────────────────────────

/// Top-level EcoRDF configuration.
///
/// Loaded from `ecordf.toml`; every field has a built-in default so the file
/// is fully optional — missing keys fall back to their defaults.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    /// Index build tunables (chunk size for external sort, etc.).
    pub build: BuildConfig,
    /// Query execution tunables.
    pub query: QueryConfig,
    /// HTTP server defaults (overridable via CLI flags).
    pub server: ServerConfig,
    /// RDF model / schema hints for query optimisation.
    pub model: ModelConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            build: BuildConfig::default(),
            query: QueryConfig::default(),
            server: ServerConfig::default(),
            model: ModelConfig::default(),
        }
    }
}

impl Config {
    /// Load from a TOML file.
    ///
    /// Returns `Err` if the file exists but cannot be read or parsed.
    /// Returns `Ok(Config::default())` if the file does not exist.
    pub fn load_or_default(path: &Path) -> Result<Self, ConfigError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        Self::load(path)
    }

    /// Load from an explicit TOML file path.
    ///
    /// Returns `Err` if the file cannot be read or contains invalid TOML.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| ConfigError(format!("cannot read {:?}: {}", path, e)))?;
        toml::from_str(&text)
            .map_err(|e| ConfigError(format!("cannot parse {:?}: {}", path, e)))
    }

    /// Resolve the config file to use, given an optional explicit path and the
    /// store directory.
    ///
    /// Resolution order:
    /// 1. `explicit` — a path supplied by `--config`
    /// 2. `<store_dir>/ecordf.toml`
    /// 3. Built-in defaults
    pub fn resolve(explicit: Option<&Path>, store_dir: &Path) -> Result<Self, ConfigError> {
        if let Some(p) = explicit {
            return Self::load(p);
        }
        let auto = store_dir.join("ecordf.toml");
        Self::load_or_default(&auto)
    }
}

// ── Build config ──────────────────────────────────────────────────────────────

/// Tunables for the index build phase.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct BuildConfig {
    /// Number of triples per sorted chunk during external-sort index build.
    ///
    /// During `ecordf build`, incoming triples are accumulated in memory in
    /// sorted chunks of this size.  Each full chunk is flushed to a temporary
    /// file, sorted, and later k-way merged into the final index.  This bounds
    /// peak memory to approximately `chunk_size × 40 bytes` (the three
    /// SPO/POS/OSP index buffers × 12 bytes each, plus dictionary overhead).
    ///
    /// | chunk_size | peak index-buffer RAM |
    /// |------------|----------------------|
    /// | 1_000_000  |  ≈ 36 MB             |
    /// | 5_000_000  |  ≈ 180 MB  (default) |
    /// | 20_000_000 |  ≈ 720 MB            |
    ///
    /// Set to `0` to disable external sort entirely and load all triples into
    /// RAM before sorting (the old behaviour).  Only do this on machines with
    /// many tens of GB of free memory and datasets that fit comfortably in RAM.
    pub chunk_size: usize,

    /// RAM budget (in MiB) for the string-collection buffer in the two-pass
    /// dictionary builder (used when `chunk_size > 0`).
    ///
    /// During Phase 1 of the two-pass load, unique RDF terms are accumulated
    /// in memory up to this size, then sorted, deduped, and flushed to a
    /// temporary chunk file.  All chunks are later k-way merged on disk.
    ///
    /// | dict_chunk_mb | peak string-buffer RAM |
    /// |---------------|------------------------|
    /// |  50           |  ≈  50 MB              |
    /// | 200           |  ≈ 200 MB  (default)   |
    /// | 500           |  ≈ 500 MB              |
    ///
    /// Larger values mean fewer chunk files and a faster merge, at the cost of
    /// more RAM.  A value of 200 MB is a safe default for machines with ≥ 1 GB
    /// of free memory.
    pub dict_chunk_mb: usize,

    /// Number of threads used for parallel file loading during `ecordf build`.
    ///
    /// EcoRDF processes multiple input files in parallel: each thread handles
    /// one file at a time (Phase 1 string collection + Phase 2 triple loading),
    /// then all results are merged.  The total peak RAM stays approximately
    /// constant because per-thread budgets are scaled down by thread count.
    ///
    /// | parallel_threads | behaviour                            |
    /// |------------------|--------------------------------------|
    /// | 0                | all CPU cores (default, recommended) |
    /// | 1                | single-threaded (for debugging)      |
    /// | N                | exactly N threads                    |
    ///
    /// Set to 1 if you hit memory pressure on a machine with many cores.
    pub parallel_threads: usize,
}

impl Default for BuildConfig {
    fn default() -> Self {
        Self {
            chunk_size: 5_000_000,
            dict_chunk_mb: 200,
            parallel_threads: 0,
        }
    }
}

// ── Query config ──────────────────────────────────────────────────────────────

/// Tunables for the query executor.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct QueryConfig {
    /// Maximum number of rows any intermediate result may hold.
    ///
    /// Protects against out-of-memory (OOM) on pathological queries such as
    /// unconstrained cross-joins over large datasets.  When a join would push
    /// the row count past this limit, execution is cut short and an error is
    /// returned to the client.
    ///
    /// Memory estimate: `max_intermediate_rows × ~40 bytes`
    ///   -  1_000_000 rows ≈   40 MB
    ///   -  5_000_000 rows ≈  200 MB
    ///   - 50_000_000 rows ≈ 2000 MB ← default
    ///   -100_000_000 rows ≈ 4000 MB
    ///
    /// Raise on servers with plentiful RAM.
    /// Lower on memory-constrained environments (embedded, shared hosts).
    pub max_intermediate_rows: usize,

    /// Row threshold for choosing bind-join over hash-join.
    ///
    /// When the left-side result has **≤ this many rows** and the right-side
    /// plan is independent of left variables (e.g. a second BGP with no shared
    /// variable), the executor uses *bind-join*: it re-executes the right plan
    /// once per left row, substituting the current binding.  This gives the
    /// index a chance to prune using the bound value and is typically faster
    /// than materialising the whole right side and hash-joining.
    ///
    /// Above this threshold the executor switches to *hash-join*
    /// (materialise both sides, probe with a hash map) to bound memory use.
    ///
    /// Raise if your queries produce moderate-to-large left results that still
    /// benefit from index-guided right-side probing.
    /// Lower to prefer hash-join earlier (lower peak memory, but more scans).
    pub bind_join_threshold: usize,
}

impl Default for QueryConfig {
    fn default() -> Self {
        Self {
            max_intermediate_rows: 50_000_000,
            bind_join_threshold: 10_000,
        }
    }
}

// ── Server config ─────────────────────────────────────────────────────────────

/// HTTP server defaults.
///
/// All fields can be overridden by the corresponding CLI flag; the config
/// file only sets the *default* values when the flag is not supplied.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Bind address for the SPARQL HTTP endpoint.
    ///
    /// - `"127.0.0.1"` — loopback only; safe for local/development use.
    /// - `"0.0.0.0"` — all interfaces; required when serving over a network.
    ///
    /// Overridable with `--host` on the command line.
    pub host: String,

    /// TCP port for the SPARQL HTTP endpoint.
    ///
    /// Choose a port ≥ 1024 to avoid requiring root privileges.
    /// Common choices: 7878 (default), 8080, 3030.
    ///
    /// Overridable with `--port` on the command line.
    pub port: u16,

    /// Allowed CORS origins for the HTTP endpoint.
    ///
    /// Controls the `Access-Control-Allow-Origin` response header:
    ///
    /// - `""` (empty string) — CORS disabled; no ACAO header is sent.
    ///   **Use this for local or intranet deployments** where browser
    ///   cross-origin requests are not needed.
    /// - `"*"` — Allow any origin.  Convenient for public read-only APIs,
    ///   but do **not** use with authentication cookies or private data.
    /// - `"https://app.example.com,https://other.example.com"` — Whitelist
    ///   specific origins (comma-separated, no spaces).
    ///
    /// Overridable with `--cors` on the command line.
    pub cors_origins: String,

    /// Maximum number of queries that may execute concurrently.
    ///
    /// Each query runs in tokio's blocking thread pool (`spawn_blocking`).
    /// This limit caps how many blocking threads are occupied by query
    /// execution simultaneously, preventing a burst of slow queries from
    /// exhausting system resources.
    ///
    /// - `0` — no limit (bounded only by tokio's blocking pool, default 512).
    /// - `N` — at most N queries run in parallel; excess requests are queued.
    ///
    /// A good starting point is `2 × number_of_CPU_cores`.  Raise for
    /// I/O-bound or short queries; lower for RAM-heavy analytical queries
    /// where running too many in parallel causes memory pressure.
    pub max_concurrent_queries: usize,

    /// MiB of index data to read into the OS page cache in a background thread
    /// immediately after startup.
    ///
    /// EcoRDF uses `memmap2` — index pages are loaded on first access.  For
    /// large stores the first few queries after a cold start can be slow due to
    /// page faults.  This setting pre-populates the page cache before any
    /// queries arrive, eliminating that latency.
    ///
    /// The budget is spread across SPO / POS / OSP / dict (and GSPO if present);
    /// POS receives a 2× share as the most-heavily-used index.
    ///
    /// - `0` — disabled (default); no warmup is performed.
    /// - `4096`  — warm 4 GB; good starting point for medium stores.
    /// - `16384` — warm 16 GB; suitable for UniProt-scale datasets.
    ///
    /// Overridable with `--warmup-mb` on the command line (CLI takes precedence
    /// over this config value when non-zero).
    ///
    /// **RAM note**: warmup data goes into the OS page cache, not the process
    /// heap.  Process RSS barely changes.  The cache is evictable by the kernel
    /// under memory pressure.
    pub warmup_mb: u64,

    /// MiB of process heap to use for the in-RAM predicate cache.
    ///
    /// At startup EcoRDF loads medium-sized predicates from the POS index into
    /// a sorted in-RAM cache.  Cached predicates are accessed with O(log N)
    /// binary search or O(N) linear merge instead of O(N) HDD sequential scan,
    /// making **the first query as fast as subsequent (page-cached) ones**.
    ///
    /// Predicates are selected smallest-first; no single predicate may exceed
    /// 50% of the budget.  Building runs in a background thread so the server
    /// accepts queries immediately.
    ///
    /// **RAM impact**: this budget is process heap (RSS), not OS page cache.
    /// Avoid values that would squeeze out other workloads.
    ///
    /// The per-predicate cap is `pred_cache_mb / 2`.  For faldo:position
    /// (11.8 M entries × 16 B = 188 MB), the budget must be ≥ 512 MB so the
    /// cap (256 MB) exceeds the predicate's size.  The default 1024 MB gives a
    /// comfortable 512 MB cap and leaves room for both faldo predicates plus
    /// the many small JPOST predicates.
    ///
    /// | Value | Typical coverage (JPostDB)                       |
    /// |-------|--------------------------------------------------|
    /// | 0     | disabled                                          |
    /// | 512   | small predicates only (cap = 256 MB, but faldo:  |
    /// |       |   position = 188 MB just barely fits the cap)    |
    /// | 1024  | faldo:position + faldo:begin + JPOST predicates  |
    /// | 2048  | most predicates < 60 M triples (cap = 1024 MB)  |
    ///
    /// Overridable with `--pred-cache-mb` on the command line.
    pub pred_cache_mb: u64,

    /// Per-predicate size cap (MiB) for the predicate cache.
    ///
    /// Limits how large any single predicate's entry may be.  When a predicate's
    /// (subject, object) pairs would require more than this many MiB, it is skipped
    /// during cache build.
    ///
    /// **Why this matters**: the default cap is `pred_cache_mb / 2`.  If two
    /// predicates are each close to (but under) that cap, they can together consume
    /// almost the entire budget, leaving no room for smaller predicates that you
    /// actually care about.
    ///
    /// Example: with `pred_cache_mb = 1024` (cap = 512 MB), two predicates at
    /// 479 MB each consume 957 MB, leaving only 67 MB — not enough for
    /// faldo:begin/position (178 MB each).  Setting `pred_cache_per_pred_cap_mb = 200`
    /// skips the 479 MB predicates so faldo gets cached instead.
    ///
    /// | Value | Effect                                                        |
    /// |-------|---------------------------------------------------------------|
    /// | 0     | use `pred_cache_mb / 2` (default)                            |
    /// | 200   | skip predicates > 200 MB; keeps faldo (178 MB) within budget |
    /// | 512   | same as default with pred_cache_mb = 1024                    |
    ///
    /// Overridable with `--pred-cache-per-pred-cap-mb` on the command line.
    pub pred_cache_per_pred_cap_mb: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 7878,
            cors_origins: String::new(),
            max_concurrent_queries: 0, // unlimited by default
            warmup_mb: 0,              // disabled by default
            pred_cache_mb: 1024,           // 1024 MB heap; 50% cap = 512 MB/predicate (covers faldo)
            pred_cache_per_pred_cap_mb: 0, // 0 = use pred_cache_mb / 2 (default)
        }
    }
}

// ── Model config ──────────────────────────────────────────────────────────────

/// RDF model / schema hints for query optimisation.
///
/// EcoRDF can read rdf-config `model.yaml` + `prefix.yaml` files at startup
/// to discover commonly-used compound property paths (multi-hop traversals
/// through blank nodes).  These paths are pre-materialised in RAM so that
/// SPARQL property-path evaluation avoids repeated HDD scans.
///
/// ## Example
///
/// ```toml
/// [model]
/// rdf_configs = [
///   "https://github.com/dbcls/rdf-config/tree/master/config/uniprot",
///   "https://github.com/dbcls/rdf-config/tree/master/config/jpostdb",
///   "/local/path/to/my-rdf-config",
/// ]
/// path_cache_mb = 512
/// ```
///
/// Each entry in `rdf_configs` is either:
/// - A GitHub repository tree URL (`https://github.com/<owner>/<repo>/tree/<branch>/<path>`)
/// - A local directory path containing `prefix.yaml` and `model.yaml`
///
/// Paths of length ≥ 2 extracted from the model are materialised up to
/// `path_cache_mb` MiB of RAM.  Set `path_cache_mb = 0` to parse the model
/// but skip materialisation (useful for diagnostics).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ModelConfig {
    /// List of rdf-config directories to load (local paths or GitHub URLs).
    pub rdf_configs: Vec<String>,

    /// RAM budget (MiB) for the path cache.  Set to 0 to disable.
    pub path_cache_mb: u64,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            rdf_configs: Vec::new(),
            path_cache_mb: 0, // disabled unless explicitly configured
        }
    }
}

// ── Error type ────────────────────────────────────────────────────────────────

/// Error returned when a config file cannot be read or parsed.
#[derive(Debug)]
pub struct ConfigError(pub String);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "config error: {}", self.0)
    }
}

impl std::error::Error for ConfigError {}

impl From<ConfigError> for std::io::Error {
    fn from(e: ConfigError) -> Self {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
    }
}
