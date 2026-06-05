//! # HTTP SPARQL 1.1 Protocol Endpoint
//!
//! Implements: https://www.w3.org/TR/sparql11-protocol/
//!
//! Endpoints:
//!   GET  /sparql?query=...&format=json  → SPARQL results JSON
//!   POST /sparql  (application/x-www-form-urlencoded or application/sparql-query)
//!
//! Response formats:
//!   application/sparql-results+json (default)
//!   application/sparql-results+xml
//!   text/tab-separated-values
//!   text/csv

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use axum::{
    extract::{Query as AxumQuery, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Semaphore;
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::store::{QueryResult, Store};

pub type SharedStore = Arc<Store>;

/// Shared server state: the store plus an optional concurrency limiter.
#[derive(Clone)]
pub struct AppState {
    pub store: SharedStore,
    /// `None` = unlimited.  `Some(sem)` = at most N concurrent queries.
    pub semaphore: Option<Arc<Semaphore>>,
}

// ── Request types ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct SparqlGetParams {
    pub query: Option<String>,
    pub format: Option<String>,
    pub timeout: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct SparqlFormParams {
    pub query: Option<String>,
    pub format: Option<String>,
}

// ── Router ────────────────────────────────────────────────────────────────────

/// Build the axum router, optionally attaching a CORS layer.
///
/// `cors_origins`:
///   - `None`  → no CORS headers added (default, safe for local-only use)
///   - `Some("*")` → `Access-Control-Allow-Origin: *`
///   - `Some("https://example.com,https://app.example.com")` → specific origins
pub fn build_router(state: AppState, cors_origins: Option<&str>) -> Router {
    let router = Router::new()
        .route("/sparql", get(handle_get).post(handle_post))
        .route("/", get(handle_root))
        .route("/stats", get(handle_stats))
        .with_state(state);

    match cors_origins {
        None => router,
        Some("*") => router.layer(
            CorsLayer::new()
                .allow_origin(AllowOrigin::any())
                .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
                .allow_headers([
                    header::CONTENT_TYPE,
                    header::ACCEPT,
                    header::AUTHORIZATION,
                ]),
        ),
        Some(origins) => {
            let allow: Vec<HeaderValue> = origins
                .split(',')
                .map(|o| o.trim())
                .filter(|o| !o.is_empty())
                .filter_map(|o| HeaderValue::from_str(o).ok())
                .collect();
            router.layer(
                CorsLayer::new()
                    .allow_origin(AllowOrigin::list(allow))
                    .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
                    .allow_headers([
                        header::CONTENT_TYPE,
                        header::ACCEPT,
                        header::AUTHORIZATION,
                    ]),
            )
        }
    }
}

async fn handle_root() -> impl IntoResponse {
    let html = r#"<!DOCTYPE html>
<html>
<head><title>EcoRDF SPARQL Endpoint</title></head>
<body>
<h1>EcoRDF SPARQL 1.1 Endpoint</h1>
<p>Cost-efficient RDF triple store: low memory (vs Qlever) + fast (vs Virtuoso)</p>
<h2>Usage</h2>
<pre>GET /sparql?query=SELECT+*+WHERE+{+?s+?p+?o+}+LIMIT+10</pre>
<pre>POST /sparql  Content-Type: application/sparql-query</pre>
<h2>Formats</h2>
<ul>
  <li>application/sparql-results+json (default)</li>
  <li>application/sparql-results+xml</li>
  <li>text/tab-separated-values</li>
  <li>text/csv</li>
</ul>
<h2>Test</h2>
<form method="GET" action="/sparql">
  <textarea name="query" rows="5" cols="60">SELECT ?s ?p ?o WHERE { ?s ?p ?o } LIMIT 10</textarea><br>
  <input type="submit" value="Run Query">
</form>
</body></html>"#;
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        html,
    )
}

async fn handle_stats(State(state): State<AppState>) -> impl IntoResponse {
    let stats = state.store.stats();
    let body = json!({
        "triples": stats.triple_count,
        "terms": stats.term_count,
        "directory": stats.dir.display().to_string(),
    });
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
}

async fn handle_get(
    State(state): State<AppState>,
    AxumQuery(params): AxumQuery<SparqlGetParams>,
    headers: HeaderMap,
) -> Response {
    let query = match params.query {
        Some(q) => q,
        None => return error_response(StatusCode::BAD_REQUEST, "missing 'query' parameter"),
    };
    let format = params.format
        .or_else(|| accept_format(&headers))
        .unwrap_or_else(|| "json".to_string());

    run_query(state, query, format).await
}

async fn handle_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let (query, format) = if content_type.contains("application/sparql-query") {
        // Raw SPARQL in body
        let q = String::from_utf8_lossy(&body).to_string();
        let fmt = accept_format(&headers).unwrap_or_else(|| "json".to_string());
        (q, fmt)
    } else {
        // URL-encoded form
        let params: HashMap<String, String> = form_urlencoded::parse(body.as_ref())
            .into_owned()
            .collect();
        let q = params.get("query").cloned().unwrap_or_default();
        let fmt = params.get("format").cloned()
            .or_else(|| accept_format(&headers))
            .unwrap_or_else(|| "json".to_string());
        (q, fmt)
    };

    if query.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "missing query");
    }
    run_query(state, query, format).await
}

/// Offload the synchronous, CPU-bound query execution to tokio's blocking
/// thread pool so that async worker threads remain free for I/O.
///
/// `store.query()` is entirely synchronous.  Calling it directly from an
/// async handler would block a tokio worker thread for the full duration of
/// the query, limiting concurrency to the number of CPU cores and starving
/// the async runtime.  `spawn_blocking` moves the work to a dedicated pool
/// (default: up to 512 threads), allowing many queries to run in parallel.
///
/// If `state.semaphore` is set, at most N queries are admitted simultaneously;
/// excess requests wait until a slot is released.
///
/// If `store.config.server.query_timeout_secs > 0`, a cancellation flag is
/// set after the timeout and the executor's inner loops abort at the next
/// checkpoint.  The HTTP response is 408 with a JSON error body.
async fn run_query(state: AppState, query: String, format: String) -> Response {
    // Acquire a concurrency slot if a limit is configured.
    let _permit = if let Some(ref sem) = state.semaphore {
        match sem.clone().acquire_owned().await {
            Ok(p) => Some(p),
            Err(_) => return error_response(StatusCode::SERVICE_UNAVAILABLE, "server shutting down"),
        }
    } else {
        None
    };

    let timeout_secs = state.store.config.server.query_timeout_secs;
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_for_task = Arc::clone(&cancel);

    let store = Arc::clone(&state.store);
    let cancel_for_exec = Arc::clone(&cancel);
    let task = tokio::task::spawn_blocking(move || {
        execute_query_with_cancel(&store, &query, &format, cancel_for_exec)
    });

    if timeout_secs > 0 {
        match tokio::time::timeout(Duration::from_secs(timeout_secs), task).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "query execution panicked"),
            Err(_) => {
                // Timeout elapsed: signal the blocking thread to abort.
                cancel_for_task.store(true, Ordering::Relaxed);
                tracing::warn!(timeout_secs, "query timed out");
                error_response(StatusCode::REQUEST_TIMEOUT,
                    &format!("query exceeded {}s timeout", timeout_secs))
            }
        }
    } else {
        match task.await {
            Ok(response) => response,
            Err(_) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "query execution panicked"),
        }
    }
}

/// Execute a query and build the HTTP response, timing the decode/serialise phase
/// separately from index execution (which is timed in `store::query`).
///
/// Adds an `X-EcoRDF-Timing` response header:
///   `X-EcoRDF-Timing: decode_us=NNN; serialize_us=NNN; total_us=NNN; rows=NNN`
///
/// Combined with the `INFO ecordf::store: query` log line you get the full breakdown:
///   - `parse_us`     — SPARQL parser
///   - `execute_us`   — optimizer + index seeks + join (from store log)
///   - `decode_us`    — TermId → string (ReadonlyDict lookups)
///   - `serialize_us` — JSON / XML / TSV formatting
///   - `total_us`     — decode + serialize combined (this function)
fn execute_query_with_cancel(
    store: &Store,
    query: &str,
    format: &str,
    cancel: Arc<AtomicBool>,
) -> Response {
    let t_req = Instant::now();
    let hints = detect_bnode_hints(query);

    let result = match store.query_with_cancel(query, cancel) {
        Ok(r)  => r,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, &e.to_string()),
    };

    build_query_response(store, result, format, t_req, &hints)
}

fn execute_query(store: &Store, query: &str, format: &str) -> Response {
    let t_req = Instant::now();
    let hints = detect_bnode_hints(query);

    let result = match store.query(query) {
        Ok(r)  => r,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, &e.to_string()),
    };

    build_query_response(store, result, format, t_req, &hints)
}

fn build_query_response(store: &Store, result: QueryResult, format: &str, t_req: Instant, hints: &[String]) -> Response {

    match &result {
        QueryResult::Select(rs) => {
            if rs.overflow {
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &format!(
                        "Query result exceeded memory limit ({} rows). \
                         Add LIMIT / tighter FILTER to reduce result size.",
                        rs.rows.len()
                    ),
                );
            }
            let rows = rs.rows.len();

            // ── Decode: TermId → String ───────────────────────────────────────
            // This step touches the ReadonlyDict mmap for every result cell.
            // For large result sets with cold page cache it can dominate total time.
            let t_decode = Instant::now();
            let decoded = decode_result_set(rs, store);
            let decode_us = t_decode.elapsed().as_micros();

            // ── Serialize: build JSON/XML/TSV bytes ───────────────────────────
            let t_ser = Instant::now();
            let mut response = format_decoded(&decoded, rs, format, hints);
            let serialize_us = t_ser.elapsed().as_micros();

            let total_us = t_req.elapsed().as_micros();
            tracing::info!(
                decode_us,
                serialize_us,
                total_us,
                rows,
                "decode+serialize"
            );

            // Add timing header so it's visible in curl / browser devtools.
            let timing_val = format!(
                "decode_us={}; serialize_us={}; total_us={}; rows={}",
                decode_us, serialize_us, total_us, rows
            );
            if let Ok(v) = header::HeaderValue::from_str(&timing_val) {
                response.headers_mut().insert("x-ecordf-timing", v);
            }
            response
        }
        QueryResult::Ask(b) => format_ask(*b, format),
        QueryResult::Describe(rs) => {
            let t_decode = Instant::now();
            let decoded = decode_result_set(rs, store);
            let decode_us = t_decode.elapsed().as_micros();
            let t_ser = Instant::now();
            let body = describe_to_ntriples(&decoded);
            let serialize_us = t_ser.elapsed().as_micros();
            let total_us = t_req.elapsed().as_micros();
            tracing::info!(decode_us, serialize_us, total_us, rows = rs.rows.len(), "describe decode+serialize");
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/n-triples; charset=utf-8")],
                body,
            ).into_response()
        }
    }
}

/// Pre-decode all TermIds in a ResultSet to strings.
///
/// Separating this from serialisation lets us time just the dict-lookup cost.
struct DecodedRow(Vec<Option<String>>);

fn decode_result_set(rs: &crate::sparql::ResultSet, store: &Store) -> Vec<DecodedRow> {
    rs.rows.iter().map(|row| {
        // .copied() converts &Option<TermId> → Option<TermId> (TermId = u64 is Copy)
        DecodedRow(row.iter().copied().map(|cell| {
            cell.map(|id| store.dict.decode(id))
        }).collect())
    }).collect()
}

/// Build the HTTP response body from pre-decoded strings.
fn format_decoded(
    decoded: &[DecodedRow],
    rs: &crate::sparql::ResultSet,
    format: &str,
    hints: &[String],
) -> Response {
    match format.to_ascii_lowercase().as_str() {
        "xml" | "application/sparql-results+xml" => {
            let body = decoded_to_xml(decoded, rs);
            let mut response = (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/sparql-results+xml; charset=utf-8")],
                body,
            ).into_response();
            if !hints.is_empty() {
                let hint_str = hints.join("; ");
                if let Ok(v) = header::HeaderValue::from_str(&hint_str) {
                    response.headers_mut().insert("x-ecordf-hints", v);
                }
            }
            response
        }
        "tsv" | "text/tab-separated-values" => {
            let body = decoded_to_tsv(decoded, rs);
            let mut response = (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/tab-separated-values; charset=utf-8")],
                body,
            ).into_response();
            if !hints.is_empty() {
                let hint_str = hints.join("; ");
                if let Ok(v) = header::HeaderValue::from_str(&hint_str) {
                    response.headers_mut().insert("x-ecordf-hints", v);
                }
            }
            response
        }
        "csv" | "text/csv" => {
            let body = decoded_to_csv(decoded, rs);
            let mut response = (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/csv; charset=utf-8")],
                body,
            ).into_response();
            if !hints.is_empty() {
                let hint_str = hints.join("; ");
                if let Ok(v) = header::HeaderValue::from_str(&hint_str) {
                    response.headers_mut().insert("x-ecordf-hints", v);
                }
            }
            response
        }
        _ => {
            let body = decoded_to_json(decoded, rs, hints);
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/sparql-results+json; charset=utf-8")],
                body.to_string(),
            ).into_response()
        }
    }
}

// ── Formatters ────────────────────────────────────────────────────────────────
//
// All formatters now work from pre-decoded `DecodedRow` values so that the
// decode (TermId→String) and serialise phases can be timed independently.

fn format_ask(result: bool, format: &str) -> Response {
    let body = match format.to_ascii_lowercase().as_str() {
        "xml" => format!(
            r#"<?xml version="1.0"?><sparql xmlns="http://www.w3.org/2005/sparql-results#"><boolean>{}</boolean></sparql>"#,
            result
        ),
        _ => json!({
            "head": {},
            "boolean": result
        }).to_string(),
    };
    let ct = if format.contains("xml") {
        "application/sparql-results+xml"
    } else {
        "application/sparql-results+json"
    };
    (StatusCode::OK, [(header::CONTENT_TYPE, ct)], body).into_response()
}

/// SPARQL Results JSON format (W3C standard) — uses pre-decoded strings.
fn decoded_to_json(decoded: &[DecodedRow], rs: &crate::sparql::ResultSet, hints: &[String]) -> Value {
    let vars: Vec<Value> = rs.variables.iter().map(|v| json!(v)).collect();

    let bindings: Vec<Value> = decoded.iter().map(|row| {
        let mut obj = serde_json::Map::new();
        for (i, var) in rs.variables.iter().enumerate() {
            if let Some(Some(s)) = row.0.get(i) {
                obj.insert(var.clone(), encode_term_json(s));
            }
        }
        Value::Object(obj)
    }).collect();

    let mut head = serde_json::Map::new();
    head.insert("vars".into(), json!(vars));
    if !hints.is_empty() {
        head.insert("hints".into(), json!(hints));
    }
    json!({
        "head": head,
        "results": { "bindings": bindings }
    })
}

fn encode_term_json(s: &str) -> Value {
    if s.starts_with('"') {
        // Literal
        let value = extract_literal_value(s);
        let mut obj = serde_json::Map::new();
        obj.insert("type".into(), json!("literal"));
        obj.insert("value".into(), json!(value));
        if let Some(lang) = extract_lang(s) {
            obj.insert("xml:lang".into(), json!(lang));
        } else if let Some(dt) = extract_datatype(s) {
            obj.insert("datatype".into(), json!(dt));
        }
        Value::Object(obj)
    } else if s.starts_with("_:") {
        json!({ "type": "bnode", "value": &s[2..] })
    } else {
        json!({ "type": "uri", "value": s })
    }
}

/// SPARQL Results XML format — uses pre-decoded strings.
fn decoded_to_xml(decoded: &[DecodedRow], rs: &crate::sparql::ResultSet) -> String {
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <sparql xmlns=\"http://www.w3.org/2005/sparql-results#\">\n  <head>"
    );
    for var in &rs.variables {
        xml.push_str(&format!("\n    <variable name=\"{}\"/>", xml_escape(var)));
    }
    xml.push_str("\n  </head>\n  <results>");
    for row in decoded {
        xml.push_str("\n    <result>");
        for (i, var) in rs.variables.iter().enumerate() {
            if let Some(Some(s)) = row.0.get(i) {
                xml.push_str(&format!("\n      <binding name=\"{}\">", xml_escape(var)));
                xml.push_str(&encode_term_xml(s));
                xml.push_str("</binding>");
            }
        }
        xml.push_str("\n    </result>");
    }
    xml.push_str("\n  </results>\n</sparql>");
    xml
}

fn encode_term_xml(s: &str) -> String {
    if s.starts_with('"') {
        let value = extract_literal_value(s);
        if let Some(lang) = extract_lang(s) {
            format!("<literal xml:lang=\"{}\">{}</literal>", lang, xml_escape(&value))
        } else if let Some(dt) = extract_datatype(s) {
            format!("<literal datatype=\"{}\">{}</literal>", dt, xml_escape(&value))
        } else {
            format!("<literal>{}</literal>", xml_escape(&value))
        }
    } else if s.starts_with("_:") {
        format!("<bnode>{}</bnode>", xml_escape(&s[2..]))
    } else {
        format!("<uri>{}</uri>", xml_escape(s))
    }
}

/// Format DESCRIBE result rows (s, p, o decoded strings) as N-Triples.
/// dict.decode returns IRIs without <>, literals with quotes, bnodes with _:.
fn describe_to_ntriples(decoded: &[DecodedRow]) -> String {
    let mut out = String::new();
    for row in decoded {
        let s = row.0.get(0).and_then(|v| v.as_deref());
        let p = row.0.get(1).and_then(|v| v.as_deref());
        let o = row.0.get(2).and_then(|v| v.as_deref());
        if let (Some(s), Some(p), Some(o)) = (s, p, o) {
            out.push_str(&ntriples_term(s));
            out.push(' ');
            out.push_str(&ntriples_term(p));
            out.push(' ');
            out.push_str(&ntriples_term(o));
            out.push_str(" .\n");
        }
    }
    out
}

/// Wrap a decoded term string in N-Triples syntax.
/// IRIs get <>, literals and bnodes are kept as-is.
fn ntriples_term(s: &str) -> String {
    if s.starts_with('"') || s.starts_with("_:") {
        s.to_string()
    } else {
        format!("<{}>", s)
    }
}

fn decoded_to_tsv(decoded: &[DecodedRow], rs: &crate::sparql::ResultSet) -> String {
    let mut out = String::new();
    let hdr: Vec<String> = rs.variables.iter().map(|v| format!("?{}", v)).collect();
    out.push_str(&hdr.join("\t"));
    out.push('\n');
    for row in decoded {
        let cells: Vec<String> = (0..rs.variables.len()).map(|i| {
            match row.0.get(i) {
                Some(Some(s)) => {
                    if s.starts_with('"') || s.starts_with("_:") { s.clone() }
                    else { format!("<{}>", s) }
                }
                _ => String::new(),
            }
        }).collect();
        out.push_str(&cells.join("\t"));
        out.push('\n');
    }
    out
}

fn decoded_to_csv(decoded: &[DecodedRow], rs: &crate::sparql::ResultSet) -> String {
    let mut out = String::new();
    out.push_str(&rs.variables.join(","));
    out.push('\n');
    for row in decoded {
        let cells: Vec<String> = (0..rs.variables.len()).map(|i| {
            match row.0.get(i) {
                Some(Some(s)) => csv_escape(s),
                _ => String::new(),
            }
        }).collect();
        out.push_str(&cells.join(","));
        out.push('\n');
    }
    out
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn accept_format(headers: &HeaderMap) -> Option<String> {
    let accept = headers.get(header::ACCEPT)?.to_str().ok()?;
    if accept.contains("application/sparql-results+xml") { return Some("xml".into()); }
    if accept.contains("text/tab-separated-values") { return Some("tsv".into()); }
    if accept.contains("text/csv") { return Some("csv".into()); }
    None
}

fn error_response(status: StatusCode, msg: &str) -> Response {
    (status, [(header::CONTENT_TYPE, "text/plain")], msg.to_string()).into_response()
}

fn extract_literal_value(s: &str) -> String {
    if s.starts_with('"') {
        let inner = &s[1..];
        if let Some(end) = inner.find('"') {
            return inner[..end].to_string();
        }
    }
    s.to_string()
}

fn extract_lang(s: &str) -> Option<&str> {
    let at = s.rfind('@')?;
    if at > 0 && s.as_bytes()[at - 1] == b'"' {
        Some(&s[at + 1..])
    } else { None }
}

fn extract_datatype(s: &str) -> Option<&str> {
    let start = s.find("^^<")?;
    let rest = &s[start + 3..];
    let end = rest.find('>')?;
    Some(&rest[..end])
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
     .replace('<', "&lt;")
     .replace('>', "&gt;")
     .replace('"', "&quot;")
}

fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn collect_role_vars<'a>(
    pat: &'a crate::sparql::ast::GraphPattern,
    subj: &mut HashSet<&'a str>,
    obj:  &mut HashSet<&'a str>,
) {
    use crate::sparql::ast::GraphPattern;
    match pat {
        GraphPattern::Bgp(triples) => {
            for t in triples {
                if let crate::sparql::ast::Term::Variable(v) = &t.s { subj.insert(v.as_str()); }
                if let crate::sparql::ast::Term::Variable(v) = &t.o { obj.insert(v.as_str()); }
            }
        }
        GraphPattern::PathPattern { s, o, .. } => {
            if let crate::sparql::ast::Term::Variable(v) = s { subj.insert(v.as_str()); }
            if let crate::sparql::ast::Term::Variable(v) = o { obj.insert(v.as_str()); }
        }
        GraphPattern::Join(a, b) | GraphPattern::Union(a, b) => {
            collect_role_vars(a, subj, obj);
            collect_role_vars(b, subj, obj);
        }
        GraphPattern::Optional(main, opt) => {
            collect_role_vars(main, subj, obj);
            collect_role_vars(opt, subj, obj);
        }
        GraphPattern::Filter(inner, _)
        | GraphPattern::Extend(inner, _, _)
        | GraphPattern::Graph(_, inner) => collect_role_vars(inner, subj, obj),
        GraphPattern::Subquery(_) | GraphPattern::Values(_) | GraphPattern::Empty => {}
    }
}

/// Detect intermediate "join node" variables that could be rewritten as
/// `[]` blank node property lists.
///
/// A variable is a candidate if ALL of these hold:
///   1. It appears as the *object* of at least one triple pattern.
///   2. It appears as the *subject* of at least one triple or path pattern.
///   3. It is NOT projected by SELECT, GROUP BY, or ORDER BY.
pub fn detect_bnode_hints(query_str: &str) -> Vec<String> {
    use crate::sparql::ast::{QueryForm, Projection, SelectItem, Expression};

    let query = match crate::sparql::parse_query(query_str) {
        Ok(q) => q,
        _ => return vec![],
    };
    let sq = match query.form {
        QueryForm::Select(sq) => sq,
        _ => return vec![],
    };

    // Collect projected / visible variables
    let mut projected: HashSet<String> = HashSet::new();
    match &sq.projection {
        Projection::Wildcard => return vec![],
        Projection::Variables(items) => {
            for item in items {
                match item {
                    SelectItem::Variable(v) => { projected.insert(v.clone()); }
                    SelectItem::Alias(_, name) => { projected.insert(name.clone()); }
                }
            }
        }
    }
    for gc in &sq.group_by {
        if let Expression::Variable(v) = &gc.expr { projected.insert(v.clone()); }
    }
    for oc in &sq.order_by {
        if let Expression::Variable(v) = &oc.expr { projected.insert(v.clone()); }
    }

    // Collect subject / object variable roles
    let mut subj_vars: HashSet<&str> = HashSet::new();
    let mut obj_vars:  HashSet<&str> = HashSet::new();
    collect_role_vars(&sq.pattern, &mut subj_vars, &mut obj_vars);

    // Candidates: appears as both subject and object, not projected
    let mut candidates: Vec<String> = subj_vars
        .iter()
        .filter(|&&v| obj_vars.contains(v) && !projected.contains(v))
        .map(|&v| v.to_string())
        .collect();
    candidates.sort();

    if candidates.is_empty() {
        return vec![];
    }

    let list = candidates.join(", ");
    vec![format!(
        "Intermediate join variable(s) not in SELECT/GROUP BY: {}. \
         Rewriting as [] blank node property lists may allow better join optimization.",
        list
    )]
}

/// Start the HTTP server.
///
/// `cors_origins`: passed directly to `build_router` — see its doc for accepted values.
pub async fn serve(store: Store, host: &str, port: u16, cors_origins: Option<&str>) -> io::Result<()> {
    let max_concurrent = store.config.server.max_concurrent_queries;
    let semaphore = if max_concurrent > 0 {
        eprintln!("Concurrency limit: {} simultaneous queries", max_concurrent);
        Some(Arc::new(Semaphore::new(max_concurrent)))
    } else {
        eprintln!("Concurrency limit: unlimited (bounded by tokio blocking pool, default 512)");
        None
    };
    let state = AppState {
        store: Arc::new(store),
        semaphore,
    };
    let app = build_router(state, cors_origins);
    let addr = format!("{}:{}", host, port);

    if let Some(origins) = cors_origins {
        eprintln!("CORS: Access-Control-Allow-Origin: {}", origins);
    } else {
        eprintln!("CORS: disabled (use --cors '*' to enable)");
    }
    eprintln!("EcoRDF SPARQL endpoint: http://{}/sparql", addr);

    let listener = tokio::net::TcpListener::bind(&addr).await
        .map_err(|e| io::Error::new(io::ErrorKind::AddrInUse, e))?;
    axum::serve(listener, app).await
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
}
