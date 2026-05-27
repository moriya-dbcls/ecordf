//! # SPARQL 1.1 Parser (recursive descent)
//!
//! Parses a SPARQL query string into the AST defined in `ast.rs`.
//! No external parsing library required — pure Rust.
//!
//! ## Coverage
//!
//! - SELECT (wildcard, explicit variables, expressions, DISTINCT/REDUCED)
//! - ASK
//! - CONSTRUCT
//! - WHERE clause: BGP, OPTIONAL, UNION, FILTER, BIND, VALUES, subquery
//! - Expressions: arithmetic, comparison, logical, all built-in functions
//! - Aggregates: COUNT, SUM, MIN, MAX, AVG, GROUP_CONCAT, SAMPLE
//! - GROUP BY, HAVING, ORDER BY (ASC/DESC), LIMIT, OFFSET
//! - PREFIX declarations
//! - Literals: plain, typed, language-tagged
//! - Property path syntax (basic: /, |, ?, *, + → converted to triple patterns)

use std::collections::HashMap;

use super::ast::*;

// ══════════════════════════════════════════════════════════════════════════════
// Tokenizer
// ══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, PartialEq)]
enum Token<'a> {
    // Punctuation
    LBrace, RBrace, LParen, RParen, LBracket, RBracket,
    Dot, Semicolon, Comma, Pipe, Slash, Hat, At,
    Star, Plus, Question, Bang,
    // Operators
    Eq, Ne, Lt, Le, Gt, Ge,
    Plus2, Minus, Times, Div,
    And, Or,
    Arrow,     // =>  (unused but reserved)
    // Keywords (case-insensitive)
    Kw(&'a str),
    // Literals
    IriRef(&'a str),   // <...>
    PrefixedName(&'a str, &'a str),  // prefix:local
    BlankNodeLabel(&'a str),  // _:label
    Anon,                     // []
    StringLit(String),        // "..." or '...' (with escape processing)
    IntegerLit(i64),
    DecimalLit(f64),
    DoubleLit(f64),
    // Variable
    Var(&'a str),
    // End of input
    Eof,
}

struct Lexer<'a> {
    input: &'a str,
    pos: usize,
    peeked: Option<(Token<'a>, usize)>,
}

impl<'a> Lexer<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, pos: 0, peeked: None }
    }

    fn remaining(&self) -> &'a str {
        &self.input[self.pos..]
    }

    fn skip_ws_and_comments(&mut self) {
        loop {
            // Whitespace
            let before = self.pos;
            while self.pos < self.input.len()
                && self.input.as_bytes()[self.pos].is_ascii_whitespace()
            {
                self.pos += 1;
            }
            // # comment until EOL
            if self.pos < self.input.len() && self.input.as_bytes()[self.pos] == b'#' {
                while self.pos < self.input.len() && self.input.as_bytes()[self.pos] != b'\n' {
                    self.pos += 1;
                }
            }
            if self.pos == before {
                break;
            }
        }
    }

    fn peek(&mut self) -> &Token<'a> {
        if self.peeked.is_none() {
            let start = self.pos;
            let tok = self.next_token();
            self.peeked = Some((tok, self.pos));
            self.pos = start;
        }
        &self.peeked.as_ref().unwrap().0
    }

    fn next(&mut self) -> Token<'a> {
        if let Some((tok, end)) = self.peeked.take() {
            self.pos = end;
            return tok;
        }
        self.next_token()
    }

    fn next_token(&mut self) -> Token<'a> {
        self.skip_ws_and_comments();
        if self.pos >= self.input.len() {
            return Token::Eof;
        }
        let b = self.input.as_bytes()[self.pos];

        match b {
            b'{' => { self.pos += 1; Token::LBrace }
            b'}' => { self.pos += 1; Token::RBrace }
            b'(' => { self.pos += 1; Token::LParen }
            b')' => { self.pos += 1; Token::RParen }
            b'[' => {
                self.pos += 1;
                self.skip_ws_and_comments();
                if self.pos < self.input.len() && self.input.as_bytes()[self.pos] == b']' {
                    self.pos += 1;
                    Token::Anon
                } else {
                    Token::LBracket
                }
            }
            b']' => { self.pos += 1; Token::RBracket }
            b'.' => { self.pos += 1; Token::Dot }
            b';' => { self.pos += 1; Token::Semicolon }
            b',' => { self.pos += 1; Token::Comma }
            b'|' => {
                self.pos += 1;
                if self.pos < self.input.len() && self.input.as_bytes()[self.pos] == b'|' {
                    self.pos += 1; Token::Or
                } else {
                    Token::Pipe
                }
            }
            b'/' => { self.pos += 1; Token::Slash }
            b'^' => { self.pos += 1; Token::Hat }
            b'@' => { self.pos += 1; Token::At }
            b'*' => { self.pos += 1; Token::Times }
            b'+' => { self.pos += 1; Token::Plus2 }
            b'?' => {
                self.pos += 1;
                let start = self.pos;
                while self.pos < self.input.len()
                    && is_varname_char(self.input.as_bytes()[self.pos])
                {
                    self.pos += 1;
                }
                if self.pos > start {
                    Token::Var(&self.input[start..self.pos])
                } else {
                    Token::Question // bare '?' used in property paths
                }
            }
            b'$' => {
                self.pos += 1;
                let start = self.pos;
                while self.pos < self.input.len()
                    && is_varname_char(self.input.as_bytes()[self.pos])
                {
                    self.pos += 1;
                }
                Token::Var(&self.input[start..self.pos])
            }
            b'!' => {
                self.pos += 1;
                if self.pos < self.input.len() && self.input.as_bytes()[self.pos] == b'=' {
                    self.pos += 1; Token::Ne
                } else {
                    Token::Bang
                }
            }
            b'=' => { self.pos += 1; Token::Eq }
            b'<' => {
                // Check for IRI ref
                if self.input[self.pos..].starts_with('<') {
                    // Could be IRI or <= or <<
                    if self.pos + 1 < self.input.len() {
                        let next = self.input.as_bytes()[self.pos + 1];
                        if next == b'=' {
                            self.pos += 2;
                            return Token::Le;
                        }
                        if next != b' ' && next != b'>' {
                            // IRI ref
                            self.pos += 1; // skip <
                            let start = self.pos;
                            while self.pos < self.input.len() && self.input.as_bytes()[self.pos] != b'>' {
                                self.pos += 1;
                            }
                            let iri = &self.input[start..self.pos];
                            self.pos += 1; // skip >
                            return Token::IriRef(iri);
                        }
                    }
                    self.pos += 1;
                    Token::Lt
                } else {
                    self.pos += 1;
                    Token::Lt
                }
            }
            b'>' => {
                self.pos += 1;
                if self.pos < self.input.len() && self.input.as_bytes()[self.pos] == b'=' {
                    self.pos += 1; Token::Ge
                } else {
                    Token::Gt
                }
            }
            b'&' => {
                self.pos += 1;
                if self.pos < self.input.len() && self.input.as_bytes()[self.pos] == b'&' {
                    self.pos += 1; Token::And
                } else {
                    Token::And // single & treated as &&
                }
            }
            b'-' => { self.pos += 1; Token::Minus }
            b':' => {
                // Bare colon: empty-prefix name — e.g. `: ` in `PREFIX : <iri>`
                // or `:local` in triple patterns when the default prefix is declared.
                self.pos += 1;
                let local_start = self.pos;
                while self.pos < self.input.len() && is_name_char(self.input.as_bytes()[self.pos]) {
                    self.pos += 1;
                }
                Token::PrefixedName("", &self.input[local_start..self.pos])
            }
            b'"' | b'\'' => self.lex_string(),
            b'_' => self.lex_blank_or_kw(),
            b'0'..=b'9' => self.lex_number(),
            _ if b.is_ascii_alphabetic() => self.lex_keyword_or_prefixed(),
            _ => { self.pos += 1; Token::Eof } // skip unknown
        }
    }

    fn lex_string(&mut self) -> Token<'a> {
        let quote = self.input.as_bytes()[self.pos];
        let triple = self.input[self.pos..].starts_with("\"\"\"")
            || self.input[self.pos..].starts_with("'''");

        let (start_skip, end_skip) = if triple { (3, 3) } else { (1, 1) };
        self.pos += start_skip;

        let end_seq = if triple {
            if quote == b'"' { "\"\"\"" } else { "'''" }
        } else {
            if quote == b'"' { "\"" } else { "'" }
        };

        let mut s = String::new();
        while self.pos < self.input.len() {
            if self.input[self.pos..].starts_with(end_seq) {
                self.pos += end_seq.len();
                break;
            }
            if self.input.as_bytes()[self.pos] == b'\\' {
                self.pos += 1;
                if self.pos < self.input.len() {
                    let esc = self.input.as_bytes()[self.pos];
                    s.push(match esc {
                        b'n' => '\n', b't' => '\t', b'r' => '\r',
                        b'\\' => '\\', b'"' => '"', b'\'' => '\'',
                        _ => esc as char,
                    });
                    self.pos += 1;
                }
            } else {
                s.push(self.input[self.pos..].chars().next().unwrap());
                self.pos += self.input[self.pos..].chars().next().unwrap().len_utf8();
            }
        }
        Token::StringLit(s)
    }

    fn lex_number(&mut self) -> Token<'a> {
        let start = self.pos;
        let mut is_float = false;
        while self.pos < self.input.len() && self.input.as_bytes()[self.pos].is_ascii_digit() {
            self.pos += 1;
        }
        if self.pos < self.input.len() && self.input.as_bytes()[self.pos] == b'.' {
            is_float = true;
            self.pos += 1;
            while self.pos < self.input.len() && self.input.as_bytes()[self.pos].is_ascii_digit() {
                self.pos += 1;
            }
        }
        if self.pos < self.input.len() && (self.input.as_bytes()[self.pos] == b'e' || self.input.as_bytes()[self.pos] == b'E') {
            is_float = true;
            self.pos += 1;
            if self.pos < self.input.len() && (self.input.as_bytes()[self.pos] == b'+' || self.input.as_bytes()[self.pos] == b'-') {
                self.pos += 1;
            }
            while self.pos < self.input.len() && self.input.as_bytes()[self.pos].is_ascii_digit() {
                self.pos += 1;
            }
        }
        let num_str = &self.input[start..self.pos];
        if is_float {
            Token::DoubleLit(num_str.parse().unwrap_or(0.0))
        } else {
            Token::IntegerLit(num_str.parse().unwrap_or(0))
        }
    }

    fn lex_blank_or_kw(&mut self) -> Token<'a> {
        if self.input[self.pos..].starts_with("_:") {
            self.pos += 2;
            let start = self.pos;
            while self.pos < self.input.len() && is_name_char(self.input.as_bytes()[self.pos]) {
                self.pos += 1;
            }
            Token::BlankNodeLabel(&self.input[start..self.pos])
        } else {
            self.lex_keyword_or_prefixed()
        }
    }

    fn lex_keyword_or_prefixed(&mut self) -> Token<'a> {
        let start = self.pos;
        while self.pos < self.input.len() && is_name_char(self.input.as_bytes()[self.pos]) {
            self.pos += 1;
        }
        let word = &self.input[start..self.pos];

        // Check for prefixed name (word:local)
        if self.pos < self.input.len() && self.input.as_bytes()[self.pos] == b':' {
            self.pos += 1;
            let local_start = self.pos;
            while self.pos < self.input.len() && is_name_char(self.input.as_bytes()[self.pos]) {
                self.pos += 1;
            }
            return Token::PrefixedName(word, &self.input[local_start..self.pos]);
        }

        // Keyword (case-insensitive stored as-is)
        Token::Kw(word)
    }
}

fn is_name_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.' || b > 127
}

fn is_varname_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b > 127
}

// ══════════════════════════════════════════════════════════════════════════════
// Parser
// ══════════════════════════════════════════════════════════════════════════════

/// Internal helper: result of parsing a predicate position —
/// either a simple term (plain triple) or a property path expression.
enum VerbOrPath {
    Term(Term),
    Path(PropertyPath),
}

pub struct ParseError(pub String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SPARQL parse error: {}", self.0)
    }
}

type ParseResult<T> = Result<T, ParseError>;

pub struct Parser<'a> {
    lexer: Lexer<'a>,
    prefixes: HashMap<String, String>,
    blank_counter: u32,
}

impl<'a> Parser<'a> {
    pub fn new(input: &'a str) -> Self {
        let mut prefixes = HashMap::new();
        // Default prefixes
        prefixes.insert("rdf".into(), "http://www.w3.org/1999/02/22-rdf-syntax-ns#".into());
        prefixes.insert("rdfs".into(), "http://www.w3.org/2000/01/rdf-schema#".into());
        prefixes.insert("owl".into(), "http://www.w3.org/2002/07/owl#".into());
        prefixes.insert("xsd".into(), "http://www.w3.org/2001/XMLSchema#".into());
        Self { lexer: Lexer::new(input), prefixes, blank_counter: 0 }
    }

    pub fn parse(mut self) -> ParseResult<Query> {
        // Prologue: BASE and PREFIX declarations
        loop {
            match self.lexer.peek() {
                Token::Kw(k) if k.eq_ignore_ascii_case("prefix") => {
                    self.lexer.next();
                    self.parse_prefix_decl()?;
                }
                Token::Kw(k) if k.eq_ignore_ascii_case("base") => {
                    self.lexer.next();
                    // BASE <iri> — we ignore BASE for now
                    self.expect_iri_ref()?;
                }
                _ => break,
            }
        }

        let prefixes = self.prefixes.clone();
        let form = self.parse_query_form()?;
        Ok(Query { prefixes, form })
    }

    fn parse_prefix_decl(&mut self) -> ParseResult<()> {
        // Recognise the prefix name before the IRI.
        //
        // Common tokenisations of "PREFIX foo: <iri>", "PREFIX : <iri>", etc.:
        //   "foo:"   → PrefixedName("foo", "")   — colon already consumed
        //   ":"      → PrefixedName("", "")       — bare colon (empty prefix)
        //   "foo" " " ":" → Kw("foo") + PrefixedName("","") — space before colon
        let prefix = match self.lexer.next() {
            // Colon was already consumed by the lexer as part of the prefixed-name token.
            // This covers "foo:", ":", and even ":local" where local must be empty here.
            Token::PrefixedName(p, local) if local.is_empty() => p.to_string(),
            // The prefix name was tokenised as a keyword (happens when there is a space
            // between the name and the colon, e.g. "PREFIX rdf : <iri>").
            // In that case the very next token is the bare colon PrefixedName("","").
            Token::Kw(k) => {
                let p = k.to_string();
                // Consume the trailing bare colon if present.
                if matches!(self.lexer.peek(), Token::PrefixedName(pn, ln) if pn.is_empty() && ln.is_empty()) {
                    self.lexer.next();
                }
                p
            }
            other => return Err(ParseError(format!("expected prefix name, got {:?}", other))),
        };
        let iri = self.expect_iri_ref()?;
        self.prefixes.insert(prefix, iri);
        Ok(())
    }

    fn expect_iri_ref(&mut self) -> ParseResult<String> {
        match self.lexer.next() {
            Token::IriRef(iri) => Ok(iri.to_string()),
            other => Err(ParseError(format!("expected <IRI>, got {:?}", other))),
        }
    }

    fn parse_query_form(&mut self) -> ParseResult<QueryForm> {
        match self.lexer.peek().clone() {
            Token::Kw(k) if k.eq_ignore_ascii_case("select") => {
                self.lexer.next();
                Ok(QueryForm::Select(self.parse_select()?))
            }
            Token::Kw(k) if k.eq_ignore_ascii_case("ask") => {
                self.lexer.next();
                Ok(QueryForm::Ask(self.parse_ask()?))
            }
            Token::Kw(k) if k.eq_ignore_ascii_case("construct") => {
                self.lexer.next();
                Ok(QueryForm::Construct(self.parse_construct()?))
            }
            other => Err(ParseError(format!("expected SELECT/ASK/CONSTRUCT, got {:?}", other))),
        }
    }

    fn parse_select(&mut self) -> ParseResult<SelectQuery> {
        let distinct = match self.lexer.peek() {
            Token::Kw(k) if k.eq_ignore_ascii_case("distinct") => {
                self.lexer.next(); true
            }
            Token::Kw(k) if k.eq_ignore_ascii_case("reduced") => {
                self.lexer.next(); true // treat REDUCED as DISTINCT
            }
            _ => false,
        };

        let projection = match self.lexer.peek() {
            Token::Times => { self.lexer.next(); Projection::Wildcard }
            _ => {
                let mut items = Vec::new();
                loop {
                    match self.lexer.peek() {
                        Token::Var(_) => {
                            if let Token::Var(v) = self.lexer.next() {
                                items.push(SelectItem::Variable(v.to_string()));
                            }
                        }
                        Token::LParen => {
                            self.lexer.next(); // (
                            let expr = self.parse_expression()?;
                            self.expect_kw("as")?;
                            let name = self.parse_var()?;
                            self.expect(Token::RParen)?;
                            items.push(SelectItem::Alias(expr, name));
                        }
                        _ => break,
                    }
                }
                if items.is_empty() {
                    return Err(ParseError("expected projection variables".into()));
                }
                Projection::Variables(items)
            }
        };

        let dataset = self.parse_dataset_clauses()?;
        self.expect_kw("where")?;
        let pattern = self.parse_group_graph_pattern()?;
        let group_by = self.parse_group_by()?;
        let having = self.parse_having()?;
        let order_by = self.parse_order_by()?;
        let (limit, offset) = self.parse_limit_offset()?;
        let values = self.parse_values_clause()?;

        Ok(SelectQuery {
            distinct,
            projection,
            dataset,
            pattern,
            group_by,
            having,
            order_by,
            limit,
            offset,
            values,
        })
    }

    fn parse_ask(&mut self) -> ParseResult<AskQuery> {
        let dataset = self.parse_dataset_clauses()?;
        self.expect_kw("where")?;
        let pattern = self.parse_group_graph_pattern()?;
        Ok(AskQuery { dataset, pattern })
    }

    fn parse_construct(&mut self) -> ParseResult<ConstructQuery> {
        let template = if matches!(self.lexer.peek(), Token::LBrace) {
            self.lexer.next();
            let t = self.parse_triple_block()?;
            self.expect(Token::RBrace)?;
            t
        } else {
            Vec::new()
        };
        let dataset = self.parse_dataset_clauses()?;
        self.expect_kw("where")?;
        let pattern = self.parse_group_graph_pattern()?;
        let (limit, offset) = self.parse_limit_offset()?;
        Ok(ConstructQuery { template, dataset, pattern, limit, offset })
    }

    fn parse_dataset_clauses(&mut self) -> ParseResult<Vec<DatasetClause>> {
        let mut clauses = Vec::new();
        loop {
            match self.lexer.peek() {
                Token::Kw(k) if k.eq_ignore_ascii_case("from") => {
                    self.lexer.next();
                    let named = if matches!(self.lexer.peek(), Token::Kw(k) if k.eq_ignore_ascii_case("named")) {
                        self.lexer.next(); true
                    } else { false };
                    let iri = self.parse_iri()?;
                    clauses.push(DatasetClause { named, iri });
                }
                _ => break,
            }
        }
        Ok(clauses)
    }

    fn parse_group_graph_pattern(&mut self) -> ParseResult<GraphPattern> {
        self.expect(Token::LBrace)?;
        let pat = self.parse_group_graph_pattern_sub()?;
        self.expect(Token::RBrace)?;
        Ok(pat)
    }

    fn parse_group_graph_pattern_sub(&mut self) -> ParseResult<GraphPattern> {
        let mut patterns: Vec<GraphPattern> = Vec::new();

        loop {
            match self.lexer.peek().clone() {
                Token::RBrace => break,
                Token::Eof => break,

                // OPTIONAL
                Token::Kw(k) if k.eq_ignore_ascii_case("optional") => {
                    self.lexer.next();
                    let opt = self.parse_group_graph_pattern()?;
                    let left = combine_patterns(patterns.drain(..).collect());
                    patterns.push(GraphPattern::Optional(Box::new(left), Box::new(opt)));
                }

                // UNION
                Token::Kw(k) if k.eq_ignore_ascii_case("union") => {
                    self.lexer.next();
                    let right = self.parse_group_graph_pattern()?;
                    let left = combine_patterns(patterns.drain(..).collect());
                    patterns.push(GraphPattern::Union(Box::new(left), Box::new(right)));
                }

                // FILTER
                Token::Kw(k) if k.eq_ignore_ascii_case("filter") => {
                    self.lexer.next();
                    let expr = self.parse_constraint()?;
                    let inner = combine_patterns(patterns.drain(..).collect());
                    patterns.push(GraphPattern::Filter(Box::new(inner), expr));
                }

                // BIND
                Token::Kw(k) if k.eq_ignore_ascii_case("bind") => {
                    self.lexer.next();
                    self.expect(Token::LParen)?;
                    let expr = self.parse_expression()?;
                    self.expect_kw("as")?;
                    let var = self.parse_var()?;
                    self.expect(Token::RParen)?;
                    let inner = combine_patterns(patterns.drain(..).collect());
                    patterns.push(GraphPattern::Extend(Box::new(inner), expr, var));
                }

                // VALUES inline
                Token::Kw(k) if k.eq_ignore_ascii_case("values") => {
                    self.lexer.next();
                    let vc = self.parse_inline_data()?;
                    patterns.push(GraphPattern::Values(vc));
                }

                // GRAPH
                Token::Kw(k) if k.eq_ignore_ascii_case("graph") => {
                    self.lexer.next();
                    let graph = self.parse_var_or_iri()?;
                    let inner = self.parse_group_graph_pattern()?;
                    patterns.push(GraphPattern::Graph(graph, Box::new(inner)));
                }

                // Subquery: SELECT inside {}
                Token::Kw(k) if k.eq_ignore_ascii_case("select") => {
                    self.lexer.next();
                    let sq = self.parse_select()?;
                    patterns.push(GraphPattern::Subquery(Box::new(sq)));
                }

                // Solution modifiers (LIMIT, OFFSET, ORDER BY, GROUP BY, HAVING) are not
                // valid inside a WHERE clause.  The user likely meant to put LIMIT inside
                // the subquery `{ SELECT … LIMIT n }` or at the outer SELECT level.
                // Return a clear error rather than silently misparssing the rest of the query.
                Token::Kw(k) if matches!(
                    k.to_ascii_lowercase().as_str(),
                    "limit" | "offset" | "order" | "group" | "having"
                ) => {
                    let kl = k.to_ascii_lowercase();
                    return Err(ParseError(format!(
                        "'{}' is not allowed inside a WHERE clause. \
                         If you meant to limit a subquery, place it inside the subquery braces: \
                         {{ SELECT … WHERE {{ … }} {} n }}",
                        kl.to_ascii_uppercase(), kl.to_ascii_uppercase()
                    )));
                }

                // Nested group graph pattern
                Token::LBrace => {
                    let inner = self.parse_group_graph_pattern()?;
                    patterns.push(inner);
                }

                // Triple patterns (and property path patterns)
                _ => {
                    // parse_triples_block returns a mix of Bgp and PathPattern nodes
                    let pats = self.parse_triples_block()?;
                    patterns.extend(pats);
                }
            }
        }

        Ok(combine_patterns(patterns))
    }

    // ── Property path support ─────────────────────────────────────────────────

    /// Internal: result of parsing a predicate — either a plain Term (variable or
    /// simple IRI → ordinary triple pattern) or a PropertyPath.
    fn parse_verb_or_path(&mut self) -> ParseResult<VerbOrPath> {
        // 'a' keyword = rdf:type
        if matches!(self.lexer.peek(), Token::Kw(k) if k.eq_ignore_ascii_case("a")) {
            self.lexer.next();
            let iri = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type".to_string();
            // Check for path modifier after 'a'
            return match self.lexer.peek() {
                Token::Times  => { self.lexer.next(); Ok(VerbOrPath::Path(PropertyPath::ZeroOrMore(Box::new(PropertyPath::Iri(iri))))) }
                Token::Plus2  => { self.lexer.next(); Ok(VerbOrPath::Path(PropertyPath::OneOrMore(Box::new(PropertyPath::Iri(iri))))) }
                Token::Question => { self.lexer.next(); Ok(VerbOrPath::Path(PropertyPath::ZeroOrOne(Box::new(PropertyPath::Iri(iri))))) }
                _ => Ok(VerbOrPath::Term(Term::Iri(iri))),
            };
        }

        // Variable predicates — not a path expression
        if matches!(self.lexer.peek(), Token::Var(_)) {
            return Ok(VerbOrPath::Term(self.parse_var_or_term()?));
        }

        // IRI / grouped path / inverse — parse as full path alternative
        if matches!(self.lexer.peek(),
            Token::IriRef(_) | Token::PrefixedName(_, _) | Token::Hat | Token::LParen)
        {
            let path = self.parse_path_alternative()?;
            // If it's just a plain IRI (no modifiers), return as a regular Term
            // to keep the leapfrog optimizer on the fast path.
            return match path {
                PropertyPath::Iri(ref iri) => Ok(VerbOrPath::Term(Term::Iri(iri.clone()))),
                _ => Ok(VerbOrPath::Path(path)),
            };
        }

        Err(ParseError(format!("expected predicate or property path, got {:?}", self.lexer.peek())))
    }

    /// PathAlternative ::= PathSequence ( '|' PathSequence )*
    fn parse_path_alternative(&mut self) -> ParseResult<PropertyPath> {
        let first = self.parse_path_sequence()?;
        if !matches!(self.lexer.peek(), Token::Pipe) {
            return Ok(first);
        }
        let mut alts = vec![first];
        while matches!(self.lexer.peek(), Token::Pipe) {
            self.lexer.next();
            alts.push(self.parse_path_sequence()?);
        }
        Ok(PropertyPath::Alternative(alts))
    }

    /// PathSequence ::= PathEltOrInverse ( '/' PathEltOrInverse )*
    fn parse_path_sequence(&mut self) -> ParseResult<PropertyPath> {
        let first = self.parse_path_elt_or_inverse()?;
        if !matches!(self.lexer.peek(), Token::Slash) {
            return Ok(first);
        }
        let mut steps = vec![first];
        while matches!(self.lexer.peek(), Token::Slash) {
            self.lexer.next();
            steps.push(self.parse_path_elt_or_inverse()?);
        }
        Ok(PropertyPath::Sequence(steps))
    }

    /// PathEltOrInverse ::= PathElt | '^' PathElt
    fn parse_path_elt_or_inverse(&mut self) -> ParseResult<PropertyPath> {
        if matches!(self.lexer.peek(), Token::Hat) {
            self.lexer.next();
            let elt = self.parse_path_elt()?;
            Ok(PropertyPath::Inverse(Box::new(elt)))
        } else {
            self.parse_path_elt()
        }
    }

    /// PathElt ::= PathPrimary PathMod?
    fn parse_path_elt(&mut self) -> ParseResult<PropertyPath> {
        let primary = self.parse_path_primary()?;
        match self.lexer.peek() {
            Token::Times   => { self.lexer.next(); Ok(PropertyPath::ZeroOrMore(Box::new(primary))) }
            Token::Plus2   => { self.lexer.next(); Ok(PropertyPath::OneOrMore(Box::new(primary))) }
            Token::Question => { self.lexer.next(); Ok(PropertyPath::ZeroOrOne(Box::new(primary))) }
            _ => Ok(primary),
        }
    }

    /// PathPrimary ::= iri | 'a' | '(' PathAlternative ')'
    fn parse_path_primary(&mut self) -> ParseResult<PropertyPath> {
        match self.lexer.peek().clone() {
            Token::LParen => {
                self.lexer.next();
                let path = self.parse_path_alternative()?;
                self.expect(Token::RParen)?;
                Ok(path)
            }
            Token::Kw(k) if k.eq_ignore_ascii_case("a") => {
                self.lexer.next();
                Ok(PropertyPath::Iri("http://www.w3.org/1999/02/22-rdf-syntax-ns#type".into()))
            }
            Token::IriRef(_) | Token::PrefixedName(_, _) => {
                let iri = self.parse_iri()?;
                Ok(PropertyPath::Iri(iri))
            }
            other => Err(ParseError(format!("expected path element, got {:?}", other))),
        }
    }

    // ── Triple block parsing ──────────────────────────────────────────────────

    /// Parse a triple block from a WHERE clause.
    /// Returns a mix of `GraphPattern::Bgp` and `GraphPattern::PathPattern` nodes.
    fn parse_triples_block(&mut self) -> ParseResult<Vec<GraphPattern>> {
        let mut all_patterns: Vec<GraphPattern> = Vec::new();
        let mut cur_triples: Vec<TriplePatternAst> = Vec::new();

        loop {
            // Consume any stray dots
            while matches!(self.lexer.peek(), Token::Dot) {
                self.lexer.next();
            }
            // Stop at block-ending tokens
            match self.lexer.peek() {
                // LBrace starts a nested group / subquery — never a triple subject.
                // Without this check, parse_var_or_term would unconditionally consume
                // the `{` via lexer.next() before returning Err, silently eating the
                // opening brace of `{ SELECT ... }` blocks.
                Token::RBrace | Token::Eof | Token::LBrace => break,
                Token::Kw(k) => {
                    let kl = k.to_ascii_lowercase();
                    if matches!(kl.as_str(), "optional" | "union" | "filter" | "bind"
                        | "values" | "graph" | "select"
                        // Solution modifiers — not valid inside a WHERE clause.
                        // Stop here so they are not silently consumed as a subject term.
                        | "limit" | "offset" | "order" | "group" | "having") { break; }
                }
                _ => {}
            }
            // Parse subject
            let subject = match self.parse_var_or_term() {
                Ok(t) => t,
                Err(_) => break,
            };
            // Parse predicate-object pairs
            let mut sub_triples: Vec<TriplePatternAst> = Vec::new();
            let mut sub_paths: Vec<GraphPattern> = Vec::new();
            self.parse_property_list(&subject, &mut sub_triples, &mut sub_paths)?;

            cur_triples.extend(sub_triples);
            if !sub_paths.is_empty() {
                if !cur_triples.is_empty() {
                    all_patterns.push(GraphPattern::Bgp(cur_triples.drain(..).collect()));
                }
                all_patterns.extend(sub_paths);
            }

            // Consume optional trailing dot
            if matches!(self.lexer.peek(), Token::Dot) {
                self.lexer.next();
            } else {
                match self.lexer.peek() {
                    Token::RBrace | Token::Eof => break,
                    Token::Kw(k) => {
                        let kl = k.to_ascii_lowercase();
                        if matches!(kl.as_str(), "optional" | "union" | "filter" | "bind"
                            | "values" | "graph" | "select") { break; }
                    }
                    _ => {}
                }
            }
        }

        if !cur_triples.is_empty() {
            all_patterns.push(GraphPattern::Bgp(cur_triples));
        }
        Ok(all_patterns)
    }

    /// parse_property_list now handles both plain predicates and property paths.
    /// Plain predicates produce triples; property paths produce PathPattern nodes.
    fn parse_property_list(
        &mut self,
        subject: &Term,
        triples: &mut Vec<TriplePatternAst>,
        path_patterns: &mut Vec<GraphPattern>,
    ) -> ParseResult<()> {
        if self.is_prop_list_end() {
            return Ok(());
        }
        self.parse_one_pred_obj(subject, triples, path_patterns)?;

        while matches!(self.lexer.peek(), Token::Semicolon) {
            self.lexer.next();
            if self.is_prop_list_end() { break; }
            self.parse_one_pred_obj(subject, triples, path_patterns)?;
        }
        Ok(())
    }

    fn parse_one_pred_obj(
        &mut self,
        subject: &Term,
        triples: &mut Vec<TriplePatternAst>,
        path_patterns: &mut Vec<GraphPattern>,
    ) -> ParseResult<()> {
        match self.parse_verb_or_path()? {
            VerbOrPath::Term(pred) => {
                // Regular predicate → triple patterns
                loop {
                    let obj = self.parse_object(triples, path_patterns)?;
                    triples.push(TriplePatternAst { s: subject.clone(), p: pred.clone(), o: obj });
                    if matches!(self.lexer.peek(), Token::Comma) { self.lexer.next(); } else { break; }
                }
            }
            VerbOrPath::Path(path) => {
                // Property path predicate → PathPattern nodes
                loop {
                    let obj = self.parse_object(triples, path_patterns)?;
                    path_patterns.push(GraphPattern::PathPattern {
                        s: subject.clone(),
                        path: path.clone(),
                        o: obj,
                    });
                    if matches!(self.lexer.peek(), Token::Comma) { self.lexer.next(); } else { break; }
                }
            }
        }
        Ok(())
    }

    fn parse_object(
        &mut self,
        triples: &mut Vec<TriplePatternAst>,
        path_patterns: &mut Vec<GraphPattern>,
    ) -> ParseResult<Term> {
        // Handle blank node property lists: [ pred obj ; ... ]
        if matches!(self.lexer.peek(), Token::LBracket) {
            self.lexer.next();
            let bn_id = self.fresh_blank();
            let bn = Term::BlankNode(bn_id);
            self.parse_property_list(&bn, triples, path_patterns)?;
            self.expect(Token::RBracket)?;
            return Ok(bn);
        }
        self.parse_var_or_term()
    }

    /// Parse triple block for CONSTRUCT template (no property paths needed).
    fn parse_triple_block(&mut self) -> ParseResult<Vec<TriplePatternAst>> {
        let mut triples = Vec::new();
        let mut _paths = Vec::new(); // property paths in CONSTRUCT are ignored
        loop {
            match self.lexer.peek() {
                Token::RBrace | Token::Eof => break,
                _ => {}
            }
            let s = match self.parse_var_or_term() {
                Ok(t) => t,
                Err(_) => break,
            };
            self.parse_property_list(&s, &mut triples, &mut _paths)?;
            if matches!(self.lexer.peek(), Token::Dot) { self.lexer.next(); }
        }
        Ok(triples)
    }

    fn parse_var_or_term(&mut self) -> ParseResult<Term> {
        match self.lexer.next() {
            Token::Var(v) => Ok(Term::Variable(v.to_string())),
            Token::IriRef(iri) => Ok(Term::Iri(iri.to_string())),
            Token::PrefixedName(prefix, local) => {
                let base = self.prefixes.get(prefix)
                    .ok_or_else(|| ParseError(format!("unknown prefix: {}", prefix)))?
                    .clone();
                Ok(Term::Iri(format!("{}{}", base, local)))
            }
            // Keep the full "_:label" form so the executor can look it up in the
            // dictionary, which stores blank nodes with the "_:" prefix.
            Token::BlankNodeLabel(l) => Ok(Term::BlankNode(format!("_:{}", l))),
            Token::Anon => Ok(Term::BlankNode(self.fresh_blank())),
            Token::StringLit(s) => {
                // Check for datatype or lang tag
                let (dt, lang) = self.parse_literal_suffix()?;
                Ok(Term::Literal(Literal { value: s, datatype: dt, lang }))
            }
            Token::IntegerLit(n) => Ok(Term::Literal(Literal::typed(
                n.to_string(),
                "http://www.w3.org/2001/XMLSchema#integer",
            ))),
            Token::DecimalLit(f) | Token::DoubleLit(f) => Ok(Term::Literal(Literal::typed(
                format!("{}", f),
                "http://www.w3.org/2001/XMLSchema#decimal",
            ))),
            Token::Kw(k) if k.eq_ignore_ascii_case("true") =>
                Ok(Term::Literal(Literal::typed("true", "http://www.w3.org/2001/XMLSchema#boolean"))),
            Token::Kw(k) if k.eq_ignore_ascii_case("false") =>
                Ok(Term::Literal(Literal::typed("false", "http://www.w3.org/2001/XMLSchema#boolean"))),
            other => Err(ParseError(format!("expected term, got {:?}", other))),
        }
    }

    fn parse_var_or_iri(&mut self) -> ParseResult<Term> {
        match self.lexer.peek() {
            Token::Var(_) | Token::IriRef(_) | Token::PrefixedName(_, _) => {
                self.parse_var_or_term()
            }
            other => Err(ParseError(format!("expected var or IRI, got {:?}", other))),
        }
    }

    fn parse_literal_suffix(&mut self) -> ParseResult<(Option<String>, Option<String>)> {
        match self.lexer.peek() {
            Token::Hat => {
                self.lexer.next();
                // Expect another ^
                if matches!(self.lexer.peek(), Token::Hat) {
                    self.lexer.next();
                }
                let dt = self.parse_iri()?;
                Ok((Some(dt), None))
            }
            Token::At => {
                self.lexer.next();
                // Language tag: letters and hyphens
                let lang = match self.lexer.next() {
                    Token::Kw(k) => k.to_string(),
                    other => return Err(ParseError(format!("expected lang tag, got {:?}", other))),
                };
                Ok((None, Some(lang)))
            }
            _ => Ok((None, None)),
        }
    }

    fn parse_iri(&mut self) -> ParseResult<String> {
        match self.lexer.next() {
            Token::IriRef(iri) => Ok(iri.to_string()),
            Token::PrefixedName(prefix, local) => {
                let base = self.prefixes.get(prefix)
                    .ok_or_else(|| ParseError(format!("unknown prefix: {}", prefix)))?
                    .clone();
                Ok(format!("{}{}", base, local))
            }
            other => Err(ParseError(format!("expected IRI, got {:?}", other))),
        }
    }

    fn parse_var(&mut self) -> ParseResult<String> {
        match self.lexer.next() {
            Token::Var(v) => Ok(v.to_string()),
            other => Err(ParseError(format!("expected variable, got {:?}", other))),
        }
    }

    fn parse_constraint(&mut self) -> ParseResult<Expression> {
        if matches!(self.lexer.peek(), Token::LParen) {
            self.lexer.next();
            let e = self.parse_expression()?;
            self.expect(Token::RParen)?;
            Ok(e)
        } else {
            // Built-in call without parens (rare)
            self.parse_expression()
        }
    }

    // ── Expression parsing (Pratt / precedence climbing) ──────────────────────

    fn parse_expression(&mut self) -> ParseResult<Expression> {
        self.parse_or_expr()
    }

    fn parse_or_expr(&mut self) -> ParseResult<Expression> {
        let mut left = self.parse_and_expr()?;
        while matches!(self.lexer.peek(), Token::Or) {
            self.lexer.next();
            let right = self.parse_and_expr()?;
            left = Expression::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and_expr(&mut self) -> ParseResult<Expression> {
        let mut left = self.parse_relational()?;
        while matches!(self.lexer.peek(), Token::And) {
            self.lexer.next();
            let right = self.parse_relational()?;
            left = Expression::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_relational(&mut self) -> ParseResult<Expression> {
        let left = self.parse_additive()?;
        match self.lexer.peek() {
            Token::Eq  => { self.lexer.next(); Ok(Expression::Eq(Box::new(left), Box::new(self.parse_additive()?))) }
            Token::Ne  => { self.lexer.next(); Ok(Expression::Ne(Box::new(left), Box::new(self.parse_additive()?))) }
            Token::Lt  => { self.lexer.next(); Ok(Expression::Lt(Box::new(left), Box::new(self.parse_additive()?))) }
            Token::Le  => { self.lexer.next(); Ok(Expression::Le(Box::new(left), Box::new(self.parse_additive()?))) }
            Token::Gt  => { self.lexer.next(); Ok(Expression::Gt(Box::new(left), Box::new(self.parse_additive()?))) }
            Token::Ge  => { self.lexer.next(); Ok(Expression::Ge(Box::new(left), Box::new(self.parse_additive()?))) }
            _ => Ok(left),
        }
    }

    fn parse_additive(&mut self) -> ParseResult<Expression> {
        let mut left = self.parse_multiplicative()?;
        loop {
            match self.lexer.peek() {
                Token::Plus2 => { self.lexer.next(); let r = self.parse_multiplicative()?; left = Expression::Add(Box::new(left), Box::new(r)); }
                Token::Minus => { self.lexer.next(); let r = self.parse_multiplicative()?; left = Expression::Sub(Box::new(left), Box::new(r)); }
                _ => break,
            }
        }
        Ok(left)
    }

    fn parse_multiplicative(&mut self) -> ParseResult<Expression> {
        let mut left = self.parse_unary()?;
        loop {
            match self.lexer.peek() {
                Token::Times => { self.lexer.next(); let r = self.parse_unary()?; left = Expression::Mul(Box::new(left), Box::new(r)); }
                Token::Slash => { self.lexer.next(); let r = self.parse_unary()?; left = Expression::Div(Box::new(left), Box::new(r)); }
                _ => break,
            }
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> ParseResult<Expression> {
        match self.lexer.peek() {
            Token::Bang  => { self.lexer.next(); Ok(Expression::Not(Box::new(self.parse_primary()?))) }
            Token::Minus => { self.lexer.next(); Ok(Expression::Neg(Box::new(self.parse_primary()?))) }
            Token::Plus2 => { self.lexer.next(); self.parse_primary() }
            _ => self.parse_primary(),
        }
    }

    fn parse_primary(&mut self) -> ParseResult<Expression> {
        match self.lexer.peek().clone() {
            Token::LParen => {
                self.lexer.next();
                let e = self.parse_expression()?;
                self.expect(Token::RParen)?;
                Ok(e)
            }
            Token::Var(v) => {
                let s = v.to_string();
                self.lexer.next();
                Ok(Expression::Variable(s))
            }
            Token::IriRef(iri) => {
                let s = iri.to_string();
                self.lexer.next();
                Ok(Expression::Iri(s))
            }
            Token::PrefixedName(prefix, local) => {
                let prefix = prefix.to_string();
                let local = local.to_string();
                self.lexer.next();
                let base = self.prefixes.get(&prefix)
                    .ok_or_else(|| ParseError(format!("unknown prefix: {}", prefix)))?
                    .clone();
                Ok(Expression::Iri(format!("{}{}", base, local)))
            }
            Token::StringLit(s) => {
                self.lexer.next();
                let (dt, lang) = self.parse_literal_suffix()?;
                Ok(Expression::Literal(Literal { value: s, datatype: dt, lang }))
            }
            Token::IntegerLit(n) => {
                self.lexer.next();
                Ok(Expression::Literal(Literal::typed(n.to_string(), "http://www.w3.org/2001/XMLSchema#integer")))
            }
            Token::DecimalLit(f) | Token::DoubleLit(f) => {
                self.lexer.next();
                Ok(Expression::Literal(Literal::typed(f.to_string(), "http://www.w3.org/2001/XMLSchema#decimal")))
            }
            Token::Kw(k) if k.eq_ignore_ascii_case("true") => {
                self.lexer.next();
                Ok(Expression::Literal(Literal::typed("true", "http://www.w3.org/2001/XMLSchema#boolean")))
            }
            Token::Kw(k) if k.eq_ignore_ascii_case("false") => {
                self.lexer.next();
                Ok(Expression::Literal(Literal::typed("false", "http://www.w3.org/2001/XMLSchema#boolean")))
            }
            Token::Kw(_) => self.parse_builtin_call(),
            _ => Err(ParseError(format!("unexpected token in expression: {:?}", self.lexer.peek()))),
        }
    }

    fn parse_builtin_call(&mut self) -> ParseResult<Expression> {
        let kw = match self.lexer.next() {
            Token::Kw(k) => k.to_ascii_lowercase(),
            other => return Err(ParseError(format!("expected function name, got {:?}", other))),
        };

        match kw.as_str() {
            "bound" => {
                self.expect(Token::LParen)?;
                let v = self.parse_var()?;
                self.expect(Token::RParen)?;
                Ok(Expression::Bound(v))
            }
            "isiri" | "isuri" => {
                let e = self.parse_paren_expr()?;
                Ok(Expression::IsIri(Box::new(e)))
            }
            "isliteral" => { let e = self.parse_paren_expr()?; Ok(Expression::IsLiteral(Box::new(e))) }
            "isblank" => { let e = self.parse_paren_expr()?; Ok(Expression::IsBlank(Box::new(e))) }
            "isnumeric" => { let e = self.parse_paren_expr()?; Ok(Expression::IsNumeric(Box::new(e))) }
            "str" => { let e = self.parse_paren_expr()?; Ok(Expression::Str(Box::new(e))) }
            "lang" => { let e = self.parse_paren_expr()?; Ok(Expression::Lang(Box::new(e))) }
            "datatype" => { let e = self.parse_paren_expr()?; Ok(Expression::Datatype(Box::new(e))) }
            "iri" | "uri" => { let e = self.parse_paren_expr()?; Ok(Expression::Iri2(Box::new(e))) }
            "strlen" => { let e = self.parse_paren_expr()?; Ok(Expression::Strlen(Box::new(e))) }
            "ucase" => { let e = self.parse_paren_expr()?; Ok(Expression::UCase(Box::new(e))) }
            "lcase" => { let e = self.parse_paren_expr()?; Ok(Expression::LCase(Box::new(e))) }
            "abs" => { let e = self.parse_paren_expr()?; Ok(Expression::Abs(Box::new(e))) }
            "round" => { let e = self.parse_paren_expr()?; Ok(Expression::Round(Box::new(e))) }
            "ceil" => { let e = self.parse_paren_expr()?; Ok(Expression::Ceil(Box::new(e))) }
            "floor" => { let e = self.parse_paren_expr()?; Ok(Expression::Floor(Box::new(e))) }
            "year" => { let e = self.parse_paren_expr()?; Ok(Expression::Year(Box::new(e))) }
            "month" => { let e = self.parse_paren_expr()?; Ok(Expression::Month(Box::new(e))) }
            "day" => { let e = self.parse_paren_expr()?; Ok(Expression::Day(Box::new(e))) }
            "now" => { Ok(Expression::Now) }
            "sameterm" => {
                self.expect(Token::LParen)?;
                let a = self.parse_expression()?;
                self.expect(Token::Comma)?;
                let b = self.parse_expression()?;
                self.expect(Token::RParen)?;
                Ok(Expression::SameTerm(Box::new(a), Box::new(b)))
            }
            "langmatches" => {
                self.expect(Token::LParen)?;
                let a = self.parse_expression()?;
                self.expect(Token::Comma)?;
                let b = self.parse_expression()?;
                self.expect(Token::RParen)?;
                Ok(Expression::LangMatches(Box::new(a), Box::new(b)))
            }
            "regex" => {
                self.expect(Token::LParen)?;
                let text = self.parse_expression()?;
                self.expect(Token::Comma)?;
                let pattern = self.parse_expression()?;
                let flags = if matches!(self.lexer.peek(), Token::Comma) {
                    self.lexer.next();
                    Some(Box::new(self.parse_expression()?))
                } else { None };
                self.expect(Token::RParen)?;
                Ok(Expression::Regex(Box::new(text), Box::new(pattern), flags))
            }
            "substr" => {
                self.expect(Token::LParen)?;
                let s = self.parse_expression()?;
                self.expect(Token::Comma)?;
                let start = self.parse_expression()?;
                let len = if matches!(self.lexer.peek(), Token::Comma) {
                    self.lexer.next();
                    Some(Box::new(self.parse_expression()?))
                } else { None };
                self.expect(Token::RParen)?;
                Ok(Expression::Substr(Box::new(s), Box::new(start), len))
            }
            "concat" => {
                self.expect(Token::LParen)?;
                let mut args = vec![self.parse_expression()?];
                while matches!(self.lexer.peek(), Token::Comma) {
                    self.lexer.next();
                    args.push(self.parse_expression()?);
                }
                self.expect(Token::RParen)?;
                Ok(Expression::Concat(args))
            }
            "contains" => {
                self.expect(Token::LParen)?;
                let a = self.parse_expression()?;
                self.expect(Token::Comma)?;
                let b = self.parse_expression()?;
                self.expect(Token::RParen)?;
                Ok(Expression::Contains(Box::new(a), Box::new(b)))
            }
            "replace" => {
                self.expect(Token::LParen)?;
                let s = self.parse_expression()?;
                self.expect(Token::Comma)?;
                let pattern = self.parse_expression()?;
                self.expect(Token::Comma)?;
                let replacement = self.parse_expression()?;
                let flags = if matches!(self.lexer.peek(), Token::Comma) {
                    self.lexer.next();
                    Some(Box::new(self.parse_expression()?))
                } else {
                    None
                };
                self.expect(Token::RParen)?;
                Ok(Expression::Replace(Box::new(s), Box::new(pattern), Box::new(replacement), flags))
            }
            "strstarts" => {
                self.expect(Token::LParen)?;
                let a = self.parse_expression()?;
                self.expect(Token::Comma)?;
                let b = self.parse_expression()?;
                self.expect(Token::RParen)?;
                Ok(Expression::StrStarts(Box::new(a), Box::new(b)))
            }
            "strends" => {
                self.expect(Token::LParen)?;
                let a = self.parse_expression()?;
                self.expect(Token::Comma)?;
                let b = self.parse_expression()?;
                self.expect(Token::RParen)?;
                Ok(Expression::StrEnds(Box::new(a), Box::new(b)))
            }
            "if" => {
                self.expect(Token::LParen)?;
                let cond = self.parse_expression()?;
                self.expect(Token::Comma)?;
                let then = self.parse_expression()?;
                self.expect(Token::Comma)?;
                let else_ = self.parse_expression()?;
                self.expect(Token::RParen)?;
                Ok(Expression::If(Box::new(cond), Box::new(then), Box::new(else_)))
            }
            "coalesce" => {
                self.expect(Token::LParen)?;
                let mut args = vec![self.parse_expression()?];
                while matches!(self.lexer.peek(), Token::Comma) {
                    self.lexer.next();
                    args.push(self.parse_expression()?);
                }
                self.expect(Token::RParen)?;
                Ok(Expression::Coalesce(args))
            }
            // Aggregates
            "count" => self.parse_aggregate_count(),
            "sum"   => self.parse_aggregate(|d, e| Expression::Sum   { distinct: d, expr: Box::new(e) }),
            "min"   => self.parse_aggregate(|d, e| Expression::Min   { distinct: d, expr: Box::new(e) }),
            "max"   => self.parse_aggregate(|d, e| Expression::Max   { distinct: d, expr: Box::new(e) }),
            "avg"   => self.parse_aggregate(|d, e| Expression::Avg   { distinct: d, expr: Box::new(e) }),
            "sample" => self.parse_aggregate(|d, e| Expression::Sample { distinct: d, expr: Box::new(e) }),
            "group_concat" => self.parse_group_concat(),

            other => Err(ParseError(format!("unknown function: {}", other))),
        }
    }

    fn parse_paren_expr(&mut self) -> ParseResult<Expression> {
        self.expect(Token::LParen)?;
        let e = self.parse_expression()?;
        self.expect(Token::RParen)?;
        Ok(e)
    }

    fn parse_aggregate_count(&mut self) -> ParseResult<Expression> {
        self.expect(Token::LParen)?;
        let distinct = if matches!(self.lexer.peek(), Token::Kw(k) if k.eq_ignore_ascii_case("distinct")) {
            self.lexer.next(); true
        } else { false };
        let expr = if matches!(self.lexer.peek(), Token::Times) {
            self.lexer.next(); None
        } else {
            Some(Box::new(self.parse_expression()?))
        };
        self.expect(Token::RParen)?;
        Ok(Expression::Count { distinct, expr })
    }

    fn parse_aggregate<F: Fn(bool, Expression) -> Expression>(&mut self, f: F) -> ParseResult<Expression> {
        self.expect(Token::LParen)?;
        let distinct = if matches!(self.lexer.peek(), Token::Kw(k) if k.eq_ignore_ascii_case("distinct")) {
            self.lexer.next(); true
        } else { false };
        let expr = self.parse_expression()?;
        self.expect(Token::RParen)?;
        Ok(f(distinct, expr))
    }

    fn parse_group_concat(&mut self) -> ParseResult<Expression> {
        self.expect(Token::LParen)?;
        let distinct = if matches!(self.lexer.peek(), Token::Kw(k) if k.eq_ignore_ascii_case("distinct")) {
            self.lexer.next(); true
        } else { false };
        let expr = self.parse_expression()?;
        let separator = if matches!(self.lexer.peek(), Token::Semicolon) {
            self.lexer.next();
            self.expect_kw("separator")?;
            self.expect(Token::Eq)?;
            match self.lexer.next() {
                Token::StringLit(s) => Some(s),
                _ => None,
            }
        } else { None };
        self.expect(Token::RParen)?;
        Ok(Expression::GroupConcat { distinct, expr: Box::new(expr), separator })
    }

    // ── Modifiers ─────────────────────────────────────────────────────────────

    fn parse_group_by(&mut self) -> ParseResult<Vec<GroupCondition>> {
        if !matches!(self.lexer.peek(), Token::Kw(k) if k.eq_ignore_ascii_case("group")) {
            return Ok(Vec::new());
        }
        self.lexer.next();
        self.expect_kw("by")?;
        let mut conditions = Vec::new();
        loop {
            match self.lexer.peek() {
                Token::LParen => {
                    self.lexer.next();
                    let expr = self.parse_expression()?;
                    let alias = if matches!(self.lexer.peek(), Token::Kw(k) if k.eq_ignore_ascii_case("as")) {
                        self.lexer.next();
                        Some(self.parse_var()?)
                    } else { None };
                    self.expect(Token::RParen)?;
                    conditions.push(GroupCondition { expr, alias });
                }
                Token::Var(_) => {
                    let v = self.parse_var()?;
                    conditions.push(GroupCondition { expr: Expression::Variable(v), alias: None });
                }
                _ => break,
            }
        }
        Ok(conditions)
    }

    fn parse_having(&mut self) -> ParseResult<Vec<Expression>> {
        if !matches!(self.lexer.peek(), Token::Kw(k) if k.eq_ignore_ascii_case("having")) {
            return Ok(Vec::new());
        }
        self.lexer.next();
        Ok(vec![self.parse_constraint()?])
    }

    fn parse_order_by(&mut self) -> ParseResult<Vec<OrderCondition>> {
        if !matches!(self.lexer.peek(), Token::Kw(k) if k.eq_ignore_ascii_case("order")) {
            return Ok(Vec::new());
        }
        self.lexer.next();
        self.expect_kw("by")?;
        let mut conditions = Vec::new();
        loop {
            match self.lexer.peek().clone() {
                Token::Kw(k) if k.eq_ignore_ascii_case("asc") => {
                    self.lexer.next();
                    let expr = self.parse_paren_expr()?;
                    conditions.push(OrderCondition { direction: OrderDirection::Asc, expr });
                }
                Token::Kw(k) if k.eq_ignore_ascii_case("desc") => {
                    self.lexer.next();
                    let expr = self.parse_paren_expr()?;
                    conditions.push(OrderCondition { direction: OrderDirection::Desc, expr });
                }
                Token::Var(_) | Token::LParen => {
                    let expr = self.parse_expression()?;
                    conditions.push(OrderCondition { direction: OrderDirection::Asc, expr });
                }
                _ => break,
            }
        }
        Ok(conditions)
    }

    fn parse_limit_offset(&mut self) -> ParseResult<(Option<u64>, Option<u64>)> {
        let mut limit = None;
        let mut offset = None;
        loop {
            match self.lexer.peek() {
                Token::Kw(k) if k.eq_ignore_ascii_case("limit") => {
                    self.lexer.next();
                    if let Token::IntegerLit(n) = self.lexer.next() {
                        limit = Some(n as u64);
                    }
                }
                Token::Kw(k) if k.eq_ignore_ascii_case("offset") => {
                    self.lexer.next();
                    if let Token::IntegerLit(n) = self.lexer.next() {
                        offset = Some(n as u64);
                    }
                }
                _ => break,
            }
        }
        Ok((limit, offset))
    }

    fn parse_values_clause(&mut self) -> ParseResult<Option<ValuesClause>> {
        if !matches!(self.lexer.peek(), Token::Kw(k) if k.eq_ignore_ascii_case("values")) {
            return Ok(None);
        }
        self.lexer.next();
        Ok(Some(self.parse_inline_data()?))
    }

    fn parse_inline_data(&mut self) -> ParseResult<ValuesClause> {
        // VALUES ?x { val... } or VALUES (?x ?y) { (val val) ... }
        let (variables, multi) = match self.lexer.peek() {
            Token::Var(_) => {
                let v = self.parse_var()?;
                (vec![v], false)
            }
            Token::LParen => {
                self.lexer.next();
                let mut vars = Vec::new();
                while let Token::Var(_) = self.lexer.peek() {
                    vars.push(self.parse_var()?);
                }
                self.expect(Token::RParen)?;
                (vars, true)
            }
            _ => return Err(ParseError("expected variable(s) after VALUES".into())),
        };

        self.expect(Token::LBrace)?;
        let mut rows = Vec::new();

        loop {
            match self.lexer.peek() {
                Token::RBrace | Token::Eof => break,
                Token::LParen if multi => {
                    self.lexer.next();
                    let mut row = Vec::new();
                    while !matches!(self.lexer.peek(), Token::RParen | Token::Eof) {
                        let term = if matches!(self.lexer.peek(), Token::Kw(k) if k.eq_ignore_ascii_case("undef")) {
                            self.lexer.next(); None
                        } else {
                            Some(self.parse_var_or_term()?)
                        };
                        row.push(term);
                    }
                    self.expect(Token::RParen)?;
                    rows.push(row);
                }
                _ if !multi => {
                    let term = if matches!(self.lexer.peek(), Token::Kw(k) if k.eq_ignore_ascii_case("undef")) {
                        self.lexer.next(); None
                    } else {
                        Some(self.parse_var_or_term()?)
                    };
                    rows.push(vec![term]);
                }
                _ => break,
            }
        }
        self.expect(Token::RBrace)?;
        Ok(ValuesClause { variables, rows })
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn expect(&mut self, expected: Token<'static>) -> ParseResult<()> {
        let tok = self.lexer.next();
        // Simplified equality check by variant
        let ok = match (&tok, &expected) {
            (Token::LBrace, Token::LBrace) => true,
            (Token::RBrace, Token::RBrace) => true,
            (Token::LParen, Token::LParen) => true,
            (Token::RParen, Token::RParen) => true,
            (Token::LBracket, Token::LBracket) => true,
            (Token::RBracket, Token::RBracket) => true,
            (Token::Dot, Token::Dot) => true,
            (Token::Comma, Token::Comma) => true,
            (Token::Semicolon, Token::Semicolon) => true,
            (Token::Eq, Token::Eq) => true,
            (Token::Times, Token::Times) => true,
            _ => false,
        };
        if ok { Ok(()) } else {
            Err(ParseError(format!("expected {:?}, got {:?}", expected, tok)))
        }
    }

    fn expect_kw(&mut self, kw: &str) -> ParseResult<()> {
        match self.lexer.next() {
            Token::Kw(k) if k.eq_ignore_ascii_case(kw) => Ok(()),
            other => Err(ParseError(format!("expected keyword '{}', got {:?}", kw, other))),
        }
    }


    /// True if the next token ends a property list (no more pred-obj pairs expected).
    fn is_prop_list_end(&mut self) -> bool {
        match self.lexer.peek() {
            Token::Dot | Token::RBrace | Token::Eof | Token::Semicolon => true,
            Token::Kw(k) => {
                let kl = k.to_ascii_lowercase();
                matches!(
                    kl.as_str(),
                    "optional" | "union" | "filter" | "bind"
                        | "values" | "graph" | "select"
                        | "limit" | "offset" | "order" | "group"
                        | "having" | "where"
                )
            }
            _ => false,
        }
    }

    fn fresh_blank(&mut self) -> String {
        let id = self.blank_counter;
        self.blank_counter += 1;
        format!("_:b{}", id)
    }
}

/// Combine a list of patterns into a single Join tree.
fn combine_patterns(patterns: Vec<GraphPattern>) -> GraphPattern {
    let non_empty: Vec<_> = patterns.into_iter().filter(|p| !matches!(p, GraphPattern::Empty)).collect();
    match non_empty.len() {
        0 => GraphPattern::Empty,
        1 => non_empty.into_iter().next().unwrap(),
        _ => non_empty.into_iter().reduce(|a, b| GraphPattern::Join(Box::new(a), Box::new(b))).unwrap(),
    }
}

/// Public entry point.
pub fn parse_query(input: &str) -> Result<Query, ParseError> {
    Parser::new(input).parse()
}
