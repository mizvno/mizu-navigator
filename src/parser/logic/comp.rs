//! `comp` (computed variable) parsing, dependency tracking, and incremental
//! recomputation.

use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::{BTreeSet, VecDeque};

use crate::core::errors::MizuError;
use crate::core::types::{StringInterner, Symbol, VariableStore};

use super::ast::{ComputedBinding, Expr, MizuFunction};
use super::lexer::{Cursor, assert_cursor_empty, leading_spaces, lex};
use super::parse::parse_expr;

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
    parse_computed_with_functions(logic_content, interner, &FxHashMap::default())
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
        let expr = parse_expr(&mut cursor, 0, 0, interner)?;
        assert_cursor_empty(&cursor, expr_src)?;

        let name_sym = interner.get_or_intern(name);
        let mut dep_set: FxHashSet<Symbol> = FxHashSet::default();
        collect_vars(&expr, &mut dep_set);
        // Union the globals read inside every function reachable from the RHS,
        // so mutations to those globals also trigger a recompute.
        collect_reachable_function_reads(&expr, functions, &function_names, &mut dep_set);
        // Function names are code references, not data dependencies.
        for fname in &function_names {
            dep_set.remove(fname);
        }
        dep_set.remove(&name_sym);

        bindings.push(ComputedBinding {
            name: name_sym,
            expr,
            depends_on: dep_set.into_iter().collect(),
        });
    }

    // Reject documents that declare more `comp` bindings than
    // MAX_COMP_BINDINGS. `recompute_computed_bindings` grants each firing
    // comp its own fresh MAX_INSTRUCTIONS budget (see that constant's docs
    // and `formal/MizuFormal/Budget.lean`'s `T1_reaction_bound`), so an
    // unbounded comp count would let a single event cascade through
    // arbitrarily many full-budget re-evaluations. Rejecting here, at parse
    // time, turns that into a clear load-time error instead of a runtime
    // DoS or an undiagnosable timeout.
    if bindings.len() > crate::core::types::MAX_COMP_BINDINGS {
        return Err(MizuError::ParseError(format!(
            "document declares {} `comp` bindings, exceeding the maximum of {} \
             (MAX_COMP_BINDINGS); split the logic across fewer computed variables \
             or reduce reliance on derived state",
            bindings.len(),
            crate::core::types::MAX_COMP_BINDINGS
        )));
    }

    topo_sort_computed(bindings)
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
/// `store.state_machine.undo_log` via [`VariableStore::set_symbol`], so it will be
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

    while let Some(i) = candidates.pop_first() {
        let cb = &bindings[i];
        if !cb.depends_on.iter().any(|dep| changed.contains(dep)) {
            continue;
        }
        store.state_machine.instruction_count = 0;
        if let Ok(val) = store
            .state_machine
            .evaluate(&cb.expr, 0, functions, &store.interner)
        {
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
    for cb in bindings {
        if !cb.depends_on.iter().any(|dep| changed.contains(dep)) {
            continue;
        }
        store.state_machine.instruction_count = 0;
        if let Ok(val) = store
            .state_machine
            .evaluate(&cb.expr, 0, functions, &store.interner)
        {
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
fn collect_vars(expr: &Expr, out: &mut FxHashSet<Symbol>) {
    match expr {
        Expr::Variable(sym) => {
            out.insert(*sym);
        }
        Expr::Literal(_) => {}
        Expr::BinaryOp { left, right, .. } => {
            collect_vars(left, out);
            collect_vars(right, out);
        }
        Expr::FunctionCall { args, .. } => {
            for arg in args {
                collect_vars(arg, out);
            }
        }
        Expr::Let { value, body, .. } => {
            collect_vars(value, out);
            collect_vars(body, out);
        }
        Expr::Not(inner) => collect_vars(inner, out),
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            collect_vars(condition, out);
            collect_vars(then_expr, out);
            collect_vars(else_expr, out);
        }
        Expr::FieldAccess { base, .. } => collect_vars(base, out),
    }
}

/// Collects all `FunctionCall` and variable reference symbols that match defined functions.
pub(super) fn collect_calls(expr: &Expr, out: &mut FxHashSet<Symbol>, function_names: &FxHashSet<Symbol>) {
    match expr {
        Expr::Literal(_) => {}
        Expr::Variable(sym) => {
            if function_names.contains(sym) {
                out.insert(*sym);
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_calls(left, out, function_names);
            collect_calls(right, out, function_names);
        }
        Expr::FunctionCall { name: sym, args } => {
            out.insert(*sym);
            for arg in args {
                collect_calls(arg, out, function_names);
            }
        }
        Expr::Let { value, body, .. } => {
            collect_calls(value, out, function_names);
            collect_calls(body, out, function_names);
        }
        Expr::Not(inner) => collect_calls(inner, out, function_names),
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            collect_calls(condition, out, function_names);
            collect_calls(then_expr, out, function_names);
            collect_calls(else_expr, out, function_names);
        }
        Expr::FieldAccess { base, .. } => collect_calls(base, out, function_names),
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
    functions: &FxHashMap<Symbol, MizuFunction>,
    function_names: &FxHashSet<Symbol>,
    out: &mut FxHashSet<Symbol>,
) {
    let mut initial_calls: FxHashSet<Symbol> = FxHashSet::default();
    collect_calls(expr, &mut initial_calls, function_names);

    let mut visited: FxHashSet<Symbol> = FxHashSet::default();
    let mut worklist: Vec<Symbol> = initial_calls.into_iter().collect();

    while let Some(sym) = worklist.pop() {
        if !visited.insert(sym) {
            continue;
        }
        let Some(func) = functions.get(&sym) else {
            continue;
        };
        collect_vars(&func.body, out);
        let mut nested_calls: FxHashSet<Symbol> = FxHashSet::default();
        collect_calls(&func.body, &mut nested_calls, function_names);
        worklist.extend(nested_calls);
    }
}
