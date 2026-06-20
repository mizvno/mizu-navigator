use crate::core::errors::MizuError;

/// Represents a parsed `mizu://` URI.
#[derive(Debug, Clone, PartialEq)]
pub struct MizuUri {
    /// The domain name extracted from the URI.
    pub domain: String,
    /// The path segment extracted from the URI.
    pub path: String,
}

impl MizuUri {
    /// Parses a URI string, expecting the `mizu://` scheme.
    pub fn parse(uri: &str) -> Result<Self, MizuError> {
        if let Some(rest) = uri.strip_prefix("mizu://") {
            // Locate the first character that ends the host component.
            // Without this, `mizu://evil.com?q=x` stores `"evil.com?q=x"` as the
            // domain, which bypasses all downstream validation and allows an attacker
            // to smuggle query/fragment data into the domain field (origin spoofing).
            let host_end = rest
                .find(['/', '?', '#'])
                .unwrap_or(rest.len());
            let domain = rest[..host_end].to_string();
            let path_rest = &rest[host_end..];

            if domain.is_empty() {
                return Err(MizuError::Network(
                    "Empty domain in mizu:// URI".to_string(),
                ));
            }
            // Belt-and-suspenders: ensure no query/fragment delimiters leaked into
            // the domain string despite the host_end scan above.
            if domain.contains('?') || domain.contains('#') {
                return Err(MizuError::Network(
                    "mizu:// domain must not contain '?' or '#'".to_string(),
                ));
            }
            // Reject credential-spoofing attempts: `@` embeds userinfo in the host.
            if domain.contains('@') {
                return Err(MizuError::Network(
                    "mizu:// domain must not contain '@' (credential-spoofing attempt rejected)"
                        .to_string(),
                ));
            }
            // The mizu protocol uses a fixed port (MIZU_PORT); explicit port overrides
            // are not part of the spec and indicate either a misconfigured client or an
            // attempt to redirect traffic to an attacker-controlled port.
            if domain.contains(':') {
                return Err(MizuError::Network(
                    "mizu:// domain must not contain ':' (port specifications are not allowed)"
                        .to_string(),
                ));
            }
            // ASCII control characters (U+0000–U+001F and U+007F) have no legitimate
            // use in a domain name and can be used to smuggle hidden data or confuse
            // downstream parsers.
            if domain.bytes().any(|b| b < 0x20 || b == 0x7f) {
                return Err(MizuError::Network(
                    "mizu:// domain contains control characters".to_string(),
                ));
            }
            // Only paths (starting with '/') carry through; '?' and '#' components
            // that immediately follow the host are discarded — the mizu protocol
            // does not define query strings or fragment identifiers at the transport
            // layer, so they have no legitimate use and may only indicate injection.
            let path = if path_rest.starts_with('/') {
                path_rest.to_string()
            } else {
                "/".to_string()
            };
            Ok(Self { domain, path })
        } else {
            Err(MizuError::Network(
                "URI must use the mizu:// scheme".to_string(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_mizu_uri() {
        let uri = MizuUri::parse("mizu://api.example.com/data").unwrap();
        assert_eq!(uri.domain, "api.example.com");
        assert_eq!(uri.path, "/data");

        let uri_root = MizuUri::parse("mizu://api.example.com").unwrap();
        assert_eq!(uri_root.domain, "api.example.com");
        assert_eq!(uri_root.path, "/");
    }

    #[test]
    fn test_invalid_scheme() {
        assert!(MizuUri::parse("https://api.example.com").is_err());
        assert!(MizuUri::parse("mizu:/api.example.com").is_err());
    }

    #[test]
    fn test_empty_domain() {
        assert!(MizuUri::parse("mizu:///data").is_err());
    }

    // m4 — strict URI validation tests

    #[test]
    fn test_at_sign_in_domain_rejected() {
        // `user@host` syntax can be used to spoof the displayed origin.
        let cases = [
            "mizu://user@evil.com/page",
            "mizu://@evil.com/page",
            "mizu://user:pass@evil.com/",
        ];
        for uri in cases {
            let result = MizuUri::parse(uri);
            assert!(
                matches!(result, Err(crate::core::errors::MizuError::Network(_))),
                "expected Network error for URI with '@': {uri}"
            );
        }
    }

    #[test]
    fn test_port_in_domain_rejected() {
        // Explicit port overrides are not part of the mizu:// spec.
        let cases = [
            "mizu://evil.com:8080/page",
            "mizu://localhost:1234/",
            "mizu://10.0.0.1:9999/data",
        ];
        for uri in cases {
            let result = MizuUri::parse(uri);
            assert!(
                matches!(result, Err(crate::core::errors::MizuError::Network(_))),
                "expected Network error for URI with port: {uri}"
            );
        }
    }

    #[test]
    fn test_control_characters_in_domain_rejected() {
        // Control chars (0x00–0x1F, 0x7F) in a domain name are always malicious.
        let cases = [
            "mizu://evil\x00.com/",
            "mizu://evil\r\n.com/",
            "mizu://evil\x1b[.com/",
            "mizu://evil\x7f.com/",
        ];
        for uri in cases {
            let result = MizuUri::parse(uri);
            assert!(
                matches!(result, Err(crate::core::errors::MizuError::Network(_))),
                "expected Network error for URI with control char"
            );
        }
    }

    #[test]
    fn test_valid_domain_still_accepted() {
        // Regression guard: legitimate domains must not be broken by the new checks.
        let uri = MizuUri::parse("mizu://example.opennic/path/to/page").unwrap();
        assert_eq!(uri.domain, "example.opennic");
        assert_eq!(uri.path, "/path/to/page");
    }

    // TASK 2 — query/fragment injection regression tests

    #[test]
    fn test_query_string_cannot_poison_domain() {
        // `mizu://evil.com?inject=x` must NOT store "evil.com?inject=x" as the domain.
        // The host boundary scan must terminate at '?'.
        let uri = MizuUri::parse("mizu://evil.com?inject=x").unwrap();
        assert_eq!(
            uri.domain, "evil.com",
            "domain must be 'evil.com', not 'evil.com?inject=x'"
        );
    }

    #[test]
    fn test_fragment_cannot_poison_domain() {
        // `mizu://evil.com#frag` must terminate the host at '#'.
        let uri = MizuUri::parse("mizu://evil.com#frag").unwrap();
        assert_eq!(uri.domain, "evil.com");
    }

    #[test]
    fn test_query_before_path_does_not_reach_domain() {
        // `mizu://evil.com?q=x/page` — the '?' fires before '/', so domain='evil.com'.
        let uri = MizuUri::parse("mizu://evil.com?q=x/page").unwrap();
        assert_eq!(uri.domain, "evil.com");
        // Path is '/' because path_rest starts with '?', not '/'.
        assert_eq!(uri.path, "/");
    }

    #[test]
    fn test_normal_path_after_host_preserved() {
        // Regression: a normal path after a clean host must still work.
        let uri = MizuUri::parse("mizu://data.local/api/v1/items").unwrap();
        assert_eq!(uri.domain, "data.local");
        assert_eq!(uri.path, "/api/v1/items");
    }

    #[test]
    fn test_query_inside_path_segment_allowed() {
        // If the path itself contains '?', that is in path_rest (after the first '/'),
        // so it is preserved in the path and never touches the domain.
        let uri = MizuUri::parse("mizu://api.local/search?q=hello").unwrap();
        assert_eq!(uri.domain, "api.local");
        assert_eq!(uri.path, "/search?q=hello");
    }
}
