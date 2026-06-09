//! Core triple types.
//!
//! All strings are stored as u64 IDs from the Dictionary.
//! This keeps the hot path (index operations) to pure integer arithmetic.
//! u64 IDs are required for datasets (such as full UniProt) whose unique term
//! count exceeds the u32 limit of ~4.3 billion.

/// An RDF term as a dictionary ID.
/// u64::MAX is reserved as "unbound" (variable marker in pattern matching).
pub type TermId = u64;

/// Sentinel value for an unbound variable in a triple pattern.
pub const UNBOUND: TermId = u64::MAX;

/// Special IRI used as the default graph identifier in GSPO index.
pub const DEFAULT_GRAPH_IRI: &str = "urn:ecordf:default";

/// A concrete RDF triple (subject, predicate, object) with encoded IDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Triple {
    pub s: TermId,
    pub p: TermId,
    pub o: TermId,
}

/// A concrete RDF quad (triple + named graph) with encoded IDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Quad {
    pub s: TermId,
    pub p: TermId,
    pub o: TermId,
    pub g: TermId,
}

impl Quad {
    pub fn new(s: TermId, p: TermId, o: TermId, g: TermId) -> Self {
        Self { s, p, o, g }
    }

    pub fn to_triple(self) -> Triple {
        Triple::new(self.s, self.p, self.o)
    }
}

impl Triple {
    #[inline]
    pub fn new(s: TermId, p: TermId, o: TermId) -> Self {
        Self { s, p, o }
    }

    /// Encode as a 24-byte array for binary storage (3 × u64 LE).
    #[inline]
    pub fn to_bytes(self) -> [u8; 24] {
        let mut b = [0u8; 24];
        b[0..8].copy_from_slice(&self.s.to_le_bytes());
        b[8..16].copy_from_slice(&self.p.to_le_bytes());
        b[16..24].copy_from_slice(&self.o.to_le_bytes());
        b
    }

    /// Decode from a 24-byte array (3 × u64 LE).
    #[inline]
    pub fn from_bytes(b: &[u8; 24]) -> Self {
        Self {
            s: u64::from_le_bytes(b[0..8].try_into().unwrap()),
            p: u64::from_le_bytes(b[8..16].try_into().unwrap()),
            o: u64::from_le_bytes(b[16..24].try_into().unwrap()),
        }
    }
}

/// A triple pattern where any component may be UNBOUND (i.e., a variable).
#[derive(Debug, Clone, Copy)]
pub struct TriplePattern {
    pub s: TermId,
    pub p: TermId,
    pub o: TermId,
}

impl TriplePattern {
    pub fn new(s: TermId, p: TermId, o: TermId) -> Self {
        Self { s, p, o }
    }

    /// Number of bound (constant) positions.
    pub fn bound_count(&self) -> u8 {
        (self.s != UNBOUND) as u8 + (self.p != UNBOUND) as u8 + (self.o != UNBOUND) as u8
    }

    /// True if this triple matches the pattern.
    #[inline]
    pub fn matches(&self, t: &Triple) -> bool {
        (self.s == UNBOUND || self.s == t.s)
            && (self.p == UNBOUND || self.p == t.p)
            && (self.o == UNBOUND || self.o == t.o)
    }
}

/// Which index to use for a given triple pattern.
///
/// The six permutations cover all combinations of two bound positions:
///
/// ```text
///   Index  Sort order   Primary  Secondary  Tertiary   Best for
///   ─────  ──────────   ───────  ─────────  ────────   ────────
///   SPO    (S, P, O)    S        P          O          s, s+p, all
///   POS    (P, O, S)    P        O          S          p, p+o   (pred_idx → O(1) pred range)
///   OSP    (O, S, P)    O        S          P          o, o+s
///   PSO    (P, S, O)    P        S          O          p+s      (pred_idx → O(1) pred range)
///   SOP    (S, O, P)    S        O          P          s+o      (skip→ O(log deg(s)))
///   OPS    (O, P, S)    O        P          S          o+p      (skip→ O(log deg(o)))
/// ```
///
/// PSO, SOP, OPS are present only in stores built with `--six-indexes` (or the new
/// default).  Older stores fall back to the nearest existing 3-index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexKind {
    Spo, // sorted (S,P,O): primary=S, secondary=P
    Pos, // sorted (P,O,S): primary=P (pred_idx), secondary=O
    Osp, // sorted (O,S,P): primary=O, secondary=S
    Pso, // sorted (P,S,O): primary=P (pred_idx), secondary=S
    Sop, // sorted (S,O,P): primary=S, secondary=O
    Ops, // sorted (O,P,S): primary=O, secondary=P
}

impl TriplePattern {
    /// Choose the best index given which components are bound.
    ///
    /// For two-bound patterns the index whose *primary* key matches the first
    /// bound variable is preferred, breaking ties by choosing the index whose
    /// *secondary* key also matches — this lets `range_for_pattern` narrow the
    /// search to `O(log degree(primary_key))` rather than `O(log total_count)`.
    ///
    /// PSO / SOP / OPS are used when available (6-index store).  `TripleIndex`
    /// falls back to the nearest 3-index when the extra files are absent.
    pub fn best_index(&self) -> IndexKind {
        match (self.s != UNBOUND, self.p != UNBOUND, self.o != UNBOUND) {
            // ── two or three bound ──────────────────────────────────────────────
            // (s+p): SPO secondary search bounded by degree(s) after upper_hint.
            // For very large predicates this beats PSO's O(log |pred|).
            (true,  true,  _   ) => IndexKind::Spo,
            // (s+o): SOP — primary=s (skip), secondary=o binary within s's range.
            // SPO secondary key is P, so it can't efficiently find a bound O.
            (true,  false, true ) => IndexKind::Sop,
            // (p+o): POS — pred_idx gives exact predicate range O(1), then
            // binary search for O within that range.
            (false, true,  true ) => IndexKind::Pos,
            // ── single bound ────────────────────────────────────────────────────
            (true,  false, false) => IndexKind::Spo,
            (false, true,  false) => IndexKind::Pos,
            (false, false, true ) => IndexKind::Osp,
            // ── none bound (full scan) ──────────────────────────────────────────
            (false, false, false) => IndexKind::Spo,
            // (all three bound is already covered by the (true, true, _) arm above)
        }
    }
}
