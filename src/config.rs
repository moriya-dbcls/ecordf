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
//! # Per-query memory cap (MiB) for intermediate results. 0 = disabled (rows-only).
//! max_intermediate_mb   = 0
//!
//! [server]
//! host        = "127.0.0.1"
//! port        = 7878
//! cors_origins = ""
//! # Deprecated; prefer total_query_mem_mb. 0 = rely on the memory pool.
//! max_concurrent_queries = 0
//! # Server-wide query memory pool (MiB). New queries reserve their per-query
//! # limit from here before running. 0 = no pool gate (back-compat).
//! total_query_mem_mb = 0
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

    /// Automatically compress column files with delta + Zstd (ECOCOL04) after build.
    ///
    /// When `true`, runs both `compress-cols` (delta encoding) and
    /// `recompress-zstd` (Zstd block compression) immediately after the index
    /// build completes. The resulting `.c0.zst` / `.c1.zst` / `.c2.zst` files
    /// are 10–20× smaller than the raw columns and dramatically reduce I/O cost
    /// for HDD/SSD-based stores.
    ///
    /// Default: `false`.  Enable with `--compress` on the command line or set
    /// `auto_compress = true` in `ecordf.toml`.
    ///
    /// ## 環境別推奨値
    ///
    /// | 環境 | 推奨値 | 理由 |
    /// |------|--------|------|
    /// | HDD（7200 rpm 等） | **`true` を強く推奨** | I/O 削減がクエリ速度に直結 |
    /// | SATA SSD | **`true` を推奨** | ディスク節約 + I/O ボトルネック解消 |
    /// | NVMe SSD | 任意 | ディスク節約目的なら `true` |
    /// | 全データが OS ページキャッシュに乗る大容量 RAM | `false` を推奨 | Zstd 展開コストが相対的に割高 |
    pub auto_compress: bool,

    /// Per-file buffer size for Phase 2a string collection (MB).
    ///
    /// Larger values reduce the number of p2a chunk files, speeding up the
    /// streaming Phase 2 join step at the cost of more RAM per thread.
    /// Default: 64 MB.  For 50B+ triple datasets, 512–2048 is recommended.
    pub p2a_buf_mb: usize,

    /// Automatically run blank-node semantic reordering after the index is built.
    ///
    /// Groups blank nodes sharing the same `rdf:type` into consecutive TermId
    /// ranges so that bind-join probes and LFTJ seeks access a compact region
    /// of the column index.  Expected improvement: up to ~20× cache efficiency
    /// for type-filtered blank-node queries.
    ///
    /// Cost: one additional pass over the index (similar to Phase 2 time).
    ///
    /// Default: `true`.  Disable with `--no-reorder-bnodes` on the command line
    /// or set `reorder_bnodes = false` in `ecordf.toml` for fast iterative builds.
    pub reorder_bnodes: bool,
}

impl Default for BuildConfig {
    fn default() -> Self {
        Self {
            chunk_size: 5_000_000,
            dict_chunk_mb: 200,
            parallel_threads: 0,
            auto_compress: false,
            p2a_buf_mb: 64,
            reorder_bnodes: true,
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

    /// Memory cap (MiB) a single query's intermediate results may use.  0 = disabled.
    ///
    /// Complements `max_intermediate_rows`: that limit bounds the *row count*
    /// regardless of width, whereas this bounds the *byte footprint* by also
    /// accounting for the result's arity (number of columns).  The executor
    /// derives an arity-aware row cap via [`QueryConfig::row_cap`] and enforces
    /// the stricter of the two.
    ///
    /// - `0` — disabled (default); only `max_intermediate_rows` is enforced.
    ///   Preserves the previous behaviour for backward compatibility.
    /// - `N` — a query may keep at most `N` MiB of intermediate rows; wide
    ///   results (more columns) are capped at proportionally fewer rows.
    ///
    /// This is the per-query budget the server reserves from the global pool
    /// (`server.total_query_mem_mb`) before running a query.
    pub max_intermediate_mb: usize,

    /// Minimum per-query memory reservation (MiB) for variable admission.
    ///
    /// The server estimates each query's intermediate-result peak before it runs
    /// and reserves a proportional, query-specific slice of the global pool
    /// (`server.total_query_mem_mb`) — instead of every query reserving the full
    /// `max_intermediate_mb`.  This floor is the *smallest* such reservation: it
    /// is also the runtime memory cap a light query receives.
    ///
    /// `floor(total_query_mem_mb / query_reserve_floor_mb)` is therefore the
    /// approximate maximum number of light queries that may run concurrently.
    /// Raising the floor lowers light-query concurrency but gives each light
    /// query more head-room; lowering it admits more light queries at once.
    ///
    /// Default: `256`.
    pub query_reserve_floor_mb: u64,

    /// Safety multiplier (percent) applied to the estimated intermediate-result
    /// peak when sizing a query's reservation.  `150` = ×1.5.
    ///
    /// The cardinality estimate is approximate; multiplying by this factor guards
    /// against mild under-estimation so that a query is not aborted by its own
    /// (reservation-derived) runtime memory cap.  Higher values are safer but
    /// reserve more of the pool per query, reducing concurrency.
    ///
    /// Default: `150`.
    pub query_reserve_safety_pct: u32,
}

impl Default for QueryConfig {
    fn default() -> Self {
        Self {
            max_intermediate_rows: 50_000_000,
            bind_join_threshold: 10_000,
            max_intermediate_mb: 0, // disabled; only max_intermediate_rows enforced (back-compat)
            query_reserve_floor_mb: 256,  // minimum per-query reservation = light-query cap
            query_reserve_safety_pct: 150, // ×1.5 head-room over the estimated peak
        }
    }
}

impl QueryConfig {
    /// 指定アリティ(列数)の中間結果が保持してよい最大行数。
    /// 絶対行数上限 max_intermediate_rows と、メモリ上限 max_intermediate_mb の
    /// 両方を尊重し、厳しい方を返す。
    pub fn row_cap(&self, arity: usize) -> usize {
        let arity = arity.max(1) as u64;
        let by_rows = self.max_intermediate_rows;
        if self.max_intermediate_mb == 0 {
            return by_rows.max(1);
        }
        // 1 TermId=8B + Vec/dedup オーバヘッド込みで保守的に 24B/列 と見積る
        const BYTES_PER_TERM: u64 = 24;
        let budget = self.max_intermediate_mb as u64 * 1024 * 1024;
        let by_mem = (budget / (arity * BYTES_PER_TERM)) as usize;
        by_mem.min(by_rows).max(1)
    }

    /// 推定行数・アリティから「このクエリの予約MB（= 実行時メモリ上限）」を算出する。
    ///
    /// `reserve = clamp( ceil(est_rows * arity * BYTES_PER_TERM * safety_pct/100 / 1MiB),
    ///                   query_reserve_floor_mb, ceil )`
    /// で、`ceil = max_intermediate_mb`。`max_intermediate_mb == 0`（cap 無効・レガシー）の
    /// ときは上限クランプを行わず `max(floor, est由来MB)` を返す。
    ///
    /// 予約 = 実行時上限。サーバはこの戻り値をプールから予約し、同じ値を `budget_mb` として
    /// クエリへ渡すことで「プール総和 ≤ プール容量」を厳密に保つ。すべて飽和演算で行うため
    /// `est_rows == u64::MAX` 等でも overflow せず、ceil（または現実的な大値）に張り付く。
    pub fn reserve_mb_for(&self, est_rows: u64, arity: usize) -> u64 {
        // row_cap と同じ保守係数（8B TermId + Vec/dedup オーバヘッド込み）。
        const BYTES_PER_TERM: u64 = 24;
        const MIB: u64 = 1024 * 1024;

        let arity = (arity.max(1)) as u64;
        let floor = self.query_reserve_floor_mb.max(1);

        // bytes = est_rows * arity * BYTES_PER_TERM * safety_pct / 100（飽和）。
        let bytes = est_rows
            .saturating_mul(arity)
            .saturating_mul(BYTES_PER_TERM)
            .saturating_mul(self.query_reserve_safety_pct as u64)
            / 100;
        // ceil(bytes / 1MiB)（飽和加算で overflow させない）。
        let est_mb = bytes.saturating_add(MIB - 1) / MIB;

        if self.max_intermediate_mb == 0 {
            // cap 無効: 上限クランプ無し。floor は必ず保証する。
            return est_mb.max(floor);
        }

        // ceil = max_intermediate_mb。floor > ceil の誤設定でも panic しないようガード。
        let ceil = (self.max_intermediate_mb as u64).max(floor);
        est_mb.clamp(floor, ceil)
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

    /// **非推奨。** `total_query_mem_mb` によるメモリ予約が主ゲート。`0` 推奨。
    /// `> 0` のときのみ追加の数ゲートとして併用する。
    ///
    /// Maximum number of queries that may execute concurrently.
    ///
    /// Each query runs in tokio's blocking thread pool (`spawn_blocking`).
    /// This limit caps how many blocking threads are occupied by query
    /// execution simultaneously, preventing a burst of slow queries from
    /// exhausting system resources.
    ///
    /// - `0` — no separate count limit (**recommended**); concurrency is governed
    ///   instead by the memory pool `total_query_mem_mb`, which admits roughly
    ///   `floor(total_query_mem_mb / per-query reservation)` queries at a time.
    /// - `N` — additionally cap at N queries in parallel; excess requests are
    ///   queued.  Used as a supplementary count gate *on top of* the memory pool.
    ///
    /// Historically a good starting point was `2 × number_of_CPU_cores`, but
    /// the memory pool now bounds concurrency more precisely, so leave this at
    /// `0` and tune `total_query_mem_mb` instead.
    pub max_concurrent_queries: usize,

    /// サーバ全体のクエリ用メモリプール(MiB)。`0` = プールゲートなし（後方互換）。
    ///
    /// Server-wide memory pool (MiB) for query intermediate results.  A new query
    /// reserves its per-query limit (≈ `query.max_intermediate_mb`, or an estimate
    /// derived from `query.max_intermediate_rows` when that is 0) from this pool
    /// before it starts executing.  When the pool lacks enough free budget, the
    /// query **waits** (it is not rejected) until an in-flight query finishes and
    /// returns its reservation.  Because each query reserves its full limit up
    /// front, no query blocks for more memory mid-flight, so this cannot deadlock.
    ///
    /// Effective concurrency is therefore `floor(total_query_mem_mb / per-query
    /// reservation)`, which is the intended replacement for `max_concurrent_queries`.
    ///
    /// - `0` — disabled (default); no pool gate, queries start immediately
    ///   (the previous unbounded behaviour).  Preserves backward compatibility.
    /// - `N` — admit queries only while the reserved total stays ≤ N MiB.
    ///
    /// Set this to a safe fraction of physical RAM (leaving room for caches and
    /// the page cache) to prevent the OOM killer from terminating the server.
    pub total_query_mem_mb: u64,

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

    /// RAM budget for the TypeCache (MiB).  0 = disabled.
    ///
    /// Builds a per-class `HashSet<subject_id>` from `rdf:type` at startup,
    /// turning `?x a SomeClass` filter steps from O(|pred_range|) POS scans
    /// into O(log |class|) binary searches.
    ///
    /// Recommended: 256–512 MB covers all classes in most bio RDF stores.
    pub type_cache_mb: u64,

    /// Per-query wall-clock timeout in seconds.  0 = no timeout (default).
    ///
    /// When a query exceeds this limit:
    ///   1. A cancellation flag is set — the executor detects it at its next
    ///      inner-loop checkpoint and returns an error result immediately.
    ///   2. The HTTP response is 408 Request Timeout with a JSON error body.
    ///
    /// Recommended: 30–300 seconds depending on expected workload.
    /// Set lower values (e.g. 60) when EcoRDF shares a server with other
    /// services to prevent runaway queries from consuming the page cache.
    pub query_timeout_secs: u64,

    /// Advise the OS to release index pages from page cache after large
    /// sequential scans (MADV_DONTNEED).  0 = disabled (default).
    ///
    /// When enabled, any sequential POS/SPO/OSP scan that reads more than
    /// `scan_dontneed_mb` MB will call `madvise(MADV_DONTNEED)` on the
    /// consumed range after the scan completes, releasing those pages back
    /// to the OS page cache pool.
    ///
    /// Effect: prevents EcoRDF from monopolising the page cache after large
    /// scans, reducing impact on co-located services.
    /// Trade-off: subsequent identical scans will page-fault again (cold).
    /// Recommended: 512–2048 MB threshold.  Disable (0) if EcoRDF is the
    /// only significant service on the host.
    pub scan_dontneed_mb: u64,

}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 7878,
            cors_origins: String::new(),
            max_concurrent_queries: 0,     // deprecated; 0 = rely on total_query_mem_mb pool
            total_query_mem_mb: 0,         // 0 = no pool gate (back-compat); set to bound RAM
            warmup_mb: 0,                  // disabled by default
            pred_cache_mb: 1024,           // 1024 MB heap; 50% cap = 512 MB/predicate (covers faldo)
            pred_cache_per_pred_cap_mb: 0, // 0 = use pred_cache_mb / 2 (default)
            type_cache_mb: 256,            // 256 MB covers all rdf:type classes in bio RDF
            query_timeout_secs: 0,         // no timeout by default
            scan_dontneed_mb: 0,           // disabled by default (enable when sharing server)
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

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a QueryConfig with an explicit byte ceiling for reservation tests.
    fn qcfg(max_mb: usize) -> QueryConfig {
        QueryConfig {
            max_intermediate_mb: max_mb,
            ..QueryConfig::default()
        }
    }

    #[test]
    fn reserve_small_est_hits_floor() {
        // A 1-row, 1-column query is far below the floor → reserved at the floor.
        let c = qcfg(4096); // floor 256, safety 150
        assert_eq!(c.reserve_mb_for(1, 1), 256);
        assert_eq!(c.reserve_mb_for(0, 3), 256);
    }

    #[test]
    fn reserve_mid_est_is_proportional() {
        // 10M rows × arity 2 × 24 B × 1.5 = 720_000_000 B ≈ 687 MiB, inside [256, 4096].
        let c = qcfg(4096);
        let mb = c.reserve_mb_for(10_000_000, 2);
        assert!(mb > 256 && mb < 4096, "expected proportional value, got {mb}");
        // ceil(720_000_000 / 1MiB) = 687.
        assert_eq!(mb, 687);
    }

    #[test]
    fn reserve_saturates_to_ceil() {
        // u64::MAX must saturate to the ceiling, never wrap to a small value.
        let c = qcfg(4096);
        assert_eq!(c.reserve_mb_for(u64::MAX, 1), 4096);
        assert_eq!(c.reserve_mb_for(u64::MAX, 16), 4096);
    }

    #[test]
    fn reserve_arity_increases_reservation() {
        let c = qcfg(4096);
        let a1 = c.reserve_mb_for(5_000_000, 1);
        let a4 = c.reserve_mb_for(5_000_000, 4);
        assert!(a4 > a1, "wider arity should reserve more: {a1} vs {a4}");
    }

    #[test]
    fn reserve_no_ceiling_when_cap_disabled() {
        // max_intermediate_mb == 0: no upper clamp, but floor is still guaranteed
        // and the value must not overflow on u64::MAX.
        let c = qcfg(0);
        assert_eq!(c.reserve_mb_for(1, 1), 256); // floor
        let huge = c.reserve_mb_for(u64::MAX, 8);
        assert!(huge >= 256, "floor must hold even with cap disabled, got {huge}");
        // No panic / wrap: result is a sane large-but-finite u64.
        assert!(huge < u64::MAX);
    }
}
