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
