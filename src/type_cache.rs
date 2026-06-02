//! # TypeCache — fast `?x a SomeClass` membership lookup
//!
//! Builds a per-class Roaring Bitmap (subjects) from the `rdf:type` predicate
//! at startup.  Type-membership checks become O(1) bitmap lookups instead of
//! O(log N) binary searches on sorted Vec.
//!
//! ## Why Roaring Bitmap (改善3)
//!
//! Previous implementation used `Vec<TermId>` with `binary_search`.
//!
//! ```text
//! Improvement          | Vec<TermId>        | RoaringTreemap
//! ---------------------|--------------------|-----------------
//! Single contains()    | O(log N)           | O(1)
//! Two-class intersect  | O(N) merge         | SIMD AND, O(N/64)
//! Memory (3.7M subjects)| 28 MB              | 2–4 MB (dense) / 10–20 MB (sparse)
//! ```
//!
//! `RoaringTreemap` supports u64 keys (required since TermId = u64).
//! `RoaringBitmap` (u32 only) would not work for large datasets like UniProt.
//!
//! ## Memory layout
//!
//! ```text
//! classes: HashMap<class_id, RoaringTreemap>
//!   jpost:Peptide          → RoaringTreemap { 3.7M subjects }  ~2-4 MB
//!   jpost:PeptideEvidence  → RoaringTreemap { 9.3M subjects }  ~5-9 MB
//!   jpost:PSM              → RoaringTreemap { 10.5M subjects } ~5-10 MB
//!   …
//! ```

use std::collections::HashMap;

use roaring::RoaringTreemap;

use crate::dict_builder::QueryDict;
use crate::index::TripleIndex;
use crate::triple::{TermId, TriplePattern, UNBOUND};

// ── Public API ────────────────────────────────────────────────────────────────

/// Per-class subject membership cache built from `rdf:type` triples.
///
/// Uses [`RoaringTreemap`] per class for O(1) membership testing and
/// O(N/64) set intersection (SIMD AND), replacing the O(log N) binary
/// search on `Vec<TermId>` used previously.
#[derive(Clone)]
pub struct TypeCache {
    /// `TermId` of `rdf:type`, or `None` if not found in the dictionary.
    rdf_type_id: Option<TermId>,
    /// class_id → Roaring Bitmap of subject IDs.
    classes: HashMap<TermId, RoaringTreemap>,
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
    /// by class into Roaring Bitmaps.  Classes are loaded largest-first until
    /// `budget_bytes` is exhausted.  `budget_bytes = 0` returns `Self::empty()`.
    ///
    /// Memory estimate: `serialized_size()` per class bitmap.
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

        // Phase 1: collect subjects per class into Roaring Bitmaps.
        let mut raw: HashMap<TermId, RoaringTreemap> = HashMap::new();
        let mut total_triples = 0usize;
        for triple in index.scan(&pat) {
            raw.entry(triple.o).or_default().insert(triple.s);
            total_triples += 1;
        }

        // Phase 2: measure serialised size of each bitmap.
        let total_bytes: usize = raw.values()
            .map(|bm| bm.serialized_size())
            .sum();

        // Phase 3: apply budget — keep smallest classes first.
        let classes = if total_bytes <= budget_bytes {
            raw
        } else {
            // Sort by serialised size descending; insert smallest first.
            let mut by_size: Vec<(TermId, RoaringTreemap)> = raw.into_iter().collect();
            by_size.sort_unstable_by_key(|(_, bm)| std::cmp::Reverse(bm.serialized_size()));

            let mut budget_left = budget_bytes;
            let mut kept: HashMap<TermId, RoaringTreemap> = HashMap::new();

            for (class_id, bm) in by_size.into_iter().rev() {
                let cost = bm.serialized_size();
                if cost <= budget_left {
                    budget_left -= cost;
                    kept.insert(class_id, bm);
                }
            }
            kept
        };

        let bytes_used: usize = classes.values().map(|bm| bm.serialized_size()).sum();

        tracing::info!(
            classes = classes.len(),
            total_triples,
            mb_used = bytes_used / (1024 * 1024),
            mb_budget = budget_bytes / (1024 * 1024),
            elapsed_ms = t.elapsed().as_millis(),
            "type-cache: build complete (RoaringTreemap)"
        );

        Self { rdf_type_id: Some(rdf_type_id), classes, bytes_used }
    }

    // ── Query API ─────────────────────────────────────────────────────────────

    /// The `TermId` of `rdf:type`, or `None` if the dictionary has no entry.
    #[inline]
    pub fn rdf_type_id(&self) -> Option<TermId> {
        self.rdf_type_id
    }

    /// Returns `true` if `class_id` has a cached bitmap.
    #[inline]
    pub fn has_class(&self, class_id: TermId) -> bool {
        self.classes.contains_key(&class_id)
    }

    /// Returns the bitmap for `class_id`, or `None` if the class is not cached.
    #[inline]
    pub fn get_bitmap(&self, class_id: TermId) -> Option<&RoaringTreemap> {
        self.classes.get(&class_id)
    }

    /// Returns a sorted subject slice for `class_id` by iterating the bitmap,
    /// or `None` if the class is not cached.
    ///
    /// **Prefer [`get_bitmap`] + [`RoaringTreemap::contains`]** for single
    /// membership checks.  Use this only when a `&[TermId]` slice is required.
    #[inline]
    pub fn get_class(&self, class_id: TermId) -> Option<Vec<TermId>> {
        self.classes.get(&class_id).map(|bm| bm.iter().collect())
    }

    /// Check whether `subject_id` is an instance of `class_id` in O(1).
    ///
    /// Returns `Some(true/false)` when the class is cached,
    /// `None` when the class is absent (caller must fall back to index scan).
    #[inline]
    pub fn contains(&self, class_id: TermId, subject_id: TermId) -> Option<bool> {
        self.classes
            .get(&class_id)
            .map(|bm| bm.contains(subject_id))
    }

    /// Intersect two classes and return a sorted Vec of common subjects.
    ///
    /// Uses SIMD AND on the underlying bitmaps — O(N/64) instead of O(N) merge.
    /// Returns `None` if either class is not cached.
    #[inline]
    pub fn intersect_classes(&self, class_a: TermId, class_b: TermId) -> Option<Vec<TermId>> {
        let bm_a = self.classes.get(&class_a)?;
        let bm_b = self.classes.get(&class_b)?;
        let intersection = bm_a & bm_b;
        Some(intersection.iter().collect())
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
