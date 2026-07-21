//! Local-host classification and the `insecure-dev` certificate verifier.

#[cfg(feature = "insecure-dev")]
use std::sync::Arc;

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
pub(super) struct LocalOrWebPkiVerifier {
    pub(super) webpki: Arc<rustls::client::WebPkiServerVerifier>,
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
