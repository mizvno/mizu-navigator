//! Hand-rolled tokeniser for the Mizu logic block.

use crate::core::errors::MizuError;

/// Internal token produced by the Mizu logic lexer.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Token {
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
    /// `{`
    LBrace,
    /// `}`
    RBrace,
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
pub(super) fn lex(source: &str) -> Result<Vec<Token>, MizuError> {
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
            (b'{', _) => (Token::LBrace, 1),
            (b'}', _) => (Token::RBrace, 1),
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
pub(super) struct Cursor<'a> {
    tokens: &'a [Token],
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub(super) fn new(tokens: &'a [Token]) -> Self {
        Self { tokens, pos: 0 }
    }

    pub(super) fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    pub(super) fn next(&mut self) -> Option<&Token> {
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
        Token::LBrace => "`{`".to_owned(),
        Token::RBrace => "`}`".to_owned(),
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
pub(super) fn assert_cursor_empty(cursor: &Cursor<'_>, context: &str) -> Result<(), MizuError> {
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


/// Returns the number of leading space characters in `line`.
#[inline]
pub(super) fn leading_spaces(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}
