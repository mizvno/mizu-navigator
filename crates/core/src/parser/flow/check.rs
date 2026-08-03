//! `check_information_flow`: the top-level entry point that enforces
//! invariant F1 via sound, iterative taint propagation over the DAG of
//! functions, computed variables, and assignments.

use rustc_hash::{FxHashMap, FxHashSet};

use crate::core::errors::MizuError;
use crate::core::types::Symbol;
use crate::parser::layout::{EventBlock, MizuNode};
use crate::parser::logic::{Action, ComputedBinding, MizuFunction};
use crate::parser::urls::UrlRegistry;

use super::helpers::{
    action_exprs, build_taint_path, collect_get_system_time_targets, is_expr_tainted,
};
use super::types::{ActionContext, TaintOrigin};

/// Enforces invariant F1 (see `SECURITY-INVARIANTS.md`).  Sound, iterative
/// propagation over the DAG.  Returns `(sources, sinks, violations)` on
/// success, or the first violating flow as a parse error with a
/// human-readable path (source var → … → sink).
pub fn check_information_flow(
    dom: &ego_tree::Tree<MizuNode>,
    timers: &[crate::parser::logic::RootTimer],
    functions: &FxHashMap<Symbol, MizuFunction>,
    comps: &mut [ComputedBinding],
    _urls: &UrlRegistry,
    interner: &crate::core::types::StringInterner,
) -> Result<(usize, usize, usize), MizuError> {
    // `get_system_time`'s single argument is a write-target identifier fixed
    // at parse time (`parser::logic.rs` rejects anything but a bare
    // identifier there) — not a value read. `gst_sym` lets the taint walk
    // recognise and skip over it structurally, the same way `Action::Assign`'s
    // own `target` is never itself taint-checked (only its RHS `expr` is).
    let gst_sym = interner.get("get_system_time");

    let mut tainted_vars: FxHashSet<Symbol> = FxHashSet::default();
    let mut tainted_functions: FxHashSet<Symbol> = FxHashSet::default();
    let mut taint_origins: FxHashMap<Symbol, TaintOrigin> = FxHashMap::default();

    // Collect all actions and their contexts
    let mut actions: Vec<(ActionContext, &Action)> = Vec::new();

    // 1. Traverse layout for events
    for node in dom.nodes() {
        for block in node.value().events.values() {
            match block {
                EventBlock::Click { action } | EventBlock::Submit { action } => {
                    actions.push((ActionContext::UserGesture, action));
                }
            }
        }
    }

    // 2. Add root timers
    for timer in timers {
        actions.push((ActionContext::NonInteractive, &timer.action));
    }

    // ── Initialize tainted sources ──────────────────────────────────────────

    // $form fields are tainted (user input)
    if let Some(form_sym) = interner.get("$form") {
        tainted_vars.insert(form_sym);
        taint_origins.insert(form_sym, TaintOrigin::FormField);
    }

    // NetworkCall target_var is tainted (values from the network)
    for (_, action) in &actions {
        if let Action::NetworkCall {
            target_var,
            method,
            alias_sym,
            ..
        } = action
            && let Some(sym) = interner.get(target_var)
        {
            tainted_vars.insert(sym);
            let alias_name = interner.resolve(*alias_sym).unwrap_or("<unknown>");
            taint_origins.insert(
                sym,
                TaintOrigin::NetworkResponse {
                    action_desc: format!("{method:?}({alias_name})"),
                },
            );
        }
    }

    let source_count = tainted_vars.len();

    // ── Propagation (fixpoint) ──────────────────────────────────────────────

    let mut worklist: Vec<Symbol> = tainted_vars.iter().copied().collect();

    #[derive(Clone)]
    enum Dependent<'a> {
        Function(Symbol),
        Comp(&'a ComputedBinding),
        Assign(Symbol),
    }

    let mut graph: FxHashMap<Symbol, Vec<Dependent>> = FxHashMap::default();

    fn extract_deps(
        expr: &crate::parser::logic::Expr,
        arena: &crate::parser::logic::ExprArena,
        gst_sym: Option<Symbol>,
        deps: &mut FxHashSet<Symbol>,
    ) {
        use crate::parser::logic::Expr;
        match expr {
            Expr::Variable(sym) => {
                deps.insert(*sym);
            }
            Expr::FunctionCall {
                name,
                args_start,
                args_len,
            } => {
                if Some(*name) != gst_sym {
                    deps.insert(*name);
                }
                for arg_idx in 0..*args_len {
                    extract_deps(
                        &arena[arena.args(*args_start, *args_len)[arg_idx as usize]],
                        arena,
                        gst_sym,
                        deps,
                    );
                }
            }
            Expr::BinaryOp { left, right, .. } => {
                extract_deps(&arena[*left], arena, gst_sym, deps);
                extract_deps(&arena[*right], arena, gst_sym, deps);
            }
            Expr::Not(operand) => {
                extract_deps(&arena[*operand], arena, gst_sym, deps);
            }
            Expr::FieldAccess { base, .. } => {
                extract_deps(&arena[*base], arena, gst_sym, deps);
            }
            Expr::Let { value, body, .. } => {
                extract_deps(&arena[*value], arena, gst_sym, deps);
                extract_deps(&arena[*body], arena, gst_sym, deps);
            }
            Expr::IfElse {
                condition,
                then_expr,
                else_expr,
            } => {
                extract_deps(&arena[*condition], arena, gst_sym, deps);
                extract_deps(&arena[*then_expr], arena, gst_sym, deps);
                extract_deps(&arena[*else_expr], arena, gst_sym, deps);
            }
            Expr::Literal(_) => {}
        }
    }
    // Build the graph
    for (sym, func) in functions {
        let mut deps = FxHashSet::default();
        extract_deps(func.body.root(), &func.body.arena, gst_sym, &mut deps);
        for dep in deps {
            graph
                .entry(dep)
                .or_default()
                .push(Dependent::Function(*sym));
        }
    }

    for comp in comps.iter() {
        let mut deps = FxHashSet::default();
        extract_deps(comp.expr.root(), &comp.expr.arena, gst_sym, &mut deps);
        for dep in deps {
            graph.entry(dep).or_default().push(Dependent::Comp(comp));
        }
    }

    for (_, action) in &actions {
        if let Action::Assign { target, expr } = action {
            if let Some(target_sym) = interner.get(target) {
                let mut deps = FxHashSet::default();
                extract_deps(expr.root(), &expr.arena, gst_sym, &mut deps);
                for dep in deps {
                    graph
                        .entry(dep)
                        .or_default()
                        .push(Dependent::Assign(target_sym));
                }
            }
        }
    }

    while let Some(tainted_sym) = worklist.pop() {
        if let Some(dependents) = graph.get(&tainted_sym) {
            for dep in dependents {
                match dep {
                    Dependent::Function(sym) => {
                        if !tainted_functions.contains(sym) {
                            tainted_functions.insert(*sym);
                            worklist.push(*sym);
                        }
                    }
                    Dependent::Comp(comp) => {
                        if !tainted_vars.contains(&comp.name) {
                            tainted_vars.insert(comp.name);
                            worklist.push(comp.name);
                            let from_name = interner
                                .resolve(tainted_sym)
                                .unwrap_or("<unknown>")
                                .to_string();
                            taint_origins.insert(
                                comp.name,
                                TaintOrigin::Propagated {
                                    from_var: from_name,
                                },
                            );
                        }
                    }
                    Dependent::Assign(target_sym) => {
                        if !tainted_vars.contains(target_sym) {
                            tainted_vars.insert(*target_sym);
                            worklist.push(*target_sym);
                            let from_name = interner
                                .resolve(tainted_sym)
                                .unwrap_or("<unknown>")
                                .to_string();
                            taint_origins.insert(
                                *target_sym,
                                TaintOrigin::Propagated {
                                    from_var: from_name,
                                },
                            );
                        }
                    }
                }
            }
        }
    }

    // ── get_system_time targets: treat like Action::Assign with a static
    // Symbol ───────────────────────────────────────────────────────────────
    //
    // `get_system_time(target)` can appear anywhere an expression can (inside
    // `Action::Eval`, an `Assign`'s RHS, a `comp`, a function body — it is
    // not restricted to a top-level `Action` the way `Assign`/`Navigate`/
    // `NetworkCall` are). Now that its argument is a parse-time-fixed
    // Symbol (`parser::logic.rs`), every occurrence is enumerable: walk
    // every expression the checker already has in hand — function bodies,
    // `comp` RHSs, and every action's constituent expression(s) — and reject
    // the document if any target names a `comp` (computed) variable. This is
    // the same protection `execute_action` already gives ordinary `Assign`,
    // but enforced at load time (fail-closed) instead of only when the
    // owning timer/handler happens to fire at runtime.
    if let Some(gst_sym) = gst_sym {
        let mut gst_targets = Vec::new();
        for func in functions.values() {
            collect_get_system_time_targets(
                func.body.root(),
                &func.body.arena,
                gst_sym,
                &mut gst_targets,
            );
        }
        for comp in comps.iter() {
            collect_get_system_time_targets(
                comp.expr.root(),
                &comp.expr.arena,
                gst_sym,
                &mut gst_targets,
            );
        }
        for (_, action) in &actions {
            for tree in action_exprs(action) {
                collect_get_system_time_targets(
                    tree.root(),
                    &tree.arena,
                    gst_sym,
                    &mut gst_targets,
                );
            }
        }
        for target in gst_targets {
            if let Some(comp) = comps.iter().find(|c| c.name == target) {
                let name = interner.resolve(comp.name).unwrap_or("<unknown>");
                return Err(MizuError::ParseError(format!(
                    "get_system_time cannot target `{name}`: it is a computed \
                     (`comp`) variable, which cannot be assigned to."
                )));
            }
        }
    }

    // ── Check sinks ─────────────────────────────────────────────────────────

    // Record, on each binding, whether the fixpoint classified it as tainted.
    // `recompute_computed_bindings` spends tainted and untainted comps from
    // separate instruction pools, so that computation driven by a network
    // response cannot exhaust the budget an untainted binding needs. Without
    // the split, a hostile server could change an untainted value simply by
    // making its own response expensive — a starvation channel around the very
    // non-interference guarantee this checker establishes.
    for cb in comps.iter_mut() {
        cb.tainted = tainted_vars.contains(&cb.name);
    }

    let mut num_sinks = 0;
    for (ctx, action) in &actions {
        // path_param is NOT a sink — it is gated by construction via the
        // runtime A1+A2 validation (single segment, no delimiters,
        // percent-encoded).  See SECURITY-INVARIANTS.md §5, gate G2.
        if let Action::Navigate { url } = action {
            num_sinks += 1;
            if is_expr_tainted(
                url.root(),
                &url.arena,
                &tainted_vars,
                &tainted_functions,
                gst_sym,
            ) {
                // Gate G1: user-gesture navigation discharges taint
                if *ctx != ActionContext::UserGesture {
                    // Build diagnostic path (F3)
                    let path = build_taint_path(
                        url.root(),
                        &url.arena,
                        &tainted_vars,
                        &tainted_functions,
                        &taint_origins,
                        interner,
                        gst_sym,
                    );
                    return Err(MizuError::ParseError(format!(
                        "Information Flow Violation: {path} reaches 'navigate' \
                         without a user gesture gate."
                    )));
                }
            }
        }
    }

    Ok((source_count, num_sinks, 0))
}
