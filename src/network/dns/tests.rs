//! Tests for the dns module.

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
    let primary_fails = std::future::ready(Err::<SocketAddr, _>(MizuError::DnsError(timeout_err)));

    // Secondary pool: returns a canned address immediately.
    let secondary_succeeds = std::future::ready(Ok::<SocketAddr, _>(SocketAddr::from((
        [9, 9, 9, 9],
        *MIZU_PORT,
    ))));

    let result = resolve_with_pool_fallback(primary_fails, secondary_succeeds).await;

    assert!(
        result.is_ok(),
        "failover must succeed transparently when primary pool times out: {result:?}"
    );
    assert_eq!(
        result.unwrap(),
        SocketAddr::from(([9, 9, 9, 9], *MIZU_PORT)),
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
    let dns_err = ResolveError::from(ResolveErrorKind::Message("no records found (NXDOMAIN)"));
    let primary_nxdomain = std::future::ready(Err::<SocketAddr, _>(MizuError::DnsError(dns_err)));
    let secondary_would_succeed = std::future::ready(Ok::<SocketAddr, _>(SocketAddr::from((
        [1, 1, 1, 1],
        *MIZU_PORT,
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

/// Verifies that [`build_dns_resolver`] constructs successfully without
/// panicking when a Tokio runtime is active.
#[tokio::test]
async fn resolver_builds_without_panic() {
    let _resolver = build_dns_resolver();
}

// ── Local shortcut resolution (no network required) ─────────────────────

#[tokio::test]
async fn bare_ip_bypasses_dns() {
    let resolver = build_dns_resolver();
    let addr = resolve_domain(&resolver, "1.2.3.4", 7399, true)
        .await
        .unwrap();
    assert_eq!(addr.get().to_string(), "1.2.3.4:7399");
}

#[tokio::test]
async fn localhost_maps_to_loopback() {
    let resolver = build_dns_resolver();
    let addr = resolve_domain(&resolver, "localhost", 7399, true)
        .await
        .unwrap();
    assert_eq!(addr.get(), SocketAddr::from(([127, 0, 0, 1], 7399)));
}

#[tokio::test]
async fn localhost_fqdn_maps_to_loopback() {
    let resolver = build_dns_resolver();
    let addr = resolve_domain(&resolver, "localhost.", 7399, true)
        .await
        .unwrap();
    assert_eq!(addr.get(), SocketAddr::from(([127, 0, 0, 1], 7399)));
}

#[tokio::test]
async fn ipv6_loopback_bypasses_dns() {
    let resolver = build_dns_resolver();
    let addr = resolve_domain(&resolver, "::1", 443, true).await.unwrap();
    assert_eq!(
        addr.get(),
        SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], 443))
    );
}

/// SSRF regression: a document-supplied target (`allow_private_literal =
/// false`, the value every subresource fetch — image/data-call — must
/// pass) must not be able to reach a private/loopback literal, even
/// though the same literal is allowed for a user-driven navigation.
#[tokio::test]
async fn document_supplied_private_literal_is_rejected() {
    let resolver = build_dns_resolver();
    let err = resolve_domain(&resolver, "127.0.0.1", 7399, false)
        .await
        .expect_err("a document-supplied loopback literal must be rejected");
    assert!(matches!(err, MizuError::SecurityViolation(_)));

    let err = resolve_domain(&resolver, "192.168.1.5", 7399, false)
        .await
        .expect_err("a document-supplied private literal must be rejected");
    assert!(matches!(err, MizuError::SecurityViolation(_)));
}

/// The `localhost` shortcut is a name, not a literal, so it is untouched
/// by `allow_private_literal` — it still maps to loopback either way
/// (`authorize_resolved_address`'s loopback-name branch enforces this,
/// not the literal branch).
#[tokio::test]
async fn document_supplied_localhost_name_still_maps_to_loopback() {
    let resolver = build_dns_resolver();
    let addr = resolve_domain(&resolver, "localhost", 7399, false)
        .await
        .unwrap();
    assert_eq!(addr.get(), SocketAddr::from(([127, 0, 0, 1], 7399)));
}
