use std::sync::Arc;
use std::time::{Duration, Instant};

use quinn::Endpoint;

use crate::core::errors::MizuError;
use crate::network::uri::MizuUri;
use crate::network::vault::VaultEntry;
use crate::network::{NetworkCmd, NetworkResult};

// HTTP/3 stack: h3 (framing), h3-quinn (QUIC adapter), http (standard types).
//
// h3 0.0.8 build() returns (Connection<driver>, SendRequest<sender>):
//   • Connection — drives connection-level frames (SETTINGS, GOAWAY, PING);
//     spawned as a background task so the event loop never blocks on it.
//   • SendRequest — opens HTTP/3 request streams; stored in the pool.
//
// Because SendRequest::send_request(&mut self, ...) requires exclusive access,
// the pool wraps it in a tokio Mutex.  The lock is held only for the brief
// duration of send_request (sends the HEADERS frame); stream-level I/O runs
// without holding the lock, enabling full H3 request multiplexing.
type H3Sender = h3::client::SendRequest<h3_quinn::OpenStreams, bytes::Bytes>;
type H3Client = Arc<tokio::sync::Mutex<H3Sender>>;

/// The sole ALPN token advertised by Mizu clients and enforced on every
/// incoming connection.  Servers that do not negotiate this exact token are
/// dropped before any application data is exchanged.
pub(crate) const MIZU_ALPN: &[u8] = b"mizu/3";


// ---------------------------------------------------------------------------
// Storage dispatch: debounced, per-domain batched redb writes (RM-12)
//
// `NetworkCmd::StorageStore` commands are no longer written straight through
// to `crate::core::storage::StoragePool` one record (one `redb` write
// transaction) at a time. Instead they are queued in a
// [`StorageWriteDebouncer`]: commands for the same domain arriving within
// `STORAGE_DEBOUNCE_WINDOW` are coalesced (last-write-wins per key) and
// committed together via `StorageEngine::write_batch` in a *single* `redb`
// write transaction — instead of one transaction (and, absent a `Durability`
// override in `open_db`, likely one `fsync`) per key. A domain that
// accumulates `STORAGE_BATCH_MAX_KEYS` distinct keys before the window
// elapses is flushed immediately rather than continuing to buffer, bounding
// both memory and worst-case write latency under sustained writes (e.g.
// `store_local` called once per keystroke or per animation frame).
//
// DURABILITY TRADEOFF — read before touching this: a write queued by the
// debouncer is only guaranteed durable once its batch's `redb` transaction
// commits, up to `STORAGE_DEBOUNCE_WINDOW` (or until `STORAGE_BATCH_MAX_KEYS`
// is reached) after the document's `store_local(key, value)` call returns
// control to the evaluator. If the process terminates abnormally (crash,
// `kill -9`, power loss) inside that window, the write is lost even though
// `store_local` already "completed" from the document's perspective. This is
// an accepted, explicit tradeoff for this runtime:
//   * Mizu documents have no `read_local` (invariant S1, `SECURITY-INVARIANTS.md`)
//     and therefore cannot observe whether a given write has landed — there is
//     no *document-visible* correctness invariant this weakens, only the
//     informal expectation that storage survives a clean-ish exit, and the
//     window is short (low hundreds of ms) either way.
//   * It does NOT apply to authentication tokens: `VaultEntry`
//     (`network::vault`) never goes through `StoragePool`/`redb` at all — every
//     `save()` writes straight to the OS keyring — so bearer tokens keep the
//     pre-existing immediate-write guarantee unconditionally, unaffected by
//     this change.
//   * Any future caller that needs a guaranteed-immediate, non-debounced write
//     to redb-backed local storage can call `StoragePool::write_record`
//     directly (this file's own tests do exactly that) — it bypasses the
//     debouncer entirely and remains a single, immediately-durable
//     transaction, as documented on `write_record` itself.
//
// Dispatching each flush via `tokio::task::spawn_blocking` keeps the keyring
// IPC and filesystem I/O off both the UI thread and the async dispatch loop
// below, exactly as before this change.
// ---------------------------------------------------------------------------

/// Debounce window for [`StorageWriteDebouncer`]: writes to the same domain
/// arriving within this window of the first buffered (unflushed) write are
/// batched into one `redb` transaction. Chosen from the middle of the
/// 100–250ms range suggested for this kind of UI-driven debounce: long enough
/// to coalesce a burst of per-keystroke/per-frame writes, short enough that
/// the durability window above stays unnoticeable in practice.
pub(crate) const STORAGE_DEBOUNCE_WINDOW: Duration = Duration::from_millis(150);

/// Maximum number of distinct keys buffered for one domain before a flush is
/// forced immediately, regardless of how much of `STORAGE_DEBOUNCE_WINDOW`
/// remains. Without this, a document writing continuously (a new key every
/// frame, never repeating) would keep resetting into "still within the
/// window" forever and accumulate unboundedly.
pub(crate) const STORAGE_BATCH_MAX_KEYS: usize = 64;

/// Batches [`NetworkCmd::StorageStore`] writes to the same domain that arrive
/// within [`STORAGE_DEBOUNCE_WINDOW`] of each other into a single
/// `StorageEngine::write_batch` transaction. See the "Storage dispatch" doc
/// comment above for the durability tradeoff this introduces and why it is
/// scoped to non-credential local storage only.
///
/// One instance is shared (via `Clone`, which shares the inner `Arc`) across
/// every `StorageStore` dispatch for the lifetime of the network thread.
#[derive(Clone)]
pub(crate) struct StorageWriteDebouncer {
    /// Per-domain (keyed by `ValidatedDomain::as_str()`, the SHA-256 hex
    /// digest) buffer of not-yet-flushed writes. `HashMap<key, value>` so
    /// that repeated writes to the same key within one window collapse to
    /// last-write-wins instead of encrypting/inserting each one individually
    /// when the batch is eventually committed.
    pending: Arc<std::sync::Mutex<std::collections::HashMap<String, std::collections::HashMap<String, crate::core::types::Value>>>>,
    window: Duration,
    max_keys: usize,
}

impl StorageWriteDebouncer {
    pub(crate) fn new() -> Self {
        Self::with_params(STORAGE_DEBOUNCE_WINDOW, STORAGE_BATCH_MAX_KEYS)
    }

    /// Like [`Self::new`], but with explicit window/threshold — used by tests
    /// that need short, deterministic timing instead of waiting out the
    /// production window.
    pub(crate) fn with_params(window: Duration, max_keys: usize) -> Self {
        Self {
            pending: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            window,
            max_keys,
        }
    }

    /// Queues one `(key, value)` write for `domain`. Returns immediately —
    /// the actual `redb` transaction happens later, on a spawned task, once
    /// this domain's batch is flushed (by the debounce timer elapsing or by
    /// hitting `max_keys`).
    pub(crate) fn submit(
        &self,
        storage_pool: crate::core::storage::StoragePool,
        domain: crate::core::storage::ValidatedDomain,
        key: String,
        value: crate::core::types::Value,
    ) {
        let domain_key = domain.as_str().to_string();
        let mut should_spawn_timer = false;
        let mut immediate_flush = None;
        {
            let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
            let entry = pending.entry(domain_key.clone()).or_default();
            let was_empty = entry.is_empty();
            entry.insert(key, value);
            if entry.len() >= self.max_keys {
                immediate_flush = Some(std::mem::take(entry));
                pending.remove(&domain_key);
            } else if was_empty {
                should_spawn_timer = true;
            }
            // else: batch already non-empty and below threshold — a timer
            // from the first write into this batch is already scheduled and
            // will pick up this write when it fires. No new timer needed.
        }

        if let Some(records) = immediate_flush {
            Self::spawn_flush(storage_pool, domain, records);
        } else if should_spawn_timer {
            let pending_arc = self.pending.clone();
            let window = self.window;
            tokio::spawn(async move {
                tokio::time::sleep(window).await;
                let records = {
                    let mut pending = pending_arc.lock().unwrap_or_else(|p| p.into_inner());
                    pending.remove(&domain_key)
                };
                if let Some(records) = records
                    && !records.is_empty()
                {
                    Self::spawn_flush(storage_pool, domain, records);
                }
            });
        }
    }

    /// Commits one batch as a single `redb` write transaction on the
    /// blocking thread pool, so filesystem I/O never blocks the async
    /// dispatch loop or the debounce timer tasks above.
    fn spawn_flush(
        storage_pool: crate::core::storage::StoragePool,
        domain: crate::core::storage::ValidatedDomain,
        records: std::collections::HashMap<String, crate::core::types::Value>,
    ) {
        tokio::task::spawn_blocking(move || {
            let engine = match storage_pool.get_or_open(&domain) {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        domain = %domain.as_str(),
                        "storage batch flush: failed to open engine"
                    );
                    return;
                }
            };
            if let Err(e) = engine.write_batch(records.iter().map(|(k, v)| (k.as_str(), v))) {
                tracing::warn!(error = %e, domain = %domain.as_str(), "storage batch flush failed");
            }
        });
    }
}


/// Maximum time allowed to establish one HTTP/3 connection: the QUIC
/// transport handshake (`Endpoint::connect(...).await`) plus the H3-layer
/// setup (`h3::client::builder().build(...).await`, which exchanges the
/// initial SETTINGS frames). A server that accepts the QUIC handshake but
/// never completes the H3-level setup — or never responds at the transport
/// level at all — would otherwise hang this call forever, holding its
/// `MAX_CONCURRENT_FETCHES` permit indefinitely.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum time allowed for one complete HTTP/3 request/response exchange
/// once a connection is established: sending the request (HEADERS + body),
/// and receiving the response HEADERS and all DATA frames. Guards against a
/// server that completes the handshake but then never sends a response, or
/// stalls mid-body.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// QUIC idle timeout: the transport closes a connection that has exchanged
/// no packets for this long, even if the application never reports an
/// error. Set on every client [`quinn::TransportConfig`] so a
/// silently-stalled-but-still-"open" connection doesn't sit around
/// indefinitely, and reused as [`H3ConnectionPool`]'s own idle-reap
/// threshold (see [`H3ConnectionPool::make_room`]) so a pool entry whose
/// underlying QUIC connection the transport has already closed for
/// idleness doesn't linger in the map either.
pub(crate) const QUIC_MAX_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// QUIC keep-alive interval: how often a PING frame is sent on an otherwise
/// idle connection to prevent NAT/firewall state from expiring and to keep
/// [`QUIC_MAX_IDLE_TIMEOUT`] from firing on connections that are merely
/// quiet, not dead. Must be well under `QUIC_MAX_IDLE_TIMEOUT`.
pub(crate) const QUIC_KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// Maximum number of live per-domain HTTP/3 connections
/// [`H3ConnectionPool`] retains at once. Reached only by a document that
/// legitimately talks to many distinct domains; once at capacity, the
/// least-recently-used connection is evicted to make room (see
/// [`H3ConnectionPool::make_room`]) rather than growing the pool without
/// bound.
pub(crate) const MAX_POOL_SIZE: usize = 32;

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
fn verify_negotiated_alpn(
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
        Self::new_with_connect_timeout(CONNECT_TIMEOUT)
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
    fn make_room<V>(
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
        Self::make_room(&mut map, now, QUIC_MAX_IDLE_TIMEOUT, MAX_POOL_SIZE);

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


/// Maximum number of [`NetworkResult`] messages buffered between the network
/// worker and the UI event loop.  When the channel is full, async tasks suspend
/// on `.send().await` rather than allocating unbounded memory.
pub(crate) const MAX_UI_CHANNEL_CAPACITY: usize = 32;

/// Maximum number of fetch operations executing concurrently inside the Tokio
/// runtime.  Tasks acquire a semaphore permit before performing any I/O and
/// park until a permit is released.
pub(crate) const MAX_CONCURRENT_FETCHES: usize = 16;

/// Spawns the background network thread and initialises the QUIC endpoint.
///
/// `allow_insecure`: when `true`, TLS certificate verification is skipped
/// (development only).  When `false`, only servers presenting a certificate
/// that chains to a WebPKI root are accepted.
pub fn spawn_network_thread(
    rx: tokio::sync::mpsc::UnboundedReceiver<NetworkCmd>,
    tx: tokio::sync::mpsc::Sender<NetworkResult>,
    #[cfg(feature = "insecure-dev")] allow_insecure: bool,
) {
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                if tx
                    .blocking_send(NetworkResult::Error(MizuError::Network(format!(
                        "Tokio error: {}",
                        e
                    ))))
                    .is_err()
                {
                    tracing::warn!(
                        "UI channel closed before Tokio startup error could be delivered"
                    );
                }
                return;
            }
        };

        rt.block_on(async move {
            let mut provider = rustls::crypto::aws_lc_rs::default_provider();
            provider.kx_groups = vec![rustls::crypto::aws_lc_rs::kx_group::X25519MLKEM768];
            let _ = provider.install_default();

            let mut endpoint = match Endpoint::client(std::net::SocketAddr::from(([0, 0, 0, 0], 0)))
            {
                Ok(ep) => ep,
                Err(e) => {
                    if tx
                        .send(NetworkResult::Error(MizuError::Network(format!(
                            "Endpoint error: {}",
                            e
                        ))))
                        .await
                        .is_err()
                    {
                        tracing::warn!("UI channel closed before QUIC endpoint error could be delivered");
                    }
                    return;
                }
            };

            // Build the OpenNIC resolver once for the lifetime of the network thread.
            let dns_resolver = crate::network::opennic::build_opennic_resolver();
            tracing::debug!("OpenNIC DNS resolver initialised");

            #[cfg(feature = "insecure-dev")]
            let mut client_config = if allow_insecure {
                tracing::warn!(
                    "insecure-dev: TLS bypass active — certificate verification skipped for local hosts only"
                );
                let roots = Arc::new(rustls::RootCertStore::from_iter(
                    webpki_roots::TLS_SERVER_ROOTS.iter().cloned(),
                ));
                let webpki = match rustls::client::WebPkiServerVerifier::builder(roots).build() {
                    Ok(v) => v,
                    Err(e) => {
                        if tx
                            .send(NetworkResult::Error(MizuError::Network(format!(
                                "insecure-dev TLS verifier build failed: {e:?}"
                            ))))
                            .await
                            .is_err()
                        {
                            tracing::warn!("UI channel closed before TLS verifier error could be delivered");
                        }
                        return;
                    }
                };
                rustls::ClientConfig::builder()
                    .dangerous()
                    .with_custom_certificate_verifier(Arc::new(LocalOrWebPkiVerifier { webpki }))
                    .with_no_client_auth()
            } else {
                let roots = rustls::RootCertStore::from_iter(
                    webpki_roots::TLS_SERVER_ROOTS.iter().cloned(),
                );
                rustls::ClientConfig::builder()
                    .with_root_certificates(roots)
                    .with_no_client_auth()
            };
            #[cfg(not(feature = "insecure-dev"))]
            let mut client_config = {
                let roots = rustls::RootCertStore::from_iter(
                    webpki_roots::TLS_SERVER_ROOTS.iter().cloned(),
                );
                rustls::ClientConfig::builder()
                    .with_root_certificates(roots)
                    .with_no_client_auth()
            };

            client_config.alpn_protocols = vec![MIZU_ALPN.to_vec()];

            let quic_config = match quinn::crypto::rustls::QuicClientConfig::try_from(client_config)
            {
                Ok(c) => c,
                Err(e) => {
                    if tx
                        .send(NetworkResult::Error(MizuError::Network(format!(
                            "TLS error: {:?}",
                            e
                        ))))
                        .await
                        .is_err()
                    {
                        tracing::warn!("UI channel closed before TLS config error could be delivered");
                    }
                    return;
                }
            };
            let mut client_config = quinn::ClientConfig::new(Arc::new(quic_config));

            // Explicit transport config: without this, quinn's library
            // defaults apply, which do not protect against an
            // application-level stall on an otherwise-alive connection (the
            // transport never notices the server just stopped talking).
            // `max_idle_timeout` closes the connection once no packets have
            // been exchanged for QUIC_MAX_IDLE_TIMEOUT; `keep_alive_interval`
            // sends PING frames often enough that a merely-quiet (not dead)
            // connection never trips it.
            let idle_timeout: quinn::IdleTimeout = match QUIC_MAX_IDLE_TIMEOUT.try_into() {
                Ok(t) => t,
                Err(e) => {
                    if tx
                        .send(NetworkResult::Error(MizuError::Network(format!(
                            "QUIC idle timeout config error: {e}"
                        ))))
                        .await
                        .is_err()
                    {
                        tracing::warn!(
                            "UI channel closed before QUIC transport config error could be delivered"
                        );
                    }
                    return;
                }
            };
            let mut transport_config = quinn::TransportConfig::default();
            transport_config
                .max_idle_timeout(Some(idle_timeout))
                .keep_alive_interval(Some(QUIC_KEEP_ALIVE_INTERVAL));
            client_config.transport_config(Arc::new(transport_config));

            endpoint.set_default_client_config(client_config);

            // Semaphore: caps concurrent active fetches to MAX_CONCURRENT_FETCHES.
            // Permits are acquired *inside* each spawned task (option b), so the
            // dispatch loop itself never blocks — StorageStore and other cheap
            // commands are always dispatched immediately.
            let semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_FETCHES));

            // HTTP/3 connection pool: reuses established QUIC connections across
            // fetches to the same domain, eliminating redundant TLS 1.3 handshakes.
            let pool = H3ConnectionPool::new();

            // Pool of open per-domain encrypted storage engines; see the
            // "Storage dispatch" block comment above.
            let storage_pool = crate::core::storage::StoragePool::new();
            // Debounces/batches `StorageStore` writes to the same domain;
            // see the "Storage dispatch" block comment above.
            let storage_debouncer = StorageWriteDebouncer::new();

            // Rebind as mutable so UnboundedReceiver::recv() can take &mut self.
            let mut rx = rx;

            while let Some(cmd) = rx.recv().await {
                match cmd {
                    NetworkCmd::Fetch {
                        method,
                        url,
                        target_var,
                        is_remote_origin,
                        payload,
                    } => {
                        let tx_clone = tx.clone();
                        let endpoint_clone = endpoint.clone();
                        let pool_clone = pool.clone();
                        let dns_clone = dns_resolver.clone();
                        let sem_clone = semaphore.clone();

                        tokio::spawn(async move {
                            // Acquire permit before doing any I/O; park if at limit.
                            let Ok(permit) = sem_clone.acquire_owned().await else {
                                return;
                            };
                            let _permit = permit; // RAII: released when this task exits

                            // Serialise the optional payload to JSON once, before any I/O,
                            // so a serialisation failure aborts cleanly without touching
                            // the network.
                            let request_body = match payload
                                .as_ref()
                                .map(|v| serde_json::to_vec(&crate::core::types::to_json(v)))
                            {
                                Some(Ok(vec)) => Some(bytes::Bytes::from(vec)),
                                Some(Err(e)) => {
                                    if tx_clone
                                        .send(NetworkResult::Error(MizuError::Network(format!(
                                            "request payload serialisation failed: {e}"
                                        ))))
                                        .await
                                        .is_err()
                                    {
                                        tracing::trace!(
                                            "UI channel closed; payload serialisation error dropped"
                                        );
                                    }
                                    return;
                                }
                                None => None,
                            };

                            match handle_fetch(
                                &endpoint_clone,
                                &pool_clone,
                                &dns_clone,
                                &method,
                                &url,
                                is_remote_origin,
                                request_body,
                            )
                            .await
                            {
                                Ok((Some(new_url), _)) => {
                                    if tx_clone
                                        .send(NetworkResult::NavigationRedirect { new_url })
                                        .await
                                        .is_err()
                                    {
                                        tracing::trace!("UI channel closed; Fetch redirect result dropped");
                                    }
                                }
                                Ok((None, data)) => {
                                    if tx_clone
                                        .send(NetworkResult::Success { target_var, data })
                                        .await
                                        .is_err()
                                    {
                                        tracing::trace!("UI channel closed; Fetch success result dropped");
                                    }
                                }
                                Err(e) => {
                                    // Surface the failure into the call's bound
                                    // variable so the document can display it.
                                    if tx_clone
                                        .send(NetworkResult::FetchFailed {
                                            target_var,
                                            error: e,
                                        })
                                        .await
                                        .is_err()
                                    {
                                        tracing::trace!("UI channel closed; Fetch error result dropped");
                                    }
                                }
                            }
                        });
                    }
                    NetworkCmd::Navigate { url } => {
                        let tx_clone = tx.clone();
                        let endpoint_clone = endpoint.clone();
                        let pool_clone = pool.clone();
                        let dns_clone = dns_resolver.clone();
                        let sem_clone = semaphore.clone();

                        tokio::spawn(async move {
                            let Ok(permit) = sem_clone.acquire_owned().await else {
                                return;
                            };
                            let _permit = permit;

                            // Navigate commands are always mizu:// targets — file:// and all
                            // other unsupported schemes are rejected upstream by
                            // resolve_navigate_url (window.rs) and additionally by handle_fetch_raw.
                            match handle_fetch(
                                &endpoint_clone,
                                &pool_clone,
                                &dns_clone,
                                "GET",
                                &url,
                                false,
                                None,
                            )
                            .await
                            {
                                Ok((Some(new_url), _)) => {
                                    tracing::debug!(url = %new_url, "navigation redirect");
                                    if tx_clone
                                        .send(NetworkResult::NavigationRedirect { new_url })
                                        .await
                                        .is_err()
                                    {
                                        tracing::trace!("UI channel closed; Navigate redirect dropped");
                                    }
                                }
                                Ok((None, crate::core::types::Value::String(source))) => {
                                    tracing::debug!("navigation payload fetched");
                                    if tx_clone
                                        .send(NetworkResult::NavigateSuccess {
                                            url,
                                            source: source.to_string(),
                                        })
                                        .await
                                        .is_err()
                                    {
                                        tracing::trace!("UI channel closed; NavigateSuccess dropped");
                                    }
                                }
                                Ok((None, _)) => {
                                    tracing::warn!(
                                        "navigation expected string payload, got other type"
                                    );
                                    if tx_clone
                                        .send(NetworkResult::Error(MizuError::Network(
                                            "Expected string payload for navigation".to_string(),
                                        )))
                                        .await
                                        .is_err()
                                    {
                                        tracing::trace!("UI channel closed; Navigate type-error dropped");
                                    }
                                }
                                Err(e) => {
                                    tracing::error!(error = ?e, "navigation fetch failed");
                                    if tx_clone.send(NetworkResult::Error(e)).await.is_err() {
                                        tracing::trace!("UI channel closed; Navigate error dropped");
                                    }
                                }
                            }
                        });
                    }
                    NetworkCmd::FetchImage {
                        url,
                        is_remote_origin,
                        sandbox_base,
                    } => {
                        let tx_clone = tx.clone();
                        let sem_clone = semaphore.clone();

                        // Local-file images: wrap spawn_blocking in an async task so
                        // we can still acquire the semaphore permit and use .await sends.
                        if url.starts_with("file://") {
                            tokio::spawn(async move {
                                let Ok(permit) = sem_clone.acquire_owned().await else {
                                    return;
                                };
                                let _permit = permit;

                                let url_clone = url.clone();
                                let url_for_err = url.clone();
                                let result = tokio::task::spawn_blocking(move || {
                                    handle_fetch_file(&url_clone, sandbox_base.as_deref())
                                        .map(|body| (url_clone, body))
                                })
                                .await;

                                match result {
                                    Ok(Ok((u, body))) => {
                                        if let Some(img) =
                                            crate::render::window::decode_image_bytes(&body)
                                        {
                                            if tx_clone
                                                .send(NetworkResult::FetchImageSuccess {
                                                    url: u,
                                                    image: img,
                                                })
                                                .await
                                                .is_err()
                                            {
                                                tracing::trace!("UI channel closed; local FetchImageSuccess dropped");
                                            }
                                        } else if tx_clone
                                            .send(NetworkResult::FetchImageFailed {
                                                url: u,
                                                error: MizuError::Network(
                                                    "Failed to decode local image bytes"
                                                        .to_string(),
                                                ),
                                            })
                                            .await
                                            .is_err()
                                        {
                                            tracing::trace!("UI channel closed; local FetchImageFailed (decode) dropped");
                                        }
                                    }
                                    Ok(Err(e)) => {
                                        if tx_clone
                                            .send(NetworkResult::FetchImageFailed {
                                                url: url_for_err,
                                                error: e,
                                            })
                                            .await
                                            .is_err()
                                        {
                                            tracing::trace!("UI channel closed; local FetchImageFailed dropped");
                                        }
                                    }
                                    Err(je) => {
                                        if tx_clone
                                            .send(NetworkResult::FetchImageFailed {
                                                url: url_for_err,
                                                error: MizuError::Network(format!(
                                                    "spawn_blocking panicked: {je}"
                                                )),
                                            })
                                            .await
                                            .is_err()
                                        {
                                            tracing::trace!("UI channel closed; local FetchImageFailed (panic) dropped");
                                        }
                                    }
                                }
                            });
                            continue;
                        }

                        let endpoint_clone = endpoint.clone();
                        let pool_clone = pool.clone();
                        let dns_clone = dns_resolver.clone();

                        tokio::spawn(async move {
                            let Ok(permit) = sem_clone.acquire_owned().await else {
                                return;
                            };
                            let _permit = permit;

                            match handle_fetch_raw(
                                &endpoint_clone,
                                &pool_clone,
                                &dns_clone,
                                "GET",
                                &url,
                                is_remote_origin,
                                None,
                            )
                            .await
                            {
                                Ok((status, headers, body)) => {
                                    let domain =
                                        MizuUri::parse(&url).map(|u| u.domain).unwrap_or_default();
                                    match parse_http_response(status, &headers, &body, &domain) {
                                        Ok(Some(new_url)) => {
                                            if tx_clone
                                                .send(NetworkResult::NavigationRedirect { new_url })
                                                .await
                                                .is_err()
                                            {
                                                tracing::trace!("UI channel closed; QUIC image redirect dropped");
                                            }
                                        }
                                        Ok(None) => {
                                            if let Some(animated_img) =
                                                crate::render::window::decode_image_bytes(&body)
                                            {
                                                if tx_clone
                                                    .send(NetworkResult::FetchImageSuccess {
                                                        url,
                                                        image: animated_img,
                                                    })
                                                    .await
                                                    .is_err()
                                                {
                                                    tracing::trace!("UI channel closed; FetchImageSuccess dropped");
                                                }
                                            } else if tx_clone
                                                .send(NetworkResult::FetchImageFailed {
                                                    url,
                                                    error: MizuError::Network(
                                                        "Failed to decode image bytes".to_string(),
                                                    ),
                                                })
                                                .await
                                                .is_err()
                                            {
                                                tracing::trace!("UI channel closed; FetchImageFailed (decode) dropped");
                                            }
                                        }
                                        Err(e) => {
                                            if tx_clone
                                                .send(NetworkResult::FetchImageFailed {
                                                    url,
                                                    error: e,
                                                })
                                                .await
                                                .is_err()
                                            {
                                                tracing::trace!("UI channel closed; FetchImageFailed (headers) dropped");
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    if tx_clone
                                        .send(NetworkResult::FetchImageFailed { url, error: e })
                                        .await
                                        .is_err()
                                    {
                                        tracing::trace!("UI channel closed; FetchImageFailed (QUIC) dropped");
                                    }
                                }
                            }
                        });
                    }
                    NetworkCmd::NetworkRequest { .. } => {
                        // Unresolved alias: the LogicWorker should have converted NetworkCall →
                        // ResolvedCall → NetworkCmd::Fetch before this command is sent.
                        // If it arrives here the alias was never resolved — discard with a warning.
                        tracing::warn!(
                            "NetworkRequest with unresolved alias reached worker — \
                             should have been converted to ResolvedCall by LogicWorker; skipped"
                        );
                    }
                    NetworkCmd::StorageStore { domain, key, value } => {
                        // Debounced/batched write — see the "Storage
                        // dispatch" block comment above for the durability
                        // tradeoff this introduces. `ValidatedDomain::from_raw`
                        // is cheap (one SHA-256 hash) so it's fine to do it
                        // here on the dispatch loop rather than deferring it
                        // into the blocking flush task.
                        let validated = crate::core::storage::ValidatedDomain::from_raw(&domain);
                        storage_debouncer.submit(storage_pool.clone(), validated, key, value);
                    }
                }
            }
        });
    });
}

/// Reads a local `file://` resource from disk, enforcing the sandbox.
///
/// `sandbox_base` is the parent directory of the currently-loaded document.
/// If `None`, all `file://` access is denied (security default).  If `Some`,
/// the resolved path must start with the base; escape attempts return
/// [`MizuError::SecurityViolation`].
fn handle_fetch_file(url_str: &str, sandbox_base: Option<&str>) -> Result<Vec<u8>, MizuError> {
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

async fn handle_fetch(
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
async fn handle_fetch_raw(
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
        crate::network::opennic::MIZU_PORT,
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
const MAX_RESPONSE_BODY_BYTES: usize = 32 * 1024 * 1024;

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
async fn do_h3_request(
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

    tokio::time::timeout(REQUEST_TIMEOUT, exchange)
        .await
        .map_err(|_elapsed| {
            MizuError::Network(format!(
                "H3 request to {} timed out after {REQUEST_TIMEOUT:?}",
                uri.domain
            ))
        })?
}

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
const MAX_TOKEN_TTL_SECS: u64 = 86_400; // 24 hours

/// Loads the vault entry for `domain`, verifies it has not expired, and checks
/// that `method` is within scope.
///
/// On expiry the stale entry is evicted before [`MizuError::SecurityViolation`]
/// is returned.  Returns `Ok(None)` when no entry exists for `domain`.
fn load_valid_entry(
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
struct MizuAuthSetHeader {
    token: String,
    scope: Vec<String>,
    exp: Option<u64>,
}

/// Parses the value of a `Mizu-Auth-Set` HTTP response header.
///
/// Expected format: `<token> SCOPE=<method>[,<method>...] EXP=<unix_seconds>`
///
/// Unknown key=value pairs are silently ignored for forward compatibility.
/// Returns `None` if the value is empty or has no token.
fn parse_mizu_auth_set_header(value: &str) -> Option<MizuAuthSetHeader> {
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
fn process_mizu_auth_set(value: &str, domain: &str) -> Result<(), MizuError> {
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
    let exp = raw_exp.min(now.saturating_add(MAX_TOKEN_TTL_SECS));

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
fn parse_http_response(
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A `rustls` certificate verifier that accepts anything — test-only,
    /// never compiled into production (unlike the `insecure-dev`-gated
    /// `LocalOrWebPkiVerifier`, which still validates non-local hosts).
    /// Used by [`test_client_endpoint`] to build a real client TLS config so
    /// tests can drive an actual QUIC handshake attempt against a local
    /// listener without needing a certificate trusted by WebPKI.
    #[derive(Debug)]
    struct AcceptAnyCertVerifier;

    impl rustls::client::danger::ServerCertVerifier for AcceptAnyCertVerifier {
        fn verify_server_cert(
            &self,
            _end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _server_name: &rustls::pki_types::ServerName<'_>,
            _ocsp_response: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            rustls::crypto::aws_lc_rs::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    /// Builds a client `Endpoint` with a real (test-only) TLS config —
    /// `mizu/3` ALPN, certificate verification skipped — so `connect()`
    /// actually attempts a QUIC handshake instead of failing synchronously
    /// with "no default client config" the way a bare `Endpoint::client(...)`
    /// does. Requires a crypto provider to already be installed (callers
    /// already do this for other reasons, e.g. building the H3 pool).
    fn test_client_endpoint() -> Endpoint {
        let mut endpoint = Endpoint::client(std::net::SocketAddr::from(([0, 0, 0, 0], 0)))
            .expect("client endpoint must be creatable");
        let mut client_config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyCertVerifier))
            .with_no_client_auth();
        client_config.alpn_protocols = vec![MIZU_ALPN.to_vec()];
        let quic_config = quinn::crypto::rustls::QuicClientConfig::try_from(client_config)
            .expect("test QuicClientConfig must build");
        endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(quic_config)));
        endpoint
    }

    // — lossy UTF-8 body decoding (parse_body_value)

    #[test]
    fn test_parse_body_value_valid_utf8() {
        let val = parse_body_value(b"hello world");
        assert_eq!(
            val,
            crate::core::types::Value::from("hello world".to_string())
        );
    }

    #[test]
    fn test_parse_body_value_invalid_utf8_replaced_with_replacement_char() {
        // 0xFF is not valid UTF-8 — must be replaced with U+FFFD, not panic.
        let val = parse_body_value(b"hello \xff world");
        match val {
            crate::core::types::Value::String(s) => {
                assert!(
                    s.contains('\u{FFFD}'),
                    "invalid bytes must be replaced with U+FFFD, got: {s:?}"
                );
                assert!(s.contains("hello"), "valid prefix must be preserved");
            }
            other => panic!("expected Value::String, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_body_value_empty_body() {
        let val = parse_body_value(b"");
        assert_eq!(val, crate::core::types::Value::from(String::new()));
    }

    #[test]
    fn test_parse_body_value_all_bytes_no_panic() {
        // Full 0..=255 range — must return Value::String without panicking.
        let body: Vec<u8> = (0u8..=255u8).collect();
        let val = parse_body_value(&body);
        assert!(
            matches!(val, crate::core::types::Value::String(_)),
            "arbitrary byte payloads must yield Value::String"
        );
    }

    // — response body size ceiling (check_response_body_budget)

    #[test]
    fn test_response_body_budget_allows_under_limit() {
        assert!(check_response_body_budget(0, 1024).is_ok());
        assert!(check_response_body_budget(MAX_RESPONSE_BODY_BYTES - 1, 1).is_ok());
    }

    #[test]
    fn test_response_body_budget_rejects_over_limit() {
        let result = check_response_body_budget(MAX_RESPONSE_BODY_BYTES, 1);
        assert!(
            matches!(result, Err(MizuError::SecurityViolation(_))),
            "exceeding the body ceiling must yield SecurityViolation (non-retryable): {result:?}"
        );
    }

    #[test]
    fn test_response_body_budget_no_overflow_panic() {
        // usize::MAX incoming must saturate, not wrap around to a small value.
        let result = check_response_body_budget(1, usize::MAX);
        assert!(
            matches!(result, Err(MizuError::SecurityViolation(_))),
            "saturating add must still reject: {result:?}"
        );
    }

    #[test]
    fn test_parse_body_value_multibyte_utf8_preserved() {
        // Valid multi-byte UTF-8 (e.g. Japanese) must round-trip without replacement.
        let text = "こんにちは世界";
        let val = parse_body_value(text.as_bytes());
        match val {
            crate::core::types::Value::String(s) => {
                assert_eq!(s.as_ref(), text, "valid UTF-8 must be preserved exactly");
                assert!(!s.contains('\u{FFFD}'), "no replacement chars expected");
            }
            other => panic!("expected Value::String, got {other:?}"),
        }
    }

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
        let result2 =
            parse_http_response(http::StatusCode::OK, &headers, b"ok", "test_domain2.local");
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

    #[test]
    fn test_uri_parsing_for_navigate() {
        let uri = MizuUri::parse("mizu://localhost/index.mizu").unwrap();
        assert_eq!(uri.domain, "localhost");
        assert_eq!(uri.path, "/index.mizu");
    }

    #[tokio::test]
    async fn test_file_scheme_always_rejected_by_h3_fetch() {
        // handle_fetch_raw must never serve file:// — those go through
        // handle_fetch_file (sandbox-enforced).
        let provider = rustls::crypto::aws_lc_rs::default_provider();
        let _ = provider.install_default();
        let endpoint = Endpoint::client(std::net::SocketAddr::from(([0, 0, 0, 0], 0))).unwrap();
        let pool = H3ConnectionPool::new();
        let dns = crate::network::opennic::build_opennic_resolver();

        for is_remote_origin in [false, true] {
            let result = handle_fetch_raw(
                &endpoint,
                &pool,
                &dns,
                "GET",
                "file:///etc/passwd",
                is_remote_origin,
                None,
            )
            .await;
            assert!(
                matches!(result, Err(MizuError::SecurityViolation(_))),
                "file:// must be rejected by the H3 fetch path \
                 (is_remote_origin={is_remote_origin}): {result:?}"
            );
        }
    }

    #[test]
    fn test_file_url_path_traversal_blocked_in_fetch_file() {
        // handle_fetch_file must block traversal attempts even when sandbox_base is provided.
        let result = handle_fetch_file(
            "file:///home/user/app/../../etc/passwd",
            Some("home/user/app"),
        );
        assert!(
            matches!(result, Err(MizuError::SecurityViolation(_))),
            "path traversal must be blocked by file_sandbox_contains, got: {result:?}"
        );
    }

    #[test]
    fn test_file_fetch_no_sandbox_base_blocked() {
        // No sandbox_base configured → all file:// access denied.
        let result = handle_fetch_file("file:///home/user/app/image.png", None);
        assert!(
            matches!(result, Err(MizuError::SecurityViolation(_))),
            "file:// with no sandbox_base must be denied: {result:?}"
        );
    }

    /// Verifies `StoragePool::write_record`'s own immediate-write guarantee:
    /// no write-behind cache sits in front of it, so the value is visible to
    /// a subsequent read with no artificial delay (no sleep between write
    /// and read). RM-12: the production `NetworkCmd::StorageStore` dispatch
    /// now goes through `StorageWriteDebouncer` instead of calling this
    /// directly (see `storage_debounce_*` tests below) — `write_record`
    /// itself is unchanged and remains available as the non-debounced,
    /// immediate-write primitive.
    #[test]
    fn test_storage_store_writes_directly_with_no_delay() {
        let tmp_dir = std::env::temp_dir().join("mizu_test_worker_direct_write");
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let path = tmp_dir.join("direct.enc");

        let db = redb::Database::create(&path).unwrap();
        {
            let write_txn = db.begin_write().unwrap();
            {
                let _ = write_txn.open_table(crate::core::storage::STORAGE_TABLE).unwrap();
            }
            write_txn.commit().unwrap();
        }
        let engine = std::sync::Arc::new(
            crate::core::storage::StorageEngine::from_parts(db, [0x33u8; 32])
        );

        let pool = crate::core::storage::StoragePool::new();
        let domain = crate::core::storage::ValidatedDomain::from_raw("direct-write-test.local");
        pool.insert_for_test(&domain, engine.clone());

        pool.write_record(&domain, "session_token", &crate::core::types::Value::from("abc123"))
            .expect("write_record must succeed");

        let data = engine.read_all().expect("read_all");
        assert_eq!(
            data.get("session_token"),
            Some(&crate::core::types::Value::from("abc123")),
            "value must be readable immediately after write_record returns, with no debounce delay"
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    /// Builds a fresh temp-file-backed `redb`-based `StorageEngine`
    /// (`write_batch_call_count()` starts at 0) for the `storage_debounce_*`
    /// tests below. Returns the engine (wrapped in `Arc`, matching how
    /// `StoragePool` stores it) and the temp directory, so callers can clean
    /// up when done.
    fn make_debounce_test_engine(
        name: &str,
    ) -> (std::sync::Arc<crate::core::storage::StorageEngine>, std::path::PathBuf) {
        let tmp_dir = std::env::temp_dir().join(format!("mizu_test_storage_debounce_{name}"));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let path = tmp_dir.join("test.enc");
        let db = redb::Database::create(&path).unwrap();
        {
            let write_txn = db.begin_write().unwrap();
            {
                let _ = write_txn.open_table(crate::core::storage::STORAGE_TABLE).unwrap();
            }
            write_txn.commit().unwrap();
        }
        let engine = std::sync::Arc::new(crate::core::storage::StorageEngine::from_parts(
            db,
            [0x55u8; 32],
        ));
        (engine, tmp_dir)
    }

    /// RM-12 (a): several `StorageStore`-equivalent `submit` calls for the
    /// same domain, issued back-to-back with no delay between them, must not
    /// each open their own `redb` transaction — they must be coalesced into
    /// one `write_batch` call once the debounce window elapses.
    #[tokio::test]
    async fn storage_debounce_batches_closely_spaced_writes_into_one_transaction() {
        let (engine, tmp_dir) = make_debounce_test_engine("batch");
        let pool = crate::core::storage::StoragePool::new();
        let domain = crate::core::storage::ValidatedDomain::from_raw("debounce-batch-test.local");
        pool.insert_for_test(&domain, engine.clone());

        let window = Duration::from_millis(60);
        let debouncer = StorageWriteDebouncer::with_params(window, 64);

        for i in 0..5 {
            debouncer.submit(
                pool.clone(),
                crate::core::storage::ValidatedDomain::from_raw("debounce-batch-test.local"),
                format!("key_{i}"),
                crate::core::types::Value::Int(i),
            );
        }

        // Still within the debounce window: nothing should have been
        // committed to redb yet.
        assert_eq!(
            engine.write_batch_call_count(),
            0,
            "writes must not be flushed before the debounce window elapses"
        );

        tokio::time::sleep(window + Duration::from_millis(100)).await;

        assert_eq!(
            engine.write_batch_call_count(),
            1,
            "5 closely-spaced writes to the same domain must land in exactly 1 redb transaction, not 5"
        );

        let data = engine.read_all().expect("read_all");
        for i in 0..5 {
            assert_eq!(
                data.get(&format!("key_{i}")),
                Some(&crate::core::types::Value::Int(i)),
                "key_{i} must be persisted and readable after the batch flush"
            );
        }

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    /// RM-12 (a)/(b): once `max_keys` distinct keys are buffered for a
    /// domain, the batch must flush immediately rather than waiting out the
    /// (here, deliberately long) debounce window — bounding worst-case
    /// latency and memory under sustained writes.
    #[tokio::test]
    async fn storage_debounce_max_keys_forces_immediate_flush() {
        let (engine, tmp_dir) = make_debounce_test_engine("maxkeys");
        let pool = crate::core::storage::StoragePool::new();
        let domain = crate::core::storage::ValidatedDomain::from_raw("debounce-maxkeys-test.local");
        pool.insert_for_test(&domain, engine.clone());

        // Window is long enough that this test would time out waiting for it
        // — the flush must instead be triggered by hitting max_keys.
        let debouncer = StorageWriteDebouncer::with_params(Duration::from_secs(30), 3);

        for i in 0..3 {
            debouncer.submit(
                pool.clone(),
                crate::core::storage::ValidatedDomain::from_raw("debounce-maxkeys-test.local"),
                format!("key_{i}"),
                crate::core::types::Value::Int(i),
            );
        }

        // Give the spawned spawn_blocking flush task a moment to run — it's
        // triggered synchronously by the 3rd `submit` call, well before the
        // 30s window would ever elapse.
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert_eq!(
            engine.write_batch_call_count(),
            1,
            "hitting max_keys must force an immediate flush without waiting for the debounce window"
        );
        let data = engine.read_all().expect("read_all");
        assert_eq!(data.len(), 3, "all 3 keys must be persisted by the threshold-triggered flush");

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    /// RM-12 (b): repeated writes to the *same* key within one debounce
    /// window must collapse to last-write-wins and still land in a single
    /// transaction — not one entry per write.
    #[tokio::test]
    async fn storage_debounce_same_key_last_write_wins() {
        let (engine, tmp_dir) = make_debounce_test_engine("lastwrite");
        let pool = crate::core::storage::StoragePool::new();
        let domain = crate::core::storage::ValidatedDomain::from_raw("debounce-lastwrite-test.local");
        pool.insert_for_test(&domain, engine.clone());

        let window = Duration::from_millis(60);
        let debouncer = StorageWriteDebouncer::with_params(window, 64);

        for v in 1..=3 {
            debouncer.submit(
                pool.clone(),
                crate::core::storage::ValidatedDomain::from_raw("debounce-lastwrite-test.local"),
                "counter".to_string(),
                crate::core::types::Value::Int(v),
            );
        }

        tokio::time::sleep(window + Duration::from_millis(100)).await;

        assert_eq!(engine.write_batch_call_count(), 1);
        let data = engine.read_all().expect("read_all");
        assert_eq!(
            data.get("counter"),
            Some(&crate::core::types::Value::Int(3)),
            "last write within the window must win"
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    /// RM-12: writes to two different domains must not be merged into one
    /// transaction — each domain gets its own independent batch/flush.
    #[tokio::test]
    async fn storage_debounce_batches_per_domain_independently() {
        let (engine_a, tmp_a) = make_debounce_test_engine("domain_a");
        let (engine_b, tmp_b) = make_debounce_test_engine("domain_b");
        let pool = crate::core::storage::StoragePool::new();
        let domain_a = crate::core::storage::ValidatedDomain::from_raw("debounce-domain-a.local");
        let domain_b = crate::core::storage::ValidatedDomain::from_raw("debounce-domain-b.local");
        pool.insert_for_test(&domain_a, engine_a.clone());
        pool.insert_for_test(&domain_b, engine_b.clone());

        let window = Duration::from_millis(60);
        let debouncer = StorageWriteDebouncer::with_params(window, 64);

        debouncer.submit(
            pool.clone(),
            crate::core::storage::ValidatedDomain::from_raw("debounce-domain-a.local"),
            "a_key".to_string(),
            crate::core::types::Value::from("a_value"),
        );
        debouncer.submit(
            pool.clone(),
            crate::core::storage::ValidatedDomain::from_raw("debounce-domain-b.local"),
            "b_key".to_string(),
            crate::core::types::Value::from("b_value"),
        );

        tokio::time::sleep(window + Duration::from_millis(100)).await;

        assert_eq!(engine_a.write_batch_call_count(), 1);
        assert_eq!(engine_b.write_batch_call_count(), 1);
        assert_eq!(
            engine_a.read_all().unwrap().get("a_key"),
            Some(&crate::core::types::Value::from("a_value"))
        );
        assert_eq!(
            engine_b.read_all().unwrap().get("b_key"),
            Some(&crate::core::types::Value::from("b_value"))
        );

        let _ = std::fs::remove_dir_all(&tmp_a);
        let _ = std::fs::remove_dir_all(&tmp_b);
    }

    /// BLOCKER 2 — Verifies that concurrent `get_or_connect` calls to the same
    /// domain do not deadlock or produce panic, and that failed connections are
    /// not cached in the H3 pool.
    ///
    /// Full connection-reuse verification requires an integration test with a
    /// live server.  This unit test focuses on the pool's concurrent safety
    /// invariants exercisable without network access:
    ///   • No deadlock when multiple tasks race on the same domain.
    ///   • Failed connections are never inserted into the pool.
    ///   • The pool correctly reports 0 entries after all attempts fail.
    ///
    /// RM-05: this used to wrap `get_or_connect` in a manual
    /// `tokio::time::timeout` from the test side, because production had no
    /// timeout of its own — the call could otherwise hang indefinitely
    /// against a non-responsive target. `get_or_connect` now enforces
    /// `CONNECT_TIMEOUT` internally, so the test calls it directly (via a
    /// short per-instance override so it stays fast) and that manual
    /// workaround is gone — see `stalled_handshake_releases_permit_within_timeout`
    /// for a test of the timeout firing itself.
    #[tokio::test]
    async fn test_h3_connection_pool_concurrent_safety_and_failed_eviction() {
        use std::sync::Arc;

        let provider = rustls::crypto::aws_lc_rs::default_provider();
        let _ = provider.install_default();

        let endpoint = Arc::new(
            Endpoint::client(std::net::SocketAddr::from(([0, 0, 0, 0], 0)))
                .expect("client endpoint must be creatable"),
        );

        // Short override so the test stays fast; still exercises the real
        // production timeout code path, not a test-side wrapper.
        let short_timeout = std::time::Duration::from_millis(500);
        let pool = Arc::new(H3ConnectionPool::new_with_connect_timeout(short_timeout));

        assert_eq!(pool.len().await, 0, "pool must be empty at construction");

        // Use localhost:1 — no server is running, all connects fail (or, for
        // a non-responsive target, time out) at the QUIC handshake stage.
        let addr: std::net::SocketAddr = "127.0.0.1:1".parse().unwrap();

        let mut handles = Vec::new();
        for _ in 0..3 {
            let pool = pool.clone();
            let ep = endpoint.clone();
            handles.push(tokio::spawn(async move {
                pool.get_or_connect(&ep, addr, "no-server.mizu.local").await
            }));
        }

        for handle in handles {
            let _ = handle.await.expect("spawned task must not panic");
        }

        assert_eq!(
            pool.len().await,
            0,
            "failed connections must never be inserted into the H3 pool"
        );
    }

    /// RM-05 — Verifies that a server which accepts the QUIC transport
    /// connection (receives and reads every packet the client sends) but
    /// never completes the application (H3) handshake causes
    /// `get_or_connect` to fail with a timeout error — rather than hanging
    /// forever — and that a semaphore permit held across the call, exactly
    /// mirroring `spawn_network_thread`'s `MAX_CONCURRENT_FETCHES` discipline
    /// (acquire before I/O, release via RAII when the task exits), is
    /// released once the call returns.
    #[tokio::test]
    async fn stalled_handshake_releases_permit_within_timeout() {
        let provider = rustls::crypto::aws_lc_rs::default_provider();
        let _ = provider.install_default();

        // A UDP socket that receives (and silently discards) every datagram
        // sent to it — the "server" accepts the transport-level connection
        // attempt (packets arrive, no ICMP port-unreachable) but never sends
        // a single byte back, so the QUIC handshake never completes.
        let blackhole = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("blackhole socket must bind");
        let blackhole_addr = blackhole.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            while blackhole.recv_from(&mut buf).await.is_ok() {
                // Deliberately never reply.
            }
        });

        // A real (not `#[cfg(insecure-dev)]`-gated) client TLS config, same
        // shape production builds, so `connect()` actually attempts the QUIC
        // handshake instead of failing synchronously with "no default client
        // config" — the blackhole never gets far enough for certificate
        // verification to matter, so accepting-anything here is fine.
        let endpoint = test_client_endpoint();
        // Short override so the test stays fast; still exercises the real
        // CONNECT_TIMEOUT code path in get_or_connect, not a mock.
        let pool = H3ConnectionPool::new_with_connect_timeout(Duration::from_millis(300));

        let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
        let sem_clone = semaphore.clone();

        let start = std::time::Instant::now();
        let task = tokio::spawn(async move {
            // Same discipline as spawn_network_thread: acquire before I/O,
            // hold across the call, release via RAII when this task exits.
            let permit = sem_clone.acquire_owned().await.unwrap();
            let _permit = permit;
            pool.get_or_connect(&endpoint, blackhole_addr, "stalled.mizu.local")
                .await
        });

        // The outer bound is generous relative to the pool's 300ms connect
        // timeout — if the production fix regressed, this fires instead of
        // the test hanging forever.
        let result = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect(
                "get_or_connect must return well within the test's outer bound \
                 — a stalled handshake must not hang forever",
            )
            .expect("task must not panic");
        let elapsed = start.elapsed();

        match result {
            Err(MizuError::Network(_)) => {}
            Ok(_) => panic!("a stalled handshake must not succeed"),
            Err(other) => panic!("expected a Network (timeout) error, got: {other:?}"),
        }
        // Sanity check that this actually exercised the timeout path (the
        // connect attempt genuinely reached the QUIC handshake and hung
        // there) rather than failing some other, instant way.
        assert!(
            elapsed >= Duration::from_millis(250),
            "expected the 300ms connect_timeout to be what bounded this call, \
             but it returned after only {elapsed:?} — likely failed for a \
             different (non-timeout) reason"
        );

        // The permit was released when the spawned task exited (RAII drop
        // of `_permit`), so a fresh acquire is immediately available.
        assert_eq!(
            semaphore.available_permits(),
            1,
            "the semaphore permit must be released once the stalled connect times out"
        );
    }

    /// RM-05 — Verifies `H3ConnectionPool::make_room` — the exact function
    /// `get_or_connect` calls before inserting a new entry — never lets the
    /// pool grow beyond `MAX_POOL_SIZE`, even when connecting to far more
    /// distinct domains than the limit allows. Exercised directly on the
    /// eviction *decision* logic (generic over the stored value, `()` here)
    /// rather than through `get_or_connect`, since constructing `MAX_POOL_SIZE
    /// + 1` genuine live H3 connections would require that many real servers;
    /// this tests the identical code path production uses.
    #[test]
    fn pool_never_exceeds_max_size() {
        let mut map: std::collections::HashMap<String, ((), Instant)> =
            std::collections::HashMap::new();
        let now = Instant::now();

        for i in 0..(MAX_POOL_SIZE + 10) {
            H3ConnectionPool::make_room(&mut map, now, QUIC_MAX_IDLE_TIMEOUT, MAX_POOL_SIZE);
            map.insert(format!("domain-{i}.example"), ((), now));
            assert!(
                map.len() <= MAX_POOL_SIZE,
                "pool must never exceed MAX_POOL_SIZE ({MAX_POOL_SIZE}) while \
                 inserting domain #{i}, got {}",
                map.len()
            );
        }

        assert_eq!(
            map.len(),
            MAX_POOL_SIZE,
            "pool must be exactly at capacity after inserting more domains than it allows"
        );
    }

    /// RM-05 — `make_room` must also reap entries idle longer than
    /// `max_idle`, independent of the size cap.
    #[test]
    fn pool_reaps_idle_entries() {
        let mut map: std::collections::HashMap<String, ((), Instant)> =
            std::collections::HashMap::new();
        let now = Instant::now();
        let long_idle = now - Duration::from_secs(120);

        map.insert("stale.example".to_string(), ((), long_idle));
        map.insert("fresh.example".to_string(), ((), now));

        H3ConnectionPool::make_room(&mut map, now, QUIC_MAX_IDLE_TIMEOUT, MAX_POOL_SIZE);

        assert!(
            !map.contains_key("stale.example"),
            "an entry idle longer than max_idle must be reaped"
        );
        assert!(
            map.contains_key("fresh.example"),
            "a recently-used entry must not be reaped"
        );
    }

    /// MAJOR 2 — Verifies that dot-path interpolation correctly falls through to
    /// the global store when the overlay contains the root key but the full
    /// nested path is absent.
    ///
    /// Pre-fix behaviour: `{user.email}` resolves to the literal `{user.email}`
    /// because `handled` was set to `true` as soon as the overlay contained any
    /// `user` key, even though `resolve_dot_path` returned `None`.
    ///
    /// Post-fix behaviour: `handled` is `false` when `resolve_dot_path` returns
    /// `None`, so Phase 2 (global store) is consulted and the correct email is
    /// returned.
    #[test]
    fn test_dot_path_cascade_to_global_store_when_overlay_lacks_leaf() {
        use std::collections::{BTreeMap, HashMap};
        use std::sync::Arc;

        // Global store: user record that has both `name` and `email`.
        let mut store = crate::core::types::VariableStore::new();
        let mut global_user = Vec::<(std::sync::Arc<str>, crate::core::types::Value)>::new();
        global_user.push((Arc::from("name"), crate::core::types::Value::from("Alice")));
        global_user.push((Arc::from("email"), crate::core::types::Value::from("alice@example.com")));
        store.set(
            "user",
            { global_user.sort_by(|a, b| a.0.cmp(&b.0)); crate::core::types::Value::Record(Arc::from(global_user)) },
        );

        // Overlay: user record that only has `name` — no `email` field.
        let mut overlay_user = Vec::<(std::sync::Arc<str>, crate::core::types::Value)>::new();
        overlay_user.push((Arc::from("name"), crate::core::types::Value::from("Bob")));
        let mut overlay: HashMap<String, crate::core::types::Value> = HashMap::new();
        overlay.insert(
            "user".to_string(),
            { overlay_user.sort_by(|a, b| a.0.cmp(&b.0)); crate::core::types::Value::Record(Arc::from(overlay_user)) },
        );

        // Interpolating `{user.name}` should resolve from the overlay (Bob).
        let name_result = store
            .interpolate_with_overlay("{user.name}", &overlay)
            .expect("interpolation must not error");
        assert_eq!(
            name_result, "Bob",
            "overlay must win for a path it fully resolves ({{user.name}})"
        );

        // Interpolating `{user.email}` must cascade to the global store because
        // the overlay's user record lacks the `email` field.
        //
        // Pre-fix:  returns "{user.email}" (raw placeholder) — handled was true
        //           even though resolve_dot_path returned None.
        // Post-fix: returns "alice@example.com" from the global store.
        let email_result = store
            .interpolate_with_overlay("{user.email}", &overlay)
            .expect("interpolation must not error");
        assert_eq!(
            email_result, "alice@example.com",
            "global store must be consulted when overlay root exists but path is incomplete"
        );

        // Confirm that a path absent from both overlay AND global store still
        // renders the raw placeholder (unchanged from the pre-fix behaviour).
        let missing_result = store
            .interpolate_with_overlay("{user.phone}", &overlay)
            .expect("interpolation must not error");
        assert_eq!(
            missing_result, "{user.phone}",
            "path absent from both overlay and global store must render as raw placeholder"
        );
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
        let result =
            parse_http_response(http::StatusCode::NOT_FOUND, &headers, b"not found", "x.local");
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
        let result = parse_http_response(
            http::StatusCode::FOUND,
            &headers,
            b"",
            "example.local",
        );
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

    /// The ALPN constant must be exactly `b"mizu/3"`.
    #[test]
    fn test_mizu_alpn_constant_is_mizu3() {
        assert_eq!(
            MIZU_ALPN, b"mizu/3",
            "MIZU_ALPN must be exactly b\"mizu/3\""
        );
    }

    /// RM-11 — `verify_negotiated_alpn` must reject a server that completed
    /// the QUIC handshake without ever negotiating an ALPN protocol at all
    /// (the RFC 7301 gap the doc comment on `H3ConnectionPool` used to claim
    /// was closed but wasn't), as well as a server that negotiated some
    /// other protocol, and must accept only an exact `mizu/3` match.
    #[test]
    fn test_verify_negotiated_alpn_rejects_missing_or_wrong_protocol() {
        let no_protocol: Box<dyn std::any::Any> = Box::new(quinn::crypto::rustls::HandshakeData {
            protocol: None,
            server_name: None,
        });
        let result = verify_negotiated_alpn(Some(no_protocol), "no-alpn.mizu.test");
        assert!(
            matches!(result, Err(MizuError::SecurityViolation(_))),
            "a handshake that negotiated no ALPN protocol at all must be rejected: {result:?}"
        );

        let wrong_protocol: Box<dyn std::any::Any> =
            Box::new(quinn::crypto::rustls::HandshakeData {
                protocol: Some(b"h3".to_vec()),
                server_name: None,
            });
        let result = verify_negotiated_alpn(Some(wrong_protocol), "wrong-alpn.mizu.test");
        assert!(
            matches!(result, Err(MizuError::SecurityViolation(_))),
            "a handshake that negotiated a different ALPN protocol must be rejected: {result:?}"
        );

        let result = verify_negotiated_alpn(None, "no-handshake-data.mizu.test");
        assert!(
            matches!(result, Err(MizuError::SecurityViolation(_))),
            "missing handshake data entirely must be rejected, not treated as trusted: {result:?}"
        );

        let correct_protocol: Box<dyn std::any::Any> =
            Box::new(quinn::crypto::rustls::HandshakeData {
                protocol: Some(MIZU_ALPN.to_vec()),
                server_name: None,
            });
        assert!(
            verify_negotiated_alpn(Some(correct_protocol), "ok.mizu.test").is_ok(),
            "an exact mizu/3 match must be accepted"
        );
    }
}

/// Always-compiled constant — `false` in production builds; `true` only when the
/// crate is compiled with `--features insecure-dev`.
#[allow(dead_code)] // intentional: available in test builds and insecure-dev builds
pub(crate) const INSECURE_DEV_ACTIVE: bool = cfg!(feature = "insecure-dev");

/// Returns `true` when `host` is a loopback address (`127.0.0.0/8`, `::1`) or a
/// loopback hostname (`localhost`, `*.localhost`).
///
/// Deliberately excludes RFC 1918 private ranges and `.local` (mDNS) names:
/// on a shared LAN those can be claimed or answered by other machines, so they
/// receive no special trust — neither for the insecure-dev TLS bypass, nor for
/// the file→remote SSRF block, nor for the storage quota tier.  Only traffic
/// that provably never leaves this machine is treated as local.
///
/// Compiled in all configurations so that the locality invariant is testable
/// regardless of the active feature set.
#[allow(dead_code)] // intentional: used by is_local_server_name (insecure-dev) and tests
pub(crate) fn is_local_host(host: &str) -> bool {
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    if let Ok(addr) = host.parse::<std::net::IpAddr>() {
        return addr.is_loopback();
    }
    false
}

/// Classifies a [`rustls::pki_types::ServerName`] as local or non-local.
///
/// This is the single source of truth used by [`LocalOrWebPkiVerifier`].
/// Compiled only when `insecure-dev` is active.
#[cfg(feature = "insecure-dev")]
fn is_local_server_name(server_name: &rustls::pki_types::ServerName<'_>) -> bool {
    match server_name {
        rustls::pki_types::ServerName::DnsName(name) => is_local_host(name.as_ref()),
        rustls::pki_types::ServerName::IpAddress(addr) => match addr {
            rustls::pki_types::IpAddr::V4(v4) => {
                let std_v4 = std::net::Ipv4Addr::from(*v4);
                std_v4.is_loopback()
            }
            rustls::pki_types::IpAddr::V6(v6) => {
                let std_v6 = std::net::Ipv6Addr::from(*v6);
                std_v6.is_loopback()
            }
        },
        _ => false, // ServerName is #[non_exhaustive]
    }
}

/// TLS verifier active only in `insecure-dev` builds when `--allow-insecure` is set.
///
/// * **Loopback hosts** (`localhost` / `*.localhost` / `127.0.0.0/8` / `::1`):
///   bypasses certificate verification and emits a `tracing::warn!`.
/// * **All other hosts** (including RFC 1918 LAN addresses and `.local` mDNS
///   names): delegates to WebPKI — `--allow-insecure` has no effect for them;
///   invalid certificates still cause connection failures.
#[cfg(feature = "insecure-dev")]
struct LocalOrWebPkiVerifier {
    webpki: Arc<rustls::client::WebPkiServerVerifier>,
}

#[cfg(feature = "insecure-dev")]
impl std::fmt::Debug for LocalOrWebPkiVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalOrWebPkiVerifier").finish()
    }
}

#[cfg(feature = "insecure-dev")]
impl rustls::client::danger::ServerCertVerifier for LocalOrWebPkiVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        intermediates: &[rustls::pki_types::CertificateDer<'_>],
        server_name: &rustls::pki_types::ServerName<'_>,
        ocsp_response: &[u8],
        now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if is_local_server_name(server_name) {
            tracing::warn!(
                server = ?server_name,
                "insecure-dev: TLS certificate verification bypassed for local host"
            );
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            self.webpki.verify_server_cert(
                end_entity,
                intermediates,
                server_name,
                ocsp_response,
                now,
            )
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.webpki.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.webpki.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.webpki.supported_verify_schemes()
    }
}

#[cfg(test)]
mod tests_insecure_dev {
    use super::*;

    /// Verifies that `INSECURE_DEV_ACTIVE` is `false` in the default (production)
    /// build.  The bypass must never be compiled in without an explicit opt-in.
    ///
    /// Gated on `not(feature = "insecure-dev")`: when the suite itself is
    /// compiled with the opt-in feature the constant is `true` by definition,
    /// so the assertion is only meaningful in the default configuration.
    #[cfg(not(feature = "insecure-dev"))]
    #[test]
    fn test_insecure_mode_disabled_by_default() {
        assert!(
            !INSECURE_DEV_ACTIVE,
            "insecure-dev must be inactive in default/production builds"
        );
    }

    /// Everything that is not provably loopback must be rejected by
    /// `is_local_host` — public hosts, but also RFC 1918 LAN addresses and
    /// `.local` mDNS names, which other machines on a shared network can claim.
    #[test]
    fn test_insecure_mode_rejected_for_public_hosts() {
        let public_hosts = [
            "example.com",
            "8.8.8.8",
            "1.1.1.1",
            "evil.localhost.example.com", // not a .localhost suffix
            "bar.local",                  // mDNS — spoofable on a shared LAN
            "192.168.0.1",                // RFC 1918 — not loopback
            "10.0.0.1",                   // RFC 1918 — not loopback
            "172.16.0.1",                 // RFC 1918 — not loopback
            "192.167.0.1",
            "172.15.255.255",
            "11.0.0.1",
        ];
        for host in public_hosts {
            assert!(
                !is_local_host(host),
                "is_local_host must return false for non-loopback host: {host}"
            );
        }
    }

    /// Only loopback addresses and `localhost` / `*.localhost` hostnames must
    /// be accepted by `is_local_host`.
    #[test]
    fn test_insecure_mode_allowed_for_loopback() {
        let local_hosts = ["localhost", "foo.localhost", "127.0.0.1", "127.1.2.3", "::1"];
        for host in local_hosts {
            assert!(
                is_local_host(host),
                "is_local_host must return true for loopback host: {host}"
            );
        }
    }
}

#[cfg(test)]
mod tests_backpressure {
    use super::*;
    use crate::core::errors::MizuError;
    use crate::network::NetworkResult;

    /// Verifies that the UI-bound channel is bounded at `MAX_UI_CHANNEL_CAPACITY`.
    ///
    /// Scenario: the network worker attempts to send 100 results but the UI
    /// thread stops consuming.  After `MAX_UI_CHANNEL_CAPACITY` messages the
    /// channel must be full and `try_send` must fail — no unbounded allocation.
    #[tokio::test]
    async fn test_network_to_ui_backpressure_sustained_flood() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<NetworkResult>(MAX_UI_CHANNEL_CAPACITY);

        // Fill the channel to capacity — every try_send up to the limit must succeed.
        for i in 0..MAX_UI_CHANNEL_CAPACITY {
            tx.try_send(NetworkResult::Error(MizuError::Network(format!("msg {i}"))))
                .unwrap_or_else(|_| panic!("try_send must succeed for slot {i}"));
        }

        // The (MAX_UI_CHANNEL_CAPACITY + 1)-th message must be rejected immediately.
        let overflow = tx.try_send(NetworkResult::Error(MizuError::Network(
            "overflow".to_string(),
        )));
        assert!(
            overflow.is_err(),
            "channel must reject messages beyond MAX_UI_CHANNEL_CAPACITY={MAX_UI_CHANNEL_CAPACITY}"
        );

        // Drain and verify exactly MAX_UI_CHANNEL_CAPACITY messages were buffered.
        let mut count = 0usize;
        while rx.try_recv().is_ok() {
            count += 1;
        }
        assert_eq!(
            count, MAX_UI_CHANNEL_CAPACITY,
            "exactly MAX_UI_CHANNEL_CAPACITY messages must have been buffered"
        );
    }

    /// Verifies that the semaphore caps concurrent active fetches at
    /// `MAX_CONCURRENT_FETCHES` even when 50 tasks are spawned simultaneously.
    #[tokio::test]
    async fn test_concurrent_fetch_throttling_limits() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_FETCHES));
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..50 {
            let sem = semaphore.clone();
            let active = active.clone();
            let peak = peak.clone();
            handles.push(tokio::spawn(async move {
                let Ok(permit) = sem.acquire_owned().await else {
                    return;
                };
                let _permit = permit;
                // Track concurrent executions and record the peak.
                let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(current, Ordering::SeqCst);
                // Simulate network I/O latency.
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                active.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for h in handles {
            h.await.unwrap_or(());
        }

        let observed_peak = peak.load(Ordering::SeqCst);
        assert!(
            observed_peak <= MAX_CONCURRENT_FETCHES,
            "peak concurrent fetches ({observed_peak}) must not exceed \
             MAX_CONCURRENT_FETCHES ({MAX_CONCURRENT_FETCHES})"
        );
        // Confirm all 50 tasks eventually ran (semaphore is not permanently exhausted).
        assert_eq!(
            active.load(Ordering::SeqCst),
            0,
            "all tasks must have completed"
        );
    }

    /// Verifies graceful recovery: tasks suspended on a full channel are
    /// unblocked as soon as the UI drains messages via `try_recv`.
    #[tokio::test]
    async fn test_backpressure_graceful_recovery() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<NetworkResult>(MAX_UI_CHANNEL_CAPACITY);

        // Fill the channel to capacity so the next send will block.
        for i in 0..MAX_UI_CHANNEL_CAPACITY {
            tx.try_send(NetworkResult::Error(MizuError::Network(format!(
                "fill {i}"
            ))))
            .unwrap_or_else(|_| panic!("fill slot {i} must succeed"));
        }

        // Spawn a task that blocks on the full channel — simulates a suspended fetch.
        let tx2 = tx.clone();
        let sender = tokio::spawn(async move {
            tx2.send(NetworkResult::Error(MizuError::Network(
                "recovered".to_string(),
            )))
            .await
            .unwrap_or(());
        });

        // Give the sender a tick to reach the awaiting-send state.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // UI resumes consuming — drain the backlog.
        let mut drained = 0usize;
        while rx.try_recv().is_ok() {
            drained += 1;
        }
        assert_eq!(
            drained, MAX_UI_CHANNEL_CAPACITY,
            "all buffered messages must be drained"
        );

        // The suspended sender must now complete within a short timeout.
        tokio::time::timeout(std::time::Duration::from_millis(200), sender)
            .await
            .unwrap_or_else(|_| panic!("suspended sender must unblock after channel drains"))
            .unwrap_or(());

        // The recovery message must have arrived in the channel.
        let recovered = rx.try_recv().unwrap_or_else(|_| {
            panic!("recovered message must be in channel after sender completes")
        });
        assert!(
            matches!(recovered, NetworkResult::Error(_)),
            "recovered message must be the one sent by the suspended task"
        );
    }
}
