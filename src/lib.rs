//! # EcoRDF — Cost-Efficient RDF Triple Store
//!
//! ## Design Goals
//!
//! | System   | Problem                        | Our solution                        |
//! |----------|--------------------------------|-------------------------------------|
//! | Virtuoso | Slow on complex SPARQL JOINs   | Leapfrog Triejoin (optimal)         |
//! | Qlever   | Loads entire dataset into RAM  | memmap2: OS-managed paging          |
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

pub mod config;
pub mod dict;
pub mod index;
pub mod loader;
pub mod server;
pub mod sparql;
pub mod stats;
pub mod store;
pub mod triple;

pub use config::Config;
pub use loader::InputSpec;
pub use stats::StoreStatistics;
pub use store::Store;
