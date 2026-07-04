//! DNS-over-TLS (DoT) split-horizon resolver for `mizu://`.
//!
//! # Architecture
//!
//! All DNS queries are transmitted exclusively over TLS (RFC 7858, port 853).
//! Plain UDP/TCP port-53 queries are **categorically forbidden**: every
//! [`NameServerConfig`] in every pool uses [`Protocol::Tls`].  No cleartext
//! DNS traffic can leak, preventing ISP NXDOMAIN hijacking and traffic analysis.
//!
//! ## Two-pool split-horizon routing
//!
//! | Pool | Servers | Covers |
//! |------|---------|--------|
//! | **Primary** | Quad9 + Cloudflare DoT | ICANN domains (`.com`, `.org`, …) |
//! | **OpenNIC** | OpenNIC Tier-2 DoT | Alternative TLDs (`.geek`, `.pirate`, …) |
//!
//! Every query's TLD is inspected before dispatch (see [`select_pool_for_domain`]):
//! * OpenNIC TLDs → OpenNIC pool.
//! * Everything else → primary pool, with transparent fallback to OpenNIC on
//!   transient network errors (OpenNIC Tier-2 nodes also resolve ICANN).
//!
//! ## Resilience within each pool
//!
//! Each pool's [`TokioAsyncResolver`] is built with all servers for that pool.
//! Hickory's built-in connection manager queries servers in parallel (fastest-
//! response strategy), handles per-server TLS reconnection, retry back-off, and
//! failure isolation automatically — providing round-robin–style load distribution
//! and per-node health-aware selection with no additional code.
//!
//! ## Certificate validation
//!
//! The `webpki-roots` Cargo feature is enabled on `hickory-resolver`.  Every TLS
//! handshake is verified against the WebPKI root store embedded in the crate.  A
//! DoT endpoint whose certificate chain is not trusted by a WebPKI root is
//! rejected before any DNS payload is exchanged.  The per-entry `tls_dns_name`
//! field carries the SNI hostname used for both the TLS `ClientHello` extension
//! and certificate identity verification.
//!
//! ## Special cases
//!
//! * Bare IP addresses → returned as-is (no DNS query issued).
//! * `localhost` → resolves to `127.0.0.1` immediately (no DNS query issued).

#![forbid(unsafe_code)]

use std::future::Future;
use std::net::{IpAddr, SocketAddr};

use hickory_resolver::{
    TokioAsyncResolver,
    config::{NameServerConfig, NameServerConfigGroup, Protocol, ResolverConfig, ResolverOpts},
};

use crate::core::errors::MizuError;


/// Primary DoT servers for standard ICANN domains.
///
/// Two providers are included for redundancy:
/// * **Quad9** (`9.9.9.9`, `149.112.112.112`): threat-intelligence blocking,
///   GDPR-compliant, operated by a Swiss non-profit (Quad9 foundation).
/// * **Cloudflare** (`1.1.1.1`, `1.0.0.1`): fastest global DoT latency;
///   no-logging policy independently audited by KPMG.
///
/// Both providers commit to not selling query data.
static PRIMARY_DOT_SERVERS: &[(&str, u16, &str)] = &[
    ("9.9.9.9", 853, "dns.quad9.net"),         // Quad9 — primary IPv4
    ("149.112.112.112", 853, "dns.quad9.net"), // Quad9 — secondary IPv4
    ("1.1.1.1", 853, "cloudflare-dns.com"),    // Cloudflare — primary IPv4
    ("1.0.0.1", 853, "cloudflare-dns.com"),    // Cloudflare — secondary IPv4
];

/// OpenNIC Tier-2 DoT server pool.
///
/// These nodes are required for alternative TLDs that ICANN resolvers cannot
/// serve.  The four original IPs are retained as bootstrap seeds; future
/// Tier-2 nodes discovered from `opennic.glue` DNS are candidates for
/// expansion of this list.
///
/// All four entries use DoT port 853 and a verified SNI hostname.  No
/// cleartext port-53 entry exists in this list.
static OPENNIC_DOT_SERVERS: &[(&str, u16, &str)] = &[
    ("185.121.177.177", 853, "ns4.any.dns.opennic.glue"), // T2 anycast — global
    ("169.239.202.202", 853, "ns4.any.dns.opennic.glue"), // T2 anycast — global
    ("198.251.90.108", 853, "ns3.any.dns.opennic.glue"),  // T2 — North America
    ("185.56.187.149", 853, "ns1.is.dns.opennic.glue"),   // T2 — Europe
];

/// Top-level domains served exclusively by the OpenNIC network.
///
/// Domains whose TLD matches one of these labels are routed to the OpenNIC
/// pool.  All other TLDs (standard ICANN) go to the primary pool.
///
/// Source: <https://wiki.opennic.org/opennic/dot>  (as of 2024-06)
const OPENNIC_TLDS: &[&str] = &[
    "bbs", "chan", "cyb", "dyn", "epic", "free", "fur", "geek", "gopher", "indy", "libre",
    "neo", "null", "o", "oss", "oz", "parody", "pirate", "te", "uu",
];


/// Which resolver pool should handle a DNS query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DnsPool {
    /// Quad9 + Cloudflare DoT — for standard ICANN domains.
    Primary,
    /// OpenNIC Tier-2 DoT — for alternative TLDs (`.geek`, …).
    OpenNic,
}

/// Returns which DNS pool should resolve `domain`.
///
/// Extracts the rightmost label (TLD) of `domain` after stripping an optional
/// trailing dot, then compares it case-insensitively against the known set of
/// OpenNIC-only TLDs.
///
/// # Examples
///
/// ```
/// use mizu::network::opennic::{select_pool_for_domain, DnsPool};
/// assert_eq!(select_pool_for_domain("google.com"), DnsPool::Primary);
/// assert_eq!(select_pool_for_domain("chat.geek"), DnsPool::OpenNic);
/// ```
pub fn select_pool_for_domain(domain: &str) -> DnsPool {
    let bare = domain.trim_end_matches('.');
    let tld = bare.rsplit('.').next().unwrap_or(bare);
    if OPENNIC_TLDS.iter().any(|&t| t.eq_ignore_ascii_case(tld)) {
        DnsPool::OpenNic
    } else {
        DnsPool::Primary
    }
}


/// Split-horizon DoT resolver for Mizu.
///
/// Internally maintains two [`TokioAsyncResolver`] pools:
/// * `primary` — Quad9 + Cloudflare, for ICANN domains.
/// * `opennic` — OpenNIC Tier-2, for alternative TLDs.
///
/// Both pools are cheap to clone (backed by `Arc`).
/// Construct via [`build_opennic_resolver`].
#[derive(Clone)]
pub struct MizuDnsResolver {
    primary: TokioAsyncResolver,
    opennic: TokioAsyncResolver,
}

/// Mizu protocol port used on every `mizu://` server.
pub const MIZU_PORT: u16 = 7399;


/// Builds [`NameServerConfig`] entries for the given server list.
///
/// Every entry uses [`Protocol::Tls`] and an explicit SNI hostname.
/// The `tls_config` field is left `None` so hickory's internal `CLIENT_CONFIG`
/// (populated from `webpki-roots`) provides certificate chain validation.
///
/// Exposed as `pub(crate)` so tests can inspect the produced configs without
/// constructing a full resolver.
pub(crate) fn build_nameserver_configs(servers: &[(&str, u16, &str)]) -> Vec<NameServerConfig> {
    servers
        .iter()
        .filter_map(|(ip_str, port, sni)| {
            let ip: IpAddr = ip_str.parse().ok()?;
            let mut cfg = NameServerConfig::new(SocketAddr::new(ip, *port), Protocol::Tls);
            cfg.tls_dns_name = Some((*sni).to_string());
            Some(cfg)
        })
        .collect()
}

fn build_resolver_from_pool(servers: &[(&str, u16, &str)]) -> TokioAsyncResolver {
    let configs = build_nameserver_configs(servers);
    let group = NameServerConfigGroup::from(configs);
    let config = ResolverConfig::from_parts(None, vec![], group);

    let mut opts = ResolverOpts::default();
    // Per-server query timeout; hickory's parallel-query strategy returns as
    // soon as the first server answers, so overall latency ≈ min(server RTTs).
    opts.timeout = std::time::Duration::from_secs(4);
    // Retry each server at most twice before the pool gives up.
    opts.attempts = 2;
    // DoT is stream-based; TCP fallback is only relevant for UDP paths.
    opts.try_tcp_on_error = false;
    // Never apply OS ndots / search-domain logic to `mizu://` host names.
    opts.ndots = 0;

    TokioAsyncResolver::tokio(config, opts)
}

/// Constructs the split-horizon [`MizuDnsResolver`] with both DoT pools.
///
/// The returned resolver is cheap to clone and must be called while a Tokio
/// runtime is active (required by hickory's connection manager initialisation).
pub fn build_opennic_resolver() -> MizuDnsResolver {
    MizuDnsResolver {
        primary: build_resolver_from_pool(PRIMARY_DOT_SERVERS),
        opennic: build_resolver_from_pool(OPENNIC_DOT_SERVERS),
    }
}


/// Tries `primary_lookup` first; if it returns a transient network error,
/// retries transparently via `fallback_lookup`.
///
/// DNS-level errors (NXDOMAIN, SERVFAIL, format errors) are **not** retried —
/// they are authoritative responses that should propagate to the caller.
///
/// The function accepts generic `Future` arguments so that tests can inject
/// synchronous mock results without requiring live DNS infrastructure.
pub(crate) async fn resolve_with_pool_fallback(
    primary_lookup: impl Future<Output = Result<SocketAddr, MizuError>>,
    fallback_lookup: impl Future<Output = Result<SocketAddr, MizuError>>,
) -> Result<SocketAddr, MizuError> {
    match primary_lookup.await {
        Ok(addr) => Ok(addr),
        Err(e) if is_transient_dns_error(&e) => {
            tracing::warn!(
                error = %e,
                "primary DoT pool failed; retrying via secondary pool"
            );
            fallback_lookup.await
        }
        Err(e) => Err(e),
    }
}

/// Returns `true` for network-level errors that justify a pool switch.
///
/// Returns `false` for DNS-level errors (NXDOMAIN, SERVFAIL, etc.) — those are
/// authoritative responses and must not trigger a pool retry.
///
/// Matches on the strongly-typed [`hickory_resolver::error::ResolveErrorKind`]
/// variants so the classification is immune to upstream changes in error message
/// formatting.  Only `Timeout` and `Io` errors (connection-refused, network-
/// unreachable, etc.) are transient; all other variants (including
/// `NoRecordsFound`, `Message`, `Proto`) are authoritative.
fn is_transient_dns_error(e: &MizuError) -> bool {
    use hickory_resolver::error::ResolveErrorKind;
    use std::io::ErrorKind as IOKind;

    let MizuError::DnsError(re) = e else {
        return false;
    };
    match re.kind() {
        ResolveErrorKind::Timeout => true,
        ResolveErrorKind::Io(io_err) => matches!(
            io_err.kind(),
            IOKind::TimedOut
                | IOKind::ConnectionRefused
                | IOKind::NetworkUnreachable
                | IOKind::HostUnreachable
                | IOKind::ConnectionAborted
                | IOKind::ConnectionReset
        ),
        _ => false,
    }
}


/// Resolves `domain` via the split-horizon DoT pool and returns a [`SocketAddr`]
/// for `port`.
///
/// Resolution order:
/// 1. Bare IP address → returned unchanged (no DNS query issued).
/// 2. `localhost` → `127.0.0.1:port` (no DNS query issued).
/// 3. OpenNIC TLD → query the OpenNIC pool exclusively.
/// 4. ICANN TLD → query the primary pool; on a transient failure, transparently
///    fall back to the OpenNIC pool (which also resolves ICANN upstreams).
pub async fn resolve_domain(
    resolver: &MizuDnsResolver,
    domain: &str,
    port: u16,
) -> Result<SocketAddr, MizuError> {
    let bare = domain.trim_end_matches('.');

    // ── Direct IP — skip DNS entirely ────────────────────────────────────────
    if let Ok(ip) = bare.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }

    // ── localhost shortcut — always loopback ──────────────────────────────────
    if bare.eq_ignore_ascii_case("localhost") {
        return Ok(SocketAddr::from(([127, 0, 0, 1], port)));
    }

    // Trailing dot → FQDN semantics; suppresses ndots/search-domain expansion.
    let fqdn = format!("{bare}.");

    match select_pool_for_domain(bare) {
        DnsPool::Primary => {
            // Primary pool first (Quad9/Cloudflare); on transient failure the
            // OpenNIC Tier-2 pool acts as a backup (it also resolves ICANN via
            // its upstream forwarders).
            resolve_with_pool_fallback(
                resolve_ip(resolver.primary.clone(), fqdn.clone(), port),
                resolve_ip(resolver.opennic.clone(), fqdn, port),
            )
            .await
        }
        DnsPool::OpenNic => {
            // Alternative TLDs cannot be resolved by ICANN authorities; the
            // OpenNIC pool is the sole option.
            resolve_ip(resolver.opennic.clone(), fqdn, port).await
        }
    }
}

/// Looks up `fqdn` via `resolver` and returns the best [`SocketAddr`] for `port`.
///
/// Prefers IPv4 addresses; falls back to the first IPv6 address if no IPv4
/// record is returned.
///
/// Resolution errors are propagated as [`MizuError::DnsError`] (preserving the
/// strongly-typed [`hickory_resolver::error::ResolveError`]) so that
/// [`is_transient_dns_error`] can classify them by variant rather than by
/// scraping formatted strings.
async fn resolve_ip(
    resolver: TokioAsyncResolver,
    fqdn: String,
    port: u16,
) -> Result<SocketAddr, MizuError> {
    let bare = fqdn.trim_end_matches('.').to_owned();
    let lookup = resolver
        .lookup_ip(fqdn.as_str())
        .await
        .map_err(|e| {
            tracing::debug!(domain = %bare, error = %e, "DoT lookup failed");
            MizuError::DnsError(e)
        })?;

    let mut ipv6_fallback: Option<SocketAddr> = None;
    for ip in lookup.iter() {
        let addr = SocketAddr::new(ip, port);
        if addr.is_ipv4() {
            tracing::debug!(domain = %bare, %addr, "DoT resolved (IPv4)");
            return Ok(addr);
        }
        if ipv6_fallback.is_none() {
            ipv6_fallback = Some(addr);
        }
    }

    if let Some(addr) = ipv6_fallback {
        tracing::debug!(domain = %bare, %addr, "DoT resolved (IPv6 fallback)");
        return Ok(addr);
    }

    Err(MizuError::Network(format!(
        "DoT: no address returned for '{bare}'"
    )))
}


#[cfg(test)]
mod tests {
    use super::*;

    // ── Static configuration integrity ──────────────────────────────────────

    #[test]
    fn all_primary_server_ips_are_valid() {
        for (ip_str, _, _) in PRIMARY_DOT_SERVERS {
            assert!(
                ip_str.parse::<IpAddr>().is_ok(),
                "primary server '{ip_str}' is not a valid IP address"
            );
        }
    }

    #[test]
    fn all_opennic_server_ips_are_valid() {
        for (ip_str, _, _) in OPENNIC_DOT_SERVERS {
            assert!(
                ip_str.parse::<IpAddr>().is_ok(),
                "OpenNIC server '{ip_str}' is not a valid IP address"
            );
        }
    }

    #[test]
    fn primary_pool_has_at_least_two_servers() {
        assert!(
            PRIMARY_DOT_SERVERS.len() >= 2,
            "at least 2 primary DoT servers are required for redundancy"
        );
    }

    #[test]
    fn opennic_pool_has_at_least_four_servers() {
        assert!(
            OPENNIC_DOT_SERVERS.len() >= 4,
            "at least 4 OpenNIC DoT servers are required for resilience"
        );
    }

    #[test]
    fn all_primary_servers_have_non_empty_sni() {
        for (ip, _, sni) in PRIMARY_DOT_SERVERS {
            assert!(
                !sni.is_empty(),
                "primary server {ip} has an empty SNI; certificate validation would be skipped"
            );
        }
    }

    #[test]
    fn all_opennic_servers_have_non_empty_sni() {
        for (ip, _, sni) in OPENNIC_DOT_SERVERS {
            assert!(
                !sni.is_empty(),
                "OpenNIC server {ip} has an empty SNI; certificate validation would be skipped"
            );
        }
    }

    // ── Required test: DoT-enforcement scan ─────────────────────────────────

    /// Scans every [`NameServerConfig`] in both pools and asserts that no entry
    /// uses [`Protocol::Udp`], [`Protocol::Tcp`], or port 53 (cleartext DNS).
    ///
    /// This test would catch any accidental introduction of a cleartext server
    /// into the server lists, regardless of how the list is constructed.
    #[test]
    fn test_strict_dot_enforcement() {
        let all_configs: Vec<NameServerConfig> = build_nameserver_configs(PRIMARY_DOT_SERVERS)
            .into_iter()
            .chain(build_nameserver_configs(OPENNIC_DOT_SERVERS))
            .collect();

        assert!(
            !all_configs.is_empty(),
            "resolver configuration must contain at least one server"
        );

        for cfg in &all_configs {
            assert!(
                !matches!(cfg.protocol, Protocol::Udp | Protocol::Tcp),
                "SECURITY VIOLATION: server {} uses cleartext DNS protocol {:?} \
                 — only Protocol::Tls is permitted",
                cfg.socket_addr,
                cfg.protocol
            );
            assert_eq!(
                cfg.protocol,
                Protocol::Tls,
                "server {} must use Protocol::Tls for DNS-over-TLS",
                cfg.socket_addr
            );
            assert_ne!(
                cfg.socket_addr.port(),
                53,
                "SECURITY VIOLATION: server {} is on port 53 (cleartext DNS) \
                 — DoT port 853 is required",
                cfg.socket_addr
            );
            assert!(
                cfg.tls_dns_name.is_some(),
                "server {} has no SNI hostname; TLS certificate identity cannot be verified",
                cfg.socket_addr
            );
        }
    }

    // ── Required test: split-horizon TLD routing ────────────────────────────

    /// Verifies that the TLD router sends standard ICANN domains to the primary
    /// pool and OpenNIC alternative TLDs to the OpenNIC pool.
    #[test]
    fn test_dns_routing_by_tld() {
        // Standard ICANN TLDs → primary pool
        assert_eq!(select_pool_for_domain("google.com"), DnsPool::Primary);
        assert_eq!(select_pool_for_domain("example.org"), DnsPool::Primary);
        assert_eq!(select_pool_for_domain("docs.rs"), DnsPool::Primary);
        assert_eq!(select_pool_for_domain("site.net"), DnsPool::Primary);
        assert_eq!(select_pool_for_domain("api.io"), DnsPool::Primary);

        // FQDN notation (trailing dot) must be handled correctly
        assert_eq!(select_pool_for_domain("google.com."), DnsPool::Primary);
        assert_eq!(select_pool_for_domain("chat.geek."), DnsPool::OpenNic);

        // `.mizu` is NOT an OpenNIC TLD (it does not exist in any root):
        // it must fall through to the primary pool like any unknown TLD.
        assert_eq!(select_pool_for_domain("app.mizu"), DnsPool::Primary);

        // OpenNIC TLDs → OpenNIC pool
        assert_eq!(select_pool_for_domain("site.geek"), DnsPool::OpenNic);
        assert_eq!(select_pool_for_domain("app.dyn"), DnsPool::OpenNic);
        assert_eq!(select_pool_for_domain("board.bbs"), DnsPool::OpenNic);
        assert_eq!(select_pool_for_domain("project.indy"), DnsPool::OpenNic);
        assert_eq!(select_pool_for_domain("mirror.libre"), DnsPool::OpenNic);
        assert_eq!(select_pool_for_domain("forum.pirate"), DnsPool::OpenNic);
        assert_eq!(select_pool_for_domain("net.free"), DnsPool::OpenNic);

        // TLD comparison must be case-insensitive
        assert_eq!(select_pool_for_domain("site.GEEK"), DnsPool::OpenNic);
        assert_eq!(select_pool_for_domain("forum.Pirate"), DnsPool::OpenNic);
    }

    // ── Required test: cross-pool failover ──────────────────────────────────

    /// Verifies that when the primary DoT pool returns a transient network
    /// error, [`resolve_with_pool_fallback`] transparently retries via the
    /// secondary pool and returns the secondary pool's result without
    /// propagating an error to the caller.
    ///
    /// Uses synchronous mock futures and strongly-typed `ResolveError` values
    /// to avoid requiring live DNS infrastructure and to validate the typed
    /// error-classification path (no string scraping).
    #[tokio::test]
    async fn test_dns_resolver_failover() {
        use hickory_resolver::error::{ResolveError, ResolveErrorKind};

        // Primary pool: simulate a connection timeout via the strongly-typed variant.
        let timeout_err = ResolveError::from(ResolveErrorKind::Timeout);
        let primary_fails =
            std::future::ready(Err::<SocketAddr, _>(MizuError::DnsError(timeout_err)));

        // Secondary pool: returns a canned address immediately.
        let secondary_succeeds = std::future::ready(Ok::<SocketAddr, _>(SocketAddr::from((
            [9, 9, 9, 9],
            MIZU_PORT,
        ))));

        let result = resolve_with_pool_fallback(primary_fails, secondary_succeeds).await;

        assert!(
            result.is_ok(),
            "failover must succeed transparently when primary pool times out: {result:?}"
        );
        assert_eq!(
            result.unwrap(),
            SocketAddr::from(([9, 9, 9, 9], MIZU_PORT)),
            "result must come from the secondary pool after primary failure"
        );
    }

    /// Non-transient DNS errors (NXDOMAIN, protocol messages) must NOT trigger
    /// a pool fallback — they are authoritative responses, not network failures.
    #[tokio::test]
    async fn test_nxdomain_is_not_retried() {
        use hickory_resolver::error::{ResolveError, ResolveErrorKind};

        // Use a Message error to represent a non-transient DNS failure.
        // NoRecordsFound would be more precise but requires constructing a
        // hickory_proto Query struct; Message correctly exercises the "not
        // Timeout / not Io → no retry" gate.
        let dns_err =
            ResolveError::from(ResolveErrorKind::Message("no records found (NXDOMAIN)"));
        let primary_nxdomain =
            std::future::ready(Err::<SocketAddr, _>(MizuError::DnsError(dns_err)));
        let secondary_would_succeed = std::future::ready(Ok::<SocketAddr, _>(SocketAddr::from((
            [1, 1, 1, 1],
            MIZU_PORT,
        ))));

        let result = resolve_with_pool_fallback(primary_nxdomain, secondary_would_succeed).await;

        // The error must propagate; the secondary future must NOT be awaited.
        assert!(
            result.is_err(),
            "non-transient DNS error must not trigger a pool fallback: {result:?}"
        );
    }

    // ── Task 3: is_transient_dns_error typed-matching unit tests ────────────

    /// `ResolveErrorKind::Timeout` must be classified as transient.
    #[test]
    fn is_transient_for_timeout_error() {
        use hickory_resolver::error::{ResolveError, ResolveErrorKind};
        let e = MizuError::DnsError(ResolveError::from(ResolveErrorKind::Timeout));
        assert!(
            is_transient_dns_error(&e),
            "ResolveErrorKind::Timeout must be classified as transient"
        );
    }

    /// `ResolveErrorKind::Io(ConnectionRefused)` must be classified as transient.
    #[test]
    fn is_transient_for_io_connection_refused() {
        use hickory_resolver::error::{ResolveError, ResolveErrorKind};
        let io_err = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        let e = MizuError::DnsError(ResolveError::from(ResolveErrorKind::Io(io_err)));
        assert!(
            is_transient_dns_error(&e),
            "Io(ConnectionRefused) must be classified as transient"
        );
    }

    /// `ResolveErrorKind::Message` (non-transient protocol error) must return false.
    #[test]
    fn is_not_transient_for_protocol_message_error() {
        use hickory_resolver::error::{ResolveError, ResolveErrorKind};
        let e = MizuError::DnsError(ResolveError::from(ResolveErrorKind::Message(
            "no records found",
        )));
        assert!(
            !is_transient_dns_error(&e),
            "ResolveErrorKind::Message must not be classified as transient"
        );
    }

    /// `MizuError::Network` is no longer produced by `resolve_ip` — it must
    /// never trigger the transient-error gate (validates old string-scraping is gone).
    #[test]
    fn network_variant_is_never_transient_dns() {
        let e = MizuError::Network("timed out after 4s".to_string());
        assert!(
            !is_transient_dns_error(&e),
            "MizuError::Network must not be classified as a transient DNS error \
             — only MizuError::DnsError carries typed resolver errors"
        );
    }

    // ── Resolver construction ────────────────────────────────────────────────

    /// Verifies that [`build_opennic_resolver`] constructs successfully without
    /// panicking when a Tokio runtime is active.
    #[tokio::test]
    async fn resolver_builds_without_panic() {
        let _resolver = build_opennic_resolver();
    }

    // ── Local shortcut resolution (no network required) ─────────────────────

    #[tokio::test]
    async fn bare_ip_bypasses_dns() {
        let resolver = build_opennic_resolver();
        let addr = resolve_domain(&resolver, "1.2.3.4", 7399).await.unwrap();
        assert_eq!(addr.to_string(), "1.2.3.4:7399");
    }

    #[tokio::test]
    async fn localhost_maps_to_loopback() {
        let resolver = build_opennic_resolver();
        let addr = resolve_domain(&resolver, "localhost", 7399).await.unwrap();
        assert_eq!(addr, SocketAddr::from(([127, 0, 0, 1], 7399)));
    }

    #[tokio::test]
    async fn localhost_fqdn_maps_to_loopback() {
        let resolver = build_opennic_resolver();
        let addr = resolve_domain(&resolver, "localhost.", 7399).await.unwrap();
        assert_eq!(addr, SocketAddr::from(([127, 0, 0, 1], 7399)));
    }

    #[tokio::test]
    async fn ipv6_loopback_bypasses_dns() {
        let resolver = build_opennic_resolver();
        let addr = resolve_domain(&resolver, "::1", 443).await.unwrap();
        assert_eq!(addr, SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], 443)));
    }
}
