//! `H3ConnectionPool`, ALPN verification, connect/request timeouts.

use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use quinn::Endpoint;

use crate::core::errors::MizuError;

use super::MIZU_ALPN;

type H3Sender = h3::client::SendRequest<h3_quinn::OpenStreams, bytes::Bytes>;
type H3Client = Arc<tokio::sync::Mutex<H3Sender>>;

/// Maximum time allowed to establish one HTTP/3 connection: the QUIC
/// transport handshake (`Endpoint::connect(...).await`) plus the H3-layer
/// setup (`h3::client::builder().build(...).await`, which exchanges the
/// initial SETTINGS frames). A server that accepts the QUIC handshake but
/// never completes the H3-level setup — or never responds at the transport
/// level at all — would otherwise hang this call forever, holding its
/// `MAX_CONCURRENT_FETCHES` permit indefinitely.
pub(crate) static CONNECT_TIMEOUT: LazyLock<Duration> =
    LazyLock::new(|| Duration::from_secs(crate::core::config::CONFIG.connect_timeout_secs));

/// Maximum time allowed for one complete HTTP/3 request/response exchange
/// once a connection is established: sending the request (HEADERS + body),
/// and receiving the response HEADERS and all DATA frames. Guards against a
/// server that completes the handshake but then never sends a response, or
/// stalls mid-body.
pub(crate) static REQUEST_TIMEOUT: LazyLock<Duration> =
    LazyLock::new(|| Duration::from_secs(crate::core::config::CONFIG.request_timeout_secs));

/// QUIC idle timeout: the transport closes a connection that has exchanged
/// no packets for this long, even if the application never reports an
/// error. Set on every client [`quinn::TransportConfig`] so a
/// silently-stalled-but-still-"open" connection doesn't sit around
/// indefinitely, and reused as [`H3ConnectionPool`]'s own idle-reap
/// threshold (see [`H3ConnectionPool::make_room`]) so a pool entry whose
/// underlying QUIC connection the transport has already closed for
/// idleness doesn't linger in the map either.
pub(crate) static QUIC_MAX_IDLE_TIMEOUT: LazyLock<Duration> =
    LazyLock::new(|| Duration::from_secs(crate::core::config::CONFIG.quic_max_idle_timeout_secs));

/// QUIC keep-alive interval: how often a PING frame is sent on an otherwise
/// idle connection to prevent NAT/firewall state from expiring and to keep
/// [`QUIC_MAX_IDLE_TIMEOUT`] from firing on connections that are merely
/// quiet, not dead. Must be well under `QUIC_MAX_IDLE_TIMEOUT`.
pub(crate) static QUIC_KEEP_ALIVE_INTERVAL: LazyLock<Duration> = LazyLock::new(|| {
    Duration::from_secs(crate::core::config::CONFIG.quic_keep_alive_interval_secs)
});

/// Maximum number of live per-domain HTTP/3 connections
/// [`H3ConnectionPool`] retains at once. Reached only by a document that
/// legitimately talks to many distinct domains; once at capacity, the
/// least-recently-used connection is evicted to make room (see
/// [`H3ConnectionPool::make_room`]) rather than growing the pool without
/// bound.
pub(crate) static MAX_POOL_SIZE: LazyLock<usize> =
    LazyLock::new(|| crate::core::config::CONFIG.max_pool_size);

/// Thread-safe pool of live HTTP/3 connection handles, one per domain.
///
/// Each entry wraps an `h3::client::Connection` behind a `Mutex` so that
/// concurrent tasks can open new HTTP/3 request streams on the same QUIC
/// connection without triggering redundant TLS 1.3 handshakes.
///
/// ## Concurrency model
///
/// The `Mutex` is held only during `send_request` (which writes the HTTP/3
/// HEADERS frame and registers the stream with the connection).  Stream-level
/// I/O — `finish`, `recv_response`, `recv_data` — proceeds without the lock,
/// so concurrent requests to the same domain are fully multiplexed.
///
/// The outer pool `Mutex` is held across `endpoint.connect().await` only when
/// no cached entry exists, serialising concurrent handshake attempts to the
/// *same* domain to exactly one handshake. That await is bounded by
/// [`CONNECT_TIMEOUT`], so a stalled handshake cannot hold the lock (or the
/// caller's fetch-concurrency permit) forever.
///
/// ## ALPN enforcement
///
/// After the QUIC handshake, [`get_or_connect`](H3ConnectionPool::get_or_connect)
/// reads back the negotiated ALPN (`quinn::Connection::handshake_data`,
/// downcast to `quinn::crypto::rustls::HandshakeData`) and rejects the
/// connection outright — before it is inserted into the pool or used for any
/// application traffic — unless it is exactly [`MIZU_ALPN`]. See
/// `verify_negotiated_alpn`.
///
/// This check is redundant *in the current configuration*: rustls's client
/// enforces RFC 9001 for QUIC connections specifically (a QUIC client that
/// offered a non-empty ALPN list aborts the handshake with
/// `NoApplicationProtocol` if the server doesn't select one from it), and
/// because the client offers `mizu/3` as its *sole* protocol, any protocol
/// the server does select must be `mizu/3`. It is nonetheless kept as
/// explicit, testable, application-level defence-in-depth — it does not rely
/// on the QUIC-specific carve-out in rustls's ALPN handling continuing to
/// behave this way, and it stays correct even if [`MIZU_ALPN`]'s call site
/// ever configures more than one protocol.
///
/// ## Dead-connection eviction
///
/// If `send_request` fails with a network error the caller evicts the entry
/// via [`H3ConnectionPool::evict`] and retries once, transparently replacing
/// the stale connection. The pool additionally bounds itself to
/// [`MAX_POOL_SIZE`] entries (LRU eviction) and reaps entries idle longer
/// than [`QUIC_MAX_IDLE_TIMEOUT`] on next use — see
/// [`H3ConnectionPool::make_room`].
#[derive(Clone)]
pub(crate) struct H3ConnectionPool {
    connections: Arc<tokio::sync::Mutex<std::collections::HashMap<String, (H3Client, Instant)>>>,
    /// QUIC+H3 handshake timeout for this pool instance. Always
    /// [`CONNECT_TIMEOUT`] in production; overridable in tests (see
    /// [`H3ConnectionPool::new_with_connect_timeout`]) so a test that
    /// deliberately never completes the handshake doesn't have to wait out
    /// the full production timeout to prove it fires.
    connect_timeout: Duration,
}

/// Checks `handshake_data` — the value returned by
/// `quinn::Connection::handshake_data()` right after a QUIC handshake
/// completes — against [`MIZU_ALPN`], and rejects anything else: a missing
/// value, a value of an unexpected concrete type, or a negotiated protocol
/// (including "none negotiated") other than `mizu/3`.
///
/// Factored out of [`H3ConnectionPool::get_or_connect`] as a plain function
/// over `Box<dyn Any>` (rather than inlined against a live
/// `quinn::Connection`) so this rejection logic is unit-testable without
/// spinning up a real QUIC server — see the "ALPN enforcement" section on
/// [`H3ConnectionPool`]'s doc comment for why the check exists despite
/// rustls already closing most of this gap on its own.
pub(super) fn verify_negotiated_alpn(
    handshake_data: Option<Box<dyn std::any::Any>>,
    domain: &str,
) -> Result<(), MizuError> {
    let negotiated = handshake_data
        .and_then(|data| data.downcast::<quinn::crypto::rustls::HandshakeData>().ok())
        .and_then(|data| data.protocol);
    if negotiated.as_deref() == Some(MIZU_ALPN) {
        return Ok(());
    }
    Err(MizuError::SecurityViolation(format!(
        "ALPN mismatch connecting to {domain}: expected {:?}, server negotiated {:?}",
        String::from_utf8_lossy(MIZU_ALPN),
        negotiated
            .as_deref()
            .map(String::from_utf8_lossy)
            .map(|s| s.into_owned()),
    )))
}

impl H3ConnectionPool {
    pub(crate) fn new() -> Self {
        Self::new_with_connect_timeout(*CONNECT_TIMEOUT)
    }

    /// Like [`Self::new`], but with an explicit handshake timeout — used by
    /// tests that need the *real* timeout code path to fire quickly rather
    /// than waiting out [`CONNECT_TIMEOUT`].
    pub(crate) fn new_with_connect_timeout(connect_timeout: Duration) -> Self {
        Self {
            connections: Arc::new(tokio::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            connect_timeout,
        }
    }

    /// Drops entries idle longer than `max_idle`, then — if still at or over
    /// `max_pool_size` — evicts the single least-recently-used entry, so a
    /// caller about to insert one new entry never pushes the map over
    /// `max_pool_size`.
    ///
    /// Generic over the stored value type (production always instantiates
    /// `V = H3Client`) purely so the eviction *decision* logic can be
    /// exercised directly by a unit test (`pool_never_exceeds_max_size`)
    /// without constructing a live H3 connection — the production code path
    /// and the test both call this exact function.
    pub(super) fn make_room<V>(
        map: &mut std::collections::HashMap<String, (V, Instant)>,
        now: Instant,
        max_idle: Duration,
        max_pool_size: usize,
    ) {
        map.retain(|_, (_, last_used)| now.duration_since(*last_used) < max_idle);
        while map.len() >= max_pool_size {
            let Some(lru_domain) = map
                .iter()
                .min_by_key(|(_, (_, last_used))| *last_used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            map.remove(&lru_domain);
        }
    }

    /// Returns a live HTTP/3 connection handle for `domain`, establishing one
    /// via `endpoint` if no valid cached entry exists.
    pub(crate) async fn get_or_connect(
        &self,
        endpoint: &Endpoint,
        addr: std::net::SocketAddr,
        domain: &str,
    ) -> Result<H3Client, MizuError> {
        let mut map = self.connections.lock().await;
        let now = Instant::now();
        if let Some((h3, last_used)) = map.get_mut(domain) {
            *last_used = now;
            return Ok(h3.clone());
        }
        Self::make_room(&mut map, now, *QUIC_MAX_IDLE_TIMEOUT, *MAX_POOL_SIZE);

        // Guard held across await — at most one concurrent handshake per
        // domain. Bounded by `self.connect_timeout` so a server that never
        // completes the QUIC or H3-level handshake cannot hold this lock (or
        // the caller's fetch-concurrency permit) forever.
        let (mut driver, sender) = tokio::time::timeout(self.connect_timeout, async {
            let quinn_conn = endpoint
                .connect(addr, domain)
                .map_err(|e| MizuError::Network(format!("Connect error: {e}")))?
                .await
                .map_err(|e| MizuError::Network(format!("Connection failed: {e}")))?;

            // Explicit post-handshake ALPN check — see the "ALPN enforcement"
            // section on this struct's doc comment.
            verify_negotiated_alpn(quinn_conn.handshake_data(), domain)?;

            h3::client::builder()
                .build::<_, h3_quinn::OpenStreams, bytes::Bytes>(h3_quinn::Connection::new(
                    quinn_conn,
                ))
                .await
                .map_err(|e| MizuError::Network(format!("H3 connection setup error: {e}")))
        })
        .await
        .map_err(|_elapsed| {
            MizuError::Network(format!(
                "QUIC/H3 handshake to {domain} timed out after {:?}",
                self.connect_timeout
            ))
        })??;

        // Drive connection-level frames (SETTINGS, GOAWAY) in a background
        // task so the network dispatch loop is never blocked on them.
        let domain_owned = domain.to_string();
        tokio::spawn(async move {
            let _ = std::future::poll_fn(|cx| driver.poll_close(cx)).await;
            tracing::debug!(domain = %domain_owned, "H3 connection driver closed");
        });

        let h3_client = Arc::new(tokio::sync::Mutex::new(sender));
        map.insert(domain.to_string(), (h3_client.clone(), Instant::now()));
        Ok(h3_client)
    }

    /// Removes the cached entry for `domain`.
    ///
    /// Called by `handle_fetch_raw` when `send_request` fails, so that the next
    /// request to the same domain triggers a fresh QUIC handshake.
    pub(crate) async fn evict(&self, domain: &str) {
        self.connections.lock().await.remove(domain);
    }

    /// Returns the number of live connections currently held by the pool.
    #[cfg(test)]
    pub(crate) async fn len(&self) -> usize {
        self.connections.lock().await.len()
    }
}
