//! # `logic` — Mizu Logic Block Parser, DAG Validator & Expression Evaluator
//!
//! This module implements Phase 4 of the Mizu compilation pipeline: it
//! transforms the raw `logic_block` string produced by [`super::splitter`]
//! into a validated, recursion-free [`HashMap`] of procedure definitions,
//! and exposes an [`evaluate`] function to reduce any [`Expr`] to a
//! concrete [`Value`].
//!
//! ## Pipeline Position
//!
//! ```text
//! logic_block: String   (from parser::splitter)
//!        │
//!        ▼
//! ┌─────────────────────────────────┐
//! │  logic::lex                     │  tokenise the source text
//! │  logic::parse_function_def      │  Pratt-parse each function
//! │  logic::check_dag               │  Kahn's algorithm cycle detection
//! │  logic::evaluate                │  expression evaluator
//! └───────────────┬─────────────────┘
//!                 │  HashMap<Symbol, MizuFunction>
//!                 ▼
//!         (Phase 5) renderer / layout binding
//! ```
//!
//! ## Parsing Strategy
//!
//! ### Tokeniser
//!
//! A hand-rolled byte scanner converts the source text into a flat
//! [`Vec<Token>`].  This avoids any regex dependency and keeps error positions
//! precise.
//!
//! ### Expression Parser — Pratt (Top-Down Operator Precedence)
//!
//! Operator precedence is encoded as a *binding power* table:
//!
//! | Operator | Left BP | Right BP |
//! |----------|---------|----------|
//! | `+`      | 10      | 11       |
//! | `-`      | 10      | 11       |
//! | `*`      | 20      | 21       |
//! | `/`      | 20      | 21       |
//!
//! Higher binding power means the operator binds its operands more tightly.
//! The right BP is one higher than the left to implement left-associativity
//! (e.g., `1 - 2 - 3` parses as `(1 - 2) - 3`).
//!
//! ### Anti-Recursion DAG — Kahn's Algorithm
//!
//! After all function bodies are parsed, a directed call-graph is constructed:
//! a directed edge `A → B` means function `A` calls function `B`.  Kahn's
//! algorithm performs a BFS topological sort:
//!
//! 1. Compute the in-degree of every node.
//! 2. Enqueue all nodes with in-degree zero.
//! 3. For each node dequeued, reduce its successors' in-degrees; enqueue newly
//!    zero-in-degree nodes.
//! 4. If the queue empties before all nodes are visited, a cycle exists →
//!    `MizuError::ParseError("Recursion and infinite loops are strictly forbidden")`.
//!
//! This guarantees compile-time rejection of both direct recursion (`f` calls
//! `f`) and mutual recursion (`f` calls `g`, `g` calls `f`).

#![forbid(unsafe_code)]

use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::VecDeque;
use std::sync::Arc;

use crate::core::errors::MizuError;
use crate::core::types::{StringInterner, Symbol, Value, VariableStore};
use crate::parser::urls::{EndpointKind, UrlRegistry};


/// The type annotation on a Mizu function parameter or binding.
///
/// Only the four concrete types that `check_type` can actually enforce are
/// represented. Parameters without an annotation use `None` at the call site
/// and accept any runtime value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueType {
    /// Corresponds to the `num` keyword — maps to [`Value::Int`] or [`Value::Float`].
    Num,
    /// Corresponds to the `string` keyword — maps to [`Value::String`].
    Str,
    /// Corresponds to the `bool` keyword — maps to [`Value::Bool`].
    Bool,
    /// Corresponds to the `list` keyword — matches any [`Value::List`] at runtime.
    List,
}

impl ValueType {
    /// Returns the Mizu source-language keyword for this type.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            ValueType::Num => "num",
            ValueType::Str => "string",
            ValueType::Bool => "bool",
            ValueType::List => "list",
        }
    }
}


/// HTTP method for a compile-time–validated network call.
///
/// Used by [`Action::NetworkCall`] — the Mizu source verbs `GET`, `POST`,
/// `PUT`, `DELETE`, and `QUERY` each map to one variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkMethod {
    /// `GET` — retrieve a resource.
    Get,
    /// `POST` — create or submit.
    Post,
    /// `PUT` — replace a resource.
    Put,
    /// `DELETE` — remove a resource.
    Delete,
    /// `QUERY` — server-side filter / search (non-standard extension).
    Query,
}

impl NetworkMethod {
    /// Returns the uppercase HTTP method string.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            NetworkMethod::Get => "GET",
            NetworkMethod::Post => "POST",
            NetworkMethod::Put => "PUT",
            NetworkMethod::Delete => "DELETE",
            NetworkMethod::Query => "QUERY",
        }
    }
}


/// A recurring timer declared at the root of the `logic` block.
///
/// Syntax: `timer <interval> -> <action>`
///
/// Example: `timer 500ms -> count = count + 1`
#[derive(Debug, Clone, PartialEq)]
pub struct RootTimer {
    /// How often the action fires.
    pub interval: TimerInterval,
    /// The action to execute on each tick.
    pub action: Action,
}

/// A timer interval, either a literal millisecond count or a variable name.
#[derive(Debug, Clone, PartialEq)]
pub enum TimerInterval {
    /// A constant interval in milliseconds (e.g. `500ms` → `500`).
    Millis(u64),
    /// A variable identifier whose runtime value specifies milliseconds.
    Variable(String),
}

/// A binary operator (arithmetic, comparison, or logical).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BinOp {
    /// Addition (`+`).
    Add,
    /// Subtraction (`-`).
    Sub,
    /// Multiplication (`*`).
    Mul,
    /// Division (`/`).
    Div,
    /// Equality (`==`).
    Eq,
    /// Inequality (`!=`).
    Ne,
    /// Less-than (`<`).
    Lt,
    /// Greater-than (`>`).
    Gt,
    /// Less-than-or-equal (`<=`).
    Le,
    /// Greater-than-or-equal (`>=`).
    Ge,
    /// Logical AND (`&&`).
    And,
    /// Logical OR (`||`).
    Or,
}

/// An expression node in the Mizu AST.
///
/// `Expr` is a read-only AST tree — there are no mutation nodes,
/// no assignment nodes, and no loop nodes.  Every evaluation is a
/// deterministic fold over this tree.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// A compile-time constant literal.
    Literal(Value),

    /// A variable reference resolved at evaluation time via [`VariableStore`].
    /// The identifier is pre-interned at parse time — no HashMap lookup at runtime.
    Variable(Symbol),

    /// A binary arithmetic operation.
    BinaryOp {
        /// Left-hand operand.
        left: Box<Expr>,
        /// The operator.
        op: BinOp,
        /// Right-hand operand.
        right: Box<Expr>,
    },

    /// A call to a named Mizu function.
    FunctionCall {
        /// The function name, pre-interned at parse time.
        name: Symbol,
        /// Positional argument expressions, evaluated left-to-right.
        args: Vec<Expr>,
    },

    /// A local binding used in multi-line function bodies.
    ///
    /// `let name = value_expr in body_expr`
    ///
    /// This node is not written by users; the parser synthesises it from
    /// indented `name = expr` lines within a multi-line function body.
    Let {
        /// The bound name, pre-interned at parse time.
        name: Symbol,
        /// The expression whose result is bound to `name`.
        value: Box<Expr>,
        /// The expression that may reference `name`.
        body: Box<Expr>,
    },

    /// Logical NOT unary operator (`!expr`).
    Not(Box<Expr>),

    /// A conditional expression — produced by both syntactic forms:
    ///
    /// * `if <cond> then <then> else <else>`
    /// * `<cond> ? <then> : <else>`
    ///
    /// Evaluation is **lazy**: only the selected branch is evaluated.
    /// The condition must evaluate to `bool`; a non-bool condition is a
    /// `TypeError`.
    IfElse {
        /// The boolean guard expression.
        condition: Box<Expr>,
        /// Expression evaluated when condition is true.
        then_expr: Box<Expr>,
        /// Expression evaluated when condition is false.
        else_expr: Box<Expr>,
    },

    /// Field access on a [`Value::Record`]: `base.field`.
    ///
    /// `base` must evaluate to a `Record`; accessing a missing field or a
    /// non-record base is a runtime error.  Chains (`a.b.c`) are represented
    /// as left-nested nodes: `FieldAccess { base: FieldAccess { base: a, field: b }, field: c }`.
    FieldAccess {
        /// The base expression, which must evaluate to a `Record`.
        base: Box<Expr>,
        /// The field name to look up in the record.
        field: Arc<str>,
    },
}

/// An interactive action triggered by an event.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// An expression evaluated for its effects (e.g., calling a procedure).
    Eval(Expr),
    /// An assignment that mutates a variable in the store.
    Assign {
        /// The target variable name.
        target: String,
        /// The expression to evaluate and assign.
        expr: Expr,
    },
    /// A declarative navigation request to completely replace the document.
    Navigate {
        /// The URI expression to navigate to.
        url: Expr,
    },
    /// A compile-time–validated HTTP call via a named URL alias.
    ///
    /// The alias is resolved at parse time against the [`UrlRegistry`]; a
    /// missing or wrong-kind alias is a hard compile error.
    NetworkCall {
        /// HTTP verb.
        method: NetworkMethod,
        /// The interned Symbol for the URL alias (e.g. `login` → `Symbol(N)`).
        alias_sym: Symbol,
        /// Optional JSON payload expression (used by POST, PUT, QUERY).
        payload: Option<Box<Expr>>,
        /// Optional path parameter expression (used by DELETE for `/item/{id}`).
        path_param: Option<Box<Expr>>,
        /// The variable name that receives the response.
        target_var: String,
    },
}

/// A compiled Mizu function definition.
///
/// After passing the DAG validation step, instances of this struct can be
/// used freely by [`evaluate`] without risk of infinite recursion.
#[derive(Debug, Clone)]
pub struct MizuFunction {
    /// Ordered list of `(parameter_symbol, optional_type_annotation)` pairs.
    /// `None` means the parameter is untyped — any runtime value is accepted.
    /// The symbol is pre-interned — no string allocation at call time.
    pub params: Vec<(Symbol, Option<ValueType>)>,
    /// The function body expression (may be a chain of [`Expr::Let`] nodes
    /// for multi-line functions, with the return value at the innermost body).
    pub body: Expr,
}

/// A computed (derived) variable that auto-recomputes when dependencies change.
///
/// Syntax: `comp name = expr`
///
/// The `depends_on` list is derived statically by walking the right-hand-side
/// AST with [`collect_vars`].  Bindings are stored in topological order
/// (dependencies before dependents) after [`parse_computed`] validates the
/// absence of cycles.
#[derive(Debug, Clone)]
pub struct ComputedBinding {
    /// Interned symbol for the variable name.
    pub name: Symbol,
    /// The expression that defines this variable's value.
    pub expr: Expr,
    /// Symbols of all variables referenced by `expr` (may include other comp vars).
    pub depends_on: Vec<Symbol>,
}


/// Internal token produced by the Mizu logic lexer.
#[derive(Debug, Clone, PartialEq)]
enum Token {
    /// An identifier or keyword.
    Ident(String),
    /// A numeric literal (already parsed to `f64`).
    Num(f64),
    /// A string literal (content without surrounding quotes).
    Str(String),
    /// `true` or `false`.
    Bool(bool),
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `,`
    Comma,
    /// `:`
    Colon,
    /// `=` (assignment)
    Eq,
    /// `==`
    EqEq,
    /// `!=`
    BangEq,
    /// `<`
    Lt,
    /// `>`
    Gt,
    /// `<=`
    LtEq,
    /// `>=`
    GtEq,
    /// `&&`
    AndAnd,
    /// `||`
    OrOr,
    /// `!`
    Bang,
    /// `+`
    Plus,
    /// `-`
    Minus,
    /// `*`
    Star,
    /// `/`
    Slash,
    /// `?` (ternary operator: `cond ? then : else`)
    Question,
    /// `.` (field access operator: `record.field`)
    Dot,
    /// End of a logical line (newline after non-whitespace content).
    Newline,
    /// A logical indentation increase.
    Indent,
    /// A logical indentation decrease.
    Dedent,
}

/// Tokenises a single function definition source string.
///
/// The input is a slice of lines belonging to one function (as assembled by
/// `parse_logic`).  Returns a flat token stream including `Indent`/`Dedent`
/// markers that let the parser track block structure without counting spaces.
fn lex(source: &str) -> Result<Vec<Token>, MizuError> {
    let mut tokens: Vec<Token> = Vec::new();
    let mut indent_stack: Vec<usize> = vec![0];

    for (line_idx, raw_line) in source.lines().enumerate() {
        // Strip trailing whitespace; skip blank lines.
        let line = raw_line.trim_end();
        if line.trim().is_empty() {
            continue;
        }

        // ── Measure indentation ──────────────────────────────────────────
        let indent = leading_spaces(line);
        let &current = indent_stack.last().ok_or_else(|| {
            MizuError::ParseError(format!("line {}: indent stack underflow", line_idx + 1))
        })?;

        if indent > current {
            indent_stack.push(indent);
            tokens.push(Token::Indent);
        } else if indent < current {
            // Pop until we reach a matching indent or the stack is exhausted.
            while indent_stack.last().copied().unwrap_or(0) > indent {
                indent_stack.pop();
                tokens.push(Token::Dedent);
            }
            if indent_stack.last().copied().unwrap_or(0) != indent {
                return Err(MizuError::ParseError(format!(
                    "line {}: inconsistent indentation ({indent} spaces does not \
                     match any enclosing level)",
                    line_idx + 1
                )));
            }
        }

        // ── Scan the content part of the line ────────────────────────────
        lex_line(line.trim_start(), &mut tokens, line_idx + 1)?;
        tokens.push(Token::Newline);
    }

    // Emit final Dedent tokens to close any open blocks.
    while indent_stack.len() > 1 {
        indent_stack.pop();
        tokens.push(Token::Dedent);
    }

    Ok(tokens)
}

/// Scans a single line's *content* (already stripped of leading whitespace)
/// and appends tokens to `out`.
fn lex_line(content: &str, out: &mut Vec<Token>, line_num: usize) -> Result<(), MizuError> {
    let bytes = content.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        let b = bytes[i];

        // Skip whitespace within a line.
        if b == b' ' || b == b'\t' {
            i += 1;
            continue;
        }

        // ── String literal ───────────────────────────────────────────────
        if b == b'"' {
            i += 1; // skip opening quote
            let start = i;
            while i < bytes.len() && bytes[i] != b'"' {
                if bytes[i] == b'\\' {
                    i += 1; // skip escaped char
                }
                i += 1;
            }
            if i >= bytes.len() {
                return Err(MizuError::ParseError(format!(
                    "line {line_num}: unterminated string literal"
                )));
            }
            // SAFETY: content is valid UTF-8 (came from a &str slice).
            let s = std::str::from_utf8(&bytes[start..i]).map_err(|_| {
                MizuError::ParseError(format!("line {line_num}: invalid UTF-8 in string literal"))
            })?;
            out.push(Token::Str(s.to_owned()));
            i += 1; // skip closing quote
            continue;
        }

        // ── Numeric literal ──────────────────────────────────────────────
        if b.is_ascii_digit() || (b == b'-' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit())
        {
            // Only treat leading `-` as part of a number if the previous
            // token is NOT a value-producing token (to disambiguate `a - b`).
            let is_negation_start = b == b'-'
                && !matches!(
                    out.last(),
                    Some(
                        Token::Num(_)
                            | Token::Ident(_)
                            | Token::Bool(_)
                            | Token::Str(_)
                            | Token::RParen
                    )
                );

            if b == b'-' && !is_negation_start {
                out.push(Token::Minus);
                i += 1;
                continue;
            }

            let start = i;
            if b == b'-' {
                i += 1;
            }
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                i += 1;
            }
            let num_str = std::str::from_utf8(&bytes[start..i]).map_err(|_| {
                MizuError::ParseError(format!("line {line_num}: invalid numeric token"))
            })?;
            let n: f64 = num_str.parse().map_err(|_| {
                MizuError::ParseError(format!(
                    "line {line_num}: cannot parse `{num_str}` as a number"
                ))
            })?;
            out.push(Token::Num(n));
            continue;
        }

        // ── Identifier / keyword ─────────────────────────────────────────
        // `$` is allowed as a prefix for magic variables (e.g. `$form`).
        if b.is_ascii_alphabetic() || b == b'_' || b == b'$' {
            let start = i;
            i += 1; // consume the first char (may be `$`)
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let word = std::str::from_utf8(&bytes[start..i]).map_err(|_| {
                MizuError::ParseError(format!("line {line_num}: invalid UTF-8 in identifier"))
            })?;
            let tok = match word {
                "true" => Token::Bool(true),
                "false" => Token::Bool(false),
                _ => Token::Ident(word.to_owned()),
            };
            out.push(tok);
            continue;
        }

        // ── Operator tokens (with one-byte lookahead for multi-char ops) ──
        let next = bytes.get(i + 1).copied();
        let (tok, advance) = match (b, next) {
            (b'=', Some(b'=')) => (Token::EqEq, 2),
            (b'!', Some(b'=')) => (Token::BangEq, 2),
            (b'<', Some(b'=')) => (Token::LtEq, 2),
            (b'>', Some(b'=')) => (Token::GtEq, 2),
            (b'&', Some(b'&')) => (Token::AndAnd, 2),
            (b'|', Some(b'|')) => (Token::OrOr, 2),
            (b'=', _) => (Token::Eq, 1),
            (b'!', _) => (Token::Bang, 1),
            (b'<', _) => (Token::Lt, 1),
            (b'>', _) => (Token::Gt, 1),
            (b'(', _) => (Token::LParen, 1),
            (b')', _) => (Token::RParen, 1),
            (b',', _) => (Token::Comma, 1),
            (b':', _) => (Token::Colon, 1),
            (b'+', _) => (Token::Plus, 1),
            (b'-', _) => (Token::Minus, 1),
            (b'*', _) => (Token::Star, 1),
            (b'/', _) => (Token::Slash, 1),
            (b'?', _) => (Token::Question, 1),
            (b'.', _) => (Token::Dot, 1),
            (other, _) => {
                return Err(MizuError::ParseError(format!(
                    "line {line_num}: unexpected character `{}`",
                    other as char
                )));
            }
        };
        out.push(tok);
        i += advance;
    }
    Ok(())
}


/// A simple indexed cursor over a token stream.
struct Cursor<'a> {
    tokens: &'a [Token],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(tokens: &'a [Token]) -> Self {
        Self { tokens, pos: 0 }
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Option<&Token> {
        let tok = self.tokens.get(self.pos)?;
        self.pos += 1;
        Some(tok)
    }
}

/// Returns a human-readable representation of a token for use in error messages.
fn token_display(tok: &Token) -> String {
    match tok {
        Token::Ident(s) => format!("`{s}`"),
        Token::Str(s) => format!("`\"{s}\"`"),
        Token::Num(n) => format!("`{n}`"),
        Token::Bool(b) => format!("`{b}`"),
        Token::LParen => "`(`".to_owned(),
        Token::RParen => "`)`".to_owned(),
        Token::Comma => "`,`".to_owned(),
        Token::Colon => "`:`".to_owned(),
        Token::Eq => "`=`".to_owned(),
        Token::EqEq => "`==`".to_owned(),
        Token::BangEq => "`!=`".to_owned(),
        Token::Lt => "`<`".to_owned(),
        Token::Gt => "`>`".to_owned(),
        Token::LtEq => "`<=`".to_owned(),
        Token::GtEq => "`>=`".to_owned(),
        Token::AndAnd => "`&&`".to_owned(),
        Token::OrOr => "`||`".to_owned(),
        Token::Bang => "`!`".to_owned(),
        Token::Plus => "`+`".to_owned(),
        Token::Minus => "`-`".to_owned(),
        Token::Star => "`*`".to_owned(),
        Token::Slash => "`/`".to_owned(),
        Token::Question => "`?`".to_owned(),
        Token::Dot => "`.`".to_owned(),
        Token::Newline => "<newline>".to_owned(),
        Token::Indent => "<indent>".to_owned(),
        Token::Dedent => "<dedent>".to_owned(),
    }
}

/// Verifies that the cursor has no remaining *semantic* tokens after a complete
/// expression parse.  Structural tokens (`Newline`, `Indent`, `Dedent`) that
/// the lexer appends to every line are skipped — they carry no meaning in
/// a single-expression context such as an action string.
///
/// If a real token remains, returns a `ParseError` describing it and pointing
/// to `context` (e.g. the surrounding action string).
///
/// This is the root-cause fix for the "silent attribute loss" bug: if a user
/// accidentally writes a layout attribute after an action on the same line
/// (e.g. `click -> count = count + 1 class "btn"`), the expression parser stops
/// at `class` and returns successfully — but the cursor is not exhausted.  This
/// function converts that leftover into a hard error instead of silent data loss.
fn assert_cursor_empty(cursor: &Cursor<'_>, context: &str) -> Result<(), MizuError> {
    // Skip past any trailing structural tokens that the lexer appends to every
    // non-blank line; these are not user-visible syntax.
    let mut pos = cursor.pos;
    while let Some(tok) = cursor.tokens.get(pos) {
        match tok {
            Token::Newline | Token::Indent | Token::Dedent => pos += 1,
            _ => {
                return Err(MizuError::ParseError(format!(
                    "unexpected token {} after expression{}\n  \
                     hint: `->` consumes the entire line — layout attributes (e.g. `class`, `id`) \
                     must appear on the element line, not after the action",
                    token_display(tok),
                    if context.is_empty() {
                        String::new()
                    } else {
                        format!(" in {context}")
                    },
                )));
            }
        }
    }
    Ok(())
}


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
fn parse_expr(
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
            // DESIGN: the lexer produces only `f64`, so the Int-vs-Float choice
            // here is driven purely by the *value* (`4/2` → Int, `5/2` → Float).
            // This makes a literal's static type depend on its runtime value.
            // Unresolved: either unify on a single `num` type, or distinguish
            // integer vs float literals already in the lexer. See also [B].
            if n.fract() == 0.0 {
                Expr::Literal(Value::Int(*n as i64))
            } else {
                Expr::Literal(Value::Float(*n))
            }
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
                        "tipo `{type_str}` non supportato; usa: num, string, bool, list"
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

        // Parse interval — mirrors parse_interval in layout.rs.
        // Accepted forms: `500ms`, `60s`, `1.5s`, bare integer (ms), variable name.
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
        dep_set.remove(&name_sym);

        bindings.push(ComputedBinding {
            name: name_sym,
            expr,
            depends_on: dep_set.into_iter().collect(),
        });
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

/// Re-evaluates computed bindings whose dependencies include any symbol in `mutated`.
///
/// `bindings` must be in topological order (see [`parse_computed`]).
/// Any newly evaluated comp binding that produces a changed value is recorded in
/// `store.state_machine.undo_log` via [`VariableStore::set_symbol`], so it will be
/// picked up by the logic worker's `send_response` along with the original mutations.
///
/// Returns a superset of `mutated` extended with the symbols of any comp bindings
/// that were re-evaluated, so a chained call can propagate the recomputation.
pub fn recompute_computed_bindings(
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
fn collect_calls(expr: &Expr, out: &mut FxHashSet<Symbol>, function_names: &FxHashSet<Symbol>) {
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
];

/// Walks `expr` and returns the name of the first side-effecting function
/// call found, or `None` if the expression is pure.
pub fn find_side_effect_call(expr: &Expr, interner: &StringInterner) -> Option<String> {
    match expr {
        Expr::Literal(_) | Expr::Variable(_) => None,
        Expr::BinaryOp { left, right, .. } => {
            find_side_effect_call(left, interner).or_else(|| find_side_effect_call(right, interner))
        }
        Expr::FunctionCall { name, args } => {
            if let Some(n) = interner.resolve(*name)
                && SIDE_EFFECT_BUILTINS.contains(&n)
            {
                return Some(n.to_string());
            }
            for arg in args {
                if let Some(n) = find_side_effect_call(arg, interner) {
                    return Some(n);
                }
            }
            None
        }
        Expr::Let { value, body, .. } => {
            find_side_effect_call(value, interner).or_else(|| find_side_effect_call(body, interner))
        }
        Expr::Not(inner) => find_side_effect_call(inner, interner),
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => find_side_effect_call(condition, interner)
            .or_else(|| find_side_effect_call(then_expr, interner))
            .or_else(|| find_side_effect_call(else_expr, interner)),
        Expr::FieldAccess { base, .. } => find_side_effect_call(base, interner),
    }
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

/// Executes a compiled [`Action`] against the provided variable store.
///
/// Returns `true` if the action was an assignment (store mutated), `false` otherwise.
pub fn execute_action(
    action: &Action,
    store: &mut VariableStore,
    functions: &FxHashMap<Symbol, MizuFunction>,
) -> Result<bool, MizuError> {
    // Reset the instruction counter so the budget applies per action, not cumulatively.
    store.state_machine.instruction_count = 0;
    match action {
        Action::Assign { target, expr } => {
            if let Some(sym) = store.interner.get(target)
                && store.state_machine.computed_var_syms.contains(&sym)
            {
                return Err(MizuError::ExecutionError(format!(
                    "cannot assign to computed variable `{target}`"
                )));
            }
            let result = store
                .state_machine
                .evaluate(expr, 0, functions, &store.interner)?;
            store.set(target, result);
            Ok(true)
        }
        Action::Eval(expr) => {
            store
                .state_machine
                .evaluate(expr, 0, functions, &store.interner)?;
            Ok(false)
        }
        Action::Navigate { url } => {
            let eval_url = store
                .state_machine
                .evaluate(url, 0, functions, &store.interner)?;
            let url_str = match eval_url {
                Value::String(s) => s.to_string(),
                _ => {
                    return Err(MizuError::ExecutionError(
                        "Navigate URL must evaluate to a string".to_string(),
                    ));
                }
            };

            store
                .state_machine
                .accumulated_actions
                .push(crate::network::RuntimeAction::Navigate { url: url_str });
            Ok(true)
        }
        Action::NetworkCall {
            method,
            alias_sym,
            payload,
            path_param,
            target_var,
        } => {
            // Evaluate optional payload and path_param expressions.
            let payload_val = if let Some(p) = payload {
                Some(
                    store
                        .state_machine
                        .evaluate(p, 0, functions, &store.interner)?,
                )
            } else {
                None
            };
            let path_param_str = if let Some(pp) = path_param {
                let v = store
                    .state_machine
                    .evaluate(pp, 0, functions, &store.interner)?;
                Some(match v {
                    Value::String(s) => s.to_string(),
                    Value::Int(n) => n.to_string(),
                    Value::Float(f) => f.to_string(),
                    _ => {
                        return Err(MizuError::ExecutionError(
                            "path_param must be a string or number".to_string(),
                        ));
                    }
                })
            } else {
                None
            };
            store.state_machine.accumulated_actions.push(
                crate::network::RuntimeAction::NetworkCall {
                    method: method.clone(),
                    endpoint_symbol: alias_sym.0,
                    payload: payload_val,
                    path_param: path_param_str,
                    target_variable: target_var.clone(),
                },
            );
            Ok(true)
        }
    }
}

/// Evaluates a Mizu expression to a concrete [`Value`].
///
/// Resets `instruction_count` to `0` before delegating so the per-expression
/// budget is enforced from scratch on each call.
pub fn evaluate(
    expr: &Expr,
    store: &mut VariableStore,
    functions: &FxHashMap<Symbol, MizuFunction>,
    frame_pointer: usize,
) -> Result<Value, MizuError> {
    store.state_machine.instruction_count = 0;
    store
        .state_machine
        .evaluate(expr, frame_pointer, functions, &store.interner)
}

/// Applies a binary arithmetic operator to two already-evaluated values.
pub(crate) fn apply_binop(op: &BinOp, lv: Value, rv: Value) -> Result<Value, MizuError> {
    match (op, lv, rv) {
        // Num operations — Int×Int uses checked arithmetic to catch overflow in release builds.
        (BinOp::Add, Value::Int(l), Value::Int(r)) => l
            .checked_add(r)
            .map(Value::Int)
            .ok_or_else(|| MizuError::ExecutionError("integer overflow".to_owned())),
        (BinOp::Add, Value::Int(l), Value::Float(r)) => Ok(Value::Float(l as f64 + r)),
        (BinOp::Add, Value::Float(l), Value::Int(r)) => Ok(Value::Float(l + r as f64)),
        (BinOp::Add, Value::Float(l), Value::Float(r)) => Ok(Value::Float(l + r)),

        (BinOp::Sub, Value::Int(l), Value::Int(r)) => l
            .checked_sub(r)
            .map(Value::Int)
            .ok_or_else(|| MizuError::ExecutionError("integer overflow".to_owned())),
        (BinOp::Sub, Value::Int(l), Value::Float(r)) => Ok(Value::Float(l as f64 - r)),
        (BinOp::Sub, Value::Float(l), Value::Int(r)) => Ok(Value::Float(l - r as f64)),
        (BinOp::Sub, Value::Float(l), Value::Float(r)) => Ok(Value::Float(l - r)),

        (BinOp::Mul, Value::Int(l), Value::Int(r)) => l
            .checked_mul(r)
            .map(Value::Int)
            .ok_or_else(|| MizuError::ExecutionError("integer overflow".to_owned())),
        (BinOp::Mul, Value::Int(l), Value::Float(r)) => Ok(Value::Float(l as f64 * r)),
        (BinOp::Mul, Value::Float(l), Value::Int(r)) => Ok(Value::Float(l * r as f64)),
        (BinOp::Mul, Value::Float(l), Value::Float(r)) => Ok(Value::Float(l * r)),

        (BinOp::Div, Value::Int(l), Value::Int(r)) => {
            if r == 0 {
                Err(MizuError::DivisionByZero)
            } else {
                match (l.checked_rem(r), l.checked_div(r)) {
                    (Some(rem), Some(div)) => {
                        if rem == 0 {
                            Ok(Value::Int(div))
                        } else {
                            Ok(Value::Float(l as f64 / r as f64))
                        }
                    }
                    _ => Err(MizuError::ExecutionError("integer overflow".to_owned())),
                }
            }
        }
        (BinOp::Div, Value::Int(l), Value::Float(r)) => {
            if r == 0.0 {
                Err(MizuError::DivisionByZero)
            } else {
                Ok(Value::Float(l as f64 / r))
            }
        }
        (BinOp::Div, Value::Float(l), Value::Int(r)) => {
            if r == 0 {
                Err(MizuError::DivisionByZero)
            } else {
                Ok(Value::Float(l / r as f64))
            }
        }
        (BinOp::Div, Value::Float(l), Value::Float(r)) => {
            if r == 0.0 {
                Err(MizuError::DivisionByZero)
            } else {
                Ok(Value::Float(l / r))
            }
        }

        // String concatenation via `+`
        (BinOp::Add, Value::String(l), Value::String(r)) => Ok(Value::String(
            std::sync::Arc::from(format!("{l}{r}").as_str()),
        )),

        // Equality — works across numerics (with coercion) and strings/bools
        (BinOp::Eq, Value::Int(l), Value::Int(r)) => Ok(Value::Bool(l == r)),
        (BinOp::Eq, Value::Float(l), Value::Float(r)) => Ok(Value::Bool(l == r)),
        (BinOp::Eq, Value::Int(l), Value::Float(r)) => Ok(Value::Bool(l as f64 == r)),
        (BinOp::Eq, Value::Float(l), Value::Int(r)) => Ok(Value::Bool(l == r as f64)),
        (BinOp::Eq, Value::String(l), Value::String(r)) => Ok(Value::Bool(l == r)),
        (BinOp::Eq, Value::Bool(l), Value::Bool(r)) => Ok(Value::Bool(l == r)),
        (BinOp::Eq, Value::Null, Value::Null) => Ok(Value::Bool(true)),
        (BinOp::Eq, Value::Null, _) => Ok(Value::Bool(false)),
        (BinOp::Eq, _, Value::Null) => Ok(Value::Bool(false)),

        // Inequality — mirrors equality
        (BinOp::Ne, Value::Int(l), Value::Int(r)) => Ok(Value::Bool(l != r)),
        (BinOp::Ne, Value::Float(l), Value::Float(r)) => Ok(Value::Bool(l != r)),
        (BinOp::Ne, Value::Int(l), Value::Float(r)) => Ok(Value::Bool(l as f64 != r)),
        (BinOp::Ne, Value::Float(l), Value::Int(r)) => Ok(Value::Bool(l != r as f64)),
        (BinOp::Ne, Value::String(l), Value::String(r)) => Ok(Value::Bool(l != r)),
        (BinOp::Ne, Value::Bool(l), Value::Bool(r)) => Ok(Value::Bool(l != r)),
        (BinOp::Ne, Value::Null, Value::Null) => Ok(Value::Bool(false)),
        (BinOp::Ne, Value::Null, _) => Ok(Value::Bool(true)),
        (BinOp::Ne, _, Value::Null) => Ok(Value::Bool(true)),

        // Ordering — numeric types only
        (BinOp::Lt, Value::Int(l), Value::Int(r)) => Ok(Value::Bool(l < r)),
        (BinOp::Lt, Value::Float(l), Value::Float(r)) => Ok(Value::Bool(l < r)),
        (BinOp::Lt, Value::Int(l), Value::Float(r)) => Ok(Value::Bool((l as f64) < r)),
        (BinOp::Lt, Value::Float(l), Value::Int(r)) => Ok(Value::Bool(l < r as f64)),

        (BinOp::Gt, Value::Int(l), Value::Int(r)) => Ok(Value::Bool(l > r)),
        (BinOp::Gt, Value::Float(l), Value::Float(r)) => Ok(Value::Bool(l > r)),
        (BinOp::Gt, Value::Int(l), Value::Float(r)) => Ok(Value::Bool((l as f64) > r)),
        (BinOp::Gt, Value::Float(l), Value::Int(r)) => Ok(Value::Bool(l > r as f64)),

        (BinOp::Le, Value::Int(l), Value::Int(r)) => Ok(Value::Bool(l <= r)),
        (BinOp::Le, Value::Float(l), Value::Float(r)) => Ok(Value::Bool(l <= r)),
        (BinOp::Le, Value::Int(l), Value::Float(r)) => Ok(Value::Bool((l as f64) <= r)),
        (BinOp::Le, Value::Float(l), Value::Int(r)) => Ok(Value::Bool(l <= r as f64)),

        (BinOp::Ge, Value::Int(l), Value::Int(r)) => Ok(Value::Bool(l >= r)),
        (BinOp::Ge, Value::Float(l), Value::Float(r)) => Ok(Value::Bool(l >= r)),
        (BinOp::Ge, Value::Int(l), Value::Float(r)) => Ok(Value::Bool((l as f64) >= r)),
        (BinOp::Ge, Value::Float(l), Value::Int(r)) => Ok(Value::Bool(l >= r as f64)),

        // Logical AND / OR — bool operands only
        (BinOp::And, Value::Bool(l), Value::Bool(r)) => Ok(Value::Bool(l && r)),
        (BinOp::Or, Value::Bool(l), Value::Bool(r)) => Ok(Value::Bool(l || r)),

        // Type mismatch
        (_, l, _) => Err(MizuError::TypeError {
            expected: "compatible operand types",
            found: type_name(&l),
        }),
    }
}

/// Returns the Mizu type-name string for a runtime value.
pub(crate) fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Int(_) | Value::Float(_) => "num",
        Value::String(_) => "string",
        Value::Bool(_) => "bool",
        Value::List(_) => "list",
        Value::Record(_) => "record",
        Value::Null => "null",
    }
}

/// Verifies that a runtime argument value matches the declared parameter type.
///
/// `None` means the parameter has no type annotation — any value is accepted.
pub(crate) fn check_type(
    val: &Value,
    expected: Option<&ValueType>,
    func_name: &str,
    param_name: &str,
) -> Result<(), MizuError> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let ok = matches!(
        (val, expected),
        (Value::Int(_), ValueType::Num)
            | (Value::Float(_), ValueType::Num)
            | (Value::String(_), ValueType::Str)
            | (Value::Bool(_), ValueType::Bool)
            | (Value::List(_), ValueType::List)
    );
    if !ok {
        return Err(MizuError::TypeError {
            expected: expected.as_str(),
            found: type_name(val),
        });
    }
    let _ = (func_name, param_name);
    Ok(())
}


/// Returns the number of leading space characters in `line`.
#[inline]
fn leading_spaces(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}


#[cfg(test)]
mod tests {
    use super::{
        Action, BinOp, Expr, MizuFunction, NetworkMethod, TimerInterval, Value, ValueType,
        VariableStore, parse_action, parse_action_with_urls, parse_logic, parse_root_timers,
    };
    use crate::core::errors::MizuError;
    use crate::core::types::{StringInterner, Symbol};
    use rustc_hash::{FxHashMap, FxHashSet};
    use std::rc::Rc;

    fn single_fn(
        src: &str,
    ) -> Result<(FxHashMap<Symbol, MizuFunction>, StringInterner), MizuError> {
        let mut interner = StringInterner::new();
        let fns = parse_logic(src, &mut interner)?;
        Ok((fns, interner))
    }

    fn evaluate(
        expr: &Expr,
        store: &Rc<VariableStore>,
        functions: &FxHashMap<Symbol, MizuFunction>,
    ) -> Result<Value, MizuError> {
        let mut temp_store = (**store).clone();
        super::evaluate(expr, &mut temp_store, functions, 0)
    }

    fn execute_action(
        action: &Action,
        store: &mut Rc<VariableStore>,
        functions: &FxHashMap<Symbol, MizuFunction>,
    ) -> Result<bool, MizuError> {
        let mut temp_store = (**store).clone();
        let result = super::execute_action(action, &mut temp_store, functions)?;
        *store = Rc::new(temp_store);
        Ok(result)
    }

    fn eval_src(src: &str) -> Result<Value, MizuError> {
        let wrapper = format!("  f() : {src}\n");
        let (fns, interner) = single_fn(&wrapper)?;
        let f_sym = interner
            .get("f")
            .ok_or_else(|| MizuError::ParseError("f not found in interner".to_string()))?;
        let store = Rc::new(VariableStore::with_interner(interner));
        evaluate(&fns[&f_sym].body, &store, &fns)
    }

    // ────────────────────────────────────────────────────────────────────────
    // Lexer / parser — happy paths
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn parse_inline_function_no_args() {
        let (fns, interner) = single_fn("    pi() : 3.14159\n").unwrap();
        let pi_sym = interner.get("pi").unwrap();
        assert!(fns.contains_key(&pi_sym));
        let f = &fns[&pi_sym];
        assert!(f.params.is_empty());
        assert_eq!(f.body, Expr::Literal(Value::Float(3.14159)));
    }

    #[test]
    fn parse_inline_function_single_num_param() {
        let (fns, interner) = single_fn("    vat(p: num) : p * 1.22\n").unwrap();
        let vat_sym = interner.get("vat").unwrap();
        let f = &fns[&vat_sym];
        let p_sym = interner.get("p").unwrap();
        assert_eq!(f.params, vec![(p_sym, Some(ValueType::Num))]);
        // Body should be BinaryOp(Variable(p_sym), Mul, Literal(1.22))
        assert!(matches!(&f.body, Expr::BinaryOp { op: BinOp::Mul, .. }));
    }

    #[test]
    fn parse_inline_function_two_params() {
        let (fns, interner) = single_fn("    add(a: num, b: num) : a + b\n").unwrap();
        let add_sym = interner.get("add").unwrap();
        let f = &fns[&add_sym];
        assert_eq!(f.params.len(), 2);
        let a_sym = interner.get("a").unwrap();
        let b_sym = interner.get("b").unwrap();
        assert_eq!(f.params[0], (a_sym, Some(ValueType::Num)));
        assert_eq!(f.params[1], (b_sym, Some(ValueType::Num)));
    }

    #[test]
    fn parse_inline_string_param() {
        let (fns, interner) = single_fn("    greet(name: string) : name\n").unwrap();
        let greet_sym = interner.get("greet").unwrap();
        let f = &fns[&greet_sym];
        let name_sym = interner.get("name").unwrap();
        assert_eq!(f.params[0], (name_sym, Some(ValueType::Str)));
    }

    #[test]
    fn parse_inline_bool_param() {
        let (fns, interner) = single_fn("    id_bool(b: bool) : b\n").unwrap();
        let sym = interner.get("id_bool").unwrap();
        let f = &fns[&sym];
        let b_sym = interner.get("b").unwrap();
        assert_eq!(f.params[0], (b_sym, Some(ValueType::Bool)));
    }

    #[test]
    fn parse_inline_list_param() {
        let (fns, interner) = single_fn("    first(items: list) : items\n").unwrap();
        let sym = interner.get("first").unwrap();
        let f = &fns[&sym];
        let items_sym = interner.get("items").unwrap();
        assert_eq!(f.params[0], (items_sym, Some(ValueType::List)));
    }

    #[test]
    fn parse_multiple_functions() {
        let src = r"
    double(x: num) : x * 2
    triple(x: num) : x * 3
";
        let (fns, interner) = single_fn(src).unwrap();
        assert_eq!(fns.len(), 2);
        assert!(
            interner
                .get("double")
                .map_or(false, |s| fns.contains_key(&s))
        );
        assert!(
            interner
                .get("triple")
                .map_or(false, |s| fns.contains_key(&s))
        );
    }

    #[test]
    fn parse_multiline_function_with_binding() {
        let src = r"
    total(price: num, qty: num)
        netto = price * qty
        netto * 1.22
";
        let (fns, interner) = single_fn(src).unwrap();
        let total_sym = interner.get("total").unwrap();
        let f = &fns[&total_sym];
        let netto_sym = interner.get("netto").unwrap();
        // Body should be Let { name: netto_sym, value: price * qty, body: netto * 1.22 }
        assert!(matches!(&f.body, Expr::Let { name, .. } if *name == netto_sym));
    }

    #[test]
    fn parse_function_calling_another() {
        let src = r"
    vat(p: num) : p * 1.22
    total(p: num, q: num) : vat(p * q)
";
        let (fns, interner) = single_fn(src).unwrap();
        assert_eq!(fns.len(), 2);
        let total_sym = interner.get("total").unwrap();
        let vat_sym = interner.get("vat").unwrap();
        let body = &fns[&total_sym].body;
        assert!(matches!(body, Expr::FunctionCall { name, .. } if *name == vat_sym));
    }

    #[test]
    fn parse_empty_logic_block() {
        let fns = parse_logic("", &mut StringInterner::new()).unwrap();
        assert!(fns.is_empty());
    }

    #[test]
    fn parse_logic_blank_only() {
        let fns = parse_logic("   \n  \n", &mut StringInterner::new()).unwrap();
        assert!(fns.is_empty());
    }

    // ────────────────────────────────────────────────────────────────────────
    // Operator precedence (Pratt parser correctness)
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn pratt_mul_before_add() {
        // `2 + 3 * 4` should parse as `2 + (3 * 4)`, not `(2 + 3) * 4`.
        let (fns, interner) = single_fn("    f() : 2 + 3 * 4\n").unwrap();
        let f_sym = interner.get("f").unwrap();
        let store = Rc::new(VariableStore::with_interner(interner));
        let result = evaluate(&fns[&f_sym].body, &store, &fns).unwrap();
        // 2 + 12 = 14
        assert_eq!(result, Value::Int(14));
    }

    #[test]
    fn pratt_parentheses_override_precedence() {
        // `(2 + 3) * 4` should be 20.
        let (fns, interner) = single_fn("    f() : (2 + 3) * 4\n").unwrap();
        let f_sym = interner.get("f").unwrap();
        let store = Rc::new(VariableStore::with_interner(interner));
        let result = evaluate(&fns[&f_sym].body, &store, &fns).unwrap();
        assert_eq!(result, Value::Int(20));
    }

    #[test]
    fn pratt_left_associativity_subtraction() {
        // `10 - 3 - 2` should be `(10 - 3) - 2 = 5`, NOT `10 - (3 - 2) = 9`.
        let (fns, interner) = single_fn("    f() : 10 - 3 - 2\n").unwrap();
        let f_sym = interner.get("f").unwrap();
        let store = Rc::new(VariableStore::with_interner(interner));
        let result = evaluate(&fns[&f_sym].body, &store, &fns).unwrap();
        assert_eq!(result, Value::Int(5));
    }

    #[test]
    fn pratt_left_associativity_division() {
        // `12 / 6 / 2` → `(12/6)/2 = 1`.
        let (fns, interner) = single_fn("    f() : 12 / 6 / 2\n").unwrap();
        let f_sym = interner.get("f").unwrap();
        let store = Rc::new(VariableStore::with_interner(interner));
        let result = evaluate(&fns[&f_sym].body, &store, &fns).unwrap();
        assert_eq!(result, Value::Int(1));
    }

    #[test]
    fn pratt_complex_expression() {
        // `1 + 2 * 3 + 4 / 2` = `1 + 6 + 2 = 9`
        let (fns, interner) = single_fn("    f() : 1 + 2 * 3 + 4 / 2\n").unwrap();
        let f_sym = interner.get("f").unwrap();
        let store = Rc::new(VariableStore::with_interner(interner));
        let result = evaluate(&fns[&f_sym].body, &store, &fns).unwrap();
        assert_eq!(result, Value::Int(9));
    }

    // ────────────────────────────────────────────────────────────────────────
    // Evaluator — happy paths
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn evaluate_literal_num() {
        let expr = Expr::Literal(Value::Float(42.0));
        let store = Rc::new(VariableStore::new());
        let fns = FxHashMap::default();
        assert_eq!(evaluate(&expr, &store, &fns).unwrap(), Value::Float(42.0));
    }

    #[test]
    fn evaluate_literal_bool() {
        let expr = Expr::Literal(Value::Bool(true));
        let store = Rc::new(VariableStore::new());
        let fns = FxHashMap::default();
        assert_eq!(evaluate(&expr, &store, &fns).unwrap(), Value::Bool(true));
    }

    #[test]
    fn evaluate_variable_lookup() {
        let mut store = VariableStore::new();
        store.set("x", 7.0_f64);
        let x_sym = store.interner.get("x").unwrap();
        let store = Rc::new(store);
        let expr = Expr::Variable(x_sym);
        let fns = FxHashMap::default();
        assert_eq!(evaluate(&expr, &store, &fns).unwrap(), Value::Float(7.0));
    }

    #[test]
    fn evaluate_addition() {
        let expr = Expr::BinaryOp {
            left: Box::new(Expr::Literal(Value::Float(3.0))),
            op: BinOp::Add,
            right: Box::new(Expr::Literal(Value::Float(4.0))),
        };
        let store = Rc::new(VariableStore::new());
        let fns = FxHashMap::default();
        assert_eq!(evaluate(&expr, &store, &fns).unwrap(), Value::Float(7.0));
    }

    #[test]
    fn evaluate_subtraction() {
        let expr = Expr::BinaryOp {
            left: Box::new(Expr::Literal(Value::Float(10.0))),
            op: BinOp::Sub,
            right: Box::new(Expr::Literal(Value::Float(3.5))),
        };
        let store = Rc::new(VariableStore::new());
        let fns = FxHashMap::default();
        assert_eq!(evaluate(&expr, &store, &fns).unwrap(), Value::Float(6.5));
    }

    #[test]
    fn evaluate_multiplication() {
        let expr = Expr::BinaryOp {
            left: Box::new(Expr::Literal(Value::Float(6.0))),
            op: BinOp::Mul,
            right: Box::new(Expr::Literal(Value::Float(7.0))),
        };
        let store = Rc::new(VariableStore::new());
        let fns = FxHashMap::default();
        assert_eq!(evaluate(&expr, &store, &fns).unwrap(), Value::Float(42.0));
    }

    #[test]
    fn evaluate_division() {
        let expr = Expr::BinaryOp {
            left: Box::new(Expr::Literal(Value::Float(15.0))),
            op: BinOp::Div,
            right: Box::new(Expr::Literal(Value::Float(3.0))),
        };
        let store = Rc::new(VariableStore::new());
        let fns = FxHashMap::default();
        assert_eq!(evaluate(&expr, &store, &fns).unwrap(), Value::Float(5.0));
    }

    #[test]
    fn evaluate_string_concatenation() {
        let expr = Expr::BinaryOp {
            left: Box::new(Expr::Literal(Value::String(std::sync::Arc::from(
                "Hello, ",
            )))),
            op: BinOp::Add,
            right: Box::new(Expr::Literal(Value::String(std::sync::Arc::from("Mizu!")))),
        };
        let store = Rc::new(VariableStore::new());
        let fns = FxHashMap::default();
        assert_eq!(
            evaluate(&expr, &store, &fns).unwrap(),
            Value::String(std::sync::Arc::from("Hello, Mizu!"))
        );
    }

    #[test]
    fn evaluate_inline_function_call() {
        let src = "    vat(p: num) : p * 1.22\n";
        let (fns, interner) = single_fn(src).unwrap();
        let vat_sym = interner.get("vat").unwrap();
        let mut store = VariableStore::with_interner(interner);
        store.set("p", 100.0_f64);
        let store = Rc::new(store);
        let call_expr = Expr::FunctionCall {
            name: vat_sym,
            args: vec![Expr::Literal(Value::Float(100.0))],
        };
        let result = evaluate(&call_expr, &store, &fns).unwrap();
        // 100 * 1.22 = 122
        assert_eq!(result, Value::Float(122.0));
    }

    #[test]
    fn evaluate_function_calling_function() {
        let src = r"
    double(x: num) : x * 2
    quadruple(x: num) : double(double(x))
";
        let (fns, interner) = single_fn(src).unwrap();
        let quadruple_sym = interner.get("quadruple").unwrap();
        let call_expr = Expr::FunctionCall {
            name: quadruple_sym,
            args: vec![Expr::Literal(Value::Float(3.0))],
        };
        let store = Rc::new(VariableStore::with_interner(interner));
        let result = evaluate(&call_expr, &store, &fns).unwrap();
        // 3 * 4 = 12
        assert_eq!(result, Value::Float(12.0));
    }

    #[test]
    fn evaluate_multiline_function_with_let_binding() {
        let src = r"
    total(price: num, qty: num)
        netto = price * qty
        netto * 1.22
";
        let (fns, interner) = single_fn(src).unwrap();
        let total_sym = interner.get("total").unwrap();
        let call_expr = Expr::FunctionCall {
            name: total_sym,
            args: vec![
                Expr::Literal(Value::Float(10.0)),
                Expr::Literal(Value::Float(3.0)),
            ],
        };
        let store = Rc::new(VariableStore::with_interner(interner));
        let result = evaluate(&call_expr, &store, &fns).unwrap();
        // netto = 10 * 3 = 30, result = 30 * 1.22 = 36.6
        let expected = 30.0_f64 * 1.22_f64;
        match result {
            Value::Float(n) => {
                assert!((n - expected).abs() < 1e-10, "got {n}, expected {expected}")
            }
            other => panic!("expected Float, got {other:?}"),
        }
    }

    #[test]
    fn evaluate_function_with_store_variables() {
        // Outer store values should NOT bleed into the function's local scope.
        let src = "    area(w: num, h: num) : w * h\n";
        let (fns, interner) = single_fn(src).unwrap();
        let area_sym = interner.get("area").unwrap();
        let mut outer_store = VariableStore::with_interner(interner);
        outer_store.set("w", 999.0_f64); // should be ignored inside the function
        let outer_store = Rc::new(outer_store);
        let call_expr = Expr::FunctionCall {
            name: area_sym,
            args: vec![
                Expr::Literal(Value::Float(5.0)),
                Expr::Literal(Value::Float(4.0)),
            ],
        };
        // Function arguments override the outer store inside the function body.
        let result = evaluate(&call_expr, &outer_store, &fns).unwrap();
        assert_eq!(result, Value::Float(20.0));
    }

    // ────────────────────────────────────────────────────────────────────────
    // DAG anti-recursion (security / Turing-completeness guardrail)
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn error_direct_recursion_rejected() {
        // `f` calls itself → cycle A → A.
        let src = "    f(x: num) : f(x)\n";
        let result = parse_logic(src, &mut StringInterner::new());
        assert!(
            matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("cycle")),
            "expected cycle detection error, got: {result:?}"
        );
    }

    #[test]
    fn error_mutual_recursion_rejected() {
        // `ping` calls `pong`, `pong` calls `ping` → cycle A → B → A.
        let src = r"
    ping(x: num) : pong(x)
    pong(x: num) : ping(x)
";
        let result = parse_logic(src, &mut StringInterner::new());
        assert!(
            matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("cycle")),
            "expected mutual recursion error, got: {result:?}"
        );
    }

    #[test]
    fn dag_accepts_chain_a_calls_b() {
        // `b` is defined first (in-degree 0), `a` calls `b` — acyclic.
        let src = r"
    b(x: num) : x * 2
    a(x: num) : b(x)
";
        let fns = parse_logic(src, &mut StringInterner::new());
        assert!(fns.is_ok(), "expected Ok for acyclic DAG, got: {fns:?}");
    }

    #[test]
    fn dag_accepts_three_level_chain() {
        let src = r"
    leaf(x: num) : x
    mid(x: num) : leaf(x) * 2
    root(x: num) : mid(x) + 1
";
        assert!(parse_logic(src, &mut StringInterner::new()).is_ok());
    }

    // ────────────────────────────────────────────────────────────────────────
    // Type error paths
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn error_num_plus_bool_is_type_error() {
        let expr = Expr::BinaryOp {
            left: Box::new(Expr::Literal(Value::Float(1.0))),
            op: BinOp::Add,
            right: Box::new(Expr::Literal(Value::Bool(true))),
        };
        let store = Rc::new(VariableStore::new());
        let fns = FxHashMap::default();
        let result = evaluate(&expr, &store, &fns);
        assert!(
            matches!(result, Err(MizuError::TypeError { .. })),
            "expected TypeError, got: {result:?}"
        );
    }

    #[test]
    fn error_num_mul_string_is_type_error() {
        let expr = Expr::BinaryOp {
            left: Box::new(Expr::Literal(Value::Float(2.0))),
            op: BinOp::Mul,
            right: Box::new(Expr::Literal(Value::String(std::sync::Arc::from("oops")))),
        };
        let store = Rc::new(VariableStore::new());
        let fns = FxHashMap::default();
        let result = evaluate(&expr, &store, &fns);
        assert!(matches!(result, Err(MizuError::TypeError { .. })));
    }

    #[test]
    fn error_bool_sub_num_is_type_error() {
        let expr = Expr::BinaryOp {
            left: Box::new(Expr::Literal(Value::Bool(true))),
            op: BinOp::Sub,
            right: Box::new(Expr::Literal(Value::Float(1.0))),
        };
        let store = Rc::new(VariableStore::new());
        let fns = FxHashMap::default();
        let result = evaluate(&expr, &store, &fns);
        assert!(matches!(result, Err(MizuError::TypeError { .. })));
    }

    #[test]
    fn error_wrong_argument_type_for_function() {
        // `vat` expects `num`, but receives `bool`.
        let src = "    vat(p: num) : p * 1.22\n";
        let (fns, interner) = single_fn(src).unwrap();
        let vat_sym = interner.get("vat").unwrap();
        let call_expr = Expr::FunctionCall {
            name: vat_sym,
            args: vec![Expr::Literal(Value::Bool(true))],
        };
        let store = Rc::new(VariableStore::with_interner(interner));
        let result = evaluate(&call_expr, &store, &fns);
        assert!(
            matches!(result, Err(MizuError::TypeError { .. })),
            "expected TypeError for wrong argument type, got: {result:?}"
        );
    }

    #[test]
    fn error_wrong_arity_too_few() {
        let src = "    add(a: num, b: num) : a + b\n";
        let (fns, interner) = single_fn(src).unwrap();
        let add_sym = interner.get("add").unwrap();
        let call_expr = Expr::FunctionCall {
            name: add_sym,
            args: vec![Expr::Literal(Value::Float(1.0))],
        };
        let store = Rc::new(VariableStore::with_interner(interner));
        let result = evaluate(&call_expr, &store, &fns);
        assert!(
            matches!(result, Err(MizuError::ParseError(ref m)) if m.contains("argument")),
            "expected arity error, got: {result:?}"
        );
    }

    #[test]
    fn error_wrong_arity_too_many() {
        let src = "    inc(x: num) : x + 1\n";
        let (fns, interner) = single_fn(src).unwrap();
        let inc_sym = interner.get("inc").unwrap();
        let call_expr = Expr::FunctionCall {
            name: inc_sym,
            args: vec![
                Expr::Literal(Value::Float(1.0)),
                Expr::Literal(Value::Float(2.0)),
            ],
        };
        let store = Rc::new(VariableStore::with_interner(interner));
        let result = evaluate(&call_expr, &store, &fns);
        assert!(matches!(result, Err(MizuError::ParseError(_))));
    }

    #[test]
    fn error_undefined_function_call() {
        let mut interner = StringInterner::new();
        let ghost_sym = interner.get_or_intern("ghost");
        let call_expr = Expr::FunctionCall {
            name: ghost_sym,
            args: vec![],
        };
        let store = Rc::new(VariableStore::with_interner(interner));
        let fns = FxHashMap::default();
        let result = evaluate(&call_expr, &store, &fns);
        assert!(
            matches!(result, Err(MizuError::ParseError(ref m)) if m.contains("ghost")),
            "expected undefined-function error, got: {result:?}"
        );
    }

    #[test]
    fn error_variable_not_found() {
        let mut interner = StringInterner::new();
        let missing_sym = interner.get_or_intern("missing");
        let expr = Expr::Variable(missing_sym);
        let store = Rc::new(VariableStore::with_interner(interner));
        let fns = FxHashMap::default();
        let result = evaluate(&expr, &store, &fns);
        assert!(
            matches!(result, Err(MizuError::VariableNotFound(_))),
            "expected VariableNotFound, got: {result:?}"
        );
    }

    // ────────────────────────────────────────────────────────────────────────
    // Parser failure paths
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn unannotated_param_is_valid() {
        // Parameters without `: type` are now legal — they accept any value.
        let result = parse_logic("    id(x) : x\n", &mut StringInterner::new());
        assert!(
            result.is_ok(),
            "unannotated param should parse successfully, got: {result:?}"
        );
    }

    #[test]
    fn error_unknown_type_keyword() {
        let result = parse_logic("    f(x: integer) : x\n", &mut StringInterner::new());
        assert!(
            matches!(result, Err(MizuError::ParseError(ref m)) if m.contains("unknown type")),
            "expected unknown-type error, got: {result:?}"
        );
    }

    #[test]
    fn error_function_without_body() {
        // Header with `:` but nothing after it.
        let result = parse_logic("    f(x: num) :\n", &mut StringInterner::new());
        assert!(
            matches!(result, Err(MizuError::ParseError(_))),
            "expected ParseError for body-less function, got: {result:?}"
        );
    }

    #[test]
    fn error_multiline_last_line_is_binding() {
        // The last line of a multi-line function must be a bare expression.
        let src = r"
    f(x: num)
        a = x * 2
        b = a + 1
";
        let result = parse_logic(src, &mut StringInterner::new());
        assert!(
            matches!(result, Err(MizuError::ParseError(_))),
            "expected ParseError when last body line is a binding, got: {result:?}"
        );
    }

    #[test]
    fn test_case_insensitive_types_and_aliases() {
        let src = r"
    greet(name: Str) : name
    VAT(p: Number) : p * 1.22
    check(b: Boolean) : b
";
        let mut interner = StringInterner::new();
        let result = parse_logic(src, &mut interner).unwrap();
        assert!(
            interner
                .get("greet")
                .map_or(false, |s| result.contains_key(&s))
        );
        assert!(
            interner
                .get("VAT")
                .map_or(false, |s| result.contains_key(&s))
        );
        assert!(
            interner
                .get("check")
                .map_or(false, |s| result.contains_key(&s))
        );

        let greet_sym = interner.get("greet").unwrap();
        let greet_fn = &result[&greet_sym];
        assert_eq!(greet_fn.params[0].1, Some(ValueType::Str));

        let vat_sym = interner.get("VAT").unwrap();
        let vat_fn = &result[&vat_sym];
        assert_eq!(vat_fn.params[0].1, Some(ValueType::Num));

        let check_sym = interner.get("check").unwrap();
        let check_fn = &result[&check_sym];
        assert_eq!(check_fn.params[0].1, Some(ValueType::Bool));
    }


    #[test]
    fn execute_action_assignment_mutates_store() {
        let mut store = VariableStore::new();
        store.set("count", 1.0_f64);
        let mut store = Rc::new(store);
        let functions = FxHashMap::default();

        let action = parse_action("count = count + 1", &mut StringInterner::new()).unwrap();
        let mutated = execute_action(&action, &mut store, &functions).unwrap();
        assert!(mutated);
        assert_eq!(*store.get("count").unwrap(), Value::Float(2.0));
    }

    #[test]
    fn execute_action_pure_expression_no_mutation() {
        let mut store = VariableStore::new();
        store.set("count", 1.0_f64);
        let mut store = Rc::new(store);
        let functions = FxHashMap::default();

        let action = parse_action("count + 1", &mut StringInterner::new()).unwrap();
        let mutated = execute_action(&action, &mut store, &functions).unwrap();
        assert!(!mutated);
        // Ensure count wasn't mutated
        assert_eq!(*store.get("count").unwrap(), Value::Float(1.0));
    }

    #[test]
    fn parse_action_invalid_assignment() {
        let err = parse_action("= 5", &mut StringInterner::new()).unwrap_err();
        assert!(matches!(err, MizuError::ParseError(_)));
    }

    #[test]
    fn parse_variable_definition() {
        let mut interner = StringInterner::new();
        let fns = parse_logic("    count = 10\n", &mut interner).unwrap();
        let count_sym = interner.get("count").unwrap();
        assert!(fns.contains_key(&count_sym));
        let f = &fns[&count_sym];
        assert!(f.params.is_empty());
        assert_eq!(f.body, Expr::Literal(Value::Int(10)));
    }

    #[test]
    fn error_variable_fallback_no_implicit_call() {
        let mut interner = StringInterner::new();
        let fns = parse_logic("    count = 10\n", &mut interner).unwrap();
        let count_sym = interner.get("count").unwrap();
        let store = Rc::new(VariableStore::with_interner(interner));
        let expr = Expr::Variable(count_sym);
        let result = evaluate(&expr, &store, &fns);
        assert!(
            matches!(result, Err(MizuError::VariableNotFound(ref name)) if name == "count"),
            "expected VariableNotFound for count, got: {result:?}"
        );
    }

    #[test]
    fn error_recursive_variable_definition_rejected() {
        // count = count + 1 is a cycle
        let src = "    count = count + 1\n";
        let result = parse_logic(src, &mut StringInterner::new());
        assert!(
            matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("cycle")),
            "expected cycle error, got: {result:?}"
        );
    }

    #[test]
    fn error_mutually_recursive_variables_rejected() {
        let src = r"
    a = b + 1
    b = a + 1
";
        let result = parse_logic(src, &mut StringInterner::new());
        assert!(
            matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("cycle")),
            "expected mutual recursion error, got: {result:?}"
        );
    }

    #[test]
    fn test_cooperative_checkpointing_timeout() {
        use crate::core::types::{MAX_INSTRUCTIONS, StateMachine};

        // Pre-saturate the instruction counter to MAX_INSTRUCTIONS.
        // The very next call to `evaluate` increments it to MAX_INSTRUCTIONS + 1,
        // triggering the `instruction_count > MAX_INSTRUCTIONS` check immediately.
        // This avoids building a deep recursive tree that would overflow the call
        // stack in debug mode before the instruction limit is ever reached.
        let mut sm = StateMachine::new();
        sm.instruction_count = MAX_INSTRUCTIONS;

        let interner = crate::core::types::StringInterner::new();
        let fns = FxHashMap::default();
        let expr = Expr::Literal(Value::Int(1));

        let res = sm.evaluate(&expr, 0, &fns, &interner);
        assert!(
            matches!(res, Err(MizuError::Timeout)),
            "expected Timeout, got: {res:?}"
        );
    }

    #[test]
    fn test_instruction_budget_resets_per_action() {
        // Verify that execute_action resets instruction_count to 0 before each evaluation,
        // so two consecutive actions each get the full MAX_INSTRUCTIONS budget.
        use crate::core::types::MAX_INSTRUCTIONS;

        let mut store = VariableStore::new();
        let fns = FxHashMap::default();
        let mut interner = crate::core::types::StringInterner::new();
        let x_sym = interner.get_or_intern("x");
        store.interner = interner;
        store.state_machine.set_global(x_sym, Value::Int(0));

        // First action — must succeed even if counter was near-exhausted from a prior call.
        store.state_machine.instruction_count = MAX_INSTRUCTIONS - 1;
        let action1 = Action::Assign {
            target: "x".to_string(),
            expr: Expr::Literal(Value::Int(1)),
        };
        let r1 = super::execute_action(&action1, &mut store, &fns);
        assert!(
            r1.is_ok(),
            "first action should succeed (counter reset to 0): {r1:?}"
        );

        // Second action — counter was reset by execute_action, must also succeed.
        let action2 = Action::Assign {
            target: "x".to_string(),
            expr: Expr::Literal(Value::Int(2)),
        };
        let r2 = super::execute_action(&action2, &mut store, &fns);
        assert!(
            r2.is_ok(),
            "second action should succeed (counter reset to 0): {r2:?}"
        );
    }

    #[test]
    fn test_flat_state_machine_scoping() {
        use crate::core::types::StateMachine;

        let mut sm = StateMachine::new();
        let mut interner = crate::core::types::StringInterner::new();
        let fns = FxHashMap::default();

        // Set global variables
        let x_sym = interner.get_or_intern("x");
        let y_sym = interner.get_or_intern("y");
        sm.set_global(x_sym, Value::Int(10));
        sm.set_global(y_sym, Value::Int(20));

        // Evaluate an expression shadowing 'x' using Let binding:
        // let x = 15 in x + y
        let expr = Expr::Let {
            name: x_sym,
            value: Box::new(Expr::Literal(Value::Int(15))),
            body: Box::new(Expr::BinaryOp {
                left: Box::new(Expr::Variable(x_sym)),
                op: BinOp::Add,
                right: Box::new(Expr::Variable(y_sym)),
            }),
        };

        let res = sm.evaluate(&expr, 0, &fns, &interner).unwrap();
        assert_eq!(res, Value::Int(35));
    }

    // ────────────────────────────────────────────────────────────────────────
    // Comparison operators
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn compare_int_eq_true() {
        assert_eq!(eval_src("3 == 3").unwrap(), Value::Bool(true));
    }

    #[test]
    fn compare_int_eq_false() {
        assert_eq!(eval_src("3 == 4").unwrap(), Value::Bool(false));
    }

    #[test]
    fn compare_int_ne() {
        assert_eq!(eval_src("3 != 4").unwrap(), Value::Bool(true));
        assert_eq!(eval_src("3 != 3").unwrap(), Value::Bool(false));
    }

    #[test]
    fn compare_int_lt_gt() {
        assert_eq!(eval_src("2 < 5").unwrap(), Value::Bool(true));
        assert_eq!(eval_src("5 < 2").unwrap(), Value::Bool(false));
        assert_eq!(eval_src("5 > 2").unwrap(), Value::Bool(true));
        assert_eq!(eval_src("2 > 5").unwrap(), Value::Bool(false));
    }

    #[test]
    fn compare_int_le_ge() {
        assert_eq!(eval_src("3 <= 3").unwrap(), Value::Bool(true));
        assert_eq!(eval_src("2 <= 3").unwrap(), Value::Bool(true));
        assert_eq!(eval_src("4 <= 3").unwrap(), Value::Bool(false));
        assert_eq!(eval_src("3 >= 3").unwrap(), Value::Bool(true));
        assert_eq!(eval_src("4 >= 3").unwrap(), Value::Bool(true));
        assert_eq!(eval_src("2 >= 3").unwrap(), Value::Bool(false));
    }

    #[test]
    fn compare_float_int_mixed() {
        assert_eq!(eval_src("3.0 == 3").unwrap(), Value::Bool(true));
        assert_eq!(eval_src("3 < 3.5").unwrap(), Value::Bool(true));
        assert_eq!(eval_src("4 > 3.5").unwrap(), Value::Bool(true));
    }

    #[test]
    fn compare_strings_eq_ne() {
        assert_eq!(
            eval_src(r#""hello" == "hello""#).unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            eval_src(r#""hello" == "world""#).unwrap(),
            Value::Bool(false)
        );
        assert_eq!(
            eval_src(r#""hello" != "world""#).unwrap(),
            Value::Bool(true)
        );
    }

    #[test]
    fn compare_bools_eq() {
        assert_eq!(eval_src("true == true").unwrap(), Value::Bool(true));
        assert_eq!(eval_src("true == false").unwrap(), Value::Bool(false));
    }

    // ────────────────────────────────────────────────────────────────────────
    // Logical operators (&&, ||, !)
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn logical_and() {
        assert_eq!(eval_src("true && true").unwrap(), Value::Bool(true));
        assert_eq!(eval_src("true && false").unwrap(), Value::Bool(false));
        assert_eq!(eval_src("false && false").unwrap(), Value::Bool(false));
    }

    #[test]
    fn logical_or() {
        assert_eq!(eval_src("true || false").unwrap(), Value::Bool(true));
        assert_eq!(eval_src("false || false").unwrap(), Value::Bool(false));
        assert_eq!(eval_src("false || true").unwrap(), Value::Bool(true));
    }

    #[test]
    fn logical_not() {
        assert_eq!(eval_src("!true").unwrap(), Value::Bool(false));
        assert_eq!(eval_src("!false").unwrap(), Value::Bool(true));
    }

    #[test]
    fn logical_combined_precedence() {
        // `3 > 2 && 1 < 5` → `true && true` → `true`
        assert_eq!(eval_src("3 > 2 && 1 < 5").unwrap(), Value::Bool(true));
        // `!false || false` → `true || false` → `true`
        assert_eq!(eval_src("!false || false").unwrap(), Value::Bool(true));
    }

    // ────────────────────────────────────────────────────────────────────────
    // Conditional expressions: if/then/else and ternary ?:
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn if_then_else_true_branch() {
        assert_eq!(eval_src("if true then 1 else 2").unwrap(), Value::Int(1));
    }

    #[test]
    fn if_then_else_false_branch() {
        assert_eq!(eval_src("if false then 1 else 2").unwrap(), Value::Int(2));
    }

    #[test]
    fn if_then_else_with_expression_condition() {
        assert_eq!(
            eval_src("if 3 > 2 then 10 else 20").unwrap(),
            Value::Int(10)
        );
        assert_eq!(
            eval_src("if 1 > 2 then 10 else 20").unwrap(),
            Value::Int(20)
        );
    }

    #[test]
    fn if_then_else_returns_string() {
        assert_eq!(
            eval_src(r#"if true then "si" else "no""#).unwrap(),
            Value::String(std::sync::Arc::from("si"))
        );
    }

    #[test]
    fn ternary_true_branch() {
        assert_eq!(eval_src("true ? 1 : 2").unwrap(), Value::Int(1));
    }

    #[test]
    fn ternary_false_branch() {
        assert_eq!(eval_src("false ? 1 : 2").unwrap(), Value::Int(2));
    }

    #[test]
    fn ternary_with_expression_condition() {
        assert_eq!(eval_src("5 > 3 ? 100 : 200").unwrap(), Value::Int(100));
        assert_eq!(eval_src("1 == 2 ? 100 : 200").unwrap(), Value::Int(200));
    }

    #[test]
    fn ternary_right_associative() {
        // `true ? 1 : false ? 2 : 3` → `true ? 1 : (false ? 2 : 3)` → 1
        assert_eq!(eval_src("true ? 1 : false ? 2 : 3").unwrap(), Value::Int(1));
        // `false ? 1 : false ? 2 : 3` → `false ? 1 : (false ? 2 : 3)` → 3
        assert_eq!(
            eval_src("false ? 1 : false ? 2 : 3").unwrap(),
            Value::Int(3)
        );
    }

    #[test]
    fn if_else_non_bool_condition_is_type_error() {
        let err = eval_src("if 42 then 1 else 2").unwrap_err();
        assert!(matches!(err, MizuError::TypeError { .. }));
    }

    #[test]
    fn ternary_non_bool_condition_is_type_error() {
        let err = eval_src(r#""yes" ? 1 : 2"#).unwrap_err();
        assert!(matches!(err, MizuError::TypeError { .. }));
    }

    #[test]
    fn if_then_missing_else_is_parse_error() {
        let src = "doppio(n: num) : if n > 0 then n";
        let err = parse_logic(src, &mut StringInterner::new()).unwrap_err();
        assert!(matches!(err, MizuError::ParseError(_)));
    }

    #[test]
    fn if_else_used_in_function_body() {
        let src = "
absolute_value(n: num) : if n >= 0 then n else 0 - n
";
        let mut interner = StringInterner::new();
        let fns = parse_logic(src.trim(), &mut interner).unwrap();
        let va_sym = interner.get("absolute_value").unwrap();
        let mut store = VariableStore::with_interner(interner);
        let pos = fns[&va_sym].body.clone();
        store.set("n", Value::Float(5.0));
        let v = store
            .state_machine
            .evaluate(&pos, 0, &fns, &store.interner)
            .unwrap();
        // just verify the function compiles — full eval needs param binding
        let _ = v;
        // Smoke test: parse succeeds and body is IfElse
        assert!(matches!(fns[&va_sym].body, Expr::IfElse { .. }));
    }

    // ────────────────────────────────────────────────────────────────────────
    // Type-error failure paths for new operators
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn error_lt_on_strings_is_type_error() {
        let result = eval_src(r#""a" < "b""#);
        assert!(matches!(result, Err(MizuError::TypeError { .. })));
    }

    #[test]
    fn error_and_on_nums_is_type_error() {
        let result = eval_src("1 && 0");
        assert!(matches!(result, Err(MizuError::TypeError { .. })));
    }

    #[test]
    fn error_not_on_num_is_type_error() {
        let result = eval_src("!42");
        assert!(matches!(result, Err(MizuError::TypeError { .. })));
    }

    // ────────────────────────────────────────────────────────────────────────
    // parse_action must not confuse `==` with assignment
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn parse_action_comparison_is_eval_not_assign() {
        let action = parse_action("x == 5", &mut StringInterner::new()).unwrap();
        assert!(
            matches!(action, Action::Eval(_)),
            "expected Eval for comparison expression, got: {action:?}"
        );
    }

    #[test]
    fn parse_action_assignment_after_comparison_operators() {
        // `result = a != b` must parse as Assign{target="result", expr=Ne(a, b)}
        // (won't work without store variables, just check it parses as Assign)
        let action = parse_action("flag = true", &mut StringInterner::new()).unwrap();
        assert!(
            matches!(action, Action::Assign { ref target, .. } if target == "flag"),
            "expected Assign, got: {action:?}"
        );
    }

    // ────────────────────────────────────────────────────────────────────────
    // Cursor-exhaustion: trailing tokens after a complete expression
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn parse_action_trailing_ident_after_assign_is_error() {
        // Simulates: `button click -> count = count + 1 class "btn"`
        // The expression `count + 1` is valid, but `class` is a leftover token.
        let err = parse_action("count = count + 1 class", &mut StringInterner::new()).unwrap_err();
        assert!(
            matches!(err, MizuError::ParseError(ref msg) if msg.contains("unexpected token")),
            "expected ParseError about unexpected token, got: {err:?}"
        );
    }

    #[test]
    fn parse_action_trailing_string_after_assign_is_error() {
        // `count = count + 1 "leftover"` — trailing string literal
        let err = parse_action(
            r#"count = count + 1 "leftover""#,
            &mut StringInterner::new(),
        )
        .unwrap_err();
        assert!(
            matches!(err, MizuError::ParseError(ref msg) if msg.contains("unexpected token")),
            "expected ParseError about unexpected token, got: {err:?}"
        );
    }

    #[test]
    fn parse_action_trailing_token_after_eval_is_error() {
        // `myFn() class "x"` — Eval action with trailing junk
        let err = parse_action("true class", &mut StringInterner::new()).unwrap_err();
        assert!(
            matches!(err, MizuError::ParseError(ref msg) if msg.contains("unexpected token")),
            "expected ParseError about unexpected token, got: {err:?}"
        );
    }

    #[test]
    fn parse_action_trailing_token_after_navigate_is_error() {
        // `navigate "url" class "x"` — URL parsed, then junk
        let err = parse_action(
            r#"navigate "mizu://host/page" class "x""#,
            &mut StringInterner::new(),
        )
        .unwrap_err();
        assert!(
            matches!(err, MizuError::ParseError(ref msg) if msg.contains("unexpected token")),
            "expected ParseError about unexpected token, got: {err:?}"
        );
    }

    #[test]
    fn parse_action_clean_assign_still_ok() {
        // Regression: valid action must still parse without error
        let action = parse_action("count = count + 1", &mut StringInterner::new()).unwrap();
        assert!(matches!(action, Action::Assign { ref target, .. } if target == "count"));
    }

    #[test]
    fn parse_action_clean_navigate_still_ok() {
        let action =
            parse_action(r#"navigate "mizu://host/page""#, &mut StringInterner::new()).unwrap();
        assert!(matches!(action, Action::Navigate { .. }));
    }

    #[test]
    fn parse_action_lowercase_get_is_error() {
        // Lowercase `get url -> var` must be rejected; only `GET(alias) -> var` is valid.
        let err = parse_action(
            r#"get "mizu://host/data" -> result"#,
            &mut StringInterner::new(),
        )
        .unwrap_err();
        assert!(
            matches!(err, MizuError::ParseError(ref msg) if msg.contains("lowercase") && msg.contains("get")),
            "expected ParseError about lowercase get, got: {err:?}"
        );
    }

    #[test]
    fn parse_action_lowercase_post_is_error() {
        let err = parse_action(
            r#"post "mizu://host/submit" -> result"#,
            &mut StringInterner::new(),
        )
        .unwrap_err();
        assert!(
            matches!(err, MizuError::ParseError(ref msg) if msg.contains("lowercase") && msg.contains("post")),
            "expected ParseError about lowercase post, got: {err:?}"
        );
    }

    #[test]
    fn parse_action_lowercase_put_is_error() {
        let err = parse_action(
            r#"put "mizu://host/item" -> result"#,
            &mut StringInterner::new(),
        )
        .unwrap_err();
        assert!(
            matches!(err, MizuError::ParseError(ref msg) if msg.contains("lowercase") && msg.contains("put")),
            "expected ParseError about lowercase put, got: {err:?}"
        );
    }

    #[test]
    fn parse_action_lowercase_delete_is_error() {
        let err = parse_action(
            r#"delete "mizu://host/item/1" -> result"#,
            &mut StringInterner::new(),
        )
        .unwrap_err();
        assert!(
            matches!(err, MizuError::ParseError(ref msg) if msg.contains("lowercase") && msg.contains("delete")),
            "expected ParseError about lowercase delete, got: {err:?}"
        );
    }

    // ────────────────────────────────────────────────────────────────────────
    // NetworkMethod — as_str round-trip
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn network_method_as_str_values() {
        assert_eq!(NetworkMethod::Get.as_str(), "GET");
        assert_eq!(NetworkMethod::Post.as_str(), "POST");
        assert_eq!(NetworkMethod::Put.as_str(), "PUT");
        assert_eq!(NetworkMethod::Delete.as_str(), "DELETE");
        assert_eq!(NetworkMethod::Query.as_str(), "QUERY");
    }

    // ────────────────────────────────────────────────────────────────────────
    // Expanded ValueType parsing (list, dict, record, any)
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn parse_params_list_becomes_list() {
        let src = "f(items: list) : 1";
        let mut interner = StringInterner::new();
        let fns = parse_logic(src, &mut interner).unwrap();
        let sym = interner.get("f").unwrap();
        assert_eq!(fns[&sym].params[0].1, Some(ValueType::List));
    }

    #[test]
    fn parse_params_dict_annotation_is_error() {
        let src = "f(d: dict) : 1";
        let mut interner = StringInterner::new();
        let result = parse_logic(src, &mut interner);
        assert!(
            matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("dict")),
            "expected ParseError for `dict`, got: {result:?}"
        );
    }

    #[test]
    fn parse_params_record_annotation_is_error() {
        let src = "f(r: record) : 1";
        let mut interner = StringInterner::new();
        let result = parse_logic(src, &mut interner);
        assert!(
            matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("record")),
            "expected ParseError for `record`, got: {result:?}"
        );
    }

    #[test]
    fn parse_params_any_annotation_is_error() {
        let src = "f(x: any) : 1";
        let mut interner = StringInterner::new();
        let result = parse_logic(src, &mut interner);
        assert!(
            matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("any")),
            "expected ParseError for `any`, got: {result:?}"
        );
    }

    #[test]
    fn parse_params_no_annotation_produces_none() {
        // f(x) — no `: type` — parameter should be untyped (None)
        let src = "f(x) : x";
        let mut interner = StringInterner::new();
        let fns = parse_logic(src, &mut interner).unwrap();
        let sym = interner.get("f").unwrap();
        let x_sym = interner.get("x").unwrap();
        assert_eq!(fns[&sym].params, vec![(x_sym, None)]);
    }

    #[test]
    fn parse_params_partial_annotation() {
        // f(x: num, y) — first param typed, second untyped
        let src = "f(x: num, y) : x";
        let mut interner = StringInterner::new();
        let fns = parse_logic(src, &mut interner).unwrap();
        let sym = interner.get("f").unwrap();
        let x_sym = interner.get("x").unwrap();
        let y_sym = interner.get("y").unwrap();
        assert_eq!(
            fns[&sym].params,
            vec![(x_sym, Some(ValueType::Num)), (y_sym, None)]
        );
    }

    // ────────────────────────────────────────────────────────────────────────
    // parse_action_with_urls — HTTP verb without registry (registry = None)
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn parse_action_with_urls_get_no_registry_produces_network_call() {
        let mut interner = StringInterner::new();
        let action = parse_action_with_urls("GET(users) -> result", &mut interner, None).unwrap();
        assert!(matches!(action, Action::NetworkCall {
            method: NetworkMethod::Get,
            ref target_var,
            ..
        } if target_var == "result"));
    }

    #[test]
    fn parse_action_with_urls_get_with_path_param_no_registry() {
        // GET(alias, path_param) — second slot is path_param, no payload
        let mut interner = StringInterner::new();
        let action =
            parse_action_with_urls("GET(users, user_id) -> data", &mut interner, None).unwrap();
        if let Action::NetworkCall {
            method,
            payload,
            path_param,
            ..
        } = action
        {
            assert_eq!(method, NetworkMethod::Get);
            assert!(payload.is_none(), "GET must never have a payload");
            assert!(path_param.is_some(), "GET second arg should be path_param");
        } else {
            panic!("expected NetworkCall");
        }
    }

    #[test]
    fn parse_action_with_urls_post_with_payload_no_registry() {
        // POST(alias, payload) — second slot is payload
        let mut interner = StringInterner::new();
        let action =
            parse_action_with_urls(r#"POST(orders, $form) -> resp"#, &mut interner, None).unwrap();
        if let Action::NetworkCall {
            method,
            payload,
            path_param,
            ..
        } = action
        {
            assert_eq!(method, NetworkMethod::Post);
            assert!(payload.is_some(), "POST second arg should be payload");
            assert!(path_param.is_none());
        } else {
            panic!("expected NetworkCall");
        }
    }

    #[test]
    fn parse_action_with_urls_post_with_payload_and_path_param_no_registry() {
        // POST(alias, payload, path_param) — all three slots
        let mut interner = StringInterner::new();
        let action = parse_action_with_urls(
            r#"POST(orders, $form, order_id) -> resp"#,
            &mut interner,
            None,
        )
        .unwrap();
        if let Action::NetworkCall {
            method,
            payload,
            path_param,
            ..
        } = action
        {
            assert_eq!(method, NetworkMethod::Post);
            assert!(payload.is_some());
            assert!(path_param.is_some());
        } else {
            panic!("expected NetworkCall");
        }
    }

    #[test]
    fn parse_action_with_urls_delete_no_path_param_no_registry() {
        // DELETE(alias) — no path_param
        let mut interner = StringInterner::new();
        let action = parse_action_with_urls("DELETE(item) -> ok", &mut interner, None).unwrap();
        if let Action::NetworkCall {
            method,
            payload,
            path_param,
            ..
        } = action
        {
            assert_eq!(method, NetworkMethod::Delete);
            assert!(payload.is_none(), "DELETE must never have a payload");
            assert!(path_param.is_none());
        } else {
            panic!("expected NetworkCall");
        }
    }

    #[test]
    fn parse_action_with_urls_delete_with_path_param_no_registry() {
        // DELETE(alias, path_param) — second slot is path_param
        let mut interner = StringInterner::new();
        let action =
            parse_action_with_urls("DELETE(items, item_id) -> ok", &mut interner, None).unwrap();
        if let Action::NetworkCall {
            method,
            payload,
            path_param,
            ..
        } = action
        {
            assert_eq!(method, NetworkMethod::Delete);
            assert!(payload.is_none(), "DELETE must never have a payload");
            assert!(path_param.is_some());
        } else {
            panic!("expected NetworkCall");
        }
    }

    #[test]
    fn parse_action_with_urls_get_with_three_args_is_error() {
        // GET(alias, path_param, extra) — GET does not accept a body, so 3 args → error
        let mut interner = StringInterner::new();
        let err = parse_action_with_urls("GET(users, user_id, extra) -> data", &mut interner, None)
            .unwrap_err();
        assert!(
            matches!(err, MizuError::ParseError(ref msg) if msg.contains("does not accept a body")),
            "expected ParseError about no body, got: {err:?}"
        );
    }

    #[test]
    fn parse_action_with_urls_get_registry_unknown_alias_is_error() {
        use crate::parser::urls::{EndpointKind, UrlEndpoint, UrlRegistry};
        let mut registry: UrlRegistry = rustc_hash::FxHashMap::default();
        let mut interner = StringInterner::new();
        // Register `users` as an API endpoint so the alias *exists*
        let sym = interner.get_or_intern("users");
        registry.insert(
            sym,
            UrlEndpoint {
                kind: EndpointKind::Api,
                raw_target: "/api/users".to_string(),
            },
        );

        // `unknown_alias` is NOT in the registry → compile error
        let err = parse_action_with_urls(
            "GET(unknown_alias) -> result",
            &mut interner,
            Some(&registry),
        )
        .unwrap_err();
        assert!(
            matches!(err, MizuError::ParseError(ref msg) if msg.contains("not defined in the `urls` block")),
            "expected ParseError about missing alias, got: {err:?}"
        );
    }

    #[test]
    fn parse_action_with_urls_get_registry_media_alias_is_error() {
        use crate::parser::urls::{EndpointKind, UrlEndpoint, UrlRegistry};
        let mut registry: UrlRegistry = rustc_hash::FxHashMap::default();
        let mut interner = StringInterner::new();
        let sym = interner.get_or_intern("logo");
        registry.insert(
            sym,
            UrlEndpoint {
                kind: EndpointKind::Media,
                raw_target: "mizu://media/logo.png".to_string(),
            },
        );

        let err = parse_action_with_urls("GET(logo) -> result", &mut interner, Some(&registry))
            .unwrap_err();
        assert!(
            matches!(err, MizuError::ParseError(ref msg) if msg.contains("media")),
            "expected ParseError about media endpoint, got: {err:?}"
        );
    }

    #[test]
    fn parse_action_with_urls_get_registry_valid_alias_ok() {
        use crate::parser::urls::{EndpointKind, UrlEndpoint, UrlRegistry};
        let mut registry: UrlRegistry = rustc_hash::FxHashMap::default();
        let mut interner = StringInterner::new();
        let sym = interner.get_or_intern("users");
        registry.insert(
            sym,
            UrlEndpoint {
                kind: EndpointKind::Api,
                raw_target: "/api/users".to_string(),
            },
        );

        let action =
            parse_action_with_urls("GET(users) -> data", &mut interner, Some(&registry)).unwrap();
        assert!(matches!(
            action,
            Action::NetworkCall {
                method: NetworkMethod::Get,
                ..
            }
        ));
    }

    #[test]
    fn parse_action_with_urls_get_missing_parens_is_error() {
        let mut interner = StringInterner::new();
        let err = parse_action_with_urls("GET users -> result", &mut interner, None).unwrap_err();
        assert!(matches!(err, MizuError::ParseError(_)));
    }

    #[test]
    fn parse_action_with_urls_get_missing_arrow_is_error() {
        let mut interner = StringInterner::new();
        let err = parse_action_with_urls("GET(users)", &mut interner, None).unwrap_err();
        assert!(matches!(err, MizuError::ParseError(_)));
    }

    // ────────────────────────────────────────────────────────────────────────
    // parse_root_timers — happy paths and error cases
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn parse_root_timers_milliseconds_literal() {
        let src = "timer 500ms -> count = count + 1";
        let mut interner = StringInterner::new();
        let timers = parse_root_timers(src, &mut interner).unwrap();
        assert_eq!(timers.len(), 1);
        assert_eq!(timers[0].interval, TimerInterval::Millis(500));
        assert!(matches!(timers[0].action, Action::Assign { ref target, .. } if target == "count"));
    }

    #[test]
    fn parse_root_timers_bare_number_milliseconds() {
        let src = "timer 1000 -> flag = true";
        let mut interner = StringInterner::new();
        let timers = parse_root_timers(src, &mut interner).unwrap();
        assert_eq!(timers.len(), 1);
        assert_eq!(timers[0].interval, TimerInterval::Millis(1000));
    }

    #[test]
    fn parse_root_timers_variable_interval() {
        // Use a name that does NOT end in "ms" so it isn't misidentified as a literal.
        let src = "timer tick_rate -> refresh = true";
        let mut interner = StringInterner::new();
        let timers = parse_root_timers(src, &mut interner).unwrap();
        assert_eq!(timers.len(), 1);
        assert_eq!(
            timers[0].interval,
            TimerInterval::Variable("tick_rate".to_string())
        );
    }

    #[test]
    fn parse_root_timers_multiple_timers() {
        let src = "timer 100ms -> a = 1\ntimer 200ms -> b = 2";
        let mut interner = StringInterner::new();
        let timers = parse_root_timers(src, &mut interner).unwrap();
        assert_eq!(timers.len(), 2);
        assert_eq!(timers[0].interval, TimerInterval::Millis(100));
        assert_eq!(timers[1].interval, TimerInterval::Millis(200));
    }

    #[test]
    fn parse_root_timers_non_timer_lines_are_ignored() {
        // parse_root_timers skips non-timer lines; parse_logic handles functions
        let src = "double(x: num) : x + x\ntimer 300ms -> flag = true";
        let mut interner = StringInterner::new();
        let timers = parse_root_timers(src, &mut interner).unwrap();
        assert_eq!(timers.len(), 1);
        assert_eq!(timers[0].interval, TimerInterval::Millis(300));
    }

    #[test]
    fn parse_root_timers_missing_arrow_is_error() {
        let src = "timer 500ms count = count + 1";
        let mut interner = StringInterner::new();
        let err = parse_root_timers(src, &mut interner).unwrap_err();
        assert!(
            matches!(err, MizuError::ParseError(ref msg) if msg.contains("->")),
            "expected ParseError about missing `->`, got: {err:?}"
        );
    }

    #[test]
    fn parse_root_timers_empty_source_returns_empty_vec() {
        let mut interner = StringInterner::new();
        let timers = parse_root_timers("", &mut interner).unwrap();
        assert!(timers.is_empty());
    }

    #[test]
    fn timer_interval_seconds() {
        let src = "timer 60s -> x = 1";
        let mut interner = StringInterner::new();
        let timers = parse_root_timers(src, &mut interner).unwrap();
        assert_eq!(timers.len(), 1);
        assert_eq!(timers[0].interval, TimerInterval::Millis(60000));
    }

    #[test]
    fn timer_interval_fractional_seconds() {
        let src = "timer 1.5s -> x = 1";
        let mut interner = StringInterner::new();
        let timers = parse_root_timers(src, &mut interner).unwrap();
        assert_eq!(timers.len(), 1);
        assert_eq!(timers[0].interval, TimerInterval::Millis(1500));
    }

    #[test]
    fn timer_interval_ms_unchanged() {
        let src = "timer 500ms -> x = 1";
        let mut interner = StringInterner::new();
        let timers = parse_root_timers(src, &mut interner).unwrap();
        assert_eq!(timers.len(), 1);
        assert_eq!(timers[0].interval, TimerInterval::Millis(500));
    }

    // ────────────────────────────────────────────────────────────────────────
    // $form magic variable — lexed as Ident("$form")
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn dollar_form_variable_is_valid_assign_target() {
        // `$form = 1` must parse as Assign with target "$form"
        let action = parse_action("$form = 1", &mut StringInterner::new()).unwrap();
        assert!(
            matches!(action, Action::Assign { ref target, .. } if target == "$form"),
            "expected Assign with target $form, got: {action:?}"
        );
    }

    // ────────────────────────────────────────────────────────────────────────
    // Integer overflow — apply_binop checked arithmetic
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn apply_binop_add_overflow() {
        let result = super::apply_binop(&BinOp::Add, Value::Int(i64::MAX), Value::Int(1));
        assert!(
            matches!(result, Err(MizuError::ExecutionError(_))),
            "expected ExecutionError for i64::MAX + 1, got: {result:?}"
        );
    }

    #[test]
    fn apply_binop_mul_overflow() {
        let result = super::apply_binop(&BinOp::Mul, Value::Int(i64::MAX), Value::Int(2));
        assert!(
            matches!(result, Err(MizuError::ExecutionError(_))),
            "expected ExecutionError for i64::MAX * 2, got: {result:?}"
        );
    }

    #[test]
    fn apply_binop_sub_underflow() {
        let result = super::apply_binop(&BinOp::Sub, Value::Int(i64::MIN), Value::Int(1));
        assert!(
            matches!(result, Err(MizuError::ExecutionError(_))),
            "expected ExecutionError for i64::MIN - 1, got: {result:?}"
        );
    }

    #[test]
    fn apply_binop_div_overflow() {
        let result = super::apply_binop(&BinOp::Div, Value::Int(i64::MIN), Value::Int(-1));
        assert!(
            matches!(result, Err(MizuError::ExecutionError(_))),
            "expected ExecutionError for i64::MIN / -1, got: {result:?}"
        );
    }

    // ────────────────────────────────────────────────────────────────────────
    // comp keyword tests
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_comp_cycle_rejected() {
        let src = "    comp a = b + 1\n    comp b = a + 1\n";
        let mut interner = StringInterner::new();
        let result = super::parse_computed(src, &mut interner);
        assert!(
            matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("cycle")),
            "expected cycle error, got: {result:?}"
        );
    }

    #[test]
    fn test_comp_assignment_rejected() {
        let src = "    comp derived = 42\n";
        let mut interner = StringInterner::new();
        let computed = super::parse_computed(src, &mut interner).unwrap();
        assert_eq!(computed.len(), 1);

        let mut store = VariableStore::with_interner(interner);
        let derived_sym = store.interner.get_or_intern("derived");
        store.state_machine.computed_var_syms.insert(derived_sym);

        let fns = FxHashMap::default();
        let action = Action::Assign {
            target: "derived".to_string(),
            expr: Expr::Literal(Value::Int(99)),
        };
        let result = super::execute_action(&action, &mut store, &fns);
        assert!(
            matches!(result, Err(MizuError::ExecutionError(ref msg)) if msg.contains("computed variable")),
            "expected ExecutionError for comp assignment, got: {result:?}"
        );
    }

    #[test]
    fn test_comp_initial_value() {
        let src = "    comp derived = total + 1\n";
        let mut interner = StringInterner::new();
        let computed = super::parse_computed(src, &mut interner).unwrap();

        let mut store = VariableStore::with_interner(interner);
        store.set("total", Value::Int(5));

        let fns = FxHashMap::default();
        let all_syms: FxHashSet<Symbol> =
            store.state_machine.global_store.keys().copied().collect();
        super::recompute_computed_bindings(&mut store, &computed, &fns, &all_syms);

        let derived_sym = store.interner.get("derived").unwrap();
        assert_eq!(*store.state_machine.get_global(derived_sym), Value::Int(6));
    }

    #[test]
    fn test_comp_evaluated_on_dependency_change() {
        let src = "    comp double = x * 2\n";
        let mut interner = StringInterner::new();
        let computed = super::parse_computed(src, &mut interner).unwrap();

        let mut store = VariableStore::with_interner(interner);
        store.set("x", Value::Int(10));
        let fns = FxHashMap::default();

        let all_syms: FxHashSet<Symbol> =
            store.state_machine.global_store.keys().copied().collect();
        super::recompute_computed_bindings(&mut store, &computed, &fns, &all_syms);
        let double_sym = store.interner.get("double").unwrap();
        assert_eq!(*store.state_machine.get_global(double_sym), Value::Int(20));

        // Mutate x and recompute
        store.state_machine.undo_log.clear();
        store.set("x", Value::Int(7));
        let x_sym = store.interner.get("x").unwrap();
        let mutated: FxHashSet<Symbol> = [x_sym].into_iter().collect();
        super::recompute_computed_bindings(&mut store, &computed, &fns, &mutated);
        assert_eq!(*store.state_machine.get_global(double_sym), Value::Int(14));
    }

    #[test]
    fn test_comp_chain() {
        // comp a = x + 1; comp b = a * 2 → must be evaluated in topo order
        let src = "    comp a = x + 1\n    comp b = a * 2\n";
        let mut interner = StringInterner::new();
        let computed = super::parse_computed(src, &mut interner).unwrap();

        let a_pos = computed
            .iter()
            .position(|cb| interner.resolve(cb.name) == Some("a"))
            .unwrap();
        let b_pos = computed
            .iter()
            .position(|cb| interner.resolve(cb.name) == Some("b"))
            .unwrap();
        assert!(a_pos < b_pos, "a must precede b in topological order");

        let mut store = VariableStore::with_interner(interner);
        store.set("x", Value::Int(3));
        let fns = FxHashMap::default();

        let all_syms: FxHashSet<Symbol> =
            store.state_machine.global_store.keys().copied().collect();
        super::recompute_computed_bindings(&mut store, &computed, &fns, &all_syms);

        let a_sym = store.interner.get("a").unwrap();
        let b_sym = store.interner.get("b").unwrap();
        assert_eq!(*store.state_machine.get_global(a_sym), Value::Int(4));
        assert_eq!(*store.state_machine.get_global(b_sym), Value::Int(8));
    }

    // ── Depth guard tests ────────────────────────────────────────────────────

    #[test]
    fn parse_deeply_nested_rejected() {
        // 300 nested parentheses — must produce a ParseError, not a stack overflow.
        let depth = 300usize;
        let src = format!("{}{}{}", "(".repeat(depth), "1", ")".repeat(depth));
        let mut interner = StringInterner::new();
        let result = super::parse_expr_standalone(&src, &mut interner);
        match result {
            Err(MizuError::ParseError(msg)) => {
                assert!(
                    msg.contains("nesting too deep"),
                    "error must mention nesting depth: {msg}"
                );
            }
            other => panic!("expected ParseError for deeply nested expr, got: {other:?}"),
        }
    }

    #[test]
    fn parse_deep_unary_chain_rejected() {
        // 300 consecutive `!` operators — must produce a ParseError, not a stack overflow.
        let src = format!("{}true", "!".repeat(300));
        let mut interner = StringInterner::new();
        let result = super::parse_expr_standalone(&src, &mut interner);
        match result {
            Err(MizuError::ParseError(msg)) => {
                assert!(
                    msg.contains("nesting too deep"),
                    "error must mention nesting depth: {msg}"
                );
            }
            other => panic!("expected ParseError for deep unary chain, got: {other:?}"),
        }
    }

    #[test]
    fn parse_normal_nesting_ok() {
        // 10 levels of nesting is well within the limit and must parse successfully.
        let depth = 10usize;
        let src = format!("{}{}{}", "(".repeat(depth), "42", ")".repeat(depth));
        let mut interner = StringInterner::new();
        let result = super::parse_expr_standalone(&src, &mut interner);
        assert!(
            result.is_ok(),
            "normal nesting depth must parse without error: {result:?}"
        );
    }
}
