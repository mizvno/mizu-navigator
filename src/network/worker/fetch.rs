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
pub(super) fn handle_fetch_file(
    url_str: &str,
    sandbox_base: Option<&str>,
) -> Result<Vec<u8>, MizuError> {
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

/// Performs a top-level *navigation* request: one hop, with any 3xx surfaced
/// to the caller rather than followed here.
///
/// Navigation redirects deliberately go back out to the UI thread: that is the
/// only place that owns the navigation choke point (`navigate_to_url`), the
/// per-tab redirect budget, and the initiator that decides whether a
/// cross-origin hop is authorised. Following them down here would route around
/// all three.
pub(super) async fn handle_navigate(
    endpoint: &Endpoint,
    pool: &H3ConnectionPool,
    dns: &crate::network::opennic::MizuDnsResolver,
    method: &str,
    url_str: &str,
    _is_remote_origin: bool,
    request_body: Option<bytes::Bytes>,
    content_type: Option<String>,
    custom_headers: &[(String, String)],
) -> Result<(Option<String>, crate::core::types::Value), MizuError> {
    let (status, headers, body) = handle_fetch_raw(
        endpoint,
        pool,
        dns,
        method,
        url_str,
        _is_remote_origin,
        request_body,
        content_type,
        custom_headers,
        // A top-level navigation is the destination written in the URL bar
        // (or reached via a same-origin/gesture-authorised hop of one) —
        // see `handle_fetch_raw`'s `allow_private_literal` doc comment.
        true,
    )
    .await?;
    let domain = MizuUri::parse(url_str)
        .map(|u| u.domain)
        .unwrap_or_default();
    let redirect = parse_http_response(status, &headers, &body, &domain)?;
    Ok((redirect, parse_body_value(&body)))
}

/// Maximum number of same-origin redirects a *subresource* request follows
/// inside the worker before giving up.
///
/// Shares `max_redirects` with the top-level navigation budget: both answer the
/// same question (how many hops is a server allowed to make us take), and a
/// subresource has no reason to be granted more than a navigation.
fn max_subresource_redirects() -> u32 {
    crate::core::config::CONFIG.max_redirects
}

/// Issues a subresource request (a document's `GET`/`POST`/… data call, or an
/// image fetch) and returns the final response.
///
/// **Invariant N1.** A subresource redirect is followed here, or it fails here
/// — it is never handed back to the UI thread as a navigation. Only two
/// outcomes are possible:
///
/// * **Same-origin 3xx** — followed internally, up to
///   [`max_subresource_redirects`] hops. This is what a `Location` header on a
///   data endpoint legitimately means.
/// * **Cross-origin (or non-`mizu://`) 3xx** — [`MizuError::SecurityViolation`],
///   surfaced to the document as a failed fetch. A background data call or an
///   `<image>` load must not be able to retarget another origin behind the
///   document's back, and — the reason this function exists — must not be able
///   to become a top-level navigation at all: the redirect result carries no
///   real navigation initiator, so promoting one forces the UI thread to invent
///   an agency it was never granted.
async fn handle_fetch_subresource_raw(
    endpoint: &Endpoint,
    pool: &H3ConnectionPool,
    dns: &crate::network::opennic::MizuDnsResolver,
    method: &str,
    url_str: &str,
    is_remote_origin: bool,
    request_body: Option<bytes::Bytes>,
    content_type: Option<String>,
    custom_headers: &[(String, String)],
) -> Result<(http::StatusCode, http::HeaderMap, Vec<u8>), MizuError> {
    let mut current = url_str.to_string();
    let budget = max_subresource_redirects();

    for _ in 0..=budget {
        let (status, headers, body) = handle_fetch_raw(
            endpoint,
            pool,
            dns,
            method,
            &current,
            is_remote_origin,
            // `Bytes::clone` is a refcount bump, so re-sending the payload on
            // the next hop costs nothing.
            request_body.clone(),
            content_type.clone(),
            custom_headers,
            // A subresource fetch (data call or image) is document-triggered,
            // never user-driven, so a literal-IP target must clear the same
            // public-routability bar as a resolved name (SSRF guard).
            false,
        )
        .await?;

        let origin = MizuUri::parse(&current)
            .map(|u| u.domain)
            .unwrap_or_default();
        let Some(next_url) = parse_http_response(status, &headers, &body, &origin)? else {
            return Ok((status, headers, body));
        };

        // The redirect target must be a `mizu://` URL on the same host. A
        // relative `Location` was already re-based onto `origin` by
        // `parse_http_response`, so it always passes; anything that does not is
        // a server steering this request somewhere the document did not ask for.
        let target_origin = MizuUri::parse(&next_url).map(|u| u.domain).ok();
        if target_origin.as_deref() != Some(origin.as_str()) {
            return Err(MizuError::SecurityViolation(format!(
                "cross-origin redirect blocked: subresource request to `{origin}` \
                 was redirected to `{next_url}`; a subresource may only follow \
                 same-origin redirects and is never promoted to a navigation"
            )));
        }
        current = next_url;
    }

    Err(MizuError::Network(format!(
        "subresource request to {url_str} exceeded the {budget}-redirect limit"
    )))
}

/// Subresource data fetch: the decoded response body, with same-origin
/// redirects already followed. See [`handle_fetch_subresource_raw`].
pub(super) async fn handle_fetch(
    endpoint: &Endpoint,
    pool: &H3ConnectionPool,
    dns: &crate::network::opennic::MizuDnsResolver,
    method: &str,
    url_str: &str,
    is_remote_origin: bool,
    request_body: Option<bytes::Bytes>,
    content_type: Option<String>,
    custom_headers: &[(String, String)],
) -> Result<crate::core::types::Value, MizuError> {
    let (_status, _headers, body) = handle_fetch_subresource_raw(
        endpoint,
        pool,
        dns,
        method,
        url_str,
        is_remote_origin,
        request_body,
        content_type,
        custom_headers,
    )
    .await?;
    Ok(parse_body_value(&body))
}

/// Subresource binary fetch (images): the raw response bytes, with same-origin
/// redirects already followed. See [`handle_fetch_subresource_raw`].
pub(super) async fn handle_fetch_bytes(
    endpoint: &Endpoint,
    pool: &H3ConnectionPool,
    dns: &crate::network::opennic::MizuDnsResolver,
    method: &str,
    url_str: &str,
    is_remote_origin: bool,
) -> Result<Vec<u8>, MizuError> {
    let (_status, _headers, body) = handle_fetch_subresource_raw(
        endpoint,
        pool,
        dns,
        method,
        url_str,
        is_remote_origin,
        None,
        None,
        &[],
    )
    .await?;
    Ok(body)
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
///
/// `is_navigation` must be `true` only for a top-level navigation
/// (`handle_navigate`'s call) and `false` for a subresource fetch (a data
/// call or image, via `handle_fetch_subresource_raw`). It is forwarded to
/// `resolve_domain` as `allow_private_literal`: a literal-IP `mizu://` host
/// self-authorizes for a navigation the user drove, but a document-triggered
/// subresource must clear the public-routability check like any other
/// target, or an `<image>`/`NetworkCall` embedding
/// `mizu://169.254.169.254/…` becomes blind SSRF against loopback/LAN/
/// link-local addresses.
pub(super) async fn handle_fetch_raw(
    endpoint: &Endpoint,
    pool: &H3ConnectionPool,
    dns: &crate::network::opennic::MizuDnsResolver,
    method: &str,
    url_str: &str,
    _is_remote_origin: bool,
    request_body: Option<bytes::Bytes>,
    content_type: Option<String>,
    custom_headers: &[(String, String)],
    is_navigation: bool,
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
        is_navigation,
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
        content_type.clone(),
        custom_headers,
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
                content_type,
                custom_headers,
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

/// Builds the HTTP/3 request headers/method/URI (everything but the body),
/// with no network I/O — split out from [`do_h3_request`] so header
/// attachment (`Content-Type`, the vault `Authorization` bearer token, and
/// document-declared custom headers) is unit-testable without a live QUIC
/// connection.
///
/// The `:scheme` pseudo-header is set to `"https"` because `h3` validates
/// scheme conformance; Mizu's custom `mizu://` routing is enforced by the
/// ALPN layer, not the HTTP scheme header.
///
/// Custom header *names* were already validated (syntax + reserved denylist)
/// at parse time; a header *value* is only checked here, via
/// `http::request::Builder`'s own `TryInto<HeaderValue>` conversion — which
/// is what actually rejects an unsafe value (e.g. one containing CR/LF).
/// That rejection surfaces as `Err` from this function, before any I/O is
/// attempted, so a bad value aborts the whole request rather than silently
/// stripping just that header.
fn build_h3_request(
    uri: &MizuUri,
    method: &str,
    opt_entry: Option<&VaultEntry>,
    content_type: Option<String>,
    custom_headers: &[(String, String)],
) -> Result<http::Request<()>, MizuError> {
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

    // The `Content-Type` is selected by the request's declared `PayloadFormat`
    // (`Multipart`'s includes a per-request random boundary — see
    // `payload::serialize_payload`); `None` for body-less requests.
    if let Some(ct) = content_type {
        req_builder = req_builder.header(http::header::CONTENT_TYPE, ct.as_str());
    }

    for (name, value) in custom_headers {
        req_builder = req_builder.header(name.as_str(), value.as_str());
    }

    req_builder
        .body(())
        .map_err(|e| MizuError::Network(format!("Request build error: {e}")))
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
    content_type: Option<String>,
    custom_headers: &[(String, String)],
) -> Result<(http::StatusCode, http::HeaderMap, Vec<u8>), MizuError> {
    let h3_client = pool.get_or_connect(endpoint, addr, &uri.domain).await?;

    let req = build_h3_request(uri, method, opt_entry, content_type, custom_headers)?;

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

#[cfg(test)]
mod tests {
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
}
