//! Taint-propagation helpers: `action_exprs` (every expression an `Action`
//! embeds), `collect_get_system_time_targets`, `is_expr_tainted`/
//! `find_tainted_var_in_expr` (the expression-level taint checkers), and
//! `build_taint_path` (diagnostic path formatting, F3).

use rustc_hash::{FxHashMap, FxHashSet};

use crate::core::types::Symbol;
use crate::parser::logic::{Action, Expr, ExprArena, ExprTree};

use super::types::TaintOrigin;

/// Every expression an `Action` directly embeds, in evaluation order.
/// Used to walk the whole action graph looking for `get_system_time` calls,
/// which — unlike `Navigate`/`NetworkCall`/`Assign` — are not a top-level
/// `Action` variant of their own and can be nested anywhere inside these.
pub(super) fn action_exprs(action: &Action) -> Vec<&ExprTree> {
    match action {
        Action::Eval(e) => vec![e],
        Action::Assign { expr, .. } => vec![expr],
        Action::Navigate { url } => vec![url],
        Action::NetworkCall {
            payload,
            path_param,
            headers,
            ..
        } => {
            let mut exprs = Vec::new();
            if let Some(p) = payload {
                exprs.push(p);
            }
            if let Some(p) = path_param {
                exprs.push(p);
            }
            for (_, value_expr) in headers {
                exprs.push(value_expr);
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
/// target — the whole point of this walk is to make that target visible to
/// the checker, exactly as an `Action::Assign`'s target already is.
pub(super) fn collect_get_system_time_targets(
    expr: &Expr,
    arena: &ExprArena,
    gst_sym: Symbol,
    out: &mut Vec<Symbol>,
) {
    match expr {
        Expr::Literal(_) | Expr::Variable(_) => {}
        Expr::BinaryOp { left, right, .. } => {
            collect_get_system_time_targets(&arena[*left], arena, gst_sym, out);
            collect_get_system_time_targets(&arena[*right], arena, gst_sym, out);
        }
        Expr::FunctionCall {
            name,
            args_start,
            args_len,
        } => {
            let args = arena.args(*args_start, *args_len);
            if *name == gst_sym
                && let [id] = args
                && let Expr::Variable(target) = &arena[*id]
            {
                out.push(*target);
            }
            for &arg in args {
                collect_get_system_time_targets(&arena[arg], arena, gst_sym, out);
            }
        }
        Expr::Let { value, body, .. } => {
            collect_get_system_time_targets(&arena[*value], arena, gst_sym, out);
            collect_get_system_time_targets(&arena[*body], arena, gst_sym, out);
        }
        Expr::Not(inner) => collect_get_system_time_targets(&arena[*inner], arena, gst_sym, out),
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            collect_get_system_time_targets(&arena[*condition], arena, gst_sym, out);
            collect_get_system_time_targets(&arena[*then_expr], arena, gst_sym, out);
            collect_get_system_time_targets(&arena[*else_expr], arena, gst_sym, out);
        }
        Expr::FieldAccess { base, .. } => {
            collect_get_system_time_targets(&arena[*base], arena, gst_sym, out)
        }
    }
}

/// Checks whether `expr` reads any tainted variable or calls a tainted function.
///
/// `gst_sym`, if `Some`, is the interned `get_system_time` symbol: a call to
/// it is skipped structurally (its argument names a write target, never
/// read as a value — see the comment on `gst_sym` in
/// `check_information_flow`) rather than walked like an ordinary argument.
pub(super) fn is_expr_tainted(
    expr: &Expr,
    arena: &ExprArena,
    tainted_vars: &FxHashSet<Symbol>,
    tainted_functions: &FxHashSet<Symbol>,
    gst_sym: Option<Symbol>,
) -> bool {
    match expr {
        Expr::Variable(sym) => tainted_vars.contains(sym),
        Expr::Literal(_) => false,
        Expr::BinaryOp { left, right, .. } => {
            is_expr_tainted(
                &arena[*left],
                arena,
                tainted_vars,
                tainted_functions,
                gst_sym,
            ) || is_expr_tainted(
                &arena[*right],
                arena,
                tainted_vars,
                tainted_functions,
                gst_sym,
            )
        }
        Expr::FunctionCall {
            name,
            args_start,
            args_len,
        } => {
            if Some(*name) == gst_sym {
                return false;
            }
            if tainted_functions.contains(name) {
                return true;
            }
            for &arg in arena.args(*args_start, *args_len) {
                if is_expr_tainted(&arena[arg], arena, tainted_vars, tainted_functions, gst_sym) {
                    return true;
                }
            }
            false
        }
        Expr::Let { value, body, .. } => {
            is_expr_tainted(
                &arena[*value],
                arena,
                tainted_vars,
                tainted_functions,
                gst_sym,
            ) || is_expr_tainted(
                &arena[*body],
                arena,
                tainted_vars,
                tainted_functions,
                gst_sym,
            )
        }
        Expr::Not(inner) => is_expr_tainted(
            &arena[*inner],
            arena,
            tainted_vars,
            tainted_functions,
            gst_sym,
        ),
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            is_expr_tainted(
                &arena[*condition],
                arena,
                tainted_vars,
                tainted_functions,
                gst_sym,
            ) || is_expr_tainted(
                &arena[*then_expr],
                arena,
                tainted_vars,
                tainted_functions,
                gst_sym,
            ) || is_expr_tainted(
                &arena[*else_expr],
                arena,
                tainted_vars,
                tainted_functions,
                gst_sym,
            )
        }
        Expr::FieldAccess { base, .. } => is_expr_tainted(
            &arena[*base],
            arena,
            tainted_vars,
            tainted_functions,
            gst_sym,
        ),
    }
}

/// Finds the first tainted variable symbol in an expression (for origin
/// tracking). `gst_sym`: see `is_expr_tainted`.
fn find_tainted_var_in_expr(
    expr: &Expr,
    arena: &ExprArena,
    tainted_vars: &FxHashSet<Symbol>,
    tainted_functions: &FxHashSet<Symbol>,
    gst_sym: Option<Symbol>,
) -> Option<Symbol> {
    match expr {
        Expr::Variable(sym) => {
            if tainted_vars.contains(sym) {
                Some(*sym)
            } else {
                None
            }
        }
        Expr::Literal(_) => None,
        Expr::BinaryOp { left, right, .. } => find_tainted_var_in_expr(
            &arena[*left],
            arena,
            tainted_vars,
            tainted_functions,
            gst_sym,
        )
        .or_else(|| {
            find_tainted_var_in_expr(
                &arena[*right],
                arena,
                tainted_vars,
                tainted_functions,
                gst_sym,
            )
        }),
        Expr::FunctionCall {
            name,
            args_start,
            args_len,
        } => {
            if Some(*name) == gst_sym {
                return None;
            }
            if tainted_functions.contains(name) {
                return Some(*name);
            }
            for &arg in arena.args(*args_start, *args_len) {
                if let Some(s) = find_tainted_var_in_expr(
                    &arena[arg],
                    arena,
                    tainted_vars,
                    tainted_functions,
                    gst_sym,
                ) {
                    return Some(s);
                }
            }
            None
        }
        Expr::Let { value, body, .. } => find_tainted_var_in_expr(
            &arena[*value],
            arena,
            tainted_vars,
            tainted_functions,
            gst_sym,
        )
        .or_else(|| {
            find_tainted_var_in_expr(
                &arena[*body],
                arena,
                tainted_vars,
                tainted_functions,
                gst_sym,
            )
        }),
        Expr::Not(inner) => find_tainted_var_in_expr(
            &arena[*inner],
            arena,
            tainted_vars,
            tainted_functions,
            gst_sym,
        ),
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => find_tainted_var_in_expr(
            &arena[*condition],
            arena,
            tainted_vars,
            tainted_functions,
            gst_sym,
        )
        .or_else(|| {
            find_tainted_var_in_expr(
                &arena[*then_expr],
                arena,
                tainted_vars,
                tainted_functions,
                gst_sym,
            )
        })
        .or_else(|| {
            find_tainted_var_in_expr(
                &arena[*else_expr],
                arena,
                tainted_vars,
                tainted_functions,
                gst_sym,
            )
        }),
        Expr::FieldAccess { base, .. } => find_tainted_var_in_expr(
            &arena[*base],
            arena,
            tainted_vars,
            tainted_functions,
            gst_sym,
        ),
    }
}

// No Kani harness for the taint-propagation core here (see
// `SECURITY-INVARIANTS.md` §8 for the rest of this crate's Kani coverage).
// `is_expr_tainted` recurses through `ExprArena`/`Expr::FunctionCall`'s
// internal `for &arg in arena.args(..)` loop; CBMC's symbolic execution
// explores that loop's shape across the whole function body regardless of
// which `Expr` variant a harness's concrete input actually selects, and
// this stayed pathologically slow (unresolved after 5+ minutes, even for
// a harness with a single concrete two-node tree and *zero* remaining
// `kani::any()` calls after multiple attempts to eliminate symbolic input
// as the cause — narrowing it down to something inherent to this
// recursion+loop shape under CBMC, not to how the harness was written).
// This is the same class of wall `parser::logic::eval::kani_proofs`
// documents for `Value`/`Expr` recursive-type verification (T4): "a real
// but modest down payment... not a substitute" for the larger Lean
// development in `formal/`, which already proves `check_information_flow`'s
// soundness (T2) structurally over these exact recursive types.

/// Builds a human-readable taint path for diagnostics (F3).
///
/// Example output: `"value 'next' (tainted from GET(api))"`
pub(super) fn build_taint_path(
    expr: &Expr,
    arena: &ExprArena,
    tainted_vars: &FxHashSet<Symbol>,
    tainted_functions: &FxHashSet<Symbol>,
    origins: &FxHashMap<Symbol, TaintOrigin>,
    interner: &crate::core::types::StringInterner,
    gst_sym: Option<Symbol>,
) -> String {
    if let Some(sym) =
        find_tainted_var_in_expr(expr, arena, tainted_vars, tainted_functions, gst_sym)
    {
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
