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
        // `core::types::from_json_str`'s depth handling: truncating malicious
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

// No Kani harness for `MizuUri::parse` here (see `SECURITY-INVARIANTS.md`
// §8 for the rest of the Kani coverage in this crate): `MizuUri::parse`
// calls `url::Url::parse`, which is reachable to IDNA/Unicode-normalization
// codepaths in `idna`/`icu_normalizer` — specifically
// `icu_normalizer::Decomposition::new_with_supplements` operating over a
// `zerovec::ZeroSlice<u16>`. Kani 0.67's MIR-to-goto codegen hits an
// internal compiler error on that type (`operand.rs:351`, "entered
// unreachable code") *whenever `url::Url::parse` is reachable from any
// harness in the crate at all* — this is a static whole-function codegen
// failure, not a dynamically-reached branch, so no amount of bounding the
// symbolic input avoids it. Confirmed empirically: adding a harness that
// calls `MizuUri::parse` breaks `cargo kani` for the entire crate; removing
// it restores the other harnesses. This is an upstream Kani/zerovec
// incompatibility (https://github.com/model-checking/kani), not something
// fixable from this codebase — same category of hard tooling wall as the
// `rav1e` blocker documented on the main crate's extraction rationale.

#[cfg(test)]
mod tests;
