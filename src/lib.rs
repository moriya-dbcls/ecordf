//! # EcoRDF — Cost-Efficient RDF Triple Store
//!
//! ## Design Goals
//!
//! - **Virtuoso** の複雑な SPARQL JOIN における遅さを、Leapfrog Triejoin（最悪ケース最適）で解決する。
//! - **低メモリ・低依存** な構成で大規模 RDF データを扱えるようにする。
//!
//! memmap2 による OS 管理ページング、外部ソートによるビルド時 RAM バウンド、
//! ヒストグラムベースの結合順序最適化が主な特徴です。
//!
//! ## Architecture
//!
//! ```text
//!   SPARQL query
//!       │
//!   [Parser]  ── recursive-descent, zero-copy token refs
//!       │
//!   [Algebra] ── SPARQL → relational algebra tree
//!       │
//!   [Optimizer] ── histogram-based cardinality estimation
//!       │            greedy join ordering
//!       │
//!   [Executor] ── Leapfrog Triejoin for BGPs
//!       │          Hash join fallback for OPTIONAL/UNION
//!       │
//!   [Index Layer] ── 3 sorted mmap arrays (SPO, POS, OSP)
//!       │             binary search for range scans
//!       │
//!   [Dictionary] ── string ↔ u32 with namespace compression
//! ```

pub mod col_delta;
pub mod config;
pub mod dict;
pub mod dict_builder;
pub mod index;
pub mod loader;
pub mod path_cache;
pub mod pred_partition;
pub mod predcache;
pub mod rdf_config;
pub mod server;
pub mod sparql;
pub mod stats;
pub mod store;
pub mod triple;
pub mod type_cache;

pub use config::Config;
pub use loader::InputSpec;
pub use stats::StoreStatistics;
pub use store::Store;
