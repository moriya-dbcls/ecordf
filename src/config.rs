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
//! [query]
//! max_intermediate_rows = 5_000_000
//! bind_join_threshold   = 10_000
//!
//! [server]
//! host        = "127.0.0.1"
//! port        = 7878
//! cors_origins = ""
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
    /// Query execution tunables.
    pub query: QueryConfig,
    /// HTTP server defaults (overridable via CLI flags).
    pub server: ServerConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            query: QueryConfig::default(),
            server: ServerConfig::default(),
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
    ///   - 1_000_000 rows ≈  40 MB
    ///   - 5_000_000 rows ≈ 200 MB  ← default
    ///  10_000_000 rows ≈ 400 MB
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
            max_intermediate_rows: 5_000_000,
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
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 7878,
            cors_origins: String::new(),
            max_concurrent_queries: 0, // unlimited by default
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
