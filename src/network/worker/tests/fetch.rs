//! Tests for `fetch.rs`: lossy UTF-8 body decoding, the response-body size
//! ceiling, and the `file://` sandbox (`handle_fetch_file`/`handle_fetch_raw`).

use super::*;

// — lossy UTF-8 body decoding (parse_body_value)

#[test]
fn test_parse_body_value_valid_utf8() {
    let val = parse_body_value(b"hello world");
    assert!(
        val.budget_eq(
            &crate::core::types::Value::from("hello world".to_string()),
            &mut u64::MAX,
            u64::MAX
        )
        .unwrap_or(false)
    );
}

#[test]
fn test_parse_body_value_invalid_utf8_replaced_with_replacement_char() {
    // 0xFF is not valid UTF-8 — must be replaced with U+FFFD, not panic.
    let val = parse_body_value(b"hello \xff world");
    match val {
        crate::core::types::Value::String(ref s) => {
            assert!(
                s.contains('\u{FFFD}'),
                "invalid bytes must be replaced with U+FFFD, got: {s:?}"
            );
            assert!(s.contains("hello"), "valid prefix must be preserved");
        }
        other => panic!("expected Value::String, got {other:?}"),
    }
}

#[test]
fn test_parse_body_value_empty_body() {
    let val = parse_body_value(b"");
    assert!(
        val.budget_eq(
            &crate::core::types::Value::from(String::new()),
            &mut u64::MAX,
            u64::MAX
        )
        .unwrap_or(false)
    );
}

#[test]
fn test_parse_body_value_all_bytes_no_panic() {
    // Full 0..=255 range — must return Value::String without panicking.
    let body: Vec<u8> = (0u8..=255u8).collect();
    let val = parse_body_value(&body);
    assert!(
        matches!(val, crate::core::types::Value::String(_)),
        "arbitrary byte payloads must yield Value::String"
    );
}

// — response body size ceiling (check_response_body_budget)

#[test]
fn test_response_body_budget_allows_under_limit() {
    assert!(check_response_body_budget(0, 1024).is_ok());
    assert!(check_response_body_budget(MAX_RESPONSE_BODY_BYTES - 1, 1).is_ok());
}

#[test]
fn test_response_body_budget_rejects_over_limit() {
    let result = check_response_body_budget(MAX_RESPONSE_BODY_BYTES, 1);
    assert!(
        matches!(result, Err(MizuError::SecurityViolation(_))),
        "exceeding the body ceiling must yield SecurityViolation (non-retryable): {result:?}"
    );
}

#[test]
fn test_response_body_budget_no_overflow_panic() {
    // usize::MAX incoming must saturate, not wrap around to a small value.
    let result = check_response_body_budget(1, usize::MAX);
    assert!(
        matches!(result, Err(MizuError::SecurityViolation(_))),
        "saturating add must still reject: {result:?}"
    );
}

#[test]
fn test_parse_body_value_multibyte_utf8_preserved() {
    // Valid multi-byte UTF-8 (e.g. Japanese) must round-trip without replacement.
    let text = "こんにちは世界";
    let val = parse_body_value(text.as_bytes());
    match val {
        crate::core::types::Value::String(ref s) => {
            assert_eq!(s.as_ref(), text, "valid UTF-8 must be preserved exactly");
            assert!(!s.contains('\u{FFFD}'), "no replacement chars expected");
        }
        other => panic!("expected Value::String, got {other:?}"),
    }
}

#[test]
fn test_uri_parsing_for_navigate() {
    let uri = MizuUri::parse("mizu://localhost/index.mizu").unwrap();
    assert_eq!(uri.domain, "localhost");
    assert_eq!(uri.path, "/index.mizu");
}

#[tokio::test]
async fn test_file_scheme_always_rejected_by_h3_fetch() {
    // handle_fetch_raw must never serve file:// — those go through
    // handle_fetch_file (sandbox-enforced).
    let provider = rustls::crypto::aws_lc_rs::default_provider();
    let _ = provider.install_default();
    let endpoint = Endpoint::client(std::net::SocketAddr::from(([0, 0, 0, 0], 0))).unwrap();
    let pool = H3ConnectionPool::new();
    let dns = crate::network::dns::build_dns_resolver();

    for is_remote_origin in [false, true] {
        let result = handle_fetch_raw(
            &endpoint,
            &pool,
            &dns,
            "GET",
            "file:///etc/passwd",
            is_remote_origin,
            None,
            None,
            &[],
            false,
        )
        .await;
        assert!(
            matches!(result, Err(MizuError::SecurityViolation(_))),
            "file:// must be rejected by the H3 fetch path \
                 (is_remote_origin={is_remote_origin}): {result:?}"
        );
    }
}

#[test]
fn test_file_url_path_traversal_blocked_in_fetch_file() {
    // handle_fetch_file must block traversal attempts even when sandbox_base is provided.
    let result = handle_fetch_file(
        "file:///home/user/app/../../etc/passwd",
        Some("home/user/app"),
    );
    assert!(
        matches!(result, Err(MizuError::SecurityViolation(_))),
        "path traversal must be blocked by file_sandbox_contains, got: {result:?}"
    );
}

#[test]
fn test_file_fetch_no_sandbox_base_blocked() {
    // No sandbox_base configured → all file:// access denied.
    let result = handle_fetch_file("file:///home/user/app/image.png", None);
    assert!(
        matches!(result, Err(MizuError::SecurityViolation(_))),
        "file:// with no sandbox_base must be denied: {result:?}"
    );
}
