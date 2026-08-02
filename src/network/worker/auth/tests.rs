//! Tests for the auth module.

use super::*;

#[test]
fn strip_list_covers_the_required_names_case_insensitively() {
    for name in ["Mizu-Auth-Set", "mizu-auth-set", "MIZU-AUTH-SET"] {
        assert!(is_stripped_response_header(name), "{name} must be stripped");
    }
    for name in ["Set-Cookie", "set-cookie"] {
        assert!(is_stripped_response_header(name), "{name} must be stripped");
    }
    for name in ["WWW-Authenticate", "www-authenticate"] {
        assert!(is_stripped_response_header(name), "{name} must be stripped");
    }
}

#[test]
fn strip_list_covers_the_mizu_prefix_wholesale() {
    assert!(is_stripped_response_header("Mizu-Anything-Else"));
    assert!(is_stripped_response_header("mizu-future-signal"));
}

#[test]
fn strip_list_does_not_reject_ordinary_headers() {
    for name in ["Content-Type", "X-Custom", "Location", "Date"] {
        assert!(
            !is_stripped_response_header(name),
            "{name} must not be treated as runtime-reserved"
        );
    }
}

/// Full-pipeline regression: a 200 response carrying `Mizu-Auth-Set`
/// alongside a JSON body must still (a) import the vault token — the
/// auth mechanism keeps working — and (b) never let that token, or any
/// other stripped header, appear in the `Value` eventually bound to the
/// `NetworkCall`'s `target_var`. `parse_body_value` takes only the body
/// bytes (no `HeaderMap`), so this holds by construction today; this
/// test pins that invariant so it fails loudly if that signature ever
/// changes to also thread headers through.
#[test]
fn mizu_auth_set_never_reaches_the_target_var_value() {
    use crate::network::worker::fetch::parse_body_value;

    let distinctive_token = "super-secret-token-should-never-leak-into-a-value";
    let mut headers = http::HeaderMap::new();
    headers.insert(
        "mizu-auth-set",
        http::HeaderValue::from_str(&format!(
            "{distinctive_token} SCOPE=GET EXP={}",
            u64::MAX / 2
        ))
        .unwrap(),
    );
    headers.insert(
        "set-cookie",
        http::HeaderValue::from_str("session=abc").unwrap(),
    );

    let body = br#"{"ok":true}"#;
    let domain = "mizu-auth-strip-test.example";

    let redirect = parse_http_response(http::StatusCode::OK, &headers, body, domain).unwrap();
    assert!(redirect.is_none());

    let value = parse_body_value(body);
    let rendered = value.to_string();
    assert!(
        !rendered.contains(distinctive_token),
        "the vault token must never appear in the value bound to target_var"
    );
    assert!(!rendered.contains("session=abc"));
    assert_eq!(rendered, String::from_utf8_lossy(body));

    // Auth processing itself still worked — the vault entry was written.
    // The OS keyring backend is not guaranteed available in every test
    // environment (see `vault::tests::test_token_rotation_and_explicit_revocation`
    // for the same defensive pattern), so this half of the check is
    // best-effort; the no-leak assertions above are unconditional.
    let vault_domain = crate::core::storage::ValidatedDomain::from_raw(domain);
    if let Ok(Some(entry)) = crate::network::vault::VaultEntry::load(&vault_domain) {
        assert_eq!(entry.token, distinctive_token);
    }
    let _ = crate::network::vault::VaultEntry::delete(&vault_domain);
}
