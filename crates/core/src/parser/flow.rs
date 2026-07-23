//! # `flow` â€” Load-time Information Flow Checker
//!
//! Enforces invariant **F1** (gated information flow) from
//! `SECURITY-INVARIANTS.md`.  Runs after `check_dag` and `comp` extraction,
//! before the document is considered ready.
//!
//! ## Algorithm
//!
//! The checker computes a tainted-symbol set by iterative propagation over the
//! DAG of functions, computed variables, and assignments.  Because the graph is
//! acyclic and finite (enforced by `check_dag`), the fixpoint converges in a
//! bounded number of iterations.
//!
//! After convergence, every sink expression (`Action::Navigate.url`) is checked:
//! a sink whose expression reads any tainted symbol without a discharging gate
//! is rejected.
//!
//! ## Soundness
//!
//! The checker is **sound** (never misses a real sourceâ†’sink flow) and **may be
//! conservative** (over-approximation â†’ spurious rejection is acceptable).
//! Any analysis uncertainty (unresolved symbol, unexpected node) is treated as
//! tainted/rejected, never as clean.

use crate::core::errors::MizuError;
use crate::core::types::Symbol;
use crate::parser::logic::{Action, Expr, MizuFunction, ComputedBinding};
use crate::parser::layout::{MizuNode, EventBlock};
use crate::parser::urls::UrlRegistry;
use rustc_hash::{FxHashMap, FxHashSet};

/// Context of an action to determine if it passes a gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionContext {
    /// A user gesture event, e.g. click or submit.  Acts as a gate for
    /// cross-origin navigation (gate G1 in `SECURITY-INVARIANTS.md`).
    UserGesture,
    /// A non-interactive trigger, e.g. root timer or network response.
    NonInteractive,
}

/// Why a variable became tainted â€” used for diagnostic messages (F3).
#[derive(Debug, Clone)]
enum TaintOrigin {
    /// Tainted because it receives the response from a network call.
    NetworkResponse { action_desc: String },
    /// Tainted because it is bound from a `$form` field.
    FormField,
    /// Tainted because it was assigned/computed from another tainted variable.
    Propagated { from_var: String },
}

/// Enforces invariant F1 (see `SECURITY-INVARIANTS.md`).  Sound, iterative
/// propagation over the DAG.  Returns `(sources, sinks, violations)` on
/// success, or the first violating flow as a parse error with a
/// human-readable path (source var â†’ â€¦ â†’ sink).
pub fn check_information_flow(
    dom: &ego_tree::Tree<MizuNode>,
    timers: &[crate::parser::logic::RootTimer],
    functions: &FxHashMap<Symbol, MizuFunction>,
    comps: &[ComputedBinding],
    _urls: &UrlRegistry,
    interner: &crate::core::types::StringInterner,
) -> Result<(usize, usize, usize), MizuError> {
    // `get_system_time`'s single argument is a write-target identifier fixed
    // at parse time (`parser::logic.rs` rejects anything but a bare
    // identifier there) â€” not a value read. `gst_sym` lets the taint walk
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

    // â”€â”€ Initialize tainted sources â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    // $form fields are tainted (user input)
    if let Some(form_sym) = interner.get("$form") {
        tainted_vars.insert(form_sym);
        taint_origins.insert(form_sym, TaintOrigin::FormField);
    }

    // NetworkCall target_var is tainted (values from the network)
    for (_, action) in &actions {
        if let Action::NetworkCall { target_var, method, alias_sym, .. } = action
            && let Some(sym) = interner.get(target_var)
        {
            tainted_vars.insert(sym);
            let alias_name = interner.resolve(*alias_sym).unwrap_or("<unknown>");
            taint_origins.insert(sym, TaintOrigin::NetworkResponse {
                action_desc: format!("{method:?}({alias_name})"),
            });
        }
    }

    let source_count = tainted_vars.len();

    // â”€â”€ Propagation (fixpoint) â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    let mut changed = true;
    while changed {
        changed = false;

        // Propagate through user-defined functions
        for (sym, func) in functions {
            if !tainted_functions.contains(sym)
                && is_expr_tainted(&func.body, &tainted_vars, &tainted_functions, gst_sym)
            {
                tainted_functions.insert(*sym);
                changed = true;
            }
        }

        // Propagate through ComputedBindings
        for comp in comps {
            if !tainted_vars.contains(&comp.name)
                && is_expr_tainted(&comp.expr, &tainted_vars, &tainted_functions, gst_sym)
            {
                tainted_vars.insert(comp.name);
                // Track the propagation origin for diagnostics
                if let Some(source_sym) = find_tainted_var_in_expr(
                    &comp.expr, &tainted_vars, &tainted_functions, gst_sym,
                ) {
                    let from_name = interner.resolve(source_sym)
                        .unwrap_or("<unknown>").to_string();
                    taint_origins.insert(comp.name, TaintOrigin::Propagated {
                        from_var: from_name,
                    });
                }
                changed = true;
            }
        }

        // Propagate through Assign actions
        for (_, action) in &actions {
            match action {
                Action::Assign { target, expr } => {
                    if let Some(target_sym) = interner.get(target)
                        && !tainted_vars.contains(&target_sym)
                        && is_expr_tainted(expr, &tainted_vars, &tainted_functions, gst_sym)
                    {
                        tainted_vars.insert(target_sym);
                        if let Some(source_sym) = find_tainted_var_in_expr(
                            expr, &tainted_vars, &tainted_functions, gst_sym,
                        ) {
                            let from_name = interner.resolve(source_sym)
                                .unwrap_or("<unknown>").to_string();
                            taint_origins.insert(target_sym, TaintOrigin::Propagated {
                                from_var: from_name,
                            });
                        }
                        changed = true;
                    }
                }
                Action::NetworkCall { target_var, .. } => {
                    if let Some(target_sym) = interner.get(target_var)
                        && !tainted_vars.contains(&target_sym)
                    {
                        tainted_vars.insert(target_sym);
                        changed = true;
                    }
                }
                _ => {}
            }
        }
    }

    // â”€â”€ get_system_time targets: treat like Action::Assign with a static
    // Symbol â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
    //
    // `get_system_time(target)` can appear anywhere an expression can (inside
    // `Action::Eval`, an `Assign`'s RHS, a `comp`, a function body â€” it is
    // not restricted to a top-level `Action` the way `Assign`/`Navigate`/
    // `NetworkCall` are). Now that its argument is a parse-time-fixed
    // Symbol (`parser::logic.rs`), every occurrence is enumerable: walk
    // every expression the checker already has in hand â€” function bodies,
    // `comp` RHSs, and every action's constituent expression(s) â€” and reject
    // the document if any target names a `comp` (computed) variable. This is
    // the same protection `execute_action` already gives ordinary `Assign`,
    // but enforced at load time (fail-closed) instead of only when the
    // owning timer/handler happens to fire at runtime.
    if let Some(gst_sym) = gst_sym {
        let mut gst_targets = Vec::new();
        for func in functions.values() {
            collect_get_system_time_targets(&func.body, gst_sym, &mut gst_targets);
        }
        for comp in comps {
            collect_get_system_time_targets(&comp.expr, gst_sym, &mut gst_targets);
        }
        for (_, action) in &actions {
            for expr in action_exprs(action) {
                collect_get_system_time_targets(expr, gst_sym, &mut gst_targets);
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

    // â”€â”€ Check sinks â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    let mut num_sinks = 0;
    for (ctx, action) in &actions {
        // path_param is NOT a sink â€” it is gated by construction via the
        // runtime A1+A2 validation (single segment, no delimiters,
        // percent-encoded).  See SECURITY-INVARIANTS.md Â§5, gate G2.
        if let Action::Navigate { url } = action {
            num_sinks += 1;
            if is_expr_tainted(url, &tainted_vars, &tainted_functions, gst_sym) {
                // Gate G1: user-gesture navigation discharges taint
                if *ctx != ActionContext::UserGesture {
                    // Build diagnostic path (F3)
                    let path = build_taint_path(
                        url, &tainted_vars, &tainted_functions,
                        &taint_origins, interner, gst_sym,
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

/// Every expression an `Action` directly embeds, in evaluation order.
/// Used to walk the whole action graph looking for `get_system_time` calls,
/// which â€” unlike `Navigate`/`NetworkCall`/`Assign` â€” are not a top-level
/// `Action` variant of their own and can be nested anywhere inside these.
fn action_exprs(action: &Action) -> Vec<&Expr> {
    match action {
        Action::Eval(e) => vec![e],
        Action::Assign { expr, .. } => vec![expr],
        Action::Navigate { url } => vec![url],
        Action::NetworkCall { payload, path_param, .. } => {
            let mut exprs = Vec::new();
            if let Some(p) = payload {
                exprs.push(p.as_ref());
            }
            if let Some(p) = path_param {
                exprs.push(p.as_ref());
            }
            exprs
        }
    }
}

/// Collects the target `Symbol` of every `get_system_time(target)` call
/// found anywhere within `expr`. `gst_sym` is the interned `get_system_time`
/// symbol (see `check_information_flow`). Since the parser now rejects any
/// `get_system_time` argument that isn't a bare identifier
/// (`parser::logic.rs`), every call found here has a statically-known
/// target â€” the whole point of this walk is to make that target visible to
/// the checker, exactly as an `Action::Assign`'s target already is.
fn collect_get_system_time_targets(expr: &Expr, gst_sym: Symbol, out: &mut Vec<Symbol>) {
    match expr {
        Expr::Literal(_) | Expr::Variable(_) => {}
        Expr::BinaryOp { left, right, .. } => {
            collect_get_system_time_targets(left, gst_sym, out);
            collect_get_system_time_targets(right, gst_sym, out);
        }
        Expr::FunctionCall { name, args } => {
            if *name == gst_sym
                && let [Expr::Variable(target)] = args.as_slice()
            {
                out.push(*target);
            }
            for arg in args {
                collect_get_system_time_targets(arg, gst_sym, out);
            }
        }
        Expr::Let { value, body, .. } => {
            collect_get_system_time_targets(value, gst_sym, out);
            collect_get_system_time_targets(body, gst_sym, out);
        }
        Expr::Not(inner) => collect_get_system_time_targets(inner, gst_sym, out),
        Expr::IfElse { condition, then_expr, else_expr } => {
            collect_get_system_time_targets(condition, gst_sym, out);
            collect_get_system_time_targets(then_expr, gst_sym, out);
            collect_get_system_time_targets(else_expr, gst_sym, out);
        }
        Expr::FieldAccess { base, .. } => collect_get_system_time_targets(base, gst_sym, out),
    }
}

/// Checks whether `expr` reads any tainted variable or calls a tainted function.
///
/// `gst_sym`, if `Some`, is the interned `get_system_time` symbol: a call to
/// it is skipped structurally (its argument names a write target, never
/// read as a value â€” see the comment on `gst_sym` in
/// `check_information_flow`) rather than walked like an ordinary argument.
fn is_expr_tainted(
    expr: &Expr,
    tainted_vars: &FxHashSet<Symbol>,
    tainted_functions: &FxHashSet<Symbol>,
    gst_sym: Option<Symbol>,
) -> bool {
    match expr {
        Expr::Variable(sym) => tainted_vars.contains(sym),
        Expr::Literal(_) => false,
        Expr::BinaryOp { left, right, .. } => {
            is_expr_tainted(left, tainted_vars, tainted_functions, gst_sym)
                || is_expr_tainted(right, tainted_vars, tainted_functions, gst_sym)
        }
        Expr::FunctionCall { name, args } => {
            if Some(*name) == gst_sym {
                return false;
            }
            if tainted_functions.contains(name) {
                return true;
            }
            for arg in args {
                if is_expr_tainted(arg, tainted_vars, tainted_functions, gst_sym) {
                    return true;
                }
            }
            false
        }
        Expr::Let { value, body, .. } => {
            is_expr_tainted(value, tainted_vars, tainted_functions, gst_sym)
                || is_expr_tainted(body, tainted_vars, tainted_functions, gst_sym)
        }
        Expr::Not(inner) => is_expr_tainted(inner, tainted_vars, tainted_functions, gst_sym),
        Expr::IfElse { condition, then_expr, else_expr } => {
            is_expr_tainted(condition, tainted_vars, tainted_functions, gst_sym)
                || is_expr_tainted(then_expr, tainted_vars, tainted_functions, gst_sym)
                || is_expr_tainted(else_expr, tainted_vars, tainted_functions, gst_sym)
        }
        Expr::FieldAccess { base, .. } => {
            is_expr_tainted(base, tainted_vars, tainted_functions, gst_sym)
        }
    }
}

/// Finds the first tainted variable symbol in an expression (for origin
/// tracking). `gst_sym`: see `is_expr_tainted`.
fn find_tainted_var_in_expr(
    expr: &Expr,
    tainted_vars: &FxHashSet<Symbol>,
    tainted_functions: &FxHashSet<Symbol>,
    gst_sym: Option<Symbol>,
) -> Option<Symbol> {
    match expr {
        Expr::Variable(sym) => {
            if tainted_vars.contains(sym) { Some(*sym) } else { None }
        }
        Expr::Literal(_) => None,
        Expr::BinaryOp { left, right, .. } => {
            find_tainted_var_in_expr(left, tainted_vars, tainted_functions, gst_sym)
                .or_else(|| find_tainted_var_in_expr(right, tainted_vars, tainted_functions, gst_sym))
        }
        Expr::FunctionCall { name, args } => {
            if Some(*name) == gst_sym {
                return None;
            }
            if tainted_functions.contains(name) {
                return Some(*name);
            }
            for arg in args {
                if let Some(s) = find_tainted_var_in_expr(arg, tainted_vars, tainted_functions, gst_sym) {
                    return Some(s);
                }
            }
            None
        }
        Expr::Let { value, body, .. } => {
            find_tainted_var_in_expr(value, tainted_vars, tainted_functions, gst_sym)
                .or_else(|| find_tainted_var_in_expr(body, tainted_vars, tainted_functions, gst_sym))
        }
        Expr::Not(inner) => find_tainted_var_in_expr(inner, tainted_vars, tainted_functions, gst_sym),
        Expr::IfElse { condition, then_expr, else_expr } => {
            find_tainted_var_in_expr(condition, tainted_vars, tainted_functions, gst_sym)
                .or_else(|| find_tainted_var_in_expr(then_expr, tainted_vars, tainted_functions, gst_sym))
                .or_else(|| find_tainted_var_in_expr(else_expr, tainted_vars, tainted_functions, gst_sym))
        }
        Expr::FieldAccess { base, .. } => {
            find_tainted_var_in_expr(base, tainted_vars, tainted_functions, gst_sym)
        }
    }
}

/// Builds a human-readable taint path for diagnostics (F3).
///
/// Example output: `"value 'next' (tainted from GET(api))"`
fn build_taint_path(
    expr: &Expr,
    tainted_vars: &FxHashSet<Symbol>,
    tainted_functions: &FxHashSet<Symbol>,
    origins: &FxHashMap<Symbol, TaintOrigin>,
    interner: &crate::core::types::StringInterner,
    gst_sym: Option<Symbol>,
) -> String {
    if let Some(sym) = find_tainted_var_in_expr(expr, tainted_vars, tainted_functions, gst_sym) {
        let var_name = interner.resolve(sym).unwrap_or("<unknown>");
        if let Some(origin) = origins.get(&sym) {
            match origin {
                TaintOrigin::NetworkResponse { action_desc } => {
                    format!("value '{var_name}' (from {action_desc})")
                }
                TaintOrigin::FormField => {
                    format!("value '{var_name}' (from $form)")
                }
                TaintOrigin::Propagated { from_var } => {
                    format!("value '{var_name}' (derived from '{from_var}')")
                }
            }
        } else {
            format!("tainted value '{var_name}'")
        }
    } else {
        "tainted expression".to_string()
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::StringInterner;
    use crate::parser::urls::parse_urls;
    use crate::parser::logic::{parse_logic, parse_computed_with_functions, parse_root_timers};
    use crate::parser::layout::parse_layout_with_urls;
    use crate::parser::splitter::split_source_with_origin;

    fn check_flow_doc(src: &str) -> Result<(usize, usize, usize), MizuError> {
        let current_dir = std::env::current_dir().unwrap_or_default();
        let blocks = split_source_with_origin(src, &current_dir, crate::parser::Origin::Network).unwrap();
        let mut interner = StringInterner::new();
        let urls = parse_urls(&blocks.urls_block, &mut interner).unwrap_or_default();
        let functions = parse_logic(&blocks.logic_block, &mut interner).unwrap_or_default();
        let comps = parse_computed_with_functions(&blocks.logic_block, &mut interner, &functions).unwrap_or_default();
        let timers = parse_root_timers(&blocks.logic_block, &mut interner).unwrap_or_default();
        let dom = parse_layout_with_urls(&blocks.layout_block, &mut interner, Some(&urls), true).unwrap();

        check_information_flow(&dom, &timers, &functions, &comps, &urls, &interner)
    }

    // â”€â”€ Core flow violation tests â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn network_var_into_navigate_rejected() {
        let doc = r#"
urls
    api: mizu://api.example.com/
logic
    timer 1s -> GET(api) -> data
    timer 2s -> navigate data
layout
    window
        "#;
        let res = check_flow_doc(doc);
        assert!(res.is_err(), "Expected flow violation");
        let msg = res.unwrap_err().to_string();
        assert!(msg.contains("navigate"), "error should mention navigate: {msg}");
        assert!(msg.contains("data"), "error should mention the tainted var: {msg}");
    }

    #[test]
    fn clean_constant_into_navigate_allowed() {
        let doc = r#"
logic
    timer 1s -> navigate "mizu://safe.com/"
layout
    window
        "#;
        let res = check_flow_doc(doc);
        assert!(res.is_ok(), "Expected flow allowed");
    }

    #[test]
    fn form_field_into_navigate_rejected() {
        let doc = r#"
logic
    timer 1s -> navigate $form.dest
layout
    window
        "#;
        let res = check_flow_doc(doc);
        assert!(res.is_err(), "Expected flow violation from form field");
        let msg = res.unwrap_err().to_string();
        assert!(msg.contains("$form"), "error should mention $form: {msg}");
    }

    #[test]
    fn gated_gesture_navigation_allowed() {
        let doc = r#"
urls
    api: mizu://api.example.com/
logic
    timer 1s -> GET(api) -> data
layout
    window
        button
            click -> navigate data
        "#;
        let res = check_flow_doc(doc);
        assert!(res.is_ok(), "Expected flow allowed for gesture");
    }

    // â”€â”€ path_param is gated by construction â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn network_var_into_path_param_allowed_by_construction() {
        // path_param is NOT a taint sink â€” it is gated by runtime A1+A2
        // validation.  This test verifies the design change from the previous
        // validate_path-based gate to the by-construction gate.
        let doc = r#"
urls
    api: mizu://api.example.com/user/{id}
logic
    timer 1s -> GET(api) -> data
    timer 2s -> GET(api, data) -> profile
layout
    window
        "#;
        let res = check_flow_doc(doc);
        assert!(res.is_ok(), "path_param should be allowed (gated by construction)");
    }

    // â”€â”€ Taint propagation tests â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn taint_propagates_through_binop_let_ifelse_fieldaccess() {
        // `data` is tainted (from GET), navigating `data.url` should be
        // rejected since FieldAccess propagates taint.
        let doc = r#"
urls
    api: mizu://api.example.com/
logic
    timer 1s -> GET(api) -> data
    timer 2s -> navigate data.url
layout
    window
        "#;
        let res = check_flow_doc(doc);
        assert!(res.is_err(), "FieldAccess on tainted var should propagate taint");
    }

    #[test]
    fn pure_literal_flow_untainted() {
        // A constant string should never be tainted
        let doc = r#"
logic
    timer 1s -> navigate "mizu://pure.example.com/"
layout
    window
        "#;
        let res = check_flow_doc(doc);
        assert!(res.is_ok(), "Pure literal should not be tainted");
    }

    #[test]
    fn taint_through_comp_chain_rejected() {
        // source â†’ comp â†’ sink: `data` (from GET) â†’ `comp derived = data` â†’
        // navigate `derived` without gesture should be rejected.
        let doc = r#"
urls
    api: mizu://api.example.com/
logic
    timer 1s -> GET(api) -> data
    comp derived = data
    timer 2s -> navigate derived
layout
    window
        "#;
        let res = check_flow_doc(doc);
        assert!(res.is_err(), "Taint through comp chain should be rejected");
    }

    #[test]
    fn taint_propagates_through_function_return() {
        // A user function that returns a tainted global should taint the result
        let doc = r#"
urls
    api: mizu://api.example.com/
logic
    passthrough(x) : x
    timer 1s -> GET(api) -> data
    timer 2s -> navigate passthrough(data)
layout
    window
        "#;
        let res = check_flow_doc(doc);
        assert!(res.is_err(), "Function returning tainted arg should propagate taint");
    }

    #[test]
    fn taint_propagates_through_transitive_global() {
        // A function reads a tainted global transitively
        let doc = r#"
urls
    api: mizu://api.example.com/
logic
    read_data() : data
    timer 1s -> GET(api) -> data
    timer 2s -> navigate read_data()
layout
    window
        "#;
        let res = check_flow_doc(doc);
        assert!(res.is_err(), "Function reading tainted global should propagate taint");
    }

    // â”€â”€ Precision / over-approximation test â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn over_approximation_may_reject_but_never_misses() {
        // Documented case: `if true then "safe" else data` â€” the checker
        // conservatively marks this as tainted because the else branch reads
        // `data`, even though at runtime the else is never taken.
        // This is acceptable: sound over complete.
        let doc = r#"
urls
    api: mizu://api.example.com/
logic
    timer 1s -> GET(api) -> data
    timer 2s -> navigate if true then "mizu://safe.com/" else data
layout
    window
        "#;
        let res = check_flow_doc(doc);
        // This SHOULD be rejected by the conservative checker (over-approximation)
        assert!(res.is_err(),
            "Conservative checker should reject: dead branch still reads tainted var");
    }

    // â”€â”€ get_system_time: static write-target (RM-04) â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn get_system_time_targeting_comp_variable_rejected() {
        // Load-time equivalent of `execute_action`'s "cannot assign to
        // computed variable" guard, extended to get_system_time now that its
        // target is statically visible to the checker.
        let doc = r#"
logic
    comp derived = 1 + 1
    timer 1s -> get_system_time(derived)
layout
    window
        "#;
        let res = check_flow_doc(doc);
        assert!(
            res.is_err(),
            "Expected rejection: get_system_time cannot target a comp variable"
        );
        let msg = res.unwrap_err().to_string();
        assert!(msg.contains("derived"), "error should name the comp var: {msg}");
        assert!(
            msg.contains("computed") || msg.contains("comp"),
            "error should explain why: {msg}"
        );
    }

    #[test]
    fn get_system_time_targeting_plain_variable_allowed() {
        let doc = r#"
logic
    timer 1s -> get_system_time(elapsed)
layout
    window
        "#;
        let res = check_flow_doc(doc);
        assert!(
            res.is_ok(),
            "get_system_time targeting an ordinary variable should be allowed: {res:?}"
        );
    }

    #[test]
    fn get_system_time_nested_in_assign_targeting_comp_rejected() {
        // The target-collecting walk must reach into every action's
        // expression, not just bare `Action::Eval` â€” here it's nested as
        // the RHS of an Assign.
        let doc = r#"
logic
    comp derived = 1 + 1
    timer 1s -> result = get_system_time(derived)
layout
    window
        "#;
        let res = check_flow_doc(doc);
        assert!(
            res.is_err(),
            "get_system_time nested in an Assign's RHS must still be caught"
        );
    }

    // â”€â”€ Diagnostics (F3) â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn diagnostic_includes_source_and_sink() {
        let doc = r#"
urls
    feed: mizu://api.example.com/feed
logic
    timer 1s -> GET(feed) -> next
    timer 2s -> navigate next
layout
    window
        "#;
        let res = check_flow_doc(doc);
        assert!(res.is_err());
        let msg = res.unwrap_err().to_string();
        // F3: error message should mention the tainted variable and its source
        assert!(msg.contains("next"), "diagnostic should name the tainted var: {msg}");
        assert!(
            msg.contains("GET") || msg.contains("feed") || msg.contains("navigate"),
            "diagnostic should mention the source or sink: {msg}"
        );
    }
}
