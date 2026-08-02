//! `split_source`/`split_source_with_origin` (the section-dispatch state
//! machine) and `process_import` (splicing a single `import` directive's
//! target file into the right buffer).

use std::path::Path;

use crate::core::errors::MizuError;

use super::helpers::{
    check_no_nested_imports, is_import_directive, parse_import_path, push_line,
    resolve_import_target, strip_comment,
};
use super::types::{ActiveBlock, ImportTarget, Origin, ParsedSource};

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
///     doc "App"
/// "#;
///
/// let parsed = split_source(source, Path::new(".")).unwrap();
/// assert!(parsed.logic_block.contains("tax"));
/// assert!(parsed.layout_block.contains("doc"));
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
    let import_path = parse_import_path(trimmed_line)
        .map_err(|e| MizuError::ParseError(format!("line {line_number}: {e}")))?;

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
