//! Tests for the urls module.

use super::*;
use crate::core::types::StringInterner;

fn parse(content: &str) -> Result<UrlRegistry, MizuError> {
    let mut interner = StringInterner::new();
    parse_urls(content, &mut interner)
}

fn parse_with_interner<'a>(
    content: &str,
    interner: &'a mut StringInterner,
) -> Result<UrlRegistry, MizuError> {
    parse_urls(content, interner)
}

// ── Happy paths ──────────────────────────────────────────────────────────

#[test]
fn parse_single_api_endpoint() {
    let registry = parse("    api login /api/v1/login\n").unwrap();
    assert_eq!(registry.len(), 1);
    let entry = registry.values().next().unwrap();
    assert_eq!(entry.kind, EndpointKind::Api);
    assert_eq!(entry.raw_target, "/api/v1/login");
}

#[test]
fn parse_single_media_endpoint() {
    let registry = parse("    media logo mizu://cdn.example.com/logo.png\n").unwrap();
    assert_eq!(registry.len(), 1);
    let entry = registry.values().next().unwrap();
    assert_eq!(entry.kind, EndpointKind::Media);
    assert_eq!(entry.raw_target, "mizu://cdn.example.com/logo.png");
}

#[test]
fn parse_multiple_endpoints() {
    let content = "\
    api login /api/v1/login
    api profile /api/v1/profile
    media logo mizu://cdn.example.com/logo.png
";
    let registry = parse(content).unwrap();
    assert_eq!(registry.len(), 3);
}

#[test]
fn parse_blank_lines_are_skipped() {
    // Blank padding lines from the splitter must be silently skipped.
    let content = "\n    api health /health\n\n";
    let registry = parse(content).unwrap();
    assert_eq!(registry.len(), 1);
}

#[test]
fn parse_alias_is_interned_consistently() {
    let mut interner = StringInterner::new();
    let registry = parse_with_interner("    api login /api/v1/login\n", &mut interner).unwrap();
    // The symbol for "login" in the registry must equal the one in the
    // interner (same string → same symbol).
    let expected_sym = interner.get("login").expect("login must be interned");
    assert!(
        registry.contains_key(&expected_sym),
        "registry must be keyed by the interned symbol for `login`"
    );
}

#[test]
fn empty_content_produces_empty_registry() {
    let registry = parse("").unwrap();
    assert!(registry.is_empty());
}

// ── Compile-time guards ───────────────────────────────────────────────────

#[test]
fn api_without_leading_slash_fails() {
    let result = parse("    api bad api/v1/oops\n");
    assert!(
        matches!(result, Err(MizuError::ParseError(_))),
        "api with non-relative path must fail"
    );
    if let Err(MizuError::ParseError(msg)) = result {
        assert!(
            msg.contains("must start with `/`"),
            "error must explain the constraint: {msg}"
        );
    }
}

#[test]
fn media_without_mizu_scheme_fails() {
    let result = parse("    media img https://cdn.example.com/x.png\n");
    assert!(
        matches!(result, Err(MizuError::ParseError(_))),
        "media with non-mizu URL must fail"
    );
    if let Err(MizuError::ParseError(msg)) = result {
        assert!(
            msg.contains("must start with `mizu://`"),
            "error must explain the constraint: {msg}"
        );
    }
}

#[test]
fn unknown_keyword_fails() {
    let result = parse("    fetch data /foo\n");
    assert!(
        matches!(result, Err(MizuError::ParseError(_))),
        "unknown keyword must fail"
    );
    if let Err(MizuError::ParseError(msg)) = result {
        assert!(msg.contains("unknown endpoint keyword"), "error: {msg}");
    }
}

#[test]
fn missing_alias_fails() {
    let result = parse("    api\n");
    assert!(
        matches!(result, Err(MizuError::ParseError(_))),
        "missing alias must fail"
    );
}

#[test]
fn missing_target_fails() {
    let result = parse("    api login\n");
    assert!(
        matches!(result, Err(MizuError::ParseError(_))),
        "missing target must fail"
    );
}

#[test]
fn duplicate_alias_fails() {
    let content = "\
    api login /api/v1/login
    api login /api/v1/login2
";
    let result = parse(content);
    assert!(
        matches!(result, Err(MizuError::ParseError(_))),
        "duplicate alias must fail"
    );
    if let Err(MizuError::ParseError(msg)) = result {
        assert!(msg.contains("duplicate alias"), "error: {msg}");
    }
}
