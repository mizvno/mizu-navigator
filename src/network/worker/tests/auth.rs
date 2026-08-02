//! Tests for `auth.rs`: `Mizu-Auth-Set` token handling and HTTP response
//! classification (`parse_http_response`).

use super::*;

/// An attacker cannot inject a `Mizu-Auth-Set` token by embedding the
/// header syntax in the *body* of a 200 response: the parser only reads
/// the HTTP header map, never the body.
#[test]
fn test_prevent_token_injection_in_payload() {
    let mut headers = http::HeaderMap::new();
    // No `Mizu-Auth-Set` header — only the body contains the injection attempt.
    let body = b"Payload data containing Mizu-Auth-Set: hacker_token SCOPE=GET EXP=9999999999";
    let result = parse_http_response(http::StatusCode::OK, &headers, body, "test_domain.local");

    assert!(result.is_ok(), "200 response must succeed: {result:?}");
    // Body-injected token must NOT have reached the vault.
    let td = crate::core::storage::ValidatedDomain::from_raw("test_domain.local");
    if let Ok(Some(entry)) = VaultEntry::load(&td) {
        assert_ne!(
            entry.token, "hacker_token",
            "body-injected token must not be stored in the vault"
        );
    }

    // Also verify that a Mizu-Auth-Set header WITH the hacker token in the
    // header map IS processed, but only when sent as an actual HTTP header.
    let future_exp = std::time::SystemTime::UNIX_EPOCH
        .elapsed()
        .map(|d| d.as_secs() + 3600)
        .unwrap_or(9_999_999_999);
    let auth_val = format!("legit_token SCOPE=GET EXP={future_exp}");
    headers.insert(
        http::HeaderName::from_static("mizu-auth-set"),
        http::HeaderValue::from_str(&auth_val).unwrap(),
    );
    let result2 = parse_http_response(http::StatusCode::OK, &headers, b"ok", "test_domain2.local");
    assert!(
        result2.is_ok(),
        "valid Mizu-Auth-Set header must not error: {result2:?}"
    );
}

#[test]
fn test_expired_token_is_not_sent() {
    // Expiry detection is pure logic — no keyring needed.
    let past_exp = VaultEntry {
        token: "must_not_be_sent".to_string(),
        allowed_methods: vec!["GET".to_string()],
        exp: 1, // 1970-01-01 — definitively in the past
    };
    assert!(
        past_exp.is_expired(),
        "entry with exp=1 must be detected as expired"
    );

    let future_exp_secs = std::time::SystemTime::UNIX_EPOCH
        .elapsed()
        .map(|d| d.as_secs() + 3600)
        .unwrap_or(9_999_999_999);
    let fresh = VaultEntry {
        token: "ok".to_string(),
        allowed_methods: vec!["GET".to_string()],
        exp: future_exp_secs,
    };
    assert!(
        !fresh.is_expired(),
        "entry with future exp must not be expired"
    );

    // If the keyring round-trips in this environment, verify end-to-end eviction.
    let domain_raw = "expired-send-test.mizu.test";
    let vd = crate::core::storage::ValidatedDomain::from_raw(domain_raw);
    VaultEntry::save(&vd, &past_exp).expect("save must not error");
    let roundtrip = VaultEntry::load(&vd)
        .ok()
        .flatten()
        .map(|e| e.token == "must_not_be_sent")
        .unwrap_or(false);

    if roundtrip {
        // load_valid_entry must reject with SecurityViolation and evict the token.
        let result = load_valid_entry(&vd, "GET");
        assert!(
            matches!(result, Err(MizuError::SecurityViolation(_))),
            "expired token must cause SecurityViolation: {result:?}"
        );
        let after = VaultEntry::load(&vd).expect("load after eviction must not error");
        assert!(
            after.is_none(),
            "expired token must be evicted from vault: {after:?}"
        );
    } else {
        VaultEntry::delete(&vd).ok();
    }
}

/// 200 OK with no auth header must return Ok(None) — success, no redirect.
#[test]
fn test_http_200_is_success() {
    let headers = http::HeaderMap::new();
    let result = parse_http_response(http::StatusCode::OK, &headers, b"hello", "x.local");
    assert_eq!(result.unwrap(), None, "200 must yield Ok(None)");
}

/// 4xx responses must map to MizuError::Network with the body as message.
#[test]
fn test_http_404_is_error() {
    let headers = http::HeaderMap::new();
    let result = parse_http_response(
        http::StatusCode::NOT_FOUND,
        &headers,
        b"not found",
        "x.local",
    );
    assert!(
        matches!(result, Err(MizuError::Network(ref msg)) if msg == "not found"),
        "404 must yield MizuError::Network with body text: {result:?}"
    );
}

/// 500 responses must also map to MizuError::Network.
#[test]
fn test_http_500_is_error() {
    let headers = http::HeaderMap::new();
    let result = parse_http_response(
        http::StatusCode::INTERNAL_SERVER_ERROR,
        &headers,
        b"server exploded",
        "x.local",
    );
    assert!(
        matches!(result, Err(MizuError::Network(_))),
        "500 must yield MizuError::Network: {result:?}"
    );
}

/// 3xx responses must return Ok(Some(url)) with the body as the new URL.
#[test]
fn test_http_301_absolute_redirect() {
    let headers = http::HeaderMap::new();
    let result = parse_http_response(
        http::StatusCode::MOVED_PERMANENTLY,
        &headers,
        b"mizu://other.local/page",
        "origin.local",
    );
    assert_eq!(
        result.unwrap(),
        Some("mizu://other.local/page".to_string()),
        "absolute redirect URL must pass through unchanged"
    );
}

/// Relative redirect (no scheme) must be prepended with `mizu://<domain>`.
#[test]
fn test_http_302_relative_redirect_gets_domain_prefix() {
    let headers = http::HeaderMap::new();
    let result = parse_http_response(
        http::StatusCode::FOUND,
        &headers,
        b"/new/path",
        "example.local",
    );
    assert_eq!(
        result.unwrap(),
        Some("mizu://example.local/new/path".to_string()),
        "relative redirect must be prefixed with mizu://<domain>"
    );
}

/// Redirect via Location header must be preferred over the body.
#[test]
fn test_http_302_redirect_via_location_header() {
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::LOCATION,
        http::HeaderValue::from_static("/header-path"),
    );
    let result = parse_http_response(
        http::StatusCode::FOUND,
        &headers,
        b"/body-path",
        "example.local",
    );
    assert_eq!(
        result.unwrap(),
        Some("mizu://example.local/header-path".to_string()),
        "Location header must take precedence over body"
    );
}

/// Redirection with empty location and body must yield a Network error.
#[test]
fn test_http_302_empty_redirect_yields_error() {
    let headers = http::HeaderMap::new();
    let result = parse_http_response(http::StatusCode::FOUND, &headers, b"", "example.local");
    assert!(
        matches!(result, Err(MizuError::Network(ref msg)) if msg.contains("Empty redirect destination")),
        "empty redirect must yield MizuError::Network: {result:?}"
    );
}

/// `parse_mizu_auth_set_header` must correctly parse a well-formed value.
#[test]
fn test_mizu_auth_set_header_parsed_ok() {
    let auth = parse_mizu_auth_set_header("tok123 SCOPE=GET,POST EXP=9999999999")
        .expect("valid header must parse");
    assert_eq!(auth.token, "tok123");
    assert_eq!(auth.scope, vec!["GET", "POST"]);
    assert_eq!(auth.exp, Some(9_999_999_999));
}

/// Auth header with no EXP field must be stored without exp.
#[test]
fn test_mizu_auth_set_header_missing_exp_is_none() {
    let auth = parse_mizu_auth_set_header("tok SCOPE=GET").expect("should parse");
    assert_eq!(auth.exp, None);
}

/// `process_mizu_auth_set` must reject a header without EXP.
#[test]
fn test_mizu_auth_set_missing_exp_rejected() {
    let result = process_mizu_auth_set("tok SCOPE=GET", "no-exp.local");
    assert!(
        matches!(result, Err(MizuError::SecurityViolation(_))),
        "missing EXP must yield SecurityViolation: {result:?}"
    );
}

/// `process_mizu_auth_set` must reject already-expired tokens.
#[test]
fn test_mizu_auth_set_expired_token_rejected() {
    let result = process_mizu_auth_set("tok SCOPE=GET EXP=1", "expired.local");
    assert!(
        matches!(result, Err(MizuError::SecurityViolation(_))),
        "expired token (EXP=1) must yield SecurityViolation: {result:?}"
    );
}

/// `process_mizu_auth_set` must reject tokens whose entire scope is outside
/// the permitted-methods ceiling.
#[test]
fn test_mizu_auth_set_scope_ceiling_rejects_unknown_methods() {
    let future_exp = std::time::SystemTime::UNIX_EPOCH
        .elapsed()
        .map(|d| d.as_secs() + 3600)
        .unwrap_or(9_999_999_999);
    let header = format!("tok SCOPE=HACK,TRACE EXP={future_exp}");
    let result = process_mizu_auth_set(&header, "ceiling.local");
    assert!(
        matches!(result, Err(MizuError::SecurityViolation(_))),
        "scope with only forbidden methods must yield SecurityViolation: {result:?}"
    );
}
