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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::config::QueryConfig;
use crate::dict_builder::QueryDict;
use crate::index::{GspoIndexFile, TripleIndex};
use crate::path_cache::PathCache;
use crate::pred_partition::PredPartitions;
use crate::predcache::{self, PredCache};
use crate::stats::StoreStatistics;
use crate::triple::{TermId, TriplePattern, UNBOUND};
use crate::type_cache::TypeCache;
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
    /// In-RAM predicate cache for accelerating large predicate scans.
    /// `PredCache::empty()` when no cache is configured.
    pub pred_cache: PredCache,
    /// In-RAM path cache for multi-hop property paths from rdf-config.
    /// `PathCache::empty()` when no cache is configured.
    pub path_cache: PathCache,
    /// Per-class subject membership cache for `?x a SomeClass` filter patterns.
    pub type_cache: TypeCache,
    /// On-disk per-predicate sorted (S,O) partition files.
    pub pred_partitions: PredPartitions,
    /// Cancellation flag set by the query-timeout task.
    ///
    /// Checked at each bind_join inner-loop iteration.  When `true`, execution
    /// stops immediately and returns a timeout error result.
    /// Wrapped in `Arc` so the tokio timeout task can set it from async context
    /// while the blocking executor thread checks it.
    pub cancel: Arc<AtomicBool>,
    /// Threshold in bytes above which a sequential index scan triggers
    /// `madvise(MADV_DONTNEED)` after completion.  0 = disabled.
    /// Set from `server.scan_dontneed_mb`.
    pub scan_dontneed_bytes: usize,
    /// LIMIT pushdown budget: BGP execution stops once this many rows are
    /// accumulated, avoiding full-dataset scans for queries with small LIMITs.
    ///
    /// Set to `Some(limit + offset)` at the start of `execute_select` when the
    /// query has no ORDER BY / DISTINCT / GROUP BY (those require all rows).
    /// Reset to `None` for subqueries so their own LIMIT logic is independent.
    ///
    /// Uses `Cell` so it can be updated from `&self` context without changing
    /// function signatures across the recursive call tree.
    pushdown_limit: std::cell::Cell<Option<usize>>,
}

impl<'a> Executor<'a> {
    pub fn new(index: &'a TripleIndex, dict: &'a QueryDict) -> Self {
        Self { index, dict, config: QueryConfig::default(), stats: None, pred_cache: PredCache::empty(), path_cache: PathCache::empty(), type_cache: TypeCache::empty(), pred_partitions: PredPartitions::empty(), cancel: Arc::new(AtomicBool::new(false)), scan_dontneed_bytes: 0, pushdown_limit: std::cell::Cell::new(None) }
    }

    pub fn with_config(index: &'a TripleIndex, dict: &'a QueryDict, config: QueryConfig) -> Self {
        Self { index, dict, config, stats: None, pred_cache: PredCache::empty(), path_cache: PathCache::empty(), type_cache: TypeCache::empty(), pred_partitions: PredPartitions::empty(), cancel: Arc::new(AtomicBool::new(false)), scan_dontneed_bytes: 0, pushdown_limit: std::cell::Cell::new(None) }
    }

    pub fn with_config_and_stats(
        index: &'a TripleIndex,
        dict: &'a QueryDict,
        config: QueryConfig,
        stats: Option<&'a StoreStatistics>,
    ) -> Self {
        Self { index, dict, config, stats, pred_cache: PredCache::empty(), path_cache: PathCache::empty(), type_cache: TypeCache::empty(), pred_partitions: PredPartitions::empty(), cancel: Arc::new(AtomicBool::new(false)), scan_dontneed_bytes: 0, pushdown_limit: std::cell::Cell::new(None) }
    }

    /// Builder: attach a TypeCache.
    pub fn with_type_cache(mut self, cache: TypeCache) -> Self {
        self.type_cache = cache;
        self
    }

    /// Builder: attach PredPartitions.
    pub fn with_pred_partitions(mut self, parts: PredPartitions) -> Self {
        self.pred_partitions = parts;
        self
    }

    /// Builder: attach a shared cancellation flag.
    ///
    /// The timeout task sets this to `true`; the executor checks it at each
    /// bind_join iteration and aborts immediately when it fires.
    pub fn with_cancel(mut self, cancel: Arc<AtomicBool>) -> Self {
        self.cancel = cancel;
        self
    }

    /// Builder: set the sequential scan DONTNEED threshold (bytes).
    pub fn with_scan_dontneed_bytes(mut self, bytes: usize) -> Self {
        self.scan_dontneed_bytes = bytes;
        self
    }

    /// Returns `true` if the query has been cancelled (timeout fired).
    #[inline]
    fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Returns `true` if the current BGP row count has reached the pushdown limit.
    #[inline]
    fn is_limit_reached(&self, rows: usize) -> bool {
        matches!(self.pushdown_limit.get(), Some(lim) if rows >= lim)
    }

    /// Builder: attach a predicate cache.
    ///
    /// The cache is built at server startup in a background thread. Predicates
    /// cached there are served from RAM (O(log N) probe or O(N) merge-join)
    /// instead of triggering a sequential HDD scan.
    pub fn with_pred_cache(mut self, cache: PredCache) -> Self {
        self.pred_cache = cache;
        self
    }

    /// Builder: attach a path cache.
    ///
    /// The cache is built at server startup from rdf-config compound paths.
    /// Multi-hop property paths found in the cache are served from a RAM-resident
    /// sorted array instead of stepping through the index hop-by-hop.
    pub fn with_path_cache(mut self, cache: PathCache) -> Self {
        self.path_cache = cache;
        self
    }

    /// Execute a full query and return results as a ResultSet.
    ///
    /// Emits a `DEBUG` log with:
    /// - `plan_us`  : µs spent in the optimizer (`optimize_bgp`)
    /// - `bgp_us`   : µs spent executing the physical plan (index seeks + joins)
    /// - `post_us`  : µs for GROUP BY / ORDER BY / DISTINCT / LIMIT
    /// - `rows`     : final row count after all post-processing
    /// - `plan`     : the physical plan chosen by the optimizer
    pub fn execute_select(&self, query: &SelectQuery) -> ResultSet {
        // 1. Optimize the join order
        let t_plan = Instant::now();
        let plan = optimize_bgp(&query.pattern, self.index, self.dict, self.stats);
        let plan_us = t_plan.elapsed().as_micros();
        tracing::debug!(?plan, plan_us, "query plan");

        // LIMIT pushdown: propagate LIMIT into BGP execution when post-processing
        // does not require all rows (no ORDER BY / DISTINCT / GROUP BY / HAVING /
        // aggregates).  Stops joins early once `limit + offset` rows are found.
        // Reset to None after execution so subquery calls don't bleed the limit.
        let can_pushdown = query.order_by.is_empty()
            && !query.distinct
            && query.group_by.is_empty()
            && query.having.is_empty()
            && !projection_has_aggregates(query);
        if can_pushdown {
            if let Some(lim) = query.limit {
                let budget = (lim as usize).saturating_add(query.offset.unwrap_or(0) as usize);
                self.pushdown_limit.set(Some(budget));
                tracing::debug!(limit = lim, offset = ?query.offset, budget, "LIMIT pushdown enabled");
            } else {
                self.pushdown_limit.set(None);
            }
        } else {
            self.pushdown_limit.set(None);
        }

        if let Some(fast_result) = self.try_count_distinct_cross_product(query, &plan) {
            return fast_result;
        }

<<<<<<< HEAD
        if let Some(fast_result) = self.try_count_star_cross_product(query, &plan) {
            return fast_result;
        }

        if let Some(fast_result) = self.try_count_star_single_scan(query, &plan) {
            return fast_result;
        }

        // ── Cross-product guard ────────────────────────────────────────────────
        // Pure cross-product with no useful LIMIT → abort rather than scanning 100M+ rows.
        // With LIMIT the cross-product is bounded and may be intentional.
        {
            let mut cp_leaves: Vec<&ExecutionPlan> = Vec::new();
            let is_cross_product = collect_cross_product_leaves(&plan, &mut cp_leaves)
                && cp_leaves.len() >= 2;
            let limit_val = query.limit.unwrap_or(u64::MAX) as usize;
            if is_cross_product && limit_val > self.config.max_intermediate_rows {
                let out_vars = if let Projection::Variables(items) = &query.projection {
                    items.iter().map(|i| match i {
                        SelectItem::Variable(v) => v.clone(),
                        SelectItem::Alias(_, n) => n.clone(),
                    }).collect()
                } else { vec![] };
                let mut result = ResultSet::empty(out_vars);
                result.overflow = true;
                tracing::warn!(
                    leaves = cp_leaves.len(),
                    "cross-product query without LIMIT — aborting to prevent OOM"
                );
                return result;
            }
        }
        // ── end cross-product guard ────────────────────────────────────────────

=======
>>>>>>> 583087b0f936c7c64e6a487eb4dfd6a098398d26
        // 2. Execute
        let t_bgp = Instant::now();
        let mut bindings = self.execute_plan(&plan);
        self.pushdown_limit.set(None); // clear after BGP so later calls are unaffected

        // Note: SPARQL LIMIT is "at most N rows", not "exactly N rows".
        // Returning fewer rows than LIMIT is valid when filter steps after the
        // pushdown cutoff reduce the intermediate result.  A retry without
        // pushdown would process the full dataset (e.g. 10M PSMs × 7 blanknodes
        // = 70M rows) and is far too expensive for large stores.
        // We log a debug message so operators can observe the behaviour.
        if can_pushdown && !bindings.overflow {
            if let Some(lim) = query.limit {
                if bindings.rows.len() < lim as usize {
                    tracing::debug!(
                        got = bindings.rows.len(),
                        limit = lim,
                        "LIMIT pushdown: fewer rows than limit (filter reduced result); \
                         returning early — SPARQL LIMIT is a maximum, not a guarantee"
                    );
                }
            }
        }

        let bgp_us = t_bgp.elapsed().as_micros();

        // Short-circuit: if execution was truncated, return immediately with the
        // overflow flag set so the caller can return an error to the client.
        if bindings.overflow {
            return bindings;
        }

        // Log how many rows came out of the raw BGP before post-processing.
        tracing::debug!(bgp_us, rows_before_post = bindings.rows.len(), "BGP done");

        let t_post = Instant::now();

        // 3. Apply GROUP BY + aggregates if present.
        //    If GROUP BY is absent but the projection contains aggregate expressions
        //    (e.g. COUNT(*), SUM(?x)), all rows form a single implicit group — SPARQL 1.1 §11.
        if !query.group_by.is_empty() || projection_has_aggregates(query) {
            let t = Instant::now();
            bindings = self.apply_group_by(&bindings, query);
            tracing::debug!(groupby_us = t.elapsed().as_micros(), rows = bindings.rows.len(), "GROUP BY");
        }
        let bgp_rows = bindings.rows.len();

        // 4. Apply HAVING
        // After GROUP BY, aggregate expressions in HAVING (e.g. COUNT(?pe)) are no
        // longer directly evaluable because the raw variable ?pe is gone.  Rewrite
        // them to their aliased variable names from the projection before evaluating.
        if !query.having.is_empty() {
            let aliases: Vec<(Expression, String)> =
                if let Projection::Variables(items) = &query.projection {
                    items.iter().filter_map(|item| {
                        if let SelectItem::Alias(expr, name) = item {
                            Some((expr.clone(), name.clone()))
                        } else { None }
                    }).collect()
                } else { Vec::new() };

            for having in &query.having {
                let rewritten = rewrite_having_agg(having.clone(), &aliases);
                bindings.rows.retain(|row| {
                    let b = row_to_binding(&bindings.variables, row);
                    self.eval_bool(&rewritten, &b).unwrap_or(false)
                });
            }
        }

        // 5. Apply ORDER BY
        if !query.order_by.is_empty() {
            let t = Instant::now();
            let vars = bindings.variables.clone();
            let order = query.order_by.clone();
            let m = bindings.rows.len();

            // Pre-compute sort keys once per row (avoids O(M log M) eval_order_key calls).
            let keys: Vec<Vec<String>> = bindings.rows.iter().map(|row| {
                let b = row_to_binding(&vars, row);
                order.iter().map(|cond| self.eval_order_key(&cond.expr, &b)).collect()
            }).collect();

            let cmp_fn = |ai: &usize, bi: &usize| -> std::cmp::Ordering {
                for (ki, cond) in order.iter().enumerate() {
                    let va = keys[*ai].get(ki).map(|s| s.as_str()).unwrap_or("");
                    let vb = keys[*bi].get(ki).map(|s| s.as_str()).unwrap_or("");
                    let c = compare_order_keys(va, vb);
                    let c = if cond.direction == OrderDirection::Desc { c.reverse() } else { c };
                    if c != std::cmp::Ordering::Equal { return c; }
                }
                std::cmp::Ordering::Equal
            };

            // Number of rows we actually need (LIMIT + OFFSET, capped at M).
            let need = query.limit
                .map(|lim| {
                    (lim as usize)
                        .saturating_add(query.offset.unwrap_or(0) as usize)
                        .min(m)
                })
                .unwrap_or(m);

            let mut indices: Vec<usize> = (0..m).collect();

            // Partial sort when need << M (at most 1/4 of rows needed).
            let partial = need > 0 && need < m / 4;
            if partial {
                indices.select_nth_unstable_by(need - 1, |a, b| cmp_fn(a, b));
                indices[..need].sort_unstable_by(|a, b| cmp_fn(a, b));
            } else {
                indices.sort_unstable_by(|a, b| cmp_fn(a, b));
            }

            let old_rows = std::mem::take(&mut bindings.rows);
            let take = if partial { need } else { m };
            bindings.rows = indices[..take].iter().map(|&i| old_rows[i].clone()).collect();

            tracing::debug!(
                orderby_us = t.elapsed().as_micros(),
                rows_in = m,
                rows_out = bindings.rows.len(),
                partial_sort = partial,
                "ORDER BY"
            );
        }

        // 6. Project output variables (before DISTINCT so deduplication operates on
        //    the projected variables only, as required by SPARQL 1.1 §18.2.5).
        //    ORDER BY is applied before projection (step 5) so that ORDER BY can still
        //    reference non-projected variables (e.g. SELECT ?x ORDER BY ?y).
        let mut result = self.project(&bindings, &query.projection);

        // 7. Apply DISTINCT after projection (SPARQL 1.1 §18.2.5: Project → Distinct → Slice).
        //    Uses HashSet retain instead of sort+dedup to preserve ORDER BY ordering.
        if query.distinct {
            let t = Instant::now();
            let before = result.rows.len();
            let mut seen: HashSet<Vec<Option<TermId>>> = HashSet::with_capacity(before);
            result.rows.retain(|row| seen.insert(row.clone()));
            tracing::debug!(
                distinct_us = t.elapsed().as_micros(),
                rows_in = before,
                rows_out = result.rows.len(),
                "DISTINCT"
            );
        }

        // 8. OFFSET + LIMIT (after DISTINCT so row counts are correct)
        if let Some(off) = query.offset {
            if off as usize >= result.rows.len() {
                result.rows.clear();
            } else {
                result.rows.drain(0..off as usize);
            }
        }
        if let Some(lim) = query.limit {
            result.rows.truncate(lim as usize);
        }

        let post_us = t_post.elapsed().as_micros();
        tracing::debug!(
            plan_us,
            bgp_us,
            bgp_rows,
            post_us,
            rows = result.rows.len(),
            "execute_select done"
        );
        result
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
                let t_join = std::time::Instant::now();
                let left_rs = self.execute_plan_with_ctx(left, outer);
                if left_rs.overflow { return left_rs; }
                let left_us = t_join.elapsed().as_micros();

                // Log the right-plan type and left-side row count so we can
                // identify which join step is slow.
                let right_desc = match right.as_ref() {
                    ExecutionPlan::ScanAst(p) => {
                        let pred = match &p.p {
                            crate::sparql::ast::Term::Iri(iri) => {
                                let s = iri.as_str();
                                s.rsplit('/').next().unwrap_or(s).rsplit('#').next().unwrap_or(s).to_string()
                            },
                            other => format!("{other:?}"),
                        };
                        format!("ScanAst({})", pred)
                    },
                    ExecutionPlan::ScanBound { outer_vars, .. } => {
                        format!("ScanBound(outer={:?})", outer_vars.iter().map(|(v,_)| v.as_str()).collect::<Vec<_>>())
                    },
                    ExecutionPlan::PathPattern { path, .. } => {
                        format!("PathPattern({path:?})")
                    },
                    ExecutionPlan::Join(..) => "Join(nested)".to_string(),
                    other => format!("{other:?}").chars().take(40).collect(),
                };
                tracing::debug!(
                    left_rows = left_rs.rows.len(),
                    left_us,
                    right = %right_desc,
                    "Join: left done"
                );

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

                // PathPattern right plans: choose between bind_join (N targeted
                // probes) and hash_join (full sequential path scan + in-memory
                // join) based on a cost model rather than a fixed threshold.
                //
                // Cost model (cold cache, index files on HDD):
                //   bind_join cost ≈ N_groups × path_steps × 150 ms   (cold HDD SPO seek)
                //   hash_join cost ≈ first_pred_range × 200 ns         (HDD seq read ~120 MB/s)
                //
                // We estimate first_pred_range via pos_predicate_range().
                // For paths whose first predicate is unknown/starred we fall back
                // to a conservative fixed threshold of 500.
                //
                // Examples (cold HDD, 150ms/probe, 200ns/triple seq):
                //   faldo (N=508, 2-hop, pred_range=11.8M jpo:faldo:begin):
                //     bind = 508 × 2 × 150ms = 152s (observed: 161s ✓)
                //     scan = 11.8M × 200ns = 2.4s → hash_join wins
                //     actual hash_join (with filtered scan) ≈ 5s vs bind 152s → 30× faster ✓
                //   faldo (N=508, pred_range=40M uniprot:begin_position):
                //     bind = 152s  scan = 40M × 200ns = 8s → hash_join still wins (30×)
                //   rdf:type (N=239, pred_range=545M):
                //     bind = 239 × 150ms = 36s   scan = 545M × 200ns = 109s → bind_join ✓
                //
                // NOTE: scan_cost_ns only estimates step 0 (first predicate's POS range).
                // For multi-hop Sequence paths, the actual hash_join cost also includes
                // step 1+ batch_scan I/O. The filtered-subject optimization (see below)
                // reduces step 1 from a 11.6M-entry HashMap to ~508-entry, cutting
                // actual hash_join cost from ~18s to ~5s for the faldo case.
                if let ExecutionPlan::PathPattern { path, .. } = right.as_ref() {
                    // SPO seek cost: ~150 ms per group per hop (cold HDD random access).
                    // Empirical: faldo N=508, 2-hop → bind_join took 161s = 317ms/probe.
                    // With 150ms: 508 × 2 × 150ms = 152s ≈ observed.
                    const SPO_SEEK_NS: u64 = 150_000_000;
                    // Sequential POS read cost on HDD: ~200 ns per triple.
                    // HDD sequential ~120 MB/s, 24 bytes/triple → 24/120e6 = 200 ns.
                    const POS_READ_NS: u64 = 200;
                    // RAM binary-search cost when pred_cache is loaded (path NOT in path_cache).
                    // Empirical (faldo 2-hop, 11.8M pairs, 188MB): ~50ms per probe due
                    // to TLB/page-cache misses on the large sorted array.
                    const CACHE_SEEK_NS: u64 = 50_000_000;

                    let n = left_rs.rows.len() as u64;
                    let path_steps = path_step_count(path).max(1) as u64;

                    // ── Cache-aware seek cost ────────────────────────────────
                    // Priority 1: full path in path_cache
                    //   → bind_join does a binary search per group: O(log M) ≈ 2 µs.
                    //   → hash_join would clone the entire cached Vec (potentially
                    //     100s of MB) then hash-probe.  Bind_join wins decisively.
                    //
                    // Priority 2: all individual steps in pred_cache
                    //   → bind_join replaces cold HDD SPO seeks with RAM binary
                    //     searches (~2 µs vs 150 ms).
                    //
                    // Priority 3: neither → use cold-HDD SPO seek estimate.
                    let path_cached = path_in_path_cache(path, &self.dict, &self.path_cache);
                    let all_cached  = path_cached
                        || path_all_iris_cached(path, &self.dict, &self.pred_cache);

                    // Priority 0: path fully in path_cache AND N_left is small.
                    // Binary search is O(N × log M): 263 × 23 × 100ns ≈ 0.6ms.
                    // Any scan-based approach (eval_sequence_with_subject_filter or
                    // linear scan) costs O(M_cache) ≈ 11.8M iterations ≈ 600ms+.
                    // Insert BEFORE the cost-model / use_hash block so path_cache
                    // takes priority for the small-N case.
                    const PATH_CACHE_BSEARCH_THRESHOLD: u64 = 100_000;
                    if path_cached && n < PATH_CACHE_BSEARCH_THRESHOLD {
                        tracing::debug!(
                            left_rows = n,
                            cached_path = true,
                            "Join PathPattern: path_cache binary search (small N_left)"
                        );
                        return self.path_cache_merge_join(left_rs, right, outer);
                    }

                    let seek_ns = if all_cached { CACHE_SEEK_NS } else { SPO_SEEK_NS };
                    let bind_cost_ns = n * path_steps * seek_ns;

                    // Estimate scan cost using the first predicate's POS range.
                    // With pred_cache, a full scan still needs to materialise the pairs
                    // (either from HDD or from a cache clone), so we keep the HDD estimate;
                    // bind_join with cache will win anyway when seek_ns is tiny.
                    let scan_cost_ns: Option<u64> = path_first_iri(path)
                        .and_then(|iri| self.dict.lookup(iri))
                        .and_then(|pred_id| self.index.pos_predicate_range(pred_id))
                        .map(|(lo, hi)| (hi - lo) as u64 * POS_READ_NS);

                    let use_hash = match scan_cost_ns {
                        Some(scan) => {
                            tracing::debug!(
                                left_rows = n,
                                path_steps,
                                path_cached,
                                all_cached,
                                seek_ns,
                                bind_cost_us = bind_cost_ns / 1000,
                                scan_cost_us = scan / 1000,
                                "Join PathPattern: cost model"
                            );
                            scan < bind_cost_ns
                        }
                        // Unknown predicate range → fall back to conservative threshold.
                        None => n > 500,
                    };

                    if use_hash {
                        // ── Filtered-subject optimization ─────────────────────
                        // When the path is a multi-hop Sequence AND the path's
                        // subject variable is a join variable bound in left_rs,
                        // restrict step 0 output to only those subjects.
                        //
                        // Without this: step 0 emits 11.8M pairs; step 1 builds
                        // a 11.6M-entry HashMap → ~13s.
                        // With this:    step 0 emits 11.8M, filter retains 508;
                        // step 1 builds 508-entry HashMap → ~0.05s.
                        //
                        // We keep hash_join (not bind_join) because step 0 still
                        // uses the full sequential POS scan rather than N random
                        // SPO probes (508 × 150ms = 76s on cold HDD).
                        if let ExecutionPlan::PathPattern { s: path_s, path: PropertyPath::Sequence(steps), o: path_o } = right.as_ref() {
                            if steps.len() >= 2 {
                                if let Term::Variable(s_var) = path_s {
                                    if let Some(s_col) = left_rs.variable_index(s_var.as_str()) {
                                        let subjects: HashSet<TermId> = left_rs.rows.iter()
                                            .filter_map(|row| row.get(s_col).copied().flatten())
                                            .collect();
                                        // Apply the filter only when it is selective:
                                        // subjects ≤ 100K ensures HashSet build O(n) is cheap
                                        // and step 0 → step 1 reduction is meaningful.
                                        // For very large left sides, the overhead of building
                                        // the HashSet exceeds the benefit → skip to standard.
                                        const FILTER_SUBJECT_CAP: usize = 100_000;
                                        if !subjects.is_empty() && subjects.len() <= FILTER_SUBJECT_CAP {
                                            tracing::debug!(
                                                left_rows = n,
                                                unique_subjects = subjects.len(),
                                                path_steps = steps.len(),
                                                "Join PathPattern: filtered hash_join (subject filter on step 0)"
                                            );
                                            // Resolve the path object term.
                                            // o_id: Ok(None)     = variable (unbound)
                                            //        Ok(Some(id)) = constant found in dict
                                            //        Err(())      = constant NOT in dict → skip optimization
                                            let path_o_term = self.substitute_term(path_o, outer);
                                            let o_id_result: Result<Option<TermId>, ()> = match &path_o_term {
                                                Term::Variable(_) => Ok(None),
                                                t => self.encode_term(t).ok_or(()).map(Some),
                                            };
                                            if let Ok(o_id) = o_id_result {
                                            // Evaluate the sequence with the subject filter.
                                            let pairs = self.eval_sequence_with_subject_filter(
                                                steps, None, o_id, &subjects,
                                            );
                                            // Build a ResultSet from the pairs.
                                            let o_var_name = if let Term::Variable(v) = path_o {
                                                Some(v.clone())
                                            } else {
                                                None
                                            };
                                            let mut vars: Vec<String> = vec![s_var.clone()];
                                            if let Some(ref v) = o_var_name {
                                                if !vars.contains(v) { vars.push(v.clone()); }
                                            }
                                            let mut right_rs = ResultSet::empty(vars.clone());
                                            for (sid, oid) in pairs {
                                                let mut row = vec![None; vars.len()];
                                                if let Some(i) = right_rs.variable_index(s_var) {
                                                    row[i] = Some(sid);
                                                }
                                                if let Some(ref v) = o_var_name {
                                                    if let Some(i) = right_rs.variable_index(v) {
                                                        row[i] = Some(oid);
                                                    }
                                                }
                                                right_rs.rows.push(row);
                                            }
                                            right_rs.rows.sort_unstable();
                                            right_rs.rows.dedup();
                                            return self.hash_join(left_rs, right_rs);
                                            } // end if o_id_result.is_ok()
                                        }
                                    }
                                }
                            }
                        }
                        // Fall back to standard full-path hash_join.
                        tracing::debug!(
                            left_rows = n,
                            "Join PathPattern: hash_join (full path scan)"
                        );
                        let right_rs = self.execute_plan_with_ctx(right, outer);
                        if right_rs.overflow { return right_rs; }
                        return self.hash_join(left_rs, right_rs);
                    }
                    // When the full path is in path_cache, use a single linear scan
                    // (merge scan) instead of N binary searches.
                    // Empirical: 263 probes × 52ms (page-fault penalty) = 13.7s with
                    // bind_join; merge scan reads 188MB sequentially ≈ 94ms.
                    if path_cached {
                        tracing::debug!(
                            left_rows = n,
                            "Join PathPattern: path_cache merge scan"
                        );
                        return self.path_cache_merge_join(left_rs, right, outer);
                    }
                    tracing::debug!(
                        left_rows = n,
                        "Join PathPattern: bind_join (targeted probes)"
                    );
                    return self.bind_join(left_rs, right, outer);
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
                let mut right_rs = self.execute_plan_with_ctx(right, outer);
                if right_rs.overflow { return right_rs; }

                // ── SIP (Sideways Information Passing) pre-filter (改善5) ───────
                // Before hash_join, filter right_rs to only rows whose shared-variable
                // values are present in left_rs.  This is a "late SIP": the right scan
                // I/O is unchanged (sequential POS reads are already fast), but we
                // shrink the right result set before the hash-join phase, reducing
                // memory pressure and hash-probe work.
                //
                // We apply SIP only when:
                //   1. left_rs is small enough that building the filter sets is cheap
                //      (≤ SIP_MAX_LEFT_VALUES unique values per shared variable).
                //   2. right_rs is significantly larger than left_rs (otherwise the
                //      join is already cheap).
                //
                // Example: left=508 peptides, right=10M rdf:type triples.
                //   Build filter {508 peptide IDs} → filter right_rs to 508 rows
                //   before hash_join → hash_join 508×508 instead of 508×10M.
                const SIP_MAX_LEFT_VALUES: usize = 100_000;
                if left_rs.rows.len() < right_rs.rows.len() / 10 {
                    // Find shared variables (same name in both result sets).
                    let sip_pairs: Vec<(usize, usize)> = left_rs.variables.iter()
                        .enumerate()
                        .filter_map(|(li, lv)| right_rs.variable_index(lv).map(|ri| (li, ri)))
                        .collect();
                    if !sip_pairs.is_empty() {
                        // Build per-variable HashSets from left_rs.
                        let sip_filters: Vec<HashSet<TermId>> = sip_pairs.iter()
                            .map(|&(li, _)| {
                                left_rs.rows.iter()
                                    .filter_map(|row| row.get(li).copied().flatten())
                                    .collect::<HashSet<TermId>>()
                            })
                            .collect();
                        // Only apply if all filter sets are small (cheap to build + probe).
                        if sip_filters.iter().all(|fs| fs.len() <= SIP_MAX_LEFT_VALUES) {
                            let before = right_rs.rows.len();
                            right_rs.rows.retain(|row| {
                                sip_pairs.iter().zip(sip_filters.iter()).all(|(&(_, ri), fs)| {
                                    row.get(ri).and_then(|v| *v).map_or(true, |id| fs.contains(&id))
                                })
                            });
                            tracing::debug!(
                                shared_vars = sip_pairs.len(),
                                right_before = before,
                                right_after = right_rs.rows.len(),
                                reduction = before.saturating_sub(right_rs.rows.len()),
                                "SIP pre-filter applied"
                            );
                        }
                    }
                }

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
                //
                // Save and restore the outer pushdown_limit so the subquery's own
                // execute_select (which resets pushdown_limit) doesn't clear ours.
                let saved = self.pushdown_limit.get();
                let result = self.execute_select(sq);
                self.pushdown_limit.set(saved);
                result
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
                // Collect variable→position mappings for variables NOT bound in outer.
                // Term::BlankNode is handled as a fallback for any blank-node term that
                // was not converted to Term::Variable by the parser.
                let mut variables: Vec<(String, u8)> = Vec::new();
                let collect_var = |t: &Term, pos: u8, vars: &mut Vec<(String, u8)>| {
                    let name = match t {
                        Term::Variable(v) => Some(v.clone()),
                        Term::BlankNode(b) => Some(b.clone()),
                        _ => None,
                    };
                    if let Some(n) = name {
                        if !outer.contains_key(n.as_str()) { vars.push((n, pos)); }
                    }
                };
                collect_var(&ast_pat.s, 0, &mut variables);
                collect_var(&ast_pat.p, 1, &mut variables);
                collect_var(&ast_pat.o, 2, &mut variables);
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
                        // Blank nodes that reach here were not converted by the parser
                        // (e.g. from programmatic plan construction).  Treat like a free
                        // variable: check outer binding first, fall back to UNBOUND.
                        Term::BlankNode(b) => {
                            if let Some(&id) = outer.get(b.as_str()) {
                                Some(id)
                            } else {
                                Some(UNBOUND)
                            }
                        }
                        Term::Iri(iri) => self.dict.lookup(iri.as_str()),
                        Term::Literal(lit) => self.dict.lookup(&lit.to_ntriples()),
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

            // ScanBound: constants were pre-encoded at plan-compile time.
            // Runtime work is O(|outer_vars|) hash lookups — no dictionary scan.
            ExecutionPlan::ScanBound { base, free_vars, outer_vars } => {
                let mut pattern = *base;
                let mut outer_additions: Vec<(String, TermId)> = Vec::new();

                for (var, pos) in outer_vars {
                    if let Some(&id) = outer.get(var.as_str()) {
                        match pos {
                            0 => pattern.s = id,
                            1 => pattern.p = id,
                            _ => pattern.o = id,
                        }
                        outer_additions.push((var.clone(), id));
                    }
                    // If var is absent from outer (e.g. hash_join context with empty outer),
                    // the position stays UNBOUND → wildcard scan.  Correctness is preserved
                    // because the enclosing hash_join will filter incompatible rows.
                }

                let scan_rs = self.execute_scan(&pattern, free_vars);

                if outer_additions.is_empty() {
                    scan_rs
                } else {
                    // Re-inject outer-bound variables so subsequent joins can match on them.
                    let mut all_vars = scan_rs.variables.clone();
                    let mut to_add: Vec<(String, TermId)> = Vec::new();
                    for (var, id) in &outer_additions {
                        if !all_vars.contains(var) {
                            all_vars.push(var.clone());
                            to_add.push((var.clone(), *id));
                        }
                    }
                    if to_add.is_empty() {
                        scan_rs
                    } else {
                        let mut out_rs = ResultSet::empty(all_vars);
                        out_rs.overflow = scan_rs.overflow;
                        for mut row in scan_rs.rows {
                            for (_, id) in &to_add { row.push(Some(*id)); }
                            out_rs.rows.push(row);
                        }
                        out_rs
                    }
                }
            }
        }
    }

    fn execute_scan(&self, pat: &TriplePattern, variables: &[(String, u8)]) -> ResultSet {
        // Fast path: s and p are bound → binary search in pred_cache.
        //
        // Covers three sub-cases:
        //   (s=X, p=P, o=free) → find all objects for subject X under predicate P
        //   (s=X, p=P, o=Y)   → existence check: is (X, P, Y) in the store?
        //
        // pred_cache stores (subject, object) pairs sorted by (s, o), enabling:
        //   - subject range: partition_point binary search → O(log M)
        //   - existence:     binary_search(&(s, o))         → O(log M)
        //
        // For bind_join workloads (e.g. 263 pepevi × rdf:type filter ≈ 263 × 40 ms HDD):
        //   Without cache: 263 SPO seeks × ~40 ms = 10 s
        //   With cache:    263 binary searches × ~1 µs = <1 ms  (if predicate is cached)
        if pat.s != UNBOUND && pat.p != UNBOUND {
            if let Some(pairs) = self.pred_cache.get(pat.p) {
                let bound_s = pat.s;
                let lo = pairs.partition_point(|&(s, _)| s < bound_s);

                if pat.o != UNBOUND {
                    // (s=X, p=P, o=Y): existence check via binary search.
                    let bound_o = pat.o;
                    let found = pairs[lo..].iter()
                        .take_while(|&&(s, _)| s == bound_s)
                        .any(|&(_, o)| o == bound_o);
                    if !found {
                        return ResultSet::empty(variables.iter().map(|(n, _)| n.clone()).collect());
                    }
                    // Return the single matching row.
                    let var_names: Vec<String> = variables.iter().map(|(n, _)| n.clone()).collect();
                    let mut rs = ResultSet::empty(var_names);
                    let mut row = vec![None; variables.len()];
                    for (i, (_, pos)) in variables.iter().enumerate() {
                        row[i] = Some(match pos { 0 => bound_s, 1 => pat.p, _ => bound_o });
                    }
                    rs.rows.push(row);
                    return rs;
                }

                // (s=X, p=P, o=free): collect all objects for this subject.
                let var_names: Vec<String> = variables.iter().map(|(n, _)| n.clone()).collect();
                let mut rs = ResultSet::empty(var_names);
                for &(s, o) in &pairs[lo..] {
                    if s != bound_s { break; }
                    let mut row = vec![None; variables.len()];
                    for (i, (_, pos)) in variables.iter().enumerate() {
                        row[i] = Some(match pos { 0 => s, 1 => pat.p, _ => o });
                    }
                    rs.rows.push(row);
                }
                return rs;
            }
        }

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
        // Always build on the smaller side to minimise HashMap allocation cost.
        // The caller passes (left, right) by convention but we may swap them here.
        if right.rows.len() > left.rows.len() * 4 {
            // right is significantly larger: swap so we build on left, probe right.
            tracing::debug!(
                left_rows = left.rows.len(),
                right_rows = right.rows.len(),
                "hash_join: swapping sides (build on smaller left)"
            );
            return self.hash_join_impl(right, left);
        }
        self.hash_join_impl(left, right)
    }

    /// Inner hash join: build on `right`, probe with `left`.
    /// Output column order follows `left` variables first, then new `right` variables.
    fn hash_join_impl(&self, left: ResultSet, right: ResultSet) -> ResultSet {
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

        // Build hash table on right side.
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

    // ── Predicate-scan join ───────────────────────────────────────────────────

    /// Replace N random ScanBound probes with **one sequential POS scan**.
    ///
    /// ## When this is used
    ///
    /// Triggered from `bind_join` when:
    ///   - `right_plan` is `ScanBound` with outer var at **subject** position (0),
    ///     a fixed predicate, and either a free object (join) or fixed object
    ///     (membership filter).
    ///   - The number of unique groups exceeds `PRED_SCAN_THRESHOLD`.
    ///   - The outer var is NOT already bound in the enclosing `outer` context.
    ///
    /// ## Why this is faster
    ///
    /// Normal `bind_join` with N=508 groups calls `execute_scan` 508 times,
    /// each doing a binary search + random SSD read → O(N × page_miss_latency).
    /// At 150 ms per cold page: **508 × 150 ms ≈ 76 s per predicate**.
    ///
    /// This method instead scans the full POS range for the predicate once
    /// (sequential I/O) and filters by the known subject set in memory:
    /// O(|predicate_triples|) sequential I/O.  With 10 M triples at 120 MB/s (HDD):
    /// **~2 s per predicate** (vs 263 × 150 ms = 39 s for bind_join).
    ///
    /// Returns `None` if the pattern does not match; caller falls back to
    /// `bind_join`.
    /// PSO early-termination fast path for `bind_join`.
    ///
    /// Bypasses the O(N_left) group-building HashMap when LIMIT pushdown is
    /// active and there are far more left rows than the pushdown limit.
    ///
    /// Algorithm:
    ///   1. PSO scan for the predicate → collect `push_limit` unique subjects
    ///      with their objects.  Cost: O(push_limit × avg_fanout).
    ///   2. Build a HashSet of those subjects for O(1) lookup.
    ///   3. Linear scan of `left.rows`: for each row, if its subject is in the
    ///      PSO subject set, emit output rows.  Cost: O(N_left) sequential.
    ///
    /// Returns `None` when conditions are not met (caller falls back to the
    /// standard group-building path).
    fn try_pso_fast_path(
        &self,
        left: &ResultSet,
        right_plan: &ExecutionPlan,
        needed: &[(usize, String)],
        outer: &Binding,
    ) -> Option<ResultSet> {
        // Conditions: pushdown active, one needed left var, right is a join ScanBound.
        let push_limit = self.pushdown_limit.get()?;

        // Must have exactly one needed variable (the subject key).
        if needed.len() != 1 { return None; }
        let (subj_col, subj_var) = &needed[0];

        // Fast path only worth it when left is significantly larger than limit.
        if left.rows.len() <= push_limit * 4 { return None; }

        // Right plan must be a join ScanBound: s=outer_var, p=fixed, o=free.
        let (base, free_vars, outer_vars) = match right_plan {
            ExecutionPlan::ScanBound { base, free_vars, outer_vars } => (base, free_vars, outer_vars),
            _ => return None,
        };
        if outer_vars.len() != 1 || outer_vars[0].1 != 0 { return None; }
        if outer.contains_key(outer_vars[0].0.as_str()) { return None; }
        if base.p == UNBOUND { return None; }
        // Join mode only (o is free, one free_var at position 2).
        if base.o != UNBOUND || free_vars.len() != 1 || free_vars[0].1 != 2 { return None; }
        // PSO must be available.
        if !self.index.has_pso() { return None; }

        // Check pred_cache/pred_partition first — they're faster than PSO for small predicates.
        if self.pred_cache.get(base.p).is_some() { return None; }
        if self.pred_partitions.get(base.p).is_some() { return None; }

        let free_var_name = &free_vars[0].0;
        let outer_var_name = &outer_vars[0].0;

        // Build a subject filter from the left side so PSO only returns subjects
        // that are actually present in the left result.
        //
        // Without this filter the PSO scan returns the first `push_limit` subjects
        // in PSO index order (arbitrary IDs), which may have zero overlap with the
        // left result, producing an incorrect empty output.
        //
        // Skip the filter when left is very small (< push_limit × 4) — the early
        // exit condition above already handles that case — or when the left
        // column index is out of range.
        let left_subjects: HashSet<TermId> = left.rows.iter()
            .filter_map(|row| row.get(*subj_col).copied().flatten())
            .collect();

        if left_subjects.is_empty() { return None; }

        // Collect unique subjects from PSO, restricted to left_subjects.
        let (s_to_objects, _exhausted) = self.index.scan_pso_subject_limit(
            base.p, push_limit, &left_subjects
        );

        if s_to_objects.is_empty() { return None; }

        let fanout = s_to_objects.values().map(|v| v.len()).max().unwrap_or(1).max(1);
        let expansion_limit = push_limit.saturating_mul(fanout);

        tracing::debug!(
            pred = base.p,
            left_rows = left.rows.len(),
            push_limit,
            pso_subjects = s_to_objects.len(),
            fanout,
            "PSO fast path: collected subjects, scanning left"
        );

        // Build output schema.
        let mut out_vars = left.variables.clone();
        if !out_vars.contains(free_var_name) { out_vars.push(free_var_name.clone()); }
        let mut result = ResultSet::empty(out_vars.clone());
        let out_len = out_vars.len();
        let free_var_oi = result.variable_index(free_var_name);

        // Linear scan of left.rows: for each row, check subject ∈ PSO subjects.
        'scan: for lr in &left.rows {
            if self.is_cancelled() { result.overflow = true; break; }

            let s_id = match lr.get(*subj_col).copied().flatten() {
                Some(id) => id,
                None => continue,
            };
            let Some(objects) = s_to_objects.get(&s_id) else { continue };

            for &o_id in objects {
                let mut row = vec![None; out_len];
                for (li, lv) in left.variables.iter().enumerate() {
                    if let Some(oi) = result.variable_index(lv) {
                        row[oi] = lr.get(li).copied().flatten();
                    }
                }
                if let Some(oi) = free_var_oi { row[oi] = Some(o_id); }
                // Re-inject the outer var so downstream joins can match on it.
                if let Some(oi) = result.variable_index(outer_var_name) {
                    row[oi] = Some(s_id);
                }
                result.rows.push(row);
                if result.rows.len() >= self.config.max_intermediate_rows {
                    result.overflow = true;
                    break 'scan;
                }
                if matches!(Some(expansion_limit), Some(lim) if result.rows.len() >= lim) {
                    tracing::debug!(rows = result.rows.len(), "PSO fast path: expansion limit reached");
                    break 'scan;
                }
            }
        }

        tracing::debug!(
            out_rows = result.rows.len(),
            "PSO fast path: done"
        );
        Some(result)
    }

    fn try_predicate_scan_join(
        &self,
        left: &ResultSet,
        right_plan: &ExecutionPlan,
        groups: &[(Vec<Option<TermId>>, Vec<usize>)],
        needed: &[(usize, String)],
        outer: &Binding,
    ) -> Option<ResultSet> {
        // ── Pattern matching ──────────────────────────────────────────────────
        let (base, free_vars, outer_vars) = match right_plan {
            ExecutionPlan::ScanBound { base, free_vars, outer_vars } => (base, free_vars, outer_vars),
            _ => return None,
        };

        // Require exactly one outer var at subject position (0).
        if outer_vars.len() != 1 || outer_vars[0].1 != 0 {
            return None;
        }
        let outer_var_name = &outer_vars[0].0;

        // Skip if the var is already bound in the enclosing context — with all
        // groups sharing the same S, the threshold wouldn't have triggered anyway.
        if outer.contains_key(outer_var_name.as_str()) {
            return None;
        }

        // Predicate must be fixed.
        if base.p == UNBOUND {
            return None;
        }

        // Distinguish join (O free, one free_var at pos 2) from filter (O fixed).
        let is_join   = base.o == UNBOUND && free_vars.len() == 1 && free_vars[0].1 == 2;
        let is_filter = base.o != UNBOUND && free_vars.is_empty();
        if !is_join && !is_filter {
            return None;
        }

        // Key position in group-key array for the outer var.
        let kp = needed.iter().position(|(_, v)| v == outer_var_name)?;

        // ── Cost gate ─────────────────────────────────────────────────────────
        // Index files reside on HDD (~120 MB/s sequential, 24 bytes/triple → 200 ns/triple).
        // SPO SkipIndex cold seek: ~150 ms per group (HDD random access + page fault).
        // Crossover = SPO_seek / POS_read = 150_000_000 ns / 200 ns = 750_000 triples.
        // Round to 1_000_000 for a small safety margin.
        //
        // Empirical validation (cold HDD):
        //   step2: N=263, pred=14.6M  → threshold=263M >> 14.6M → POS scan ✓
        //   step4: N=239, pred=15.4M  → threshold=239M >> 15.4M → POS scan ✓
        //   step6: N=239, pred=545M   → threshold=239M <  545M  → bind_join ✓
        //          (POS scan cost=545M×200ns=109s > bind_join=57.6s → correctly rejects)
        //
        // EXCEPTION: filter patterns (is_filter=true, P and O both fixed).
        // The scan only covers the tiny (P,O) subrange in POS — NOT the full P range.
        // For step7 (rdf:type filter, pred_range=545M, N=239):
        //   bind_join: 239 × 150ms = 35.8s predicted (actual: 171s cold!)
        //   (P,O) scan: binary search log2(545M)≈29 pages × 12ms = 348ms + tiny scan
        //   → always bypass cost gate for filter patterns.
        const CROSSOVER: usize = 1_000_000; // (150_000_000 ns) / (200 ns/triple on HDD)
        if !is_filter {
            if let Some((lo, hi)) = self.index.pos_predicate_range(base.p) {
                let pred_range = hi - lo;
                if pred_range > groups.len().saturating_mul(CROSSOVER) {
                    tracing::debug!(
                        groups = groups.len(),
                        pred_range,
                        threshold = groups.len() * CROSSOVER,
                        pred = base.p,
                        "try_predicate_scan_join: pred range too large, falling back to SPO bind_join"
                    );
                    return None;
                }
            }
        }

        let t0 = std::time::Instant::now();

        // ── Collect unique subject values ─────────────────────────────────────
        let subjects: HashSet<TermId> = groups.iter()
            .filter_map(|(key, _)| key.get(kp).copied().flatten())
            .collect();

        // Scan pattern: P fixed, S/O as determined above.
        let scan_pat = TriplePattern { s: UNBOUND, p: base.p, o: base.o };

        tracing::debug!(
            groups = groups.len(),
            unique_subjects = subjects.len(),
            pred = base.p,
            is_filter,
            "bind_join: switching to predicate scan (sequential POS)"
        );

        // ── Filter mode: membership test (S, P, O_fixed) ─────────────────────
        if is_filter {
            // Priority 1: TypeCache — O(log |class|) binary search per subject.
            // Covers `?x a SomeClass` patterns (P=rdf:type, O=class_id).
            // Avoids the full rdf:type POS/OPS scan entirely.
            //
            // Priority 2: pred_cache — sorted (S,O) Vec in RAM.
            // Priority 3: pred_partitions — sorted (S,O) mmap file on disk.
            // Priority 4: POS/OPS index scan (fallback).
            let passing: HashSet<TermId> = if let Some(class_bitmap) =
                self.type_cache.rdf_type_id()
                    .filter(|&rdf_type| base.p == rdf_type)
                    .and_then(|_| self.type_cache.get_bitmap(base.o))
            {
                tracing::debug!(
                    pred = base.p,
                    class = base.o,
                    unique_subjects = subjects.len(),
                    class_size = class_bitmap.len(),
                    mode = "type_cache_filter_bitmap",
                    "bind_join filter: using type cache (RoaringTreemap O(1))"
                );
                subjects.iter().copied().filter(|&s| {
                    class_bitmap.contains(s)   // O(1) bitmap lookup
                }).collect()
            } else if let Some(cached) = self.pred_cache.get(base.p) {
                tracing::debug!(
                    pred = base.p,
                    unique_subjects = subjects.len(),
                    cached_pairs = cached.len(),
                    mode = "pred_cache_filter",
                    "bind_join filter: using cached predicate"
                );
                let target_o = base.o;
                subjects.iter().copied().filter(|&s| {
                    cached.binary_search(&(s, target_o)).is_ok()
                }).collect()
            } else if let Some(part) = self.pred_partitions.get(base.p) {
                tracing::debug!(
                    pred = base.p,
                    unique_subjects = subjects.len(),
                    part_pairs = part.len(),
                    mode = "pred_partition_filter",
                    "bind_join filter: using pred partition"
                );
                let target_o = base.o;
                subjects.iter().copied().filter(|&s| {
                    part.contains(s, target_o)
                }).collect()
            } else {
                // Scan the (P, O_fixed) range in POS/OPS — typically just a handful of
                // entries — and collect passing subject IDs.
                self.index.scan(&scan_pat)
                    .filter(|t| subjects.contains(&t.s))
                    .map(|t| t.s)
                    .collect()
            };

            let mut result = ResultSet::empty(left.variables.clone());
            for (key, row_indices) in groups {
                if self.is_cancelled() {
                    result.overflow = true;
                    return Some(result);
                }
                if let Some(s_id) = key.get(kp).copied().flatten() {
                    if passing.contains(&s_id) {
                        for &ri in row_indices {
                            result.rows.push(left.rows[ri].clone());
                            if result.rows.len() >= self.config.max_intermediate_rows {
                                result.overflow = true;
                                return Some(result);
                            }
                            if self.is_limit_reached(result.rows.len()) {
                                tracing::debug!(rows = result.rows.len(), "predicate scan (filter): LIMIT reached, stopping early");
                                return Some(result);
                            }
                        }
                    }
                }
            }

            tracing::debug!(
                elapsed_us = t0.elapsed().as_micros(),
                out_rows = result.rows.len(),
                "bind_join predicate scan (filter) done"
            );
            return Some(result);
        }

        // ── Join mode: S=outer → O via predicate P ────────────────────────────
        let free_var_name = &free_vars[0].0;

        // Collect (S → [O]) pairs for matching subjects.
        // Prefer pred_cache: merge scan (sort subjects + two-pointer) → O(N log N + M) RAM.
        // Fall back to sequential POS scan → O(pred_range) HDD.
        // Build (S → [O]) mapping from the best available source:
        // pred_cache (RAM) → pred_partitions (mmap) → index scan (HDD).
        let mut s_to_objects: HashMap<TermId, Vec<TermId>> = HashMap::new();
        if let Some(cached) = self.pred_cache.get(base.p) {
            tracing::debug!(
                pred = base.p,
                unique_subjects = subjects.len(),
                cached_pairs = cached.len(),
                mode = "pred_cache_join",
                "bind_join join: using cached predicate"
            );
            // Merge-join: sort subjects then scan pred_cache sequentially.
            // Binary search: O(N × log M) with random access → cache-miss heavy for large M.
            // Merge join: O(N log N + N + M) with sequential access → prefetcher-friendly.
            // For M ≥ 1M pairs the sequential scan is ~10–20× faster due to cache behaviour.
            let mut subjects_sorted: Vec<TermId> = subjects.iter().copied().collect();
            subjects_sorted.sort_unstable();
            let mut ci = 0usize;
            for &s in &subjects_sorted {
                // Skip past pred_cache entries whose subject < s.
                while ci < cached.len() && cached[ci].0 < s { ci += 1; }
                if ci >= cached.len() { break; }
                if cached[ci].0 == s {
                    let start = ci;
                    while ci < cached.len() && cached[ci].0 == s { ci += 1; }
                    let objs: Vec<TermId> = cached[start..ci].iter().map(|&(_, o)| o).collect();
                    s_to_objects.insert(s, objs);
                }
            }
        } else if let Some(part) = self.pred_partitions.get(base.p) {
            // Check if this is a functional predicate (改善6): each subject has
            // at most one object.  If so, use get_single_object() which is O(log N)
            // with zero Vec allocation, vs get_objects() which allocates a slice.
            let is_functional = self.stats
                .map(|st| st.is_functional(base.p))
                .unwrap_or(false);

            if is_functional {
                tracing::debug!(
                    pred = base.p,
                    unique_subjects = subjects.len(),
                    part_pairs = part.len(),
                    mode = "pred_partition_functional_join",
                    "bind_join join: using pred partition (functional predicate, O(log N) single lookup)"
                );
                for &s in &subjects {
                    if let Some(o) = part.get_single_object(s) {
                        s_to_objects.insert(s, vec![o]);
                    }
                }
            } else {
                tracing::debug!(
                    pred = base.p,
                    unique_subjects = subjects.len(),
                    part_pairs = part.len(),
                    mode = "pred_partition_join",
                    "bind_join join: using pred partition"
                );
                for &s in &subjects {
                    let pairs = part.get_objects(s);
                    if !pairs.is_empty() {
                        let objs: Vec<TermId> = pairs.iter().map(|&(_, o)| o).collect();
                        s_to_objects.insert(s, objs);
                    }
                }
            }
        } else if let Some(push_limit) = self.pushdown_limit.get().filter(|&lim| {
            // Use PSO early-termination when:
            //   1. LIMIT pushdown is active
            //   2. PSO index is available
            //   3. subjects.len() > push_limit (more candidates than needed —
            //      the whole point is to avoid scanning all of them)
            //
            // PSO is (P, S, O): subjects are contiguous per predicate.
            // Scanning PSO until `push_limit` unique subjects are collected
            // costs O(push_limit × fanout) instead of O(|pred_range|).
            //
            // Example: dct:identifier, 10 M entries, LIMIT 1 000
            //   POS scan:  reads 10 M entries → 15 s
            //   PSO early: reads 1 000 entries → < 1 ms
            self.index.has_pso() && subjects.len() > lim
        }) {
            tracing::debug!(
                pred = base.p,
                unique_subjects = subjects.len(),
                push_limit,
                mode = "pso_early_termination",
                "bind_join join: using PSO early-termination scan"
            );
            let (pso_result, exhausted) = self.index.scan_pso_subject_limit(
                base.p, push_limit, &subjects
            );
            s_to_objects = pso_result;
            tracing::debug!(
                collected = s_to_objects.len(),
                exhausted,
                "PSO early-termination: done"
            );
        } else {
            // One sequential POS/OPS scan: collect (S → [O]) pairs for matching subjects.
            for triple in self.index.scan(&scan_pat) {
                if subjects.contains(&triple.s) {
                    s_to_objects.entry(triple.s).or_default().push(triple.o);
                }
            }
            // Release page cache after large sequential scans to avoid monopolising RAM.
            if self.scan_dontneed_bytes > 0 {
                let kind = scan_pat.best_index();
                let released = self.index.advise_dontneed(kind);
                if released >= self.scan_dontneed_bytes {
                    tracing::debug!(
                        released_mb = released / (1024 * 1024),
                        kind = ?kind,
                        "predicate scan: MADV_DONTNEED released page cache"
                    );
                }
            }
        }

        // Build output schema: left vars + the new free variable.
        let mut out_vars = left.variables.clone();
        if !out_vars.contains(free_var_name) {
            out_vars.push(free_var_name.clone());
        }

        let mut result = ResultSet::empty(out_vars.clone());
        let out_len = out_vars.len();

        // Fast lookup: subject_id → row indices in the left result set.
        let mut s_to_rows: HashMap<TermId, &Vec<usize>> = HashMap::new();
        for (key, row_indices) in groups {
            if let Some(s_id) = key.get(kp).copied().flatten() {
                s_to_rows.insert(s_id, row_indices);
            }
        }

        let triples_matched: usize = s_to_objects.values().map(|v| v.len()).sum();

        // Fanout-adjusted expansion limit for 1:N join steps.
        //
        // Problem: applying pushdown_limit directly to the expansion row count
        // causes premature truncation when each subject expands to multiple
        // objects (1:N join).
        //
        // Example: LIMIT 1000, PSM → SIO_000216 (7 nodes/PSM)
        //   raw limit = 1000 → stops after 1000 rows = 142 PSMs × 7 nodes
        //   only 142 PSMs get v1 resolved → type filter returns 142 (wrong!)
        //
        // Fix: scale the expansion limit by max observed fanout:
        //   fanout = 7 → expansion_limit = 1000 × 7 = 7000
        //   all 1000 PSMs × 7 nodes = 7000 rows → collected in full → filter
        //   returns 1000 (correct)
        //
        // For 1:1 joins (G-6 dct:identifier, fanout=1):
        //   expansion_limit = 200 × 1 = 200 → same behaviour as before ✓
        let max_fanout = s_to_objects.values().map(|v| v.len()).max().unwrap_or(1).max(1);
        let expansion_limit: Option<usize> = self.pushdown_limit.get()
            .map(|lim| lim.saturating_mul(max_fanout));

        tracing::debug!(
            max_fanout,
            expansion_limit = ?expansion_limit,
            "predicate scan (join): computed expansion limit"
        );

        // Expand: for each (S, O) pair, cross with all left rows sharing that S.
        'expand: for (s_id, objects) in &s_to_objects {
            if self.is_cancelled() {
                result.overflow = true;
                break 'expand;
            }
            let Some(row_indices) = s_to_rows.get(s_id) else { continue; };
            for &o_id in objects {
                for &row_idx in *row_indices {
                    let lr = &left.rows[row_idx];
                    let mut row = vec![None; out_len];
                    for (li, lv) in left.variables.iter().enumerate() {
                        if let Some(oi) = result.variable_index(lv) {
                            row[oi] = lr.get(li).copied().flatten();
                        }
                    }
                    if let Some(oi) = result.variable_index(free_var_name) {
                        row[oi] = Some(o_id);
                    }
                    result.rows.push(row);
                    if result.rows.len() >= self.config.max_intermediate_rows {
                        result.overflow = true;
                        return Some(result);
                    }
                    if matches!(expansion_limit, Some(lim) if result.rows.len() >= lim) {
                        tracing::debug!(rows = result.rows.len(), max_fanout, "predicate scan (join): expansion limit reached, stopping early");
                        break 'expand;
                    }
                }
            }
        }

        tracing::debug!(
            elapsed_us = t0.elapsed().as_micros(),
            triples_matched,
            out_rows = result.rows.len(),
            "bind_join predicate scan (join) done"
        );
        Some(result)
    }

    // ── Bind-join (index nested-loop join) ────────────────────────────────────

    /// For each *unique binding* seen in `left` that is actually referenced by
    /// `right_plan`, execute `right_plan` once and distribute the results across
    /// all left rows that share that binding.
    ///
    /// ## Why this matters
    ///
    /// Naively executing right_plan once per left row causes redundant work
    /// whenever two left rows produce the same substitution for the variables
    /// right_plan actually uses.  The classic case is an independent join branch:
    ///
    /// ```text
    /// left = [(?protein=P, ?tax=T1),   ← same ?protein, different ?tax
    ///         (?protein=P, ?tax=T2),
    ///         (?protein=P, ?tax=T3)]
    /// right_plan = ?protein jpo:hasPeptideEvidence ?pepevi … (100 rows)
    /// ```
    ///
    /// `?tax` is irrelevant to right_plan.  Without deduplication, right_plan
    /// runs 3 times and produces 300 rows (3 × 100).  With deduplication it
    /// runs once and still produces 300 rows — but at 1/3 the work.
    ///
    /// ## Algorithm
    ///
    /// 1. Identify which left variables right_plan actually references
    ///    (`plan_referenced_vars`).
    /// 2. Group left rows by their values at those positions.
    /// 3. For each group: execute right_plan once with the shared binding,
    ///    then cross-join the result with every left row in the group.
    fn bind_join(&self, left: ResultSet, right_plan: &ExecutionPlan, outer: &Binding) -> ResultSet {
        let t_bj = std::time::Instant::now();
        // Variables that right_plan will substitute from the outer context.
        let right_refs = plan_referenced_vars(right_plan);

        // Indices (and names) of left variables that right_plan actually needs.
        let needed: Vec<(usize, String)> = left.variables.iter().enumerate()
            .filter(|(_, v)| right_refs.contains(v.as_str()))
            .map(|(i, v)| (i, v.clone()))
            .collect();

        // ── PSO early-termination fast path ───────────────────────────────────
        //
        // When LIMIT pushdown is active AND:
        //   - right_plan is a join ScanBound (s=outer, p=fixed, o=free)
        //   - PSO index is available
        //   - left has far more rows than the pushdown limit
        //
        // We bypass the expensive group-building phase entirely.
        //
        // Normal path: build 10M groups (HashMap) → PSO → expand → 1000 rows
        //   Cost: O(N_left) HashMap build + O(LIMIT) PSO = 10s + 1.3s
        //
        // Fast path: PSO first → scan left.rows once for matching subjects
        //   Cost: O(LIMIT) PSO + O(N_left) linear scan = 1.3s + ~1s
        //   (no HashMap allocation, just sequential reads + HashSet lookup)
        if let Some(fast) = self.try_pso_fast_path(&left, right_plan, &needed, outer) {
            tracing::debug!(
                left_rows = left.rows.len(),
                out_rows = fast.rows.len(),
                elapsed_us = t_bj.elapsed().as_micros(),
                "bind_join done (PSO fast path)"
            );
            return fast;
        }

        // Group left rows by the binding key right_plan actually uses.
        // Insertion-order Vec preserves output row order.
        let mut key_to_group: HashMap<Vec<Option<TermId>>, usize> = HashMap::new();
        let mut groups: Vec<(Vec<Option<TermId>>, Vec<usize>)> = Vec::new();
        for (row_idx, row) in left.rows.iter().enumerate() {
            let key: Vec<Option<TermId>> = needed.iter()
                .map(|(i, _)| row.get(*i).copied().flatten())
                .collect();
            let g = *key_to_group.entry(key.clone()).or_insert_with(|| {
                let id = groups.len();
                groups.push((key, Vec::new()));
                id
            });
            groups[g].1.push(row_idx);
        }

        let mut out_vars: Vec<String> = left.variables.clone();
        let mut result = ResultSet::empty(out_vars.clone());

        // Switch to faster I/O strategies when the group count is large enough
        // that N individual random-I/O probes would dominate query time.
        //
        // At 150 ms per cold SSD page, PRED_SCAN_THRESHOLD=32 means we only
        // pay ~5 s before switching to sub-second sequential alternatives.
        const PRED_SCAN_THRESHOLD: usize = 32;

        // ── I/O Optimization 1: Predicate Scan ───────────────────────────────
        //
        // Replace N ScanBound probes with one sequential POS scan.
        // Converts O(N × random_io_latency) → O(|predicate_range|) sequential.
        // For N=508 @ 150 ms each: ~76 s → ~0.5 s per predicate.
        //
        // Falls through to the normal loop if the ScanBound shape does not
        // match (outer var must be at subject position, P must be fixed).
        if groups.len() > PRED_SCAN_THRESHOLD {
            if let Some(fast_result) = self.try_predicate_scan_join(
                &left, right_plan, &groups, &needed, outer,
            ) {
                tracing::debug!(
                    groups = groups.len(),
                    left_rows = left.rows.len(),
                    out_rows = fast_result.rows.len(),
                    elapsed_us = t_bj.elapsed().as_micros(),
                    "bind_join done (predicate scan)"
                );
                return fast_result;
            }
        }

        // ── I/O Optimization 2: Sort groups by primary TermId (index locality) ─
        //
        // SPO / POS / OSP indexes are sorted by TermId.  When groups are processed
        // in ascending TermId order, successive HDD reads advance through the index
        // monotonically — similar to a sequential scan rather than random seeks.
        //
        // SAFETY CAP: only sort when the group count is small enough that the
        // sort overhead is justified.  For very large group counts (> MAX_SORT_GROUPS)
        // the sort itself can take seconds and consumes significant memory bandwidth.
        // Beyond ~100 K groups the I/O locality gain also diminishes: with 100 K+
        // random subjects the OS page cache cannot hold them all anyway, so the
        // benefit of sorted order approaches zero.
        //
        // SPARQL result sets are bags (unordered); ORDER BY is applied later at the
        // SELECT level, so changing group evaluation order is always safe.
        const MAX_SORT_GROUPS: usize = 100_000;
        if groups.len() > 1 && groups.len() <= MAX_SORT_GROUPS {
            groups.sort_unstable_by_key(|(key, _)| {
                key.iter().find_map(|k| *k).unwrap_or(TermId::MAX)
            });
            tracing::debug!(
                groups = groups.len(),
                "bind_join: groups sorted by TermId for index locality"
            );
        }

        // ── I/O Optimization 3: Batch madvise Prefetch ───────────────────────
        //
        // Fire madvise(MADV_WILLNEED) hints *before* starting execution so the
        // OS can pipeline disk reads in the background.
        //
        // SAFETY CAP: MAX_PREFETCH_GROUPS limits the number of groups for which
        // we issue hints.  Without this cap, a query with millions of groups
        // (e.g. all PSMs: N=1M) causes:
        //
        //   1. 1M HashMap clones (outer.clone()) → gigabytes of heap churn.
        //   2. 1M × N_patterns madvise syscalls → kernel MADV queue exhaustion,
        //      stalling the entire system (login impossible).
        //
        // The OS I/O queue depth is ~32–256 on modern hardware; issuing more
        // than ~1024 hints simultaneously provides no additional throughput.
        // For large group counts the predicate-scan path (Opt 1) is the right
        // tool; the prefetch path is a fallback for moderate counts only.
        //
        // Predicates in pred_cache are skipped (no HDD I/O needed).
        const MAX_PREFETCH_GROUPS: usize = 1_024;
        if groups.len() > PRED_SCAN_THRESHOLD {
            let prefetch_n = MAX_PREFETCH_GROUPS.min(groups.len());
            // Reuse the patterns Vec across iterations to avoid per-group allocation.
            let mut patterns: Vec<TriplePattern> = Vec::new();
            let mut total_prefetches = 0usize;
            for (key, _) in groups.iter().take(prefetch_n) {
                // Build the full binding for this group (outer + group-key vars).
                let mut row_binding = outer.clone();
                for (key_pos, (_, var_name)) in needed.iter().enumerate() {
                    if let Some(id) = key.get(key_pos).copied().flatten() {
                        row_binding.insert(var_name.clone(), id);
                    }
                }
                patterns.clear(); // reuse allocation
                collect_scanbound_patterns(right_plan, &row_binding, &self.pred_cache, &mut patterns);
                for pat in &patterns {
                    self.index.prefetch_pattern(pat);
                    total_prefetches += 1;
                }
            }
            if total_prefetches > 0 {
                tracing::debug!(
                    groups = groups.len(),
                    prefetched_groups = prefetch_n,
                    prefetches = total_prefetches,
                    capped = (groups.len() > MAX_PREFETCH_GROUPS),
                    "bind_join: batch prefetch fired (madvise WILLNEED)"
                );
            }
        }

        for (key, row_indices) in &groups {
            // Check cancellation flag (set by query timeout) at each group boundary.
            if self.is_cancelled() {
                result.overflow = true;
                tracing::debug!("bind_join: query cancelled (timeout)");
                return result;
            }

            // Build binding: outer context + the needed left variables for this group.
            let mut row_binding = outer.clone();
            for (key_pos, (left_idx, var_name)) in needed.iter().enumerate() {
                if let Some(id) = key.get(key_pos).copied().flatten() {
                    row_binding.insert(var_name.clone(), id);
                }
                let _ = left_idx; // used via key indexing above
            }

            // Execute right plan ONCE for this unique binding.
            let right_rs = self.execute_plan_with_ctx(right_plan, &row_binding);
            let right_overflow = right_rs.overflow;

            // Expand output schema on first non-empty right result.
            for rv in &right_rs.variables {
                if !out_vars.contains(rv) { out_vars.push(rv.clone()); }
            }
            if result.variables.len() < out_vars.len() {
                result.variables = out_vars.clone();
                let new_len = out_vars.len();
                for row in &mut result.rows { row.resize(new_len, None); }
            }

            let out_len = result.variables.len();

            if right_rs.rows.is_empty() {
                // No match for any row in this group → inner join semantics: skip all.
                if right_overflow { result.overflow = true; return result; }
                continue;
            }

            // Distribute right results across all left rows in this group.
            for &row_idx in row_indices {
                let left_row = &left.rows[row_idx];
                for right_row in &right_rs.rows {
                    let mut row = vec![None; out_len];
                    // Fill from left.
                    for (li, lv) in left.variables.iter().enumerate() {
                        if let Some(oi) = result.variable_index(lv) {
                            row[oi] = left_row.get(li).copied().flatten();
                        }
                    }
                    // Fill from right — skip inconsistent rows (plan-binding mismatch).
                    //
                    // With ScanAst-based plans, right can only return rows consistent
                    // with the substituted binding, so this check is mostly a guard.
                    let mut consistent = true;
                    for (ri, rv) in right_rs.variables.iter().enumerate() {
                        if let Some(oi) = result.variable_index(rv) {
                            let rv_val = right_row.get(ri).copied().flatten();
                            match (row[oi], rv_val) {
                                (Some(l), Some(r)) if l != r => { consistent = false; break; }
                                (None, v) => { row[oi] = v; }
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
                    if self.is_limit_reached(result.rows.len()) {
                        tracing::debug!(rows = result.rows.len(), "bind_join: LIMIT reached, stopping early");
                        return result;
                    }
                }
            }

            if right_overflow {
                result.overflow = true;
                return result;
            }
            // Check limit after finishing a group (catches the case where the
            // last row of a group exactly hit the limit).
            if self.is_limit_reached(result.rows.len()) {
                break;
            }
        }
        tracing::debug!(
            groups = groups.len(),
            left_rows = left.rows.len(),
            out_rows = result.rows.len(),
            elapsed_us = t_bj.elapsed().as_micros(),
            "bind_join done"
        );
        result
    }

    /// Fast path for `SELECT (COUNT(*) AS ?alias …) WHERE { pure cross-product }`.
    ///
    /// If the plan is a tree of `Join(Scan, Scan)` nodes with no `ScanBound`
    fn try_count_distinct_cross_product(
        &self,
        query: &SelectQuery,
        plan: &ExecutionPlan,
    ) -> Option<ResultSet> {
        if !query.group_by.is_empty() || !query.having.is_empty() || query.distinct {
            return None;
        }

        // Projection must be exactly one COUNT(DISTINCT ?var) alias.
        let (count_var, alias_name) = if let Projection::Variables(items) = &query.projection {
            if items.len() != 1 { return None; }
            match &items[0] {
                SelectItem::Alias(Expression::Count { distinct: true, expr: Some(inner) }, name) => {
                    if let Expression::Variable(v) = inner.as_ref() {
                        (v.clone(), name.clone())
                    } else { return None; }
                }
                _ => return None,
            }
        } else { return None; };

        // Plan must decompose into independent Scan leaves.
        let mut leaves: Vec<&ExecutionPlan> = Vec::new();
        if !collect_cross_product_leaves(plan, &mut leaves) { return None; }
        if leaves.len() < 2 { return None; }

        // Find the unique leaf that produces count_var.
        let var_leaf = leaves.into_iter().find(|leaf| {
            if let ExecutionPlan::Scan { variables, .. } = leaf {
                variables.iter().any(|(v, _)| v == &count_var)
            } else { false }
        })?;

        let rs = self.execute_plan(var_leaf);
        if rs.overflow { return None; }
        let col = rs.variable_index(&count_var)?;
        let distinct_count = rs.rows.iter()
            .filter_map(|row| row.get(col).copied().flatten())
            .collect::<HashSet<TermId>>()
            .len();

        tracing::debug!(
            distinct_count,
            var = count_var,
            "cross-product COUNT(DISTINCT ?x): computed from single scan"
        );

        let n_str = format!("\"{}\"^^<http://www.w3.org/2001/XMLSchema#integer>", distinct_count);
        let n_id = self.dict.encode(&n_str);
        let mut result = ResultSet::empty(vec![alias_name]);
        result.rows.push(vec![Some(n_id)]);
        Some(result)
    }

<<<<<<< HEAD
    /// Fast path for cross-product COUNT(*).
    ///
    /// Detects: SELECT (COUNT(*) AS ?alias) WHERE { pure cross-product of independent scans }
    /// Computes: N1 × N2 × ... × Nk  without materialising the cross-product.
    fn try_count_star_cross_product(
        &self,
        query: &SelectQuery,
        plan: &ExecutionPlan,
    ) -> Option<ResultSet> {
        if !query.group_by.is_empty() || !query.having.is_empty()
            || query.distinct || query.offset.is_some() {
            return None;
        }

        let alias_name = if let Projection::Variables(items) = &query.projection {
            if items.len() != 1 { return None; }
            match &items[0] {
                SelectItem::Alias(
                    Expression::Count { distinct: false, expr: None },
                    name,
                ) => name.clone(),
                _ => return None,
            }
        } else { return None; };

        let mut leaves: Vec<&ExecutionPlan> = Vec::new();
        if !collect_cross_product_leaves(plan, &mut leaves) { return None; }
        if leaves.len() < 2 { return None; }

        let mut product: u128 = 1;
        for leaf in &leaves {
            let ExecutionPlan::Scan { pattern, .. } = leaf else { return None; };
            let n = self.index.estimate(pattern) as u128;
            if n == 0 { product = 0; break; }
            product = product.saturating_mul(n);
        }

        tracing::debug!(product, leaves = leaves.len(), "cross-product COUNT(*): computed analytically");

        let n_str = format!("\"{}\"^^<http://www.w3.org/2001/XMLSchema#integer>", product);
        let n_id = self.dict.encode(&n_str);
        let mut result = ResultSet::empty(vec![alias_name]);
        result.rows.push(vec![Some(n_id)]);
        Some(result)
    }

    /// Fast path for `SELECT (COUNT(*) AS ?alias) WHERE { single_triple_pattern }`.
    ///
    /// When the plan is a single `Scan` (no joins, no filters) and the projection
    /// is exactly one COUNT(*) alias, compute the count via `index.estimate()`
    /// — an O(log N) binary-search range count — instead of materialising and
    /// counting all matching rows.
    fn try_count_star_single_scan(
        &self,
        query: &SelectQuery,
        plan: &ExecutionPlan,
    ) -> Option<ResultSet> {
        if !query.group_by.is_empty() || !query.having.is_empty()
            || query.distinct || query.offset.is_some() {
            return None;
        }

        let alias_name = if let Projection::Variables(items) = &query.projection {
            if items.len() != 1 { return None; }
            match &items[0] {
                SelectItem::Alias(
                    Expression::Count { distinct: false, expr: None },
                    name,
                ) => name.clone(),
                _ => return None,
            }
        } else { return None; };

        let pattern = match plan {
            ExecutionPlan::Scan { pattern, .. } => pattern,
            _ => return None,
        };

        let n = self.index.estimate(pattern);
        tracing::debug!(n, "single-scan COUNT(*): computed via index.estimate");

        let n_str = format!("\"{}\"^^<http://www.w3.org/2001/XMLSchema#integer>", n);
        let n_id = self.dict.encode(&n_str);
        let mut result = ResultSet::empty(vec![alias_name]);
        result.rows.push(vec![Some(n_id)]);
        Some(result)
    }

=======
>>>>>>> 583087b0f936c7c64e6a487eb4dfd6a098398d26
    /// Merge-scan join for PathPattern when the full path is in path_cache.
    ///
    /// Instead of N binary searches (one per left row) — each causing TLB/page-cache
    /// misses across the 188 MB sorted array — this reads the cached pairs exactly once
    /// in sequential order and matches them against a HashMap of source TermIds from left.
    ///
    /// Complexity: O(N_left + M_cache) instead of O(N_left × log M_cache).
    /// Empirical speedup for faldo 2-hop (N=263, M=11.8M): 13.7 s → ~100 ms.
    fn path_cache_merge_join(
        &self,
        left: ResultSet,
        right_plan: &ExecutionPlan,
        outer: &Binding,
    ) -> ResultSet {
        let t0 = std::time::Instant::now();

        let (path_s_term, path, path_o_term) = match right_plan {
            ExecutionPlan::PathPattern { s, path, o } => (s, path, o),
            _ => return self.bind_join(left, right_plan, outer),
        };

        // Resolve path → TermIds for cache lookup (Sequence of IRI steps only).
        let path_ids_opt: Option<Vec<TermId>> = match path {
            PropertyPath::Sequence(steps) => steps.iter().map(|s| {
                if let PropertyPath::Iri(iri) = s { self.dict.lookup(iri) } else { None }
            }).collect(),
            PropertyPath::Iri(iri) => self.dict.lookup(iri).map(|id| vec![id]),
            _ => None,
        };
        let path_ids = match path_ids_opt {
            Some(ids) => ids,
            None => return self.bind_join(left, right_plan, outer),
        };

        let cached = match self.path_cache.get(&path_ids) {
            Some(c) => c,
            None => return self.bind_join(left, right_plan, outer),
        };

        // path_s must be a Variable present in left.
        let s_var_name: String = match path_s_term {
            Term::Variable(v) => v.clone(),
            _ => return self.bind_join(left, right_plan, outer),
        };
        let s_col = match left.variable_index(&s_var_name) {
            Some(col) => col,
            None => return self.bind_join(left, right_plan, outer),
        };

        // Resolve path_o after applying outer bindings.
        let path_o_sub = self.substitute_term(path_o_term, outer);
        let (o_var_name, o_filter, o_left_col): (Option<String>, Option<TermId>, Option<usize>) =
            match &path_o_sub {
                Term::Variable(v) => {
                    let o_col = left.variable_index(v.as_str());
                    (Some(v.clone()), None, o_col)
                }
                t => match self.encode_term(t) {
                    Some(oid) => (None, Some(oid), None),
                    None => return ResultSet::empty(left.variables.clone()),
                },
            };

<<<<<<< HEAD
=======
        // Build src_id → list of left-row indices (O(N_left)).
        let mut src_to_rows: HashMap<TermId, Vec<usize>> = HashMap::new();
        for (row_idx, row) in left.rows.iter().enumerate() {
            if let Some(Some(sid)) = row.get(s_col) {
                src_to_rows.entry(*sid).or_default().push(row_idx);
            }
        }

>>>>>>> 583087b0f936c7c64e6a487eb4dfd6a098398d26
        // Output variable list: left vars + new path_o var (if free).
        let mut out_vars: Vec<String> = left.variables.clone();
        if let Some(ref ov) = o_var_name {
            if o_left_col.is_none() && !out_vars.contains(ov) {
                out_vars.push(ov.clone());
            }
        }
        let mut result = ResultSet::empty(out_vars.clone());
        let out_s_col = result.variable_index(&s_var_name);
        let out_o_col = o_var_name.as_deref().and_then(|ov| result.variable_index(ov));

<<<<<<< HEAD
        // Adaptive join strategy:
        //   N_left small (< 100K): binary search per unique source value
        //     Cost: O(N_unique × log M + total_matches) – avoids full cache scan
        //   N_left large (≥ 100K): build HashMap + single linear scan of cache
        //     Cost: O(M_cache) sequential – avoids N×log(M) DRAM misses
        //
        // Cross-over: N×log(M)×100ns = M×50ns → N ≈ M/(2×log(M)) ≈ 256K for M=11.8M.
        // We use 100K as a conservative threshold.
        const BINARY_SEARCH_THRESHOLD: usize = 100_000;

        if left.rows.len() < BINARY_SEARCH_THRESHOLD {
            // Collect and sort (src, row_idx) pairs so we can binary-search cached once
            // per unique source value instead of scanning all M_cache entries.
            let mut src_pairs: Vec<(TermId, usize)> = left.rows.iter().enumerate()
                .filter_map(|(i, row)| row.get(s_col).copied().flatten().map(|s| (s, i)))
                .collect();
            src_pairs.sort_unstable_by_key(|&(s, _)| s);

            let mut pi = 0;
            while pi < src_pairs.len() {
                let src = src_pairs[pi].0;
                // Find all left rows with this source value.
                let end = pi + src_pairs[pi..].partition_point(|&(s, _)| s == src);
                let row_slice = &src_pairs[pi..end];
                pi = end;

                // Binary search into the sorted cached array.
                let lo = cached.partition_point(|&(s, _)| s < src);
                for &(s, dst) in &cached[lo..] {
                    if s != src { break; }
                    if let Some(oid) = o_filter { if dst != oid { continue; } }
                    for &(_, row_idx) in row_slice {
                        let left_row = &left.rows[row_idx];
                        if let Some(o_lc) = o_left_col {
                            if left_row.get(o_lc).copied().flatten() != Some(dst) { continue; }
                        }
                        let mut row = vec![None; result.variables.len()];
                        for (li, lv) in left.variables.iter().enumerate() {
                            if let Some(oi) = result.variable_index(lv) {
                                row[oi] = left_row.get(li).copied().flatten();
                            }
                        }
                        if let Some(sc) = out_s_col { row[sc] = Some(src); }
                        if let Some(oc) = out_o_col { row[oc] = Some(dst); }
                        result.rows.push(row);
                        if result.rows.len() >= self.config.max_intermediate_rows {
                            result.overflow = true;
                            return result;
                        }
                        if self.is_limit_reached(result.rows.len()) { return result; }
                    }
                }
            }
        } else {
            // Build src_id → list of left-row indices (O(N_left)).
            let mut src_to_rows: HashMap<TermId, Vec<usize>> = HashMap::new();
            for (row_idx, row) in left.rows.iter().enumerate() {
                if let Some(Some(sid)) = row.get(s_col) {
                    src_to_rows.entry(*sid).or_default().push(row_idx);
                }
            }

            // Single sequential scan through all cached pairs (O(M_cache)).
            for &(src, dst) in cached.iter() {
                if let Some(oid) = o_filter { if dst != oid { continue; } }
                let row_indices = match src_to_rows.get(&src) {
                    Some(r) => r,
                    None => continue,
                };
                for &row_idx in row_indices {
                    let left_row = &left.rows[row_idx];
                    if let Some(o_lc) = o_left_col {
                        if left_row.get(o_lc).copied().flatten() != Some(dst) { continue; }
                    }
                    let mut row = vec![None; result.variables.len()];
                    for (li, lv) in left.variables.iter().enumerate() {
                        if let Some(oi) = result.variable_index(lv) {
                            row[oi] = left_row.get(li).copied().flatten();
                        }
                    }
                    if let Some(sc) = out_s_col { row[sc] = Some(src); }
                    if let Some(oc) = out_o_col { row[oc] = Some(dst); }
                    result.rows.push(row);
                    if result.rows.len() >= self.config.max_intermediate_rows {
                        result.overflow = true;
                        return result;
                    }
                    if self.is_limit_reached(result.rows.len()) { return result; }
                }
=======
        // Single sequential scan through all cached pairs (O(M_cache)).
        for &(src, dst) in cached.iter() {
            if let Some(oid) = o_filter { if dst != oid { continue; } }
            let row_indices = match src_to_rows.get(&src) {
                Some(r) => r,
                None => continue,
            };
            for &row_idx in row_indices {
                let left_row = &left.rows[row_idx];
                if let Some(o_lc) = o_left_col {
                    if left_row.get(o_lc).copied().flatten() != Some(dst) { continue; }
                }
                let mut row = vec![None; result.variables.len()];
                for (li, lv) in left.variables.iter().enumerate() {
                    if let Some(oi) = result.variable_index(lv) {
                        row[oi] = left_row.get(li).copied().flatten();
                    }
                }
                if let Some(sc) = out_s_col { row[sc] = Some(src); }
                if let Some(oc) = out_o_col { row[oc] = Some(dst); }
                result.rows.push(row);
                if result.rows.len() >= self.config.max_intermediate_rows {
                    result.overflow = true;
                    return result;
                }
                if self.is_limit_reached(result.rows.len()) { return result; }
>>>>>>> 583087b0f936c7c64e6a487eb4dfd6a098398d26
            }
        }

        tracing::debug!(
            left_rows = left.rows.len(),
            cached_pairs = cached.len(),
            out_rows = result.rows.len(),
            elapsed_us = t0.elapsed().as_micros(),
<<<<<<< HEAD
            strategy = if left.rows.len() < BINARY_SEARCH_THRESHOLD { "binary_search" } else { "linear_scan" },
=======
>>>>>>> 583087b0f936c7c64e6a487eb4dfd6a098398d26
            "path_cache_merge_join done"
        );
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
        // Pre-compute GROUP BY column indices — used by both fast and slow path.
        let gb_col_indices: Vec<Option<usize>> = query.group_by.iter()
            .map(|gc| if let Expression::Variable(v) = &gc.expr { rs.variable_index(v) } else { None })
            .collect();

        // Determine output variables and per-column aggregate kind.
        //
        // Fast path (streaming accumulator) avoids storing all rows per group
        // and eliminates per-row row_to_binding calls.  It handles the most
        // common GROUP BY patterns: COUNT(*), COUNT(?v), COUNT(DISTINCT ?v)
        // where the inner expression is a simple variable.
        //
        // Slow path (row-storage) is used when an output column requires
        // general expression evaluation (SUM, AVG, GroupConcat, etc.).
        #[derive(Clone)]
        enum ColKind {
            GbKey(usize),          // GROUP BY variable at key index i
            CountStar,             // COUNT(*)
            Count(usize),          // COUNT(?v): column index in rs
            CountDistinct(usize),  // COUNT(DISTINCT ?v): column index in rs
            Complex,               // needs general eval_aggregate (row storage required)
        }

        let mut out_vars: Vec<String> = Vec::new();
        let mut col_kinds: Vec<ColKind> = Vec::new();

        if let Projection::Variables(items) = &query.projection {
            for item in items {
                match item {
                    SelectItem::Variable(v) => {
                        out_vars.push(v.clone());
                        let gb_i = query.group_by.iter().position(|gc| {
                            matches!(&gc.expr, Expression::Variable(gv) if gv == v)
                        }).unwrap_or(0);
                        col_kinds.push(ColKind::GbKey(gb_i));
                    }
                    SelectItem::Alias(expr, name) => {
                        out_vars.push(name.clone());
                        let kind = match expr {
                            Expression::Count { distinct: false, expr: None } =>
                                ColKind::CountStar,
                            Expression::Count { distinct: false, expr: Some(inner) } => {
                                if let Expression::Variable(v) = inner.as_ref() {
                                    if let Some(ci) = rs.variable_index(v) {
                                        ColKind::Count(ci)
                                    } else { ColKind::Complex }
                                } else { ColKind::Complex }
                            }
                            Expression::Count { distinct: true, expr: Some(inner) } => {
                                if let Expression::Variable(v) = inner.as_ref() {
                                    if let Some(ci) = rs.variable_index(v) {
                                        ColKind::CountDistinct(ci)
                                    } else { ColKind::Complex }
                                } else { ColKind::Complex }
                            }
                            _ => ColKind::Complex,
                        };
                        col_kinds.push(kind);
                    }
                }
            }
        }

        let can_stream = col_kinds.iter().all(|k| !matches!(k, ColKind::Complex));

        // Sort-then-stream is optimal when there are no CountDistinct columns:
        // the Vec<key> allocation (O(N) mallocs) dominates and sort removes it.
        // When CountDistinct is present the HashSet operations dominate (~667 ns/insert)
        // and the sort overhead (~4 s for 5.3 M elements) outweighs any key savings.
        let has_distinct = col_kinds.iter().any(|k| matches!(k, ColKind::CountDistinct(_)));

        if can_stream && !has_distinct {
            // ── Fast path A: sort-then-stream (no CountDistinct) ───────────────
            // New approach:
            // 1. Extract all GROUP BY keys into one flat array (single allocation).
            // 2. Sort row indices by key slice — O(N) when input is already ordered
            //    by the GROUP BY variable (common when the outer-loop variable is
            //    the GROUP BY key, e.g. D-1 where ?dataset drives the first scan).
            // 3. Single sequential pass emitting each group when the key changes.
            //    No HashMap, no per-row allocation, one HashSet active at a time.
            enum Acc { Count(usize), Distinct(HashSet<TermId>) }

            let key_width = gb_col_indices.len();

            // One flat Vec<Option<TermId>> for all row keys (key_width entries per row).
            let flat_keys: Vec<Option<TermId>> = rs.rows.iter().flat_map(|row| {
                gb_col_indices.iter().map(move |&ci| {
                    ci.and_then(|i| row.get(i).copied().flatten())
                })
            }).collect();

            let mut indices: Vec<usize> = (0..rs.rows.len()).collect();
            indices.sort_unstable_by(|&a, &b| {
                let ka = &flat_keys[a * key_width .. (a + 1) * key_width];
                let kb = &flat_keys[b * key_width .. (b + 1) * key_width];
                ka.cmp(kb)
            });

            let make_accs = || col_kinds.iter().filter_map(|k| match k {
                ColKind::CountStar | ColKind::Count(_) => Some(Acc::Count(0)),
                ColKind::CountDistinct(_) => Some(Acc::Distinct(HashSet::new())),
                ColKind::GbKey(_) | ColKind::Complex => None,
            }).collect::<Vec<Acc>>();

            let out_len = out_vars.len();
            let mut result = ResultSet::empty(out_vars);
            result.overflow = rs.overflow;

            if indices.is_empty() { return result; }

            let mut group_start = indices[0];
            let mut accs = make_accs();

            // Inline emit: build output row from current group's key + accs.
            macro_rules! emit_group {
                ($key_row:expr, $accs:expr) => {{
                    let key = &flat_keys[$key_row * key_width .. ($key_row + 1) * key_width];
                    let mut out_row = vec![None; out_len];
                    let mut acc_idx = 0;
                    for (out_i, kind) in col_kinds.iter().enumerate() {
                        out_row[out_i] = match kind {
                            ColKind::GbKey(ki) => key.get(*ki).copied().flatten(),
                            ColKind::CountStar | ColKind::Count(_) => {
                                let n = if let Acc::Count(c) = &$accs[acc_idx] { *c } else { 0 };
                                acc_idx += 1;
                                let s = format!("\"{}\"^^<http://www.w3.org/2001/XMLSchema#integer>", n);
                                Some(self.dict.encode(&s))
                            }
                            ColKind::CountDistinct(_) => {
                                let n = if let Acc::Distinct(set) = &$accs[acc_idx] { set.len() } else { 0 };
                                acc_idx += 1;
                                let s = format!("\"{}\"^^<http://www.w3.org/2001/XMLSchema#integer>", n);
                                Some(self.dict.encode(&s))
                            }
                            ColKind::Complex => None,
                        };
                    }
                    result.rows.push(out_row);
                }};
            }

            for &row_idx in &indices {
                let cur  = &flat_keys[row_idx   * key_width .. (row_idx   + 1) * key_width];
                let prev = &flat_keys[group_start * key_width .. (group_start + 1) * key_width];
                if cur != prev {
                    emit_group!(group_start, accs);
                    group_start = row_idx;
                    accs = make_accs();
                }

                let row = &rs.rows[row_idx];
                let mut acc_idx = 0;
                for kind in &col_kinds {
                    match kind {
                        ColKind::GbKey(_) | ColKind::Complex => {}
                        ColKind::CountStar => {
                            if let Acc::Count(c) = &mut accs[acc_idx] { *c += 1; }
                            acc_idx += 1;
                        }
                        ColKind::Count(ci) => {
                            if let Acc::Count(c) = &mut accs[acc_idx] {
                                if row.get(*ci).copied().flatten().is_some() { *c += 1; }
                            }
                            acc_idx += 1;
                        }
                        ColKind::CountDistinct(ci) => {
                            if let Acc::Distinct(set) = &mut accs[acc_idx] {
                                if let Some(id) = row.get(*ci).copied().flatten() {
                                    set.insert(id);
                                }
                            }
                            acc_idx += 1;
                        }
                    }
                }
            }
            emit_group!(group_start, accs);  // final group
            return result;
        }

        if can_stream && has_distinct {
            // ── Fast path B: HashMap accumulator (with CountDistinct) ──────────
            // HashSet.insert dominates (~667 ns/insert); sort overhead > key-alloc
            // savings so we keep the HashMap path and only skip per-row key cloning
            // via the shared flat_keys array (avoids per-row Vec<Option<TermId>> alloc).
            enum Acc { Count(usize), Distinct(HashSet<TermId>) }

            let key_width = gb_col_indices.len();
            let flat_keys: Vec<Option<TermId>> = rs.rows.iter().flat_map(|row| {
                gb_col_indices.iter().map(move |&ci| {
                    ci.and_then(|i| row.get(i).copied().flatten())
                })
            }).collect();

            // Use &[Option<TermId>] slice as logical key — but HashMap needs owned key.
            // To avoid 5.3M Vec allocs, bucket rows by (row_index) using a pre-sized map.
            // We still need one key Vec per GROUP (not per row): at most N_groups allocs.
            let mut groups: HashMap<Vec<Option<TermId>>, Vec<Acc>> =
                HashMap::with_capacity(1 << 10);

            for (row_idx, row) in rs.rows.iter().enumerate() {
                // Borrow key slice from flat_keys — compare without allocating.
                let key_slice = &flat_keys[row_idx * key_width .. (row_idx + 1) * key_width];
                // `entry` needs an owned key; use raw_entry to probe first.
                let accs = groups.entry(key_slice.to_vec()).or_insert_with(|| {
                    col_kinds.iter().filter_map(|k| match k {
                        ColKind::CountStar | ColKind::Count(_) => Some(Acc::Count(0)),
                        ColKind::CountDistinct(_) => Some(Acc::Distinct(HashSet::new())),
                        ColKind::GbKey(_) | ColKind::Complex => None,
                    }).collect()
                });

                let mut acc_idx = 0;
                for kind in &col_kinds {
                    match kind {
                        ColKind::GbKey(_) | ColKind::Complex => {}
                        ColKind::CountStar => {
                            if let Acc::Count(c) = &mut accs[acc_idx] { *c += 1; }
                            acc_idx += 1;
                        }
                        ColKind::Count(ci) => {
                            if let Acc::Count(c) = &mut accs[acc_idx] {
                                if row.get(*ci).copied().flatten().is_some() { *c += 1; }
                            }
                            acc_idx += 1;
                        }
                        ColKind::CountDistinct(ci) => {
                            if let Acc::Distinct(set) = &mut accs[acc_idx] {
                                if let Some(id) = row.get(*ci).copied().flatten() {
                                    set.insert(id);
                                }
                            }
                            acc_idx += 1;
                        }
                    }
                }
            }

            let out_len = out_vars.len();
            let mut result = ResultSet::empty(out_vars);
            result.overflow = rs.overflow;
            for (key, accs) in groups {
                let mut out_row = vec![None; out_len];
                let mut acc_idx = 0;
                for (out_i, kind) in col_kinds.iter().enumerate() {
                    out_row[out_i] = match kind {
                        ColKind::GbKey(ki) => key.get(*ki).copied().flatten(),
                        ColKind::CountStar | ColKind::Count(_) => {
                            let n = if let Acc::Count(c) = &accs[acc_idx] { *c } else { 0 };
                            acc_idx += 1;
                            let s = format!("\"{}\"^^<http://www.w3.org/2001/XMLSchema#integer>", n);
                            Some(self.dict.encode(&s))
                        }
                        ColKind::CountDistinct(_) => {
                            let n = if let Acc::Distinct(set) = &accs[acc_idx] { set.len() } else { 0 };
                            acc_idx += 1;
                            let s = format!("\"{}\"^^<http://www.w3.org/2001/XMLSchema#integer>", n);
                            Some(self.dict.encode(&s))
                        }
                        ColKind::Complex => None,
                    };
                }
                result.rows.push(out_row);
            }
            return result;
        }

        // ── Slow path: store rows per group, then compute aggregates ──────────
        let mut groups: HashMap<Vec<Option<TermId>>, Vec<Vec<Option<TermId>>>> = HashMap::new();

        for row in &rs.rows {
            let key: Vec<Option<TermId>> = gb_col_indices.iter()
                .map(|ci| ci.and_then(|i| row.get(i).copied().flatten()))
                .collect();
            groups.entry(key).or_default().push(row.clone());
        }

        let mut result = ResultSet::empty(out_vars.clone());
        result.overflow = rs.overflow;

        for (key, group_rows) in groups {
            let mut row = vec![None; out_vars.len()];

            let key_binding: Binding = query.group_by.iter().zip(key.iter())
                .filter_map(|(gc, val)| {
                    if let Expression::Variable(v) = &gc.expr {
                        val.map(|id| (v.clone(), id))
                    } else { None }
                })
                .collect();

            for (i, gc) in query.group_by.iter().enumerate() {
                if let Expression::Variable(v) = &gc.expr {
                    if let Some(out_i) = result.variable_index(v) {
                        row[out_i] = key.get(i).copied().flatten();
                    }
                }
            }

            if let Projection::Variables(items) = &query.projection {
                for (out_i, item) in items.iter().enumerate() {
                    if let SelectItem::Alias(expr, _) = item {
                        row[out_i] = if is_aggregate_expr(expr) {
                            self.eval_aggregate(expr, &group_rows, rs)
                        } else {
                            self.eval_term(expr, &key_binding)
                        };
                    }
                }
            }
            result.rows.push(row);
        }
        result
    }

    fn eval_aggregate(&self, expr: &Expression, rows: &[Vec<Option<TermId>>], rs: &ResultSet) -> Option<TermId> {
        match expr {
            Expression::Count { distinct, expr: inner } => {
                let count = if inner.is_none() {
                    // COUNT(*) — count all rows in the group.
                    rows.len()
                } else if *distinct {
                    // COUNT(DISTINCT ?x) — collect distinct TermId values.
                    let mut seen = HashSet::new();
                    for row in rows {
                        let b = row_to_binding(&rs.variables, row);
                        if let Some(inner_expr) = inner {
                            if let Some(id) = self.eval_term(inner_expr, &b) {
                                seen.insert(id);
                            }
                        }
                    }
                    seen.len()
                } else {
                    // COUNT(?x) — count rows where ?x is bound.
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
            Expression::Sum { distinct, expr } => {
                // Collect bound TermIds, optionally deduplicating.
                let ids: Vec<TermId> = if *distinct {
                    let mut seen = HashSet::new();
                    rows.iter().filter_map(|row| {
                        let b = row_to_binding(&rs.variables, row);
                        self.eval_term(expr, &b).filter(|id| seen.insert(*id))
                    }).collect()
                } else {
                    rows.iter().filter_map(|row| {
                        let b = row_to_binding(&rs.variables, row);
                        self.eval_term(expr, &b)
                    }).collect()
                };
                let sum: f64 = ids.iter().filter_map(|&id| {
                    parse_numeric(&self.dict.decode(id))
                }).sum();
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
            Expression::Avg { distinct, expr } => {
                let vals: Vec<f64> = if *distinct {
                    let mut seen = HashSet::new();
                    rows.iter().filter_map(|row| {
                        let b = row_to_binding(&rs.variables, row);
                        self.eval_term(expr, &b)
                            .filter(|id| seen.insert(*id))
                            .and_then(|id| parse_numeric(&self.dict.decode(id)))
                    }).collect()
                } else {
                    rows.iter().filter_map(|row| {
                        let b = row_to_binding(&rs.variables, row);
                        self.eval_term(expr, &b).and_then(|id| {
                            parse_numeric(&self.dict.decode(id))
                        })
                    }).collect()
                };
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
            // GROUP_CONCAT(?x [DISTINCT]; separator="...") — concatenate values.
            Expression::GroupConcat { distinct, expr, separator } => {
                let sep = separator.as_deref().unwrap_or(" ");
                let parts: Vec<String> = if *distinct {
                    let mut seen = HashSet::new();
                    rows.iter().filter_map(|row| {
                        let b = row_to_binding(&rs.variables, row);
                        self.eval_string(expr, &b).filter(|s| seen.insert(s.clone()))
                    }).collect()
                } else {
                    rows.iter().filter_map(|row| {
                        let b = row_to_binding(&rs.variables, row);
                        self.eval_string(expr, &b)
                    }).collect()
                };
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
            Expression::IsNumeric(e) => {
                let id = self.eval_term(e, binding)?;
                let s = self.dict.decode(id);
                Some(parse_numeric(&s).is_some())
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
            Expression::In(expr, list) => {
                let left_id = self.eval_term(expr, binding);
                let left_str = left_id
                    .and_then(|id| Some(self.dict.decode(id)))
                    .or_else(|| self.eval_string(expr, binding));

                if left_id.is_none() && left_str.is_none() {
                    return None;
                }

                for item in list {
                    let right_id = self.eval_term(item, binding);
                    let equal = match (left_id, right_id) {
                        (Some(a), Some(b)) => {
                            a == b || self.dict.decode(a) == self.dict.decode(b)
                        }
                        _ => {
                            let ls = left_str.as_deref();
                            let rs = right_id
                                .map(|id| self.dict.decode(id))
                                .or_else(|| self.eval_string(item, binding));
                            matches!((ls, rs.as_deref()), (Some(a), Some(b)) if a == b)
                        }
                    };
                    if equal {
                        return Some(true);
                    }
                }
                Some(false)
            }
            Expression::NotIn(expr, list) => {
                self.eval_bool(&Expression::In(expr.clone(), list.clone()), binding)
                    .map(|v| !v)
            }
            Expression::Exists(pattern) => {
                let plan = optimize_bgp(pattern, self.index, self.dict, self.stats);
                let result = self.execute_plan_with_ctx(&plan, binding);
                Some(!result.rows.is_empty())
            }
            Expression::NotExists(pattern) => {
                let plan = optimize_bgp(pattern, self.index, self.dict, self.stats);
                let result = self.execute_plan_with_ctx(&plan, binding);
                Some(result.rows.is_empty())
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
            // REPLACE(?str, ?pattern, ?replacement [, ?flags])
            // Applies a regex substitution; honours the 'i' flag for case-insensitive matching.
            Expression::Replace(s_expr, pat_expr, repl_expr, flags_expr) => {
                let text = self.eval_string(s_expr, binding)?;
                let pat  = self.eval_string(pat_expr, binding)?;
                let repl = self.eval_string(repl_expr, binding)?;
                let flag_str = flags_expr
                    .as_ref()
                    .and_then(|f| self.eval_string(f, binding))
                    .unwrap_or_default();
                let full_pat = if flag_str.contains('i') {
                    format!("(?i){}", pat)
                } else {
                    pat
                };
                let replaced = regex::Regex::new(&full_pat)
                    .ok()
                    .map(|re| re.replace_all(&text, repl.as_str()).into_owned())?;
                Some(self.dict.encode(&format!("\"{}\"", replaced)))
            }

            // ── STRLEN(?str) → xsd:integer ────────────────────────────────────
            // Returns the number of Unicode code points (chars) in the lexical form.
            Expression::Strlen(e) => {
                let s = self.eval_string(e, binding)?;
                let n = s.chars().count();
                Some(self.dict.encode(&format!(
                    "\"{}\"^^<http://www.w3.org/2001/XMLSchema#integer>", n
                )))
            }

            // ── SUBSTR(?str, startingLoc [, length]) → plain literal ──────────
            // SPARQL 1.1 §17.4.3.3 — 1-based character positions.
            // startingLoc < 1 is clamped to 1 (chars before the string are empty).
            Expression::Substr(s_expr, start_expr, len_expr) => {
                let s     = self.eval_string(s_expr, binding)?;
                let start = {
                    let v = self.eval_term(start_expr, binding)?;
                    let sv = self.dict.decode(v);
                    parse_numeric(&sv)? as isize
                };
                let chars: Vec<char> = s.chars().collect();
                let n = chars.len() as isize;
                // Convert 1-based start to 0-based, clamping to [0, n].
                let begin = (start - 1).max(0).min(n) as usize;
                let slice: String = match len_expr {
                    Some(len_e) => {
                        let lv = self.eval_term(len_e, binding)?;
                        let ls = self.dict.decode(lv);
                        let len = parse_numeric(&ls)? as isize;
                        // end = begin + len, but also clamped; negative len → ""
                        let end = (begin as isize + len).max(begin as isize).min(n) as usize;
                        chars[begin..end].iter().collect()
                    }
                    None => chars[begin..].iter().collect(),
                };
                Some(self.dict.encode(&format!("\"{}\"", slice)))
            }

            // ── STRBEFORE(?str, ?marker) → plain literal ──────────────────────
            // Returns the part of str before the first occurrence of marker.
            // If marker is "" → "". If not found → "".
            Expression::StrBefore(s_expr, m_expr) => {
                let s = self.eval_string(s_expr, binding)?;
                let m = self.eval_string(m_expr, binding)?;
                let result = if m.is_empty() {
                    String::new()
                } else {
                    s.find(m.as_str())
                        .map(|i| s[..i].to_string())
                        .unwrap_or_default()
                };
                Some(self.dict.encode(&format!("\"{}\"", result)))
            }

            // ── STRAFTER(?str, ?marker) → plain literal ───────────────────────
            // Returns the part of str after the first occurrence of marker.
            // If marker is "" → str. If not found → "".
            Expression::StrAfter(s_expr, m_expr) => {
                let s = self.eval_string(s_expr, binding)?;
                let m = self.eval_string(m_expr, binding)?;
                let result = if m.is_empty() {
                    s.clone()
                } else {
                    s.find(m.as_str())
                        .map(|i| s[i + m.len()..].to_string())
                        .unwrap_or_default()
                };
                Some(self.dict.encode(&format!("\"{}\"", result)))
            }

            // ── ENCODE_FOR_URI(?str) → plain literal ──────────────────────────
            // Percent-encodes all characters except unreserved per RFC 3986.
            Expression::EncodeForUri(e) => {
                let s = self.eval_string(e, binding)?;
                let encoded = percent_encoding::utf8_percent_encode(
                    &s,
                    percent_encoding::NON_ALPHANUMERIC,
                ).to_string();
                Some(self.dict.encode(&format!("\"{}\"", encoded)))
            }

            // ── IRI(?str) / URI(?str) → IRI ───────────────────────────────────
            // Constructs an IRI from a string literal.
            Expression::Iri2(e) => {
                let s = self.eval_string(e, binding)?;
                // Look up the bare IRI (as stored in the dictionary without <>).
                self.dict.lookup(&s)
            }

            // ── Date/time component extraction ────────────────────────────────
            // These operate on xsd:dateTime literals of the form
            // "YYYY-MM-DDTHH:MM:SS[.fff][Z|±HH:MM]"^^xsd:dateTime.
            Expression::Year(e) => {
                let s = self.eval_string(e, binding)?;
                let year = s.splitn(2, '-').next()?.parse::<i32>().ok()?;
                Some(self.dict.encode(&format!(
                    "\"{}\"^^<http://www.w3.org/2001/XMLSchema#integer>", year
                )))
            }
            Expression::Month(e) => {
                let s = self.eval_string(e, binding)?;
                let parts: Vec<&str> = s.splitn(3, '-').collect();
                let m = parts.get(1)?.parse::<u32>().ok()?;
                Some(self.dict.encode(&format!(
                    "\"{}\"^^<http://www.w3.org/2001/XMLSchema#integer>", m
                )))
            }
            Expression::Day(e) => {
                let s = self.eval_string(e, binding)?;
                let day_part = s.splitn(3, '-').nth(2)?;
                let d = day_part.splitn(2, 'T').next()?.parse::<u32>().ok()?;
                Some(self.dict.encode(&format!(
                    "\"{}\"^^<http://www.w3.org/2001/XMLSchema#integer>", d
                )))
            }
            Expression::Hours(e) => {
                let s = self.eval_string(e, binding)?;
                let t = s.splitn(2, 'T').nth(1)?;
                let h = t.splitn(2, ':').next()?.parse::<u32>().ok()?;
                Some(self.dict.encode(&format!(
                    "\"{}\"^^<http://www.w3.org/2001/XMLSchema#integer>", h
                )))
            }
            Expression::Minutes(e) => {
                let s = self.eval_string(e, binding)?;
                let t = s.splitn(2, 'T').nth(1)?;
                let m = t.splitn(3, ':').nth(1)?.parse::<u32>().ok()?;
                Some(self.dict.encode(&format!(
                    "\"{}\"^^<http://www.w3.org/2001/XMLSchema#integer>", m
                )))
            }
            Expression::Seconds(e) => {
                let s = self.eval_string(e, binding)?;
                let t = s.splitn(2, 'T').nth(1)?;
                let sec_str = t.splitn(3, ':').nth(2)?;
                // Strip timezone suffix (Z, +HH:MM, -HH:MM) if present.
                let sec_num = sec_str.trim_end_matches(|c: char| c == 'Z')
                    .split('+').next()
                    .and_then(|s| s.split('-').next())
                    .unwrap_or(sec_str);
                let sec = sec_num.parse::<f64>().ok()?;
                Some(self.dict.encode(&format!(
                    "\"{}\"^^<http://www.w3.org/2001/XMLSchema#decimal>", sec
                )))
            }
            Expression::Now => {
                // Return current UTC time as xsd:dateTime.
                // Uses std::time; format: "YYYY-MM-DDTHH:MM:SSZ"
                use std::time::{SystemTime, UNIX_EPOCH};
                let secs = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                // Simple UTC conversion (no leap seconds).
                let s = secs % 60;
                let m = (secs / 60) % 60;
                let h = (secs / 3600) % 24;
                let days = secs / 86400;
                // Gregorian calendar (days since 1970-01-01).
                let (year, month, day) = days_to_ymd(days as u64);
                let dt = format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", year, month, day, h, m, s);
                Some(self.dict.encode(&format!(
                    "\"{}\"^^<http://www.w3.org/2001/XMLSchema#dateTime>", dt
                )))
            }

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
            // ScanBound inside GRAPH: outer_vars are unsubstituted (no outer context
            // here), so those positions stay UNBOUND (wildcard).  Correctness is
            // maintained by the enclosing hash_join that will filter the rows.
            ExecutionPlan::ScanBound { base, free_vars, outer_vars: _ } => {
                let pattern = *base;
                let mut rs = ResultSet::empty(free_vars.iter().map(|(n, _)| n.clone()).collect());
                for triple in gspo.scan_graph(g_id, &pattern) {
                    let mut row = vec![None; free_vars.len()];
                    for (i, (_, pos)) in free_vars.iter().enumerate() {
                        row[i] = Some(match pos { 0 => triple.s, 1 => triple.p, _ => triple.o });
                    }
                    rs.rows.push(row);
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
            ExecutionPlan::ScanBound { free_vars, outer_vars, .. } => {
                for (v, _) in free_vars.iter().chain(outer_vars.iter()) {
                    if !out.contains(v) { out.push(v.clone()); }
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

    /// Evaluate a Sequence property path with an optional subject filter applied
    /// after step 0 ("filtered sequential scan" optimization).
    ///
    /// ## Why this exists
    ///
    /// When hash_join is chosen for a 2-hop Sequence path (e.g. `faldo:begin /
    /// faldo:position`), the normal flow is:
    ///
    /// 1. Step 0: full POS scan → 11.8M (subject, mid) pairs
    /// 2. Build 11.6M-entry HashMap from those pairs (slow, ~1 s RAM)
    /// 3. Step 1: full POS scan of next predicate, filter by HashMap → still slow
    ///
    /// When the path's subject variable is also a join variable bound in the left
    /// ResultSet (e.g. 508 peptide IDs), we can filter step 0's output immediately:
    ///
    /// 1. Step 0: full POS scan → 11.8M pairs (same sequential I/O cost)
    /// 2. **Filter**: retain only pairs whose source is in `subject_filter` → 508 pairs
    /// 3. Build 508-entry HashMap → negligible
    /// 4. Step 1: full POS scan + filter by 508-entry HashMap → fast
    ///
    /// Empirical impact (JPostDB, faldo 2-hop, N=508 left subjects):
    ///   Without filter: step0=2.4s, step1=13s, hash_join=3s → ~18s total
    ///   With filter:    step0=2.4s, filter=0.2s, step1=2.5s → ~5s total
    ///
    /// This is NOT bind_join: step 0 is still a full sequential scan (fast I/O),
    /// not 508 random SPO probes (150ms each = 76s).  The filter is O(N_step0)
    /// with O(1) HashSet lookups, applied after the sequential read.
    fn eval_sequence_with_subject_filter(
        &self,
        steps: &[PropertyPath],
        s: Option<TermId>,
        o: Option<TermId>,
        subject_filter: &HashSet<TermId>,
    ) -> Vec<(TermId, TermId)> {
        if steps.is_empty() { return Vec::new(); }

        // Path cache lookup — same as eval_path Sequence.
        // Even with a subject filter, if the path is cached we can serve it from RAM
        // and apply the filter there (cache hit is always faster).
        if !self.path_cache.is_empty() {
            let path_ids: Option<Vec<TermId>> = steps.iter()
                .map(|step| {
                    if let PropertyPath::Iri(iri) = step { self.dict.lookup(iri) } else { None }
                })
                .collect();
            if let Some(ids) = path_ids {
                if let Some(cached) = self.path_cache.get(&ids) {
                    // Apply endpoint filters then subject filter.
                    let base: Vec<(TermId, TermId)> = match (s, o) {
                        (None, None) => (*cached).clone(),
                        (Some(sid), None) => {
                            let lo = cached.partition_point(|&(cs, _)| cs < sid);
                            let hi = lo + cached[lo..].partition_point(|&(cs, _)| cs == sid);
                            cached[lo..hi].to_vec()
                        }
                        (None, Some(oid)) => {
                            cached.iter().filter(|&&(_, co)| co == oid).copied().collect()
                        }
                        (Some(sid), Some(oid)) => {
                            if cached.binary_search(&(sid, oid)).is_ok() {
                                vec![(sid, oid)]
                            } else {
                                Vec::new()
                            }
                        }
                    };
                    return base.into_iter()
                        .filter(|(subj, _)| subject_filter.contains(subj))
                        .collect();
                }
            }
        }

        // ── Step 0 ───────────────────────────────────────────────────────────────
        //
        // Two strategies, in preference order:
        //
        // A) pred_cache per-subject probe — O(|filter| × log M):
        //    When the step-0 predicate is in pred_cache, probe once per subject
        //    in the filter rather than cloning/scanning all M pairs.
        //    For M=11.8M faldo:begin pairs and |filter|=263 subjects:
        //      Full scan:  O(11.8M) reads + 11.8M filter checks ≈ 0.56 s (HDD)
        //      Per-probe:  O(263 × log₂(11.8M)) = 263 × 23 ≈ 6000 RAM ops ≈ <1 ms
        //
        // B) Full scan + retain — O(M):
        //    Fallback when step-0 predicate is not in pred_cache.  One sequential
        //    POS read then a linear filter pass.
        let t_seq = std::time::Instant::now();
        let step0_o = if steps.len() == 1 { o } else { None };

        let mut current: Vec<(TermId, TermId)> =
            if let PropertyPath::Iri(ref iri) = steps[0] {
                if let Some(pred_id) = self.dict.lookup(iri) {
                    if let Some(cached) = self.pred_cache.get(pred_id) {
                        // Strategy A: probe pred_cache once per subject.
                        //
                        // Sort subjects so binary-search positions advance monotonically,
                        // allowing sequential cache traversal (no random seeks in RAM).
                        let mut sorted_subjects: Vec<TermId> =
                            subject_filter.iter().copied().collect();
                        sorted_subjects.sort_unstable();

                        let mut result = Vec::with_capacity(sorted_subjects.len() * 2);
                        for sid in sorted_subjects {
                            let lo = cached.partition_point(|&(cs, _)| cs < sid);
                            for &(cs, co) in &cached[lo..] {
                                if cs != sid { break; }
                                if step0_o.map_or(true, |eo| co == eo) {
                                    result.push((sid, co));
                                }
                            }
                        }

                        tracing::debug!(
                            step = 0,
                            subjects_probed = subject_filter.len(),
                            pairs_out = result.len(),
                            cached_pairs = cached.len(),
                            elapsed_us = t_seq.elapsed().as_micros(),
                            "eval_seq_filtered: step 0 pred_cache probe (skipped full scan)"
                        );
                        result
                    } else {
                        // Strategy B: full scan + retain.
                        let mut v = self.eval_path(&steps[0], s, step0_o);
                        let pairs_raw = v.len();
                        v.retain(|(subj, _)| subject_filter.contains(subj));
                        tracing::debug!(
                            step = 0,
                            pairs_raw,
                            pairs_after_filter = v.len(),
                            elapsed_us = t_seq.elapsed().as_micros(),
                            "eval_seq_filtered: step 0 raw (no pred_cache)"
                        );
                        v
                    }
                } else {
                    // IRI not in dictionary → no matching triples.
                    Vec::new()
                }
            } else {
                // Non-IRI step (Inverse / Alternative / etc.): full eval + retain.
                let mut v = self.eval_path(&steps[0], s, step0_o);
                v.retain(|(subj, _)| subject_filter.contains(subj));
                v
            };

        // Logging is embedded in each strategy branch above.

        // ── Steps 1+ ─────────────────────────────────────────────────────────────
        // Identical to eval_path Sequence; current is now tiny (e.g. 508 pairs).
        for (idx, step) in steps[1..].iter().enumerate() {
            let t_step = std::time::Instant::now();
            let is_last = idx == steps.len() - 2;
            let step_o = if is_last { o } else { None };
            let mut next: Vec<(TermId, TermId)> = Vec::new();

            const CACHE_BSEARCH_THRESHOLD: usize = 32;
            let used_cache = if let PropertyPath::Iri(ref iri) = *step {
                if let Some(pred_id) = self.dict.lookup(iri) {
                    if let Some(cached) = self.pred_cache.get(pred_id) {
                        if current.len() > CACHE_BSEARCH_THRESHOLD {
                            tracing::debug!(
                                step = idx + 1,
                                current_pairs = current.len(),
                                cached_pairs = cached.len(),
                                mode = "pred_cache_merge_join",
                                "eval_seq_filtered: cached predicate merge-join"
                            );
                            predcache::merge_join_unsorted(&mut current, &*cached, step_o, &mut next);
                        } else {
                            tracing::debug!(
                                step = idx + 1,
                                current_pairs = current.len(),
                                cached_pairs = cached.len(),
                                mode = "pred_cache_bsearch",
                                "eval_seq_filtered: cached predicate binary-search"
                            );
                            for &(src, mid) in current.iter() {
                                let lo = cached.partition_point(|&(cs, _)| cs < mid);
                                let hi = lo + cached[lo..].partition_point(|&(cs, _)| cs == mid);
                                for &(_, dst) in &cached[lo..hi] {
                                    if step_o.map_or(true, |eo| eo == dst) {
                                        next.push((src, dst));
                                    }
                                }
                            }
                        }
                        true
                    } else { false }
                } else { false }
            } else { false };

            if !used_cache {
                let mut by_mid: HashMap<TermId, Vec<TermId>> = HashMap::new();
                for (a, b) in &current {
                    by_mid.entry(*b).or_default().push(*a);
                }
                let unique_mids = by_mid.len();
                const BATCH_SCAN_THRESHOLD: usize = 32;
                if unique_mids > BATCH_SCAN_THRESHOLD {
                    tracing::debug!(
                        step = idx + 1,
                        unique_mids,
                        mode = "batch_scan",
                        "eval_seq_filtered: batch scan"
                    );
                    let all_pairs = self.eval_path(step, None, step_o);
                    for (s, o) in all_pairs {
                        if let Some(srcs) = by_mid.get(&s) {
                            for &src in srcs { next.push((src, o)); }
                        }
                    }
                } else {
                    for (mid, srcs) in by_mid {
                        for (_, dst) in self.eval_path(step, Some(mid), step_o) {
                            for &src in &srcs { next.push((src, dst)); }
                        }
                    }
                }
            }

            tracing::debug!(
                step = idx + 1,
                pairs_out = next.len(),
                elapsed_us = t_step.elapsed().as_micros(),
                "eval_seq_filtered: step done"
            );
            current = next;
        }

        tracing::debug!(
            total_us = t_seq.elapsed().as_micros(),
            final_pairs = current.len(),
            "eval_seq_filtered: done"
        );
        current
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

                // Fast path: serve from pred_cache when available.
                // - (None, None)   → return clone of all cached pairs (sorted by S).
                // - (Some, None)   → binary search for s range → O(log N) seeks.
                // - (Some, Some)   → binary search for exact (s, o) → O(log N).
                // - (None, Some)   → skip cache (sorted by S, not O; scan is better).
                if let Some(cached) = self.pred_cache.get(p_id) {
                    match (s, o) {
                        (Some(sid), None) => {
                            // Subject fixed: binary search for the run [sid, sid+1).
                            let lo = cached.partition_point(|&(cs, _)| cs < sid);
                            let hi = lo + cached[lo..].partition_point(|&(cs, _)| cs == sid);
                            return cached[lo..hi].iter().map(|&(_, co)| (sid, co)).collect();
                        }
                        (Some(sid), Some(oid)) => {
                            // Both fixed: single binary search.
                            if cached.binary_search(&(sid, oid)).is_ok() {
                                return vec![(sid, oid)];
                            } else {
                                return Vec::new();
                            }
                        }
                        (None, None) => {
                            // Full scan: clone the entire Vec.
                            // Expensive (may be 100+ MB) but correct; hash_join will be
                            // chosen by the cost model anyway if the left side is large.
                            // bind_join uses the (Some, None) branch above instead.
                            return (*cached).clone();
                        }
                        (None, Some(_)) => {
                            // Object fixed, subject free: cache is sorted by S, so we
                            // can't binary-search by O.  Fall through to index scan.
                        }
                    }
                }

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
                // ── Fast path: path cache lookup ──────────────────────────────
                // If all steps are simple IRI predicates AND this exact predicate
                // sequence was pre-materialised from rdf-config model.yaml, serve
                // the result from RAM without any HDD traversal.
                if !self.path_cache.is_empty() {
                    let path_ids: Option<Vec<TermId>> = steps.iter()
                        .map(|step| {
                            if let PropertyPath::Iri(iri) = step {
                                self.dict.lookup(iri)
                            } else {
                                None // Non-IRI steps can't be in the path cache
                            }
                        })
                        .collect();

                    if let Some(ids) = path_ids {
                        if let Some(cached) = self.path_cache.get(&ids) {
                            tracing::debug!(
                                steps = steps.len(),
                                cached_pairs = cached.len(),
                                mode = "path_cache_hit",
                                "eval_path Sequence: path cache hit"
                            );
                            // Apply endpoint filters on the cached (start, end) pairs
                            return match (s, o) {
                                (None, None) => (*cached).clone(),
                                (Some(sid), None) => {
                                    let lo = cached.partition_point(|&(cs, _)| cs < sid);
                                    let hi = lo + cached[lo..].partition_point(|&(cs, _)| cs == sid);
                                    cached[lo..hi].to_vec()
                                }
                                (None, Some(oid)) => {
                                    cached.iter().filter(|&&(_, co)| co == oid).copied().collect()
                                }
                                (Some(sid), Some(oid)) => {
                                    if cached.binary_search(&(sid, oid)).is_ok() {
                                        vec![(sid, oid)]
                                    } else {
                                        Vec::new()
                                    }
                                }
                            };
                        }
                    }
                }

                // Evaluate left-to-right; intermediate nodes are unbound
                let t_seq = std::time::Instant::now();
                let mut current: Vec<(TermId, TermId)> =
                    self.eval_path(&steps[0], s, if steps.len() == 1 { o } else { None });

                tracing::debug!(
                    step = 0,
                    pairs = current.len(),
                    elapsed_us = t_seq.elapsed().as_micros(),
                    "eval_path Sequence step"
                );

                for (idx, step) in steps[1..].iter().enumerate() {
                    let t_step = std::time::Instant::now();
                    let is_last = idx == steps.len() - 2;
                    let step_o = if is_last { o } else { None };
                    let mut next: Vec<(TermId, TermId)> = Vec::new();

                    // ── Fast path: cached predicate ──────────────────────────
                    // When the step is a simple IRI predicate and its (subject, object)
                    // pairs are loaded in the pred_cache, avoid the HDD scan entirely.
                    //
                    // Two strategies depending on how many intermediate nodes we have:
                    //
                    // LARGE current (>32 pairs, hash_join mode):
                    //   merge_join_unsorted — O(N_current + N_cached) linear pass.
                    //   The POS scan at step=0 gives pairs sorted by O (the mid value).
                    //   Cached data is sorted by S (=mid). Linear merge = ~500ms for 11.8M.
                    //
                    // SMALL current (≤32 pairs, bind_join per-subject mode):
                    //   Binary search per mid — O(N_current × log N_cached).
                    //   For N_current=1-2, log₂(11.8M)=23 comps → ~0.01ms.
                    //   Merge_join would do a full linear scan = 11.8M ops per call!
                    //
                    // Threshold matches BATCH_SCAN_THRESHOLD so the two code paths
                    // are consistent.
                    const CACHE_BSEARCH_THRESHOLD: usize = 32;
                    let used_cache = if let PropertyPath::Iri(ref iri) = *step {
                        if let Some(pred_id) = self.dict.lookup(iri) {
                            if let Some(cached) = self.pred_cache.get(pred_id) {
                                if current.len() > CACHE_BSEARCH_THRESHOLD {
                                    tracing::debug!(
                                        step = idx + 1,
                                        current_pairs = current.len(),
                                        cached_pairs = cached.len(),
                                        mode = "pred_cache_merge_join",
                                        "eval_path Sequence: using cached predicate merge-join"
                                    );
                                    predcache::merge_join_unsorted(&mut current, &*cached, step_o, &mut next);
                                } else {
                                    tracing::debug!(
                                        step = idx + 1,
                                        current_pairs = current.len(),
                                        cached_pairs = cached.len(),
                                        mode = "pred_cache_bsearch",
                                        "eval_path Sequence: using cached predicate binary-search"
                                    );
                                    // Binary search per mid: O(N_current × log N_cached).
                                    for &(src, mid) in current.iter() {
                                        let lo = cached.partition_point(|&(cs, _)| cs < mid);
                                        let hi = lo + cached[lo..].partition_point(|&(cs, _)| cs == mid);
                                        for &(_, dst) in &cached[lo..hi] {
                                            if step_o.map_or(true, |eo| eo == dst) {
                                                next.push((src, dst));
                                            }
                                        }
                                    }
                                }
                                true
                            } else { false }
                        } else { false }
                    } else { false };

                    if !used_cache {
                        // Group by the right-hand intermediate node
                        let mut by_mid: HashMap<TermId, Vec<TermId>> = HashMap::new();
                        for (a, b) in &current {
                            by_mid.entry(*b).or_default().push(*a);
                        }
                        let unique_mids = by_mid.len();

                        // Batch-scan threshold: when the number of unique intermediate
                        // nodes is large, N individual random SPO probes (one per node)
                        // cause excessive random I/O.  Instead, do ONE sequential scan
                        // over the predicate's entire POS/SPO range and filter in memory.
                        // This mirrors QLever's per-predicate columnar scan approach:
                        // sequential I/O is ~100× faster than random I/O on cold SSD.
                        //
                        // Heuristic: switch when unique_mids > BATCH_SCAN_THRESHOLD.
                        // Tuning: lower = more scans (uses more I/O for small fan-outs);
                        //         higher = more random probes (worse for large fan-outs).
                        const BATCH_SCAN_THRESHOLD: usize = 32;

                        if unique_mids > BATCH_SCAN_THRESHOLD {
                            // Full-scan mode: enumerate all (s, o) for this step with
                            // s=None (unbound), then filter by the known intermediate set.
                            tracing::debug!(
                                step = idx + 1,
                                unique_mids,
                                mode = "batch_scan",
                                "eval_path Sequence: switching to batch scan"
                            );
                            let all_pairs = self.eval_path(step, None, step_o);
                            for (s, o) in all_pairs {
                                if let Some(srcs) = by_mid.get(&s) {
                                    for &src in srcs {
                                        next.push((src, o));
                                    }
                                }
                            }
                        } else {
                            // Individual-probe mode: targeted index probe per mid.
                            for (mid, srcs) in by_mid {
                                for (_, dst) in self.eval_path(step, Some(mid), step_o) {
                                    for &src in &srcs {
                                        next.push((src, dst));
                                    }
                                }
                            }
                        }
                    }

                    tracing::debug!(
                        step = idx + 1,
                        pairs_out = next.len(),
                        elapsed_us = t_step.elapsed().as_micros(),
                        "eval_path Sequence step"
                    );
                    current = next;
                }
                tracing::debug!(
                    total_us = t_seq.elapsed().as_micros(),
                    final_pairs = current.len(),
                    "eval_path Sequence done"
                );
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

    /// Produce a sort key for ORDER BY: evaluates the expression and returns the
    /// decoded string form (e.g. `"42"` for `"\"42\"^^xsd:integer"`).
    /// Returns an empty string when the expression cannot be evaluated.
    fn eval_order_key(&self, expr: &Expression, binding: &Binding) -> String {
        // Fast path: variable already in binding
        if let Expression::Variable(v) = expr {
            if let Some(&id) = binding.get(v.as_str()) {
                return extract_literal_value(&self.dict.decode(id));
            }
            return String::new();
        }
        // General expression (e.g. arithmetic, function call)
        if let Some(id) = self.eval_term(expr, binding) {
            return extract_literal_value(&self.dict.decode(id));
        }
        String::new()
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
    // Normalize first: merge fragmented BGPs so the greedy join-order optimizer
    // can see all triple patterns in a group as a single unit.
    let normalized = normalize_graph_pattern(pattern.clone());
    optimize_bgp_with_bound(&normalized, index, dict, stats, &HashSet::new())
}

// ── BGP normalization ─────────────────────────────────────────────────────────

/// Extract (triples, filter_exprs) from a sub-tree built *only* from
/// `Bgp`, `Join`, and `Filter` nodes.  Returns `None` if any other node
/// type (Optional, Union, Subquery, Values, Extend, …) is encountered,
/// which prevents unsafe cross-boundary merging.
fn extract_bgp_with_filters(pat: &GraphPattern)
    -> Option<(Vec<TriplePatternAst>, Vec<Expression>)>
{
    match pat {
        GraphPattern::Bgp(triples) => Some((triples.clone(), vec![])),
        GraphPattern::Filter(inner, expr) => {
            let (triples, mut filters) = extract_bgp_with_filters(inner)?;
            filters.push(expr.clone());
            Some((triples, filters))
        }
        GraphPattern::Join(l, r) => {
            let (mut lt, mut lf) = extract_bgp_with_filters(l)?;
            let (rt, rf)         = extract_bgp_with_filters(r)?;
            lt.extend(rt);
            lf.extend(rf);
            Some((lt, lf))
        }
        _ => None,
    }
}

/// Try to fuse two already-normalised patterns when both decompose into a
/// pure (BGP + filters) tree.  Falls back to `Join(l, r)` otherwise.
fn normalize_join(l: GraphPattern, r: GraphPattern) -> GraphPattern {
    match (extract_bgp_with_filters(&l), extract_bgp_with_filters(&r)) {
        (Some((mut lt, mut lf)), Some((rt, rf))) => {
            // Both sides are pure BGP-or-Filter-over-BGP trees.  Merge triples
            // and re-wrap with all extracted filter expressions (inner-first).
            lt.extend(rt);
            lf.extend(rf);
            lf.into_iter().fold(
                GraphPattern::Bgp(lt),
                |acc, expr| GraphPattern::Filter(Box::new(acc), expr),
            )
        }
        _ => GraphPattern::Join(Box::new(l), Box::new(r)),
    }
}

/// Normalise a `GraphPattern` for better BGP join optimisation.
///
/// **BGP fusion** — merges adjacent BGPs across `Join` nodes so that
/// `optimize_triple_patterns` can consider all triple patterns in a group
/// as a single unit and choose a globally optimal join order:
///
/// ```text
/// Join(Bgp(A), Bgp(B))  →  Bgp(A ++ B)
/// ```
///
/// **Filter lifting** — SPARQL 1.1 §18.2.2.6 specifies that `FILTER`
/// expressions in a GroupGraphPattern apply to the *whole* group, but the
/// parser wraps earlier patterns eagerly, producing
/// `Join(Filter(Bgp(A), e), Bgp(B))` instead of `Filter(Bgp(A ++ B), e)`.
/// Normalization restores the correct flat form:
///
/// ```text
/// Join(Filter(pure_bgp_tree, e), Bgp(B))  →  Filter(Bgp(A ++ B), e)
/// Join(Bgp(A), Filter(pure_bgp_tree, e))  →  Filter(Bgp(A ++ B), e)
/// Join(Filter(T1, e1), Filter(T2, e2))    →  Filter(Filter(Bgp(T1 ++ T2), e1), e2)
/// ```
///
/// The fusion is applied bottom-up and stops at any non-BGP boundary
/// (`Optional`, `Union`, `Subquery`, `Values`, `Extend`, …) to preserve the
/// semantics of those constructs.
pub fn normalize_graph_pattern(pat: GraphPattern) -> GraphPattern {
    match pat {
        GraphPattern::Join(l, r) => {
            let l = normalize_graph_pattern(*l);
            let r = normalize_graph_pattern(*r);
            normalize_join(l, r)
        }
        GraphPattern::Optional(main, opt) => GraphPattern::Optional(
            Box::new(normalize_graph_pattern(*main)),
            Box::new(normalize_graph_pattern(*opt)),
        ),
        GraphPattern::Union(a, b) => GraphPattern::Union(
            Box::new(normalize_graph_pattern(*a)),
            Box::new(normalize_graph_pattern(*b)),
        ),
        GraphPattern::Filter(inner, expr) => {
            GraphPattern::Filter(Box::new(normalize_graph_pattern(*inner)), expr)
        }
        GraphPattern::Extend(inner, expr, var) => {
            GraphPattern::Extend(Box::new(normalize_graph_pattern(*inner)), expr, var)
        }
        GraphPattern::Graph(g, inner) => {
            GraphPattern::Graph(g, Box::new(normalize_graph_pattern(*inner)))
        }
        // Leaf nodes (Bgp, Values, PathPattern, Subquery, Empty): no change.
        other => other,
    }
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

            // ── BGP+PathPattern fusion ────────────────────────────────────────
            // When the flat join list contains only Bgp and PathPattern nodes,
            // merge all BGP triple patterns into one large Bgp so that
            // optimize_triple_patterns can see the full set and pick a globally
            // optimal join order.
            //
            // Safety condition: a PathPattern's output variable must not appear
            // in any BGP triple (otherwise the BGP cannot execute before the
            // PathPattern that produces it).
            //
            // This fixes the performance gap between:
            //   U-1 ( []  syntax → one flat BGP from recursive parse_property_list)
            //   U-2/U-3 ( _:label / ?var → fragmented BGPs split by PathPatterns)
            let all_bgp_or_path = flat.iter().all(|p| {
                matches!(p, GraphPattern::Bgp(_) | GraphPattern::PathPattern { .. })
            });
            let has_bgp  = flat.iter().any(|p| matches!(p, GraphPattern::Bgp(_)));
            let has_path = flat.iter().any(|p| matches!(p, GraphPattern::PathPattern { .. }));

            if all_bgp_or_path && has_bgp && has_path {
                let mut all_triples: Vec<TriplePatternAst> = Vec::new();
                let mut path_pats: Vec<&GraphPattern> = Vec::new();
                for p in &flat {
                    match p {
                        GraphPattern::Bgp(triples) => all_triples.extend_from_slice(triples),
                        pp => path_pats.push(pp),
                    }
                }

                // Build set of all variable names that appear in BGP triples.
                let bgp_vars: HashSet<&str> = all_triples.iter().flat_map(|t| {
                    let mut v: Vec<&str> = Vec::new();
                    if let Term::Variable(s) = &t.s { v.push(s.as_str()); }
                    if let Term::Variable(s) = &t.p { v.push(s.as_str()); }
                    if let Term::Variable(s) = &t.o { v.push(s.as_str()); }
                    v
                }).collect();

                // Safety check: PathPattern outputs must not be consumed by BGP triples.
                let safe = path_pats.iter().all(|pp| {
                    if let GraphPattern::PathPattern { o, .. } = pp {
                        if let Term::Variable(v) = o {
                            return !bgp_vars.contains(v.as_str());
                        }
                    }
                    true
                });

                if safe {
                    tracing::debug!(
                        bgp_triples = all_triples.len(),
                        path_patterns = path_pats.len(),
                        "optimize_bgp_with_bound: fusing BGPs across PathPatterns"
                    );
                    let merged_bgp = GraphPattern::Bgp(all_triples);
                    let mut current_bound = bound.clone();
                    let mut plans: Vec<ExecutionPlan> = Vec::new();

                    plans.push(optimize_bgp_with_bound(
                        &merged_bgp, index, dict, stats, &current_bound,
                    ));
                    current_bound.extend(pattern_bound_vars(&merged_bgp));

                    for pp in path_pats {
                        plans.push(optimize_bgp_with_bound(
                            pp, index, dict, stats, &current_bound,
                        ));
                        current_bound.extend(pattern_bound_vars(pp));
                    }

                    return plans
                        .into_iter()
                        .reduce(|acc, p| ExecutionPlan::Join(Box::new(acc), Box::new(p)))
                        .unwrap_or(ExecutionPlan::Empty);
                }
            }
            // ── end BGP+PathPattern fusion ────────────────────────────────────

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
            // Variables and blank nodes are unbound — they are wildcards in the scan.
            // (Blank nodes in WHERE patterns are converted to Term::Variable by the
            // parser; Term::BlankNode here would only arrive from unusual code paths.)
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
    // ── Phase 1: Greedy join ordering ─────────────────────────────────────────
    //
    // At each step pick the remaining pattern with the lowest estimated result
    // count, subject to the **connected-component constraint**:
    //
    //   If any remaining pattern shares at least one variable with the already-
    //   bound set, only those connected patterns are considered.  This prevents
    //   Cartesian products — a disconnected pattern is never chosen while a
    //   connected one exists, regardless of their absolute cardinalities.
    //
    // Within the connected candidates the most selective pattern wins.
    // When no pattern shares a variable with `bound` (first step, or genuinely
    // disjoint groups), all candidates are considered globally.
    //
    // Estimation priority (see `estimate_pattern_cardinality`):
    //   1. Index binary-search range count for constant positions (always used)
    //   2. Predicate-fanout statistics for ?-position cardinality (needs stats.bin)
    //   3. Bound-variable discount ×1/100 for each already-bound variable position

    let mut remaining: Vec<&TriplePatternAst> = triples.iter().collect();
    let mut bound = initially_bound.clone();
    let mut ordered: Vec<&TriplePatternAst> = Vec::with_capacity(triples.len());

    while !remaining.is_empty() {
        // Connected-component constraint: prefer patterns sharing a variable with
        // already-bound variables to avoid premature cross products.
        let any_connected = !bound.is_empty()
            && remaining.iter().any(|t| shares_var_with_bound(t, &bound));

        let best_idx = remaining.iter().enumerate()
            .filter(|(_, t)| !any_connected || shares_var_with_bound(t, &bound))
            .min_by_key(|(_, t)| estimate_pattern_cardinality(t, &bound, index, dict, stats))
            .map(|(i, _)| i)
            .unwrap(); // safe: remaining is non-empty

        let best = remaining.remove(best_idx);
        // Extend bound with all variables introduced by the chosen pattern.
        for term in [&best.s, &best.p, &best.o] {
            match term {
                Term::Variable(v) | Term::BlankNode(v) => { bound.insert(v.clone()); }
                _ => {}
            }
        }
        ordered.push(best);
    }

    // ── Phase 2: Build a left-deep plan tree with pre-encoded scan nodes ──────
    //
    // Re-derive `current_bound` from `initially_bound`, growing it pattern by
    // pattern.  `pattern_to_scan_plan` uses it to partition each pattern's
    // variables into `free_vars` (output columns) and `outer_vars` (positions
    // that will be substituted from the bind-join context at runtime).
    //
    // Result: `ScanBound` nodes whose `base` already holds pre-encoded TermIds
    // for constants — zero dictionary lookups in the execution inner loop.

    let mut current_bound = initially_bound.clone();
    let mut plan_opt: Option<ExecutionPlan> = None;

    for pat in &ordered {
        let scan = pattern_to_scan_plan(pat, dict, &current_bound);
        // Extend current_bound for the next pattern.
        for term in [&pat.s, &pat.p, &pat.o] {
            match term {
                Term::Variable(v) | Term::BlankNode(v) => { current_bound.insert(v.clone()); }
                _ => {}
            }
        }
        plan_opt = Some(match plan_opt {
            None => scan,
            Some(acc) => ExecutionPlan::Join(Box::new(acc), Box::new(scan)),
        });
    }

    plan_opt.unwrap_or(ExecutionPlan::Empty)
}

/// Returns true if at least one variable or blank-node in `t` appears in `bound`.
/// Used by the connected-component constraint in `optimize_triple_patterns`.
fn shares_var_with_bound(t: &TriplePatternAst, bound: &HashSet<String>) -> bool {
    [&t.s, &t.p, &t.o].iter().any(|term| match term {
        Term::Variable(v) | Term::BlankNode(v) => bound.contains(v.as_str()),
        _ => false,
    })
}

/// Compile one triple-pattern AST node into a pre-encoded scan plan.
///
/// - Constant IRIs and literals are looked up in the dictionary **once** at
///   plan-compile time and stored as TermIds in `base`.
/// - Variables listed in `bound` (already bound by earlier patterns or VALUES)
///   are placed in `outer_vars`; they will be substituted from the outer
///   `Binding` at execution time — no dictionary lookup needed.
/// - Variables not in `bound` go into `free_vars` (output columns, UNBOUND
///   wildcard positions in `base`).
///
/// Returns:
///   `Scan`      — all constants encoded, no outer substitution needed.
///   `ScanBound` — some positions need runtime substitution from outer context.
///   `Values{rows:[]}` — a constant IRI/literal is absent from the dictionary;
///                        the pattern can never match, so zero rows are produced.
fn pattern_to_scan_plan(
    t: &TriplePatternAst,
    dict: &QueryDict,
    bound: &HashSet<String>,
) -> ExecutionPlan {
    let mut base = TriplePattern { s: UNBOUND, p: UNBOUND, o: UNBOUND };
    let mut free_vars: Vec<(String, u8)> = Vec::new();
    let mut outer_vars: Vec<(String, u8)> = Vec::new();
    let mut missing_constant = false;

    let mut process = |term: &Term, pos: u8| {
        match term {
            Term::Variable(v) => {
                if bound.contains(v.as_str()) {
                    outer_vars.push((v.clone(), pos));
                } else {
                    free_vars.push((v.clone(), pos));
                }
            }
            Term::BlankNode(b) => {
                // Blank nodes in WHERE patterns behave like anonymous variables.
                if bound.contains(b.as_str()) {
                    outer_vars.push((b.clone(), pos));
                } else {
                    free_vars.push((b.clone(), pos));
                }
            }
            Term::Iri(iri) => match dict.lookup(iri.as_str()) {
                Some(id) => match pos { 0 => base.s = id, 1 => base.p = id, _ => base.o = id },
                None => missing_constant = true,
            },
            Term::Literal(lit) => match dict.lookup(&lit.to_ntriples()) {
                Some(id) => match pos { 0 => base.s = id, 1 => base.p = id, _ => base.o = id },
                None => missing_constant = true,
            },
        }
    };

    process(&t.s, 0);
    process(&t.p, 1);
    process(&t.o, 2);
    // Drop the closure here to release its mutable borrows on base/free_vars/etc.
    // Without this explicit drop, the Rust borrow checker would refuse to let us
    // consume free_vars or move outer_vars in the code below.
    drop(process);

    if missing_constant {
        // A constant term is absent from the dictionary — no triple can match.
        // Return an empty Values (zero rows) with the correct variable schema.
        return ExecutionPlan::Values(ValuesClause {
            variables: free_vars.into_iter().map(|(v, _)| v).collect(),
            rows: vec![],
        });
    }

    if outer_vars.is_empty() {
        // No runtime outer substitution needed → plain pre-encoded Scan.
        ExecutionPlan::Scan { pattern: base, variables: free_vars }
    } else {
        ExecutionPlan::ScanBound { base, free_vars, outer_vars }
    }
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
                match t {
                    Term::Variable(v) => left_vars.contains(v),
                    Term::BlankNode(b) => left_vars.contains(b),
                    _ => false,
                }
            };
            has_var(&p.s) || has_var(&p.p) || has_var(&p.o)
        }
        // ScanBound: outer_vars explicitly lists the variables from the left side
        // that this scan expects.  Check if any of them appear in left_vars.
        ExecutionPlan::ScanBound { outer_vars, .. } => {
            outer_vars.iter().any(|(v, _)| left_vars.contains(v))
        }
        // PathPattern uses substitute_term(s/o, outer) to inject outer bindings,
        // so it needs bind_join whenever s or o is a left-side variable.
        ExecutionPlan::PathPattern { s, o, .. } => {
            let uses_left_var = |t: &Term| -> bool {
                match t {
                    Term::Variable(v) => left_vars.contains(v),
                    Term::BlankNode(b) => left_vars.contains(b),
                    _ => false,
                }
            };
            uses_left_var(s) || uses_left_var(o)
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

/// Collect all variable names referenced by `plan` — i.e. variables that would
/// be substituted as constants when the plan is executed inside a bind-join
/// context (via `execute_plan_with_ctx`).  Used by `bind_join` to group left
/// rows by the subset of bindings that actually affect the right plan, so each
/// unique binding is executed only once rather than once per left row.
/// Recursively collect TriplePatterns from all `ScanBound` nodes reachable
/// from `plan` through `Join` edges, substituting variables from `binding`.
///
/// Used by `bind_join` to fire a batch of madvise(MADV_WILLNEED) prefetch hints
/// *before* starting execution, so the OS can pipeline disk reads in the
/// background while the CPU sets up the first group's evaluation.
///
/// Only traverses `Join` children — other plan types (PathPattern, Subquery, …)
/// manage their own I/O and are skipped here.
///
/// Predicates that are already in `pred_cache` do not touch the HDD, so their
/// patterns are omitted (no point issuing a prefetch for a RAM-resident key).
fn collect_scanbound_patterns(
    plan: &ExecutionPlan,
    binding: &Binding,
    pred_cache: &PredCache,
    out: &mut Vec<TriplePattern>,
) {
    match plan {
        ExecutionPlan::ScanBound { base, outer_vars, .. } => {
            // Fill fixed components from the current binding.
            let mut pat = *base;
            for (ov_name, ov_pos) in outer_vars.iter() {
                if let Some(&id) = binding.get(ov_name.as_str()) {
                    match *ov_pos {
                        0 => pat.s = id,
                        1 => pat.p = id,
                        _ => pat.o = id,
                    }
                }
            }
            // Skip if the predicate is cached in RAM — no HDD I/O needed.
            if pat.p != UNBOUND && pred_cache.get(pat.p).is_some() {
                return;
            }
            out.push(pat);
        }
        // Recurse into both sides of a Join.
        ExecutionPlan::Join(left, right) => {
            collect_scanbound_patterns(left,  binding, pred_cache, out);
            collect_scanbound_patterns(right, binding, pred_cache, out);
        }
        // All other plan types have their own I/O management; skip.
        _ => {}
    }
}

fn plan_referenced_vars(plan: &ExecutionPlan) -> HashSet<String> {
    let mut vars = HashSet::new();
    collect_referenced_vars(plan, &mut vars);
    vars
}

fn collect_referenced_vars(plan: &ExecutionPlan, out: &mut HashSet<String>) {
    match plan {
        ExecutionPlan::ScanAst(p) => {
            for t in [&p.s, &p.p, &p.o] {
                match t {
                    Term::Variable(v) => { out.insert(v.clone()); }
                    Term::BlankNode(b) => { out.insert(b.clone()); }
                    _ => {}
                }
            }
        }
        // ScanBound: the variables referenced from the outer context are outer_vars.
        // free_vars are output variables produced by this scan, not inputs.
        ExecutionPlan::ScanBound { outer_vars, .. } => {
            for (v, _) in outer_vars { out.insert(v.clone()); }
        }
        ExecutionPlan::Join(l, r)
        | ExecutionPlan::Optional(l, r)
        | ExecutionPlan::Union(l, r) => {
            collect_referenced_vars(l, out);
            collect_referenced_vars(r, out);
        }
        ExecutionPlan::Filter(inner, _) | ExecutionPlan::Extend(inner, _, _) => {
            collect_referenced_vars(inner, out);
        }
        ExecutionPlan::PathPattern { s, o, .. } => {
            if let Term::Variable(v) = s { out.insert(v.clone()); }
            if let Term::Variable(v) = o { out.insert(v.clone()); }
        }
        ExecutionPlan::NamedGraph { inner, .. } => collect_referenced_vars(inner, out),
        // Subquery and Values are self-contained — they do not reference outer variables.
        _ => {}
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

/// Convert days since Unix epoch (1970-01-01) to (year, month, day).
/// Used by NOW() to produce a human-readable xsd:dateTime without chrono dependency.
fn days_to_ymd(days: u64) -> (u32, u32, u32) {
    // Algorithm: civil_from_days (Howard Hinnant, public domain)
    let z = days as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y   = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp  = (5 * doy + 2) / 153;
    let d   = doy - (153 * mp + 2) / 5 + 1;
    let m   = if mp < 10 { mp + 3 } else { mp - 9 };
    let y   = if m <= 2 { y + 1 } else { y };
    (y as u32, m as u32, d as u32)
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

/// Compare two ORDER BY sort keys (already lexical-value strings, not N-Triples).
/// Numeric strings are compared numerically; everything else lexicographically.
/// This ensures `ORDER BY ?count` works correctly for integer/decimal aggregates.
fn compare_order_keys(a: &str, b: &str) -> std::cmp::Ordering {
    match (a.parse::<f64>(), b.parse::<f64>()) {
        (Ok(na), Ok(nb)) => na.partial_cmp(&nb).unwrap_or(std::cmp::Ordering::Equal),
        _ => a.cmp(b),
    }
}

/// Rewrite aggregate sub-expressions in a HAVING condition to the alias variable
/// they were projected under.  After GROUP BY the aggregate columns are already
/// bound under their alias names (e.g. `?n_evidence`), so `COUNT(?pe)` in HAVING
/// must be resolved to `?n_evidence` before eval_bool can compare the value.
fn rewrite_having_agg(expr: Expression, aliases: &[(Expression, String)]) -> Expression {
    if is_aggregate_expr(&expr) {
        for (agg_expr, alias_name) in aliases {
            if *agg_expr == expr {
                return Expression::Variable(alias_name.clone());
            }
        }
        return expr;
    }
    match expr {
        Expression::And(a, b) => Expression::And(
            Box::new(rewrite_having_agg(*a, aliases)),
            Box::new(rewrite_having_agg(*b, aliases)),
        ),
        Expression::Or(a, b) => Expression::Or(
            Box::new(rewrite_having_agg(*a, aliases)),
            Box::new(rewrite_having_agg(*b, aliases)),
        ),
        Expression::Not(a) => Expression::Not(
            Box::new(rewrite_having_agg(*a, aliases)),
        ),
        Expression::Eq(a, b) => Expression::Eq(
            Box::new(rewrite_having_agg(*a, aliases)),
            Box::new(rewrite_having_agg(*b, aliases)),
        ),
        Expression::Ne(a, b) => Expression::Ne(
            Box::new(rewrite_having_agg(*a, aliases)),
            Box::new(rewrite_having_agg(*b, aliases)),
        ),
        Expression::Lt(a, b) => Expression::Lt(
            Box::new(rewrite_having_agg(*a, aliases)),
            Box::new(rewrite_having_agg(*b, aliases)),
        ),
        Expression::Le(a, b) => Expression::Le(
            Box::new(rewrite_having_agg(*a, aliases)),
            Box::new(rewrite_having_agg(*b, aliases)),
        ),
        Expression::Gt(a, b) => Expression::Gt(
            Box::new(rewrite_having_agg(*a, aliases)),
            Box::new(rewrite_having_agg(*b, aliases)),
        ),
        Expression::Ge(a, b) => Expression::Ge(
            Box::new(rewrite_having_agg(*a, aliases)),
            Box::new(rewrite_having_agg(*b, aliases)),
        ),
        Expression::Add(a, b) => Expression::Add(
            Box::new(rewrite_having_agg(*a, aliases)),
            Box::new(rewrite_having_agg(*b, aliases)),
        ),
        Expression::Sub(a, b) => Expression::Sub(
            Box::new(rewrite_having_agg(*a, aliases)),
            Box::new(rewrite_having_agg(*b, aliases)),
        ),
        Expression::Mul(a, b) => Expression::Mul(
            Box::new(rewrite_having_agg(*a, aliases)),
            Box::new(rewrite_having_agg(*b, aliases)),
        ),
        Expression::Div(a, b) => Expression::Div(
            Box::new(rewrite_having_agg(*a, aliases)),
            Box::new(rewrite_having_agg(*b, aliases)),
        ),
<<<<<<< HEAD
        Expression::In(expr, list) => Expression::In(
            Box::new(rewrite_having_agg(*expr, aliases)),
            list.into_iter().map(|e| rewrite_having_agg(e, aliases)).collect(),
        ),
        Expression::NotIn(expr, list) => Expression::NotIn(
            Box::new(rewrite_having_agg(*expr, aliases)),
            list.into_iter().map(|e| rewrite_having_agg(e, aliases)).collect(),
        ),
=======
>>>>>>> 583087b0f936c7c64e6a487eb4dfd6a098398d26
        other => other,
    }
}

/// Returns true when `expr` is a SPARQL aggregate function.
fn is_aggregate_expr(expr: &Expression) -> bool {
    matches!(
        expr,
        Expression::Count { .. }
            | Expression::Sum { .. }
            | Expression::Min { .. }
            | Expression::Max { .. }
            | Expression::Avg { .. }
            | Expression::GroupConcat { .. }
            | Expression::Sample { .. }
    )
}

/// Returns true when the SELECT projection contains at least one aggregate expression
/// (COUNT, SUM, MIN, MAX, AVG, GROUP_CONCAT, SAMPLE).
/// Used to trigger implicit single-group aggregation when GROUP BY is absent.
fn projection_has_aggregates(query: &SelectQuery) -> bool {
    if let Projection::Variables(items) = &query.projection {
        items.iter().any(|item| {
            if let SelectItem::Alias(expr, _) = item {
                is_aggregate_expr(expr)
            } else {
                false
            }
        })
    } else {
        false
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
        ExecutionPlan::ScanBound{..}   => "ScanBound",
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
        ExecutionPlan::ScanBound { base, free_vars, outer_vars } => {
            let free = free_vars.iter().map(|(v, p)| format!("?{}@{}", v, p)).collect::<Vec<_>>().join(",");
            let outer = outer_vars.iter().map(|(v, p)| format!("?{}@{}", v, p)).collect::<Vec<_>>().join(",");
            format!("{}ScanBound(s={} p={} o={} free=[{}] outer=[{}])",
                pad,
                if base.s == UNBOUND { "?".to_string() } else { base.s.to_string() },
                if base.p == UNBOUND { "?".to_string() } else { base.p.to_string() },
                if base.o == UNBOUND { "?".to_string() } else { base.o.to_string() },
                free, outer)
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

// ══════════════════════════════════════════════════════════════════════════════
// PropertyPath cost helpers
// ══════════════════════════════════════════════════════════════════════════════

/// Extract the first bare IRI leaf from a PropertyPath.
/// Used to estimate POS scan cost for the cost-based PathPattern join decision.
/// Return `true` if this exact property path (as a flat Sequence of IRI steps)
/// has been pre-materialised in the `path_cache`.
///
/// When `true`, the cost model uses `CACHE_SEEK_NS` (2 µs) instead of
/// `SPO_SEEK_NS` (150 ms) for the bind-join estimate, making bind-join the
/// winner even for moderate left-side cardinalities (e.g. 508 rows × 2 steps
/// × 2 µs = 2 ms vs hash-join cloning 11.8 M pairs = several seconds).
fn path_in_path_cache(
    path: &PropertyPath,
    dict: &QueryDict,
    path_cache: &PathCache,
) -> bool {
    if path_cache.is_empty() {
        return false;
    }
    // Collect predicate IDs for a flat Sequence of IRI steps only.
    // Any other shape (Inverse, Alternative, repetition) is not in path_cache.
    let ids: Option<Vec<TermId>> = match path {
        PropertyPath::Sequence(steps) => steps.iter().map(|s| {
            if let PropertyPath::Iri(iri) = s {
                dict.lookup(iri)
            } else {
                None
            }
        }).collect(),
        PropertyPath::Iri(iri) => dict.lookup(iri).map(|id| vec![id]),
        _ => None,
    };
    match ids {
        Some(ref v) if v.len() >= 2 => path_cache.get(v).is_some(),
        _ => false,
    }
}

/// Returns `None` for paths that start with a repetition (unknown range).
/// Returns true when every non-wildcard step in `path` is a simple IRI predicate
/// whose (subject, object) pairs are already loaded in `pred_cache`.
///
/// Used by the PathPattern cost model: when all steps are cached, a bind-join
/// uses O(N_groups × log M) RAM binary searches instead of O(N_groups) cold HDD
/// seeks, making bind-join essentially free vs hash-join's 11.8M-row materialisation.
fn path_all_iris_cached(
    path: &PropertyPath,
    dict: &QueryDict,
    pred_cache: &PredCache,
) -> bool {
    match path {
        PropertyPath::Iri(iri) => dict
            .lookup(iri)
            .map(|id| pred_cache.get(id).is_some())
            .unwrap_or(false),
        PropertyPath::Sequence(steps) => steps.iter().all(|s| path_all_iris_cached(s, dict, pred_cache)),
        // Inverse: the pred_cache is sorted by (S, O). For a bind_join step,
        // the bind value becomes the OBJECT, so we need to search by O — which
        // the cache can't do efficiently.  Return false so bind_join uses the
        // real SPO_SEEK_NS cost estimate instead of the optimistic CACHE_SEEK_NS.
        PropertyPath::Inverse(_) => false,
        // Alternative: conservative — both branches must be cacheable.
        PropertyPath::Alternative(alts) => alts.iter().all(|a| path_all_iris_cached(a, dict, pred_cache)),
        // Repetitions: bind-join cannot use binary search (depth unknown); decline.
        PropertyPath::ZeroOrMore(_) | PropertyPath::OneOrMore(_) | PropertyPath::ZeroOrOne(_) => false,
    }
}

fn path_first_iri(path: &PropertyPath) -> Option<&str> {
    match path {
        PropertyPath::Iri(iri) => Some(iri.as_str()),
        PropertyPath::Sequence(steps) => steps.first().and_then(path_first_iri),
        PropertyPath::Alternative(alts) => alts.first().and_then(path_first_iri),
        PropertyPath::Inverse(inner) => path_first_iri(inner),
        // Repetitions: depth unknown, can't estimate range reliably.
        PropertyPath::ZeroOrMore(_)
        | PropertyPath::OneOrMore(_)
        | PropertyPath::ZeroOrOne(_) => None,
    }
}

/// Count the number of sequential hops in a PropertyPath (lower bound estimate).
/// Sequence of N steps = N hops; everything else = 1.
fn path_step_count(path: &PropertyPath) -> usize {
    match path {
        PropertyPath::Sequence(steps) => steps.iter().map(path_step_count).sum(),
        PropertyPath::Alternative(alts) => {
            alts.iter().map(path_step_count).max().unwrap_or(1)
        }
        PropertyPath::Inverse(inner) => path_step_count(inner),
        // Repetitions: treat as 1 for cost estimation (we don't know actual depth).
        PropertyPath::ZeroOrMore(_)
        | PropertyPath::OneOrMore(_)
        | PropertyPath::ZeroOrOne(_) => 1,
        PropertyPath::Iri(_) => 1,
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Unit tests — normalize_graph_pattern
// ══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod normalize_tests {
    use super::*;

    fn var(name: &str) -> Term { Term::Variable(name.to_string()) }
    fn iri(s: &str) -> Term  { Term::Iri(s.to_string()) }

    fn tp(s: Term, p: Term, o: Term) -> TriplePatternAst {
        TriplePatternAst { s, p, o }
    }

    fn bgp(triples: Vec<TriplePatternAst>) -> GraphPattern {
        GraphPattern::Bgp(triples)
    }

    fn filter_expr() -> Expression {
        // A minimal filter expression: ?x != <something>
        Expression::Ne(
            Box::new(Expression::Variable("x".to_string())),
            Box::new(Expression::Iri("http://example.org/x".to_string())),
        )
    }

    /// Rule 1: Join(Bgp(A), Bgp(B)) → Bgp(A ++ B)
    #[test]
    fn test_fuse_two_bgps() {
        let a = tp(var("s"), iri("http://ex/p1"), var("o1"));
        let b = tp(var("s"), iri("http://ex/p2"), var("o2"));
        let pat = GraphPattern::Join(
            Box::new(bgp(vec![a.clone()])),
            Box::new(bgp(vec![b.clone()])),
        );
        let result = normalize_graph_pattern(pat);
        match result {
            GraphPattern::Bgp(triples) => {
                assert_eq!(triples.len(), 2);
            }
            other => panic!("Expected Bgp, got {:?}", other),
        }
    }

    /// Rule 1 (chained): Join(Join(Bgp(A), Bgp(B)), Bgp(C)) → Bgp(A ++ B ++ C)
    #[test]
    fn test_fuse_three_bgps() {
        let a = tp(var("s"), iri("http://ex/p1"), var("o1"));
        let b = tp(var("s"), iri("http://ex/p2"), var("o2"));
        let c = tp(var("s"), iri("http://ex/p3"), var("o3"));
        let pat = GraphPattern::Join(
            Box::new(GraphPattern::Join(
                Box::new(bgp(vec![a.clone()])),
                Box::new(bgp(vec![b.clone()])),
            )),
            Box::new(bgp(vec![c.clone()])),
        );
        let result = normalize_graph_pattern(pat);
        match result {
            GraphPattern::Bgp(triples) => {
                assert_eq!(triples.len(), 3, "All 3 triple patterns should be fused");
            }
            other => panic!("Expected Bgp, got {:?}", other),
        }
    }

    /// Rule 2: Join(Filter(Bgp(A), e), Bgp(B)) → Filter(Bgp(A ++ B), e)
    /// Models a FILTER between two groups of triple patterns.
    #[test]
    fn test_filter_lifting_left() {
        let a = tp(var("s"), iri("http://ex/type"), iri("http://ex/Protein"));
        let b = tp(var("s"), iri("http://ex/hasGO"), var("go"));
        let pat = GraphPattern::Join(
            Box::new(GraphPattern::Filter(
                Box::new(bgp(vec![a.clone()])),
                filter_expr(),
            )),
            Box::new(bgp(vec![b.clone()])),
        );
        let result = normalize_graph_pattern(pat);
        // Expected: Filter(Bgp([a, b]), e)
        match result {
            GraphPattern::Filter(inner, _) => match *inner {
                GraphPattern::Bgp(triples) => {
                    assert_eq!(triples.len(), 2,
                        "Both triple patterns should be inside the merged BGP");
                }
                other => panic!("Expected Bgp inside Filter, got {:?}", other),
            },
            other => panic!("Expected Filter, got {:?}", other),
        }
    }

    /// Rule 2 + chaining: Join(Filter(Join(Bgp(A), Bgp(B)), e), Bgp(C))
    /// → Filter(Bgp(A ++ B ++ C), e)
    #[test]
    fn test_filter_lifting_nested() {
        let a = tp(var("s"), iri("http://ex/p1"), var("o1"));
        let b = tp(var("s"), iri("http://ex/p2"), var("o2"));
        let c = tp(var("s"), iri("http://ex/p3"), var("o3"));
        let pat = GraphPattern::Join(
            Box::new(GraphPattern::Filter(
                Box::new(GraphPattern::Join(
                    Box::new(bgp(vec![a.clone()])),
                    Box::new(bgp(vec![b.clone()])),
                )),
                filter_expr(),
            )),
            Box::new(bgp(vec![c.clone()])),
        );
        let result = normalize_graph_pattern(pat);
        match result {
            GraphPattern::Filter(inner, _) => match *inner {
                GraphPattern::Bgp(triples) => {
                    assert_eq!(triples.len(), 3,
                        "All 3 triple patterns should be inside the merged BGP");
                }
                other => panic!("Expected Bgp inside Filter, got {:?}", other),
            },
            other => panic!("Expected Filter, got {:?}", other),
        }
    }

    /// Optional boundaries are NOT crossed.
    /// Join(Optional(Bgp(A), Bgp(B)), Bgp(C)) must remain a Join.
    #[test]
    fn test_no_fusion_across_optional() {
        let a = tp(var("s"), iri("http://ex/p1"), var("o1"));
        let b = tp(var("s"), iri("http://ex/label"), var("l"));
        let c = tp(var("s"), iri("http://ex/p2"), var("o2"));
        let pat = GraphPattern::Join(
            Box::new(GraphPattern::Optional(
                Box::new(bgp(vec![a.clone()])),
                Box::new(bgp(vec![b.clone()])),
            )),
            Box::new(bgp(vec![c.clone()])),
        );
        let result = normalize_graph_pattern(pat);
        // Must remain a Join (not a Bgp) — Optional boundary is preserved.
        match result {
            GraphPattern::Join(_, _) => { /* correct */ }
            other => panic!("Expected Join to be preserved, got {:?}", other),
        }
    }

    /// Extend (BIND) boundaries are NOT crossed.
    #[test]
    fn test_no_fusion_across_extend() {
        let a = tp(var("s"), iri("http://ex/p1"), var("o1"));
        let b = tp(var("s"), iri("http://ex/p2"), var("o2"));
        let pat = GraphPattern::Join(
            Box::new(GraphPattern::Extend(
                Box::new(bgp(vec![a.clone()])),
                Expression::Variable("o1".to_string()),
                "y".to_string(),
            )),
            Box::new(bgp(vec![b.clone()])),
        );
        let result = normalize_graph_pattern(pat);
        match result {
            GraphPattern::Join(_, _) => { /* correct — Extend boundary preserved */ }
            other => panic!("Expected Join to be preserved, got {:?}", other),
        }
    }
}

// ── Cross-product plan analysis ────────────────────────────────────────────

/// Recursively collect independent `Scan` leaves from a plan tree.
///
/// Returns `true` and populates `out` when every join in the tree is a pure
/// Cartesian product — i.e. all non-leaf nodes are `Join(_, Scan)` with no
/// `ScanBound`, `Filter`, or other correlated nodes.  Returns `false` and
/// leaves `out` in an indeterminate state as soon as a correlated node is found.
fn collect_cross_product_leaves<'a>(plan: &'a ExecutionPlan, out: &mut Vec<&'a ExecutionPlan>) -> bool {
    match plan {
        ExecutionPlan::Scan { .. } => {
            out.push(plan);
            true
        }
        ExecutionPlan::Join(left, right) => {
            match right.as_ref() {
                ExecutionPlan::Scan { .. } => {
                    out.push(right.as_ref());
                    collect_cross_product_leaves(left, out)
                }
                ExecutionPlan::Join(..) => {
                    // Both subtrees must be cross-product plans.
                    collect_cross_product_leaves(left, out)
                        && collect_cross_product_leaves(right, out)
                }
                _ => false,  // ScanBound, Filter, ScanAst, PathPattern, etc.
            }
        }
        _ => false,
    }
}
