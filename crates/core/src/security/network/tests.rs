//! Tests for the network module.

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
