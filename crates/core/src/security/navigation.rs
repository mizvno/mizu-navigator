/// Who or what initiated this navigation.
///
/// Carried through the entire navigation chain so the policy can distinguish
/// user agency from document agency, even across server redirects.
#[derive(Debug, Clone)]
pub enum NavigationInitiator {
    /// The user typed in the address bar, clicked a link, or pressed a
    /// keyboard shortcut (Reload, Enter in URL bar).
    UserGesture,
    /// Document logic: a `navigate` action fired by a timer tick, a
    /// network-response batch, or a computed binding — anything that did not
    /// originate from a direct user interaction.
    DocumentLogic,
    /// A server redirect of a prior navigation.  Wraps the *original*
    /// initiator so the gesture gate (N3) can look through the redirect chain:
    /// a user-gesture navigation that redirects cross-origin is still user
    /// agency → allowed.
    RedirectOf(Box<NavigationInitiator>),
    /// A Back/Forward history step (button click or `Alt+Left`/`Alt+Right`).
    /// The user clicking Back/Forward is itself a user gesture, so this
    /// carries agency exactly like [`Self::UserGesture`] under N3 — the
    /// distinct variant exists so callers (`window::history`) can tell a
    /// history restoration apart from a fresh navigation without adding a
    /// second, ungated navigation path.
    HistoryStep,
}

impl NavigationInitiator {
    /// Wraps `self` as the initiator of a server redirect of the navigation it
    /// describes.
    ///
    /// Redirect chains are *collapsed* rather than nested: the wrapped value is
    /// always the chain's root initiator, so `RedirectOf` is never more than
    /// one level deep however many hops a server strings together. This is not
    /// a cosmetic simplification — [`has_user_agency`] answers by unwinding to
    /// the root, so collapsing preserves the verdict exactly while keeping the
    /// recursion depth constant instead of proportional to attacker-chosen hop
    /// count.
    ///
    /// The whole point of carrying the value at all is that the wrapped
    /// initiator must be the *real* one. Synthesising a
    /// [`NavigationInitiator::UserGesture`] here — because "a redirect of a
    /// navigation is probably a navigation the user asked for" — would let any
    /// server turn a document-logic, same-origin request into a
    /// gesture-authorised cross-origin navigation with a single `Location`
    /// header, which is precisely the N3 block this type exists to enforce.
    #[must_use]
    pub fn redirect_of(self) -> Self {
        match self {
            Self::RedirectOf(inner) => Self::RedirectOf(inner),
            root => Self::RedirectOf(Box::new(root)),
        }
    }
}

/// The policy verdict on a proposed navigation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavigationVerdict {
    /// Navigation is permitted; carries the resolved URL.
    Allow(String),
    /// Navigation is blocked; carries a human-readable reason.
    Block(&'static str),
}

/// Extracts the domain from a `mizu://` URL, or `None` for other schemes.
///
/// Uses the same strict boundary as `MizuUri::parse`: scans for '/', '?',
/// or '#' so query strings cannot bleed into the domain token.
fn mizu_domain(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("mizu://")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    Some(&rest[..end])
}

/// Returns `true` when `host` is a host token `mizu://` actually admits: no
/// userinfo, no explicit port, and none of the WHATWG "forbidden host code
/// points" that `url::Host` (opaque-host parsing, since `mizu` is a
/// non-special scheme) rejects.
///
/// This mirrors the policy `MizuUri::parse` enforces on the way to the socket
/// (see `core::uri`), and mirrors it *by hand* rather than by calling that
/// parser. Two reasons. `check_navigation` is documented as pure and
/// allocation-free, and is the one policy function `formal/` models; and
/// `MizuUri::parse` reaches `url::Url::parse`, whose IDNA/`zerovec` codepaths
/// break Kani's codegen for the whole crate the moment they become reachable
/// from a harness (`core::uri` documents the incompatibility). What must stay
/// true is the *agreement*: a token this function accepts must be one
/// `MizuUri::parse` would also accept, or the choke point and the fetcher
/// would disagree about what the origin is. Widening either side without the
/// other is the bug to watch for.
///
/// Rejecting here is fail-closed twice over: such a URL could never be fetched
/// anyway (`MizuUri::parse` refuses it), so the only thing the old behaviour
/// bought was the chance for a spoof-shaped host — `trusted.com@evil.com`, or
/// `trusted.com\evil.com` (a backslash reads as a path separator to some
/// consumers and as a plain host character to others) — to be carried further
/// into the browser as if it were an origin.
fn is_wellformed_mizu_host(host: &str) -> bool {
    if host.is_empty() || host.contains('@') {
        return false;
    }
    // An IPv6 literal is bracketed and is full of colons; a colon anywhere
    // else is a port. `[::1]:80` is caught by the trailing-text check.
    let unbracketed = match host.strip_prefix('[') {
        Some(rest) => match rest.strip_suffix(']') {
            Some(inner) if !inner.is_empty() => inner,
            _ => return false,
        },
        None => {
            if host.contains(':') {
                return false;
            }
            host
        }
    };
    // WHATWG forbidden host code points (opaque-host parsing), minus `:`
    // which is handled above so a bracketed IPv6 literal's internal colons
    // are not rejected here.
    !unbracketed.bytes().any(|b| {
        matches!(
            b,
            0x00..=0x20 | b'#' | b'/' | b'<' | b'>' | b'?' | b'\\' | b'^' | b'|' | 0x7f
        )
    })
}

/// Returns `true` when the root initiator (unwinding `RedirectOf` chains)
/// is a [`NavigationInitiator::UserGesture`].
fn has_user_agency(initiator: &NavigationInitiator) -> bool {
    match initiator {
        NavigationInitiator::UserGesture => true,
        NavigationInitiator::DocumentLogic => false,
        NavigationInitiator::RedirectOf(inner) => has_user_agency(inner),
        NavigationInitiator::HistoryStep => true,
    }
}

/// Returns `true` when `current_url` and `target` are both `mizu://` URLs with
/// the same domain, compared ASCII-case-insensitively.
///
/// # Case
///
/// The comparison has to be case-insensitive, and for the same reason
/// `security::network::is_local_host`'s does: `mizu://` is a non-special URL
/// scheme, so the `url` crate treats its host as an opaque host and does not
/// lowercase it. Every *other* part of this runtime already treats the two
/// spellings as one thing — DNS is case-insensitive, and
/// `ValidatedDomain::from_raw` lowercases before hashing, so `mizu://Example.com`
/// and `mizu://example.com` share one encrypted store, one keyring entry and
/// one quota ledger entry. A case-sensitive check here would call that single
/// origin two origins: same data, two navigation identities. Agreeing with the
/// storage identity is what keeps "same origin" one concept.
///
/// A malformed host (userinfo or an explicit port) is never same-origin with
/// anything, including itself — see [`is_wellformed_mizu_host`].
fn is_same_origin(current_url: &str, target: &str) -> bool {
    match (mizu_domain(current_url), mizu_domain(target)) {
        (Some(a), Some(b)) => {
            is_wellformed_mizu_host(a) && is_wellformed_mizu_host(b) && a.eq_ignore_ascii_case(b)
        }
        _ => false,
    }
}

/// Single policy entry point for all navigation decisions.
///
/// This is a **pure function** — no I/O, no side effects — so it can be
/// verified with property-based testing or formal methods.
///
/// # Scheme rules (N4)
///
/// | Target scheme | Verdict |
/// |---|---|
/// | `mizu://` | Apply origin + gesture checks (N3) |
/// | `file://` from `file://` origin | `Allow` (sandbox enforced by caller) |
/// | `file://` from `mizu://` origin | `Block` (remote → local) |
/// | `http://`, `https://` | `Block` (not a Mizu scheme) |
/// | bare hostname (no `://`) | Normalise to `mizu://` and re-check |
/// | anything else | `Block` |
///
/// # Host rules
///
/// | `mizu://` host | Verdict |
/// |---|---|
/// | empty (`mizu:///path`) | `Block` |
/// | carries userinfo or an explicit port | `Block` |
/// | anything else | Apply the origin rules below |
///
/// # Origin rules (N3)
///
/// Hosts are compared ASCII-case-insensitively, matching how the storage
/// identity and DNS already treat them — see [`is_same_origin`].
///
/// | Same origin? | User gesture? | Verdict |
/// |---|---|---|
/// | Yes | any | `Allow` |
/// | No | Yes | `Allow` |
/// | No | No | `Block` |
///
/// # File-sandbox rules
///
/// `file://` → `file://` navigation is allowed at this level; the caller
/// (`navigate_to_url`) is responsible for the sandbox containment check
/// because it requires I/O (`canonicalize`).  The policy here only blocks
/// the *scheme transition* (`mizu://` → `file://`).
///
/// # Fail-secure
///
/// Any parse ambiguity or unrecognised scheme results in `Block`.
pub fn check_navigation(
    current_url: &str,
    target: &str,
    initiator: &NavigationInitiator,
) -> NavigationVerdict {
    // Empty target is always a block.
    if target.is_empty() {
        return NavigationVerdict::Block("empty navigation target");
    }

    // --- Normalise bare hostname/path to mizu:// ---
    let normalised: String;
    let effective_target = if !target.contains("://") {
        // file:// origin with a relative path: this is a local file navigation.
        if current_url.starts_with("file://") {
            // Relative paths within file:// are allowed at the policy level;
            // sandbox enforcement is the caller's responsibility.
            return NavigationVerdict::Allow(target.to_owned());
        }
        normalised = format!("mizu://{target}");
        normalised.as_str()
    } else {
        target
    };

    // --- Scheme gate (N4) ---
    if effective_target.starts_with("http://") || effective_target.starts_with("https://") {
        return NavigationVerdict::Block("http(s):// is not a navigable Mizu scheme");
    }

    // file:// target
    if effective_target.starts_with("file://") {
        if current_url.starts_with("mizu://") {
            return NavigationVerdict::Block(
                "remote document may not navigate to file:// resource",
            );
        }
        if current_url.starts_with("file://") {
            // file→file: sandbox enforced by caller.
            return NavigationVerdict::Allow(effective_target.to_owned());
        }
        // Unknown origin scheme → file: block.
        return NavigationVerdict::Block(
            "navigation to file:// from unknown origin scheme blocked",
        );
    }

    // mizu:// target
    if effective_target.starts_with("mizu://") {
        // Validate that the domain is non-empty.
        let Some(target_host) = mizu_domain(effective_target) else {
            return NavigationVerdict::Block("mizu:// URL has an empty domain");
        };
        // …and that it is a host `mizu://` admits at all. `trusted.com@evil.com`
        // reads as `trusted.com` to a human and resolves to `evil.com`; an
        // explicit port is not part of the scheme. Neither could ever be
        // fetched (`MizuUri::parse` refuses both), so blocking here costs
        // nothing and keeps a spoofable string from being carried any further
        // into the browser as if it were an origin.
        if !is_wellformed_mizu_host(target_host) {
            return NavigationVerdict::Block(
                "mizu:// URL must not carry credentials or an explicit port",
            );
        }

        // N3: origin + gesture check.
        if is_same_origin(current_url, effective_target) {
            return NavigationVerdict::Allow(effective_target.to_owned());
        }

        // Cross-origin: requires user agency.
        if has_user_agency(initiator) {
            return NavigationVerdict::Allow(effective_target.to_owned());
        }

        return NavigationVerdict::Block("cross-origin navigation without user gesture blocked");
    }

    // Any other scheme — fail secure.
    NavigationVerdict::Block("unrecognised scheme; navigation blocked")
}

// No Kani harness here, despite `check_navigation` being the most obvious
// candidate on paper (pure, no I/O — see `SECURITY-INVARIANTS.md` §8.3/§8.4
// for the full coverage table and rationale). An attempted harness over
// bounded symbolic `current_url`/`target` strings plus a depth-capped
// `RedirectOf` chain did not converge in a usable time budget under CBMC:
// symbolic-string comparison across four scheme prefixes is exactly the
// shape bounded model checking handles worst. N2/N3/N4 are covered by the
// unit tests below and, at the design level, by `formal/`'s Lean
// development. Revisit with a different string-modelling strategy.

#[cfg(test)]
mod tests {
    use super::*;

    // --- N3: Agency tests ---

    #[test]
    fn navigation_redirect_same_origin_allowed() {
        let v = check_navigation(
            "mizu://shop.example.com/index.mizu",
            "mizu://shop.example.com/other.mizu",
            &NavigationInitiator::RedirectOf(Box::new(NavigationInitiator::DocumentLogic)),
        );
        assert!(
            matches!(v, NavigationVerdict::Allow(_)),
            "same-origin redirect must be allowed: {v:?}"
        );
    }

    #[test]
    fn navigation_redirect_cross_origin_with_gesture_allowed() {
        let v = check_navigation(
            "mizu://shop.example.com/index.mizu",
            "mizu://other.example.com/page.mizu",
            &NavigationInitiator::RedirectOf(Box::new(NavigationInitiator::UserGesture)),
        );
        assert!(
            matches!(v, NavigationVerdict::Allow(_)),
            "cross-origin redirect with user gesture must be allowed: {v:?}"
        );
    }

    #[test]
    fn navigation_redirect_cross_origin_without_gesture_blocked() {
        let v = check_navigation(
            "mizu://shop.example.com/index.mizu",
            "mizu://evil.example.com/trap.mizu",
            &NavigationInitiator::RedirectOf(Box::new(NavigationInitiator::DocumentLogic)),
        );
        assert_eq!(
            v,
            NavigationVerdict::Block("cross-origin navigation without user gesture blocked"),
            "cross-origin redirect without gesture must be blocked"
        );
    }

    #[test]
    fn logic_navigate_cross_origin_without_gesture_blocked() {
        let v = check_navigation(
            "mizu://mysite.com/index.mizu",
            "mizu://evil.com/phish.mizu",
            &NavigationInitiator::DocumentLogic,
        );
        assert_eq!(
            v,
            NavigationVerdict::Block("cross-origin navigation without user gesture blocked"),
        );
    }

    #[test]
    fn logic_navigate_same_origin_without_gesture_allowed() {
        let v = check_navigation(
            "mizu://mysite.com/index.mizu",
            "mizu://mysite.com/details.mizu",
            &NavigationInitiator::DocumentLogic,
        );
        assert!(
            matches!(v, NavigationVerdict::Allow(ref u) if u == "mizu://mysite.com/details.mizu"),
            "same-origin logic navigation must be allowed: {v:?}"
        );
    }

    #[test]
    fn user_gesture_cross_origin_allowed() {
        let v = check_navigation(
            "mizu://a.com/page",
            "mizu://b.com/page",
            &NavigationInitiator::UserGesture,
        );
        assert!(
            matches!(v, NavigationVerdict::Allow(_)),
            "user-gesture cross-origin must be allowed: {v:?}"
        );
    }

    // --- N4: Scheme tests ---

    #[test]
    fn redirect_to_http_scheme_blocked() {
        let v = check_navigation(
            "mizu://origin.com/page",
            "http://evil.com/trap",
            &NavigationInitiator::UserGesture,
        );
        assert_eq!(
            v,
            NavigationVerdict::Block("http(s):// is not a navigable Mizu scheme"),
        );
    }

    #[test]
    fn redirect_to_https_scheme_blocked() {
        let v = check_navigation(
            "mizu://origin.com/page",
            "https://evil.com/trap",
            &NavigationInitiator::UserGesture,
        );
        assert_eq!(
            v,
            NavigationVerdict::Block("http(s):// is not a navigable Mizu scheme"),
        );
    }

    #[test]
    fn redirect_to_file_from_remote_blocked() {
        let v = check_navigation(
            "mizu://evil.com/page",
            "file:///etc/passwd",
            &NavigationInitiator::RedirectOf(Box::new(NavigationInitiator::UserGesture)),
        );
        assert_eq!(
            v,
            NavigationVerdict::Block("remote document may not navigate to file:// resource"),
        );
    }

    #[test]
    fn file_to_file_navigation_allowed_at_policy_level() {
        let v = check_navigation(
            "file:///home/user/app/index.mizu",
            "file:///home/user/app/about.mizu",
            &NavigationInitiator::UserGesture,
        );
        assert!(
            matches!(v, NavigationVerdict::Allow(_)),
            "file→file allowed at policy (sandbox enforced by caller): {v:?}"
        );
    }

    #[test]
    fn bare_hostname_normalised_to_mizu() {
        let v = check_navigation(
            "mizu://origin.com/page",
            "other.com/page",
            &NavigationInitiator::UserGesture,
        );
        match v {
            NavigationVerdict::Allow(url) => {
                assert!(
                    url.starts_with("mizu://"),
                    "bare hostname must be normalised to mizu://: {url}"
                );
            }
            _ => panic!("bare hostname navigation must be allowed with gesture: {v:?}"),
        }
    }

    #[test]
    fn bare_hostname_cross_origin_without_gesture_blocked() {
        let v = check_navigation(
            "mizu://origin.com/page",
            "other.com/page",
            &NavigationInitiator::DocumentLogic,
        );
        assert_eq!(
            v,
            NavigationVerdict::Block("cross-origin navigation without user gesture blocked"),
        );
    }

    #[test]
    fn empty_target_blocked() {
        let v = check_navigation(
            "mizu://origin.com/page",
            "",
            &NavigationInitiator::UserGesture,
        );
        assert_eq!(v, NavigationVerdict::Block("empty navigation target"));
    }

    #[test]
    fn unknown_scheme_blocked() {
        let v = check_navigation(
            "mizu://origin.com/page",
            "ftp://files.com/data",
            &NavigationInitiator::UserGesture,
        );
        assert_eq!(
            v,
            NavigationVerdict::Block("unrecognised scheme; navigation blocked"),
        );
    }

    #[test]
    fn mizu_empty_domain_blocked() {
        let v = check_navigation(
            "mizu://origin.com/page",
            "mizu:///path",
            &NavigationInitiator::UserGesture,
        );
        assert_eq!(
            v,
            NavigationVerdict::Block("mizu:// URL has an empty domain"),
        );
    }

    #[test]
    fn relative_path_from_file_origin_allowed() {
        let v = check_navigation(
            "file:///home/user/app/index.mizu",
            "details.mizu",
            &NavigationInitiator::UserGesture,
        );
        assert!(
            matches!(v, NavigationVerdict::Allow(ref u) if u == "details.mizu"),
            "relative path from file:// must be allowed (sandbox enforced by caller): {v:?}"
        );
    }

    #[test]
    fn deeply_nested_redirect_preserves_gesture() {
        // UserGesture → Redirect → Redirect → still user agency
        let initiator = NavigationInitiator::RedirectOf(Box::new(NavigationInitiator::RedirectOf(
            Box::new(NavigationInitiator::UserGesture),
        )));
        let v = check_navigation("mizu://a.com/page", "mizu://c.com/page", &initiator);
        assert!(
            matches!(v, NavigationVerdict::Allow(_)),
            "deeply nested redirect with root gesture must be allowed: {v:?}"
        );
    }

    #[test]
    fn deeply_nested_redirect_without_gesture_blocked() {
        let initiator = NavigationInitiator::RedirectOf(Box::new(NavigationInitiator::RedirectOf(
            Box::new(NavigationInitiator::DocumentLogic),
        )));
        let v = check_navigation("mizu://a.com/page", "mizu://c.com/page", &initiator);
        assert_eq!(
            v,
            NavigationVerdict::Block("cross-origin navigation without user gesture blocked"),
        );
    }

    #[test]
    fn file_to_file_relative_from_document_logic_allowed() {
        // file:// origins don't have cross-origin concerns — they're all local.
        let v = check_navigation(
            "file:///home/user/app/index.mizu",
            "other.mizu",
            &NavigationInitiator::DocumentLogic,
        );
        assert!(
            matches!(v, NavigationVerdict::Allow(_)),
            "file-origin relative navigation is always allowed: {v:?}"
        );
    }

    // --- Helpers ---

    #[test]
    fn mizu_domain_extracts_host() {
        assert_eq!(mizu_domain("mizu://example.com/path"), Some("example.com"));
        assert_eq!(mizu_domain("mizu://example.com"), Some("example.com"));
        assert_eq!(mizu_domain("mizu://example.com?q=1"), Some("example.com"));
        assert_eq!(mizu_domain("mizu:///path"), None);
        assert_eq!(mizu_domain("file:///path"), None);
    }

    #[test]
    fn same_origin_comparison() {
        assert!(is_same_origin("mizu://a.com/page1", "mizu://a.com/page2"));
        assert!(!is_same_origin("mizu://a.com/page", "mizu://b.com/page"));
        assert!(!is_same_origin("file:///path", "mizu://a.com/page"));
    }

    /// The two spellings share an encrypted store, a keyring entry and a quota
    /// ledger entry (`ValidatedDomain::from_raw` lowercases), and DNS sends
    /// both to the same server. They must therefore be one navigation origin
    /// too — otherwise the same data has two identities.
    #[test]
    fn origin_comparison_is_case_insensitive() {
        assert!(is_same_origin(
            "mizu://Example.com/a",
            "mizu://example.com/b"
        ));
        assert!(is_same_origin(
            "mizu://EXAMPLE.COM/a",
            "mizu://example.com/b"
        ));
        assert!(is_same_origin(
            "mizu://a.EXAMPLE.com/",
            "mizu://A.example.COM/"
        ));
        // Case-folding must not reach past the host token.
        assert!(!is_same_origin(
            "mizu://example.com/a",
            "mizu://exemple.com/a"
        ));
    }

    #[test]
    fn a_differently_cased_same_origin_navigation_needs_no_gesture() {
        // The behaviour the case rule exists for: document logic reaching its
        // own origin, spelled differently, is not a cross-origin hop.
        let v = check_navigation(
            "mizu://Example.com/index.mizu",
            "mizu://example.com/next.mizu",
            &NavigationInitiator::DocumentLogic,
        );
        assert!(
            matches!(v, NavigationVerdict::Allow(_)),
            "a differently-cased same-origin navigation must be allowed: {v:?}"
        );
    }

    /// `trusted.com@evil.com` reads as `trusted.com` and resolves to
    /// `evil.com`. `MizuUri::parse` refuses it on the way to the socket, so it
    /// could never be fetched — but it must not get as far as being treated
    /// as an origin, or committed into the address bar, either.
    #[test]
    fn hosts_with_credentials_or_a_port_are_blocked() {
        for target in [
            "mizu://trusted.com@evil.com/page",
            "mizu://user:pass@evil.com/page",
            "mizu://evil.com:8080/page",
            "mizu://[::1]:8080/page",
        ] {
            assert_eq!(
                check_navigation(
                    "mizu://a.com/page",
                    target,
                    &NavigationInitiator::UserGesture
                ),
                NavigationVerdict::Block(
                    "mizu:// URL must not carry credentials or an explicit port"
                ),
                "{target} must be blocked at the choke point"
            );
        }
    }

    #[test]
    fn a_malformed_host_is_not_same_origin_with_itself() {
        // Fail-closed: an unparseable origin must not become a way to reach
        // itself without a gesture.
        assert!(!is_same_origin(
            "mizu://a.com@evil.com/one",
            "mizu://a.com@evil.com/two"
        ));
    }

    #[test]
    fn ordinary_hosts_including_ipv6_literals_stay_wellformed() {
        // Regression guard: the port check must not reject the colons inside a
        // bracketed IPv6 literal, which is a legitimate `mizu://` host.
        for host in [
            "example.com",
            "a.b.c.opennic",
            "127.0.0.1",
            "[::1]",
            "[fe80::1]",
        ] {
            assert!(is_wellformed_mizu_host(host), "{host} must be accepted");
        }
        for host in ["u@h", "h:1", "[::1]:1", "[]", "[::1"] {
            assert!(!is_wellformed_mizu_host(host), "{host} must be rejected");
        }
    }

    /// Regression guard for the agreement with `MizuUri::parse`: a host
    /// carrying a WHATWG forbidden host code point must be rejected here too,
    /// not just by the fetcher, or a spoof-shaped string can be committed as
    /// an origin that could never actually be fetched.
    #[test]
    fn hosts_with_forbidden_code_points_are_rejected() {
        for host in [
            "trusted.com\\evil.com",
            "trusted.com/evil.com",
            "trusted.com#evil.com",
            "trusted.com?evil.com",
            "trusted.com evil.com",
            "trusted.com<evil.com",
            "trusted.com>evil.com",
            "trusted.com^evil.com",
            "trusted.com|evil.com",
            "trusted.com\tevil.com",
            "trusted.com\nevil.com",
            "",
        ] {
            assert!(!is_wellformed_mizu_host(host), "{host:?} must be rejected");
        }
    }

    #[test]
    fn a_host_with_a_forbidden_code_point_is_blocked_at_check_navigation() {
        let v = check_navigation(
            "mizu://a.com/page",
            "mizu://trusted.com\\evil.com/page",
            &NavigationInitiator::UserGesture,
        );
        assert_eq!(
            v,
            NavigationVerdict::Block("mizu:// URL must not carry credentials or an explicit port"),
        );
    }

    // --- N3: `redirect_of` preserves agency and collapses chains ---

    #[test]
    fn redirect_of_preserves_the_root_verdict() {
        for root in [
            NavigationInitiator::UserGesture,
            NavigationInitiator::DocumentLogic,
            NavigationInitiator::HistoryStep,
        ] {
            let expected = has_user_agency(&root);
            assert_eq!(
                has_user_agency(&root.clone().redirect_of()),
                expected,
                "wrapping must not change the verdict for {root:?}"
            );
        }
    }

    #[test]
    fn redirect_of_never_nests_more_than_one_level() {
        // Hop count is attacker-chosen (a server can emit a `Location` on
        // every response up to the redirect budget), so the wrapper must not
        // grow one `Box` per hop — `has_user_agency` recurses through it.
        let mut initiator = NavigationInitiator::DocumentLogic;
        for _ in 0..64 {
            initiator = initiator.redirect_of();
        }
        match &initiator {
            NavigationInitiator::RedirectOf(inner) => assert!(
                matches!(**inner, NavigationInitiator::DocumentLogic),
                "the wrapped value must stay the chain's root, got {inner:?}"
            ),
            other => panic!("expected RedirectOf, got {other:?}"),
        }
        assert!(!has_user_agency(&initiator));
    }

    #[test]
    fn redirect_of_a_gesture_chain_keeps_agency() {
        let mut initiator = NavigationInitiator::UserGesture;
        for _ in 0..64 {
            initiator = initiator.redirect_of();
        }
        assert!(has_user_agency(&initiator));
        assert!(matches!(
            check_navigation("mizu://a.com/p", "mizu://b.com/p", &initiator),
            NavigationVerdict::Allow(_)
        ));
    }

    #[test]
    fn has_user_agency_unwraps_redirects() {
        assert!(has_user_agency(&NavigationInitiator::UserGesture));
        assert!(!has_user_agency(&NavigationInitiator::DocumentLogic));
        assert!(has_user_agency(&NavigationInitiator::RedirectOf(Box::new(
            NavigationInitiator::UserGesture
        ))));
        assert!(!has_user_agency(&NavigationInitiator::RedirectOf(
            Box::new(NavigationInitiator::DocumentLogic)
        )));
    }

    // --- ux-4: history steps carry agency (N3) ---

    #[test]
    fn history_step_has_user_agency() {
        // A Back/Forward click is itself a user gesture (ux-4).
        assert!(has_user_agency(&NavigationInitiator::HistoryStep));
    }

    #[test]
    fn history_step_cross_origin_allowed() {
        let v = check_navigation(
            "mizu://a.com/page",
            "mizu://b.com/page",
            &NavigationInitiator::HistoryStep,
        );
        assert!(
            matches!(v, NavigationVerdict::Allow(_)),
            "a history step must carry the same cross-origin agency as UserGesture: {v:?}"
        );
    }
}
