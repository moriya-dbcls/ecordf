//! # rdf-config integration
//!
//! Reads `prefix.yaml` and `model.yaml` from one or more rdf-config directories
//! (local paths or GitHub repository URLs) and extracts **compound property paths**
//! — sequences of predicates that traverse blank nodes — for use by the
//! [`crate::path_cache`] materialiser.
//!
//! ## Accepted spec formats
//!
//! | Format | Example |
//! |--------|---------|
//! | Local directory | `/data/rdf-config/uniprot` |
//! | GitHub tree URL | `https://github.com/dbcls/rdf-config/tree/master/config/uniprot` |
//! | GitHub blob URL | `https://github.com/dbcls/rdf-config/blob/master/config/uniprot` |
//! | Raw GitHub URL  | `https://raw.githubusercontent.com/…/uniprot` |
//!
//! GitHub URLs are translated to `raw.githubusercontent.com` for direct file fetch.
//!
//! ## Extracted paths
//!
//! A **compound path** is a `Vec<String>` of fully-qualified IRIs (angle-bracket
//! notation, e.g. `<http://biohackathon.org/resource/faldo#begin>`).  Paths of
//! length ≥ 2 that go through blank nodes in the model are returned.
//!
//! Example: in UniProt's model.yaml the predicate chain
//! `up:annotation → up:range → faldo:begin → faldo:position` is emitted as
//! `["<http://purl.uniprot.org/core/annotation>",
//!   "<http://purl.uniprot.org/core/range>",
//!   "<http://biohackathon.org/resource/faldo#begin>",
//!   "<http://biohackathon.org/resource/faldo#position>"]`.
//!
//! The PathCache materialiser then evaluates these paths eagerly at startup so
//! SPARQL property-path evaluation can hit RAM instead of doing HDD scans.

use std::collections::HashMap;

// ── Types ─────────────────────────────────────────────────────────────────────

/// A fully-resolved compound path: each element is a `<IRI>` string.
pub type CompoundPath = Vec<String>;

/// Prefix map: `prefix_name` → `expansion_IRI`.
/// Both with and without trailing `#` / `/` are normalised during lookup.
type PrefixMap = HashMap<String, String>;

// ── Public API ────────────────────────────────────────────────────────────────

/// Load all rdf-config specs in `specs` and return the union of all compound
/// paths found in their `model.yaml` files.
///
/// `specs` is a list of strings, each of which is either a local directory path
/// or a GitHub URL (tree or blob format) pointing to a directory that contains
/// `prefix.yaml` and `model.yaml`.
///
/// Errors are logged at `WARN` level and skipped; the function returns whatever
/// paths were successfully extracted.
pub fn load_compound_paths(specs: &[String]) -> Vec<CompoundPath> {
    let mut all: Vec<CompoundPath> = Vec::new();
    for spec in specs {
        match load_one(spec) {
            Ok(mut paths) => {
                tracing::info!(
                    spec = %spec,
                    compound_paths = paths.len(),
                    "rdf-config: loaded compound paths"
                );
                all.append(&mut paths);
            }
            Err(e) => {
                tracing::warn!(spec = %spec, error = %e, "rdf-config: failed to load, skipping");
            }
        }
    }
    // Deduplicate (same path may appear in multiple models)
    all.sort();
    all.dedup();
    all
}

// ── Per-spec loader ───────────────────────────────────────────────────────────

fn load_one(spec: &str) -> Result<Vec<CompoundPath>, String> {
    let (prefix_text, model_text) = fetch_yaml_pair(spec)?;

    let prefixes = parse_prefix_yaml(&prefix_text)
        .map_err(|e| format!("prefix.yaml parse error: {}", e))?;

    let paths = extract_compound_paths(&model_text, &prefixes)
        .map_err(|e| format!("model.yaml parse error: {}", e))?;

    Ok(paths)
}

// ── YAML fetching ─────────────────────────────────────────────────────────────

/// Fetch `prefix.yaml` and `model.yaml` from a spec (local dir or GitHub URL).
fn fetch_yaml_pair(spec: &str) -> Result<(String, String), String> {
    if is_github_url(spec) {
        fetch_from_github(spec)
    } else {
        fetch_from_local(spec)
    }
}

fn is_github_url(spec: &str) -> bool {
    spec.starts_with("https://github.com/") || spec.starts_with("https://raw.githubusercontent.com/")
}

/// Convert a GitHub tree/blob URL to a raw base URL, then fetch both YAML files.
///
/// Conversions handled:
/// - `https://github.com/<owner>/<repo>/tree/<branch>/config/X`
///   → `https://raw.githubusercontent.com/<owner>/<repo>/<branch>/config/X`
/// - `https://github.com/<owner>/<repo>/blob/<branch>/config/X`
///   → same substitution
/// - `https://raw.githubusercontent.com/…/X` → used as-is
fn fetch_from_github(spec: &str) -> Result<(String, String), String> {
    let raw_base = to_raw_github_base(spec)?;

    let prefix_url = format!("{}/prefix.yaml", raw_base.trim_end_matches('/'));
    let model_url  = format!("{}/model.yaml",  raw_base.trim_end_matches('/'));

    tracing::debug!(prefix_url = %prefix_url, model_url = %model_url, "rdf-config: fetching from GitHub");

    let prefix_text = http_get(&prefix_url)?;
    let model_text  = http_get(&model_url)?;
    Ok((prefix_text, model_text))
}

fn to_raw_github_base(url: &str) -> Result<String, String> {
    if url.starts_with("https://raw.githubusercontent.com/") {
        // Already a raw URL — strip trailing slash
        return Ok(url.trim_end_matches('/').to_string());
    }

    // https://github.com/<owner>/<repo>/tree/<branch>/<path…>
    // https://github.com/<owner>/<repo>/blob/<branch>/<path…>
    let without_scheme = url
        .strip_prefix("https://github.com/")
        .ok_or_else(|| format!("unrecognised GitHub URL: {}", url))?;

    // Tokens: [owner, repo, "tree"|"blob", branch, rest…]
    let parts: Vec<&str> = without_scheme.splitn(5, '/').collect();
    if parts.len() < 4 {
        return Err(format!("GitHub URL too short (expected owner/repo/tree|blob/branch/…): {}", url));
    }
    let owner  = parts[0];
    let repo   = parts[1];
    // parts[2] is "tree" or "blob" — skip
    let branch = parts[3];
    let path   = if parts.len() == 5 { parts[4] } else { "" };

    let raw = if path.is_empty() {
        format!("https://raw.githubusercontent.com/{}/{}/{}", owner, repo, branch)
    } else {
        format!("https://raw.githubusercontent.com/{}/{}/{}/{}", owner, repo, branch, path)
    };
    Ok(raw)
}

fn fetch_from_local(dir: &str) -> Result<(String, String), String> {
    use std::path::Path;
    let base = Path::new(dir);
    let prefix_path = base.join("prefix.yaml");
    let model_path  = base.join("model.yaml");

    let prefix_text = std::fs::read_to_string(&prefix_path)
        .map_err(|e| format!("cannot read {:?}: {}", prefix_path, e))?;
    let model_text  = std::fs::read_to_string(&model_path)
        .map_err(|e| format!("cannot read {:?}: {}", model_path, e))?;
    Ok((prefix_text, model_text))
}

/// Synchronous HTTP GET using `ureq`.
fn http_get(url: &str) -> Result<String, String> {
    ureq::get(url)
        .call()
        .map_err(|e| format!("HTTP GET {}: {}", url, e))?
        .into_string()
        .map_err(|e| format!("HTTP read {}: {}", url, e))
}

// ── prefix.yaml parser ────────────────────────────────────────────────────────

/// Parse rdf-config `prefix.yaml` into a prefix → IRI map.
///
/// Format (YAML mapping):
/// ```yaml
/// rdf: http://www.w3.org/1999/02/22-rdf-syntax-ns#
/// rdfs: http://www.w3.org/2000/01/rdf-schema#
/// up: http://purl.uniprot.org/core/
/// faldo: http://biohackathon.org/resource/faldo#
/// ```
fn parse_prefix_yaml(text: &str) -> Result<PrefixMap, String> {
    let value: serde_yaml::Value = serde_yaml::from_str(text)
        .map_err(|e| format!("YAML error: {}", e))?;

    let mapping = value.as_mapping()
        .ok_or("prefix.yaml: expected a YAML mapping at top level")?;

    let mut map = PrefixMap::new();
    for (k, v) in mapping {
        if let (Some(prefix), Some(iri)) = (k.as_str(), v.as_str()) {
            map.insert(prefix.to_string(), iri.to_string());
        }
    }
    Ok(map)
}

// ── model.yaml parser ─────────────────────────────────────────────────────────

/// Parse rdf-config `model.yaml` and extract all compound property paths.
///
/// The model.yaml format is a YAML mapping where entries describe RDF classes
/// and their properties.  Properties that lead to blank nodes recursively contain
/// further properties.  We extract every root-to-leaf chain of predicates.
///
/// Blank nodes in the YAML are represented as a mapping key whose YAML value is
/// an empty sequence (`[]`).  In `serde_yaml`, such a key is `Value::Sequence(vec![])`.
///
/// Example fragment (UniProt):
/// ```yaml
/// UniProtKB:
///   - up:annotation:
///     - []:            # blank node — predicates continue below
///       - rdf:type: up:Annotation
///       - up:range:
///         - []:        # another blank node
///           - faldo:begin:
///             - []:
///               - faldo:position: xsd:integer
/// ```
///
/// This produces the path `[up:annotation, up:range, faldo:begin, faldo:position]`
/// (after prefix resolution and with `<…>` notation).
fn extract_compound_paths(
    model_text: &str,
    prefixes: &PrefixMap,
) -> Result<Vec<CompoundPath>, String> {
    let value: serde_yaml::Value = serde_yaml::from_str(model_text)
        .map_err(|e| format!("YAML error: {}", e))?;

    let mut result: Vec<CompoundPath> = Vec::new();

    // rdf-config model.yaml comes in two forms:
    //
    //   A) Sequence (the standard rdf-config format):
    //        - ClassName instance_uri:
    //          - predicate: ...
    //      Each list item is a one-key mapping whose value is the property list.
    //
    //   B) Plain mapping (used in unit tests and some hand-written files):
    //        ClassName:
    //          - predicate: ...
    //
    // Handle both so that the parser works with real rdf-config repos as well
    // as our own test fixtures.
    match &value {
        serde_yaml::Value::Mapping(top) => {
            // Form B: top-level mapping, each value is a property list.
            for (_class_name, class_def) in top {
                walk_property_list(class_def, &[], prefixes, &mut result);
            }
        }
        serde_yaml::Value::Sequence(items) => {
            // Form A: top-level sequence, each item is a one-key mapping
            // where the key is "ClassName instance_uri" and the value is
            // the property list.
            for item in items {
                if let Some(mapping) = item.as_mapping() {
                    for (_class_name, class_def) in mapping {
                        walk_property_list(class_def, &[], prefixes, &mut result);
                    }
                }
            }
        }
        _ => {
            // Unknown structure — return empty rather than error.
        }
    }

    // Filter to paths of length ≥ 2 (single predicates aren't interesting for PathCache)
    result.retain(|p| p.len() >= 2);
    result.sort();
    result.dedup();
    Ok(result)
}

/// Recursively walk a property list (YAML sequence), accumulating the current
/// predicate chain in `current_path`.
///
/// The YAML structure at each level is a sequence of mappings.  Each mapping
/// has one key (the predicate or blank-node marker) and one value (either a
/// scalar leaf or another property list).
fn walk_property_list(
    node: &serde_yaml::Value,
    current_path: &[String],
    prefixes: &PrefixMap,
    out: &mut Vec<CompoundPath>,
) {
    let Some(seq) = node.as_sequence() else { return };

    for item in seq {
        walk_property_item(item, current_path, prefixes, out);
    }
}

/// Walk a single property item.
///
/// A property item is a YAML mapping with exactly one key-value pair:
/// - key:   predicate IRI (possibly prefixed) OR blank-node marker (`[]`)
/// - value: scalar leaf (object type) OR nested property list
fn walk_property_item(
    item: &serde_yaml::Value,
    current_path: &[String],
    prefixes: &PrefixMap,
    out: &mut Vec<CompoundPath>,
) {
    let Some(mapping) = item.as_mapping() else { return };

    for (key, val) in mapping {
        // Blank-node marker: key is an empty sequence `[]` in YAML
        // → do NOT extend the path; just recurse into the value
        if is_blank_node_key(key) {
            walk_property_list(val, current_path, prefixes, out);
            continue;
        }

        // Predicate key: resolve prefix and extend the path.
        //
        // rdf-config appends cardinality markers to predicate keys:
        //   jpost:hasPeptide+   → one-or-more
        //   jpost:hasPsm*       → zero-or-more
        //   jpost:hasIsoform?   → zero-or-one
        // Strip these before IRI resolution so the resulting IRI matches
        // what is actually stored in the triple store.
        let Some(pred_str_raw) = key.as_str() else { continue };
        let pred_str = pred_str_raw.trim_end_matches(|c| c == '+' || c == '*' || c == '?');
        let resolved = resolve_iri(pred_str, prefixes);

        let mut new_path: Vec<String> = current_path.to_vec();
        new_path.push(resolved);

        // Emit path so far (even length-1, but we filter ≥ 2 later)
        if !new_path.is_empty() {
            out.push(new_path.clone());
        }

        // Recurse if value is a non-scalar (another property list)
        if val.is_sequence() {
            walk_property_list(val, &new_path, prefixes, out);
        } else if val.is_mapping() {
            // Some model.yamls use a mapping instead of a sequence at this level
            walk_property_item(val, &new_path, prefixes, out);
        }
        // Scalar leaf: nothing further to recurse into
    }
}

/// Return `true` if this YAML key represents a blank node.
///
/// In rdf-config `model.yaml`, blank nodes are written as `[]:` (an empty
/// YAML sequence as a mapping key).  In `serde_yaml` this deserialises to
/// `Value::Sequence(vec![])`.
fn is_blank_node_key(key: &serde_yaml::Value) -> bool {
    matches!(key, serde_yaml::Value::Sequence(v) if v.is_empty())
}

// ── IRI resolution ────────────────────────────────────────────────────────────

/// Resolve a possibly-prefixed string to an angle-bracket IRI.
///
/// - `rdf:type`  → `<http://www.w3.org/1999/02/22-rdf-syntax-ns#type>`
/// - `<http://…>` → unchanged (already a full IRI)
/// - `http://…`  → `<http://…>` (bare IRI without angles)
/// - unknown prefix → returned as-is with `<…>` wrapper
fn resolve_iri(s: &str, prefixes: &PrefixMap) -> String {
    let s = s.trim();
    // Already a full IRI in angle brackets
    if s.starts_with('<') && s.ends_with('>') {
        return s.to_string();
    }
    // Bare absolute IRI
    if s.starts_with("http://") || s.starts_with("https://") {
        return format!("<{}>", s);
    }
    // Prefixed name: split at first `:`
    if let Some(colon) = s.find(':') {
        let prefix = &s[..colon];
        let local  = &s[colon + 1..];
        if let Some(expansion) = prefixes.get(prefix) {
            return format!("<{}{}>", expansion, local);
        }
    }
    // Fallback: wrap whatever we have
    format!("<{}>", s)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_prefixes() -> PrefixMap {
        let mut m = PrefixMap::new();
        m.insert("rdf".into(),   "http://www.w3.org/1999/02/22-rdf-syntax-ns#".into());
        m.insert("rdfs".into(),  "http://www.w3.org/2000/01/rdf-schema#".into());
        m.insert("xsd".into(),   "http://www.w3.org/2001/XMLSchema#".into());
        m.insert("up".into(),    "http://purl.uniprot.org/core/".into());
        m.insert("faldo".into(), "http://biohackathon.org/resource/faldo#".into());
        m
    }

    #[test]
    fn test_resolve_iri_prefixed() {
        let p = make_prefixes();
        assert_eq!(resolve_iri("faldo:begin", &p),
            "<http://biohackathon.org/resource/faldo#begin>");
        assert_eq!(resolve_iri("up:annotation", &p),
            "<http://purl.uniprot.org/core/annotation>");
    }

    #[test]
    fn test_resolve_iri_bare() {
        let p = make_prefixes();
        assert_eq!(resolve_iri("http://example.org/foo", &p),
            "<http://example.org/foo>");
        assert_eq!(resolve_iri("<http://example.org/foo>", &p),
            "<http://example.org/foo>");
    }

    #[test]
    fn test_blank_node_key() {
        let bn: serde_yaml::Value = serde_yaml::from_str("[]").unwrap();
        assert!(is_blank_node_key(&bn));
        let not_bn: serde_yaml::Value = serde_yaml::from_str("\"rdf:type\"").unwrap();
        assert!(!is_blank_node_key(&not_bn));
    }

    #[test]
    fn test_extract_paths_simple() {
        let prefix_yaml = "
faldo: http://biohackathon.org/resource/faldo#
up: http://purl.uniprot.org/core/
xsd: http://www.w3.org/2001/XMLSchema#
";
        // Form B (mapping): MyClass: - up:range: ...
        let model_yaml = "
MyClass:
  - up:range:
    - []:
      - faldo:begin:
        - []:
          - faldo:position: xsd:integer
";
        let prefixes = parse_prefix_yaml(prefix_yaml).unwrap();
        let paths = extract_compound_paths(model_yaml, &prefixes).unwrap();

        let two_hop: Vec<_> = paths.iter().filter(|p| p.len() == 2).collect();
        assert!(
            two_hop.iter().any(|p|
                p[0] == "<http://purl.uniprot.org/core/range>" &&
                p[1] == "<http://biohackathon.org/resource/faldo#begin>"
            ),
            "expected [up:range, faldo:begin] in paths; got: {:?}", paths
        );
    }

    #[test]
    fn test_extract_paths_sequence_format() {
        // Form A (sequence): actual rdf-config format with leading "-"
        // Also tests cardinality suffix stripping (+, *, ?)
        let prefix_yaml = "
faldo: http://biohackathon.org/resource/faldo#
jpost: http://rdf.jpostdb.org/ontology/jpost.owl#
";
        let model_yaml = "
- Protein jpostdb:PRT001:
  - jpost:hasPeptideEvidence+:
    - []:
      - a: jpost:PeptideEvidence
      - faldo:location:
        - []:
          - a: faldo:Region
          - faldo:begin:
            - []:
              - faldo:position: xsd:integer
";
        let prefixes = parse_prefix_yaml(prefix_yaml).unwrap();
        let paths = extract_compound_paths(model_yaml, &prefixes).unwrap();

        // Should find [hasPeptideEvidence, faldo:location], [hasPeptideEvidence, faldo:location, faldo:begin], etc.
        // Cardinality '+' must be stripped from jpost:hasPeptideEvidence+
        assert!(
            !paths.is_empty(),
            "expected compound paths from sequence-format model.yaml, got none"
        );
        // faldo:location should appear in at least one path
        let has_location = paths.iter().any(|p| p.iter().any(|iri| iri.contains("faldo#location")));
        assert!(has_location, "expected faldo:location in some compound path; got: {:?}", paths);
        // faldo:begin should appear (without '+' contamination)
        let has_begin = paths.iter().any(|p| p.iter().any(|iri| iri == "<http://biohackathon.org/resource/faldo#begin>"));
        assert!(has_begin, "expected bare faldo:begin IRI; got: {:?}", paths);
    }

    #[test]
    fn test_cardinality_suffix_stripped() {
        // Predicates with +, *, ? suffixes must produce clean IRIs
        let prefix_yaml = "jpost: http://rdf.jpostdb.org/ontology/jpost.owl#\n";
        let model_yaml = "
- Cls:
  - jpost:hasA+:
    - []:
      - jpost:hasB*:
        - []:
          - jpost:hasC?: value
";
        let prefixes = parse_prefix_yaml(prefix_yaml).unwrap();
        let paths = extract_compound_paths(model_yaml, &prefixes).unwrap();
        let base = "http://rdf.jpostdb.org/ontology/jpost.owl#";
        // All IRIs must be clean (no +, *, ? at end)
        for path in &paths {
            for iri in path {
                assert!(
                    !iri.ends_with("+>") && !iri.ends_with("*>") && !iri.ends_with("?>"),
                    "cardinality suffix leaked into IRI: {}", iri
                );
            }
        }
        // Should still find paths
        let two_hop = paths.iter().filter(|p| p.len() == 2).count();
        assert!(two_hop > 0, "expected length-2 paths; got: {:?}", paths);
        // Check specific clean IRI
        assert!(
            paths.iter().any(|p| p[0] == format!("<{}hasA>", base)),
            "expected clean <jpost:hasA> IRI, got: {:?}", paths
        );
    }

    #[test]
    fn test_github_url_conversion() {
        let url = "https://github.com/dbcls/rdf-config/tree/master/config/uniprot";
        let raw = to_raw_github_base(url).unwrap();
        assert_eq!(raw, "https://raw.githubusercontent.com/dbcls/rdf-config/master/config/uniprot");
    }
}
