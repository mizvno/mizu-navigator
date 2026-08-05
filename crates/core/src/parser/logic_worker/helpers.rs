//! Shared URL-alias resolution.
//!
//! [`resolve_endpoint_url`] lives here rather than inside
//! [`super::session`] because it has two callers that must not drift apart:
//! the session's own `NetworkCall` → `ResolvedCall` conversion, and the main
//! process's capability broker, which re-derives the same URL from scratch
//! when the call arrived from an untrusted out-of-process worker. If those
//! two ever disagreed, the broker would either reject calls the document
//! legitimately made or accept ones it did not — so there is exactly one
//! implementation, and both paths call it.

use crate::core::errors::MizuError;
use crate::parser::logic::path_param_ok;
use crate::parser::{EndpointKind, UrlEndpoint};

/// Composes the concrete URL for a resolved network call.
///
/// * `Api` endpoints: prepends `mizu://{domain}` to the relative path stored
///   in `raw_target` (which always starts with `/`).
/// * `Media` endpoints: uses `raw_target` as-is (already an absolute `mizu://`
///   URL).
///
/// If `path_param` is `Some` and the URL contains a `{…}` placeholder, the
/// first placeholder is replaced with the percent-encoded param value. Otherwise the
/// encoded param is appended after a `/`. Note: only the first placeholder is replaced;
/// a second `{…}` is left literal (this is the intended behavior).
///
/// `path_param` is re-validated against the same gate as `execute_action` in
/// `logic.rs` before it is ever substituted into the URL — this is the last
/// consumption point before the value leaves the process, so it must not be
/// possible to reach this function with an unvalidated `path_param` via a
/// different code path.
///
/// Public (not `pub(crate)`) so the main-process capability broker can call
/// the exact same resolution logic when independently re-validating a
/// `NetworkCall`/`DownloadAlias` received from a sandboxed, untrusted
/// `mizu-worker` process instead of trusting a `ResolvedCall`/`DownloadMedia`
/// the worker claims to have already resolved.
pub fn resolve_endpoint_url(
    document_domain: &str,
    ep: &UrlEndpoint,
    path_param: Option<&str>,
) -> Result<String, MizuError> {
    let base_url = match ep.kind {
        EndpointKind::Api => {
            // raw_target starts with `/`; trim it so there is no double slash.
            let path = ep.raw_target.trim_start_matches('/');
            format!("mizu://{}/{}", document_domain, path)
        }
        EndpointKind::Media => ep.raw_target.clone(),
    };
    if let Some(pp) = path_param {
        if !path_param_ok(pp) {
            return Err(MizuError::ExecutionError(
                "path_param must be a single path segment".to_string(),
            ));
        }
        // Percent-encode the path param
        let mut encoded = String::with_capacity(pp.len());
        for b in pp.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    encoded.push(b as char);
                }
                _ => {
                    encoded.push('%');
                    let hex = b"0123456789ABCDEF";
                    encoded.push(hex[(b >> 4) as usize] as char);
                    encoded.push(hex[(b & 0xF) as usize] as char);
                }
            }
        }
        let pp = &encoded;

        // Replace the first `{…}` placeholder if present, otherwise append.
        if let Some(open) = base_url.find('{')
            && let Some(rel_close) = base_url[open..].find('}')
        {
            let close = open + rel_close + 1;
            return Ok(format!("{}{}{}", &base_url[..open], pp, &base_url[close..]));
        }
        Ok(format!("{}/{}", base_url.trim_end_matches('/'), pp))
    } else {
        Ok(base_url)
    }
}
