//! Test suite for `network::worker`, split to mirror the source modules it
//! exercises: [`fetch`] (body parsing, budget, file:// sandbox),
//! [`auth`] (`Mizu-Auth-Set`, HTTP response classification), [`h3_pool`]
//! (connection pool eviction/timeout, ALPN verification), and
//! [`storage_debounce`] (write coalescing).
//!
//! Fixtures shared across buckets (the accept-any TLS verifier and its
//! client endpoint, used only by `h3_pool` tests but built once here to
//! match the original file's layout) live here so each submodule can reach
//! them via `use super::*;`.

use super::auth::*;
use super::fetch::*;
use super::h3_pool::*;
use super::storage_debounce::*;
use super::*;
use crate::network::uri::MizuUri;

mod auth;
mod fetch;
mod h3_pool;
mod storage_debounce;

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

/// Builds a fresh temp-file-backed `redb`-based `StorageEngine`
/// (`write_batch_call_count()` starts at 0) for the `storage_debounce_*`
/// tests below. Returns the engine (wrapped in `Arc`, matching how
/// `StoragePool` stores it) and the temp directory, so callers can clean
/// up when done.
fn make_debounce_test_engine(
    name: &str,
) -> (
    std::sync::Arc<crate::core::storage::StorageEngine>,
    std::path::PathBuf,
) {
    let tmp_dir = std::env::temp_dir().join(format!("mizu_test_storage_debounce_{name}"));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let path = tmp_dir.join("test.enc");
    let db = redb::Database::create(&path).unwrap();
    {
        let write_txn = db.begin_write().unwrap();
        {
            let _ = write_txn
                .open_table(crate::core::storage::STORAGE_TABLE)
                .unwrap();
        }
        write_txn.commit().unwrap();
    }
    let engine = std::sync::Arc::new(crate::core::storage::StorageEngine::from_parts(
        db,
        [0x55u8; 32],
    ));
    (engine, tmp_dir)
}
