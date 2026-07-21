    use super::*;

    /// Verifies that `INSECURE_DEV_ACTIVE` is `false` in the default (production)
    /// build.  The bypass must never be compiled in without an explicit opt-in.
    ///
    /// Gated on `not(feature = "insecure-dev")`: when the suite itself is
    /// compiled with the opt-in feature the constant is `true` by definition,
    /// so the assertion is only meaningful in the default configuration.
    #[cfg(not(feature = "insecure-dev"))]
    #[test]
    fn test_insecure_mode_disabled_by_default() {
        assert!(
            !INSECURE_DEV_ACTIVE,
            "insecure-dev must be inactive in default/production builds"
        );
    }

    /// Everything that is not provably loopback must be rejected by
    /// `is_local_host` — public hosts, but also RFC 1918 LAN addresses and
    /// `.local` mDNS names, which other machines on a shared network can claim.
    #[test]
    fn test_insecure_mode_rejected_for_public_hosts() {
        let public_hosts = [
            "example.com",
            "8.8.8.8",
            "1.1.1.1",
            "evil.localhost.example.com", // not a .localhost suffix
            "bar.local",                  // mDNS — spoofable on a shared LAN
            "192.168.0.1",                // RFC 1918 — not loopback
            "10.0.0.1",                   // RFC 1918 — not loopback
            "172.16.0.1",                 // RFC 1918 — not loopback
            "192.167.0.1",
            "172.15.255.255",
            "11.0.0.1",
        ];
        for host in public_hosts {
            assert!(
                !is_local_host(host),
                "is_local_host must return false for non-loopback host: {host}"
            );
        }
    }

    /// Only loopback addresses and `localhost` / `*.localhost` hostnames must
    /// be accepted by `is_local_host`.
    #[test]
    fn test_insecure_mode_allowed_for_loopback() {
        let local_hosts = ["localhost", "foo.localhost", "127.0.0.1", "127.1.2.3", "::1"];
        for host in local_hosts {
            assert!(
                is_local_host(host),
                "is_local_host must return true for loopback host: {host}"
            );
        }
    }
