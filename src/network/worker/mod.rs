//! Background network worker thread: QUIC/H3 connection pooling, fetch
//! dispatch, storage write debouncing, and TLS/local-host trust policy.
//!
//! ## Module Layout
//!
//! * [`storage_debounce`] — `StorageWriteDebouncer` (S2 invariant).
//! * [`h3_pool`] — `H3ConnectionPool`, ALPN verification, connect/request
//!   timeouts.
//! * [`fetch`] — `file://`/HTTP(S) fetch dispatch and the H3 request
//!   execution (`do_h3_request`).
//! * [`auth`] — `Mizu-Auth-Set` header parsing and vault token import.
//! * [`tls`] — local-host classification and the `insecure-dev` certificate
//!   verifier.
//!
//! Every item that was previously a direct member of this module is
//! re-exported below, so `crate::network::worker::X` paths are unaffected by
//! this split.

use std::sync::Arc;
#[cfg(test)]
use std::time::{Duration, Instant};

use quinn::Endpoint;

use crate::core::errors::MizuError;
use crate::network::uri::MizuUri;
#[cfg(test)]
use crate::network::vault::VaultEntry;
use crate::network::{NetworkCmd, NetworkResult};

mod auth;
mod fetch;
mod h3_pool;
mod storage_debounce;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_backpressure;
#[cfg(test)]
mod tests_insecure_dev;
mod tls;

pub(crate) use h3_pool::{
    H3ConnectionPool, QUIC_KEEP_ALIVE_INTERVAL,
    QUIC_MAX_IDLE_TIMEOUT,
};
pub(crate) use storage_debounce::StorageWriteDebouncer;
pub(crate) use tls::is_local_host;
#[cfg(test)]
pub(crate) use tls::INSECURE_DEV_ACTIVE;

use auth::parse_http_response;
use fetch::{handle_fetch, handle_fetch_file, handle_fetch_raw};
#[cfg(feature = "insecure-dev")]
use tls::LocalOrWebPkiVerifier;

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

/// The sole ALPN token advertised by Mizu clients and enforced on every
/// incoming connection.  Servers that do not negotiate this exact token are
/// dropped before any application data is exchanged.
pub(crate) const MIZU_ALPN: &[u8] = b"mizu/3";

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
