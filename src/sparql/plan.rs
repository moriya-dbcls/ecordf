//! Execution plan types — the output of the query optimizer.

use crate::triple::{TriplePattern};
use super::ast::{Expression, SelectQuery, ValuesClause, TriplePatternAst, Term, PropertyPath};

/// A physical execution plan node.
#[derive(Debug, Clone)]
pub enum ExecutionPlan {
    /// No results (empty pattern).
    Empty,
    /// Scan one triple pattern (after dictionary encoding).
    Scan {
        pattern: TriplePattern,
        /// Maps variable name → position in triple (0=S, 1=P, 2=O).
        variables: Vec<(String, u8)>,
    },
    /// Scan at AST level (before dictionary encoding, resolved at runtime).
    /// Retained for path-pattern and other ad-hoc uses; the query optimizer
    /// emits `ScanBound` instead for all BGP patterns.
    ScanAst(TriplePatternAst),
    /// Scan with constants pre-encoded at plan-compile time.
    ///
    /// Unlike `ScanAst`, no dictionary lookup is needed at runtime.
    /// - `base`      : triple pattern with constants already encoded as TermIds;
    ///                 variable positions hold `UNBOUND` (wildcard).
    /// - `free_vars` : variables that remain unbound in `base` → output columns.
    /// - `outer_vars`: variables expected from the outer bind-join context;
    ///                 each is probed in the outer `Binding` and, if present,
    ///                 substituted into the corresponding position of `base`.
    ScanBound {
        base: TriplePattern,
        free_vars: Vec<(String, u8)>,
        outer_vars: Vec<(String, u8)>,
    },
    /// Leapfrog Triejoin over multiple patterns sharing variables.
    LeapfrogJoin {
        patterns: Vec<(TriplePattern, Vec<(String, u8)>)>,
    },
    /// Binary hash join.
    Join(Box<ExecutionPlan>, Box<ExecutionPlan>),
    /// LEFT OUTER join (for OPTIONAL).
    Optional(Box<ExecutionPlan>, Box<ExecutionPlan>),
    /// UNION.
    Union(Box<ExecutionPlan>, Box<ExecutionPlan>),
    /// Row-level filter.
    Filter(Box<ExecutionPlan>, Expression),
    /// BIND expression.
    Extend(Box<ExecutionPlan>, Expression, String),
    /// Inline VALUES.
    Values(ValuesClause),
    /// SPARQL 1.1 property path evaluation (transitive closure, alternation, etc.)
    PathPattern { s: Term, path: PropertyPath, o: Term },
    /// GRAPH clause — execute inner plan restricted to a named graph.
    /// `graph` is either a concrete IRI (Term::Iri) or a variable (Term::Variable).
    NamedGraph { graph: Term, inner: Box<ExecutionPlan> },
    /// { SELECT … } subquery — executed as a self-contained unit with its own
    /// DISTINCT / GROUP BY / ORDER BY / LIMIT before being joined with the outer query.
    Subquery(Box<SelectQuery>),
}
