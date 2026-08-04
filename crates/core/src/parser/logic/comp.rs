//! `comp` (computed variable) parsing, dependency tracking, and incremental
//! recomputation.

use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::{BTreeSet, VecDeque};

use crate::core::errors::MizuError;
use crate::core::types::{StringInterner, Symbol, Value, VariableStore};

use super::ast::{ComputedBinding, Expr, ExprArena, MizuFunction};
use super::lexer::{Cursor, assert_cursor_empty, leading_spaces, lex};
use super::parse::parse_expr_tree;

/// Parses `comp name = expr` declarations at the baseline indent of `logic_content`.
///
/// Returns the bindings in **topological order** (dependencies before dependents)
/// so that [`recompute_computed_bindings`] can evaluate them in a single pass.
///
/// # Errors
///
/// * [`MizuError::ParseError`] if any `comp` line is malformed.
/// * [`MizuError::ParseError`] `"computed variable cycle detected"` if two or more
///   comp variables depend on each other in a cycle.
pub fn parse_computed(
    logic_content: &str,
    interner: &mut StringInterner,
) -> Result<Vec<ComputedBinding>, MizuError> {
    parse_computed_with_functions(logic_content, interner, &FxHashMap::default(), 500)
}

/// Like [`parse_computed`], but additionally derives **transitive** data
/// dependencies through the bodies of called logic functions.
///
/// Mizu functions may read global variables directly (`f(a) : a + z` reads the
/// global `z`).  A binding `comp y = f(x)` therefore depends on `z` even though
/// `z` never appears in the binding's own right-hand side.  Walking only the
/// RHS would leave `y` stale when `z` mutates; this variant unions the
/// variables read by every function reachable from the RHS (the call graph is
/// a DAG — see [`parse_logic`]'s `check_dag` — so the walk terminates).
///
/// The dependency set is a deliberate over-approximation: parameters and
/// `let`-locals of called functions may be included.  Extra entries are
/// harmless (they can only trigger a spurious recompute); missing entries
/// would cause stale computed values.
pub fn parse_computed_with_functions(
    logic_content: &str,
    interner: &mut StringInterner,
    functions: &FxHashMap<Symbol, MizuFunction>,
    max_comp_bindings: usize,
) -> Result<Vec<ComputedBinding>, MizuError> {
    let function_names: FxHashSet<Symbol> = functions.keys().copied().collect();
    let all_lines: Vec<&str> = logic_content.lines().collect();

    let baseline = all_lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| leading_spaces(l))
        .min()
        .unwrap_or(0);

    let mut bindings: Vec<ComputedBinding> = Vec::new();

    for raw_line in &all_lines {
        if raw_line.trim().is_empty() {
            continue;
        }
        let indent = leading_spaces(raw_line);
        if indent != baseline {
            continue;
        }

        let stripped = &raw_line[baseline.min(raw_line.len())..];
        let Some(rest) = stripped.trim_start().strip_prefix("comp ") else {
            continue;
        };

        let eq_pos = rest.find('=').ok_or_else(|| {
            MizuError::ParseError(format!(
                "comp declaration missing `=`: `{}`",
                stripped.trim()
            ))
        })?;
        let name = rest[..eq_pos].trim();
        let expr_src = rest[eq_pos + 1..].trim();

        if name.is_empty() || expr_src.is_empty() {
            return Err(MizuError::ParseError(format!(
                "invalid comp declaration: `{}`",
                stripped.trim()
            )));
        }

        let tokens = lex(expr_src)?;
        let mut cursor = Cursor::new(&tokens);
        let expr = parse_expr_tree(&mut cursor, interner)?;
        assert_cursor_empty(&cursor, expr_src)?;

        let name_sym = interner.get_or_intern(name);
        let mut dep_set: FxHashSet<Symbol> = FxHashSet::default();
        collect_vars(expr.root(), &expr.arena, &mut dep_set);
        // Union the globals read inside every function reachable from the RHS,
        // so mutations to those globals also trigger a recompute.
        collect_reachable_function_reads(
            expr.root(),
            &expr.arena,
            functions,
            &function_names,
            &mut dep_set,
        );
        // `collect_vars`/`collect_reachable_function_reads` only ever insert
        // symbols that are read as *values* (call arguments, operands,
        // `Let`/param-free references) — neither one ever inserts a callee's
        // own name, since both skip the `name` field of `Expr::FunctionCall`.
        // There is therefore nothing here to filter as "a function name, not
        // a data dependency": a blanket `dep_set.remove` over every known
        // function name used to sit here, but a bare `name = expr` top-level
        // declaration is itself parsed as a zero-parameter function (see
        // `parse_logic`), so *every* plain global doubles as a "function
        // name" — the blanket removal therefore stripped the dependency on
        // any global passed as a call argument (`comp y = f(counter)` lost
        // `counter` whenever nothing else referenced it), which is the
        // common case for a `comp` calling a helper function.
        dep_set.remove(&name_sym);

        bindings.push(ComputedBinding {
            name: name_sym,
            expr,
            depends_on: dep_set.into_iter().collect(),
            // Set by `check_information_flow` once the taint fixpoint is known.
            tainted: false,
        });
    }

    // Reject documents that declare more `comp` bindings than
    // MAX_COMP_BINDINGS.
    //
    // This limit used to be load-bearing against CPU exhaustion, because
    // `recompute_computed_bindings` granted each firing comp its own fresh
    // MAX_INSTRUCTIONS budget: the comp count multiplied the per-event work.
    // It no longer does — the cascade shares two budgets, one per taint class,
    // neither scaled by the comp count — so the cap is now
    // about *memory and clarity* rather than time: each binding holds a
    // parsed `ExprTree` and an entry in the reverse index, and a document
    // needing hundreds of derived variables is far more likely to be
    // generated abuse than hand-written. Rejecting at parse time keeps that a
    // clear load-time error instead of an undiagnosable runtime shrug.
    if bindings.len() > max_comp_bindings {
        return Err(MizuError::ParseError(format!(
            "document declares {} `comp` bindings, exceeding the maximum of {} \
             (MAX_COMP_BINDINGS); split the logic across fewer computed variables \
             or reduce reliance on derived state",
            bindings.len(),
            max_comp_bindings
        )));
    }

    topo_sort_computed(bindings)
}

/// Whether the instruction pool a binding of this taint class draws on is
/// already spent.
///
/// Tainted and untainted comps have separate pools so that computation driven
/// by untrusted input cannot exhaust the allowance an untainted binding needs
/// — see `ComputedBinding::tainted`.
fn pool_exhausted(tainted: bool, spent_tainted: u64, spent_untainted: u64, budget: u64) -> bool {
    let spent = if tainted {
        spent_tainted
    } else {
        spent_untainted
    };
    spent > budget
}

/// Applies Kahn's algorithm to sort `bindings` topologically and detect cycles.
///
/// Only edges **between comp variables** are considered; dependencies on normal
/// variables or logic functions do not affect ordering.
///
/// # Errors
///
/// Returns `"computed variable cycle detected"` if the dependency graph among
/// `ComputedBinding` nodes contains a cycle.
fn topo_sort_computed(bindings: Vec<ComputedBinding>) -> Result<Vec<ComputedBinding>, MizuError> {
    if bindings.is_empty() {
        return Ok(bindings);
    }

    let comp_index: FxHashMap<Symbol, usize> = bindings
        .iter()
        .enumerate()
        .map(|(i, cb)| (cb.name, i))
        .collect();

    let n = bindings.len();
    let mut in_degree = vec![0usize; n];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n];

    for (i, cb) in bindings.iter().enumerate() {
        for &dep_sym in &cb.depends_on {
            if let Some(&j) = comp_index.get(&dep_sym) {
                dependents[j].push(i);
                in_degree[i] += 1;
            }
        }
    }

    let mut queue: VecDeque<usize> = (0..n).filter(|&i| in_degree[i] == 0).collect();
    let mut order: Vec<usize> = Vec::with_capacity(n);

    while let Some(i) = queue.pop_front() {
        order.push(i);
        for &j in &dependents[i] {
            in_degree[j] -= 1;
            if in_degree[j] == 0 {
                queue.push_back(j);
            }
        }
    }

    if order.len() != n {
        return Err(MizuError::ParseError(
            "computed variable cycle detected".to_string(),
        ));
    }

    let mut items: Vec<Option<ComputedBinding>> = bindings.into_iter().map(Some).collect();
    let mut sorted = Vec::with_capacity(n);
    for i in order {
        if let Some(cb) = items[i].take() {
            sorted.push(cb);
        }
    }
    Ok(sorted)
}

/// Reverse index mapping a symbol to the indices (into a `&[ComputedBinding]`
/// slice, in the same topological order produced by [`topo_sort_computed`])
/// of every binding whose `depends_on` contains that symbol.
///
/// Lets [`recompute_computed_bindings`] jump straight to the bindings a
/// mutation could possibly affect instead of scanning the whole document.
pub type CompReverseIndex = FxHashMap<Symbol, Vec<usize>>;

/// Builds the [`CompReverseIndex`] for `bindings`.
///
/// Call this once, whenever `bindings` is loaded or replaced (e.g. on
/// document reload) — the index is keyed by binding *position*, so it goes
/// stale if `bindings` is reordered or mutated without rebuilding it.
pub fn build_comp_reverse_index(bindings: &[ComputedBinding]) -> CompReverseIndex {
    let mut index: CompReverseIndex = FxHashMap::default();
    for (i, cb) in bindings.iter().enumerate() {
        for &dep_sym in &cb.depends_on {
            index.entry(dep_sym).or_default().push(i);
        }
    }
    index
}

/// Re-evaluates computed bindings whose dependencies include any symbol in `mutated`.
///
/// `bindings` must be in topological order (see [`parse_computed`]), and
/// `reverse_index` must be the [`CompReverseIndex`] built from that same slice
/// via [`build_comp_reverse_index`] (typically cached once at document load
/// time rather than rebuilt on every call).
/// Any newly evaluated comp binding that produces a changed value is recorded in
/// `store.evaluator.undo_log` via [`VariableStore::set_symbol`], so it will be
/// picked up by the logic worker's `send_response` along with the original mutations.
///
/// Returns a superset of `mutated` extended with the symbols of any comp bindings
/// that were re-evaluated, so a chained call can propagate the recomputation.
///
/// ## Algorithm
///
/// Rather than scanning every binding to test `depends_on ∩ changed`, this
/// walks only the bindings reachable from `mutated` through the reverse
/// index, expanding the candidate set to a fixed point as newly recomputed
/// comps unlock their own dependents:
///
/// 1. Seed a candidate set with every binding index reachable from `mutated`.
/// 2. Repeatedly pop the *smallest* remaining candidate index and evaluate it
///    (if its dependencies still intersect `changed` — always true by
///    construction, checked defensively). If it changes, fold the indices of
///    its own dependents (via the reverse index) back into the candidate set.
///
/// Because `bindings` is topologically sorted (a comp's dependencies always
/// have a strictly smaller index than the comp itself), any dependent
/// unlocked by evaluating index `i` has index `> i`. Processing candidates in
/// ascending order therefore visits exactly the same bindings, in the same
/// relative order, that the original full left-to-right scan would have
/// visited — it just skips the ones that scan would have `continue`d past
/// without evaluating. The observable result (which bindings get recomputed,
/// in what order, with what final values) is identical to the O(#bindings)
/// scan; see `test_recompute_matches_naive_scan_randomized` for a randomized
/// equivalence check.
pub fn recompute_computed_bindings(
    store: &mut VariableStore,
    bindings: &[ComputedBinding],
    functions: &FxHashMap<Symbol, MizuFunction>,
    mutated: &FxHashSet<Symbol>,
    reverse_index: &CompReverseIndex,
) -> FxHashSet<Symbol> {
    if bindings.is_empty() {
        return mutated.clone();
    }
    let mut changed = mutated.clone();

    let mut candidates: BTreeSet<usize> = BTreeSet::new();
    for sym in mutated {
        if let Some(idxs) = reverse_index.get(sym) {
            candidates.extend(idxs.iter().copied());
        }
    }
    // A binding with an empty `depends_on` (e.g. `comp greeting =
    // greet("Mizu")` — every argument a literal, no variable read) never
    // appears in `reverse_index` under any symbol, since that index is
    // built entirely from `depends_on` sets. It would therefore never be
    // reachable from `mutated` through the loop above, on the very first
    // call (the "treat every global as mutated" initial pass) or any
    // later one — nothing ever mutates *for* it, because it has nothing
    // to depend on. Its value is truly constant once computed, so this
    // only needs to add it once: `Value::Null` is otherwise unreachable
    // for a bound comp (every non-error evaluation result is stored via
    // `set_symbol` below), so it doubles as "not yet evaluated" here.
    for (i, cb) in bindings.iter().enumerate() {
        if cb.depends_on.is_empty() && matches!(store.evaluator.get_global(cb.name), Value::Null) {
            candidates.insert(i);
        }
    }

    // Two budgets for the cascade: one for tainted comps, one for untainted.
    //
    // Resetting per comp made the worst case a *product*: MAX_COMP_BINDINGS
    // (500) fully-spent budgets per event, since every firing comp got a fresh
    // allowance. Measured at ~27.5 ns/instruction that is ~6.9 s of wall time
    // at a 500k budget, on the single `LogicWorker` thread every tab shares —
    // one document could freeze all of them. Charging the cascade to one
    // counter makes the bound `2 * MAX_INSTRUCTIONS` per event (the action,
    // then all recomputation) regardless of how many comps a document
    // declares, so the budget constant means what it appears to mean.
    //
    // `formal/MizuFormal/Budget.lean`'s `T1_reaction_bound` proves
    // `work <= (1 + #comps) * B + N`. That still holds — it is now merely
    // conservative rather than tight, which is the safe direction for a
    // bound to move.
    let budget = store.evaluator.max_instructions;
    let (mut spent_tainted, mut spent_untainted) = (0u64, 0u64);
    while let Some(i) = candidates.pop_first() {
        let cb = &bindings[i];
        // A binding with no dependencies has nothing to intersect `changed`
        // with — `.any()` on an empty `depends_on` is vacuously `false`,
        // which would otherwise skip it forever despite it being a
        // deliberate candidate (see the zero-dependency seeding above).
        if !cb.depends_on.is_empty() && !cb.depends_on.iter().any(|dep| changed.contains(dep)) {
            continue;
        }
        // Spend from this binding's own pool, then bank what it used. The
        // evaluator carries a single counter, so the pools are swapped in and
        // out around each evaluation.
        if pool_exhausted(cb.tainted, spent_tainted, spent_untainted, budget) {
            continue;
        }
        store.evaluator.instruction_count = if cb.tainted {
            spent_tainted
        } else {
            spent_untainted
        };
        let evaluated = store.evaluator.evaluate(
            cb.expr.root(),
            0,
            functions,
            &store.interner,
            &cb.expr.arena,
        );
        if cb.tainted {
            spent_tainted = store.evaluator.instruction_count;
        } else {
            spent_untainted = store.evaluator.instruction_count;
        }
        if matches!(evaluated, Err(MizuError::Timeout)) {
            // This pool is spent. Keep going: the *other* pool may still have
            // room, and skipping its bindings because of this one is exactly
            // the starvation the split exists to prevent.
            tracing::warn!(
                tainted = cb.tainted,
                budget,
                "comp recomputation exhausted an instruction pool; remaining                  bindings of that taint class were not updated"
            );
            continue;
        }
        if let Ok(val) = evaluated {
            store.set_symbol(cb.name, val);
            if changed.insert(cb.name)
                && let Some(idxs) = reverse_index.get(&cb.name)
            {
                candidates.extend(idxs.iter().copied());
            }
        }
    }
    changed
}

/// Reference implementation kept byte-for-byte equivalent to the pre-index
/// O(#bindings) algorithm, used only to verify [`recompute_computed_bindings`]
/// stays behaviourally identical after the reverse-index optimization (see
/// `test_recompute_matches_naive_scan_randomized`).
#[cfg(test)]
pub(crate) fn recompute_computed_bindings_naive_scan(
    store: &mut VariableStore,
    bindings: &[ComputedBinding],
    functions: &FxHashMap<Symbol, MizuFunction>,
    mutated: &FxHashSet<Symbol>,
) -> FxHashSet<Symbol> {
    if bindings.is_empty() {
        return mutated.clone();
    }
    let mut changed = mutated.clone();
    // Shares one budget across the cascade, matching `recompute_computed_bindings`
    // — the equivalence this reference implementation exists to check covers
    // budget behaviour too, not just which bindings get visited.
    let budget = store.evaluator.max_instructions;
    let (mut spent_tainted, mut spent_untainted) = (0u64, 0u64);
    for cb in bindings {
        // See the matching comment in `recompute_computed_bindings`: a
        // no-dependency binding must still get a chance to run once (its
        // `.any()` on an empty `depends_on` is vacuously `false`), gated
        // the same way there — on it not having been evaluated yet.
        let no_deps_pending = cb.depends_on.is_empty()
            && matches!(store.evaluator.get_global(cb.name), Value::Null);
        if !no_deps_pending && !cb.depends_on.iter().any(|dep| changed.contains(dep)) {
            continue;
        }
        // Spend from this binding's own pool, then bank what it used. The
        // evaluator carries a single counter, so the pools are swapped in and
        // out around each evaluation.
        if pool_exhausted(cb.tainted, spent_tainted, spent_untainted, budget) {
            continue;
        }
        store.evaluator.instruction_count = if cb.tainted {
            spent_tainted
        } else {
            spent_untainted
        };
        let evaluated = store.evaluator.evaluate(
            cb.expr.root(),
            0,
            functions,
            &store.interner,
            &cb.expr.arena,
        );
        if cb.tainted {
            spent_tainted = store.evaluator.instruction_count;
        } else {
            spent_untainted = store.evaluator.instruction_count;
        }
        if matches!(evaluated, Err(MizuError::Timeout)) {
            continue;
        }
        if let Ok(val) = evaluated {
            store.set_symbol(cb.name, val);
            changed.insert(cb.name);
        }
    }
    changed
}

/// Walks `expr` and collects every [`Expr::Variable`] symbol into `out`.
///
/// Used by [`parse_computed`] to derive the static dependency set of a `comp`
/// right-hand side.  Function names that appear as `Expr::Variable` in the AST
/// (zero-arg calls written without parentheses) are included; the caller must
/// remove the binding's own name and any pure built-ins if desired.
fn collect_vars(expr: &Expr, arena: &ExprArena, out: &mut FxHashSet<Symbol>) {
    match expr {
        Expr::Variable(sym) => {
            out.insert(*sym);
        }
        Expr::Literal(_) => {}
        Expr::BinaryOp { left, right, .. } => {
            collect_vars(&arena[*left], arena, out);
            collect_vars(&arena[*right], arena, out);
        }
        Expr::FunctionCall {
            args_start,
            args_len,
            ..
        } => {
            for &arg in arena.args(*args_start, *args_len) {
                collect_vars(&arena[arg], arena, out);
            }
        }
        Expr::Let { value, body, .. } => {
            collect_vars(&arena[*value], arena, out);
            collect_vars(&arena[*body], arena, out);
        }
        Expr::Not(inner) => collect_vars(&arena[*inner], arena, out),
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            collect_vars(&arena[*condition], arena, out);
            collect_vars(&arena[*then_expr], arena, out);
            collect_vars(&arena[*else_expr], arena, out);
        }
        Expr::FieldAccess { base, .. } => collect_vars(&arena[*base], arena, out),
    }
}

/// Like [`collect_vars`], but only collects *free* variable references —
/// skips any [`Expr::Variable`] whose symbol is in `bound` (a function's own
/// parameters, seeded by the caller, plus any name locally bound by an
/// enclosing [`Expr::Let`] walked so far).
///
/// Used by [`collect_reachable_function_reads`] to find the globals a called
/// function's body actually reads. Without this, walking `double(x: num) :
/// x * 2`'s body with plain `collect_vars` would report `x` — the
/// function's own parameter, not a global — as a data dependency of every
/// `comp` that calls `double`.
fn collect_free_vars(
    expr: &Expr,
    arena: &ExprArena,
    bound: &mut FxHashSet<Symbol>,
    out: &mut FxHashSet<Symbol>,
) {
    match expr {
        Expr::Variable(sym) => {
            if !bound.contains(sym) {
                out.insert(*sym);
            }
        }
        Expr::Literal(_) => {}
        Expr::BinaryOp { left, right, .. } => {
            collect_free_vars(&arena[*left], arena, bound, out);
            collect_free_vars(&arena[*right], arena, bound, out);
        }
        Expr::FunctionCall {
            args_start,
            args_len,
            ..
        } => {
            for &arg in arena.args(*args_start, *args_len) {
                collect_free_vars(&arena[arg], arena, bound, out);
            }
        }
        Expr::Let { name, value, body } => {
            // `name` is not in scope while evaluating its own initializer.
            collect_free_vars(&arena[*value], arena, bound, out);
            let newly_bound = bound.insert(*name);
            collect_free_vars(&arena[*body], arena, bound, out);
            if newly_bound {
                bound.remove(name);
            }
        }
        Expr::Not(inner) => collect_free_vars(&arena[*inner], arena, bound, out),
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            collect_free_vars(&arena[*condition], arena, bound, out);
            collect_free_vars(&arena[*then_expr], arena, bound, out);
            collect_free_vars(&arena[*else_expr], arena, bound, out);
        }
        Expr::FieldAccess { base, .. } => collect_free_vars(&arena[*base], arena, bound, out),
    }
}

/// Collects all `FunctionCall` and variable reference symbols that match defined functions.
pub(super) fn collect_calls(
    expr: &Expr,
    arena: &ExprArena,
    out: &mut FxHashSet<Symbol>,
    function_names: &FxHashSet<Symbol>,
) {
    match expr {
        Expr::Literal(_) => {}
        Expr::Variable(sym) => {
            if function_names.contains(sym) {
                out.insert(*sym);
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_calls(&arena[*left], arena, out, function_names);
            collect_calls(&arena[*right], arena, out, function_names);
        }
        Expr::FunctionCall {
            name: sym,
            args_start,
            args_len,
        } => {
            out.insert(*sym);
            for &arg in arena.args(*args_start, *args_len) {
                collect_calls(&arena[arg], arena, out, function_names);
            }
        }
        Expr::Let { value, body, .. } => {
            collect_calls(&arena[*value], arena, out, function_names);
            collect_calls(&arena[*body], arena, out, function_names);
        }
        Expr::Not(inner) => collect_calls(&arena[*inner], arena, out, function_names),
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            collect_calls(&arena[*condition], arena, out, function_names);
            collect_calls(&arena[*then_expr], arena, out, function_names);
            collect_calls(&arena[*else_expr], arena, out, function_names);
        }
        Expr::FieldAccess { base, .. } => collect_calls(&arena[*base], arena, out, function_names),
    }
}

/// Unions into `out` every variable symbol read by any function transitively
/// reachable from `expr` through the call graph.
///
/// Used by [`parse_computed_with_functions`] to make `comp` dependency sets
/// sound with respect to globals read *inside* called functions.  The walk is
/// an iterative worklist with a visited set, so it terminates even on a
/// (DAG-check-rejected, hence impossible) cyclic graph — defence in depth.
fn collect_reachable_function_reads(
    expr: &Expr,
    arena: &ExprArena,
    functions: &FxHashMap<Symbol, MizuFunction>,
    function_names: &FxHashSet<Symbol>,
    out: &mut FxHashSet<Symbol>,
) {
    let mut initial_calls: FxHashSet<Symbol> = FxHashSet::default();
    collect_calls(expr, arena, &mut initial_calls, function_names);

    let mut visited: FxHashSet<Symbol> = FxHashSet::default();
    let mut worklist: Vec<Symbol> = initial_calls.into_iter().collect();

    while let Some(sym) = worklist.pop() {
        if !visited.insert(sym) {
            continue;
        }
        let Some(func) = functions.get(&sym) else {
            continue;
        };
        // Free variables only: `double(x: num) : x * 2` reads `x`, but `x`
        // is `double`'s own parameter, not a global the caller depends on.
        // Plain `collect_vars` doesn't know that — it would add `x` (and,
        // for a function with a `let`-local like `clamp`'s `limited`, that
        // too) to `out` as if it were a global read by every comp that
        // calls `double`, which is wrong regardless of the blanket-removal
        // issue fixed at this function's call site.
        let mut bound: FxHashSet<Symbol> = func.params.iter().map(|(p, _)| *p).collect();
        collect_free_vars(func.body.root(), &func.body.arena, &mut bound, out);
        let mut nested_calls: FxHashSet<Symbol> = FxHashSet::default();
        collect_calls(
            func.body.root(),
            &func.body.arena,
            &mut nested_calls,
            function_names,
        );
        worklist.extend(nested_calls);
    }
}
