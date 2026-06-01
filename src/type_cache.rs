//! # TypeCache — fast `?x a SomeClass` membership lookup
//!
//! Builds a per-class sorted `Vec<TermId>` (subjects) from the `rdf:type`
//! predicate at startup.  Type-membership checks become O(log N) binary
//! searches instead of O(|pred_range|) POS/OPS scans.
//!
//! ## Why this helps
//!
//! `rdf:type` is typically the largest predicate in biological RDF stores
//! (e.g. 1290 MB in JPostDB).  Executing `?x a jpost:PSM` with pred_cache
//! disabled requires scanning 1290 MB of POS data.  TypeCache reduces this
//! to O(log |class|) per subject — microseconds instead of seconds.
//!
//! ## Memory layout
//!
//! ```text
//! classes: HashMap<class_id, Vec<subject_id>>
//!   jpost:Peptide          → [id1, id2, …]  sorted  28 MB
//!   jpost:PeptideEvidence  → [id1, id2, …]  sorted  68 MB
//!   jpost:PSM              → [id1, id2, …]  sorted  80 MB
//!   …
//! ```
//!
//! Total for JPostDB: ~230 MB (configurable via `type_cache_mb`).

use std::collections::HashMap;

use crate::dict_builder::QueryDict;
use crate::index::TripleIndex;
use crate::triple::{TermId, TriplePattern, UNBOUND};

// ── Public API ────────────────────────────────────────────────────────────────

/// Per-class subject membership cache built from `rdf:type` triples.
#[derive(Clone)]
pub struct TypeCache {
    /// `TermId` of `rdf:type`, or `None` if not found in the dictionary.
    rdf_type_id: Option<TermId>,
    /// class_id → sorted, deduped subject list.
    classes: HashMap<TermId, Vec<TermId>>,
    bytes_used: usize,
}

impl TypeCache {
    /// Empty cache (no memory, always returns `None` for every lookup).
    pub fn empty() -> Self {
        Self { rdf_type_id: None, classes: HashMap::new(), bytes_used: 0 }
    }

    /// Build from the `rdf:type` predicate in `index`.
    ///
    /// Scans the full `rdf:type` POS range once at startup and groups subjects
    /// by class.  Classes are loaded largest-first until `budget_bytes` is
    /// exhausted.  `budget_bytes = 0` returns `Self::empty()` immediately.
    pub fn build(index: &TripleIndex, dict: &QueryDict, budget_bytes: usize) -> Self {
        if budget_bytes == 0 {
            return Self::empty();
        }

        let rdf_type_id = match dict.lookup("http://www.w3.org/1999/02/22-rdf-syntax-ns#type") {
            Some(id) => id,
            None => {
                tracing::debug!("type-cache: rdf:type not in dictionary, skipping build");
                return Self::empty();
            }
        };

        let t = std::time::Instant::now();
        let pat = TriplePattern { s: UNBOUND, p: rdf_type_id, o: UNBOUND };

        // Phase 1: collect subjects per class.
        let mut raw: HashMap<TermId, Vec<TermId>> = HashMap::new();
        for triple in index.scan(&pat) {
            raw.entry(triple.o).or_default().push(triple.s);
        }
        let total_triples: usize = raw.values().map(|v| v.len()).sum();

        // Phase 2: sort + dedup each class.
        for subjects in raw.values_mut() {
            subjects.sort_unstable();
            subjects.dedup();
        }

        // Phase 3: apply budget (drop largest classes first if over budget).
        let total_bytes: usize = raw.values().map(|v| v.len() * 8).sum();
        let classes = if total_bytes <= budget_bytes {
            raw
        } else {
            // Sort by size descending; drop until within budget.
            let mut by_size: Vec<(TermId, Vec<TermId>)> = raw.into_iter().collect();
            by_size.sort_unstable_by_key(|(_, v)| std::cmp::Reverse(v.len()));

            let mut budget_left = budget_bytes;
            let mut kept: HashMap<TermId, Vec<TermId>> = HashMap::new();

            // Insert smallest classes first (they use less budget).
            for (class_id, subjects) in by_size.into_iter().rev() {
                let cost = subjects.len() * 8;
                if cost <= budget_left {
                    budget_left -= cost;
                    kept.insert(class_id, subjects);
                }
                // Skip classes that don't fit.
            }
            kept
        };

        let bytes_used: usize = classes.values().map(|v| v.len() * 8).sum();

        tracing::info!(
            classes = classes.len(),
            total_triples,
            mb_used = bytes_used / (1024 * 1024),
            mb_budget = budget_bytes / (1024 * 1024),
            elapsed_ms = t.elapsed().as_millis(),
            "type-cache: build complete"
        );

        Self { rdf_type_id: Some(rdf_type_id), classes, bytes_used }
    }

    // ── Query API ─────────────────────────────────────────────────────────────

    /// The `TermId` of `rdf:type`, or `None` if the dictionary has no entry.
    #[inline]
    pub fn rdf_type_id(&self) -> Option<TermId> {
        self.rdf_type_id
    }

    /// Returns the sorted subject slice for `class_id`, or `None` if the class
    /// is not in the cache (either not present in data or evicted by budget).
    #[inline]
    pub fn get_class(&self, class_id: TermId) -> Option<&[TermId]> {
        self.classes.get(&class_id).map(|v| v.as_slice())
    }

    /// Check whether `subject_id` is an instance of `class_id`.
    ///
    /// Returns `Some(true/false)` when the class is cached,
    /// `None` when the class is absent (caller must fall back to index scan).
    #[inline]
    pub fn contains(&self, class_id: TermId, subject_id: TermId) -> Option<bool> {
        self.classes
            .get(&class_id)
            .map(|subjects| subjects.binary_search(&subject_id).is_ok())
    }

    /// Number of distinct classes loaded.
    pub fn len(&self) -> usize {
        self.classes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.classes.is_empty()
    }

    pub fn bytes_used(&self) -> usize {
        self.bytes_used
    }
}
