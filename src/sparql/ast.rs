//! SPARQL 1.1 Abstract Syntax Tree
//!
//! Covers: SELECT, ASK, CONSTRUCT
//! Patterns: BGP, OPTIONAL, UNION, FILTER, BIND, VALUES, subquery
//! Aggregates: COUNT, SUM, AVG, MIN, MAX, GROUP_CONCAT
//! Modifiers: DISTINCT, ORDER BY, GROUP BY, HAVING, LIMIT, OFFSET

use std::collections::HashMap;

// ── Top-level query ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Query {
    pub prefixes: HashMap<String, String>, // prefix → IRI
    pub form: QueryForm,
}

#[derive(Debug, Clone)]
pub enum QueryForm {
    Select(SelectQuery),
    Ask(AskQuery),
    Construct(ConstructQuery),
}

#[derive(Debug, Clone)]
pub struct SelectQuery {
    pub distinct: bool,
    pub projection: Projection,
    pub dataset: Vec<DatasetClause>,
    pub pattern: GraphPattern,
    pub group_by: Vec<GroupCondition>,
    pub having: Vec<Expression>,
    pub order_by: Vec<OrderCondition>,
    pub limit: Option<u64>,
    pub offset: Option<u64>,
    pub values: Option<ValuesClause>,
}

#[derive(Debug, Clone)]
pub struct AskQuery {
    pub dataset: Vec<DatasetClause>,
    pub pattern: GraphPattern,
}

#[derive(Debug, Clone)]
pub struct ConstructQuery {
    pub template: Vec<TriplePatternAst>,
    pub dataset: Vec<DatasetClause>,
    pub pattern: GraphPattern,
    pub limit: Option<u64>,
    pub offset: Option<u64>,
}

// ── Projection ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum Projection {
    Wildcard,                           // SELECT *
    Variables(Vec<SelectItem>),         // SELECT ?x (expr AS ?y) ...
}

#[derive(Debug, Clone)]
pub enum SelectItem {
    Variable(String),
    Alias(Expression, String), // (expr AS ?name)
}

// ── Dataset ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct DatasetClause {
    pub named: bool,
    pub iri: String,
}

// ── Graph Patterns ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum GraphPattern {
    /// Basic Graph Pattern: a set of triple patterns
    Bgp(Vec<TriplePatternAst>),
    /// OPTIONAL { pattern }
    Optional(Box<GraphPattern>, Box<GraphPattern>),
    /// pattern1 UNION pattern2
    Union(Box<GraphPattern>, Box<GraphPattern>),
    /// Sequence (conjunction) of patterns
    Join(Box<GraphPattern>, Box<GraphPattern>),
    /// FILTER expression
    Filter(Box<GraphPattern>, Expression),
    /// BIND (expr AS ?var)
    Extend(Box<GraphPattern>, Expression, String),
    /// { SELECT ... } (subquery)
    Subquery(Box<SelectQuery>),
    /// VALUES clause inline
    Values(ValuesClause),
    /// GRAPH ?g { pattern } or GRAPH <iri> { pattern }
    Graph(Term, Box<GraphPattern>),
    /// SPARQL 1.1 Property Path: ?s path ?o
    PathPattern { s: Term, path: PropertyPath, o: Term },
    /// Empty pattern
    Empty,
}

// ── Property Paths (SPARQL 1.1) ───────────────────────────────────────────────

/// SPARQL 1.1 property path expression.
///
/// Grammar (simplified):
///   PathAlternative  ::= PathSequence ('|' PathSequence)*
///   PathSequence     ::= PathEltOrInverse ('/' PathEltOrInverse)*
///   PathEltOrInverse ::= PathElt | '^' PathElt
///   PathElt          ::= PathPrimary PathMod?
///   PathMod          ::= '*' | '+' | '?'
///   PathPrimary      ::= iri | 'a' | '(' PathAlternative ')'
#[derive(Debug, Clone)]
pub enum PropertyPath {
    /// Simple predicate IRI (e.g. `rdfs:subClassOf`)
    Iri(String),
    /// Sequence: p1/p2/… — follow p1 then p2
    Sequence(Vec<PropertyPath>),
    /// Alternative: p1|p2 — match either p1 or p2
    Alternative(Vec<PropertyPath>),
    /// Zero or more repetitions: p*
    ZeroOrMore(Box<PropertyPath>),
    /// One or more repetitions: p+
    OneOrMore(Box<PropertyPath>),
    /// Zero or one: p?
    ZeroOrOne(Box<PropertyPath>),
    /// Inverse direction: ^p
    Inverse(Box<PropertyPath>),
}

// ── Triple Pattern (AST level, before dictionary encoding) ────────────────────

#[derive(Debug, Clone)]
pub struct TriplePatternAst {
    pub s: Term,
    pub p: Term,
    pub o: Term,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Term {
    /// ?varName
    Variable(String),
    /// <http://...> or prefix:local
    Iri(String),
    /// "string" or "string"^^<type> or "string"@lang
    Literal(Literal),
    /// [] (blank node in template)
    BlankNode(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Literal {
    pub value: String,
    pub datatype: Option<String>,
    pub lang: Option<String>,
}

impl Literal {
    pub fn plain(value: impl Into<String>) -> Self {
        Self { value: value.into(), datatype: None, lang: None }
    }

    pub fn typed(value: impl Into<String>, datatype: impl Into<String>) -> Self {
        Self { value: value.into(), datatype: Some(datatype.into()), lang: None }
    }

    pub fn lang(value: impl Into<String>, lang: impl Into<String>) -> Self {
        Self { value: value.into(), datatype: None, lang: Some(lang.into()) }
    }

    /// Canonical N-Triples representation used as dictionary key.
    pub fn to_ntriples(&self) -> String {
        if let Some(ref l) = self.lang {
            format!("\"{}\"@{}", self.value, l)
        } else if let Some(ref dt) = self.datatype {
            format!("\"{}\"^^<{}>", self.value, dt)
        } else {
            format!("\"{}\"", self.value)
        }
    }
}

// ── Expressions ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Expression {
    // Terminals
    Variable(String),
    Literal(Literal),
    Iri(String),

    // Arithmetic
    Add(Box<Expression>, Box<Expression>),
    Sub(Box<Expression>, Box<Expression>),
    Mul(Box<Expression>, Box<Expression>),
    Div(Box<Expression>, Box<Expression>),
    Neg(Box<Expression>),

    // Comparison
    Eq(Box<Expression>, Box<Expression>),
    Ne(Box<Expression>, Box<Expression>),
    Lt(Box<Expression>, Box<Expression>),
    Le(Box<Expression>, Box<Expression>),
    Gt(Box<Expression>, Box<Expression>),
    Ge(Box<Expression>, Box<Expression>),

    // Logic
    And(Box<Expression>, Box<Expression>),
    Or(Box<Expression>, Box<Expression>),
    Not(Box<Expression>),

    // Built-in functions
    Bound(String),
    IsIri(Box<Expression>),
    IsLiteral(Box<Expression>),
    IsBlank(Box<Expression>),
    IsNumeric(Box<Expression>),
    Str(Box<Expression>),
    Lang(Box<Expression>),
    Datatype(Box<Expression>),
    LangMatches(Box<Expression>, Box<Expression>),
    Regex(Box<Expression>, Box<Expression>, Option<Box<Expression>>),
    Replace(Box<Expression>, Box<Expression>, Box<Expression>, Option<Box<Expression>>),
    Substr(Box<Expression>, Box<Expression>, Option<Box<Expression>>),
    Strlen(Box<Expression>),
    StrBefore(Box<Expression>, Box<Expression>),
    StrAfter(Box<Expression>, Box<Expression>),
    EncodeForUri(Box<Expression>),
    UCase(Box<Expression>),
    LCase(Box<Expression>),
    Concat(Vec<Expression>),
    Contains(Box<Expression>, Box<Expression>),
    StrStarts(Box<Expression>, Box<Expression>),
    StrEnds(Box<Expression>, Box<Expression>),
    Abs(Box<Expression>),
    Round(Box<Expression>),
    Ceil(Box<Expression>),
    Floor(Box<Expression>),
    Year(Box<Expression>),
    Month(Box<Expression>),
    Day(Box<Expression>),
    Hours(Box<Expression>),
    Minutes(Box<Expression>),
    Seconds(Box<Expression>),
    Now,
    If(Box<Expression>, Box<Expression>, Box<Expression>),
    Coalesce(Vec<Expression>),
    SameTerm(Box<Expression>, Box<Expression>),
    Iri2(Box<Expression>),   // IRI() / URI() function

    // Aggregates
    Count { distinct: bool, expr: Option<Box<Expression>> },
    Sum { distinct: bool, expr: Box<Expression> },
    Min { distinct: bool, expr: Box<Expression> },
    Max { distinct: bool, expr: Box<Expression> },
    Avg { distinct: bool, expr: Box<Expression> },
    GroupConcat { distinct: bool, expr: Box<Expression>, separator: Option<String> },
    Sample { distinct: bool, expr: Box<Expression> },
}

// ── Modifiers ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct GroupCondition {
    pub expr: Expression,
    pub alias: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OrderCondition {
    pub direction: OrderDirection,
    pub expr: Expression,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderDirection {
    Asc,
    Desc,
}

// ── VALUES clause ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ValuesClause {
    pub variables: Vec<String>,
    pub rows: Vec<Vec<Option<Term>>>, // None = UNDEF
}
