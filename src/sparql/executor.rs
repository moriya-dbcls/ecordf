//! # Query Executor with Leapfrog Triejoin
//!
//! ## Why Leapfrog Triejoin beats hash join (Virtuoso)
//!
//! Traditional hash join: O(|R| + |S| + |output|)
//! Problem: for n-way joins, you build n-1 hash tables sequentially.
//!
//! Leapfrog Triejoin: O(N × log N × |output|) where N = number of iterators.
//! Key advantage: it intersects all iterators *simultaneously* using sorted order.
//! For SPARQL BGPs with shared variables (very common in bio queries), this is
//! dramatically faster because it skips huge swaths of the search space.
//!
//! Example: query for all proteins of a given taxon with a specific GO term.
//! Hash join: enumerate all proteins of taxon, hash-probe GO annotations.
//! Leapfrog: advance both iterators in lockstep — only touching relevant pages.
//!
//! ## Algorithm overview
//!
//! Given k sorted iterators over the same domain, each with seek(v) and next():
//! ```text
//! 1. Initialize all iterators to their minimum.
//! 2. Let max = maximum of all current values.
//! 3. Seek all iterators to max.
//!    - If any iterator exhausted → done.
//!    - If all at max → emit max, advance all, go to 2.
//!    - Else → new max found, go to 3.
//! ```
//! This runs in O(output × k × log(n_i)) per variable.

use std::collections::{HashMap, HashSet};

use crate::config::QueryConfig;
use crate::dict_builder::QueryDict;
use crate::index::{GspoIndexFile, TripleIndex};
use crate::stats::StoreStatistics;
use crate::triple::{TermId, TriplePattern, UNBOUND};
use super::ast::*;
use super::plan::ExecutionPlan;

// ── Result types ──────────────────────────────────────────────────────────────

/// A single row of query results: variable name → term ID.
pub type Binding = HashMap<String, TermId>;

/// A column-oriented result set for efficient processing.
pub struct ResultSet {
    pub variables: Vec<String>,
    /// Row-major: rows[i][j] = value of variable j in row i.
    pub rows: Vec<Vec<Option<TermId>>>,
    /// Set to true when the result was truncated due to `QueryConfig::max_intermediate_rows`.
    pub overflow: bool,
}

impl ResultSet {
    pub fn empty(variables: Vec<String>) -> Self {
        Self { variables, rows: Vec::new(), overflow: false }
    }

    pub fn one_empty_row(variables: Vec<String>) -> Self {
        let n = variables.len();
        Self { variables, rows: vec![vec![None; n]], overflow: false }
    }

    pub fn variable_index(&self, name: &str) -> Option<usize> {
        self.variables.iter().position(|v| v == name)
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Leapfrog Triejoin implementation
// ══════════════════════════════════════════════════════════════════════════════

/// A sorted scan iterator over one "column" of one index, for one variable.
/// Wraps an in-memory sorted Vec for simplicity; production would use mmap slices.
struct SortedIter {
    data: Vec<TermId>,
    pos: usize,
}

impl SortedIter {
    fn new(mut data: Vec<TermId>) -> Self {
        data.sort_unstable();
        data.dedup();
        Self { data, pos: 0 }
    }

    fn current(&self) -> Option<TermId> {
        self.data.get(self.pos).copied()
    }

    fn seek(&mut self, target: TermId) {
        // Binary search from current position
        let slice = &self.data[self.pos..];
        match slice.binary_search(&target) {
            Ok(i) | Err(i) => self.pos += i,
        }
    }

    fn advance(&mut self) {
        if self.pos < self.data.len() {
            self.pos += 1;
        }
    }

    fn is_done(&self) -> bool {
        self.pos >= self.data.len()
    }
}

/// Leapfrog join over N sorted iterators for a single variable.
/// Returns all values that appear in ALL iterators.
fn leapfrog_join(mut iters: Vec<SortedIter>) -> Vec<TermId> {
    if iters.is_empty() {
        return Vec::new();
    }
    if iters.len() == 1 {
        return iters.remove(0).data;
    }

    let mut result = Vec::new();

    // Check all start non-empty
    if iters.iter().any(|it| it.is_done()) {
        return result;
    }

    // Find initial max
    let mut max_val = iters.iter().filter_map(|it| it.current()).max().unwrap();

    'outer: loop {
        // Seek all to max_val
        for it in &mut iters {
            it.seek(max_val);
            if it.is_done() {
                break 'outer;
            }
        }

        // Check if all are now at max_val
        let all_equal = iters.iter().all(|it| it.current() == Some(max_val));

        if all_equal {
            result.push(max_val);
            // Advance all iterators past this value
            for it in &mut iters {
                it.advance();
                if it.is_done() {
                    break 'outer;
                }
            }
            // New max
            max_val = iters.iter().filter_map(|it| it.current()).max().unwrap();
        } else {
            // Update max to the new maximum after seeking
            max_val = iters.iter().filter_map(|it| it.current()).max().unwrap();
        }
    }

    result
}

// ══════════════════════════════════════════════════════════════════════════════
// Main Executor
// ══════════════════════════════════════════════════════════════════════════════

pub struct Executor<'a> {
    pub index: &'a TripleIndex,
    pub dict: &'a QueryDict,
    pub config: QueryConfig,
    /// Optional predicate statistics for join ordering.
    /// When `None`, the optimizer falls back to index-probe estimates only.
    pub stats: Option<&'a StoreStatistics>,
}

impl<'a> Executor<'a> {
    pub fn new(index: &'a TripleIndex, dict: &'a QueryDict) -> Self {
        Self { index, dict, config: QueryConfig::default(), stats: None }
    }

    pub fn with_config(index: &'a TripleIndex, dict: &'a QueryDict, config: QueryConfig) -> Self {
        Self { index, dict, config, stats: None }
    }

    pub fn with_config_and_stats(
        index: &'a TripleIndex,
        dict: &'a QueryDict,
        config: QueryConfig,
        stats: Option<&'a StoreStatistics>,
    ) -> Self {
        Self { index, dict, config, stats }
    }

    /// Execute a full query and return results as a ResultSet.
    pub fn execute_select(&self, query: &SelectQuery) -> ResultSet {
        // 1. Optimize the join order
        let plan = optimize_bgp(&query.pattern, self.index, self.dict, self.stats);

        // 2. Execute
        let mut bindings = self.execute_plan(&plan);

        // Short-circuit: if execution was truncated, return immediately with the
        // overflow flag set so the caller can return an error to the client.
        if bindings.overflow {
            return bindings;
        }

        // 3. Apply GROUP BY + aggregates if present
        if !query.group_by.is_empty() {
            bindings = self.apply_group_by(&bindings, query);
        }

        // 4. Apply HAVING
        for having in &query.having {
            bindings.rows.retain(|row| {
                let b = row_to_binding(&bindings.variables, row);
                self.eval_bool(having, &b).unwrap_or(false)
            });
        }

        // 5. Apply ORDER BY
        if !query.order_by.is_empty() {
            let vars = bindings.variables.clone();
            let order = query.order_by.clone();
            let dict = self.dict;
            bindings.rows.sort_by(|a, b| {
                for cond in &order {
                    let ba = row_to_binding(&vars, a);
                    let bb = row_to_binding(&vars, b);
                    let va = eval_expr_to_string(&cond.expr, &ba, dict);
                    let vb = eval_expr_to_string(&cond.expr, &bb, dict);
                    let cmp = va.cmp(&vb);
                    let cmp = if cond.direction == OrderDirection::Desc { cmp.reverse() } else { cmp };
                    if cmp != std::cmp::Ordering::Equal {
                        return cmp;
                    }
                }
                std::cmp::Ordering::Equal
            });
        }

        // 6. Apply DISTINCT before LIMIT (SPARQL 1.1 §18.2.5: DISTINCT/REDUCED are applied
        //    before slicing with OFFSET/LIMIT — doing it after would give wrong row counts).
        if query.distinct {
            bindings.rows.sort_unstable();
            bindings.rows.dedup();
        }

        // 7. OFFSET + LIMIT
        if let Some(off) = query.offset {
            if off as usize >= bindings.rows.len() {
                bindings.rows.clear();
            } else {
                bindings.rows.drain(0..off as usize);
            }
        }
        if let Some(lim) = query.limit {
            bindings.rows.truncate(lim as usize);
        }

        // 8. Project output variables
        self.project(&bindings, &query.projection)
    }

    pub fn execute_ask(&self, query: &AskQuery) -> bool {
        let plan = optimize_bgp(&query.pattern, self.index, self.dict, self.stats);
        let results = self.execute_plan(&plan);
        !results.rows.is_empty()
    }

    // ── Pattern execution ─────────────────────────────────────────────────────

    /// Execute a plan with no outer binding context.
    fn execute_plan(&self, plan: &ExecutionPlan) -> ResultSet {
        self.execute_plan_with_ctx(plan, &HashMap::new())
    }

    /// Execute a plan, substituting any variables present in `outer` as constants.
    ///
    /// This is the key to bind-join (also called "index nested-loop join"):
    /// when the left side of a join is small, we re-execute the right plan for
    /// each left-side row with its variable bindings substituted as constants.
    /// This turns a full table scan into a targeted index probe.
    ///
    /// Example: VALUES ?s { <Q9NYF8> } followed by ?s ?p ?o
    ///   Without ctx: full scan of all triples → millions of rows in RAM
    ///   With ctx {?s: <Q9NYF8>}: index probe (S=Q9NYF8, P=*, O=*) → ~50 rows
    fn execute_plan_with_ctx(&self, plan: &ExecutionPlan, outer: &Binding) -> ResultSet {
        match plan {
            ExecutionPlan::Empty => ResultSet::one_empty_row(Vec::new()),
            ExecutionPlan::Scan { pattern, variables } => {
                self.execute_scan(pattern, variables)
            }
            ExecutionPlan::Join(left, right) => {
                let left_rs = self.execute_plan_with_ctx(left, outer);
                if left_rs.overflow { return left_rs; }

                let needs_binding = plan_needs_outer_binding(right, &left_rs.variables);

                // Subqueries are self-contained: they ignore the outer binding and always
                // return the same rows.  Using bind_join would re-execute the subquery for
                // every left row and produce a cross-product (shared variables are never
                // substituted into the subquery).  Instead, execute once and hash_join.
                if matches!(right.as_ref(), ExecutionPlan::Subquery(_)) {
                    let right_rs = self.execute_plan_with_ctx(right, outer);
                    if right_rs.overflow { return right_rs; }
                    return self.hash_join(left_rs, right_rs);
                }

                // If the right plan contains ScanAsts that reference variables produced
                // by the left side, bind_join MUST be used regardless of left size.
                if needs_binding {
                    return self.bind_join(left_rs, right, outer);
                }

                // Right side is independent of left variables → hash_join is fine.
                if left_rs.rows.len() <= self.config.bind_join_threshold {
                    return self.bind_join(left_rs, right, outer);
                }
                let right_rs = self.execute_plan_with_ctx(right, outer);
                if right_rs.overflow { return right_rs; }
                self.hash_join(left_rs, right_rs)
            }
            ExecutionPlan::LeapfrogJoin { patterns } => {
                self.execute_leapfrog_join(patterns)
            }
            ExecutionPlan::Optional(main, opt) => {
                let main_rs = self.execute_plan_with_ctx(main, outer);
                if main_rs.overflow { return main_rs; }
                if main_rs.rows.len() <= self.config.bind_join_threshold {
                    // bind_left_join: probe opt per main row, directly building
                    // the LEFT JOIN result.  This avoids the duplicate-probe bug
                    // that the old "collect combined + left_join" approach had:
                    // when two main rows share the same ?o value, the old code
                    // inserted opt rows twice into combined, causing left_join to
                    // produce 2× too many output rows (and eventually OOM).
                    return self.bind_left_join(main_rs, opt, outer);
                }
                let opt_rs = self.execute_plan_with_ctx(opt, outer);
                if opt_rs.overflow { return opt_rs; }
                self.left_join(main_rs, opt_rs)
            }
            ExecutionPlan::Union(left, right) => {
                let mut left_rs = self.execute_plan_with_ctx(left, outer);
                if left_rs.overflow { return left_rs; }
                let right_rs = self.execute_plan_with_ctx(right, outer);
                // Propagate overflow from right branch
                let right_overflow = right_rs.overflow;
                let right_vars = right_rs.variables.clone();
                // Merge columns
                for row in right_rs.rows {
                    // Re-align row to left_rs.variables
                    let mut new_row = vec![None; left_rs.variables.len()];
                    for (j, var) in right_vars.iter().enumerate() {
                        if let Some(i) = left_rs.variable_index(var) {
                            new_row[i] = row.get(j).copied().flatten();
                        }
                    }
                    left_rs.rows.push(new_row);
                }
                if right_overflow { left_rs.overflow = true; }
                left_rs
            }
            ExecutionPlan::Filter(inner, expr) => {
                let mut rs = self.execute_plan_with_ctx(inner, outer);
                let vars = rs.variables.clone();
                rs.rows.retain(|row| {
                    let b = row_to_binding(&vars, row);
                    self.eval_bool(expr, &b).unwrap_or(false)
                });
                rs
            }
            ExecutionPlan::Extend(inner, expr, var) => {
                let mut rs = self.execute_plan_with_ctx(inner, outer);
                rs.variables.push(var.clone());
                let vars = rs.variables[..rs.variables.len()-1].to_vec();
                for row in &mut rs.rows {
                    let b = row_to_binding(&vars, row);
                    let val = self.eval_term(expr, &b);
                    row.push(val);
                }
                rs
            }
            ExecutionPlan::Values(vc) => {
                self.execute_values(vc)
            }
            ExecutionPlan::PathPattern { s, path, o } => {
                // Substitute outer bindings into path endpoints
                let s_term = self.substitute_term(s, outer);
                let o_term = self.substitute_term(o, outer);
                self.execute_path_pattern(&s_term, path, &o_term)
            }
            ExecutionPlan::Subquery(sq) => {
                // Execute the subquery as a fully self-contained SELECT:
                // its DISTINCT / GROUP BY / ORDER BY / LIMIT are applied before
                // the result is handed back to the outer query as a plain ResultSet.
                // This is the correct SPARQL 1.1 semantics for { SELECT … } subqueries.
                self.execute_select(sq)
            }
            ExecutionPlan::NamedGraph { graph, inner } => {
                self.execute_named_graph(graph, inner)
            }
            // ScanAst: encode AST-level triple pattern into dictionary IDs at runtime.
            //
            // KEY CORRECTNESS RULE: if a *constant* term (IRI / literal) is not in the
            // dictionary, no triple can possibly match → return empty immediately.
            // Using UNBOUND as a fallback would turn the constant into a wildcard and
            // return spurious results (e.g. all objects when the predicate is unknown).
            //
            // BIND-JOIN RULE: if a Variable is bound in `outer`, treat it as a constant
            // (targeted index probe instead of full scan).
            ExecutionPlan::ScanAst(ast_pat) => {
                // Collect variable→position mappings for variables NOT bound in outer
                let mut variables: Vec<(String, u8)> = Vec::new();
                if let Term::Variable(v) = &ast_pat.s {
                    if !outer.contains_key(v.as_str()) { variables.push((v.clone(), 0)); }
                }
                if let Term::Variable(v) = &ast_pat.p {
                    if !outer.contains_key(v.as_str()) { variables.push((v.clone(), 1)); }
                }
                if let Term::Variable(v) = &ast_pat.o {
                    if !outer.contains_key(v.as_str()) { variables.push((v.clone(), 2)); }
                }
                let var_names = || variables.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>();

                // Encode one AST term to a TermId.
                // Variables bound in `outer` → their bound TermId (index probe, not scan).
                // Unbound variables → UNBOUND (wildcard for index scan).
                // Constants → dictionary ID, or None if absent (→ empty result).
                let encode = |term: &Term| -> Option<TermId> {
                    match term {
                        Term::Variable(v) => {
                            if let Some(&id) = outer.get(v.as_str()) {
                                Some(id) // bound from outer context → targeted probe
                            } else {
                                Some(UNBOUND) // free variable → wildcard
                            }
                        }
                        Term::Iri(iri) => self.dict.lookup(iri.as_str()),
                        Term::Literal(lit) => self.dict.lookup(&lit.to_ntriples()),
                        Term::BlankNode(b) => self.dict.lookup(b.as_str()),
                    }
                };

                let s_id = match encode(&ast_pat.s) { Some(id) => id, None => return ResultSet::empty(var_names()) };
                let p_id = match encode(&ast_pat.p) { Some(id) => id, None => return ResultSet::empty(var_names()) };
                let o_id = match encode(&ast_pat.o) { Some(id) => id, None => return ResultSet::empty(var_names()) };

                let pattern = TriplePattern::new(s_id, p_id, o_id);
                let scan_rs = self.execute_scan(&pattern, &variables);

                // If outer has variables that were substituted as constants, we must
                // re-inject them into each result row so that subsequent joins can
                // match on them.  (The scan result only has the *unbound* variables.)
                if outer.is_empty() {
                    scan_rs
                } else {
                    // Build a combined result that includes both outer-bound vars and
                    // newly scanned vars.
                    let mut all_vars = scan_rs.variables.clone();
                    let mut outer_additions: Vec<(String, TermId)> = Vec::new();
                    for (var, &id) in outer.iter() {
                        // Only include vars that belong to THIS pattern (s/p/o)
                        let in_pat =
                            matches!(&ast_pat.s, Term::Variable(v) if v == var) ||
                            matches!(&ast_pat.p, Term::Variable(v) if v == var) ||
                            matches!(&ast_pat.o, Term::Variable(v) if v == var);
                        if in_pat && !all_vars.contains(var) {
                            all_vars.push(var.clone());
                            outer_additions.push((var.clone(), id));
                        }
                    }
                    if outer_additions.is_empty() {
                        scan_rs
                    } else {
                        let mut out = ResultSet::empty(all_vars);
                        out.overflow = scan_rs.overflow;
                        for mut row in scan_rs.rows {
                            for (_, id) in &outer_additions {
                                row.push(Some(*id));
                            }
                            out.rows.push(row);
                        }
                        out
                    }
                }
            }
        }
    }

    fn execute_scan(&self, pat: &TriplePattern, variables: &[(String, u8)]) -> ResultSet {
        let var_names: Vec<String> = variables.iter().map(|(n, _)| n.clone()).collect();
        let mut rs = ResultSet::empty(var_names);

        for triple in self.index.scan(pat) {
            let mut row = vec![None; variables.len()];
            for (i, (_, pos)) in variables.iter().enumerate() {
                let val = match pos {
                    0 => triple.s,
                    1 => triple.p,
                    _ => triple.o,
                };
                row[i] = Some(val);
            }
            rs.rows.push(row);
        }
        rs
    }

    /// Core Leapfrog Triejoin for a set of triple patterns sharing variables.
    ///
    /// This is the key performance innovation over Virtuoso.
    /// For each shared variable, we collect all the sorted value lists from
    /// each pattern that binds that variable, then intersect them with leapfrog.
    fn execute_leapfrog_join(&self, patterns: &[(TriplePattern, Vec<(String, u8)>)]) -> ResultSet {
        // For the simplest case (common in bio queries): 2+ patterns sharing one variable.
        // We collect all candidate values for each shared variable via leapfrog intersection,
        // then enumerate consistent bindings.

        // Step 1: collect all variable positions across all patterns
        let mut var_positions: HashMap<String, Vec<(usize, u8)>> = HashMap::new();
        for (pi, (_, vars)) in patterns.iter().enumerate() {
            for (varname, pos) in vars {
                var_positions.entry(varname.clone()).or_default().push((pi, *pos));
            }
        }

        // Step 2: find shared variables (appear in 2+ patterns) → leapfrog intersect
        let shared_vars: Vec<_> = var_positions.iter()
            .filter(|(_, positions)| positions.len() >= 2)
            .map(|(v, _)| v.clone())
            .collect();

        // Step 3: For the first shared variable, leapfrog intersect candidate values
        // (Full multi-variable leapfrog is complex; we handle 1 shared var here,
        //  falling back to hash join for the rest)
        if shared_vars.is_empty() || patterns.len() <= 1 {
            // No sharing → fall back to hash join of individual scans
            let scan_plans: Vec<ExecutionPlan> = patterns.iter()
                .map(|(pat, vars)| ExecutionPlan::Scan { pattern: *pat, variables: vars.clone() })
                .collect();
            let mut result = self.execute_plan(&scan_plans[0]);
            for plan in &scan_plans[1..] {
                let right = self.execute_plan(plan);
                result = self.hash_join(result, right);
            }
            return result;
        }

        let join_var = &shared_vars[0];
        let positions = &var_positions[join_var];

        // Collect candidate values from the first two patterns
        let iters: Vec<SortedIter> = positions.iter().take(4).map(|(pi, pos)| {
            let pat = &patterns[*pi].0;
            let vals: Vec<TermId> = self.index.scan(pat)
                .map(|t| match pos { 0 => t.s, 1 => t.p, _ => t.o })
                .collect();
            SortedIter::new(vals)
        }).collect();

        let candidate_values = leapfrog_join(iters);

        // Step 4: for each candidate value, enumerate consistent bindings
        let all_vars: Vec<String> = {
            let mut seen = std::collections::HashSet::new();
            let mut v = Vec::new();
            for (_, vars) in patterns {
                for (name, _) in vars {
                    if seen.insert(name.clone()) {
                        v.push(name.clone());
                    }
                }
            }
            v
        };

        let mut rs = ResultSet::empty(all_vars.clone());

        for val in candidate_values {
            // For each candidate, scan each pattern with the join variable bound
            let mut partial: Vec<Binding> = vec![{
                let mut b = HashMap::new();
                b.insert(join_var.clone(), val);
                b
            }];

            for (pat, vars) in patterns {
                // Bind the join variable if it appears in this pattern
                let bound_pat = bind_pattern_with_binding(pat, vars, join_var, val);
                let scan_rs = self.execute_scan(&bound_pat, vars);

                // Hash join partial with this scan
                let scan_bindings: Vec<Binding> = scan_rs.rows.iter()
                    .map(|row| row_to_binding(&scan_rs.variables, row))
                    .collect();

                let mut new_partial = Vec::new();
                for pb in &partial {
                    for sb in &scan_bindings {
                        if let Some(merged) = merge_bindings(pb, sb) {
                            new_partial.push(merged);
                        }
                    }
                }
                partial = new_partial;
                if partial.is_empty() { break; }
            }

            // Convert bindings to rows
            for b in partial {
                let row: Vec<Option<TermId>> = all_vars.iter()
                    .map(|v| b.get(v).copied())
                    .collect();
                rs.rows.push(row);
            }
        }

        rs
    }

    fn execute_values(&self, vc: &ValuesClause) -> ResultSet {
        let mut rs = ResultSet::empty(vc.variables.clone());
        for row in &vc.rows {
            let encoded: Vec<Option<TermId>> = row.iter().map(|term| {
                term.as_ref().and_then(|t| self.encode_term(t))
            }).collect();
            rs.rows.push(encoded);
        }
        rs
    }

    // ── Join implementations ──────────────────────────────────────────────────

    /// Classic hash join: build a hash table on the smaller side, probe with the larger.
    fn hash_join(&self, left: ResultSet, right: ResultSet) -> ResultSet {
        // Find shared variables
        let shared: Vec<(usize, usize)> = left.variables.iter().enumerate()
            .filter_map(|(li, lv)| {
                right.variable_index(lv).map(|ri| (li, ri))
            })
            .collect();

        // Merge variable list
        let mut out_vars = left.variables.clone();
        for rv in &right.variables {
            if !out_vars.contains(rv) {
                out_vars.push(rv.clone());
            }
        }

        let mut result = ResultSet::empty(out_vars.clone());

        if shared.is_empty() {
            // Cross product (rare in valid SPARQL)
            for lr in &left.rows {
                for rr in &right.rows {
                    let mut row = lr.clone();
                    for (ri, rv) in right.variables.iter().enumerate() {
                        if !left.variables.contains(rv) {
                            if let Some(out_pos) = result.variable_index(rv) {
                                while row.len() <= out_pos { row.push(None); }
                                row[out_pos] = rr[ri];
                            }
                        }
                    }
                    result.rows.push(row);
                    if result.rows.len() >= self.config.max_intermediate_rows {
                        tracing::warn!(
                            rows = result.rows.len(),
                            "leapfrog_join: intermediate result exceeded limit, truncating"
                        );
                        result.overflow = true;
                        return result;
                    }
                }
            }
            return result;
        }

        // Build hash table on right side (smaller → fewer collisions)
        let mut hash: HashMap<Vec<Option<TermId>>, Vec<Vec<Option<TermId>>>> = HashMap::new();
        for rr in &right.rows {
            let key: Vec<Option<TermId>> = shared.iter().map(|(_, ri)| rr[*ri]).collect();
            hash.entry(key).or_default().push(rr.clone());
        }

        // Probe with left side
        let out_len = out_vars.len();
        for lr in &left.rows {
            let key: Vec<Option<TermId>> = shared.iter().map(|(li, _)| lr[*li]).collect();
            if let Some(matches) = hash.get(&key) {
                for rr in matches {
                    let mut row = vec![None; out_len];
                    // Fill from left
                    for (li, lv) in left.variables.iter().enumerate() {
                        if let Some(oi) = result.variable_index(lv) {
                            row[oi] = lr[li];
                        }
                    }
                    // Fill from right (non-overlapping)
                    for (ri, rv) in right.variables.iter().enumerate() {
                        if let Some(oi) = result.variable_index(rv) {
                            if row[oi].is_none() {
                                row[oi] = rr[ri];
                            }
                        }
                    }
                    result.rows.push(row);
                    if result.rows.len() >= self.config.max_intermediate_rows {
                        tracing::warn!(
                            rows = result.rows.len(),
                            "hash_join: intermediate result exceeded limit, truncating"
                        );
                        result.overflow = true;
                        return result;
                    }
                }
            }
        }
        result
    }

    /// LEFT OUTER JOIN for OPTIONAL patterns.
    fn left_join(&self, main: ResultSet, opt: ResultSet) -> ResultSet {
        let shared: Vec<(usize, usize)> = main.variables.iter().enumerate()
            .filter_map(|(li, lv)| opt.variable_index(lv).map(|ri| (li, ri)))
            .collect();

        let mut out_vars = main.variables.clone();
        for rv in &opt.variables {
            if !out_vars.contains(rv) {
                out_vars.push(rv.clone());
            }
        }

        let mut result = ResultSet::empty(out_vars.clone());
        let out_len = result.variables.len();

        // Build hash on optional side
        let mut hash: HashMap<Vec<Option<TermId>>, Vec<Vec<Option<TermId>>>> = HashMap::new();
        for rr in &opt.rows {
            let key: Vec<_> = shared.iter().map(|(_, ri)| rr[*ri]).collect();
            hash.entry(key).or_default().push(rr.clone());
        }

        for lr in &main.rows {
            let key: Vec<_> = shared.iter().map(|(li, _)| lr[*li]).collect();
            let mut matched = false;

            if let Some(matches) = hash.get(&key) {
                for rr in matches {
                    let mut row = vec![None; out_len];
                    for (li, lv) in main.variables.iter().enumerate() {
                        if let Some(oi) = result.variable_index(lv) { row[oi] = lr[li]; }
                    }
                    for (ri, rv) in opt.variables.iter().enumerate() {
                        if let Some(oi) = result.variable_index(rv) {
                            if row[oi].is_none() { row[oi] = rr[ri]; }
                        }
                    }
                    result.rows.push(row);
                    matched = true;
                    if result.rows.len() >= self.config.max_intermediate_rows {
                        tracing::warn!(
                            rows = result.rows.len(),
                            "left_join: intermediate result exceeded limit, truncating"
                        );
                        result.overflow = true;
                        return result;
                    }
                }
            }

            if !matched {
                // Include left row with NULLs for optional variables
                let mut row = vec![None; out_len];
                for (li, lv) in main.variables.iter().enumerate() {
                    if let Some(oi) = result.variable_index(lv) { row[oi] = lr[li]; }
                }
                result.rows.push(row);
                if result.rows.len() >= self.config.max_intermediate_rows {
                    tracing::warn!(
                        rows = result.rows.len(),
                        "left_join: intermediate result exceeded limit, truncating"
                    );
                    result.overflow = true;
                    return result;
                }
            }
        }
        result
    }

    // ── Bind-join (index nested-loop join) ────────────────────────────────────

    /// For each row in `left`, re-execute `right_plan` with variables from that
    /// row substituted as constants (targeted index probes).  Then hash-join the
    /// partial right result with the single left row.
    ///
    /// This is O(|left| × probe_cost) vs hash_join's O(|left| + |right|), so it
    /// is dramatically better when |right| is large but the probe produces few rows.
    fn bind_join(&self, left: ResultSet, right_plan: &ExecutionPlan, outer: &Binding) -> ResultSet {
        // Determine output variable set (union of left and right, discovered lazily)
        let mut out_vars: Vec<String> = left.variables.clone();
        let mut result = ResultSet::empty(out_vars.clone()); // will be grown below

        for left_row in &left.rows {
            // Build a binding from this left row + outer context
            let mut row_binding = outer.clone();
            for (i, var) in left.variables.iter().enumerate() {
                if let Some(Some(id)) = left_row.get(i) {
                    row_binding.insert(var.clone(), *id);
                }
            }

            // Execute right plan with this row's bindings substituted
            let right_rs = self.execute_plan_with_ctx(right_plan, &row_binding);
            // Note: even if right_rs overflowed, we still process its partial rows
            // before returning, so the error message shows a meaningful row count.
            let right_overflow = right_rs.overflow;

            // Merge output variable schema on first non-empty right result
            for rv in &right_rs.variables {
                if !out_vars.contains(rv) {
                    out_vars.push(rv.clone());
                }
            }
            // Grow result schema if needed
            if result.variables.len() < out_vars.len() {
                result.variables = out_vars.clone();
                // Pad existing rows
                let new_len = out_vars.len();
                for row in &mut result.rows {
                    row.resize(new_len, None);
                }
            }

            let out_len = result.variables.len();
            if right_rs.rows.is_empty() {
                // No match for this left row → skip (inner join semantics)
                continue;
            }
            for right_row in &right_rs.rows {
                let mut row = vec![None; out_len];
                // Fill from left
                for (li, lv) in left.variables.iter().enumerate() {
                    if let Some(oi) = result.variable_index(lv) {
                        row[oi] = *left_row.get(li).unwrap_or(&None);
                    }
                }
                // Fill from right — and check consistency on shared variables.
                //
                // Normally, ScanAst-based right plans substitute bound variables as
                // constants (targeted index probes), so they can only return rows that
                // already agree with the left row.  But for plans that ignore the outer
                // binding (e.g. Subquery, Values), the right side may return rows with
                // conflicting values for shared variables.  We must skip those rows to
                // maintain correct inner-join semantics.
                let mut consistent = true;
                for (ri, rv) in right_rs.variables.iter().enumerate() {
                    if let Some(oi) = result.variable_index(rv) {
                        let right_val = *right_row.get(ri).unwrap_or(&None);
                        match (row[oi], right_val) {
                            // Conflict: both sides have different concrete values → skip row
                            (Some(l), Some(r)) if l != r => { consistent = false; break; }
                            // Right fills an empty slot
                            (None, rv) => { row[oi] = rv; }
                            // Both None, or both same value → no-op
                            _ => {}
                        }
                    }
                }
                if !consistent { continue; }
                result.rows.push(row);
                if result.rows.len() >= self.config.max_intermediate_rows {
                    tracing::warn!(
                        rows = result.rows.len(),
                        "bind_join: intermediate result exceeded limit, truncating"
                    );
                    result.overflow = true;
                    return result;
                }
            }
            if right_overflow {
                result.overflow = true;
                return result;
            }
        }
        result
    }

    /// Substitute a Term: if it's a Variable bound in `outer`, return an Iri term
    /// with the decoded string.  Used for path pattern endpoints in bind-join context.
    fn substitute_term(&self, term: &Term, outer: &Binding) -> Term {
        match term {
            Term::Variable(v) => {
                if let Some(&id) = outer.get(v.as_str()) {
                    let s = self.dict.decode(id);
                    if s.starts_with('"') {
                        Term::Literal(Literal::plain(s))
                    } else {
                        Term::Iri(s)
                    }
                } else {
                    term.clone()
                }
            }
            _ => term.clone(),
        }
    }

    // ── bind_left_join (OPTIONAL nested-loop) ─────────────────────────────────

    /// LEFT OUTER JOIN via index probing: for each main row, execute opt_plan
    /// with that row's variables substituted as constants, then directly produce
    /// the LEFT JOIN output (no intermediate flat table, no duplicate-probe issue).
    fn bind_left_join(&self, main: ResultSet, opt_plan: &ExecutionPlan, outer: &Binding) -> ResultSet {
        let mut out_vars = main.variables.clone();
        let mut result = ResultSet::empty(out_vars.clone());
        result.overflow = main.overflow;

        for left_row in &main.rows {
            // Build binding for this main row
            let b = row_to_binding(&main.variables, left_row);
            let mut row_binding = outer.clone();
            row_binding.extend(b.iter().map(|(k, v)| (k.clone(), *v)));

            // Probe the optional side
            let partial = self.execute_plan_with_ctx(opt_plan, &row_binding);
            // Note: overflow in opt is non-fatal for OPTIONAL — we treat it as
            // "no match" for this row rather than aborting the entire query.
            // The partial results we already have are still correct.

            // Expand output schema if opt introduces new variables
            for rv in &partial.variables {
                if !out_vars.contains(rv) {
                    out_vars.push(rv.clone());
                }
            }
            if result.variables.len() < out_vars.len() {
                result.variables = out_vars.clone();
                let new_len = result.variables.len();
                for row in &mut result.rows {
                    row.resize(new_len, None);
                }
            }

            let out_len = result.variables.len();

            if partial.rows.is_empty() {
                // No opt match → keep main row, NULLs for opt-only variables
                let mut row = vec![None; out_len];
                for (li, lv) in main.variables.iter().enumerate() {
                    if let Some(oi) = result.variable_index(lv) {
                        row[oi] = *left_row.get(li).unwrap_or(&None);
                    }
                }
                result.rows.push(row);
            } else {
                // Opt matched → one output row per opt result
                for opt_row in &partial.rows {
                    let mut row = vec![None; out_len];
                    for (li, lv) in main.variables.iter().enumerate() {
                        if let Some(oi) = result.variable_index(lv) {
                            row[oi] = *left_row.get(li).unwrap_or(&None);
                        }
                    }
                    for (ri, rv) in partial.variables.iter().enumerate() {
                        if let Some(oi) = result.variable_index(rv) {
                            if row[oi].is_none() {
                                row[oi] = *opt_row.get(ri).unwrap_or(&None);
                            }
                        }
                    }
                    result.rows.push(row);
                }
            }

            if result.rows.len() >= self.config.max_intermediate_rows {
                tracing::warn!(
                    rows = result.rows.len(),
                    "bind_left_join: intermediate result exceeded limit, truncating"
                );
                result.overflow = true;
                return result;
            }
        }
        result
    }

    // ── Aggregation ───────────────────────────────────────────────────────────

    fn apply_group_by(&self, rs: &ResultSet, query: &SelectQuery) -> ResultSet {
        // Group rows by GROUP BY key
        let mut groups: HashMap<Vec<Option<TermId>>, Vec<Vec<Option<TermId>>>> = HashMap::new();

        for row in &rs.rows {
            let key: Vec<Option<TermId>> = query.group_by.iter()
                .filter_map(|gc| {
                    if let Expression::Variable(v) = &gc.expr {
                        rs.variable_index(v).map(|i| row[i])
                    } else {
                        None
                    }
                })
                .collect();
            groups.entry(key).or_default().push(row.clone());
        }

        // Determine output variables from SELECT projection
        let mut out_vars: Vec<String> = Vec::new();
        if let Projection::Variables(items) = &query.projection {
            for item in items {
                match item {
                    SelectItem::Variable(v) => out_vars.push(v.clone()),
                    SelectItem::Alias(_, name) => out_vars.push(name.clone()),
                }
            }
        }

        let mut result = ResultSet::empty(out_vars.clone());
        result.overflow = rs.overflow; // propagate truncation flag

        for (key, group_rows) in groups {
            let mut row = vec![None; out_vars.len()];
            // Fill group-by variables
            for (i, gc) in query.group_by.iter().enumerate() {
                if let Expression::Variable(v) = &gc.expr {
                    if let Some(out_i) = result.variable_index(v) {
                        row[out_i] = key.get(i).copied().flatten();
                    }
                }
            }
            // Compute aggregates
            if let Projection::Variables(items) = &query.projection {
                for (out_i, item) in items.iter().enumerate() {
                    if let SelectItem::Alias(expr, _) = item {
                        row[out_i] = self.eval_aggregate(expr, &group_rows, rs);
                    }
                }
            }
            result.rows.push(row);
        }
        result
    }

    fn eval_aggregate(&self, expr: &Expression, rows: &[Vec<Option<TermId>>], rs: &ResultSet) -> Option<TermId> {
        match expr {
            Expression::Count { expr: inner, .. } => {
                let count = if inner.is_none() {
                    rows.len()
                } else {
                    rows.iter().filter(|row| {
                        let b = row_to_binding(&rs.variables, row);
                        if let Some(inner_expr) = inner {
                            self.eval_term(inner_expr, &b).is_some()
                        } else { true }
                    }).count()
                };
                let s = format!("\"{}\"^^<http://www.w3.org/2001/XMLSchema#integer>", count);
                Some(self.dict.encode(&s))
            }
            Expression::Sum { expr, .. } => {
                let mut sum = 0.0f64;
                for row in rows {
                    let b = row_to_binding(&rs.variables, row);
                    if let Some(id) = self.eval_term(expr, &b) {
                        let sv = self.dict.decode(id);
                        if let Some(v) = parse_numeric(&sv) {
                            sum += v;
                        }
                    }
                }
                let s = format!("\"{}\"^^<http://www.w3.org/2001/XMLSchema#decimal>", sum);
                Some(self.dict.encode(&s))
            }
            Expression::Min { expr, .. } => {
                let vals: Vec<f64> = rows.iter().filter_map(|row| {
                    let b = row_to_binding(&rs.variables, row);
                    self.eval_term(expr, &b).and_then(|id| {
                        let sv = self.dict.decode(id);
                        parse_numeric(&sv)
                    })
                }).collect();
                vals.iter().cloned().reduce(f64::min).map(|v| {
                    let s = format!("\"{}\"^^<http://www.w3.org/2001/XMLSchema#decimal>", v);
                    self.dict.encode(&s)
                })
            }
            Expression::Max { expr, .. } => {
                let vals: Vec<f64> = rows.iter().filter_map(|row| {
                    let b = row_to_binding(&rs.variables, row);
                    self.eval_term(expr, &b).and_then(|id| {
                        let sv = self.dict.decode(id);
                        parse_numeric(&sv)
                    })
                }).collect();
                vals.iter().cloned().reduce(f64::max).map(|v| {
                    let s = format!("\"{}\"^^<http://www.w3.org/2001/XMLSchema#decimal>", v);
                    self.dict.encode(&s)
                })
            }
            Expression::Avg { expr, .. } => {
                let vals: Vec<f64> = rows.iter().filter_map(|row| {
                    let b = row_to_binding(&rs.variables, row);
                    self.eval_term(expr, &b).and_then(|id| {
                        let sv = self.dict.decode(id);
                        parse_numeric(&sv)
                    })
                }).collect();
                if vals.is_empty() { return None; }
                let avg = vals.iter().sum::<f64>() / vals.len() as f64;
                let s = format!("\"{}\"^^<http://www.w3.org/2001/XMLSchema#decimal>", avg);
                Some(self.dict.encode(&s))
            }
            // SAMPLE(?x) — return the first non-null binding of ?x in the group.
            Expression::Sample { expr, .. } => {
                for row in rows {
                    let b = row_to_binding(&rs.variables, row);
                    if let Some(id) = self.eval_term(expr, &b) {
                        return Some(id);
                    }
                }
                None
            }
            // GROUP_CONCAT(?x; separator="...") — concatenate all values.
            Expression::GroupConcat { expr, separator, .. } => {
                let sep = separator.as_deref().unwrap_or(" ");
                let parts: Vec<String> = rows.iter().filter_map(|row| {
                    let b = row_to_binding(&rs.variables, row);
                    self.eval_string(expr, &b)
                }).collect();
                if parts.is_empty() { return None; }
                let joined = parts.join(sep);
                Some(self.dict.encode(&format!("\"{}\"", joined)))
            }
            _ => None,
        }
    }

    // ── Expression evaluation ─────────────────────────────────────────────────

    fn eval_bool(&self, expr: &Expression, binding: &Binding) -> Option<bool> {
        match expr {
            Expression::And(a, b) =>
                Some(self.eval_bool(a, binding)? && self.eval_bool(b, binding)?),
            Expression::Or(a, b) =>
                Some(self.eval_bool(a, binding).unwrap_or(false) || self.eval_bool(b, binding).unwrap_or(false)),
            Expression::Not(a) => Some(!self.eval_bool(a, binding)?),
            Expression::Bound(v) => Some(binding.contains_key(v.as_str())),
            Expression::Eq(a, b) => {
                // Fast path: same TermId → definitely equal.
                // Fallback: compare decoded strings (handles literals from FILTER
                // that are absent from the data dictionary).
                match (self.eval_term(a, binding), self.eval_term(b, binding)) {
                    (Some(va), Some(vb)) => Some(va == vb || self.dict.decode(va) == self.dict.decode(vb)),
                    _ => {
                        let sa = self.eval_string(a, binding)?;
                        let sb = self.eval_string(b, binding)?;
                        Some(sa == sb)
                    }
                }
            }
            Expression::Ne(a, b) => {
                match (self.eval_term(a, binding), self.eval_term(b, binding)) {
                    (Some(va), Some(vb)) => {
                        let da = self.dict.decode(va);
                        let db = self.dict.decode(vb);
                        Some(va != vb && da != db)
                    }
                    _ => {
                        let sa = self.eval_string(a, binding)?;
                        let sb = self.eval_string(b, binding)?;
                        Some(sa != sb)
                    }
                }
            }
            Expression::Lt(a, b) => self.compare_terms(a, b, binding, |o| o.is_lt()),
            Expression::Le(a, b) => self.compare_terms(a, b, binding, |o| o.is_le()),
            Expression::Gt(a, b) => self.compare_terms(a, b, binding, |o| o.is_gt()),
            Expression::Ge(a, b) => self.compare_terms(a, b, binding, |o| o.is_ge()),
            Expression::Regex(text, pattern, flags) => {
                let t = self.eval_string(text, binding)?;
                let p = self.eval_string(pattern, binding)?;
                let flag_str = flags.as_ref()
                    .and_then(|f| self.eval_string(f, binding))
                    .unwrap_or_default();
                let case_insensitive = flag_str.contains('i');
                let pat = if case_insensitive {
                    format!("(?i){}", p)
                } else {
                    p
                };
                regex::Regex::new(&pat).ok().map(|re| re.is_match(&t))
            }
            Expression::LangMatches(lang, pattern) => {
                let l = self.eval_string(lang, binding)?;
                let p = self.eval_string(pattern, binding)?;
                Some(if p == "*" { !l.is_empty() } else { l.to_ascii_lowercase().starts_with(&p.to_ascii_lowercase()) })
            }
            Expression::IsIri(e) => {
                let id = self.eval_term(e, binding)?;
                let s = self.dict.decode(id);
                Some(s.starts_with("http://") || s.starts_with("https://") || s.starts_with('<'))
            }
            Expression::IsLiteral(e) => {
                let id = self.eval_term(e, binding)?;
                let s = self.dict.decode(id);
                Some(s.starts_with('"'))
            }
            Expression::IsBlank(e) => {
                let id = self.eval_term(e, binding)?;
                let s = self.dict.decode(id);
                Some(s.starts_with("_:"))
            }
            Expression::Contains(a, b) => {
                let sa = self.eval_string(a, binding)?;
                let sb = self.eval_string(b, binding)?;
                Some(sa.contains(sb.as_str()))
            }
            Expression::StrStarts(a, b) => {
                let sa = self.eval_string(a, binding)?;
                let sb = self.eval_string(b, binding)?;
                Some(sa.starts_with(sb.as_str()))
            }
            Expression::StrEnds(a, b) => {
                let sa = self.eval_string(a, binding)?;
                let sb = self.eval_string(b, binding)?;
                Some(sa.ends_with(sb.as_str()))
            }
            Expression::SameTerm(a, b) => {
                let va = self.eval_term(a, binding)?;
                let vb = self.eval_term(b, binding)?;
                Some(va == vb)
            }
            _ => {
                // For numeric expressions, non-zero = true
                self.eval_term(expr, binding).and_then(|id| {
                    parse_numeric(&self.dict.decode(id)).map(|n| n != 0.0)
                })
            }
        }
    }

    fn compare_terms<F: Fn(std::cmp::Ordering) -> bool>(
        &self, a: &Expression, b: &Expression, binding: &Binding, check: F
    ) -> Option<bool> {
        // Get string form without requiring dict presence (handles FILTER literals).
        let sa = self.eval_string(a, binding)?;
        let sb = self.eval_string(b, binding)?;
        // Try numeric comparison first
        if let (Some(na), Some(nb)) = (parse_numeric(&sa), parse_numeric(&sb)) {
            return Some(check(na.partial_cmp(&nb)?));
        }
        // Fall back to string comparison
        Some(check(sa.as_str().cmp(sb.as_str())))
    }

    fn eval_string(&self, expr: &Expression, binding: &Binding) -> Option<String> {
        match expr {
            // STR(term) — return the lexical form directly without going through
            // dict lookup (which would fail for IRIs turned into literal form).
            Expression::Str(inner) => {
                let id = self.eval_term(inner, binding)?;
                let s = self.dict.decode(id);
                Some(extract_literal_value(&s))
            }
            // Literal used directly in an expression (e.g. the regex pattern).
            // The literal may only appear in the FILTER clause and therefore
            // never be inserted into the data dictionary.
            Expression::Literal(lit) => Some(lit.value.clone()),
            // Everything else: decode from dictionary.
            _ => {
                let id = self.eval_term(expr, binding)?;
                let s = self.dict.decode(id);
                Some(extract_literal_value(&s))
            }
        }
    }

    fn eval_term(&self, expr: &Expression, binding: &Binding) -> Option<TermId> {
        match expr {
            Expression::Variable(v) => binding.get(v.as_str()).copied(),
            Expression::Literal(lit) => {
                let s = lit.to_ntriples();
                self.dict.lookup(&s)
            }
            Expression::Iri(iri) => self.dict.lookup(iri.as_str()),
            Expression::Str(e) => {
                let id = self.eval_term(e, binding)?;
                let s = self.dict.decode(id);
                // STR(term) → plain xsd:string literal containing the lexical form.
                // For IRIs, extract_literal_value returns the IRI string as-is
                // (stored without <> in the dict), so plain = "\"http://...\"".
                // We encode it so the result is a proper literal TermId, not an IRI.
                let plain = format!("\"{}\"", extract_literal_value(&s));
                Some(self.dict.encode(&plain))
            }
            Expression::Lang(e) => {
                let id = self.eval_term(e, binding)?;
                let s = self.dict.decode(id);
                let lang = extract_lang_tag(&s).unwrap_or_default().to_string();
                Some(self.dict.encode(&format!("\"{}\"", lang)))
            }
            Expression::Datatype(e) => {
                let id = self.eval_term(e, binding)?;
                let s = self.dict.decode(id);
                let dt = extract_datatype(&s)
                    .unwrap_or("http://www.w3.org/2001/XMLSchema#string")
                    .to_string();
                self.dict.lookup(&dt)
            }
            Expression::UCase(e) => {
                let s = self.eval_string(e, binding)?.to_ascii_uppercase();
                Some(self.dict.encode(&format!("\"{}\"", s)))
            }
            Expression::LCase(e) => {
                let s = self.eval_string(e, binding)?.to_ascii_lowercase();
                Some(self.dict.encode(&format!("\"{}\"", s)))
            }
            Expression::Concat(args) => {
                let parts: Option<Vec<String>> = args.iter().map(|a| self.eval_string(a, binding)).collect();
                let joined = parts?.join("");
                Some(self.dict.encode(&format!("\"{}\"", joined)))
            }
            Expression::If(cond, then, else_) => {
                if self.eval_bool(cond, binding).unwrap_or(false) {
                    self.eval_term(then, binding)
                } else {
                    self.eval_term(else_, binding)
                }
            }
            Expression::Coalesce(args) => {
                args.iter().find_map(|a| self.eval_term(a, binding))
            }
            Expression::Add(a, b) => self.numeric_op(a, b, binding, |x, y| x + y),
            Expression::Sub(a, b) => self.numeric_op(a, b, binding, |x, y| x - y),
            Expression::Mul(a, b) => self.numeric_op(a, b, binding, |x, y| x * y),
            Expression::Div(a, b) => self.numeric_op(a, b, binding, |x, y| x / y),
            Expression::Abs(e) => self.numeric_unary(e, binding, |x| x.abs()),
            Expression::Round(e) => self.numeric_unary(e, binding, |x| x.round()),
            Expression::Ceil(e) => self.numeric_unary(e, binding, |x| x.ceil()),
            Expression::Floor(e) => self.numeric_unary(e, binding, |x| x.floor()),
            _ => None,
        }
    }

    fn numeric_op<F: Fn(f64, f64) -> f64>(
        &self, a: &Expression, b: &Expression, binding: &Binding, f: F
    ) -> Option<TermId> {
        let va = self.eval_term(a, binding)?;
        let vb = self.eval_term(b, binding)?;
        let sa = self.dict.decode(va);
        let sb = self.dict.decode(vb);
        let na = parse_numeric(&sa)?;
        let nb = parse_numeric(&sb)?;
        let result = f(na, nb);
        let s = format!("\"{}\"^^<http://www.w3.org/2001/XMLSchema#decimal>", result);
        Some(self.dict.encode(&s))
    }

    fn numeric_unary<F: Fn(f64) -> f64>(
        &self, e: &Expression, binding: &Binding, f: F
    ) -> Option<TermId> {
        let id = self.eval_term(e, binding)?;
        let sv = self.dict.decode(id);
        let n = parse_numeric(&sv)?;
        let s = format!("\"{}\"^^<http://www.w3.org/2001/XMLSchema#decimal>", f(n));
        Some(self.dict.encode(&s))
    }

    fn encode_term(&self, term: &Term) -> Option<TermId> {
        match term {
            Term::Iri(iri) => self.dict.lookup(iri),
            Term::Literal(lit) => self.dict.lookup(&lit.to_ntriples()),
            _ => None,
        }
    }

    // ── Named graph execution (GRAPH clause) ─────────────────────────────────

    /// Execute a GRAPH clause.
    ///
    /// `GRAPH <iri>  { … }` — restrict to one named graph.
    /// `GRAPH ?var   { … }` — iterate over all named graphs, bind ?var.
    fn execute_named_graph(&self, graph: &Term, inner: &ExecutionPlan) -> ResultSet {
        let gspo = match &self.index.gspo {
            Some(g) => g,
            // No GSPO index means the store was loaded from N-Triples only.
            // Return empty to signal no named graphs are available.
            None => return ResultSet::empty(self.plan_vars(inner)),
        };

        match graph {
            Term::Iri(iri) => {
                let g_id = match self.dict.lookup(iri) {
                    Some(id) => id,
                    None => return ResultSet::empty(self.plan_vars(inner)),
                };
                self.execute_in_graph(inner, g_id, gspo)
            }
            Term::Variable(var) => {
                // Enumerate every named graph and union the results.
                let graph_ids = gspo.graphs();
                let mut combined = ResultSet::empty(Vec::new());
                for g_id in graph_ids {
                    let mut rs = self.execute_in_graph(inner, g_id, gspo);
                    // Inject the graph variable into every row.
                    if rs.variable_index(var).is_none() {
                        rs.variables.push(var.clone());
                        for row in &mut rs.rows { row.push(Some(g_id)); }
                    }
                    combined = self.union_rs(combined, rs);
                }
                combined
            }
            _ => ResultSet::empty(self.plan_vars(inner)),
        }
    }

    /// Execute `inner` plan but redirect all index scans to the GSPO slice for `g_id`.
    fn execute_in_graph(
        &self,
        plan: &ExecutionPlan,
        g_id: TermId,
        gspo: &crate::index::GspoIndexFile,
    ) -> ResultSet {
        match plan {
            // The only plan node that actually touches the index is ScanAst.
            // All structural nodes (Join, Filter, …) just delegate recursively.
            ExecutionPlan::ScanAst(ast_pat) => {
                let mut variables: Vec<(String, u8)> = Vec::new();
                if let Term::Variable(v) = &ast_pat.s { variables.push((v.clone(), 0)); }
                if let Term::Variable(v) = &ast_pat.p { variables.push((v.clone(), 1)); }
                if let Term::Variable(v) = &ast_pat.o { variables.push((v.clone(), 2)); }
                let var_names = || variables.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>();

                let encode = |term: &Term| -> Option<TermId> {
                    match term {
                        Term::Variable(_) => Some(UNBOUND),
                        Term::Iri(iri) => self.dict.lookup(iri.as_str()),
                        Term::Literal(lit) => self.dict.lookup(&lit.to_ntriples()),
                        Term::BlankNode(b) => self.dict.lookup(b.as_str()),
                    }
                };
                let s_id = match encode(&ast_pat.s) { Some(id) => id, None => return ResultSet::empty(var_names()) };
                let p_id = match encode(&ast_pat.p) { Some(id) => id, None => return ResultSet::empty(var_names()) };
                let o_id = match encode(&ast_pat.o) { Some(id) => id, None => return ResultSet::empty(var_names()) };

                let pat = TriplePattern::new(s_id, p_id, o_id);
                let var_names = var_names();
                let mut rs = ResultSet::empty(var_names);
                for triple in gspo.scan_graph(g_id, &pat) {
                    let mut row = vec![None; variables.len()];
                    for (i, (_, pos)) in variables.iter().enumerate() {
                        row[i] = Some(match pos { 0 => triple.s, 1 => triple.p, _ => triple.o });
                    }
                    rs.rows.push(row);
                }
                rs
            }

            ExecutionPlan::Join(l, r) => {
                let lr = self.execute_in_graph(l, g_id, gspo);
                let rr = self.execute_in_graph(r, g_id, gspo);
                self.hash_join(lr, rr)
            }
            ExecutionPlan::Optional(main, opt) => {
                let mr = self.execute_in_graph(main, g_id, gspo);
                let or_ = self.execute_in_graph(opt, g_id, gspo);
                self.left_join(mr, or_)
            }
            ExecutionPlan::Union(a, b) => {
                let ar = self.execute_in_graph(a, g_id, gspo);
                let br = self.execute_in_graph(b, g_id, gspo);
                self.union_rs(ar, br)
            }
            ExecutionPlan::Filter(inner, expr) => {
                let mut rs = self.execute_in_graph(inner, g_id, gspo);
                let vars = rs.variables.clone();
                rs.rows.retain(|row| {
                    let b = row_to_binding(&vars, row);
                    self.eval_bool(expr, &b).unwrap_or(false)
                });
                rs
            }
            ExecutionPlan::Extend(inner, expr, var) => {
                let mut rs = self.execute_in_graph(inner, g_id, gspo);
                rs.variables.push(var.clone());
                let vars = rs.variables[..rs.variables.len()-1].to_vec();
                for row in &mut rs.rows {
                    let b = row_to_binding(&vars, row);
                    let val = self.eval_term(expr, &b);
                    row.push(val);
                }
                rs
            }
            // Delegate everything else to the normal executor.
            _ => self.execute_plan(plan),
        }
    }

    /// Merge two ResultSets (UNION semantics): align columns, append rows.
    fn union_rs(&self, mut left: ResultSet, right: ResultSet) -> ResultSet {
        if left.variables.is_empty() { return right; }
        if right.variables.is_empty() { return left; }
        // Add any new variables from right into left's schema.
        for rv in &right.variables {
            if !left.variables.contains(rv) {
                left.variables.push(rv.clone());
                for row in &mut left.rows { row.push(None); }
            }
        }
        for rr in right.rows {
            let mut new_row = vec![None; left.variables.len()];
            for (j, rv) in right.variables.iter().enumerate() {
                if let Some(i) = left.variable_index(rv) {
                    new_row[i] = rr.get(j).copied().flatten();
                }
            }
            left.rows.push(new_row);
        }
        left
    }

    /// Collect all variable names mentioned in a plan (best-effort, for empty results).
    fn plan_vars(&self, plan: &ExecutionPlan) -> Vec<String> {
        let mut vars: Vec<String> = Vec::new();
        self.collect_plan_vars(plan, &mut vars);
        vars
    }

    fn collect_plan_vars(&self, plan: &ExecutionPlan, out: &mut Vec<String>) {
        match plan {
            ExecutionPlan::ScanAst(p) => {
                for t in [&p.s, &p.p, &p.o] {
                    if let Term::Variable(v) = t {
                        if !out.contains(v) { out.push(v.clone()); }
                    }
                }
            }
            ExecutionPlan::Join(l, r) | ExecutionPlan::Optional(l, r)
            | ExecutionPlan::Union(l, r) => {
                self.collect_plan_vars(l, out);
                self.collect_plan_vars(r, out);
            }
            ExecutionPlan::Filter(i, _) | ExecutionPlan::Extend(i, _, _) => {
                self.collect_plan_vars(i, out);
            }
            ExecutionPlan::NamedGraph { inner, .. } => self.collect_plan_vars(inner, out),
            ExecutionPlan::Subquery(sq) => {
                // Only the variables the subquery *exposes* via SELECT are visible outside.
                match &sq.projection {
                    Projection::Wildcard => self.collect_plan_vars(
                        &optimize_bgp(&sq.pattern, self.index, self.dict, self.stats), out
                    ),
                    Projection::Variables(items) => {
                        for item in items {
                            let v = match item {
                                SelectItem::Variable(v) => v.clone(),
                                SelectItem::Alias(_, name) => name.clone(),
                            };
                            if !out.contains(&v) { out.push(v); }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // ── Property path execution ───────────────────────────────────────────────

    /// Top-level entry point for property path patterns.
    /// Handles variable/constant binding for s and o, then calls eval_path.
    fn execute_path_pattern(&self, s: &Term, path: &PropertyPath, o: &Term) -> ResultSet {
        // Collect variable names
        let s_var = if let Term::Variable(v) = s { Some(v.as_str()) } else { None };
        let o_var = if let Term::Variable(v) = o { Some(v.as_str()) } else { None };
        let mut vars: Vec<String> = Vec::new();
        if let Some(v) = s_var { vars.push(v.to_string()); }
        if let Some(v) = o_var { if !vars.contains(&v.to_string()) { vars.push(v.to_string()); } }

        // Encode constants; if a constant is absent from the dict → empty result
        let s_id = match s {
            Term::Variable(_) => None,
            t => match self.encode_term(t) {
                Some(id) => Some(id),
                None => return ResultSet::empty(vars),
            },
        };
        let o_id = match o {
            Term::Variable(_) => None,
            t => match self.encode_term(t) {
                Some(id) => Some(id),
                None => return ResultSet::empty(vars),
            },
        };

        // Evaluate the path; each pair is (s_value, o_value)
        let pairs = self.eval_path(path, s_id, o_id);

        let mut rs = ResultSet::empty(vars.clone());
        for (sid, oid) in pairs {
            let mut row = vec![None; vars.len()];
            if let Some(v) = s_var {
                if let Some(i) = rs.variable_index(v) { row[i] = Some(sid); }
            }
            if let Some(v) = o_var {
                if let Some(i) = rs.variable_index(v) { row[i] = Some(oid); }
            }
            rs.rows.push(row);
        }
        // Deduplicate (especially for * / + paths)
        rs.rows.sort_unstable();
        rs.rows.dedup();
        rs
    }

    /// Recursively evaluate a property path expression.
    /// Returns all (s, o) pairs satisfying the path given optional fixed endpoints.
    fn eval_path(&self, path: &PropertyPath, s: Option<TermId>, o: Option<TermId>) -> Vec<(TermId, TermId)> {
        match path {
            PropertyPath::Iri(iri) => {
                let p_id = match self.dict.lookup(iri) {
                    Some(id) => id,
                    None => return Vec::new(),
                };
                let pat = TriplePattern::new(s.unwrap_or(UNBOUND), p_id, o.unwrap_or(UNBOUND));
                self.index.scan(&pat).map(|t| (t.s, t.o)).collect()
            }

            PropertyPath::Inverse(inner) => {
                // Swap s and o, then swap result pairs back
                let pairs = self.eval_path(inner, o, s);
                pairs.into_iter().map(|(a, b)| (b, a)).collect()
            }

            PropertyPath::Alternative(alts) => {
                let mut pairs = Vec::new();
                for alt in alts {
                    pairs.extend(self.eval_path(alt, s, o));
                }
                pairs.sort_unstable();
                pairs.dedup();
                pairs
            }

            PropertyPath::Sequence(steps) if steps.is_empty() => Vec::new(),

            PropertyPath::Sequence(steps) => {
                // Evaluate left-to-right; intermediate nodes are unbound
                let mut current: Vec<(TermId, TermId)> =
                    self.eval_path(&steps[0], s, if steps.len() == 1 { o } else { None });

                for (idx, step) in steps[1..].iter().enumerate() {
                    let is_last = idx == steps.len() - 2;
                    let step_o = if is_last { o } else { None };
                    // Group by the right-hand intermediate node
                    let mut by_mid: HashMap<TermId, Vec<TermId>> = HashMap::new();
                    for (a, b) in &current {
                        by_mid.entry(*b).or_default().push(*a);
                    }
                    let mut next: Vec<(TermId, TermId)> = Vec::new();
                    for (mid, srcs) in by_mid {
                        for (_, dst) in self.eval_path(step, Some(mid), step_o) {
                            for &src in &srcs {
                                next.push((src, dst));
                            }
                        }
                    }
                    current = next;
                }
                current
            }

            PropertyPath::ZeroOrMore(inner) => self.eval_zero_or_more(inner, s, o),
            PropertyPath::OneOrMore(inner)  => self.eval_one_or_more(inner, s, o),

            PropertyPath::ZeroOrOne(inner) => {
                let mut pairs = Vec::new();
                // Zero hops = identity
                match (s, o) {
                    (Some(sid), Some(oid)) => { if sid == oid { pairs.push((sid, oid)); } }
                    (Some(sid), None)      => { pairs.push((sid, sid)); }
                    (None, Some(oid))      => { pairs.push((oid, oid)); }
                    (None, None)           => {} // skip; full enumeration is expensive
                }
                // One hop
                pairs.extend(self.eval_path(inner, s, o));
                pairs.sort_unstable();
                pairs.dedup();
                pairs
            }
        }
    }

    /// Evaluate path* (zero or more hops).
    fn eval_zero_or_more(&self, path: &PropertyPath, s: Option<TermId>, o: Option<TermId>) -> Vec<(TermId, TermId)> {
        match (s, o) {
            (Some(sid), Some(oid)) => {
                // Zero hops: sid == oid
                if sid == oid { return vec![(sid, oid)]; }
                // One-or-more hops: BFS from sid looking for oid
                if self.path_can_reach(path, sid, oid) { vec![(sid, oid)] } else { Vec::new() }
            }
            (Some(sid), None) => {
                let mut pairs = vec![(sid, sid)]; // zero hops
                pairs.extend(self.reachable_forward(path, sid).into_iter().map(|d| (sid, d)));
                pairs.sort_unstable(); pairs.dedup();
                pairs
            }
            (None, Some(oid)) => {
                let mut pairs = vec![(oid, oid)]; // zero hops
                pairs.extend(self.reachable_backward(path, oid).into_iter().map(|src| (src, oid)));
                pairs.sort_unstable(); pairs.dedup();
                pairs
            }
            (None, None) => {
                // Full transitive closure
                let edges = self.eval_path(path, None, None);
                let mut result = self.transitive_closure_from_edges(&edges);
                // Identity pairs for all appearing nodes
                let nodes: HashSet<TermId> = edges.iter().flat_map(|(a, b)| [*a, *b]).collect();
                result.extend(nodes.iter().map(|&n| (n, n)));
                result.sort_unstable(); result.dedup();
                result
            }
        }
    }

    /// Evaluate path+ (one or more hops).
    fn eval_one_or_more(&self, path: &PropertyPath, s: Option<TermId>, o: Option<TermId>) -> Vec<(TermId, TermId)> {
        match (s, o) {
            (Some(sid), Some(oid)) => {
                if self.path_can_reach(path, sid, oid) { vec![(sid, oid)] } else { Vec::new() }
            }
            (Some(sid), None) => {
                self.reachable_forward(path, sid).into_iter().map(|d| (sid, d)).collect()
            }
            (None, Some(oid)) => {
                self.reachable_backward(path, oid).into_iter().map(|src| (src, oid)).collect()
            }
            (None, None) => {
                let edges = self.eval_path(path, None, None);
                self.transitive_closure_from_edges(&edges)
            }
        }
    }

    /// BFS forward: all nodes reachable from `start` via one or more hops.
    fn reachable_forward(&self, path: &PropertyPath, start: TermId) -> Vec<TermId> {
        let mut visited: HashSet<TermId> = HashSet::new();
        let mut frontier = vec![start];
        while !frontier.is_empty() {
            let mut next = Vec::new();
            for node in frontier {
                for (_, dst) in self.eval_path(path, Some(node), None) {
                    if visited.insert(dst) { next.push(dst); }
                }
            }
            frontier = next;
        }
        visited.into_iter().collect()
    }

    /// BFS backward: all nodes that can reach `end` via one or more hops.
    fn reachable_backward(&self, path: &PropertyPath, end: TermId) -> Vec<TermId> {
        let mut visited: HashSet<TermId> = HashSet::new();
        let mut frontier = vec![end];
        while !frontier.is_empty() {
            let mut next = Vec::new();
            for node in frontier {
                for (src, _) in self.eval_path(path, None, Some(node)) {
                    if visited.insert(src) { next.push(src); }
                }
            }
            frontier = next;
        }
        visited.into_iter().collect()
    }

    /// BFS: check whether `start` can reach `target` in one or more hops.
    fn path_can_reach(&self, path: &PropertyPath, start: TermId, target: TermId) -> bool {
        let mut visited: HashSet<TermId> = HashSet::new();
        let mut frontier = vec![start];
        while !frontier.is_empty() {
            let mut next = Vec::new();
            for node in frontier {
                for (_, dst) in self.eval_path(path, Some(node), None) {
                    if dst == target { return true; }
                    if visited.insert(dst) { next.push(dst); }
                }
            }
            frontier = next;
        }
        false
    }

    /// Compute transitive closure of a set of directed edges via BFS per source node.
    fn transitive_closure_from_edges(&self, edges: &[(TermId, TermId)]) -> Vec<(TermId, TermId)> {
        let mut adj: HashMap<TermId, Vec<TermId>> = HashMap::new();
        let mut all_nodes: HashSet<TermId> = HashSet::new();
        for &(a, b) in edges {
            adj.entry(a).or_default().push(b);
            all_nodes.insert(a);
            all_nodes.insert(b);
        }
        let mut result = Vec::new();
        for &start in &all_nodes {
            let mut visited: HashSet<TermId> = HashSet::new();
            let mut frontier = vec![start];
            while !frontier.is_empty() {
                let mut next = Vec::new();
                for node in frontier {
                    if let Some(nbrs) = adj.get(&node) {
                        for &nbr in nbrs {
                            if visited.insert(nbr) {
                                result.push((start, nbr));
                                next.push(nbr);
                            }
                        }
                    }
                }
                frontier = next;
            }
        }
        result
    }

    // ── Projection ────────────────────────────────────────────────────────────

    fn project(&self, rs: &ResultSet, proj: &Projection) -> ResultSet {
        match proj {
            Projection::Wildcard => rs.clone_shallow(),
            Projection::Variables(items) => {
                let out_vars: Vec<String> = items.iter().map(|item| match item {
                    SelectItem::Variable(v) => v.clone(),
                    SelectItem::Alias(_, name) => name.clone(),
                }).collect();
                let mut out_rs = ResultSet::empty(out_vars.clone());
                out_rs.overflow = rs.overflow; // propagate truncation flag
                for row in &rs.rows {
                    let b = row_to_binding(&rs.variables, row);
                    let out_row: Vec<Option<TermId>> = items.iter().map(|item| match item {
                        SelectItem::Variable(v) => b.get(v.as_str()).copied(),
                        SelectItem::Alias(expr, name) => {
                            // Fast path: if GROUP BY already computed this alias and stored
                            // it in the binding under its alias name (e.g. ?o_count from
                            // COUNT(?o)), use that value directly.  Re-evaluating the
                            // aggregate expression here would fail because the original
                            // grouping variables (e.g. ?o) are no longer in the binding.
                            b.get(name.as_str()).copied()
                                .or_else(|| self.eval_term(expr, &b))
                        }
                    }).collect();
                    out_rs.rows.push(out_row);
                }
                out_rs
            }
        }
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Query optimizer: cardinality-based join ordering
// ══════════════════════════════════════════════════════════════════════════════

/// Analyze a graph pattern and produce an optimized execution plan.
/// The key innovation vs Virtuoso: we use histogram-based cardinality estimation
/// to pick the best join order, dramatically reducing intermediate result sizes.
/// Collect the set of variables that a pattern will bind (output variables).
fn pattern_bound_vars(pattern: &GraphPattern) -> HashSet<String> {
    let mut vars = HashSet::new();
    collect_pattern_vars(pattern, &mut vars);
    vars
}

fn collect_pattern_vars(pattern: &GraphPattern, out: &mut HashSet<String>) {
    match pattern {
        GraphPattern::Bgp(triples) => {
            for t in triples {
                if let Term::Variable(v) = &t.s { out.insert(v.clone()); }
                if let Term::Variable(v) = &t.p { out.insert(v.clone()); }
                if let Term::Variable(v) = &t.o { out.insert(v.clone()); }
            }
        }
        GraphPattern::Values(vc) => {
            for v in &vc.variables { out.insert(v.clone()); }
        }
        GraphPattern::Join(a, b) | GraphPattern::Union(a, b) => {
            collect_pattern_vars(a, out);
            collect_pattern_vars(b, out);
        }
        GraphPattern::Optional(main, _) => collect_pattern_vars(main, out),
        GraphPattern::Filter(inner, _)
        | GraphPattern::Extend(inner, _, _)
        | GraphPattern::Graph(_, inner) => collect_pattern_vars(inner, out),
        GraphPattern::Subquery(sq) => collect_pattern_vars(&sq.pattern, out),
        GraphPattern::PathPattern { s, o, .. } => {
            if let Term::Variable(v) = s { out.insert(v.clone()); }
            if let Term::Variable(v) = o { out.insert(v.clone()); }
        }
        GraphPattern::Empty => {}
    }
}

pub fn optimize_bgp(
    pattern: &GraphPattern,
    index: &TripleIndex,
    dict: &QueryDict,
    stats: Option<&StoreStatistics>,
) -> ExecutionPlan {
    optimize_bgp_with_bound(pattern, index, dict, stats, &HashSet::new())
}

fn optimize_bgp_with_bound(
    pattern: &GraphPattern,
    index: &TripleIndex,
    dict: &QueryDict,
    stats: Option<&StoreStatistics>,
    bound: &HashSet<String>,
) -> ExecutionPlan {
    match pattern {
        GraphPattern::Bgp(triples) => {
            if triples.is_empty() {
                return ExecutionPlan::Empty;
            }
            optimize_triple_patterns(triples, index, dict, stats, bound)
        }
        GraphPattern::Join(_, _) => {
            // Flatten the entire left-deep Join sequence into a flat list of patterns,
            // then reorder so that self-contained patterns (Subquery, Values) come first.
            //
            // Example: Join(Join(BGP1, Subquery), BGP2)
            //   Flattened: [BGP1, Subquery, BGP2]
            //   Hoisted:   [Subquery, BGP1, BGP2]
            //   Plan tree: Join(Join(Subquery, BGP1_with_?parent_bound), BGP2_with_all_bound)
            //
            // Without hoisting, Subquery ends up on the right side of a large left result
            // and the executor falls back to hash_join, executing right patterns with an
            // empty outer context → unconstrained full scans → cross-products / OOM.
            let mut flat: Vec<&GraphPattern> = Vec::new();
            collect_join_patterns(pattern, &mut flat);

            // Self-contained patterns first (they provide binding to subsequent patterns).
            let mut ordered: Vec<&GraphPattern> = Vec::new();
            for p in &flat {
                if matches!(p, GraphPattern::Subquery(_) | GraphPattern::Values(_)) {
                    ordered.push(p);
                }
            }
            for p in &flat {
                if !matches!(p, GraphPattern::Subquery(_) | GraphPattern::Values(_)) {
                    ordered.push(p);
                }
            }

            // Build optimized plans left-to-right, growing the bound-variable set.
            let mut current_bound = bound.clone();
            let mut plans: Vec<ExecutionPlan> = Vec::with_capacity(ordered.len());
            for p in ordered {
                plans.push(optimize_bgp_with_bound(p, index, dict, stats, &current_bound));
                current_bound.extend(pattern_bound_vars(p));
            }

            // Fold into a left-deep Join tree.
            plans.into_iter()
                .reduce(|acc, p| ExecutionPlan::Join(Box::new(acc), Box::new(p)))
                .unwrap_or(ExecutionPlan::Empty)
        }
        GraphPattern::Optional(main, opt) => {
            let main_vars = pattern_bound_vars(main);
            let mut opt_bound = bound.clone();
            opt_bound.extend(main_vars);

            let mp = optimize_bgp_with_bound(main, index, dict, stats, bound);
            let op = optimize_bgp_with_bound(opt, index, dict, stats, &opt_bound);
            ExecutionPlan::Optional(Box::new(mp), Box::new(op))
        }
        GraphPattern::Union(a, b) => {
            let ap = optimize_bgp_with_bound(a, index, dict, stats, bound);
            let bp = optimize_bgp_with_bound(b, index, dict, stats, bound);
            ExecutionPlan::Union(Box::new(ap), Box::new(bp))
        }
        GraphPattern::Filter(inner, expr) => {
            let ip = optimize_bgp_with_bound(inner, index, dict, stats, bound);
            ExecutionPlan::Filter(Box::new(ip), expr.clone())
        }
        GraphPattern::Extend(inner, expr, var) => {
            let ip = optimize_bgp_with_bound(inner, index, dict, stats, bound);
            ExecutionPlan::Extend(Box::new(ip), expr.clone(), var.clone())
        }
        GraphPattern::Values(vc) => {
            ExecutionPlan::Values(vc.clone())
        }
        GraphPattern::Subquery(sq) => {
            // A subquery is a self-contained SELECT that must be executed as a unit:
            // its own SELECT projection, DISTINCT, GROUP BY, ORDER BY, LIMIT/OFFSET are
            // all applied *inside* the subquery before its results join with the outer query.
            // We do NOT recurse into sq.pattern here — the Executor will call execute_select.
            ExecutionPlan::Subquery(sq.clone())
        }
        GraphPattern::Graph(graph_term, inner) => {
            let inner_plan = optimize_bgp_with_bound(inner, index, dict, stats, bound);
            ExecutionPlan::NamedGraph {
                graph: graph_term.clone(),
                inner: Box::new(inner_plan),
            }
        }
        GraphPattern::PathPattern { s, path, o } => {
            ExecutionPlan::PathPattern { s: s.clone(), path: path.clone(), o: o.clone() }
        }
        GraphPattern::Empty => ExecutionPlan::Empty,
    }
}

/// Estimate cardinality for a triple pattern at optimization time.
///
/// ## Two-tier strategy
///
/// **Tier 1 — index probing** (always applied):
/// Constant IRIs and literals in the pattern are encoded to TermIds via dictionary
/// lookup.  `TripleIndex::estimate()` performs a binary-search range count over the
/// best-matching sorted index.  This gives exact-to-near-exact counts for patterns
/// that have at least one constant in the leading key positions.
///
/// **Tier 2 — predicate statistics** (applied when `stats` is `Some`):
/// When the predicate is a constant but subject or object are variables, index
/// probing returns the full predicate fanout.  `StoreStatistics::estimate()` uses
/// pre-computed per-predicate subject/object counts to model SP and PO fanout,
/// giving much better estimates for `?s pred ?o`-style patterns.
///
/// **Bound variable discount**:
/// Variables already bound by earlier patterns in the join will be substituted at
/// runtime, sharply reducing result count.  Each such position reduces the estimate
/// by `BOUND_VAR_FACTOR`.  This is a deliberate underestimate — we prefer to plan
/// as if the pattern is cheap rather than generating a cross product.
fn estimate_pattern_cardinality(
    t: &TriplePatternAst,
    bound: &HashSet<String>,
    index: &TripleIndex,
    dict: &QueryDict,
    stats: Option<&StoreStatistics>,
) -> u64 {
    // Encode constant (non-variable) terms to TermIds for index probing.
    // Returns None for variables and for constants not (yet) in the dictionary.
    let resolve_const = |term: &Term| -> Option<TermId> {
        match term {
            Term::Variable(_) | Term::BlankNode(_) => None,
            Term::Iri(iri)     => dict.lookup(iri.as_str()),
            Term::Literal(lit) => dict.lookup(&lit.to_ntriples()),
        }
    };

    let s_id = resolve_const(&t.s);
    let p_id = resolve_const(&t.p);
    let o_id = resolve_const(&t.o);

    let is_bound_var = |term: &Term| -> bool {
        matches!(term, Term::Variable(v) if bound.contains(v.as_str()))
    };

    // ── Base estimate ─────────────────────────────────────────────────────────
    //
    // Tier 2 (stats) is preferred when the predicate is a known constant.
    // It models bound-variable positions via SP/PO/SPO match arms, so it already
    // accounts for the selectivity introduced by a bound variable — no further
    // discount is applied.
    //
    // Tier 1 (index probe) is used when no stats are available or the predicate is
    // a variable.  The index estimate ignores bound-variable context, so we apply
    // a conservative discount for each bound-variable position.
    let est: u64 = if let (Some(stats), Some(p)) = (stats, p_id) {
        // ── Tier 2: predicate-aware statistics ───────────────────────────────
        // The stats.estimate() match arms map (s_opt, Some(p), o_opt) to:
        //   SP  → triple_count / subject_count  (avg objects per subject)
        //   PO  → triple_count / object_count   (avg subjects per object)
        //   SPO → 1  (direct triple check)
        //   P   → total triples for predicate
        // This is already conditioned on the bound positions, so no extra discount.
        let s_opt = if s_id.is_some() || is_bound_var(&t.s) { Some(0u64) } else { None };
        let o_opt = if o_id.is_some() || is_bound_var(&t.o) { Some(0u64) } else { None };
        stats.estimate(s_opt, Some(p), o_opt)
    } else {
        // ── Tier 1: index binary-search range count ───────────────────────────
        // Build a TriplePattern with the constant positions encoded and variables
        // as UNBOUND.  estimate() does a binary search over the sorted index.
        let pat = TriplePattern {
            s: s_id.unwrap_or(UNBOUND),
            p: p_id.unwrap_or(UNBOUND),
            o: o_id.unwrap_or(UNBOUND),
        };
        let mut raw = index.estimate(&pat);

        // ── Bound-variable discount ───────────────────────────────────────────
        // The index estimate for a variable position assumes a full-range scan.
        // A bound variable will be probed once per outer row, so the actual
        // contribution to the result is much smaller.  Divide by BOUND_VAR_FACTOR
        // for each bound-variable position to model this selectivity.
        // This is intentionally conservative — we prefer to under-estimate so
        // the optimizer places constrained patterns before unconstrained ones.
        const BOUND_VAR_FACTOR: u64 = 100;
        if s_id.is_none() && is_bound_var(&t.s) { raw = (raw / BOUND_VAR_FACTOR).max(1); }
        if p_id.is_none() && is_bound_var(&t.p) { raw = (raw / BOUND_VAR_FACTOR).max(1); }
        if o_id.is_none() && is_bound_var(&t.o) { raw = (raw / BOUND_VAR_FACTOR).max(1); }
        raw
    };

    est.max(1)
}

fn optimize_triple_patterns(
    triples: &[TriplePatternAst],
    index: &TripleIndex,
    dict: &QueryDict,
    stats: Option<&StoreStatistics>,
    initially_bound: &HashSet<String>,
) -> ExecutionPlan {
    // Greedy join ordering driven by cardinality estimation:
    //   At each step pick the remaining pattern with the lowest estimated result
    //   count given the variables already bound by previously selected patterns.
    //
    // Estimation priority (see `estimate_pattern_cardinality`):
    //   1. Index probe for constant positions (O(log N), always used)
    //   2. Predicate-fanout statistics for variable positions (when stats.bin exists)
    //   3. Bound-variable discount: each already-bound variable reduces the estimate
    //      by ×1/100 to reflect runtime substitution.
    //
    // "bound" starts with variables provided by the outer context (e.g. VALUES),
    // then grows as each pattern is selected and contributes its own variables.

    let mut remaining: Vec<&TriplePatternAst> = triples.iter().collect();
    let mut bound = initially_bound.clone();
    let mut ordered: Vec<&TriplePatternAst> = Vec::with_capacity(triples.len());

    while !remaining.is_empty() {
        // Pick the pattern with the smallest estimated cardinality.
        // `min_by_key` returns the *first* minimum on ties, preserving the original
        // parse order for equally-estimated patterns (stable, no random tie-breaking).
        let best_idx = remaining.iter().enumerate()
            .min_by_key(|(_, t)| estimate_pattern_cardinality(t, &bound, index, dict, stats))
            .map(|(i, _)| i)
            .unwrap();

        let best = remaining.remove(best_idx);
        // Add variables this pattern introduces to the bound set.
        if let Term::Variable(v) = &best.s { bound.insert(v.clone()); }
        if let Term::Variable(v) = &best.p { bound.insert(v.clone()); }
        if let Term::Variable(v) = &best.o { bound.insert(v.clone()); }
        ordered.push(best);
    }

    // Build a left-deep join tree.
    let first = ExecutionPlan::ScanAst(ordered[0].clone());
    ordered[1..].iter().fold(first, |acc, pat| {
        ExecutionPlan::Join(Box::new(acc), Box::new(ExecutionPlan::ScanAst((*pat).clone())))
    })
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Flatten a left-deep `GraphPattern::Join` tree into a flat list of leaf patterns.
/// Non-Join nodes (BGP, Subquery, Values, Optional, Union, …) are leaves.
fn collect_join_patterns<'a>(pattern: &'a GraphPattern, out: &mut Vec<&'a GraphPattern>) {
    match pattern {
        GraphPattern::Join(left, right) => {
            collect_join_patterns(left, out);
            collect_join_patterns(right, out);
        }
        p => out.push(p),
    }
}

/// Returns true if `plan` contains at least one ScanAst node whose subject,
/// predicate, or object is a variable that appears in `left_vars`.
///
/// This is used to decide between bind_join and hash_join:
/// - If true  → bind_join: the right plan benefits from having those variables
///              substituted as constants (targeted index probes).
/// - If false → hash_join: the right plan is independent of the left output and
///              can be executed fully materialised.
///
/// Without this check, a large left side (> 10K rows) would force hash_join,
/// which runs the right plan with an empty outer context.  Any ScanAst that
/// references a left variable would then become an unconstrained full scan,
/// causing cross-products and OOM.
fn plan_needs_outer_binding(plan: &ExecutionPlan, left_vars: &[String]) -> bool {
    match plan {
        ExecutionPlan::ScanAst(p) => {
            let has_var = |t: &Term| -> bool {
                if let Term::Variable(v) = t { left_vars.contains(v) } else { false }
            };
            has_var(&p.s) || has_var(&p.p) || has_var(&p.o)
        }
        ExecutionPlan::Join(l, r)
        | ExecutionPlan::Optional(l, r)
        | ExecutionPlan::Union(l, r) => {
            plan_needs_outer_binding(l, left_vars) || plan_needs_outer_binding(r, left_vars)
        }
        ExecutionPlan::Filter(inner, _) | ExecutionPlan::Extend(inner, _, _) => {
            plan_needs_outer_binding(inner, left_vars)
        }
        ExecutionPlan::NamedGraph { inner, .. } => plan_needs_outer_binding(inner, left_vars),
        // Subquery and Values are self-contained — they never look at left_vars.
        ExecutionPlan::Subquery(_) | ExecutionPlan::Values(_) => false,
        _ => false,
    }
}

fn row_to_binding(variables: &[String], row: &[Option<TermId>]) -> Binding {
    let mut b = HashMap::new();
    for (i, var) in variables.iter().enumerate() {
        if let Some(Some(id)) = row.get(i) {
            b.insert(var.clone(), *id);
        }
    }
    b
}

fn merge_bindings(a: &Binding, b: &Binding) -> Option<Binding> {
    let mut merged = a.clone();
    for (k, v) in b {
        if let Some(existing) = merged.get(k) {
            if existing != v { return None; }
        } else {
            merged.insert(k.clone(), *v);
        }
    }
    Some(merged)
}

fn bind_pattern_with_binding(
    pat: &TriplePattern,
    vars: &[(String, u8)],
    var_name: &str,
    val: TermId,
) -> TriplePattern {
    let mut p = *pat;
    for (name, pos) in vars {
        if name == var_name {
            match pos {
                0 => p.s = val,
                1 => p.p = val,
                _ => p.o = val,
            }
        }
    }
    p
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

fn extract_lang_tag(s: &str) -> Option<&str> {
    let at = s.rfind('@')?;
    Some(&s[at + 1..])
}

fn extract_datatype(s: &str) -> Option<&str> {
    let start = s.find("^^<")?;
    let rest = &s[start + 3..];
    let end = rest.find('>')?;
    Some(&rest[..end])
}

fn parse_numeric(s: &str) -> Option<f64> {
    let val = extract_literal_value(s);
    val.parse::<f64>().ok()
}

fn eval_expr_to_string(expr: &Expression, binding: &Binding, dict: &QueryDict) -> String {
    match expr {
        Expression::Variable(v) => binding.get(v.as_str())
            .map(|&id| dict.decode(id))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

impl ResultSet {
    fn clone_shallow(&self) -> Self {
        Self {
            variables: self.variables.clone(),
            rows: self.rows.clone(),
            overflow: self.overflow,
        }
    }
}

// ── Debug helpers ──────────────────────────────────────────────────────────────

/// Return a short type name for an ExecutionPlan node (no recursion).
fn plan_type_name(plan: &ExecutionPlan) -> &'static str {
    match plan {
        ExecutionPlan::Empty           => "Empty",
        ExecutionPlan::Scan { .. }     => "Scan",
        ExecutionPlan::ScanAst(_)      => "ScanAst",
        ExecutionPlan::LeapfrogJoin{..}=> "LeapfrogJoin",
        ExecutionPlan::Join(_, _)      => "Join",
        ExecutionPlan::Optional(_, _)  => "Optional",
        ExecutionPlan::Union(_, _)     => "Union",
        ExecutionPlan::Filter(_, _)    => "Filter",
        ExecutionPlan::Extend(_, _, _) => "Extend",
        ExecutionPlan::Values(_)       => "Values",
        ExecutionPlan::PathPattern{..} => "PathPattern",
        ExecutionPlan::NamedGraph{..}  => "NamedGraph",
        ExecutionPlan::Subquery(_)     => "Subquery",
    }
}

/// Recursively build a human-readable plan tree string (indented).
fn describe_plan(plan: &ExecutionPlan, indent: usize) -> String {
    let pad = "  ".repeat(indent);
    match plan {
        ExecutionPlan::ScanAst(p) => {
            let s = match &p.s { Term::Variable(v) => format!("?{}", v), Term::Iri(i) => format!("<{}>", i), _ => format!("{:?}", p.s) };
            let pp = match &p.p { Term::Variable(v) => format!("?{}", v), Term::Iri(i) => format!("<{}>", i), _ => format!("{:?}", p.p) };
            let o = match &p.o { Term::Variable(v) => format!("?{}", v), Term::Iri(i) => format!("<{}>", i), _ => format!("{:?}", p.o) };
            format!("{}ScanAst({} {} {})", pad, s, pp, o)
        }
        ExecutionPlan::Join(l, r) => {
            format!("{}Join\n{}\n{}", pad,
                describe_plan(l, indent + 1),
                describe_plan(r, indent + 1))
        }
        ExecutionPlan::Optional(l, r) => {
            format!("{}Optional\n{}\n{}", pad,
                describe_plan(l, indent + 1),
                describe_plan(r, indent + 1))
        }
        ExecutionPlan::Filter(inner, _) => {
            format!("{}Filter\n{}", pad, describe_plan(inner, indent + 1))
        }
        ExecutionPlan::Extend(inner, _, var) => {
            format!("{}Extend(?{})\n{}", pad, var, describe_plan(inner, indent + 1))
        }
        ExecutionPlan::Subquery(sq) => {
            let proj = match &sq.projection {
                Projection::Wildcard => "*".to_string(),
                Projection::Variables(items) => items.iter().map(|i| match i {
                    SelectItem::Variable(v) => format!("?{}", v),
                    SelectItem::Alias(_, n) => format!("?{}", n),
                }).collect::<Vec<_>>().join(" "),
            };
            format!("{}Subquery(SELECT {} DISTINCT={} LIMIT={:?})", pad, proj, sq.distinct, sq.limit)
        }
        ExecutionPlan::PathPattern { s, path: _, o } => {
            let sv = match s { Term::Variable(v) => format!("?{}", v), Term::Iri(i) => format!("<{}>", i), _ => "?".to_string() };
            let ov = match o { Term::Variable(v) => format!("?{}", v), Term::Iri(i) => format!("<{}>", i), _ => "?".to_string() };
            format!("{}PathPattern({} path {})", pad, sv, ov)
        }
        ExecutionPlan::Values(vc) => {
            format!("{}Values({:?})", pad, vc.variables)
        }
        other => format!("{}{}", pad, plan_type_name(other)),
    }
}
