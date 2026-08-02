//! Tests for the splitter module.

use super::helpers::strip_comment;
use super::{Origin, ParsedSource, split_source, split_source_with_origin};
use crate::core::errors::MizuError;
use std::path::Path;

const NO_IMPORT_DIR: &str = ".";

// strip_comment unit tests

#[test]
fn strip_bare_comment() {
    assert_eq!(strip_comment(";; full line comment"), "");
}

#[test]
fn strip_trailing_comment() {
    assert_eq!(
        strip_comment("    tax(p: num) : p * 1.10 ;; apply VAT"),
        "    tax(p: num) : p * 1.10 "
    );
}

#[test]
fn strip_preserves_url_inside_string() {
    // `;;` inside a string literal must NOT be treated as a comment.
    let line = r#"    text "visit http://example.com for info""#;
    assert_eq!(strip_comment(line), line);
}

#[test]
fn strip_preserves_unquoted_mizu_url() {
    // `urls` block targets are unquoted: the `:` in `mizu://` scheme must not
    // start a comment (the `;;` token must be preceded by whitespace, not just `:` or `/`).
    let line = "  media logo mizu://cdn.example.com/logo.png";
    assert_eq!(strip_comment(line), line);
}

#[test]
fn strip_comment_after_unquoted_url() {
    // A real comment after an unquoted URL is delimited by whitespace.
    let line = "  media logo mizu://cdn.example.com/x.png ;; the logo";
    assert_eq!(
        strip_comment(line),
        "  media logo mizu://cdn.example.com/x.png "
    );
}

#[test]
fn strip_comment_after_string() {
    // Comment follows a closed string — must be stripped.
    let line = r#"    placeholder "User" ;; default value"#;
    assert_eq!(strip_comment(line), r#"    placeholder "User" "#);
}

#[test]
fn strip_escaped_quote_does_not_close_string() {
    // `\"` inside the string must not close the string context, so the
    // trailing `;; comment` must NOT be stripped (it's inside the string).
    let line = r#"    text "she said \";;not a comment\"" ;; real comment"#;
    let stripped = strip_comment(line);
    // The real comment at the end should be gone; the ;; inside the
    // escaped string should survive.
    assert!(stripped.contains(r#"\";;not a comment\""#));
    assert!(!stripped.contains(";; real comment"));
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
    doc \"App\"
";
    let parsed = split_source(source, Path::new(NO_IMPORT_DIR)).unwrap();
    assert_eq!(parsed.logic_block.trim(), "tax(p: num) : p * 1.10");
    assert!(parsed.style_block.contains(".card"));
    assert!(parsed.style_block.contains("padding 20"));
    assert_eq!(parsed.layout_block.trim(), "doc \"App\"");
}

#[test]
fn split_blocks_in_arbitrary_order() {
    // The spec says declaration order is free.
    let source = "\
layout
    doc \"Dashboard\"
logic
    gross(p: num, q: num) : p * q
style
    .btn
        background #333333
";
    let parsed = split_source(source, Path::new(NO_IMPORT_DIR)).unwrap();
    assert!(parsed.logic_block.contains("gross"));
    assert!(parsed.style_block.contains(".btn"));
    assert!(parsed.layout_block.contains("doc"));
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
;; this entire file is comments

;; another comment
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
    doc \"App\"
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
        parsed.layout_block.contains("doc"),
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
    let mss_content = ".card ;; a card class\n    padding 10 ;; ten pixels\n";
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
