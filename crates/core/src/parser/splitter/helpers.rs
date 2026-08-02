//! Leaf helpers: `is_import_directive`, `push_line`, `strip_comment`,
//! `parse_import_path`, `resolve_import_target`, and
//! `check_no_nested_imports`.

use std::path::Path;

use crate::core::errors::MizuError;

use super::types::ImportTarget;

/// Returns `true` if `trimmed` is a root-level `import`/`include` directive
/// (i.e. the keyword is followed by a path argument).
#[inline]
pub(super) fn is_import_directive(trimmed: &str) -> bool {
    trimmed.starts_with("import ") || trimmed.starts_with("include ")
}

#[inline]
pub(super) fn push_line(buf: &mut String, line: &str) {
    buf.push_str(line);
    buf.push('\n');
}

/// Returns the portion of `line` that precedes the first `//` comment token
/// that appears **outside** a double-quoted string literal.
///
/// ## String-awareness
///
/// A `//` sequence inside a `"…"` literal (e.g., `text "http://example.com"`)
/// is **not** treated as a comment.  The scanner tracks entry/exit of string
/// literals by counting unescaped `"` characters.
///
/// ## Why bytes, not chars?
///
/// The characters we look for (`/`, `"`, `\`) are all single-byte ASCII
/// code-points (< 0x80).  UTF-8 multi-byte sequences always have continuation
/// bytes ≥ 0x80, so scanning by byte index cannot produce false positives.
/// Slicing at an ASCII byte index always produces a valid UTF-8 boundary.
pub(super) fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut in_string = false;
    let mut i = 0usize;

    while i < len {
        match bytes[i] {
            // Toggle string context on an unescaped double-quote.
            b'"' => {
                in_string = !in_string;
            }
            // Inside a string, skip the character following a backslash so
            // that `\"` does not prematurely close the string context.
            b'\\' if in_string => {
                i += 1; // skip the escaped char (bounds-safe: i+1 may == len)
            }
            // Outside a string, a `;;` pair starts a comment — but only at the
            // start of the line or after whitespace.  This keeps `;;` inside
            // unquoted URLs intact (`media logo mizu://cdn.example.com/x.png`
            // in the `urls` block must not lose everything after `mizu:`).
            b';' if !in_string
                && i + 1 < len
                && bytes[i + 1] == b';'
                && (i == 0 || bytes[i - 1].is_ascii_whitespace()) =>
            {
                return &line[..i];
            }
            _ => {}
        }
        i += 1;
    }

    line
}

/// Parses the quoted file path from an `import "…"` directive line.
///
/// `trimmed` is expected to be the already-trimmed root-level line, e.g.
/// `import "common/theme.mss"`.
///
/// Returns a `&str` slice pointing into `trimmed` for the path inside the
/// quotes (no allocation).
pub(super) fn parse_import_path(trimmed: &str) -> Result<&str, MizuError> {
    // Strip the `import ` / `include ` keyword (already confirmed by the
    // dispatcher to start with one of them).
    let after_keyword = if let Some(rest) = trimmed.strip_prefix("import ") {
        rest.trim()
    } else if let Some(rest) = trimmed.strip_prefix("include ") {
        rest.trim()
    } else {
        return Err(MizuError::ParseError(format!(
            "malformed import directive `{trimmed}`; \
             expected `import \"…\"` or `include \"…\"`"
        )));
    };

    if after_keyword.len() < 2 || !after_keyword.starts_with('"') || !after_keyword.ends_with('"') {
        return Err(MizuError::ParseError(format!(
            "malformed import directive `{trimmed}`; \
             the path must be a double-quoted string, e.g. `import \"file.mss\"`"
        )));
    }

    let path = &after_keyword[1..after_keyword.len() - 1];

    if path.is_empty() {
        return Err(MizuError::ParseError(
            "import path must not be empty".to_owned(),
        ));
    }

    Ok(path)
}

/// Determines which buffer an imported file targets based on its extension.
///
/// Only `.mlg` (Mizu Logic) and `.mss` (Mizu Style Sheet) are permitted.
pub(super) fn resolve_import_target(import_path: &str) -> Result<ImportTarget, MizuError> {
    let ext = Path::new(import_path)
        .extension()
        .and_then(|e| e.to_str())
        .ok_or_else(|| {
            MizuError::ParseError(format!(
                "import `{import_path}` has no file extension; \
                 only `.mlg` and `.mss` are allowed"
            ))
        })?;

    match ext {
        "mlg" => Ok(ImportTarget::Logic),
        "mss" => Ok(ImportTarget::Style),
        other => Err(MizuError::ParseError(format!(
            "import extension `.{other}` is not permitted; \
             only `.mlg` (logic) and `.mss` (style) are allowed"
        ))),
    }
}

/// Scans the raw content of an imported file and returns an error if it
/// contains a root-level `import` directive.
///
/// "Root-level" means the line has zero leading spaces after comment-stripping.
/// This enforces the flat-import guardrail: imported files may not themselves
/// import further files.
pub(super) fn check_no_nested_imports(content: &str) -> Result<(), MizuError> {
    for raw_line in content.lines() {
        let line = strip_comment(raw_line);
        // A root-level line has no leading whitespace.
        if line.starts_with(' ') || line.starts_with('\t') {
            continue;
        }
        let trimmed = line.trim();
        // Match both `import "…"` / `include "…"` and a bare keyword (malformed
        // but still a nesting attempt that must be caught).
        if trimmed == "import"
            || trimmed == "include"
            || trimmed.starts_with("import ")
            || trimmed.starts_with("include ")
        {
            return Err(MizuError::ParseError(
                "nested imports are strictly forbidden: \
                 imported `.mlg`/`.mss` files cannot themselves contain `import` directives"
                    .to_owned(),
            ));
        }
    }
    Ok(())
}
