//! Tests for the fetch module.

use super::*;

fn uri() -> MizuUri {
    MizuUri::parse("mizu://example.com/api/submit").unwrap()
}

#[test]
fn custom_header_reaches_the_built_request() {
    let req = build_h3_request(
        &uri(),
        "POST",
        None,
        None,
        &[("X-Idempotency-Key".to_string(), "abc-123".to_string())],
    )
    .unwrap();
    assert_eq!(req.headers().get("x-idempotency-key").unwrap(), "abc-123");
}

#[test]
fn content_type_is_set_from_format() {
    let req = build_h3_request(
        &uri(),
        "POST",
        None,
        Some("application/yaml".to_string()),
        &[],
    )
    .unwrap();
    assert_eq!(
        req.headers().get(http::header::CONTENT_TYPE).unwrap(),
        "application/yaml"
    );
}

#[test]
fn no_content_type_when_body_less() {
    let req = build_h3_request(&uri(), "GET", None, None, &[]).unwrap();
    assert!(req.headers().get(http::header::CONTENT_TYPE).is_none());
}

#[test]
fn vault_entry_sets_authorization_bearer() {
    let entry = VaultEntry {
        token: "tok123".to_string(),
        allowed_methods: vec!["GET".to_string()],
        exp: u64::MAX,
    };
    let req = build_h3_request(&uri(), "GET", Some(&entry), None, &[]).unwrap();
    assert_eq!(
        req.headers().get(http::header::AUTHORIZATION).unwrap(),
        "Bearer tok123"
    );
}

#[test]
fn header_value_with_crlf_is_rejected_before_any_request_is_built() {
    // A value containing CR/LF must fail via `HeaderValue`'s own
    // constructor — the request is never sent, not sanitised.
    let err = build_h3_request(
        &uri(),
        "POST",
        None,
        None,
        &[("X-Evil".to_string(), "line1\r\nX-Injected: yes".to_string())],
    )
    .unwrap_err();
    assert!(matches!(err, MizuError::Network(_)));
}

#[test]
fn multiple_custom_headers_all_reach_the_request() {
    let req = build_h3_request(
        &uri(),
        "POST",
        None,
        None,
        &[
            ("X-Foo".to_string(), "foo-value".to_string()),
            ("X-Bar".to_string(), "bar-value".to_string()),
        ],
    )
    .unwrap();
    assert_eq!(req.headers().get("x-foo").unwrap(), "foo-value");
    assert_eq!(req.headers().get("x-bar").unwrap(), "bar-value");
}
