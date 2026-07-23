//! # `splitter` — Line-by-Line Macro-Block Preprocessor
//!
//! This module is the **first formal pass** of the Mizu compilation pipeline.
//! It takes raw `.mizu` source text and produces a [`ParsedSource`] struct
//! containing three isolated, comment-free, import-resolved text buffers —
//! one per macro-block — ready for subsequent parsing phases.
//!
//! ## Responsibilities (in order of execution)
//!
//! 1. **Comment stripping** — removes everything after the first `//` token
//!    that is *not* inside a string literal, on every line.
//! 2. **Import resolution** — resolves `import "file"` (or its synonym
//!    `include "file"`) directives at indentation 0 by reading the target file
//!    from the local filesystem, verifying it carries no nested imports, and
//!    splicing its content into the appropriate block buffer (`logic` for
//!    `.mlg`, `style` for `.mss`).  Import resolution is governed by an
//!    [`Origin`] trust boundary: documents delivered over the network may not
//!    use imports at all, and local imports are confined to the document's own
//!    directory (no path traversal outside it).
//! 3. **Section dispatch** — routes indented content lines into the correct
//!    block buffer (`logic`, `style`, `layout`, or `urls`) based on the most
//!    recently seen zero-indented section keyword.
//! 4. **Blank-line padding** — for every dispatched content line, empty
//!    sentinel lines are appended to all *inactive* buffers, preserving
//!    file-offset alignment so that downstream parsers can produce accurate
//!    line numbers in error messages.
//! 5. **Validation** — rejects any zero-indented token that is not a section
//!    keyword or a valid import directive, as well as any indented content
//!    encountered before the first section keyword.
//!
//! ## What This Module Does NOT Do
//!
//! * It does **not** parse expressions, property values, or structural
//!   primitives — that is the responsibility of Phase 3+ parsers.
//! * It does **not** validate the *content* of the injected blocks (e.g.,
//!   whether a `.mlg` file contains syntactically valid Mizu functions).
//! * It does **not** interpret multi-line `"""` blocks; the layout parser owns
//!   all block-level structure.

#![forbid(unsafe_code)]

use std::path::Path;

use crate::core::errors::MizuError;


/// The output of a successful [`split_source`] call.
///
/// Each field holds the raw, comment-stripped, import-resolved content of the
/// corresponding macro-block, with the leading section keyword line itself
/// omitted (i.e., the first line is the first *body* line of the block).
///
/// ## Empty blocks
///
/// If a source file omits a macro-block entirely, the corresponding field
/// contains only blank padding lines (one per content line in other blocks).
/// Use `.trim().is_empty()` rather than `.is_empty()` to test for absence.
///
/// ## Indentation preservation
///
/// Every content line retains its original indentation relative to the source
/// file.  Downstream parsers are responsible for interpreting that indentation
/// according to the Mizu grammar for each block type.
///
/// ## Line-offset alignment
///
/// For every content line dispatched to an active block, a blank sentinel line
/// (`""`) is appended to each inactive block buffer.  This guarantees that
/// line *N* of any buffer corresponds to line *N* of the virtual interleaved
/// stream, enabling accurate line numbers in downstream error messages.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedSource {
    /// Comment-stripped, import-resolved content of the `logic` macro-block.
    pub logic_block: String,
    /// Comment-stripped, import-resolved content of the `style` macro-block.
    pub style_block: String,
    /// Comment-stripped content of the `layout` macro-block.
    pub layout_block: String,
    /// Comment-stripped content of the `urls` macro-block (URL registry).
    pub urls_block: String,
}


/// Tracks which macro-block is currently being accumulated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveBlock {
    /// No section keyword has been seen yet.
    None,
    Logic,
    Style,
    Layout,
    /// URL registry block — declares `api` and `media` endpoint aliases.
    Urls,
}

/// Identifies which buffer an imported file should be spliced into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImportTarget {
    Logic,
    Style,
}

/// Trust boundary that governs how `import`/`include` directives are resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Document loaded from the local filesystem.  Imports are resolved
    /// relative to — and confined within — the document's own directory.
    LocalFile,
    /// Document delivered over the network (e.g. via `mizu://`).  Imports are
    /// forbidden entirely and **no** filesystem access is performed, so a
    /// hostile remote document cannot read arbitrary local files via
    /// `import "../../secret"`.
    Network,
}

/// Returns `true` if `trimmed` is a root-level `import`/`include` directive
/// (i.e. the keyword is followed by a path argument).
#[inline]
fn is_import_directive(trimmed: &str) -> bool {
    trimmed.starts_with("import ") || trimmed.starts_with("include ")
}


/// Splits a raw `.mizu` source string into three isolated macro-block buffers.
///
/// # Arguments
///
/// * `source`      — The complete text of the `.mizu` file.
/// * `current_dir` — The directory containing the `.mizu` file.  Used to
///   resolve relative `import` paths.  Must be an existing directory on the
///   filesystem for import directives to succeed; it is never accessed for
///   source files that contain no imports.
///
/// # Errors
///
/// | Condition | Error variant |
/// |---|---|
/// | A zero-indented token is not `logic`, `style`, `layout`, `urls`, or `import "…"` | [`MizuError::ParseError`] |
/// | Indented content appears before any section keyword | [`MizuError::ParseError`] |
/// | An `import` directive uses an unsupported extension | [`MizuError::ParseError`] |
/// | An imported file itself contains a root-level `import` | [`MizuError::ParseError`] |
/// | A malformed `import` directive (e.g., unquoted path) | [`MizuError::ParseError`] |
/// | The imported file cannot be read from disk | [`MizuError::IoError`] |
///
/// # Examples
///
/// ```
/// use mizu_core::parser::split_source;
/// use std::path::Path;
///
/// let source = r#"
/// logic
///     tax(p: num) : p * 1.10
/// layout
///     window "App"
/// "#;
///
/// let parsed = split_source(source, Path::new(".")).unwrap();
/// assert!(parsed.logic_block.contains("tax"));
/// assert!(parsed.layout_block.contains("window"));
/// assert!(parsed.style_block.trim().is_empty());
/// assert!(parsed.urls_block.trim().is_empty());
/// ```
pub fn split_source(source: &str, current_dir: &Path) -> Result<ParsedSource, MizuError> {
    split_source_with_origin(source, current_dir, Origin::LocalFile)
}

/// Splits a raw `.mizu` source string, applying the [`Origin`] trust boundary
/// to `import`/`include` directives.
///
/// This is the trust-aware counterpart of [`split_source`] (which delegates
/// here with [`Origin::LocalFile`]).
///
/// * [`Origin::LocalFile`] — imports are resolved from disk, but the resolved
///   file must be a descendant of `current_dir` (no path traversal).
/// * [`Origin::Network`] — any root-level `import`/`include` is rejected with a
///   [`MizuError::ParseError`]; the filesystem is never touched.
pub fn split_source_with_origin(
    source: &str,
    current_dir: &Path,
    origin: Origin,
) -> Result<ParsedSource, MizuError> {
    let mut logic_buf = String::new();
    let mut style_buf = String::new();
    let mut layout_buf = String::new();
    let mut urls_buf = String::new();
    let mut active = ActiveBlock::None;

    for (line_idx, raw_line) in source.lines().enumerate() {
        let line = strip_comment(raw_line);

        // We count raw bytes (not chars) because the Mizu spec mandates ASCII
        // indentation (spaces only); non-ASCII can only appear inside strings.
        let trimmed_start = line.trim_start_matches(' ');
        let indent = line.len() - trimmed_start.len();
        let trimmed = trimmed_start.trim_end();

        if trimmed.is_empty() {
            continue;
        }

        if indent == 0 {
            match trimmed {
                "logic" => {
                    active = ActiveBlock::Logic;
                }
                "style" => {
                    active = ActiveBlock::Style;
                }
                "layout" => {
                    active = ActiveBlock::Layout;
                }
                "urls" => {
                    active = ActiveBlock::Urls;
                }
                _ if is_import_directive(trimmed) => match origin {
                    Origin::Network => {
                        return Err(MizuError::ParseError(
                            "includes are not permitted in network-delivered documents".to_owned(),
                        ));
                    }
                    Origin::LocalFile => {
                        process_import(
                            trimmed,
                            current_dir,
                            &mut logic_buf,
                            &mut style_buf,
                            line_idx + 1,
                        )?;
                    }
                },
                _ => {
                    return Err(MizuError::ParseError(format!(
                        "line {}: unexpected root-level token `{trimmed}`; \
                         expected `logic`, `style`, `layout`, `urls`, or `import \"…\"`",
                        line_idx + 1
                    )));
                }
            }
            continue;
        }

        // Preserve the full line (with its original indentation) so that
        // downstream parsers can reconstruct the block's indentation tree.
        //
        // Blank-line padding (O(N)): for every content line dispatched to the
        // active buffer, a sentinel `""` is appended to all inactive buffers so
        // that line N of any buffer corresponds to the same source-file offset.
        let content_line = line.trim_end();

        match active {
            ActiveBlock::Logic => {
                push_line(&mut logic_buf, content_line);
                push_line(&mut style_buf, "");
                push_line(&mut layout_buf, "");
                push_line(&mut urls_buf, "");
            }
            ActiveBlock::Style => {
                push_line(&mut logic_buf, "");
                push_line(&mut style_buf, content_line);
                push_line(&mut layout_buf, "");
                push_line(&mut urls_buf, "");
            }
            ActiveBlock::Layout => {
                push_line(&mut logic_buf, "");
                push_line(&mut style_buf, "");
                push_line(&mut layout_buf, content_line);
                push_line(&mut urls_buf, "");
            }
            ActiveBlock::Urls => {
                push_line(&mut logic_buf, "");
                push_line(&mut style_buf, "");
                push_line(&mut layout_buf, "");
                push_line(&mut urls_buf, content_line);
            }
            ActiveBlock::None => {
                return Err(MizuError::ParseError(format!(
                    "line {}: indented content `{content_line}` appears \
                     before any section keyword (`logic`, `style`, `layout`, or `urls`)",
                    line_idx + 1
                )));
            }
        }
    }

    Ok(ParsedSource {
        logic_block: logic_buf,
        style_block: style_buf,
        layout_block: layout_buf,
        urls_block: urls_buf,
    })
}


/// Appends `line` followed by a newline to `buf`.
#[inline]
fn push_line(buf: &mut String, line: &str) {
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
fn strip_comment(line: &str) -> &str {
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
            // Outside a string, a `//` pair starts a comment — but only at the
            // start of the line or after whitespace.  This keeps `//` inside
            // unquoted URLs intact (`media logo mizu://cdn.example.com/x.png`
            // in the `urls` block must not lose everything after `mizu:`).
            b'/' if !in_string
                && i + 1 < len
                && bytes[i + 1] == b'/'
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
fn parse_import_path(trimmed: &str) -> Result<&str, MizuError> {
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
fn resolve_import_target(import_path: &str) -> Result<ImportTarget, MizuError> {
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
fn check_no_nested_imports(content: &str) -> Result<(), MizuError> {
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

/// Resolves and splices a single `import "…"` directive.
///
/// # Steps
///
/// 1. Parse the quoted path from the directive line.
/// 2. Determine the target buffer from the file extension.
/// 3. Read the file relative to `current_dir`.
/// 4. Assert no nested imports exist inside the file.
/// 5. Strip comments from each imported line and append to the target buffer.
fn process_import(
    trimmed_line: &str,
    current_dir: &Path,
    logic_buf: &mut String,
    style_buf: &mut String,
    line_number: usize,
) -> Result<(), MizuError> {
    let import_path = parse_import_path(trimmed_line).map_err(|e| {
        MizuError::ParseError(format!("line {line_number}: {e}"))
    })?;

    let target = resolve_import_target(import_path)
        .map_err(|e| MizuError::ParseError(format!("line {line_number}: {e}")))?;

    let full_path = current_dir.join(import_path);

    // Canonicalise both the base directory and the resolved file, then verify
    // the file is a descendant of the base.  This rejects traversal attempts
    // such as `import "../../secret.mlg"`.  Canonicalisation also fails for a
    // missing file, which we surface as the usual "cannot read import" error.
    let canonical_dir = std::fs::canonicalize(current_dir).map_err(|io_err| {
        MizuError::ParseError(format!(
            "line {line_number}: cannot canonicalize import base directory: {io_err}"
        ))
    })?;
    let canonical_full = std::fs::canonicalize(&full_path).map_err(|io_err| {
        MizuError::ParseError(format!(
            "line {line_number}: cannot read import `{import_path}`: {io_err}"
        ))
    })?;
    if !canonical_full.starts_with(&canonical_dir) {
        return Err(MizuError::ParseError(format!(
            "line {line_number}: import `{import_path}` escapes the document directory; \
             path traversal outside the document folder is not permitted"
        )));
    }

    let raw_content = std::fs::read_to_string(&canonical_full).map_err(|io_err| {
        MizuError::ParseError(format!(
            "line {line_number}: cannot read import `{import_path}`: {io_err}"
        ))
    })?;

    check_no_nested_imports(&raw_content)?;

    let buf = match target {
        ImportTarget::Logic => logic_buf,
        ImportTarget::Style => style_buf,
    };

    for raw_line in raw_content.lines() {
        let line = strip_comment(raw_line).trim_end();
        if !line.trim().is_empty() {
            push_line(buf, line);
        }
    }

    Ok(())
}


#[cfg(test)]
mod tests {
    use super::{Origin, ParsedSource, split_source, split_source_with_origin, strip_comment};
    use crate::core::errors::MizuError;
    use std::path::Path;

    const NO_IMPORT_DIR: &str = ".";

    // strip_comment unit tests

    #[test]
    fn strip_bare_comment() {
        assert_eq!(strip_comment("// full line comment"), "");
    }

    #[test]
    fn strip_trailing_comment() {
        assert_eq!(
            strip_comment("    tax(p: num) : p * 1.10 // apply VAT"),
            "    tax(p: num) : p * 1.10 "
        );
    }

    #[test]
    fn strip_preserves_url_inside_string() {
        // `//` inside a string literal must NOT be treated as a comment.
        let line = r#"    text "visit http://example.com for info""#;
        assert_eq!(strip_comment(line), line);
    }

    #[test]
    fn strip_preserves_unquoted_mizu_url() {
        // `urls` block targets are unquoted: the `//` of the scheme must not
        // start a comment (it is preceded by `:`, not whitespace).
        let line = "  media logo mizu://cdn.example.com/logo.png";
        assert_eq!(strip_comment(line), line);
    }

    #[test]
    fn strip_comment_after_unquoted_url() {
        // A real comment after an unquoted URL is delimited by whitespace.
        let line = "  media logo mizu://cdn.example.com/x.png // the logo";
        assert_eq!(strip_comment(line), "  media logo mizu://cdn.example.com/x.png ");
    }

    #[test]
    fn strip_comment_after_string() {
        // Comment follows a closed string — must be stripped.
        let line = r#"    placeholder "User" // default value"#;
        assert_eq!(strip_comment(line), r#"    placeholder "User" "#);
    }

    #[test]
    fn strip_escaped_quote_does_not_close_string() {
        // `\"` inside the string must not close the string context, so the
        // trailing `// comment` must NOT be stripped (it's inside the string).
        let line = r#"    text "she said \"//not a comment\"" // real comment"#;
        let stripped = strip_comment(line);
        // The real comment at the end should be gone; the // inside the
        // escaped string should survive.
        assert!(stripped.contains(r#"\"//not a comment\""#));
        assert!(!stripped.contains("// real comment"));
    }

    #[test]
    fn strip_empty_line_unchanged() {
        assert_eq!(strip_comment(""), "");
    }

    #[test]
    fn strip_line_with_no_comment_unchanged() {
        let line = "    width 100";
        assert_eq!(strip_comment(line), line);
    }

    // split_source — happy paths

    #[test]
    fn split_all_three_blocks_in_order() {
        let source = "\
logic
    tax(p: num) : p * 1.10
style
    .card
        padding 20
layout
    window \"App\"
";
        let parsed = split_source(source, Path::new(NO_IMPORT_DIR)).unwrap();
        assert_eq!(parsed.logic_block.trim(), "tax(p: num) : p * 1.10");
        assert!(parsed.style_block.contains(".card"));
        assert!(parsed.style_block.contains("padding 20"));
        assert_eq!(parsed.layout_block.trim(), "window \"App\"");
    }

    #[test]
    fn split_blocks_in_arbitrary_order() {
        // The spec says declaration order is free.
        let source = "\
layout
    window \"Dashboard\"
logic
    gross(p: num, q: num) : p * q
style
    .btn
        background #333333
";
        let parsed = split_source(source, Path::new(NO_IMPORT_DIR)).unwrap();
        assert!(parsed.logic_block.contains("gross"));
        assert!(parsed.style_block.contains(".btn"));
        assert!(parsed.layout_block.contains("window"));
    }

    #[test]
    fn split_only_logic_block() {
        let source = "\
logic
    netto(p: num) : p * 0.8
";
        let parsed = split_source(source, Path::new(NO_IMPORT_DIR)).unwrap();
        assert!(parsed.logic_block.contains("netto"));
        // Inactive buffers receive blank padding lines — use trim() to test for absence.
        assert!(parsed.style_block.trim().is_empty());
        assert!(parsed.layout_block.trim().is_empty());
        assert!(parsed.urls_block.trim().is_empty());
    }

    #[test]
    fn split_empty_source_produces_empty_blocks() {
        let parsed = split_source("", Path::new(NO_IMPORT_DIR)).unwrap();
        assert_eq!(
            parsed,
            ParsedSource {
                logic_block: String::new(),
                style_block: String::new(),
                layout_block: String::new(),
                urls_block: String::new(),
            }
        );
    }

    #[test]
    fn split_source_with_only_comments_and_blank_lines() {
        let source = "\
// this entire file is comments

// another comment
";
        let parsed = split_source(source, Path::new(NO_IMPORT_DIR)).unwrap();
        assert!(parsed.logic_block.is_empty());
        assert!(parsed.style_block.is_empty());
        assert!(parsed.layout_block.is_empty());
        assert!(parsed.urls_block.is_empty());
    }

    #[test]
    fn split_strips_inline_comments_from_content() {
        let source = "\
logic
    tax(p: num) : p * 1.10
";
        let parsed = split_source(source, Path::new(NO_IMPORT_DIR)).unwrap();
        assert!(
            !parsed.logic_block.contains("Japanese VAT"),
            "comment should be stripped"
        );
        assert!(
            parsed.logic_block.contains("p * 1.10"),
            "code should be preserved"
        );
    }

    #[test]
    fn split_preserves_comment_inside_string_in_layout() {
        let source = "\
layout
    text \"visit http://example.com\"
";
        let parsed = split_source(source, Path::new(NO_IMPORT_DIR)).unwrap();
        assert!(
            parsed.layout_block.contains("http://example.com"),
            "URL inside string must be preserved, got: {:?}",
            parsed.layout_block
        );
    }

    #[test]
    fn split_blank_lines_not_added_to_blocks() {
        let source = "\
logic

    tax(p: num) : p * 1.10

    gross(p: num, q: num) : p * q

";
        let parsed = split_source(source, Path::new(NO_IMPORT_DIR)).unwrap();
        // Both functions should be present; no blank lines between them in the
        // buffer (blank lines are skipped during accumulation).
        let lines: Vec<&str> = parsed.logic_block.lines().collect();
        assert_eq!(
            lines.len(),
            2,
            "expected exactly 2 non-blank content lines, got: {lines:?}"
        );
    }

    #[test]
    fn split_preserves_relative_indentation() {
        // Content lines keep their original indentation so Phase-3 parsers can
        // reconstruct the indentation tree.
        let source = "\
style
    .card
        padding 20
        background #ffffff
";
        let parsed = split_source(source, Path::new(NO_IMPORT_DIR)).unwrap();
        let lines: Vec<&str> = parsed.style_block.lines().collect();
        assert_eq!(lines[0], "    .card", "first line indentation");
        assert_eq!(lines[1], "        padding 20", "nested line indentation");
    }

    #[test]
    fn split_section_keyword_not_added_to_block() {
        // The `logic` / `style` / `layout` keyword line itself must not appear
        // inside the corresponding block buffer.
        let source = "\
logic
    f(x: num) : x
";
        let parsed = split_source(source, Path::new(NO_IMPORT_DIR)).unwrap();
        assert!(
            !parsed.logic_block.contains("logic"),
            "section keyword must not appear in the block buffer"
        );
    }

    #[test]
    fn split_urls_block_parsed_correctly() {
        let source = "\
urls
    api login /api/v1/login
    media logo mizu://cdn.example.com/logo.png
layout
    window \"App\"
";
        let parsed = split_source(source, Path::new(NO_IMPORT_DIR)).unwrap();
        assert!(
            parsed.urls_block.contains("api login"),
            "urls block must contain api entry: {:?}",
            parsed.urls_block
        );
        assert!(
            parsed.urls_block.contains("media logo"),
            "urls block must contain media entry: {:?}",
            parsed.urls_block
        );
        assert!(
            parsed.layout_block.contains("window"),
            "layout block must still be populated"
        );
    }

    #[test]
    fn split_blank_line_padding_aligns_offsets() {
        // After padding, every buffer has the same number of lines as the
        // number of total content lines across all blocks.
        let source = "\
logic
    a(x: num) : x
    b(x: num) : x
layout
    window \"App\"
";
        let parsed = split_source(source, Path::new(NO_IMPORT_DIR)).unwrap();
        let logic_lines = parsed.logic_block.lines().count();
        let layout_lines = parsed.layout_block.lines().count();
        // 2 logic content lines + 1 layout content line = 3 total dispatched lines.
        // Each buffer must have exactly 3 lines (real + padding).
        assert_eq!(logic_lines, 3, "logic_block line count");
        assert_eq!(layout_lines, 3, "layout_block line count");
    }

    // split_source — failure paths

    #[test]
    fn unindented_junk_text_returns_parse_error() {
        let source = "\
logic
    f(x: num) : x
unknown_token
";
        let result = split_source(source, Path::new(NO_IMPORT_DIR));
        assert!(
            matches!(result, Err(MizuError::ParseError(_))),
            "expected ParseError for root-level junk, got: {result:?}"
        );
        if let Err(MizuError::ParseError(msg)) = result {
            assert!(
                msg.contains("unknown_token"),
                "error should name the bad token"
            );
        }
    }

    #[test]
    fn indented_content_before_any_section_fails() {
        // Content at indentation > 0 before any section keyword is illegal.
        let source = "\
    orphaned_line
logic
    f(x: num) : x
";
        let result = split_source(source, Path::new(NO_IMPORT_DIR));
        assert!(
            matches!(result, Err(MizuError::ParseError(_))),
            "expected ParseError for orphaned indented content"
        );
    }

    // Import — happy paths (real filesystem via std::env::temp_dir)

    /// Writes `content` to `<temp_dir>/<name>` and returns the temp directory.
    fn write_temp_import(name: &str, content: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir();
        let path = dir.join(name);
        std::fs::write(&path, content).expect("test helper: write_temp_import");
        dir
    }

    #[test]
    fn import_mlg_injects_into_logic_block() {
        let mlg_content = "helper(x: num) : x * 2\n";
        let dir = write_temp_import("mizu_test_helper.mlg", mlg_content);

        let source = "\
import \"mizu_test_helper.mlg\"
logic
    main(x: num) : helper(x)
";
        let parsed = split_source(source, &dir).unwrap();
        assert!(
            parsed.logic_block.contains("helper(x: num) : x * 2"),
            "imported .mlg content must appear in logic_block: {:?}",
            parsed.logic_block
        );
        assert!(
            parsed.logic_block.contains("main(x: num) : helper(x)"),
            "inline logic must also appear in logic_block"
        );
    }

    #[test]
    fn import_mss_injects_into_style_block() {
        let mss_content = ".primary\n    background #0077cc\n    color #ffffff\n";
        let dir = write_temp_import("mizu_test_theme.mss", mss_content);

        let source = "\
import \"mizu_test_theme.mss\"
layout
    window \"App\"
";
        let parsed = split_source(source, &dir).unwrap();
        assert!(
            parsed.style_block.contains(".primary"),
            ".mss content must appear in style_block: {:?}",
            parsed.style_block
        );
        assert!(
            parsed.style_block.contains("#0077cc"),
            "hex color must be preserved"
        );
    }

    #[test]
    fn import_mss_comments_are_stripped() {
        let mss_content = ".card // a card class\n    padding 10 // ten pixels\n";
        let dir = write_temp_import("mizu_test_comments.mss", mss_content);

        let source = "import \"mizu_test_comments.mss\"\n";
        let parsed = split_source(source, &dir).unwrap();
        assert!(
            !parsed.style_block.contains("a card class"),
            "comments in imported file must be stripped"
        );
        assert!(
            parsed.style_block.contains(".card"),
            "class name must survive stripping"
        );
    }

    #[test]
    fn import_can_appear_between_sections() {
        let mss_content = ".footer\n    margin 0\n";
        let dir = write_temp_import("mizu_test_between.mss", mss_content);

        let source = "\
logic
    f(x: num) : x
import \"mizu_test_between.mss\"
layout
    window \"App\"
";
        let parsed = split_source(source, &dir).unwrap();
        assert!(
            parsed.style_block.contains(".footer"),
            ".mss import between sections must still inject into style_block"
        );
    }

    // Import — failure paths

    #[test]
    fn import_invalid_extension_fails() {
        let dir = write_temp_import("mizu_test_bad.txt", "some content");

        let source = "import \"mizu_test_bad.txt\"\n";
        let result = split_source(source, &dir);
        assert!(
            matches!(result, Err(MizuError::ParseError(_))),
            "expected ParseError for .txt import, got: {result:?}"
        );
        if let Err(MizuError::ParseError(msg)) = result {
            assert!(
                msg.contains(".txt") || msg.contains("not permitted"),
                "error should mention the bad extension: {msg}"
            );
        }
    }

    #[test]
    fn import_nested_import_inside_mss_fails() {
        // A .mss file that itself contains an `import` directive must be
        // rejected immediately, before its content touches any buffer.
        let nested_content = "import \"other.mss\"\n.card\n    padding 5\n";
        let dir = write_temp_import("mizu_test_nested.mss", nested_content);

        let source = "import \"mizu_test_nested.mss\"\n";
        let result = split_source(source, &dir);
        assert!(
            matches!(result, Err(MizuError::ParseError(_))),
            "expected ParseError for nested import, got: {result:?}"
        );
        if let Err(MizuError::ParseError(msg)) = result {
            assert!(
                msg.to_lowercase().contains("nested"),
                "error message should mention 'nested': {msg}"
            );
        }
    }

    #[test]
    fn import_nested_import_inside_mlg_fails() {
        let nested_content = "import \"shared.mlg\"\nhelper(x: num) : x\n";
        let dir = write_temp_import("mizu_test_nested_logic.mlg", nested_content);

        let source = "import \"mizu_test_nested_logic.mlg\"\n";
        let result = split_source(source, &dir);
        assert!(
            matches!(result, Err(MizuError::ParseError(_))),
            "expected ParseError for nested import in .mlg, got: {result:?}"
        );
    }

    #[test]
    fn import_missing_file_returns_parse_error_with_io_context() {
        // The splitter wraps io::Error inside ParseError for import failures,
        // providing the source line number as context.
        let source = "import \"__nonexistent_fixture_xyz__.mss\"\n";
        let result = split_source(source, Path::new(NO_IMPORT_DIR));
        assert!(
            matches!(result, Err(MizuError::ParseError(_))),
            "missing file import should return ParseError (wrapping io context): {result:?}"
        );
    }

    #[test]
    fn import_unquoted_path_fails() {
        let source = "import styles.mss\n";
        let result = split_source(source, Path::new(NO_IMPORT_DIR));
        assert!(
            matches!(result, Err(MizuError::ParseError(_))),
            "unquoted import path must fail, got: {result:?}"
        );
    }

    #[test]
    fn import_empty_quoted_path_fails() {
        let source = "import \"\"\n";
        let result = split_source(source, Path::new(NO_IMPORT_DIR));
        assert!(
            matches!(result, Err(MizuError::ParseError(_))),
            "empty quoted import path must fail"
        );
    }

    // Origin trust boundary (network includes + local traversal)

    #[test]
    fn network_origin_rejects_import() {
        // A network-delivered document must not be able to read local files.
        let source = "import \"../../secret.mlg\"\nlogic\n    f(x: num) : x\n";
        let result = split_source_with_origin(source, Path::new(NO_IMPORT_DIR), Origin::Network);
        match result {
            Err(MizuError::ParseError(msg)) => {
                assert!(
                    msg.contains("network-delivered"),
                    "error should mention the network trust boundary: {msg}"
                );
            }
            other => panic!("expected ParseError for network import, got: {other:?}"),
        }
    }

    #[test]
    fn network_origin_rejects_include() {
        let source = "include \"theme.mss\"\nlayout\n    window \"App\"\n";
        let result = split_source_with_origin(source, Path::new(NO_IMPORT_DIR), Origin::Network);
        assert!(
            matches!(result, Err(MizuError::ParseError(_))),
            "expected ParseError for network include, got: {result:?}"
        );
    }

    #[test]
    fn local_import_traversal_outside_dir_fails() {
        // Create a nested document directory and a sibling file *outside* it.
        // An import that escapes the document directory via `../` must fail.
        let base = std::env::temp_dir().join("mizu_traversal_test");
        let doc_dir = base.join("docdir");
        std::fs::create_dir_all(&doc_dir).expect("create doc dir");
        let outside = base.join("outside.mss");
        std::fs::write(&outside, ".x\n    padding 1\n").expect("write outside file");

        let source = "import \"../outside.mss\"\nlayout\n    window \"App\"\n";
        let result = split_source_with_origin(source, &doc_dir, Origin::LocalFile);
        match result {
            Err(MizuError::ParseError(msg)) => {
                assert!(
                    msg.contains("escapes") || msg.contains("traversal"),
                    "error should mention traversal: {msg}"
                );
            }
            other => panic!("expected ParseError for traversal, got: {other:?}"),
        }
    }

    #[test]
    fn local_include_same_directory_succeeds() {
        // A legitimate include living in the document's own directory is allowed.
        let dir = write_temp_import("mizu_test_include_ok.mss", ".legit\n    margin 2\n");
        let source = "include \"mizu_test_include_ok.mss\"\nlayout\n    window \"App\"\n";
        let parsed = split_source_with_origin(source, &dir, Origin::LocalFile)
            .expect("legitimate same-directory include must succeed");
        assert!(
            parsed.style_block.contains(".legit"),
            "included .mss content must appear in style_block: {:?}",
            parsed.style_block
        );
    }

    #[test]
    fn import_bare_keyword_alone_fails() {
        // `import` with no argument is treated as root-level junk.
        // (it doesn't start with `import ` with a space)
        let source = "import\n";
        let result = split_source(source, Path::new(NO_IMPORT_DIR));
        assert!(
            matches!(result, Err(MizuError::ParseError(_))),
            "bare `import` keyword must fail as junk token"
        );
    }
}
