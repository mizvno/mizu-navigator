//! The Pratt expression parser, block/action/timer grammar, and the
//! anti-recursion DAG check.

use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::VecDeque;
use std::sync::Arc;

use crate::core::errors::MizuError;
use crate::core::types::{StringInterner, Symbol, Value};
use crate::parser::urls::{EndpointKind, UrlRegistry};

use super::ast::{Action, BinOp, Expr, MizuFunction, NetworkMethod, RootTimer, TimerInterval, ValueType};
use super::comp::collect_calls;
use super::lexer::{Cursor, Token, assert_cursor_empty, leading_spaces, lex};

/// Parses an expression from the cursor using Pratt (top-down operator
/// precedence) parsing.
/// Maximum nesting depth for `parse_expr` recursive descent.
///
/// Prevents stack overflow on pathological input (e.g. 300 nested parentheses).
/// No legitimate Mizu expression comes close to this limit.
const MAX_PARSE_DEPTH: u32 = 256;

///
/// `min_bp` is the minimum binding power the caller is willing to absorb —
/// pass `0` to parse a full expression.
/// `depth` tracks the current recursion depth; external callers must pass `0`.
/// `interner` is used to intern all identifier names at parse time.
pub(super) fn parse_expr(
    cursor: &mut Cursor<'_>,
    min_bp: u8,
    depth: u32,
    interner: &mut StringInterner,
) -> Result<Expr, MizuError> {
    if depth > MAX_PARSE_DEPTH {
        return Err(MizuError::ParseError(
            "expression nesting too deep (max 256 levels)".to_owned(),
        ));
    }
    // ── Null denotation (prefix / atoms) ────────────────────────────────
    let mut lhs = match cursor.next() {
        Some(Token::Num(n)) => {
            let scaled = (*n * (crate::core::types::DECIMAL_SCALE as f64)).round() as i64;
            Expr::Literal(Value::Int(scaled))
        }
        Some(Token::Bool(b)) => Expr::Literal(Value::Bool(*b)),
        Some(Token::Str(s)) => Expr::Literal(Value::String(std::sync::Arc::from(s.as_str()))),

        Some(Token::Ident(name)) => {
            let name = name.clone();

            // ── `if <cond> then <then> else <else>` ─────────────────────────
            if name == "if" {
                let condition = parse_expr(cursor, 0, depth + 1, interner)?;
                match cursor.next() {
                    Some(Token::Ident(kw)) if kw == "then" => {}
                    other => {
                        return Err(MizuError::ParseError(format!(
                            "expected `then` after `if` condition, got: {other:?}"
                        )));
                    }
                }
                let then_expr = parse_expr(cursor, 0, depth + 1, interner)?;
                match cursor.next() {
                    Some(Token::Ident(kw)) if kw == "else" => {}
                    other => {
                        return Err(MizuError::ParseError(format!(
                            "expected `else` branch in `if` expression, got: {other:?}"
                        )));
                    }
                }
                let else_expr = parse_expr(cursor, 0, depth + 1, interner)?;
                return Ok(Expr::IfElse {
                    condition: Box::new(condition),
                    then_expr: Box::new(then_expr),
                    else_expr: Box::new(else_expr),
                });
            }

            // Look ahead: if `(` follows, this is a function call.
            if matches!(cursor.peek(), Some(Token::LParen)) {
                cursor.next(); // consume `(`
                let mut args = Vec::new();
                // Parse comma-separated argument list.
                while !matches!(cursor.peek(), Some(Token::RParen) | None) {
                    args.push(parse_expr(cursor, 0, depth + 1, interner)?);
                    if matches!(cursor.peek(), Some(Token::Comma)) {
                        cursor.next();
                    }
                }
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
                Expr::FunctionCall {
                    name: interner.get_or_intern(&name),
                    args,
                }
            } else {
                Expr::Variable(interner.get_or_intern(&name))
            }
        }

        // Unary minus: `-expr`
        Some(Token::Minus) => {
            let operand = parse_expr(cursor, 30, depth + 1, interner)?; // highest precedence for unary
            // Fold into a binary `0 - operand` to keep the AST simple.
            Expr::BinaryOp {
                left: Box::new(Expr::Literal(Value::Int(0))),
                op: BinOp::Sub,
                right: Box::new(operand),
            }
        }

        // Logical NOT: `!expr`
        Some(Token::Bang) => {
            let operand = parse_expr(cursor, 30, depth + 1, interner)?; // highest unary precedence
            Expr::Not(Box::new(operand))
        }

        Some(Token::LParen) => {
            let inner = parse_expr(cursor, 0, depth + 1, interner)?;
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
                Some(Token::Ident(name)) => Arc::from(name.as_str()),
                other => {
                    return Err(MizuError::ParseError(format!(
                        "expected field name after `.`, got: {other:?}"
                    )));
                }
            };
            lhs = Expr::FieldAccess {
                base: Box::new(lhs),
                field,
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
            let then_expr = parse_expr(cursor, 0, depth + 1, interner)?;
            match cursor.next() {
                Some(Token::Colon) => {}
                other => {
                    return Err(MizuError::ParseError(format!(
                        "expected `:` after `?` in ternary expression, got: {other:?}"
                    )));
                }
            }
            let else_expr = parse_expr(cursor, 0, depth + 1, interner)?; // right-assoc: min_bp = 0
            lhs = Expr::IfElse {
                condition: Box::new(lhs),
                then_expr: Box::new(then_expr),
                else_expr: Box::new(else_expr),
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
        let rhs = parse_expr(cursor, right_bp, depth + 1, interner)?;
        lhs = Expr::BinaryOp {
            left: Box::new(lhs),
            op,
            right: Box::new(rhs),
        };
    }

    Ok(lhs)
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


/// Parses a single function definition block.
///
/// `lines` is a non-empty slice of pre-cleaned source lines belonging to one
/// function (header + optional body).
fn parse_function_block(
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
        let body_expr = parse_expr(&mut cursor, 0, 0, interner)?;
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
    let body_expr: Expr;

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
        body_expr = parse_expr(&mut cursor, 0, 0, interner)?;
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
fn parse_params(
    param_str: &str,
    _context: &str,
    interner: &mut StringInterner,
) -> Result<Vec<(Symbol, Option<ValueType>)>, MizuError> {
    let mut params = Vec::new();
    if param_str.trim().is_empty() {
        return Ok(params);
    }
    for part in param_str.split(',') {
        let part = part.trim();
        let (name, vtype) = if let Some(colon) = part.find(':') {
            let name = part[..colon].trim();
            let type_str = part[colon + 1..].trim();
            let vtype = match type_str.to_lowercase().as_str() {
                "num" | "number" => ValueType::Num,
                "string" | "str" => ValueType::Str,
                "bool" | "boolean" => ValueType::Bool,
                "list" => ValueType::List,
                "dict" | "record" | "any" => {
                    return Err(MizuError::ParseError(format!(
                        "type `{type_str}` is not supported; use: num, string, bool, list"
                    )));
                }
                other => {
                    return Err(MizuError::ParseError(format!(
                        "unknown type `{other}` for parameter `{name}`; \
                         valid types: num, string, bool, list"
                    )));
                }
            };
            (name, Some(vtype))
        } else {
            (part, None)
        };
        let sym = interner.get_or_intern(name);
        params.push((sym, vtype));
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
) -> Result<Expr, MizuError> {
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

    // Parse the return expression.
    let tokens = lex(return_line)?;
    let mut cursor = Cursor::new(&tokens);
    let mut result_expr = parse_expr(&mut cursor, 0, 0, interner)?;

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
        let bind_expr = parse_expr(&mut bind_cursor, 0, 0, interner)?;
        let bind_sym = interner.get_or_intern(bind_name);
        result_expr = Expr::Let {
            name: bind_sym,
            value: Box::new(bind_expr),
            body: Box::new(result_expr),
        };
    }

    Ok(result_expr)
}

/// Finds the byte position of a bare assignment `=` in `s`, skipping over
/// multi-character operators (`==`, `!=`, `<=`, `>=`).
///
/// Returns `None` if no assignment-style `=` is present.
fn find_assignment_eq(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'=' {
            let prev_is_op = i > 0 && matches!(bytes[i - 1], b'!' | b'<' | b'>' | b'=');
            let next_is_eq = i + 1 < bytes.len() && bytes[i + 1] == b'=';
            if !prev_is_op && !next_is_eq {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Returns `true` if a trimmed line looks like `name = expr` (a binding).
///
/// A line is a binding if it contains a bare `=` (not `==`, `!=`, `<=`, `>=`)
/// AND the text before that `=` is a plain identifier.
fn looks_like_binding(line: &str) -> bool {
    if let Some(eq_pos) = find_assignment_eq(line) {
        let lhs = line[..eq_pos].trim();
        !lhs.is_empty()
            && lhs
                .chars()
                .next()
                .is_some_and(|c| c.is_alphabetic() || c == '_')
            && lhs.chars().all(|c| c.is_alphanumeric() || c == '_')
    } else {
        false
    }
}


/// Parses the `logic_block` produced by [`super::split_source`] into a
/// validated, recursion-free `HashMap` of function definitions.
///
/// ## Grammar (excerpt)
///
/// ```text
/// // Inline form
/// vat(price: num) : price * 1.22
///
/// // Multi-line form
/// total(price: num, qty: num)
///     netto = price * qty
///     netto * 1.22
/// ```
///
/// ## Errors
///
/// * [`MizuError::ParseError`] — for any syntactic violation, unknown type
///   annotation, or detected recursion cycle.
///
/// # Examples
///
/// ```
/// use mizu::parser::logic::parse_logic;
/// use mizu::core::types::StringInterner;
/// let src = "    vat(p: num) : p * 1.22\n";
/// let mut interner = StringInterner::new();
/// let fns = parse_logic(src, &mut interner).unwrap();
/// assert!(!fns.is_empty());
/// assert!(interner.get("vat").is_some());
/// ```
pub fn parse_logic(
    logic_content: &str,
    interner: &mut StringInterner,
) -> Result<FxHashMap<Symbol, MizuFunction>, MizuError> {
    // ── Group lines into per-function slices ─────────────────────────────
    // A function definition starts at a line whose leading indent equals the
    // baseline of the block (the minimum non-empty-line indent).
    let all_lines: Vec<&str> = logic_content.lines().collect();

    // Find baseline indentation.
    let baseline = all_lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| leading_spaces(l))
        .min()
        .unwrap_or(0);

    // Collect function definition groups.
    let mut groups: Vec<Vec<&str>> = Vec::new();
    let mut current_group: Vec<&str> = Vec::new();

    for line in &all_lines {
        if line.trim().is_empty() {
            continue;
        }
        let indent = leading_spaces(line);
        // Skip root-level `timer` and `comp` declarations — handled by dedicated parsers.
        if indent == baseline {
            let stripped = &line[baseline.min(line.len())..];
            if stripped.trim_start().starts_with("timer ") || stripped.trim() == "timer" {
                if !current_group.is_empty() {
                    groups.push(current_group.clone());
                    current_group.clear();
                }
                continue;
            }
            if stripped.trim_start().starts_with("comp ") {
                if !current_group.is_empty() {
                    groups.push(current_group.clone());
                    current_group.clear();
                }
                continue;
            }
        }
        if indent == baseline && !current_group.is_empty() {
            groups.push(current_group.clone());
            current_group.clear();
        }
        // Strip the baseline indent from each line before handing to the parser.
        current_group.push(&line[baseline.min(line.len())..]);
    }
    if !current_group.is_empty() {
        groups.push(current_group);
    }

    // ── Parse each function group ────────────────────────────────────────
    let mut functions: FxHashMap<Symbol, MizuFunction> = FxHashMap::default();
    for group in &groups {
        let (name, func) = parse_function_block(group, interner)?;
        let sym = interner.get_or_intern(&name);
        functions.insert(sym, func);
    }

    // ── Anti-recursion DAG check ─────────────────────────────────────────
    check_dag(&functions)?;

    Ok(functions)
}


/// Parses all `timer <interval> -> <action>` declarations from a `logic_block`.
///
/// Timer lines are silently skipped by [`parse_logic`]; this function handles
/// them as a second, independent pass over the same content.
///
/// ## Syntax
///
/// ```text
/// timer 500ms  -> count = count + 1
/// timer 1000ms -> refresh()
/// timer tick   -> tick = tick + 1   // variable interval
/// ```
///
/// The interval suffix `ms` is stripped; if the value is a plain number it is
/// treated as [`TimerInterval::Millis`], otherwise as [`TimerInterval::Variable`].
pub fn parse_root_timers(
    logic_content: &str,
    interner: &mut StringInterner,
) -> Result<Vec<RootTimer>, MizuError> {
    let mut timers: Vec<RootTimer> = Vec::new();

    let all_lines: Vec<&str> = logic_content.lines().collect();

    // Find baseline indentation (same as in parse_logic).
    let baseline = all_lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| leading_spaces(l))
        .min()
        .unwrap_or(0);

    for raw_line in &all_lines {
        if raw_line.trim().is_empty() {
            continue;
        }
        let indent = leading_spaces(raw_line);
        if indent != baseline {
            continue;
        }
        let stripped = &raw_line[baseline.min(raw_line.len())..].trim_end();
        let Some(rest) = stripped.strip_prefix("timer ") else {
            continue;
        };

        // Split on `->` to get `interval_str` and `action_str`.
        let arrow_pos = rest.find("->").ok_or_else(|| {
            MizuError::ParseError(format!("timer declaration missing `->`: `{stripped}`"))
        })?;
        let interval_str = rest[..arrow_pos].trim();
        let action_str = rest[arrow_pos + 2..].trim();

        // Parse the root-timer interval.
        // Accepted forms: `500ms`, `60s`, `1s`, bare integer (ms), variable name.
        let interval = if let Some(ms_str) = interval_str.strip_suffix("ms") {
            match ms_str.trim().parse::<u64>() {
                Ok(ms) => TimerInterval::Millis(ms),
                Err(_) => TimerInterval::Variable(ms_str.trim().to_string()),
            }
        } else if let Some(s_str) = interval_str.strip_suffix('s') {
            match s_str.trim().parse::<f64>() {
                Ok(s_val) => TimerInterval::Millis((s_val * 1000.0) as u64),
                Err(_) => TimerInterval::Variable(s_str.trim().to_string()),
            }
        } else {
            // Bare number or variable name without suffix.
            match interval_str.parse::<u64>() {
                Ok(ms) => TimerInterval::Millis(ms),
                Err(_) => TimerInterval::Variable(interval_str.to_string()),
            }
        };

        let action = parse_action(action_str, interner)?;
        timers.push(RootTimer { interval, action });
    }

    Ok(timers)
}


/// Parses a standalone expression string into an [`Expr`] AST node.
///
/// Used by the layout parser to parse conditional class conditions
/// (e.g., the `flag` part of `class active if flag`).
///
/// # Errors
///
/// Returns [`MizuError::ParseError`] if the input is syntactically invalid
/// or if tokens remain unconsumed after the expression.
pub fn parse_expr_standalone(expr: &str, interner: &mut StringInterner) -> Result<Expr, MizuError> {
    let tokens = lex(expr)?;
    let mut cursor = Cursor::new(&tokens);
    let e = parse_expr(&mut cursor, 0, 0, interner)?;
    assert_cursor_empty(&cursor, "")?;
    Ok(e)
}

/// Runs Kahn's BFS topological sort over the function call-graph to detect
/// cycles (i.e., recursion).
///
/// # Errors
///
/// Returns [`MizuError::ParseError`] if any cycle is detected.
fn check_dag(functions: &FxHashMap<Symbol, MizuFunction>) -> Result<(), MizuError> {
    let mut edges: FxHashMap<Symbol, FxHashSet<Symbol>> = FxHashMap::default();
    let mut in_degree: FxHashMap<Symbol, usize> = FxHashMap::default();

    let function_names: FxHashSet<Symbol> = functions.keys().copied().collect();

    for &sym in functions.keys() {
        edges.entry(sym).or_default();
        in_degree.entry(sym).or_insert(0);
    }

    for (&sym, func) in functions {
        let mut calls: FxHashSet<Symbol> = FxHashSet::default();
        collect_calls(&func.body, &mut calls, &function_names);

        for callee in calls {
            if functions.contains_key(&callee) {
                edges.entry(sym).or_default().insert(callee);
                *in_degree.entry(callee).or_insert(0) += 1;
            }
        }
    }

    // Kahn's BFS: start with all nodes of in-degree 0.
    let mut queue: VecDeque<Symbol> = in_degree
        .iter()
        .filter_map(|(&sym, &deg)| if deg == 0 { Some(sym) } else { None })
        .collect();

    let mut visited = 0usize;

    while let Some(node) = queue.pop_front() {
        visited += 1;
        if let Some(neighbours) = edges.get(&node) {
            let neighbours: Vec<Symbol> = neighbours.iter().copied().collect();
            for neighbour in neighbours {
                let deg = in_degree.entry(neighbour).or_insert(0);
                *deg = deg.saturating_sub(1);
                if *deg == 0 {
                    queue.push_back(neighbour);
                }
            }
        }
    }

    if visited != functions.len() {
        return Err(MizuError::ParseError(
            "Recursion and infinite loops are strictly forbidden: \
             a cycle was detected in the function call graph"
                .to_owned(),
        ));
    }

    Ok(())
}


/// Parses an action string (e.g. from a `click -> ...` event) into an [`Action`] AST node.
///
/// When `url_registry` is provided, built-in HTTP verb calls (`GET(alias)`,
/// `POST(alias, payload)`, etc.) are compile-time validated against the
/// registry.  Pass `None` to skip validation (e.g., in unit tests).
pub fn parse_action(action: &str, interner: &mut StringInterner) -> Result<Action, MizuError> {
    parse_action_with_urls(action, interner, None)
}

/// Like [`parse_action`] but accepts an optional [`UrlRegistry`] for API guard validation.
pub fn parse_action_with_urls(
    action: &str,
    interner: &mut StringInterner,
    url_registry: Option<&UrlRegistry>,
) -> Result<Action, MizuError> {
    let action_trimmed = action.trim();

    // ── Helper: parse a `VERB(alias, [...]) -> target` HTTP call ──
    //
    // Argument layout depends on whether the verb carries a request body:
    //
    //   No-body verbs  (GET, DELETE):  `(alias[, path_param])  -> var`
    //   Body verbs     (POST, PUT, QUERY): `(alias[, payload[, path_param]]) -> var`
    fn parse_network_call(
        method: NetworkMethod,
        rest: &str,
        interner: &mut StringInterner,
        url_registry: Option<&UrlRegistry>,
    ) -> Result<Action, MizuError> {
        let open = rest.find('(').ok_or_else(|| {
            MizuError::ParseError(format!(
                "network call `{m}` missing `(`: expected `{m}(alias) -> var`",
                m = method.as_str()
            ))
        })?;
        let close = rest.rfind(')').ok_or_else(|| {
            MizuError::ParseError(format!(
                "network call `{m}` missing `)`: expected `{m}(alias) -> var`",
                m = method.as_str()
            ))
        })?;
        let args_str = rest[open + 1..close].trim();
        let after_close = rest[close + 1..].trim();
        let target_var = if let Some(stripped) = after_close.strip_prefix("->") {
            stripped.trim().to_string()
        } else {
            return Err(MizuError::ParseError(format!(
                "network call `{m}` missing `-> target_var` after `)`",
                m = method.as_str()
            )));
        };

        // Whether this verb carries a request body (POST, PUT, QUERY do; GET, DELETE do not).
        let has_body = matches!(
            method,
            NetworkMethod::Post | NetworkMethod::Put | NetworkMethod::Query
        );

        // Max 3 slots: alias [, second_arg [, third_arg]]
        let mut arg_parts = args_str.splitn(3, ',').map(str::trim);

        let alias_str = arg_parts.next().filter(|s| !s.is_empty()).ok_or_else(|| {
            MizuError::ParseError(format!(
                "network call `{}` missing alias argument",
                method.as_str()
            ))
        })?;

        let alias_sym = interner.get_or_intern(alias_str);

        // ── Compile-time API guard ────────────────────────────────────────
        if let Some(registry) = url_registry {
            match registry.get(&alias_sym) {
                None => {
                    return Err(MizuError::ParseError(format!(
                        "network call `{}({alias_str})`: alias `{alias_str}` \
                         is not defined in the `urls` block",
                        method.as_str()
                    )));
                }
                Some(ep) if ep.kind != EndpointKind::Api => {
                    return Err(MizuError::ParseError(format!(
                        "network call `{}({alias_str})`: alias `{alias_str}` \
                         is a `media` endpoint, not an `api` endpoint",
                        method.as_str()
                    )));
                }
                _ => {}
            }
        }

        // Parse second and third arguments according to verb class.
        //
        // Body verbs:    slot2 = payload,    slot3 = path_param
        // No-body verbs: slot2 = path_param, slot3 = (disallowed)
        let second = arg_parts.next().filter(|s| !s.is_empty());
        let third = arg_parts.next().filter(|s| !s.is_empty());

        let (payload, path_param) = if has_body {
            // POST/PUT/QUERY(alias[, payload[, path_param]])
            let payload = if let Some(src) = second {
                let tokens = lex(src)?;
                let mut cursor = Cursor::new(&tokens);
                Some(Box::new(parse_expr(&mut cursor, 0, 0, interner)?))
            } else {
                None
            };
            let path_param = if let Some(src) = third {
                let tokens = lex(src)?;
                let mut cursor = Cursor::new(&tokens);
                Some(Box::new(parse_expr(&mut cursor, 0, 0, interner)?))
            } else {
                None
            };
            (payload, path_param)
        } else {
            // GET/DELETE(alias[, path_param])  — no body slot
            if third.is_some() {
                return Err(MizuError::ParseError(format!(
                    "network call `{}` does not accept a body argument: \
                     use `{}(alias[, path_param]) -> var`",
                    method.as_str(),
                    method.as_str()
                )));
            }
            let path_param = if let Some(src) = second {
                let tokens = lex(src)?;
                let mut cursor = Cursor::new(&tokens);
                Some(Box::new(parse_expr(&mut cursor, 0, 0, interner)?))
            } else {
                None
            };
            (None, path_param)
        };

        Ok(Action::NetworkCall {
            method,
            alias_sym,
            payload,
            path_param,
            target_var,
        })
    }

    // ── Detect uppercase HTTP verb built-ins: GET(...), POST(...), etc. ──
    let upper = action_trimmed.to_ascii_uppercase();
    let network_method = if upper.starts_with("GET(") {
        Some(NetworkMethod::Get)
    } else if upper.starts_with("POST(") {
        Some(NetworkMethod::Post)
    } else if upper.starts_with("PUT(") {
        Some(NetworkMethod::Put)
    } else if upper.starts_with("DELETE(") {
        Some(NetworkMethod::Delete)
    } else if upper.starts_with("QUERY(") {
        Some(NetworkMethod::Query)
    } else {
        None
    };

    if let Some(method) = network_method {
        let verb_len = method.as_str().len(); // "GET".len() == 3
        let rest = &action_trimmed[verb_len..]; // starts at `(`
        return parse_network_call(method, rest, interner, url_registry);
    }

    // Lowercase HTTP verbs (`get url -> var`) are intentionally rejected.
    // Network calls must use the uppercase registry form: GET(alias) -> var.
    for lc_verb in &["get ", "post ", "put ", "delete "] {
        if action_trimmed.to_ascii_lowercase().starts_with(lc_verb) {
            let verb = lc_verb.trim_end();
            return Err(MizuError::ParseError(format!(
                "lowercase `{verb}` is not a valid action; \
                 use the uppercase registry form: {}(alias) -> var",
                verb.to_ascii_uppercase()
            )));
        }
    }

    // ── `download(alias)` — compile-time validated media download ────────────
    if action_trimmed.starts_with("download(") {
        let close = action_trimmed.rfind(')').ok_or_else(|| {
            MizuError::ParseError("download: missing `)`: expected `download(alias)`".to_string())
        })?;
        let alias = action_trimmed[9..close].trim();
        if alias.is_empty() {
            return Err(MizuError::ParseError(
                "download: alias cannot be empty: expected `download(alias)`".to_string(),
            ));
        }
        let alias_sym = interner.get_or_intern(alias);
        if let Some(registry) = url_registry {
            match registry.get(&alias_sym) {
                None => {
                    return Err(MizuError::ParseError(format!(
                        "download alias `{alias}` is not declared in the `urls` block"
                    )));
                }
                Some(ep) if ep.kind != EndpointKind::Media => {
                    return Err(MizuError::ParseError(format!(
                        "download alias `{alias}` must be a `media` endpoint, not `api`"
                    )));
                }
                _ => {}
            }
        }
        let download_sym = interner.get_or_intern("download");
        return Ok(Action::Eval(Expr::FunctionCall {
            name: download_sym,
            args: vec![Expr::Variable(alias_sym)],
        }));
    }

    if let Some(rest) = action_trimmed.strip_prefix("navigate ") {
        let tokens = lex(rest.trim())?;
        let mut cursor = Cursor::new(&tokens);
        let url = parse_expr(&mut cursor, 0, 0, interner)?;
        assert_cursor_empty(&cursor, "`navigate ...`")?;
        Ok(Action::Navigate { url })
    } else if let Some(eq_pos) = find_assignment_eq(action_trimmed) {
        let lhs = action_trimmed[..eq_pos].trim();
        let rhs = action_trimmed[eq_pos + 1..].trim();

        if lhs.is_empty() || rhs.is_empty() {
            return Err(MizuError::ParseError(format!(
                "invalid assignment action: `{action}`"
            )));
        }

        let tokens = lex(rhs)?;
        let mut cursor = Cursor::new(&tokens);
        let expr = parse_expr(&mut cursor, 0, 0, interner)?;
        assert_cursor_empty(&cursor, &format!("`{lhs} = ...`"))?;
        Ok(Action::Assign {
            target: lhs.to_string(),
            expr,
        })
    } else {
        if action_trimmed.is_empty() {
            return Err(MizuError::ParseError("action cannot be empty".to_string()));
        }
        let tokens = lex(action_trimmed)?;
        let mut cursor = Cursor::new(&tokens);
        let expr = parse_expr(&mut cursor, 0, 0, interner)?;
        assert_cursor_empty(&cursor, &format!("`{action_trimmed}`"))?;
        Ok(Action::Eval(expr))
    }
}

/// True for ASCII control characters (`< 0x20` or `DEL`, `0x7F`).
///
/// Mirrors `isCtl` in `formal/MizuFormal/Semantics.lean`.
fn is_ctl(c: char) -> bool {
    (c as u32) < 0x20 || c as u32 == 0x7F
}

/// The `path_param` validation gate (G2): rejects path separators (`/`,
/// `\`), ASCII control characters, and the `..` traversal substring, so a
/// value bound from an untrusted network response can never restructure the
/// endpoint's URL path when substituted into it.
///
/// Mirrors `pathParamOk` in `formal/MizuFormal/Semantics.lean`; every call
/// site that consumes a `path_param` (`execute_action` below and
/// `resolve_endpoint_url` in `logic_worker.rs`) must run it before the value
/// is used to build a URL.
pub(crate) fn path_param_ok(s: &str) -> bool {
    !s.chars().any(|c| c == '/' || c == '\\' || is_ctl(c)) && !s.contains("..")
}
