//! # Dictionary: String ↔ u32 ID mapping
//!
//! ## Design
//!
//! The dictionary is the bridge between the human-readable RDF world (URIs, literals)
//! and the integer world of the indexes.
//!
//! ### Memory layout
//!
//! In memory: two parallel structures
//!   - `id_to_str: Vec<Box<str>>`  — decode by index
//!   - `str_to_id: HashMap<&str, u32>` — encode by string (zero-copy refs into id_to_str)
//!
//! ### Namespace compression
//!
//! Common bio prefixes (uniprot, pdb, go, mesh, chebi, ...) are stored as prefix table.
//! Full URI = prefix[i] + local_name. Saves ~40% dictionary size for typical bio datasets.
//!
//! ### Persistence format (dict.bin)
//!
//! ```text
//! [magic: b"ECOD0001"]
//! [prefix_count: u32]
//! for each prefix:
//!   [len: u16][bytes: ...]
//! [term_count: u32]
//! for each term:
//!   [prefix_id: u16]  (0xFFFF = no prefix, raw string follows)
//!   [len: u32][bytes: ...]
//! ```

use rustc_hash::FxHashMap;
use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::path::Path;
use std::sync::RwLock;

/// Common RDF namespaces in life science datasets.
/// Stored as a prefix table to save memory.
pub const KNOWN_PREFIXES: &[&str] = &[
    "http://purl.uniprot.org/uniprot/",
    "http://purl.uniprot.org/core/",
    "http://rdf.wwpdb.org/pdb/",
    "http://identifiers.org/pdb/",
    "http://purl.obolibrary.org/obo/",
    "http://www.w3.org/1999/02/22-rdf-syntax-ns#",
    "http://www.w3.org/2000/01/rdf-schema#",
    "http://www.w3.org/2002/07/owl#",
    "http://www.w3.org/2001/XMLSchema#",
    "http://purl.org/dc/terms/",
    "http://togoid.dbcls.jp/ontology/",
    "http://identifiers.org/ncbigene/",
    "http://identifiers.org/taxonomy/",
    "http://identifiers.org/pubmed/",
    "http://identifiers.org/chebi/",
    "http://identifiers.org/mesh/",
    "http://bio2rdf.org/go:",
    "http://semanticscience.org/resource/",
    "https://www.ncbi.nlm.nih.gov/gene/",
];

const MAGIC: &[u8; 8] = b"ECOD0001";
const NO_PREFIX: u16 = 0xFFFF;

/// String ↔ u32 ID dictionary with interior mutability.
///
/// `RwLock` lets `encode` be called via `&self` from multiple threads, which
/// is required at query time: the executor only holds an immutable reference
/// to the store's dict so that computed values such as `STR(IRI)`, `CONCAT(…)`,
/// `UCASE(…)` can be inserted on-the-fly and returned as proper literal TermIds.
///
/// The dictionary is never persisted after the load phase, so query-time
/// additions are ephemeral (they disappear when the store is closed).
pub struct Dictionary {
    // Forward: ID → string
    id_to_str: RwLock<Vec<Box<str>>>,
    // Reverse: string → ID
    str_to_id: RwLock<FxHashMap<String, u32>>,
    // Prefix table for namespace compression
    prefixes: Vec<String>,
}

impl Dictionary {
    pub fn new() -> Self {
        Self {
            id_to_str: RwLock::new(Vec::new()),
            str_to_id: RwLock::new(FxHashMap::default()),
            prefixes: KNOWN_PREFIXES.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Get or create an integer ID for the given string.
    ///
    /// Uses a write lock, so concurrent calls serialise correctly.
    /// Fast path: read lock only when entry already exists.
    pub fn encode(&self, s: &str) -> u32 {
        // Fast path: read lock (no contention with other readers)
        if let Some(&id) = self.str_to_id.read().unwrap().get(s) {
            return id;
        }
        // Slow path: write lock to insert new entry
        let mut id_to_str = self.id_to_str.write().unwrap();
        let mut str_to_id = self.str_to_id.write().unwrap();
        // Re-check after acquiring write lock (another thread may have beaten us)
        if let Some(&id) = str_to_id.get(s) {
            return id;
        }
        let id = id_to_str.len() as u32;
        id_to_str.push(s.into());
        str_to_id.insert(s.to_string(), id);
        id
    }

    /// Decode an integer ID to its string. Panics on out-of-range (bug, not user error).
    #[inline]
    pub fn decode(&self, id: u32) -> String {
        self.id_to_str.read().unwrap()[id as usize].to_string()
    }

    /// Lookup an ID without inserting. Returns None if not found.
    #[inline]
    pub fn lookup(&self, s: &str) -> Option<u32> {
        self.str_to_id.read().unwrap().get(s).copied()
    }

    /// Total number of distinct terms.
    #[inline]
    pub fn len(&self) -> usize {
        self.id_to_str.read().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.id_to_str.read().unwrap().is_empty()
    }

    // ── Persistence ──────────────────────────────────────────────────────────

    /// Save to binary file.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let file = File::create(path)?;
        let mut w = BufWriter::new(file);

        // Magic header
        w.write_all(MAGIC)?;

        // Prefix table
        let pc = self.prefixes.len() as u32;
        w.write_all(&pc.to_le_bytes())?;
        for p in &self.prefixes {
            let b = p.as_bytes();
            w.write_all(&(b.len() as u16).to_le_bytes())?;
            w.write_all(b)?;
        }

        // Terms
        let id_to_str = self.id_to_str.read().unwrap();
        let tc = id_to_str.len() as u32;
        w.write_all(&tc.to_le_bytes())?;

        for term in id_to_str.iter() {
            // Try to find a matching prefix
            let (prefix_id, local) = self.find_prefix(term);
            if let Some(pid) = prefix_id {
                w.write_all(&(pid as u16).to_le_bytes())?;
                let lb = local.as_bytes();
                w.write_all(&(lb.len() as u32).to_le_bytes())?;
                w.write_all(lb)?;
            } else {
                w.write_all(&NO_PREFIX.to_le_bytes())?;
                let tb = term.as_bytes();
                w.write_all(&(tb.len() as u32).to_le_bytes())?;
                w.write_all(tb)?;
            }
        }

        w.flush()
    }

    /// Load from binary file.
    pub fn load(path: &Path) -> io::Result<Self> {
        let mut file = File::open(path)?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;

        let mut pos = 0;

        // Magic check
        if &buf[pos..pos + 8] != MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid dict magic"));
        }
        pos += 8;

        // Prefix table
        let pc = u32::from_le_bytes(buf[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        let mut prefixes = Vec::with_capacity(pc);
        for _ in 0..pc {
            let len = u16::from_le_bytes(buf[pos..pos + 2].try_into().unwrap()) as usize;
            pos += 2;
            prefixes.push(String::from_utf8_lossy(&buf[pos..pos + len]).into_owned());
            pos += len;
        }

        // Terms
        let tc = u32::from_le_bytes(buf[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        let mut id_to_str: Vec<Box<str>> = Vec::with_capacity(tc);
        let mut str_to_id: FxHashMap<String, u32> = FxHashMap::default();
        str_to_id.reserve(tc);

        for id in 0..tc {
            let pid = u16::from_le_bytes(buf[pos..pos + 2].try_into().unwrap());
            pos += 2;
            let len = u32::from_le_bytes(buf[pos..pos + 4].try_into().unwrap()) as usize;
            pos += 4;
            let local = std::str::from_utf8(&buf[pos..pos + len])
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid UTF-8"))?;
            pos += len;

            let full = if pid == NO_PREFIX {
                local.to_string()
            } else {
                format!("{}{}", prefixes[pid as usize], local)
            };

            str_to_id.insert(full.clone(), id as u32);
            id_to_str.push(full.into_boxed_str());
        }

        Ok(Self {
            id_to_str: RwLock::new(id_to_str),
            str_to_id: RwLock::new(str_to_id),
            prefixes,
        })
    }

    /// Consume this `Dictionary` and return its internal storage.
    ///
    /// Used to convert an in-memory dictionary built during a one-pass load
    /// into a [`QueryDict::Legacy`] without copying all strings.
    pub fn into_parts(self) -> (Vec<Box<str>>, FxHashMap<String, u32>) {
        (
            self.id_to_str.into_inner().unwrap(),
            self.str_to_id.into_inner().unwrap(),
        )
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn find_prefix<'a>(&self, term: &'a str) -> (Option<usize>, &'a str) {
        for (i, p) in self.prefixes.iter().enumerate() {
            if term.starts_with(p.as_str()) {
                return (Some(i), &term[p.len()..]);
            }
        }
        (None, term)
    }

    /// Pretty-print a term for human display.
    pub fn display(&self, id: u32) -> String {
        let s = self.decode(id);
        // If it looks like a URI, wrap in angle brackets
        if s.starts_with("http://") || s.starts_with("https://") {
            format!("<{}>", s)
        } else if s.starts_with('"') {
            // Already formatted as literal — return as-is
            s
        } else {
            s
        }
    }
}

impl Default for Dictionary {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn roundtrip() {
        let d = Dictionary::new();
        let id1 = d.encode("http://purl.uniprot.org/uniprot/P12345");
        let id2 = d.encode("http://www.w3.org/1999/02/22-rdf-syntax-ns#type");
        let id3 = d.encode("\"Homo sapiens\"@en");
        assert_eq!(d.decode(id1), "http://purl.uniprot.org/uniprot/P12345");
        assert_eq!(d.decode(id2), "http://www.w3.org/1999/02/22-rdf-syntax-ns#type");
        assert_eq!(d.decode(id3), "\"Homo sapiens\"@en");
        assert_eq!(d.lookup("http://purl.uniprot.org/uniprot/P12345"), Some(id1));
    }

    #[test]
    fn save_load() {
        let d = Dictionary::new();
        d.encode("http://purl.uniprot.org/uniprot/A0A000");
        d.encode("http://www.w3.org/2000/01/rdf-schema#label");
        d.encode("\"test\"^^<http://www.w3.org/2001/XMLSchema#string>");

        let path = PathBuf::from("/tmp/test_dict.bin");
        d.save(&path).unwrap();

        let d2 = Dictionary::load(&path).unwrap();
        assert_eq!(d2.len(), 3);
        assert_eq!(
            d2.decode(0),
            "http://purl.uniprot.org/uniprot/A0A000"
        );
    }
}
