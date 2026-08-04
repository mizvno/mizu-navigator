//! DNS-over-TLS (DoT) split-horizon resolver for `mizu://`.
//!
//! # Architecture
//!
//! All DNS queries are transmitted exclusively over TLS (RFC 7858, port 853).
//! Plain UDP/TCP port-53 queries are **categorically forbidden**: every
//! [`NameServerConfig`] in every pool is built via `NameServerConfig::tls`,
//! whose single connection is DNS-over-TLS. No cleartext DNS traffic can
//! leak, preventing ISP NXDOMAIN hijacking and traffic analysis.
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
//! Each pool's [`TokioResolver`] is built with all servers for that pool.
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
//! rejected before any DNS payload is exchanged.  The per-entry SNI hostname
//! passed to `NameServerConfig::tls` is used for both the TLS `ClientHello`
//! extension and certificate identity verification.
//!
//! ## Special cases
//!
//! * Bare IP addresses → returned as-is (no DNS query issued).
//! * `localhost` and `*.localhost` → resolve to `127.0.0.1` immediately (no
//!   DNS query issued), per RFC 6761 §6.3.
//!
//! ## Address authorization
//!
//! Every address this module hands back has passed
//! [`mizu_core::security::network::authorize_resolved_address`], which is what
//! makes the rest of the stack's *textual* host classification meaningful. A
//! name that the quota tier and the TLS verifier treat as remote cannot be
//! pointed at loopback, the LAN, or a cloud metadata endpoint by whoever
//! controls its DNS records. See [`resolve_domain`].
//!
//! That guarantee is carried by the type system rather than by convention:
//! [`resolve_domain`] hands back an [`AuthorizedAddr`], whose field is private
//! to this module, and the connection pool accepts nothing else. Connecting to
//! an address that was never vetted is therefore not a mistake a reviewer has
//! to catch — it does not compile.

#![forbid(unsafe_code)]

use std::future::Future;
use std::net::{IpAddr, SocketAddr};

use hickory_resolver::{
    Resolver, TokioResolver,
    config::{NameServerConfig, ResolverConfig, ResolverOpts},
    net::runtime::TokioRuntimeProvider,
};

use crate::core::errors::MizuError;
use mizu_core::security::network::{
    authorize_resolved_address, ends_with_ignore_ascii_case, is_publicly_routable,
};

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
    "bbs", "chan", "cyb", "dyn", "epic", "free", "fur", "geek", "gopher", "indy", "libre", "neo",
    "null", "o", "oss", "oz", "parody", "pirate", "te", "uu",
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
/// use mizu::network::dns::{select_pool_for_domain, DnsPool};
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
/// Internally maintains two [`TokioResolver`] pools:
/// * `primary` — Quad9 + Cloudflare, for ICANN domains.
/// * `opennic` — OpenNIC Tier-2, for alternative TLDs.
///
/// Both pools are cheap to clone (backed by `Arc`).
/// Construct via [`build_dns_resolver`].
#[derive(Clone)]
pub struct MizuDnsResolver {
    primary: TokioResolver,
    opennic: TokioResolver,
}

/// Mizu protocol port used on every `mizu://` server.
pub static MIZU_PORT: std::sync::LazyLock<u16> =
    std::sync::LazyLock::new(|| crate::core::config::CONFIG.mizu_port);

/// Builds [`NameServerConfig`] entries for the given server list.
///
/// Every entry is built via `NameServerConfig::tls` with an explicit SNI
/// hostname, so its single connection is DNS-over-TLS. No explicit
/// `rustls::ClientConfig` is supplied to the resolver builder, so hickory's
/// default TLS config (populated from `webpki-roots`) provides certificate
/// chain validation.
///
/// Exposed as `pub(crate)` so tests can inspect the produced configs without
/// constructing a full resolver.
pub(crate) fn build_nameserver_configs(servers: &[(&str, u16, &str)]) -> Vec<NameServerConfig> {
    servers
        .iter()
        .filter_map(|(ip_str, port, sni)| {
            let ip: IpAddr = ip_str.parse().ok()?;
            let mut cfg = NameServerConfig::tls(ip, std::sync::Arc::from(*sni));
            // `NameServerConfig::tls` defaults the connection to DoT's
            // standard port 853, which is what every entry in
            // `PRIMARY_DOT_SERVERS`/`OPENNIC_DOT_SERVERS` already uses — set
            // explicitly rather than relied upon, so a future non-853 entry
            // in that data doesn't silently resolve to the wrong port.
            cfg.connections[0].port = *port;
            Some(cfg)
        })
        .collect()
}

fn build_resolver_from_pool(servers: &[(&str, u16, &str)]) -> TokioResolver {
    // `ResolverConfig` is `#[non_exhaustive]`: no struct-literal construction
    // from outside hickory-resolver, even with `..Default::default()`. Build
    // the default (empty name server list) and mutate the public field.
    let mut config = ResolverConfig::default();
    config.name_servers = build_nameserver_configs(servers);

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

    Resolver::builder_with_config(config, TokioRuntimeProvider::default())
        .with_options(opts)
        .build()
        .expect(
            "TLS config for a static, hardcoded DoT server list (see PRIMARY_DOT_SERVERS/\
             OPENNIC_DOT_SERVERS) — a build failure here is a startup-time configuration \
             bug, not a runtime network condition",
        )
}

/// Constructs the split-horizon [`MizuDnsResolver`] with both DoT pools.
///
/// The returned resolver is cheap to clone and must be called while a Tokio
/// runtime is active (required by hickory's connection manager initialisation).
pub fn build_dns_resolver() -> MizuDnsResolver {
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
/// Matches on the strongly-typed [`hickory_resolver::net::NetError`] variants
/// so the classification is immune to upstream changes in error message
/// formatting.  Only `Timeout` and `Io` errors (connection-refused, network-
/// unreachable, etc.) are transient; every other variant — including
/// `Dns` (NXDOMAIN, SERVFAIL, and other semantic DNS responses) and `Proto`
/// — is authoritative.
#[cfg(not(kani))]
fn is_transient_dns_error(e: &MizuError) -> bool {
    use hickory_resolver::net::NetError;
    use std::io::ErrorKind as IOKind;

    let MizuError::DnsError(re) = e else {
        return false;
    };
    match re.as_ref() {
        NetError::Timeout => true,
        NetError::Io(io_err) => matches!(
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

#[cfg(kani)]
fn is_transient_dns_error(_e: &MizuError) -> bool {
    false
}

/// A socket address that has passed [`authorize_resolved_address`].
///
/// The inner field is private and this module is the only place that can build
/// one, so the type is a *witness*: holding an `AuthorizedAddr` is evidence
/// that the SSRF and DNS-rebinding checks ran for that exact address.
///
/// This is the structural half of the guarantee, and it closes a gap that no
/// amount of testing can. Kani proves `authorize_resolved_address` classifies
/// addresses correctly, and property tests hammer the parsers — but neither
/// says anything about whether the check is *invoked* on the path to the
/// socket. A reviewer noticing a missing call is not a mechanism. Making
/// `get_or_connect` demand this type instead of a bare `SocketAddr` turns
/// "someone forgot to authorize" from a bug that compiles into one that does
/// not: the only way to obtain the argument is to have asked.
///
/// The remaining surface is this module. A new function *here* could construct
/// one without authorizing, so that is what to audit — one file, rather than
/// every call site in the crate.
#[derive(Debug, Clone, Copy)]
pub struct AuthorizedAddr(SocketAddr);

impl AuthorizedAddr {
    /// Unwraps the vetted address, for the one call that opens the socket.
    pub fn get(self) -> SocketAddr {
        self.0
    }

    /// Test-only constructor, for tests that need an address without standing
    /// up a resolver.
    ///
    /// Deliberately `cfg(test)`: it is exactly the bypass this type exists to
    /// prevent, so it must not exist in a shipped binary.
    #[cfg(test)]
    pub fn for_test(addr: SocketAddr) -> Self {
        Self(addr)
    }
}

/// Resolves `domain` via the split-horizon DoT pool and returns a [`SocketAddr`]
/// for `port`.
///
/// Resolution order:
/// 1. Bare IP address → returned unchanged (no DNS query issued).
/// 2. `localhost` / `*.localhost` → `127.0.0.1:port` (no DNS query issued).
/// 3. OpenNIC TLD → query the OpenNIC pool exclusively.
/// 4. ICANN TLD → query the primary pool; on a transient failure, transparently
///    fall back to the OpenNIC pool (which also resolves ICANN upstreams).
///
/// # Address authorization
///
/// This is the only function in the crate that turns a hostname into an
/// address, which makes it the only place the two can be checked against each
/// other — so every answer leaves here having passed
/// [`authorize_resolved_address`], and no caller can reach a socket address
/// that has not. Records are filtered against the policy during selection (see
/// [`resolve_ip`]) so that a name publishing both a routable and a
/// non-routable address still connects, to the routable one; the check here is
/// the enforcement point that produces the error.
///
/// `allow_private_literal` must be `true` only for a connection the user
/// actually drove (a top-level navigation) and `false` for anything a
/// document triggered on its own (a subresource fetch: a data call, an
/// image). It is forwarded verbatim to [`authorize_resolved_address`], which
/// is where the distinction is enforced — see that function's doc comment.
/// Getting this backwards turns a `.mizu` document's `<image>` or
/// `NetworkCall` target into blind SSRF against loopback/LAN/link-local
/// addresses.
pub async fn resolve_domain(
    resolver: &MizuDnsResolver,
    domain: &str,
    port: u16,
    allow_private_literal: bool,
) -> Result<AuthorizedAddr, MizuError> {
    let bare = domain.trim_end_matches('.');

    // ── Direct IP — skip DNS entirely ────────────────────────────────────────
    if let Some(ip) = mizu_core::security::network::parse_host_literal(bare) {
        authorize_resolved_address(bare, ip, allow_private_literal)?;
        return Ok(AuthorizedAddr(SocketAddr::new(ip, port)));
    }

    // ── localhost shortcut — always loopback ──────────────────────────────────
    //
    // Covers `*.localhost` too. RFC 6761 §6.3 reserves the whole subtree for
    // the loopback interface and directs resolvers not to send those names to
    // DNS at all; answering here rather than over the wire means a hostile
    // resolver never gets the chance to answer for a name that would then
    // receive loopback treatment elsewhere in the stack.
    if bare.eq_ignore_ascii_case("localhost") || ends_with_ignore_ascii_case(bare, ".localhost") {
        let ip = IpAddr::from([127, 0, 0, 1]);
        // Authorized like every other branch even though the verdict is a
        // foregone conclusion — a loopback name resolving to loopback is
        // precisely what `authorize_resolved_address` permits. Introducing
        // `AuthorizedAddr` surfaced this as the one construction site that
        // skipped the check, and an exemption is worth less than the property
        // that *every* address handed out of this module has been through it:
        // an invariant with one carve-out is one nobody can rely on without
        // first re-reading the code.
        authorize_resolved_address(bare, ip, allow_private_literal)?;
        return Ok(AuthorizedAddr(SocketAddr::new(ip, port)));
    }

    // Trailing dot → FQDN semantics; suppresses ndots/search-domain expansion.
    let fqdn = format!("{bare}.");

    let addr = match select_pool_for_domain(bare) {
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
    }?;

    authorize_resolved_address(bare, addr.ip(), allow_private_literal)?;
    Ok(AuthorizedAddr(addr))
}

/// Looks up `fqdn` via `resolver` and returns the best [`SocketAddr`] for `port`.
///
/// Prefers IPv4 addresses; falls back to the first IPv6 address if no IPv4
/// record is returned.
///
/// Records that are not publicly routable are skipped rather than rejected
/// outright, so a name that publishes one usable address alongside a
/// non-routable one still resolves. `fqdn` reaches here only for names — the
/// literal-IP and `*.localhost` cases return before this is called — so
/// "publicly routable" is the right filter for every address it sees, and
/// [`resolve_domain`] re-checks the winner.
///
/// Resolution errors are propagated as [`MizuError::DnsError`] (preserving the
/// strongly-typed [`hickory_resolver::net::NetError`]) so that
/// [`is_transient_dns_error`] can classify them by variant rather than by
/// scraping formatted strings.
async fn resolve_ip(
    resolver: TokioResolver,
    fqdn: String,
    port: u16,
) -> Result<SocketAddr, MizuError> {
    let bare = fqdn.trim_end_matches('.').to_owned();
    let lookup = resolver.lookup_ip(fqdn.as_str()).await.map_err(|e| {
        tracing::debug!(domain = %bare, error = %e, "DoT lookup failed");
        #[cfg(not(kani))]
        return MizuError::DnsError(Box::new(e));
        #[cfg(kani)]
        return MizuError::Network(e.to_string());
    })?;

    let mut ipv6_fallback: Option<SocketAddr> = None;
    let mut saw_non_routable = false;
    for ip in lookup.iter() {
        if !is_publicly_routable(ip) {
            tracing::warn!(
                domain = %bare,
                %ip,
                "DoT answer contains a non-routable address; skipping it"
            );
            saw_non_routable = true;
            continue;
        }
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

    // Distinguish "nothing came back" from "everything that came back pointed
    // inward": the second is a rebinding attempt and must not be reported as a
    // transient network failure, which would make the caller retry it against
    // the fallback pool.
    if saw_non_routable {
        return Err(MizuError::SecurityViolation(format!(
            "DNS rebinding blocked: every address returned for '{bare}' is non-routable"
        )));
    }

    Err(MizuError::Network(format!(
        "DoT: no address returned for '{bare}'"
    )))
}

#[cfg(all(test, not(kani)))]
mod tests;
