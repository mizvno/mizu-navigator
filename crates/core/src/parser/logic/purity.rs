//! The P1 purity / effectful-call checker.

use rustc_hash::FxHashMap;

use crate::core::types::{StringInterner, Symbol};

use super::ast::{Expr, ExprArena, MizuFunction};

/// Built-in names that are pure (no observable side effect) and therefore
/// permitted inside a pure-context expression (e.g. a class condition,
/// `class X if <expr>`).
///
/// This is an **allowlist, not a denylist**, by design (P1,
/// `SECURITY-INVARIANTS.md`): any [`Expr::FunctionCall`] name that resolves
/// to neither a user-defined function nor a name in this list is
/// conservatively rejected as effectful. Adding a new effectful builtin to
/// the evaluator's dispatch (`core::types::eval`) requires no action here —
/// it is rejected by default. Adding a new *pure* builtin requires adding
/// its name here, or it will be incorrectly rejected as effectful
/// (fail-secure in the wrong direction, but loud — a parse error — not
/// silent).
const KNOWN_PURE_BUILTINS: &[&str] =
    &["filter", "count", "sort", "length", "to_string", "contains", "has_field"];

/// Walks `expr` and returns the name of the first side-effecting function
/// call found, or `None` if the expression is pure.
///
/// `functions` is the document's user-defined-function table: a call to any
/// name in it is a call to a Mizu function, which is pure by construction
/// (the language only allows a plain expression as a function body — see
/// `docs/reference/grammar.md`'s `function_def` — so nothing effectful can
/// be reached through a direct call). A name that is in neither `functions`
/// nor [`KNOWN_PURE_BUILTINS`] is unknown and rejected fail-secure.
pub fn find_side_effect_call(
    expr: &Expr,
    arena: &ExprArena,
    interner: &StringInterner,
    functions: &FxHashMap<Symbol, MizuFunction>,
) -> Option<String> {
    match expr {
        Expr::Literal(_) | Expr::Variable(_) => None,
        Expr::BinaryOp { left, right, .. } => find_side_effect_call(&arena[*left], arena, interner, functions)
            .or_else(|| find_side_effect_call(&arena[*right], arena, interner, functions)),
        Expr::FunctionCall { name, args_start, args_len } => {
            if !functions.contains_key(name)
                && let Some(n) = interner.resolve(*name)
                && !KNOWN_PURE_BUILTINS.contains(&n)
            {
                return Some(n.to_string());
            }
            for &arg in arena.args(*args_start, *args_len) {
                if let Some(n) = find_side_effect_call(&arena[arg], arena, interner, functions) {
                    return Some(n);
                }
            }
            None
        }
        Expr::Let { value, body, .. } => find_side_effect_call(&arena[*value], arena, interner, functions)
            .or_else(|| find_side_effect_call(&arena[*body], arena, interner, functions)),
        Expr::Not(inner) => find_side_effect_call(&arena[*inner], arena, interner, functions),
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => find_side_effect_call(&arena[*condition], arena, interner, functions)
            .or_else(|| find_side_effect_call(&arena[*then_expr], arena, interner, functions))
            .or_else(|| find_side_effect_call(&arena[*else_expr], arena, interner, functions)),
        Expr::FieldAccess { base, .. } => find_side_effect_call(&arena[*base], arena, interner, functions),
    }
}
