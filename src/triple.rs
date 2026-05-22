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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexKind {
    Spo, // bound: S → S,P,O
    Pos, // bound: P → P,O,S
    Osp, // bound: O → O,S,P
}

impl TriplePattern {
    /// Choose the best index given which components are bound.
    pub fn best_index(&self) -> IndexKind {
        match (self.s != UNBOUND, self.p != UNBOUND, self.o != UNBOUND) {
            (true, _, _) => IndexKind::Spo,
            (false, true, _) => IndexKind::Pos,
            (false, false, true) => IndexKind::Osp,
            (false, false, false) => IndexKind::Spo, // full scan
        }
    }
}
