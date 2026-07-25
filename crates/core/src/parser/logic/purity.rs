//! The P1 purity / effectful-call checker.

use crate::core::types::StringInterner;

use super::ast::{Expr, ExprArena};

/// Built-in names that produce observable side effects when called.
///
/// An [`Expr`] containing a [`Expr::FunctionCall`] whose resolved name
/// is in this list is rejected as a conditional class condition.
const SIDE_EFFECT_BUILTINS: &[&str] = &[
    "GET",
    "POST",
    "PUT",
    "DELETE",
    "QUERY",
    "copy_to_clipboard",
    "store_local",
    "navigate",
    "download",
    "get_system_time",
];

/// Walks `expr` and returns the name of the first side-effecting function
/// call found, or `None` if the expression is pure.
pub fn find_side_effect_call(expr: &Expr, arena: &ExprArena, interner: &StringInterner) -> Option<String> {
    match expr {
        Expr::Literal(_) | Expr::Variable(_) => None,
        Expr::BinaryOp { left, right, .. } => {
            find_side_effect_call(&arena[*left], arena, interner)
                .or_else(|| find_side_effect_call(&arena[*right], arena, interner))
        }
        Expr::FunctionCall { name, args } => {
            if let Some(n) = interner.resolve(*name)
                && SIDE_EFFECT_BUILTINS.contains(&n)
            {
                return Some(n.to_string());
            }
            for &arg in args {
                if let Some(n) = find_side_effect_call(&arena[arg], arena, interner) {
                    return Some(n);
                }
            }
            None
        }
        Expr::Let { value, body, .. } => {
            find_side_effect_call(&arena[*value], arena, interner)
                .or_else(|| find_side_effect_call(&arena[*body], arena, interner))
        }
        Expr::Not(inner) => find_side_effect_call(&arena[*inner], arena, interner),
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => find_side_effect_call(&arena[*condition], arena, interner)
            .or_else(|| find_side_effect_call(&arena[*then_expr], arena, interner))
            .or_else(|| find_side_effect_call(&arena[*else_expr], arena, interner)),
        Expr::FieldAccess { base, .. } => find_side_effect_call(&arena[*base], arena, interner),
    }
}
