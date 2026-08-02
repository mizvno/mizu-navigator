//! Small leaf helpers shared across the parse grammar: bare-`=` scanning,
//! binding-line detection, parenthesis matching, header-name validation,
//! and control-character/path-param checks (`is_ctl`/`path_param_ok`).

use crate::core::errors::MizuError;

pub(super) fn find_assignment_eq(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'=' {
            let prev_is_op = i > 0 && matches!(bytes[i - 1], b'!' | b'<' | b'>' | b'=');
            let next_is_eq = i + 1 < bytes.len() && bytes[i + 1] == b'=';
            if !prev_is_op && !next_is_eq {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Returns `true` if a trimmed line looks like `name = expr` (a binding).
///
/// A line is a binding if it contains a bare `=` (not `==`, `!=`, `<=`, `>=`)
/// AND the text before that `=` is a plain identifier.
pub(super) fn looks_like_binding(line: &str) -> bool {
    if let Some(eq_pos) = find_assignment_eq(line) {
        let lhs = line[..eq_pos].trim();
        !lhs.is_empty()
            && lhs
                .chars()
                .next()
                .is_some_and(|c| c.is_alphabetic() || c == '_')
            && lhs.chars().all(|c| c.is_alphanumeric() || c == '_')
    } else {
        false
    }
}

pub(super) fn find_matching_paren(s: &str, open_idx: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth: i32 = 0;
    let mut i = open_idx;
    let mut in_string = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// HTTP header names a `NetworkCall`'s `header "<name>" <expr>` clause may
/// never set, checked case-insensitively at parse time.
///
/// This mirrors the web platform's "forbidden header name" concept from the
/// Fetch spec (<https://fetch.spec.whatwg.org/#forbidden-request-header>):
/// names that are either owned by another mechanism in this runtime
/// (`Content-Type` by [`PayloadFormat`]/Item 1, `Authorization` by the
/// zero-touch vault — see `network::worker::auth`) or that could otherwise
/// let a document interfere with connection-level framing the runtime
/// depends on. `Mizu-` is additionally reserved wholesale for the runtime's
/// own signaling headers (matching the existing `Mizu-Auth-Set` naming).
const RESERVED_HEADER_NAMES_EXACT: &[&str] = &[
    "host",
    "content-length",
    "content-type",
    "authorization",
    "connection",
    "transfer-encoding",
    "upgrade",
    "te",
    "trailer",
];

/// Validates a `NetworkCall` header clause's name at parse time: it must be a
/// syntactically valid HTTP header name (checked via a real constructor,
/// [`http::HeaderName::from_str`], never hand-rolled validation) and must not
/// be on the [`RESERVED_HEADER_NAMES_EXACT`] denylist or under the
/// `Proxy-`/`Sec-`/`Mizu-` reserved prefixes.
pub(super) fn validate_header_name(name: &str, method_name: &str) -> Result<(), MizuError> {
    use std::str::FromStr;
    http::HeaderName::from_str(name).map_err(|e| {
        MizuError::ParseError(format!(
            "network call `{method_name}`: invalid header name `{name}`: {e}"
        ))
    })?;

    let lower = name.to_ascii_lowercase();
    let reserved = RESERVED_HEADER_NAMES_EXACT.contains(&lower.as_str())
        || lower.starts_with("proxy-")
        || lower.starts_with("sec-")
        || lower.starts_with("mizu-");
    if reserved {
        return Err(MizuError::ParseError(format!(
            "network call `{method_name}`: header `{name}` is reserved and cannot be set by a \
             document (see the Fetch spec's forbidden-header-name list; `Content-Type` is \
             owned by the `as <format>` clause, `Authorization` by the vault mechanism, \
             and the `Mizu-` prefix is reserved for the runtime's own signaling headers)"
        )));
    }
    Ok(())
}

/// True for ASCII control characters (`< 0x20` or `DEL`, `0x7F`).
///
/// Mirrors `isCtl` in `formal/MizuFormal/Semantics.lean`.
fn is_ctl(c: char) -> bool {
    (c as u32) < 0x20 || c as u32 == 0x7F
}

/// The `path_param` validation gate (G2): rejects path separators (`/`,
/// `\`), ASCII control characters, and the `..` traversal substring, so a
/// value bound from an untrusted network response can never restructure the
/// endpoint's URL path when substituted into it.
///
/// Mirrors `pathParamOk` in `formal/MizuFormal/Semantics.lean`; every call
/// site that consumes a `path_param` (`execute_action` below and
/// `resolve_endpoint_url` in `logic_worker.rs`) must run it before the value
/// is used to build a URL.
pub(crate) fn path_param_ok(s: &str) -> bool {
    !s.chars().any(|c| c == '/' || c == '\\' || is_ctl(c)) && !s.contains("..")
}
