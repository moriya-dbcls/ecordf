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

use std::collections::HashMap;
use std::io;
use std::sync::Arc;

use axum::{
    extract::{Query as AxumQuery, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::store::{QueryResult, Store};

pub type SharedStore = Arc<Store>;

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
pub fn build_router(store: SharedStore, cors_origins: Option<&str>) -> Router {
    let router = Router::new()
        .route("/sparql", get(handle_get).post(handle_post))
        .route("/", get(handle_root))
        .route("/stats", get(handle_stats))
        .with_state(store);

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

async fn handle_stats(State(store): State<SharedStore>) -> impl IntoResponse {
    let stats = store.stats();
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
    State(store): State<SharedStore>,
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

    execute_query(&store, &query, &format)
}

async fn handle_post(
    State(store): State<SharedStore>,
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
    execute_query(&store, &query, &format)
}

fn execute_query(store: &Store, query: &str, format: &str) -> Response {
    match store.query(query) {
        Ok(result) => match &result {
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
                format_select(rs, store, format)
            }
            QueryResult::Ask(b) => format_ask(*b, format),
        },
        Err(e) => error_response(StatusCode::BAD_REQUEST, &e.to_string()),
    }
}

// ── Formatters ────────────────────────────────────────────────────────────────

fn format_select(rs: &crate::sparql::ResultSet, store: &Store, format: &str) -> Response {
    match format.to_ascii_lowercase().as_str() {
        "xml" | "application/sparql-results+xml" => {
            let body = results_to_xml(rs, store);
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/sparql-results+xml; charset=utf-8")],
                body,
            ).into_response()
        }
        "tsv" | "text/tab-separated-values" => {
            let body = results_to_tsv(rs, store);
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/tab-separated-values; charset=utf-8")],
                body,
            ).into_response()
        }
        "csv" | "text/csv" => {
            let body = results_to_csv(rs, store);
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/csv; charset=utf-8")],
                body,
            ).into_response()
        }
        _ => {
            // Default: SPARQL Results JSON
            let body = results_to_json(rs, store);
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/sparql-results+json; charset=utf-8")],
                body.to_string(),
            ).into_response()
        }
    }
}

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

/// SPARQL Results JSON format (W3C standard)
fn results_to_json(rs: &crate::sparql::ResultSet, store: &Store) -> Value {
    let vars: Vec<Value> = rs.variables.iter().map(|v| json!(v)).collect();

    let bindings: Vec<Value> = rs.rows.iter().map(|row| {
        let mut obj = serde_json::Map::new();
        for (i, var) in rs.variables.iter().enumerate() {
            if let Some(Some(id)) = row.get(i) {
                let s = store.dict.decode(*id);
                let term_json = encode_term_json(&s);
                obj.insert(var.clone(), term_json);
            }
        }
        Value::Object(obj)
    }).collect();

    json!({
        "head": { "vars": vars },
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

/// SPARQL Results XML format
fn results_to_xml(rs: &crate::sparql::ResultSet, store: &Store) -> String {
    let mut xml = String::from(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<sparql xmlns="http://www.w3.org/2005/sparql-results#">
  <head>"#
    );
    for var in &rs.variables {
        xml.push_str(&format!("\n    <variable name=\"{}\"/>", xml_escape(var)));
    }
    xml.push_str("\n  </head>\n  <results>");

    for row in &rs.rows {
        xml.push_str("\n    <result>");
        for (i, var) in rs.variables.iter().enumerate() {
            if let Some(Some(id)) = row.get(i) {
                let s = store.dict.decode(*id);
                xml.push_str(&format!("\n      <binding name=\"{}\">", xml_escape(var)));
                xml.push_str(&encode_term_xml(&s));
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

fn results_to_tsv(rs: &crate::sparql::ResultSet, store: &Store) -> String {
    let mut out = String::new();
    // Header
    let header: Vec<String> = rs.variables.iter().map(|v| format!("?{}", v)).collect();
    out.push_str(&header.join("\t"));
    out.push('\n');
    // Rows
    for row in &rs.rows {
        let cells: Vec<String> = (0..rs.variables.len()).map(|i| {
            match row.get(i) {
                Some(Some(id)) => {
                    let s = store.dict.decode(*id);
                    if s.starts_with('"') || s.starts_with("_:") { s }
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

fn results_to_csv(rs: &crate::sparql::ResultSet, store: &Store) -> String {
    let mut out = String::new();
    out.push_str(&rs.variables.join(","));
    out.push('\n');
    for row in &rs.rows {
        let cells: Vec<String> = (0..rs.variables.len()).map(|i| {
            match row.get(i) {
                Some(Some(id)) => {
                    let s = store.dict.decode(*id);
                    csv_escape(&s)
                }
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

/// Start the HTTP server.
///
/// `cors_origins`: passed directly to `build_router` — see its doc for accepted values.
pub async fn serve(store: Store, host: &str, port: u16, cors_origins: Option<&str>) -> io::Result<()> {
    let shared = Arc::new(store);
    let app = build_router(shared, cors_origins);
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
