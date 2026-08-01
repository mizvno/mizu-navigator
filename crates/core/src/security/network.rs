//! Host classification and outbound-address authorization.
//!
//! Two questions live here, and keeping them apart is the point of the module:
//!
//! * [`is_local_host`] classifies a *spelling* — what the URL says. It gates
//!   the insecure-dev TLS bypass and the storage quota tier.
//! * [`authorize_resolved_address`] classifies a *destination* — where the
//!   socket is about to go. It is the gate that makes the first question's
//!   answer trustworthy.
//!
//! Without the second, the first is only as honest as whoever controls DNS
//! for the name: `evil.com` is textually remote, so it is denied the localhost
//! quota and the TLS bypass, and then an `A` record pointing at `127.0.0.1`
//! sends the connection to a loopback service anyway. Classifying the name is
//! not the same as constraining the connection, and only the latter can be
//! enforced at the moment it matters.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::core::errors::MizuError;

/// Returns `true` when `host` is a loopback address (`127.0.0.0/8`, `::1`) or a
/// loopback hostname (`localhost`, `*.localhost`).
///
/// Deliberately excludes RFC 1918 private ranges and `.local` (mDNS) names:
/// on a shared LAN those can be claimed or answered by other machines, so they
/// receive no special trust — neither for the insecure-dev TLS bypass, nor for
/// the file→remote SSRF block, nor for the storage quota tier.  Only traffic
/// that provably never leaves this machine is treated as local.
///
/// # Case
///
/// The name comparison is ASCII case-insensitive, and has to be. `mizu://` is
/// a non-special URL scheme, so the `url` crate treats its host as an opaque
/// host and does *not* lowercase it the way it would for `https://` —
/// `MizuUri::parse("mizu://LOCALHOST/")` yields `LOCALHOST` verbatim. A
/// case-sensitive check would call that name remote, hand it the remote
/// storage quota and withhold the TLS bypass, while the resolver sent the
/// connection to `127.0.0.1` regardless. DNS names are case-insensitive; the
/// classification of one has to be too, or the two disagree about the same
/// destination.
pub fn is_local_host(host: &str) -> bool {
    is_local_host_with(host, parse_host_literal)
}

/// ASCII case-insensitive `ends_with`, without allocating a lowercased copy of
/// `haystack`.
pub fn ends_with_ignore_ascii_case(haystack: &str, suffix: &str) -> bool {
    haystack.len() >= suffix.len()
        && haystack[haystack.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
}

/// The classification logic itself, with address parsing injected.
///
/// Split out purely so the Kani harnesses can supply a symbolic
/// `Option<IpAddr>` instead of running `<IpAddr as FromStr>::from_str` inside
/// the symbolic engine. That parser is where every previous attempt at these
/// proofs disappeared: on a 6-character symbolic host it alone emitted ~2.2M
/// symex steps and 69k verification conditions — CBMC was exploring the IPv6
/// grammar (hex groups, `::` elision, embedded IPv4 tail), which is the
/// standard library's correctness, not ours. With the parse injected, the
/// harnesses cover the branch logic that actually gates the TLS bypass and the
/// SSRF block, and the real parser is exercised by the concrete cases and the
/// unit tests below.
fn is_local_host_with(host: &str, parse: impl Fn(&str) -> Option<std::net::IpAddr>) -> bool {
    if host.eq_ignore_ascii_case("localhost") || ends_with_ignore_ascii_case(host, ".localhost") {
        return true;
    }
    if let Some(addr) = parse(host) {
        return addr.is_loopback();
    }
    false
}

/// Parses `host` as a literal IP address, accepting every spelling a URL may
/// use for one.
///
/// `url::Host::parse` first, because the URL specification's IPv4 parser
/// accepts forms `IpAddr::from_str` rejects — `2130706433`, `0x7f.0.0.1`,
/// `0177.0.0.1` are all `127.0.0.1` to a browser — and a check that missed
/// them would classify a loopback literal as a name.
pub fn parse_host_literal(host: &str) -> Option<IpAddr> {
    match url::Host::parse(host) {
        Ok(url::Host::Ipv4(v4)) => Some(IpAddr::V4(v4)),
        Ok(url::Host::Ipv6(v6)) => Some(IpAddr::V6(v6)),
        Ok(url::Host::Domain(_)) => None,
        Err(_) => host.parse::<IpAddr>().ok(),
    }
}

/// Returns `true` when `ip` is a globally-routable unicast address — one that
/// a public name is legitimately allowed to resolve to.
///
/// Everything else is a destination the machine can reach but the public
/// internet cannot address: loopback, the RFC 1918 private ranges, the
/// link-local block that carries cloud instance-metadata services
/// (`169.254.169.254`), carrier-grade NAT, the documentation and benchmarking
/// blocks, multicast, and the reserved space above `240.0.0.0`. For IPv6 the
/// same list plus unique-local and the link-local unicast block.
///
/// Embedded-IPv4 forms are unwrapped and re-checked rather than treated as
/// opaque IPv6: `::ffff:127.0.0.1`, `::127.0.0.1`, the 6to4 prefix and Teredo
/// all carry a v4 address inside a v6 one, and each is a way to spell a
/// blocked destination that a naive v6-only check would wave through.
///
/// This deliberately re-implements the classification rather than calling
/// `IpAddr::is_global`, which is still unstable.
pub fn is_publicly_routable(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_publicly_routable_v4(v4),
        IpAddr::V6(v6) => is_publicly_routable_v6(v6),
    }
}

fn is_publicly_routable_v4(v4: Ipv4Addr) -> bool {
    let [a, b, c, _] = v4.octets();
    !(v4.is_unspecified()
        || v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_multicast()
        || v4.is_broadcast()
        || v4.is_documentation()
        // 100.64.0.0/10 — carrier-grade NAT (RFC 6598).
        || (a == 100 && (64..128).contains(&b))
        // 192.0.0.0/24 — IETF protocol assignments (RFC 6890).
        || (a == 192 && b == 0 && c == 0)
        // 198.18.0.0/15 — benchmarking (RFC 2544).
        || (a == 198 && (b == 18 || b == 19))
        // 240.0.0.0/4 — reserved for future use (RFC 1112 §4).
        || a >= 240)
}

fn is_publicly_routable_v6(v6: Ipv6Addr) -> bool {
    // Unwrap any embedded IPv4 address and judge it by IPv4 rules.
    if let Some(v4) = embedded_ipv4(v6) {
        return is_publicly_routable_v4(v4);
    }

    let segments = v6.segments();
    !(v6.is_unspecified()
        || v6.is_loopback()
        || v6.is_multicast()
        // fc00::/7 — unique local addresses (RFC 4193).
        || (segments[0] & 0xfe00) == 0xfc00
        // fe80::/10 — link-local unicast (RFC 4291).
        || (segments[0] & 0xffc0) == 0xfe80
        // 2001:db8::/32 — documentation (RFC 3849).
        || (segments[0] == 0x2001 && segments[1] == 0x0db8)
        // 100::/64 — discard-only (RFC 6666).
        || (segments[0] == 0x0100 && segments[1..4] == [0, 0, 0]))
}

/// Extracts the IPv4 address embedded in `v6`, for every transition mechanism
/// that carries one.
fn embedded_ipv4(v6: Ipv6Addr) -> Option<Ipv4Addr> {
    if let Some(v4) = v6.to_ipv4_mapped() {
        return Some(v4);
    }
    let segments = v6.segments();
    // ::a.b.c.d — IPv4-compatible (deprecated, still parseable).
    //
    // `segments[6] != 0` excludes `::`, `::1` and the rest of `::/104`, which
    // are not IPv4-compatible addresses at all: `::1` is loopback, and reading
    // it as `0.0.0.1` would hand it to the IPv4 rules, which have no reason to
    // reject that. Those cases fall through to the IPv6 checks, where
    // `is_loopback` and `is_unspecified` catch them.
    if segments[..6] == [0, 0, 0, 0, 0, 0] && segments[6] != 0 {
        let [a, b] = [segments[6], segments[7]];
        return Some(Ipv4Addr::new(
            (a >> 8) as u8,
            (a & 0xff) as u8,
            (b >> 8) as u8,
            (b & 0xff) as u8,
        ));
    }
    // 2002::/16 — 6to4 carries the v4 address in segments 1-2.
    if segments[0] == 0x2002 {
        let [a, b] = [segments[1], segments[2]];
        return Some(Ipv4Addr::new(
            (a >> 8) as u8,
            (a & 0xff) as u8,
            (b >> 8) as u8,
            (b & 0xff) as u8,
        ));
    }
    // 2001:0::/32 — Teredo carries the client's v4 address, bit-inverted, in
    // the last two segments.
    if segments[0] == 0x2001 && segments[1] == 0 {
        let [a, b] = [!segments[6], !segments[7]];
        return Some(Ipv4Addr::new(
            (a >> 8) as u8,
            (a & 0xff) as u8,
            (b >> 8) as u8,
            (b & 0xff) as u8,
        ));
    }
    None
}

/// The gate every outbound connection passes: is `ip` an address that `host`
/// is allowed to have resolved to?
///
/// Three cases, and the distinction between them is the whole security
/// property:
///
/// * **`host` is a literal IP.** When `allow_private_literal` is set, the
///   destination is one the user typed or clicked into the URL bar directly —
///   there is no name-to-address indirection for anyone to lie about, so the
///   user's own choice stands, including a LAN or loopback address, which is
///   how local development works. When it is not set, the literal reached
///   this call as a *document-supplied* target (an image source, a data-call
///   URL, a subresource fetch) rather than a navigation the user drove, and
///   must clear the same public-routability bar as a resolved name — a
///   `.mizu` document embedding `mizu://169.254.169.254/…` in an `<image>` or
///   `NetworkCall` is exactly the SSRF this branch exists to block. Either
///   way, the resolved address must equal the literal; a resolver that
///   "resolves" a literal to something else is not answering the question
///   that was asked.
/// * **`host` is a loopback name** (`localhost`, `*.localhost`). RFC 6761
///   reserves these for the loopback interface, so nothing else is acceptable
///   — and these are exactly the names that receive the insecure-dev TLS
///   bypass, which must never be applied to a connection leaving the machine.
/// * **`host` is any other name.** It must resolve to a publicly-routable
///   address. This is the DNS-rebinding block: a name the rest of the system
///   has already classified as remote — and granted remote-origin treatment
///   on that basis — cannot be pointed at loopback, the LAN, or a cloud
///   metadata endpoint by whoever controls its DNS records.
///
/// # Errors
///
/// [`MizuError::SecurityViolation`] naming both the host and the address, so
/// the rejection is diagnosable without having to reproduce the DNS answer.
pub fn authorize_resolved_address(
    host: &str,
    ip: IpAddr,
    allow_private_literal: bool,
) -> Result<(), MizuError> {
    if let Some(literal) = parse_host_literal(host) {
        if literal != ip {
            return Err(MizuError::SecurityViolation(format!(
                "address literal `{host}` resolved to a different address ({ip})"
            )));
        }
        return if allow_private_literal || is_publicly_routable(ip) {
            Ok(())
        } else {
            Err(MizuError::SecurityViolation(format!(
                "address literal `{host}` is not publicly routable; a document-\
                 supplied target may not address a private, loopback, or link-\
                 local host"
            )))
        };
    }

    if is_local_host(host) {
        return if ip.is_loopback() {
            Ok(())
        } else {
            Err(MizuError::SecurityViolation(format!(
                "loopback name `{host}` resolved to non-loopback address {ip}"
            )))
        };
    }

    if is_publicly_routable(ip) {
        Ok(())
    } else {
        Err(MizuError::SecurityViolation(format!(
            "DNS rebinding blocked: public name `{host}` resolved to \
             non-routable address {ip}"
        )))
    }
}

// Kani harnesses for `is_local_host` — see `SECURITY-INVARIANTS.md` §8.
// `is_local_host` gates the insecure-dev TLS bypass, the file→remote SSRF
// block, and the storage quota tier, so a wrong classification is a direct
// security bug.
//
// Every harness drives `is_local_host_with` and supplies the parse result
// symbolically; see that function's doc comment for why the real parser is
// kept out of the symbolic engine.
#[cfg(kani)]
mod kani_proofs {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    /// Representative host shapes, covering both sides of every boundary in
    /// `is_local_host_with`: the exact name, the suffix and its near-misses, a
    /// suffix-lookalike that must *not* match (`localhost.evil.com`), the empty
    /// string, and names only the address branch could accept.
    ///
    /// The harnesses below iterate this list concretely rather than indexing
    /// into it symbolically, and that detail is the whole reason they
    /// terminate. What CBMC cannot do cheaply here is reason about a `&str`
    /// whose *pointer* is symbolic: `ends_with` then compares across an
    /// unresolved set of allocations, which is what the `same_allocation`
    /// aborts in the original log were. Measured on this crate: symbolic host
    /// pointer, no result in 25 minutes; concrete host, 0.19s. A fully
    /// symbolic `String` host is worse still and was tried first — the
    /// `String::from_utf8` unwinding loop that surfaced in the log was a
    /// symptom of that attempt, and removing it only moved the wall.
    ///
    /// So the set of host *spellings* is bounded, chosen to sit on both sides
    /// of every boundary in the function. The address half stays fully
    /// symbolic.
    const HOSTS: &[&str] = &[
        "",
        "localhost",
        "LOCALHOST",
        ".localhost",
        "a.localhost",
        "A.LocalHost",
        "a.b.localhost",
        "notlocalhost",
        "localhost.evil.com",
        "evil.com",
        "127.0.0.1",
        "192.168.1.1",
        "printer.local",
    ];

    fn any_ip() -> IpAddr {
        if kani::any() {
            IpAddr::V4(Ipv4Addr::new(
                kani::any(),
                kani::any(),
                kani::any(),
                kani::any(),
            ))
        } else {
            IpAddr::V6(Ipv6Addr::new(
                kani::any(),
                kani::any(),
                kani::any(),
                kani::any(),
                kani::any(),
                kani::any(),
                kani::any(),
                kani::any(),
            ))
        }
    }

    fn any_parse_result() -> Option<IpAddr> {
        if kani::any() { Some(any_ip()) } else { None }
    }

    #[kani::proof]
    #[kani::unwind(12)]
    fn is_local_host_never_panics() {
        for host in HOSTS {
            let _ = is_local_host_with(host, |_| any_parse_result());
        }
    }

    /// The whole contract in one statement: local exactly when the host is a
    /// `localhost` name, or the address parsed out of it is a loopback address.
    ///
    /// Stated as an equality rather than as two branch-specific harnesses
    /// guarded by `kani::assume`, so both branches are covered at once and CBMC
    /// has no assumed-away half of the space to enumerate.
    #[kani::proof]
    #[kani::unwind(12)]
    fn classification_matches_specification() {
        let parsed = any_parse_result();

        for host in HOSTS {
            let expected = host.eq_ignore_ascii_case("localhost")
                || ends_with_ignore_ascii_case(host, ".localhost")
                || parsed.is_some_and(|addr| addr.is_loopback());

            assert_eq!(is_local_host_with(host, |_| parsed), expected);
        }
    }

    /// When the hostname branch does not fire, the verdict is exactly the
    /// address's own loopback classification — no widening, no narrowing —
    /// over every one of the 2^32 + 2^128 addresses.
    #[kani::proof]
    fn parsed_address_verdict_is_exactly_is_loopback() {
        let host = "example.com";
        let addr = any_ip();
        assert_eq!(is_local_host_with(host, |_| Some(addr)), addr.is_loopback());
    }

    /// No address a public name resolves to may be one the public internet
    /// cannot address, over the whole 2^32 IPv4 space.
    ///
    /// Stated over `is_publicly_routable` directly rather than through
    /// `authorize_resolved_address`, because the host argument would have to
    /// be symbolic to say anything more, and a symbolic `&str` is what makes
    /// these harnesses stop terminating (see `HOSTS` above).
    #[kani::proof]
    fn no_reserved_ipv4_is_publicly_routable() {
        let addr = Ipv4Addr::new(kani::any(), kani::any(), kani::any(), kani::any());
        if is_publicly_routable(IpAddr::V4(addr)) {
            assert!(!addr.is_loopback());
            assert!(!addr.is_private());
            assert!(!addr.is_link_local());
            assert!(!addr.is_multicast());
            assert!(!addr.is_broadcast());
            assert!(!addr.is_documentation());
            assert!(!addr.is_unspecified());
            assert!(addr.octets()[0] < 240);
        }
    }

    /// An IPv6 address carrying a blocked IPv4 address inside it is blocked.
    /// Covers the mapped, compatible, 6to4 and Teredo encodings at once by
    /// checking that whenever an embedded address exists, the verdict is
    /// exactly that address's own verdict.
    #[kani::proof]
    fn embedded_ipv4_decides_the_ipv6_verdict() {
        let v6 = Ipv6Addr::new(
            kani::any(),
            kani::any(),
            kani::any(),
            kani::any(),
            kani::any(),
            kani::any(),
            kani::any(),
            kani::any(),
        );
        if let Some(v4) = embedded_ipv4(v6) {
            assert_eq!(
                is_publicly_routable(IpAddr::V6(v6)),
                is_publicly_routable(IpAddr::V4(v4))
            );
        }
    }

    /// A loopback name may only ever authorize a loopback address, whatever
    /// the resolver answered.
    #[kani::proof]
    #[kani::unwind(12)]
    fn loopback_names_authorize_only_loopback() {
        let addr = any_ip();
        for host in ["localhost", "LOCALHOST", "api.localhost", "API.LocalHost"] {
            assert_eq!(
                authorize_resolved_address(host, addr, false).is_ok(),
                addr.is_loopback()
            );
        }
    }

    /// End-to-end through the real parser, on the cases that matter: the
    /// harnesses above inject the parse result, so these pin the wiring.
    #[kani::proof]
    #[kani::unwind(30)]
    fn concrete_hosts_classify_correctly() {
        assert!(is_local_host("localhost"));
        assert!(is_local_host("LOCALHOST"));
        assert!(is_local_host("api.localhost"));
        assert!(is_local_host("API.LocalHost"));
        assert!(is_local_host("a.b.localhost"));
        assert!(is_local_host("127.0.0.1"));
        assert!(is_local_host("127.255.255.254"));
        assert!(is_local_host("::1"));
        assert!(is_local_host("[::1]"));
        assert!(is_local_host("2130706433"));
        assert!(is_local_host("0x7f.0.0.1"));
        assert!(is_local_host("0177.0.0.1"));

        assert!(!is_local_host("notlocalhost"));
        assert!(!is_local_host("localhost.evil.com"));
        assert!(!is_local_host("evil.com"));
        assert!(!is_local_host("192.168.1.1"));
        assert!(!is_local_host("10.0.0.1"));
        assert!(!is_local_host("printer.local"));
        assert!(!is_local_host("2001:db8::1"));
    }
}

#[cfg(all(test, not(kani)))]
mod tests {
    // The crate denies these so production paths surface their failures as
    // `MizuError`; in a test a panic on a malformed fixture *is* the report.
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("test address must parse")
    }

    #[test]
    fn loopback_and_private_space_is_not_publicly_routable() {
        for s in [
            "0.0.0.0",
            "127.0.0.1",
            "127.255.255.254",
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            // The cloud instance-metadata endpoint: the single most valuable
            // SSRF target on a hosted machine.
            "169.254.169.254",
            "100.64.0.1",
            "192.0.0.1",
            "198.18.0.1",
            "192.0.2.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "fc00::1",
            "fd00::1",
            "fe80::1",
            "ff02::1",
            "2001:db8::1",
        ] {
            assert!(
                !is_publicly_routable(ip(s)),
                "{s} must not be publicly routable"
            );
        }
    }

    #[test]
    fn ordinary_public_addresses_are_routable() {
        for s in [
            "1.1.1.1",
            "8.8.8.8",
            "9.9.9.9",
            "172.15.255.255",
            "172.32.0.1",
            "100.63.255.255",
            "100.128.0.1",
            "198.17.255.255",
            "198.20.0.1",
            "2606:4700::1111",
            "2001:4860:4860::8888",
            // 6to4 and Teredo wrapping *public* IPv4 addresses stay routable —
            // the unwrapping must not become a blanket ban on the prefixes.
            "2002:0808:0808::1",
            "2001:0:1234:5678:9abc:def0:f7f7:f7f7",
        ] {
            assert!(is_publicly_routable(ip(s)), "{s} must be publicly routable");
        }
    }

    /// Every way of spelling a blocked IPv4 address inside an IPv6 one.
    #[test]
    fn embedded_ipv4_forms_do_not_launder_blocked_addresses() {
        for s in [
            // IPv4-mapped.
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "::ffff:10.0.0.1",
            // IPv4-compatible (deprecated but still parseable).
            "::127.0.0.1",
            "::192.168.0.1",
            // 6to4: 2002:7f00:0001::/48 wraps 127.0.0.1.
            "2002:7f00:1::1",
            // Teredo: the client address is the bit-inverted final 32 bits;
            // !0x8080 == 0x7f7f -> 127.127, !0xffff == 0.0 -> 127.127.0.0.
            "2001:0:1234:5678:9abc:def0:8080:ffff",
        ] {
            assert!(
                !is_publicly_routable(ip(s)),
                "{s} embeds a blocked IPv4 address and must be rejected"
            );
        }
    }

    #[test]
    fn public_name_resolving_inward_is_rejected() {
        for s in ["127.0.0.1", "169.254.169.254", "10.1.2.3", "::1", "fd00::1"] {
            let err = authorize_resolved_address("evil.com", ip(s), true)
                .expect_err("a public name must not authorize an inward address");
            assert!(
                matches!(err, MizuError::SecurityViolation(_)),
                "expected a SecurityViolation for evil.com -> {s}, got {err:?}"
            );
        }
    }

    #[test]
    fn public_name_resolving_publicly_is_accepted() {
        assert!(authorize_resolved_address("example.com", ip("93.184.216.34"), true).is_ok());
        assert!(authorize_resolved_address("example.com", ip("2606:4700::1111"), true).is_ok());
    }

    #[test]
    fn loopback_names_require_a_loopback_address() {
        assert!(authorize_resolved_address("localhost", ip("127.0.0.1"), false).is_ok());
        assert!(authorize_resolved_address("api.localhost", ip("::1"), false).is_ok());
        assert!(authorize_resolved_address("api.localhost", ip("93.184.216.34"), false).is_err());
        assert!(authorize_resolved_address("localhost", ip("10.0.0.1"), false).is_err());
    }

    /// A literal in the URL bar is the user's own explicit choice, and every
    /// spelling of one has to be recognised as a literal — otherwise
    /// `0x7f.0.0.1` would be treated as a name and judged by the rebinding
    /// rule instead of the equality rule.
    #[test]
    fn address_literals_authorize_themselves_in_every_spelling() {
        for (host, expected) in [
            ("127.0.0.1", "127.0.0.1"),
            ("2130706433", "127.0.0.1"),
            ("0x7f.0.0.1", "127.0.0.1"),
            ("0177.0.0.1", "127.0.0.1"),
            ("[::1]", "::1"),
            ("192.168.1.5", "192.168.1.5"),
            ("93.184.216.34", "93.184.216.34"),
        ] {
            assert!(
                authorize_resolved_address(host, ip(expected), true).is_ok(),
                "literal `{host}` must authorize {expected}"
            );
        }
    }

    #[test]
    fn a_literal_may_not_resolve_to_a_different_address() {
        let err = authorize_resolved_address("127.0.0.1", ip("93.184.216.34"), true)
            .expect_err("a literal must only authorize itself");
        assert!(matches!(err, MizuError::SecurityViolation(_)));
    }

    /// SSRF regression: a document-supplied target (`allow_private_literal =
    /// false`) must not be able to self-authorize a private, loopback, or
    /// link-local literal the way a user-typed address-bar target can — this
    /// is the check that blocks `mizu://169.254.169.254/…` reaching the
    /// network from an `<image>` or `NetworkCall` embedded in a `.mizu`
    /// document.
    #[test]
    fn document_supplied_literals_must_be_publicly_routable() {
        for host in [
            "127.0.0.1",
            "192.168.1.5",
            "10.0.0.1",
            "169.254.169.254",
            "[::1]",
            "[fd00::1]",
        ] {
            let literal = host.trim_start_matches('[').trim_end_matches(']');
            let err = authorize_resolved_address(host, ip(literal), false)
                .expect_err("a document-supplied private literal must be rejected");
            assert!(matches!(err, MizuError::SecurityViolation(_)));
        }

        // A publicly routable literal is still fine without the address-bar
        // exemption — the rule only tightens the private/loopback/link-local
        // ranges, it does not ban literal IPs outright.
        assert!(authorize_resolved_address("93.184.216.34", ip("93.184.216.34"), false).is_ok());
    }

    /// `mizu://` hosts are not case-normalised by the URL parser, so the two
    /// halves of this module have to agree about `LOCALHOST` on their own.
    /// Getting this wrong is not cosmetic: the name reaches loopback either
    /// way, and a case-sensitive check would hand that connection the remote
    /// storage quota while withholding the loopback TLS treatment.
    #[test]
    fn loopback_names_are_recognised_regardless_of_case() {
        for host in [
            "localhost",
            "LOCALHOST",
            "LocalHost",
            "api.LOCALHOST",
            "API.LocalHost",
        ] {
            assert!(is_local_host(host), "{host} must classify as local");
            assert!(
                authorize_resolved_address(host, ip("127.0.0.1"), false).is_ok(),
                "{host} must authorize loopback"
            );
            assert!(
                authorize_resolved_address(host, ip("93.184.216.34"), false).is_err(),
                "{host} must not authorize a public address"
            );
        }
    }

    /// The suffix rule must not be satisfied by a name that merely *contains*
    /// `localhost`, which would otherwise hand a remote name loopback
    /// treatment.
    #[test]
    fn localhost_lookalikes_are_judged_as_public_names() {
        for host in ["localhost.evil.com", "notlocalhost", "evil-localhost.com"] {
            assert!(!is_local_host(host));
            assert!(authorize_resolved_address(host, ip("127.0.0.1"), false).is_err());
        }
    }
}
