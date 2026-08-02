//! The Pratt expression parser: `parse_expr`/`parse_expr_tree`, plus the
//! `sort`/`filter` call-argument parsers whose third/fourth argument is a
//! hard keyword rather than a normal sub-expression.

use std::sync::Arc;

use crate::core::errors::MizuError;
use crate::core::types::{StringInterner, Value};

use super::super::ast::{BinOp, Expr, ExprArena, ExprTree};
use super::super::lexer::{Cursor, Token};

/// Parses an expression from the cursor using Pratt (top-down operator
/// precedence) parsing.
/// Maximum nesting depth for `parse_expr` recursive descent.
///
/// Prevents stack overflow on pathological input (e.g. 300 nested parentheses).
/// No legitimate Mizu expression comes close to this limit. Deliberately
/// independent from [`crate::core::types::MAX_EVAL_DEPTH`] (which stays
/// fixed, see `SECURITY-INVARIANTS.md`'s E1) — that's the actual runtime
/// enforcement; this is only a parse-time convenience check, so raising it
/// via override does not weaken the eval-depth guard.
///
/// An unmeasured starting value, overridable for a single run via
/// `MIZU_MAX_PARSE_DEPTH` (see the module doc on [`crate::core::config`]).
static MAX_PARSE_DEPTH: std::sync::LazyLock<u32> =
    std::sync::LazyLock::new(|| crate::core::config::CONFIG.max_parse_depth as u32);

/// Operator keywords recognised in `filter`'s optional 4-argument form:
/// `filter(list, field, op, value)`.
const FILTER_OPS: &[&str] = &["eq", "ne", "lt", "le", "gt", "ge", "contains"];

/// Parses `sort(list_expr, field_expr, direction_keyword)`'s argument list,
/// up to (not including) the closing `)`.
///
/// The third argument is a hard parse-time keyword (`asc`/`desc`), consumed
/// directly here rather than through [`parse_expr`] — it never becomes an
/// `Expr::Variable`, so a same-named variable elsewhere in scope can never
/// shadow it. This closes a real bug in the previous implementation: it
/// evaluated the third argument as a normal expression *except* when its
/// resolved variable *name* happened to be literally `asc`/`desc`, in which
/// case it silently substituted the keyword interpretation instead of ever
/// evaluating that variable — so a document with a real variable named
/// `asc` in scope would have its actual value silently discarded if passed
/// here.
fn parse_sort_call_args(
    cursor: &mut Cursor<'_>,
    depth: u32,
    interner: &mut StringInterner,
    arena: &mut ExprArena,
) -> Result<Vec<Expr>, MizuError> {
    let list = parse_expr(cursor, 0, depth + 1, interner, arena)?;
    if matches!(cursor.peek(), Some(Token::Comma)) {
        cursor.next();
    }
    let field = parse_expr(cursor, 0, depth + 1, interner, arena)?;
    if matches!(cursor.peek(), Some(Token::Comma)) {
        cursor.next();
    }
    let direction_tok = cursor.next();
    let direction_name = match direction_tok {
        Some(Token::Ident(kw)) => Some(*kw),
        _ => None,
    };
    let direction = match direction_name {
        Some("asc") => Expr::Literal(Value::String(Arc::from("asc"))),
        Some("desc") => Expr::Literal(Value::String(Arc::from("desc"))),
        _ => {
            return Err(MizuError::ParseError(format!(
                "sort's third argument must be the bare keyword `asc` or `desc` \
                 (not a variable or expression), got: {direction_tok:?}"
            )));
        }
    };
    Ok(vec![list, field, direction])
}

/// Parses `filter`'s argument list, up to (not including) the closing `)`.
///
/// Two surface forms:
/// - `filter(list, field, value)` — sugar for `op = eq`.
/// - `filter(list, field, op, value)` — `op` is one of `eq|ne|lt|le|gt|ge|
///   contains`, a hard parse-time keyword (mirroring `sort`'s direction
///   argument: never `Expr::Variable`, never falls through to a variable
///   lookup).
///
/// Always returns exactly 4 elements (`[list, field, op, value]`) — the
/// 3-argument surface form is desugared here, at parse time, so
/// `core::types::eval` and `typecheck::infer` only ever need to handle one
/// arity for `filter`.
///
/// The two forms are disambiguated *positionally*, not just by name: a
/// bare identifier in the third argument slot is only treated as the `op`
/// keyword when it is immediately followed by a comma (i.e. a fourth
/// argument follows). A genuine 3-argument call whose `value` happens to be
/// a variable literally named e.g. `eq` still parses as that variable —
/// this is what keeps the new form from reintroducing the exact class of
/// name-shadowing bug `sort`'s direction argument had.
fn parse_filter_call_args(
    cursor: &mut Cursor<'_>,
    depth: u32,
    interner: &mut StringInterner,
    arena: &mut ExprArena,
) -> Result<Vec<Expr>, MizuError> {
    let list = parse_expr(cursor, 0, depth + 1, interner, arena)?;
    if matches!(cursor.peek(), Some(Token::Comma)) {
        cursor.next();
    }
    let field = parse_expr(cursor, 0, depth + 1, interner, arena)?;
    if matches!(cursor.peek(), Some(Token::Comma)) {
        cursor.next();
    }

    let is_op_keyword = matches!(
        (cursor.peek(), cursor.peek_at(1)),
        (Some(Token::Ident(w)), Some(Token::Comma)) if FILTER_OPS.contains(w)
    );

    if is_op_keyword {
        let op = match cursor.next() {
            Some(Token::Ident(w)) => *w,
            _ => unreachable!("just confirmed by is_op_keyword"),
        };
        cursor.next(); // consume the comma `is_op_keyword` already confirmed is there
        let value = parse_expr(cursor, 0, depth + 1, interner, arena)?;
        Ok(vec![
            list,
            field,
            Expr::Literal(Value::String(Arc::from(op))),
            value,
        ])
    } else {
        let value = parse_expr(cursor, 0, depth + 1, interner, arena)?;
        Ok(vec![
            list,
            field,
            Expr::Literal(Value::String(Arc::from("eq"))),
            value,
        ])
    }
}

///
/// `min_bp` is the minimum binding power the caller is willing to absorb —
/// pass `0` to parse a full expression.
/// `depth` tracks the current recursion depth; external callers must pass `0`.
/// `interner` is used to intern all identifier names at parse time.
/// `arena` accumulates every child node this call (and its recursive
/// descendants) allocates; the returned `Expr` is *not itself* allocated
/// into `arena` — callers that need an `ExprId`/`ExprTree` for the returned
/// root must `arena.alloc(...)` it themselves (see [`parse_expr_tree`]).
pub(super) fn parse_expr(
    cursor: &mut Cursor<'_>,
    min_bp: u8,
    depth: u32,
    interner: &mut StringInterner,
    arena: &mut ExprArena,
) -> Result<Expr, MizuError> {
    if depth > *MAX_PARSE_DEPTH {
        return Err(MizuError::ParseError(format!(
            "expression nesting too deep (max {} levels)",
            *MAX_PARSE_DEPTH
        )));
    }
    // ── Null denotation (prefix / atoms) ────────────────────────────────
    let mut lhs = match cursor.next() {
        Some(Token::Num(num_str)) => {
            if num_str.contains('.') || num_str.contains('e') || num_str.contains('E') {
                let parsed: f64 = num_str
                    .parse()
                    .map_err(|_| MizuError::ParseError("Invalid float literal".into()))?;
                let scaled = (parsed * (crate::core::types::DECIMAL_SCALE as f64)).round() as i64;
                Expr::Literal(Value::Decimal(scaled))
            } else {
                let parsed: i64 = num_str
                    .parse()
                    .map_err(|_| MizuError::ParseError("Invalid int literal".into()))?;
                Expr::Literal(Value::Int(parsed))
            }
        }
        Some(Token::Bool(b)) => Expr::Literal(Value::Bool(*b)),
        Some(Token::Str(s)) => Expr::Literal(Value::String(std::sync::Arc::from(*s))),

        Some(Token::Ident(name)) => {
            let name = *name;

            // ── `if <cond> then <then> else <else>` ─────────────────────────
            if name == "if" {
                let condition = parse_expr(cursor, 0, depth + 1, interner, arena)?;
                let condition = arena.alloc(condition);
                match cursor.next() {
                    Some(Token::Ident(kw)) if *kw == "then" => {}
                    other => {
                        return Err(MizuError::ParseError(format!(
                            "expected `then` after `if` condition, got: {other:?}"
                        )));
                    }
                }
                let then_expr = parse_expr(cursor, 0, depth + 1, interner, arena)?;
                let then_expr = arena.alloc(then_expr);
                match cursor.next() {
                    Some(Token::Ident(kw)) if *kw == "else" => {}
                    other => {
                        return Err(MizuError::ParseError(format!(
                            "expected `else` branch in `if` expression, got: {other:?}"
                        )));
                    }
                }
                let else_expr = parse_expr(cursor, 0, depth + 1, interner, arena)?;
                let else_expr = arena.alloc(else_expr);
                return Ok(Expr::IfElse {
                    condition,
                    then_expr,
                    else_expr,
                });
            }

            // Look ahead: if `(` follows, this is a function call.
            if matches!(cursor.peek(), Some(Token::LParen)) {
                cursor.next(); // consume `(`
                let args = if name == "sort" {
                    parse_sort_call_args(cursor, depth, interner, arena)?
                } else if name == "filter" {
                    parse_filter_call_args(cursor, depth, interner, arena)?
                } else {
                    let mut args = Vec::new();
                    // Parse comma-separated argument list.
                    while !matches!(cursor.peek(), Some(Token::RParen) | None) {
                        let arg = parse_expr(cursor, 0, depth + 1, interner, arena)?;
                        args.push(arg);
                        if matches!(cursor.peek(), Some(Token::Comma)) {
                            cursor.next();
                        }
                    }
                    args
                };
                // Consume `)`.
                match cursor.next() {
                    Some(Token::RParen) => {}
                    _ => {
                        return Err(MizuError::ParseError(format!(
                            "expected `)` after arguments of call to `{name}`"
                        )));
                    }
                }
                // `get_system_time`'s argument selects which global variable is
                // overwritten with the current time — it must be a single bare
                // identifier, fixed at parse time, never a computed expression.
                // Without this restriction the target could be derived (even
                // indirectly) from untrusted data (`$form`, a network response),
                // making the write's destination invisible to the static flow
                // checker (`parser::flow`), which assumes every assignment
                // target is a known Symbol. See `SECURITY-INVARIANTS.md`.
                if name == "get_system_time" && !matches!(args.as_slice(), [Expr::Variable(_)]) {
                    return Err(MizuError::ParseError(
                        "get_system_time expects a single bare variable identifier, \
                         e.g. get_system_time(my_var) — not a computed expression"
                            .to_string(),
                    ));
                }
                let arg_ids: Vec<super::super::ast::ExprId> =
                    args.into_iter().map(|a| arena.alloc(a)).collect();
                let (args_start, args_len) = arena.push_args(&arg_ids)?;
                Expr::FunctionCall {
                    name: interner.get_or_intern(name),
                    args_start,
                    args_len,
                }
            } else {
                Expr::Variable(interner.get_or_intern(name))
            }
        }

        Some(Token::Minus) => {
            let operand = parse_expr(cursor, 30, depth + 1, interner, arena)?; // highest precedence for unary
            match operand {
                Expr::Literal(Value::Int(n)) => Expr::Literal(Value::Int(-n)),
                Expr::Literal(Value::Decimal(n)) => Expr::Literal(Value::Decimal(-n)),
                _ => {
                    // Fold into a binary `0 - operand` to keep the AST simple.
                    let left = arena.alloc(Expr::Literal(Value::Int(0)));
                    let right = arena.alloc(operand);
                    Expr::BinaryOp {
                        left,
                        op: BinOp::Sub,
                        right,
                    }
                }
            }
        }

        // Logical NOT: `!expr`
        Some(Token::Bang) => {
            let operand = parse_expr(cursor, 30, depth + 1, interner, arena)?; // highest unary precedence
            let operand = arena.alloc(operand);
            Expr::Not(operand)
        }

        Some(Token::LParen) => {
            let inner = parse_expr(cursor, 0, depth + 1, interner, arena)?;
            match cursor.next() {
                Some(Token::RParen) => inner,
                _ => {
                    return Err(MizuError::ParseError(
                        "expected `)` to close parenthesised expression".to_owned(),
                    ));
                }
            }
        }

        other => {
            return Err(MizuError::ParseError(format!(
                "unexpected token in expression: {other:?}"
            )));
        }
    };

    // ── Left denotation (infix operators) ───────────────────────────────
    loop {
        // ── Dot-access: `base.field` — highest precedence (50) ──────────
        if matches!(cursor.peek(), Some(Token::Dot)) {
            if 50 < min_bp {
                break;
            }
            cursor.next(); // consume `.`
            let field = match cursor.next() {
                Some(Token::Ident(name)) => interner.get_or_intern(name),
                other => {
                    return Err(MizuError::ParseError(format!(
                        "expected field name after `.`, got: {other:?}"
                    )));
                }
            };
            let base = arena.alloc(lhs);
            let field_hash = crate::core::types::hash_field(interner.resolve(field).unwrap_or(""));
            lhs = Expr::FieldAccess {
                base,
                field,
                field_hash,
            };
            continue;
        }

        // ── Ternary: `<cond> ? <then> : <else>` ─────────────────────────
        // Binding power 0 — lowest possible, right-associative.
        if matches!(cursor.peek(), Some(Token::Question)) {
            if 0 < min_bp {
                break;
            }
            cursor.next(); // consume `?`
            let then_expr = parse_expr(cursor, 0, depth + 1, interner, arena)?;
            match cursor.next() {
                Some(Token::Colon) => {}
                other => {
                    return Err(MizuError::ParseError(format!(
                        "expected `:` after `?` in ternary expression, got: {other:?}"
                    )));
                }
            }
            let else_expr = parse_expr(cursor, 0, depth + 1, interner, arena)?; // right-assoc: min_bp = 0
            let condition = arena.alloc(lhs);
            let then_expr = arena.alloc(then_expr);
            let else_expr = arena.alloc(else_expr);
            lhs = Expr::IfElse {
                condition,
                then_expr,
                else_expr,
            };
            continue;
        }

        let op = match cursor.peek() {
            Some(Token::Plus) => BinOp::Add,
            Some(Token::Minus) => BinOp::Sub,
            Some(Token::Star) => BinOp::Mul,
            Some(Token::Slash) => BinOp::Div,
            Some(Token::EqEq) => BinOp::Eq,
            Some(Token::BangEq) => BinOp::Ne,
            Some(Token::Lt) => BinOp::Lt,
            Some(Token::Gt) => BinOp::Gt,
            Some(Token::LtEq) => BinOp::Le,
            Some(Token::GtEq) => BinOp::Ge,
            Some(Token::AndAnd) => BinOp::And,
            Some(Token::OrOr) => BinOp::Or,
            _ => break,
        };

        let (left_bp, right_bp) = infix_binding_power(&op);
        if left_bp < min_bp {
            break;
        }

        cursor.next(); // consume the operator
        let rhs = parse_expr(cursor, right_bp, depth + 1, interner, arena)?;
        let left = arena.alloc(lhs);
        let right = arena.alloc(rhs);
        lhs = Expr::BinaryOp { left, op, right };
    }

    Ok(lhs)
}

/// Parses a complete expression and wraps it (with everything it
/// transitively allocated) into a self-contained [`ExprTree`].
///
/// This is the entry point every caller outside this module's own recursive
/// descent should use — [`parse_expr`] alone returns an unanchored root
/// `Expr` plus whatever it allocated into the caller-supplied `arena`, but
/// does not allocate the root itself; this does that last step.
pub(crate) fn parse_expr_tree(
    cursor: &mut Cursor<'_>,
    interner: &mut StringInterner,
) -> Result<ExprTree, MizuError> {
    let mut arena = ExprArena::new();
    let root_expr = parse_expr(cursor, 0, 0, interner, &mut arena)?;
    let root = arena.alloc(root_expr);
    Ok(ExprTree { arena, root })
}

/// Returns the `(left, right)` binding powers for a binary operator.
///
/// Left-associativity is achieved by making right BP = left BP + 1.
/// Precedence hierarchy (lowest to highest), mirroring C conventions:
///
/// | Operators              | BP     |
/// |------------------------|--------|
/// | `\|\|`                 | (1, 2) |
/// | `&&`                   | (3, 4) |
/// | `==`, `!=`             | (5, 6) |
/// | `<`, `>`, `<=`, `>=`   | (7, 8) |
/// | `+`, `-`               | (10, 11) |
/// | `*`, `/`               | (20, 21) |
const fn infix_binding_power(op: &BinOp) -> (u8, u8) {
    match op {
        BinOp::Or => (1, 2),
        BinOp::And => (3, 4),
        BinOp::Eq | BinOp::Ne => (5, 6),
        BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge => (7, 8),
        BinOp::Add | BinOp::Sub => (10, 11),
        BinOp::Mul | BinOp::Div => (20, 21),
    }
}
