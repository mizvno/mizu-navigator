//! Local-host classification logic.

/// Returns `true` when `host` is a loopback address (`127.0.0.0/8`, `::1`) or a
/// loopback hostname (`localhost`, `*.localhost`).
///
/// Deliberately excludes RFC 1918 private ranges and `.local` (mDNS) names:
/// on a shared LAN those can be claimed or answered by other machines, so they
/// receive no special trust — neither for the insecure-dev TLS bypass, nor for
/// the file→remote SSRF block, nor for the storage quota tier.  Only traffic
/// that provably never leaves this machine is treated as local.
pub fn is_local_host(host: &str) -> bool {
    is_local_host_with(host, |h| h.parse::<std::net::IpAddr>().ok())
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
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    if let Some(addr) = parse(host) {
        return addr.is_loopback();
    }
    false
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
        ".localhost",
        "a.localhost",
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
            let expected = *host == "localhost"
                || host.ends_with(".localhost")
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

    /// End-to-end through the real parser, on the cases that matter: the
    /// harnesses above inject the parse result, so these pin the wiring.
    #[kani::proof]
    #[kani::unwind(20)]
    fn concrete_hosts_classify_correctly() {
        assert!(is_local_host("localhost"));
        assert!(is_local_host("api.localhost"));
        assert!(is_local_host("a.b.localhost"));
        assert!(is_local_host("127.0.0.1"));
        assert!(is_local_host("127.255.255.254"));
        assert!(is_local_host("::1"));

        assert!(!is_local_host("notlocalhost"));
        assert!(!is_local_host("localhost.evil.com"));
        assert!(!is_local_host("evil.com"));
        assert!(!is_local_host("192.168.1.1"));
        assert!(!is_local_host("10.0.0.1"));
        assert!(!is_local_host("printer.local"));
        assert!(!is_local_host("2001:db8::1"));
    }
}
