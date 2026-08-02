//! Function-definition grammar: `parse_function_block`, `parse_type`,
//! `parse_params`, and `parse_multiline_body`.

use crate::core::errors::MizuError;
use crate::core::types::{StringInterner, Symbol};

use super::super::ast::{Expr, ExprArena, ExprTree, MizuFunction, ValueType};
use super::super::lexer::{Cursor, Token, lex};
use super::expr::{parse_expr, parse_expr_tree};
use super::helpers::looks_like_binding;

pub(super) fn parse_function_block(
    lines: &[&str],
    interner: &mut StringInterner,
) -> Result<(String, MizuFunction), MizuError> {
    let header = lines[0];

    // If it looks like a variable binding (e.g. `count = 0`), parse it as a zero-argument function.
    if looks_like_binding(header) {
        let eq_pos = header.find('=').ok_or_else(|| {
            MizuError::ParseError(format!("expected `=` in variable definition: `{header}`"))
        })?;
        let name = header[..eq_pos].trim().to_owned();
        let body_src = header[eq_pos + 1..].trim();
        if name.is_empty() || body_src.is_empty() {
            return Err(MizuError::ParseError(format!(
                "invalid variable definition: `{header}`"
            )));
        }
        let tokens = lex(body_src)?;
        let mut cursor = Cursor::new(&tokens);
        let body_expr = parse_expr_tree(&mut cursor, interner)?;
        return Ok((
            name,
            MizuFunction {
                params: Vec::new(),
                body: body_expr,
            },
        ));
    }

    // ── Parse the function header: `name(p: type, ...) : expr` ──────────
    // Split on `(` to get the name.
    let paren_pos = header.find('(').ok_or_else(|| {
        MizuError::ParseError(format!(
            "expected `(` in function definition header: `{header}`"
        ))
    })?;
    let func_name = header[..paren_pos].trim().to_owned();
    if func_name.is_empty() {
        return Err(MizuError::ParseError(
            "function name must not be empty".to_owned(),
        ));
    }

    // Everything between `(` and `)` is the parameter list.
    let after_paren = &header[paren_pos + 1..];
    let close_paren_pos = after_paren.find(')').ok_or_else(|| {
        MizuError::ParseError(format!(
            "expected `)` in function definition header: `{header}`"
        ))
    })?;
    let param_str = &after_paren[..close_paren_pos];
    let rest_after_paren = &after_paren[close_paren_pos + 1..].trim();

    // Parse parameter list.
    let params = parse_params(param_str, header, interner)?;

    // ── Determine body source ────────────────────────────────────────────
    // Two forms:
    //   1. Inline:    `func(x: num) : expr`        → rest_after_paren starts with `:`
    //   2. Multi-line: `func(x: num)\n    line1\n  last` → subsequent indented lines
    let body_expr: ExprTree;

    if let Some(colon_body) = rest_after_paren.strip_prefix(':') {
        // ── Form 1: inline ───────────────────────────────────────────────
        let body_source = colon_body.trim();
        if body_source.is_empty() {
            return Err(MizuError::ParseError(format!(
                "inline function `{func_name}` has `:` but no body expression"
            )));
        }
        let tokens = lex(body_source)?;
        let mut cursor = Cursor::new(&tokens);
        body_expr = parse_expr_tree(&mut cursor, interner)?;
    } else if lines.len() > 1 {
        // ── Form 2: multi-line ───────────────────────────────────────────
        // The body lines are lines[1..], each indented by some amount.
        // We build a chain of `Let` bindings ending with the last expression.
        body_expr = parse_multiline_body(&lines[1..], &func_name, interner)?;
    } else {
        return Err(MizuError::ParseError(format!(
            "function `{func_name}` has no body (no `:` and no indented block)"
        )));
    }

    Ok((
        func_name,
        MizuFunction {
            params,
            body: body_expr,
        },
    ))
}

/// Parses the parameter declaration string `p1: type1, p2: type2, …`.
///
/// The `: type` annotation is optional — `f(x)` is equivalent to `f(x: any)`.
/// Supported types: `num`, `string`/`str`, `bool`, `list`.
/// Writing `dict`, `record`, or `any` produces a `ParseError` (use an
/// unannotated parameter instead).
fn parse_type(
    cursor: &mut Cursor<'_>,
    _interner: &mut StringInterner,
) -> Result<ValueType, String> {
    let mut base_type = match cursor.next() {
        Some(Token::Ident(name)) => match name.to_lowercase().as_str() {
            "num" | "number" => ValueType::Num,
            "string" | "str" => ValueType::Str,
            "bool" | "boolean" => ValueType::Bool,
            "list" => {
                match cursor.next() {
                    Some(Token::Lt) => {}
                    other => return Err(format!("expected `<` after `list`, got {other:?}")),
                }
                let inner = parse_type(cursor, _interner)?;
                match cursor.next() {
                    Some(Token::Gt) => {}
                    other => return Err(format!("expected `>` after list type, got {other:?}")),
                }
                ValueType::List(Box::new(inner))
            }
            "record" => {
                match cursor.next() {
                    Some(Token::LBrace) => {}
                    other => return Err(format!("expected `{{` after `record`, got {other:?}")),
                }
                let mut fields = Vec::new();
                while !matches!(cursor.peek(), Some(Token::RBrace) | None) {
                    let field_name: std::sync::Arc<str> = match cursor.next() {
                        Some(Token::Ident(n)) => std::sync::Arc::from(*n),
                        other => return Err(format!("expected field name, got {other:?}")),
                    };
                    match cursor.next() {
                        Some(Token::Colon) => {}
                        other => {
                            return Err(format!("expected `:` after field name, got {other:?}"));
                        }
                    }
                    let field_type = parse_type(cursor, _interner)?;
                    fields.push((field_name, field_type));
                    if matches!(cursor.peek(), Some(Token::Comma)) {
                        cursor.next();
                    }
                }
                match cursor.next() {
                    Some(Token::RBrace) => {}
                    other => {
                        return Err(format!("expected `}}` after record fields, got {other:?}"));
                    }
                }
                fields.sort_by(|a, b| a.0.cmp(&b.0));
                ValueType::Record(fields)
            }
            "dict" | "any" => {
                return Err(format!(
                    "type `{}` is not supported; use: num, string, bool, list<T>, record{{...}}, or T?",
                    name
                ));
            }
            other => {
                return Err(format!(
                    "unknown type `{other}`; valid types: num, string, bool, list<T>, record{{...}}, or T?"
                ));
            }
        },
        other => return Err(format!("expected type name, got {other:?}")),
    };
    if matches!(cursor.peek(), Some(Token::Question)) {
        cursor.next();
        base_type = ValueType::Nullable(Box::new(base_type));
    }
    Ok(base_type)
}

fn parse_params(
    param_str: &str,
    _context: &str,
    interner: &mut StringInterner,
) -> Result<Vec<(Symbol, ValueType)>, MizuError> {
    let mut params = Vec::new();
    if param_str.trim().is_empty() {
        return Ok(params);
    }
    let tokens = lex(param_str)?;
    let mut cursor = Cursor::new(&tokens);

    while !matches!(cursor.peek(), None | Some(Token::Newline)) {
        let name = match cursor.next() {
            Some(Token::Ident(n)) => *n,
            other => {
                return Err(MizuError::ParseError(format!(
                    "expected parameter name, got {other:?}"
                )));
            }
        };
        match cursor.next() {
            Some(Token::Colon) => {}
            _other => {
                let fn_name = _context.split('(').next().unwrap_or(_context).trim();
                return Err(MizuError::ParseError(format!(
                    "function `{}`: parameter `{}` requires a type annotation",
                    fn_name, name
                )));
            }
        }
        let vtype = parse_type(&mut cursor, interner).map_err(MizuError::ParseError)?;
        params.push((interner.get_or_intern(name), vtype));

        if matches!(cursor.peek(), Some(Token::Comma)) {
            cursor.next();
        }
    }
    Ok(params)
}

/// Parses a multi-line function body from a slice of body lines (already
/// stripped of the function header).
///
/// Each line may be:
/// * `name = expr`  — a local binding (synthesised as `Expr::Let`).
/// * `expr`         — the implicit return value (must be the last line).
fn parse_multiline_body(
    body_lines: &[&str],
    func_name: &str,
    interner: &mut StringInterner,
) -> Result<ExprTree, MizuError> {
    if body_lines.is_empty() {
        return Err(MizuError::ParseError(format!(
            "multi-line function `{func_name}` has an empty body"
        )));
    }

    // Collect non-empty, trimmed body lines.
    let lines: Vec<&str> = body_lines
        .iter()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();

    if lines.is_empty() {
        return Err(MizuError::ParseError(format!(
            "multi-line function `{func_name}` has only blank body lines"
        )));
    }

    // Process bindings in reverse (innermost first) so they can be nested.
    // The last line is the return expression; preceding lines are `name = expr`.
    let return_line = *lines.last().ok_or_else(|| {
        MizuError::ParseError(format!(
            "multi-line function `{func_name}` has no return line"
        ))
    })?;

    // Check if the return line itself is a binding.  If so, it's an error:
    // the last line must be a bare expression.
    if looks_like_binding(return_line) {
        return Err(MizuError::ParseError(format!(
            "the last line of multi-line function `{func_name}` must be a bare expression, \
             not an assignment"
        )));
    }

    // Parse the return expression. Every line's expression shares this one
    // arena, since the whole Let-chain is a single self-contained tree.
    let mut arena = ExprArena::new();
    let tokens = lex(return_line)?;
    let mut cursor = Cursor::new(&tokens);
    let mut result_expr = parse_expr(&mut cursor, 0, 0, interner, &mut arena)?;

    // Wrap in Let-bindings from bottom to top (right-to-left over prefix lines).
    for &binding_line in lines[..lines.len() - 1].iter().rev() {
        if !looks_like_binding(binding_line) {
            return Err(MizuError::ParseError(format!(
                "non-final body line `{binding_line}` in function `{func_name}` \
                 must be an assignment (e.g., `result = a * b`)"
            )));
        }
        let eq_pos = binding_line.find('=').ok_or_else(|| {
            MizuError::ParseError(format!(
                "expected `=` in binding `{binding_line}` of function `{func_name}`"
            ))
        })?;
        let bind_name = binding_line[..eq_pos].trim();
        let bind_expr_src = binding_line[eq_pos + 1..].trim();
        let bind_tokens = lex(bind_expr_src)?;
        let mut bind_cursor = Cursor::new(&bind_tokens);
        let bind_expr = parse_expr(&mut bind_cursor, 0, 0, interner, &mut arena)?;
        let bind_sym = interner.get_or_intern(bind_name);
        let value = arena.alloc(bind_expr);
        let body = arena.alloc(result_expr);
        result_expr = Expr::Let {
            name: bind_sym,
            value,
            body,
        };
    }

    let root = arena.alloc(result_expr);
    Ok(ExprTree { arena, root })
}
