//! `Mizu-Auth-Set` header parsing and vault token import.

use crate::core::errors::MizuError;
use crate::network::vault::VaultEntry;

/// HTTP methods the Mizu runtime permits a vault token to authorise.
///
/// Server-declared scopes are intersected with this list at import time so
/// that a compromised server can never grant a method the client has not
/// explicitly whitelisted.
const PERMITTED_HTTP_METHODS: &[&str] = &["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD"];

/// Maximum time-to-live (seconds) for tokens imported via `Mizu-Auth-Set`.
///
/// Server-provided `EXP` values beyond `now + MAX_TOKEN_TTL_SECS` are capped,
/// preventing indefinitely-lived tokens.
///
/// An unmeasured starting value, overridable for a single run via
/// `MIZU_MAX_TOKEN_TTL_SECS` (see the module doc on [`crate::core::config`]).
static MAX_TOKEN_TTL_SECS: std::sync::LazyLock<u64> = std::sync::LazyLock::new(|| {
    crate::core::config::env_override("MIZU_MAX_TOKEN_TTL_SECS", 86_400) // 24 hours
});

/// Loads the vault entry for `domain`, verifies it has not expired, and checks
/// that `method` is within scope.
///
/// On expiry the stale entry is evicted before [`MizuError::SecurityViolation`]
/// is returned.  Returns `Ok(None)` when no entry exists for `domain`.
pub(super) fn load_valid_entry(
    domain: &crate::core::storage::ValidatedDomain,
    method: &str,
) -> Result<Option<VaultEntry>, MizuError> {
    let Some(entry) = VaultEntry::load(domain)? else {
        return Ok(None);
    };
    if entry.is_expired() {
        tracing::warn!(
            domain = %domain.as_str(),
            "bearer token expired; evicting from vault"
        );
        VaultEntry::delete(domain)?;
        return Err(MizuError::SecurityViolation(format!(
            "bearer token for '{}' expired; evicted — re-authenticate",
            domain.as_str()
        )));
    }
    entry.check_scope(method)?;
    Ok(Some(entry))
}

/// Response headers that must never become visible to document logic —
/// i.e. must never end up inside the [`crate::core::types::Value`] bound to
/// a `NetworkCall`'s `target_var` — regardless of any future change that
/// starts surfacing response headers to documents.
///
/// This generalizes the pre-existing "`Mizu-Auth-Set` is consumed here and
/// never forwarded" pattern into a single, explicit, centrally-consulted
/// list, rather than leaving that protection as an accidental byproduct of
/// no code currently wiring [`http::HeaderMap`] into a `Value` at all (true
/// today — see [`crate::network::worker::fetch::parse_body_value`], which
/// takes only a body byte slice — but not enforced by anything that would
/// fail loudly if a future change started doing so). Any future
/// header-surfacing code must consult [`is_stripped_response_header`]
/// rather than re-deriving this list.
///
/// `Set-Cookie` and `WWW-Authenticate` are stripped because they carry
/// authentication state a document must never read directly (mirroring the
/// `Mizu-Auth-Set` vault mechanism's own write-only, invisible-to-logic
/// design); the bare `Mizu-` prefix reservation mirrors the matching
/// request-header denylist in
/// `crates/core/src/parser/logic/parse.rs`'s `validate_header_name`.
const RESPONSE_HEADER_STRIP_LIST_EXACT: &[&str] =
    &["mizu-auth-set", "set-cookie", "www-authenticate"];

/// Returns `true` if `name` (checked case-insensitively) must never be
/// surfaced to document logic — see [`RESPONSE_HEADER_STRIP_LIST_EXACT`]'s
/// doc comment.
pub(super) fn is_stripped_response_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    RESPONSE_HEADER_STRIP_LIST_EXACT.contains(&lower.as_str()) || lower.starts_with("mizu-")
}

/// Parsed `Mizu-Auth-Set` response header.
#[derive(Debug)]
pub(super) struct MizuAuthSetHeader {
    pub(super) token: String,
    pub(super) scope: Vec<String>,
    pub(super) exp: Option<u64>,
}

/// Parses the value of a `Mizu-Auth-Set` HTTP response header.
///
/// Expected format: `<token> SCOPE=<method>[,<method>...] EXP=<unix_seconds>`
///
/// Unknown key=value pairs are silently ignored for forward compatibility.
/// Returns `None` if the value is empty or has no token.
pub(super) fn parse_mizu_auth_set_header(value: &str) -> Option<MizuAuthSetHeader> {
    let mut parts = value.split_whitespace();
    let token = parts.next()?.to_string();
    if token.is_empty() {
        return None;
    }
    let mut scope: Vec<String> = Vec::new();
    let mut exp: Option<u64> = None;
    for part in parts {
        if let Some(s) = part.strip_prefix("SCOPE=") {
            scope = s.split(',').map(|m| m.trim().to_string()).collect();
        } else if let Some(e) = part.strip_prefix("EXP=") {
            exp = e.parse::<u64>().ok();
        }
    }
    Some(MizuAuthSetHeader { token, scope, exp })
}

/// Applies the `Mizu-Auth-Set` header value, storing a vault entry for
/// `domain` after validating expiry and applying the method-scope ceiling.
pub(super) fn process_mizu_auth_set(value: &str, domain: &str) -> Result<(), MizuError> {
    let Some(auth) = parse_mizu_auth_set_header(value) else {
        return Err(MizuError::Network(
            "Mizu-Auth-Set header has invalid format".to_string(),
        ));
    };

    let Some(raw_exp) = auth.exp else {
        return Err(MizuError::SecurityViolation(
            "Mizu-Auth-Set rejected: missing EXP field (expiry is mandatory)".to_string(),
        ));
    };

    let now = std::time::SystemTime::UNIX_EPOCH
        .elapsed()
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let exp = raw_exp.min(now.saturating_add(*MAX_TOKEN_TTL_SECS));

    if exp <= now {
        return Err(MizuError::SecurityViolation(
            "Mizu-Auth-Set rejected: token is already expired at import time".to_string(),
        ));
    }

    let ceiling_methods: Vec<String> = auth
        .scope
        .into_iter()
        .filter(|m| {
            PERMITTED_HTTP_METHODS
                .iter()
                .any(|p| p.eq_ignore_ascii_case(m))
        })
        .collect();

    if ceiling_methods.is_empty() {
        return Err(MizuError::SecurityViolation(
            "Mizu-Auth-Set rejected: no permitted methods remain after scope ceiling".to_string(),
        ));
    }

    let new_entry = VaultEntry {
        token: auth.token,
        allowed_methods: ceiling_methods,
        exp,
    };
    let vault_domain = crate::core::storage::ValidatedDomain::from_raw(domain);
    VaultEntry::save(&vault_domain, &new_entry)?;
    Ok(())
}

/// Interprets an HTTP/3 response, handling redirects, errors, and auth headers.
///
/// Maps HTTP status semantics onto Mizu application semantics:
/// - 2xx: success.  Processes optional `Mizu-Auth-Set` header, returns `None`.
/// - 3xx: redirect.  Body contains the new URL (absolute or relative).
/// - 4xx / 5xx: error.  Body contains the human-readable error message.
pub(super) fn parse_http_response(
    status: http::StatusCode,
    headers: &http::HeaderMap,
    body: &[u8],
    domain: &str,
) -> Result<Option<String>, MizuError> {
    if status.is_success() {
        // Self-check tying this special-cased header to the centralised
        // strip-list: if `mizu-auth-set` were ever removed from
        // `RESPONSE_HEADER_STRIP_LIST_EXACT`, that would silently reopen the
        // exact leak this item closes should any future code start
        // forwarding response headers into a document-visible `Value`.
        debug_assert!(
            is_stripped_response_header("mizu-auth-set"),
            "mizu-auth-set must always be on the response header strip-list"
        );
        if let Some(auth_val) = headers.get("mizu-auth-set") {
            let val_str = auth_val.to_str().map_err(|_| {
                MizuError::Network("Mizu-Auth-Set header is not valid ASCII".to_string())
            })?;
            process_mizu_auth_set(val_str, domain)?;
        }
        return Ok(None);
    }

    if status.is_redirection() {
        let redirect_path = if let Some(loc_val) = headers.get(http::header::LOCATION) {
            loc_val.to_str().unwrap_or("").trim().to_string()
        } else {
            String::from_utf8_lossy(body).trim().to_string()
        };

        if redirect_path.is_empty() {
            return Err(MizuError::Network("Empty redirect destination".to_string()));
        }

        let new_url = if redirect_path.starts_with("mizu://")
            || redirect_path.starts_with("http://")
            || redirect_path.starts_with("https://")
        {
            redirect_path
        } else {
            let path = if redirect_path.starts_with('/') {
                redirect_path.clone()
            } else {
                format!("/{}", redirect_path)
            };
            format!("mizu://{}{}", domain, path)
        };
        return Ok(Some(new_url));
    }

    let body_str = String::from_utf8_lossy(body).trim().to_string();
    let err_msg = if body_str.is_empty() {
        format!("HTTP status error: {}", status)
    } else {
        body_str
    };
    Err(MizuError::Network(err_msg))
}

#[cfg(test)]
mod tests {
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
}
