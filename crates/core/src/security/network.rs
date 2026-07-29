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
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    if let Ok(addr) = host.parse::<std::net::IpAddr>() {
        return addr.is_loopback();
    }
    false
}

// Kani harnesses for `is_local_host` — see `SECURITY-INVARIANTS.md` §8.
// `is_local_host` gates the insecure-dev TLS bypass, the file→remote SSRF
// block, and the storage quota tier, so a wrong classification is a direct
// security bug. `any_bounded_ascii_string` keeps the symbolic search space
// small (bytes restricted to ASCII graphic + `.`) so CBMC stays fast on the
// `starts_with`/`ends_with`/`IpAddr::parse` comparisons.
#[cfg(kani)]
mod kani_proofs {
    use super::*;

    fn any_bounded_ascii_string<const N: usize>() -> String {
        let len: usize = kani::any();
        kani::assume(len <= N);
        let mut bytes = [0u8; N];
        for b in bytes.iter_mut().take(len) {
            *b = kani::any();
            kani::assume(b.is_ascii_graphic());
        }
        String::from_utf8(bytes[..len].to_vec()).unwrap()
    }

    #[kani::proof]
    fn is_local_host_never_panics() {
        let host = any_bounded_ascii_string::<16>();
        let _ = is_local_host(&host);
    }

    #[kani::proof]
    fn is_local_host_localhost_suffix_is_always_local() {
        assert!(is_local_host("localhost"));
        assert!(is_local_host("api.localhost"));
        assert!(is_local_host("a.b.localhost"));
        assert!(!is_local_host("notlocalhost"));
        assert!(!is_local_host("evil.com"));
    }

    #[kani::proof]
    #[kani::unwind(10)]
    fn is_local_host_agrees_with_ip_addr_loopback() {
        let a: u8 = kani::any();
        let b: u8 = kani::any();
        let c: u8 = kani::any();
        let d: u8 = kani::any();
        let host = format!("{a}.{b}.{c}.{d}");
        // A dotted-quad of `u8` components always parses (no leading zeros,
        // since `Display` for `u8` never emits them).
        let addr: std::net::IpAddr = host.parse().unwrap();
        assert_eq!(is_local_host(&host), addr.is_loopback());
    }
}
