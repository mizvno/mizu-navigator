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
    ///
    /// Structural parsing — scheme, authority, host, userinfo, port, path,
    /// query, and fragment splitting — is fully delegated to the `url`
    /// crate's WHATWG URL Standard implementation rather than hand-rolled
    /// byte scanning. Boundary detection between those components is
    /// exactly the class of code where ad-hoc string splitting invites
    /// origin-spoofing and injection bugs (a stray `?`/`#`/`@` landing on
    /// the wrong side of a `find()` call). `mizu://`-specific policy — no
    /// credentials, no explicit port, non-empty host — is enforced
    /// afterward, on the already-validated components the parser hands
    /// back.
    ///
    /// # Errors
    ///
    /// Returns [`MizuError::Network`] if the URI is not `mizu://`-scheme,
    /// has no host, carries userinfo credentials or an explicit port, or
    /// contains a raw ASCII control character anywhere in the input.
    pub fn parse(uri: &str) -> Result<Self, MizuError> {
        // The WHATWG URL parser silently *strips* ASCII tab/CR/LF found
        // anywhere in the input before parsing even begins, and silently
        // *percent-encodes* other C0 controls (e.g. DEL) into the host
        // instead of rejecting them. Both are a sanitize-rather-than-reject
        // behaviour this runtime treats as fail-insecure elsewhere (see
        // `core::types::from_json`'s depth handling: truncating malicious
        // input is not an acceptable substitute for rejecting it). A raw
        // control byte is never legitimate in a URI — RFC 3986 requires
        // percent-encoding for any such byte — so it is rejected outright
        // before it ever reaches the parser, rather than trusting the
        // parser's silent normalisation.
        if uri.bytes().any(|b| b < 0x20 || b == 0x7f) {
            return Err(MizuError::Network(
                "mizu:// URI contains control characters".to_string(),
            ));
        }

        let parsed = url::Url::parse(uri)
            .map_err(|e| MizuError::Network(format!("invalid mizu:// URI: {e}")))?;

        if parsed.scheme() != "mizu" {
            return Err(MizuError::Network(
                "URI must use the mizu:// scheme".to_string(),
            ));
        }

        // A single-slash URI (`mizu:/host`) or a bare `mizu://` with no
        // authority parses successfully under the WHATWG grammar but with
        // no host component at all.
        let domain = parsed
            .host_str()
            .filter(|h| !h.is_empty())
            .ok_or_else(|| MizuError::Network("Empty domain in mizu:// URI".to_string()))?
            .to_string();

        // mizu:// carries no userinfo: `user[:pass]@host` is a
        // credential/origin-spoofing vector (the classic
        // `trusted.com@evil.com` phishing trick), not an authentication
        // mechanism the protocol defines. A bare `@` with an empty
        // username and no password (`mizu://@host`) is normalised away
        // entirely by the URL parser — `username()`/`password()` come
        // back empty exactly as if the `@` were never present — so there
        // is no spoofable text left and it is intentionally not rejected
        // here, unlike the old parser's blanket "any `@` char" scan.
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(MizuError::Network(
                "mizu:// domain must not contain credentials".to_string(),
            ));
        }

        // The mizu protocol uses a single implicit port; an explicit port
        // override is either a misconfigured client or an attempt to
        // redirect traffic to an attacker-controlled port.
        if parsed.port().is_some() {
            return Err(MizuError::Network(
                "mizu:// domain must not contain a port".to_string(),
            ));
        }

        // The mizu:// transport layer defines no query string or fragment
        // of its own. A query attached to an explicit path is forwarded
        // verbatim as part of the request target — mizu apps rely on this
        // to call REST-style endpoints (`/search?q=hello`) — but a query
        // with no path to attach to (`mizu://host?x`) has nothing
        // legitimate to smuggle itself into and simply collapses to the
        // document root, exactly as a fragment (`mizu://host#x`) does.
        let raw_path = parsed.path();
        let path = if raw_path.is_empty() {
            "/".to_string()
        } else {
            match parsed.query() {
                Some(q) => format!("{raw_path}?{q}"),
                None => raw_path.to_string(),
            }
        };

        Ok(Self { domain, path })
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
        assert!(MizuUri::parse("mizu://").is_err());
    }

    // m4 — strict URI validation tests

    #[test]
    fn test_at_sign_in_domain_rejected() {
        // `user[:pass]@host` syntax can be used to spoof the displayed origin.
        let cases = [
            "mizu://user@evil.com/page",
            "mizu://user:pass@evil.com/",
        ];
        for uri in cases {
            let result = MizuUri::parse(uri);
            assert!(
                matches!(result, Err(crate::core::errors::MizuError::Network(_))),
                "expected Network error for URI with credentials: {uri}"
            );
        }
    }

    #[test]
    fn test_bare_at_with_no_credentials_is_not_spoofable_and_is_accepted() {
        // `mizu://@evil.com/page` has an `@` delimiter but an empty
        // username and no password — the WHATWG URL parser normalises this
        // away entirely (there is no text before `@` to misread as a
        // trusted domain, unlike `trusted.com@evil.com`), so `username()`
        // and `password()` come back empty and there is nothing to reject.
        let uri = MizuUri::parse("mizu://@evil.com/page").unwrap();
        assert_eq!(uri.domain, "evil.com");
        assert_eq!(uri.path, "/page");
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
        // Control chars (0x00-0x1F, 0x7F) anywhere in the URI are always
        // malicious — RFC 3986 requires percent-encoding for any such
        // byte, so a raw one is never legitimate. Caught by the pre-parse
        // scan before `url::Url::parse` gets a chance to silently strip
        // (tab/CR/LF) or percent-encode (DEL, other C0 controls) them.
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
                "expected Network error for URI with control char: {uri:?}, got {result:?}"
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
        let uri = MizuUri::parse("mizu://evil.com?inject=x").unwrap();
        assert_eq!(
            uri.domain, "evil.com",
            "domain must be 'evil.com', not 'evil.com?inject=x'"
        );
    }

    #[test]
    fn test_fragment_cannot_poison_domain() {
        // `mizu://evil.com#frag` must not leak the fragment into the domain.
        let uri = MizuUri::parse("mizu://evil.com#frag").unwrap();
        assert_eq!(uri.domain, "evil.com");
    }

    #[test]
    fn test_query_before_path_does_not_reach_domain() {
        // `mizu://evil.com?q=x/page` — no explicit path exists before the
        // query, so the request target collapses to the document root.
        let uri = MizuUri::parse("mizu://evil.com?q=x/page").unwrap();
        assert_eq!(uri.domain, "evil.com");
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
        // A query attached to an explicit path is forwarded verbatim —
        // mizu apps rely on this to call REST-style endpoints.
        let uri = MizuUri::parse("mizu://api.local/search?q=hello").unwrap();
        assert_eq!(uri.domain, "api.local");
        assert_eq!(uri.path, "/search?q=hello");
    }

    #[test]
    fn test_malformed_uri_is_rejected_not_panicking() {
        assert!(MizuUri::parse("not a uri at all").is_err());
        assert!(MizuUri::parse("").is_err());
    }
}
