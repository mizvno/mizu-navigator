//! Small string/token helpers used by attribute and tree parsing:
//! whitespace scanning, quoted-string parsing, and layout-keyword/lang-tag
//! shape checks.

use crate::core::errors::MizuError;
use crate::core::types::Value;
use crate::parser::logic::{Expr, ExprArena};

/// Returns the number of leading space characters in `line`.
#[inline]
pub(super) fn leading_spaces(line: &str) -> usize {
    line.as_bytes().iter().take_while(|&&b| b == b' ').count()
}

/// Splits a string on its first whitespace boundary into `(first_word, rest)`.
pub(super) fn split_first_word(s: &str) -> (&str, &str) {
    let s = s.trim();
    if let Some(pos) = s.find(|c: char| c.is_whitespace()) {
        (s[..pos].trim(), s[pos..].trim())
    } else {
        (s, "")
    }
}

/// Parses a double-quoted string from the start of `s`, resolving escape sequences.
/// Returns the parsed string and the remaining unparsed slice of `s`.
pub(super) fn parse_quoted_string(s: &str) -> Result<(String, &str), MizuError> {
    if !s.starts_with('"') {
        return Err(MizuError::ParseError(
            "Expected opening double quote".to_string(),
        ));
    }

    let mut content = String::new();
    let mut chars = s[1..].char_indices();

    while let Some((idx, c)) = chars.next() {
        if c == '"' {
            let end_idx = 1 + idx + 1; // 1 for the initial quote, idx is offset in s[1..], + 1 for length of '"' (which is 1 byte)
            let rest = &s[end_idx..];
            return Ok((content, rest));
        } else if c == '\\' {
            if let Some((_, next_c)) = chars.next() {
                content.push(next_c);
            }
        } else {
            content.push(c);
        }
    }

    Err(MizuError::ParseError(
        "Unterminated double-quoted string".to_string(),
    ))
}

/// Layout-only attribute keywords that are never valid as standalone tokens
/// inside a Mizu logic expression.  Detecting them early in the action string
/// produces a clear diagnostic instead of silent token loss.
///
/// The list is intentionally conservative — it excludes ambiguous words like
/// `type` or `width` that could legitimately be Mizu variable names.
const LAYOUT_ATTR_KEYWORDS: &[&str] = &["class", "id", "src", "href", "alt", "dir"];

/// Scans `action_str` for layout attribute keywords appearing as complete
/// whitespace-delimited words.  Returns the first offending keyword if found.
///
/// This is a defence-in-depth companion to the cursor-exhaustion check in
/// `parse_action`: it fires earlier and produces a more actionable error
/// message pointing to the specific keyword.
pub(super) fn find_trailing_layout_keyword(action_str: &str) -> Option<&'static str> {
    for word in action_str.split_whitespace() {
        if let Some(&kw) = LAYOUT_ATTR_KEYWORDS.iter().find(|&&kw| kw == word) {
            return Some(kw);
        }
    }
    None
}

/// Shape-checks a `lang` attribute value: a lowercase 2-3-letter primary
/// language subtag, optionally followed by `-` and an uppercase 2-letter
/// region subtag (e.g. `it`, `en`, `en-US`, `zh-CN`). This is a shape check
/// (BCP-47-ish), not a full BCP-47 validator — it does not check the
/// subtag against the IANA language/region registries, only the syntax.
pub(super) fn is_valid_lang_tag(value: &str) -> bool {
    let (primary, region) = match value.split_once('-') {
        Some((p, r)) => (p, Some(r)),
        None => (value, None),
    };
    let primary_ok =
        (2..=3).contains(&primary.len()) && primary.chars().all(|c| c.is_ascii_lowercase());
    let region_ok = match region {
        Some(r) => r.len() == 2 && r.chars().all(|c| c.is_ascii_uppercase()),
        None => true,
    };
    primary_ok && region_ok
}

/// Walks `expr`'s value-producing branches (recursing through nested
/// `Expr::IfElse`'s `then_expr`/`else_expr` — deliberately *not* into any
/// `condition`, which is allowed to be an arbitrary pure boolean expression)
/// and returns a description of the first branch found that is not a plain
/// `Expr::Literal(Value::String(_))`.
///
/// This is the load-bearing check that makes a ternary conditional class
/// safe to add at all: without it, `class expr ? a : b` would let a
/// document choose a CSS class name from a variable, field access, or
/// function-call result — an information-flow surface this feature must
/// not open. Every possible output is required to be a literal the
/// document author wrote, known in full before the document ever runs;
/// only *which* literal is chosen varies at runtime.
pub(super) fn find_non_literal_string_branch(
    expr: &Expr,
    arena: &ExprArena,
) -> Option<&'static str> {
    match expr {
        Expr::Literal(Value::String(_)) => None,
        Expr::IfElse {
            then_expr,
            else_expr,
            ..
        } => find_non_literal_string_branch(&arena[*then_expr], arena)
            .or_else(|| find_non_literal_string_branch(&arena[*else_expr], arena)),
        Expr::Literal(_) => Some("a non-string literal"),
        Expr::Variable(_) => Some("a variable"),
        Expr::FieldAccess { .. } => Some("a field access"),
        Expr::FunctionCall { .. } => Some("a function call"),
        Expr::BinaryOp { .. } => Some("a binary operation"),
        Expr::Not(_) => Some("a `!` expression"),
        Expr::Let { .. } => Some("a `let` binding"),
    }
}
