//! `file://`/HTTP(S) fetch dispatch and H3 request execution.


use quinn::Endpoint;

use crate::core::errors::MizuError;
use crate::network::uri::MizuUri;
use crate::network::vault::VaultEntry;

use super::auth::{load_valid_entry, parse_http_response};
use super::h3_pool::{H3ConnectionPool, REQUEST_TIMEOUT};

/// Reads a local `file://` resource from disk, enforcing the sandbox.
///
/// `sandbox_base` is the parent directory of the currently-loaded document.
/// If `None`, all `file://` access is denied (security default).  If `Some`,
/// the resolved path must start with the base; escape attempts return
/// [`MizuError::SecurityViolation`].
pub(super) fn handle_fetch_file(url_str: &str, sandbox_base: Option<&str>) -> Result<Vec<u8>, MizuError> {
    let path_str = url_str
        .strip_prefix("file:///")
        .or_else(|| url_str.strip_prefix("file://"))
        .ok_or_else(|| MizuError::Network(format!("Malformed file:// URL: {url_str}")))?;

    let target = std::path::Path::new(path_str);

    let base = match sandbox_base {
        Some(b) => std::path::Path::new(b).to_path_buf(),
        None => {
            return Err(MizuError::SecurityViolation(
                "file:// access denied: no sandbox base configured for this origin".to_string(),
            ));
        }
    };

    if !crate::render::security::file_sandbox_contains(&base, target) {
        return Err(MizuError::SecurityViolation(format!(
            "file:// access denied: '{}' escapes sandbox base '{}'",
            target.display(),
            base.display()
        )));
    }

    // TOCTOU hardening: resolve the path exactly once (following any symlinks),
    // re-verify the *resolved* form against the sandbox, then read through it.
    // The checked path and the read path are therefore the same filesystem
    // object — a symlink swapped in between check and read cannot redirect the
    // read outside the sandbox.
    let resolved = std::fs::canonicalize(target).map_err(MizuError::IoError)?;
    if !crate::render::security::file_sandbox_contains(&base, &resolved) {
        return Err(MizuError::SecurityViolation(format!(
            "file:// access denied: resolved path '{}' escapes sandbox base '{}'",
            resolved.display(),
            base.display()
        )));
    }
    std::fs::read(&resolved).map_err(MizuError::IoError)
}

/// Decodes a raw network response body to a `Value::String` using lossy UTF-8.
///
/// Invalid UTF-8 byte sequences are replaced with U+FFFD (REPLACEMENT CHARACTER
/// '�').  This is intentional and safe: no memory corruption or panics are
/// possible — only the replacement substitution.  Callers that need binary
/// payloads must use the `FetchImage` path instead.
pub(crate) fn parse_body_value(body: &[u8]) -> crate::core::types::Value {
    crate::core::types::Value::from(String::from_utf8_lossy(body).into_owned())
}

pub(super) async fn handle_fetch(
    endpoint: &Endpoint,
    pool: &H3ConnectionPool,
    dns: &crate::network::opennic::MizuDnsResolver,
    method: &str,
    url_str: &str,
    _is_remote_origin: bool,
    request_body: Option<bytes::Bytes>,
) -> Result<(Option<String>, crate::core::types::Value), MizuError> {
    let (status, headers, body) = handle_fetch_raw(
        endpoint,
        pool,
        dns,
        method,
        url_str,
        _is_remote_origin,
        request_body,
    )
    .await?;
    let domain = MizuUri::parse(url_str)
        .map(|u| u.domain)
        .unwrap_or_default();
    let redirect = parse_http_response(status, &headers, &body, &domain)?;
    Ok((redirect, parse_body_value(&body)))
}

/// Issues an HTTP/3 request over the pool and returns the raw status, response
/// headers, and body bytes.
///
/// The `file://` scheme is rejected unconditionally — local asset reads must
/// go through `handle_fetch_file` (sandbox-enforced).
///
/// On a connection-level failure the pool entry is evicted and the request is
/// retried once on a fresh connection, transparently recovering from stale
/// connections caused by server restarts or idle-timeout evictions.
pub(super) async fn handle_fetch_raw(
    endpoint: &Endpoint,
    pool: &H3ConnectionPool,
    dns: &crate::network::opennic::MizuDnsResolver,
    method: &str,
    url_str: &str,
    _is_remote_origin: bool,
    request_body: Option<bytes::Bytes>,
) -> Result<(http::StatusCode, http::HeaderMap, Vec<u8>), MizuError> {
    if url_str.starts_with("file://") {
        return Err(MizuError::SecurityViolation(
            "file:// URIs must not reach the QUIC fetch path; \
             use handle_fetch_file for sandboxed local asset reads"
                .to_string(),
        ));
    }

    let uri = MizuUri::parse(url_str)?;
    let vault_domain = crate::core::storage::ValidatedDomain::from_raw(&uri.domain);
    let opt_entry = load_valid_entry(&vault_domain, method)?;

    // ── DNS via OpenNIC ──────────────────────────────────────────────────────
    let addr = crate::network::opennic::resolve_domain(
        dns,
        &uri.domain,
        *crate::network::opennic::MIZU_PORT,
    )
    .await?;

    // First attempt. On a connection-level error, evict and retry once.
    // `Bytes::clone` is a cheap refcount bump, so the retry reuses the payload.
    match do_h3_request(
        pool,
        endpoint,
        addr,
        &uri,
        method,
        opt_entry.as_ref(),
        request_body.clone(),
    )
    .await
    {
        Ok(resp) => Ok(resp),
        Err(MizuError::Network(_)) => {
            pool.evict(&uri.domain).await;
            // Re-validate the vault entry in case the first attempt consumed it.
            let opt_entry2 = load_valid_entry(&vault_domain, method)?;
            do_h3_request(
                pool,
                endpoint,
                addr,
                &uri,
                method,
                opt_entry2.as_ref(),
                request_body,
            )
            .await
        }
        Err(e) => Err(e),
    }
}

/// Hard ceiling on the total number of body bytes accepted from a single
/// HTTP/3 response (32 MiB).
///
/// Without this cap a malicious or compromised server could stream an
/// unbounded body and exhaust client memory — the accumulation loop in
/// [`do_h3_request`] would `extend_from_slice` forever.  32 MiB comfortably
/// covers any legitimate Mizu document or media asset (image decode is
/// additionally bounded by `MAX_IMAGE_ALLOC_BYTES` after download).
pub(super) const MAX_RESPONSE_BODY_BYTES: usize = 32 * 1024 * 1024;

/// Checks that appending `incoming_len` bytes to a body of `current_len` bytes
/// stays within [`MAX_RESPONSE_BODY_BYTES`].
///
/// Returns [`MizuError::SecurityViolation`] on overflow so the caller aborts
/// the transfer; `SecurityViolation` is deliberately not a retryable error
/// class (unlike `MizuError::Network`), so the oversized download is not
/// re-attempted on a fresh connection.
pub(crate) fn check_response_body_budget(
    current_len: usize,
    incoming_len: usize,
) -> Result<(), MizuError> {
    if current_len.saturating_add(incoming_len) > MAX_RESPONSE_BODY_BYTES {
        return Err(MizuError::SecurityViolation(format!(
            "response body exceeds the {MAX_RESPONSE_BODY_BYTES}-byte limit; transfer aborted"
        )));
    }
    Ok(())
}

/// Sends a single HTTP/3 request on a pooled connection and reads the full
/// response.
///
/// `body` carries the optional JSON-serialised request payload (POST / PUT /
/// QUERY).  `None` sends a body-less request (GET / DELETE / navigation).
pub(super) async fn do_h3_request(
    pool: &H3ConnectionPool,
    endpoint: &Endpoint,
    addr: std::net::SocketAddr,
    uri: &MizuUri,
    method: &str,
    opt_entry: Option<&VaultEntry>,
    body: Option<bytes::Bytes>,
) -> Result<(http::StatusCode, http::HeaderMap, Vec<u8>), MizuError> {
    let h3_client = pool.get_or_connect(endpoint, addr, &uri.domain).await?;

    // Build the HTTP/3 request.  The `:scheme` pseudo-header is set to "https"
    // because h3 validates scheme conformance; Mizu's custom `mizu://` routing
    // is enforced by the ALPN layer, not the HTTP scheme header.
    let mut req_builder = http::Request::builder()
        .method(method)
        .uri(format!("https://{}{}", uri.domain, uri.path))
        .version(http::Version::HTTP_3)
        .header(http::header::HOST, &uri.domain);

    if let Some(entry) = opt_entry {
        req_builder = req_builder.header(
            http::header::AUTHORIZATION,
            format!("Bearer {}", entry.token),
        );
    }

    // Request payloads are always JSON (serialised from a Mizu `Value`).
    if body.is_some() {
        req_builder = req_builder.header(http::header::CONTENT_TYPE, "application/json");
    }

    let req = req_builder
        .body(())
        .map_err(|e| MizuError::Network(format!("Request build error: {e}")))?;

    // The whole send/receive exchange — HEADERS, optional body, and the full
    // response (HEADERS + all DATA frames) — is bounded by REQUEST_TIMEOUT.
    // A server that completes the handshake (see H3ConnectionPool::get_or_connect
    // for the connect-phase timeout) but then never ACKs, never sends a
    // response, or stalls mid-body would otherwise hang this call — and the
    // caller's fetch-concurrency permit — forever.
    let exchange = async {
        // Lock held only for the brief send_request call (sends the HEADERS
        // frame). Once the RequestStream handle is returned the lock is
        // released, so concurrent requests to the same domain are fully
        // H3-multiplexed.
        let mut stream = {
            let mut sender = h3_client.lock().await;
            sender
                .send_request(req)
                .await
                .map_err(|e| MizuError::Network(format!("H3 send_request failed: {e}")))?
        };

        // Transmit the request payload (if any), then signal end of body.
        if let Some(payload_bytes) = body {
            stream
                .send_data(payload_bytes)
                .await
                .map_err(|e| MizuError::Network(format!("H3 send_data failed: {e}")))?;
        }
        stream
            .finish()
            .await
            .map_err(|e| MizuError::Network(format!("H3 stream finish failed: {e}")))?;

        // Read the response HEADERS frame.
        let response = stream
            .recv_response()
            .await
            .map_err(|e| MizuError::Network(format!("H3 recv_response failed: {e}")))?;

        let status = response.status();
        let headers = response.headers().clone();

        // Read all DATA frames.  `recv_data()` returns `impl bytes::Buf`; we
        // drain each chunk via `Buf::chunk()` + `Buf::advance()` to avoid
        // allocating an intermediate owned buffer.  Accumulation is capped by
        // MAX_RESPONSE_BODY_BYTES — see `check_response_body_budget`.
        let mut resp_body: Vec<u8> = Vec::new();
        while let Some(mut chunk) = stream
            .recv_data()
            .await
            .map_err(|e| MizuError::Network(format!("H3 recv_data failed: {e}")))?
        {
            use bytes::Buf as _;
            while chunk.has_remaining() {
                let slice = chunk.chunk();
                check_response_body_budget(resp_body.len(), slice.len())?;
                resp_body.extend_from_slice(slice);
                let len = slice.len();
                chunk.advance(len);
            }
        }

        Ok::<_, MizuError>((status, headers, resp_body))
    };

    tokio::time::timeout(*REQUEST_TIMEOUT, exchange)
        .await
        .map_err(|_elapsed| {
            MizuError::Network(format!(
                "H3 request to {} timed out after {:?}",
                uri.domain, *REQUEST_TIMEOUT
            ))
        })?
}
