//! Expression/action pretty-printing: `binop_str`/`format_expr`/`format_action`.

use crate::core::types::Value;
use crate::parser::logic::{Action, BinOp, Expr, ExprArena};

fn binop_str(op: &BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Eq => "==",
        BinOp::Ne => "!=",
        BinOp::Lt => "<",
        BinOp::Gt => ">",
        BinOp::Le => "<=",
        BinOp::Ge => ">=",
        BinOp::And => "&&",
        BinOp::Or => "||",
    }
}

/// Renders an expression back to compact Mizu-like source.
///
/// Depth is naturally bounded: the parser rejects nesting beyond
/// `MAX_PARSE_DEPTH` (256), well within the native stack.
pub fn format_expr(
    e: &Expr,
    arena: &ExprArena,
    interner: &crate::core::types::FrozenInterner,
) -> String {
    match e {
        Expr::Literal(v) => match v {
            Value::String(s) => format!("\"{s}\""),
            other => format!("{other}"),
        },
        Expr::Variable(sym) => interner.resolve(*sym).unwrap_or("?").to_string(),
        Expr::BinaryOp { left, op, right } => format!(
            "{} {} {}",
            format_expr(&arena[*left], arena, interner),
            binop_str(op),
            format_expr(&arena[*right], arena, interner)
        ),
        Expr::FunctionCall {
            name,
            args_start,
            args_len,
        } => {
            let args: Vec<String> = arena
                .args(*args_start, *args_len)
                .iter()
                .map(|&a| format_expr(&arena[a], arena, interner))
                .collect();
            format!(
                "{}({})",
                interner.resolve(*name).unwrap_or("?"),
                args.join(", ")
            )
        }
        Expr::Let { name, value, body } => format!(
            "{} = {}; {}",
            interner.resolve(*name).unwrap_or("?"),
            format_expr(&arena[*value], arena, interner),
            format_expr(&arena[*body], arena, interner)
        ),
        Expr::Not(inner) => format!("!{}", format_expr(&arena[*inner], arena, interner)),
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => format!(
            "{} ? {} : {}",
            format_expr(&arena[*condition], arena, interner),
            format_expr(&arena[*then_expr], arena, interner),
            format_expr(&arena[*else_expr], arena, interner)
        ),
        Expr::FieldAccess {
            base,
            field,
            field_hash: _,
        } => {
            let field_name = interner.resolve(*field).unwrap_or("?");
            format!(
                "{}.{field_name}",
                format_expr(&arena[*base], arena, interner)
            )
        }
    }
}

/// Renders an action back to compact Mizu-like source.
pub fn format_action(a: &Action, interner: &crate::core::types::FrozenInterner) -> String {
    match a {
        Action::Assign { target, expr } => {
            format!(
                "{target} = {}",
                format_expr(expr.root(), &expr.arena, interner)
            )
        }
        Action::Eval(e) => format_expr(e.root(), &e.arena, interner),
        Action::Navigate { url } => {
            format!("navigate {}", format_expr(url.root(), &url.arena, interner))
        }
        Action::NetworkCall {
            method,
            alias_sym,
            target_var,
            ..
        } => format!(
            "{}({}) -> {}",
            method.as_str(),
            interner.resolve(*alias_sym).unwrap_or("?"),
            target_var
        ),
    }
}
