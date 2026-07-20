//! # `navigation` — Unified Navigation Policy
//!
//! This module is the **single policy choke point** for all document-level
//! navigation in Mizu.  Every proposed navigation — address bar, link click,
//! `navigate` action from logic, redirect of a prior navigation — must pass
//! through [`check_navigation`] before any state change or network command is
//! emitted.
//!
//! ## Invariants
//!
//! These invariants are the future verification target (Kani / Creusot); the
//! function is kept pure (no I/O, no side effects) for that reason.
//!
//! - **N1 — No escalation.** A network operation whose purpose is data or media
//!   (`Fetch`, `FetchImage`, `NetworkRequest`) must never cause document
//!   navigation, under any server response.  Enforced by the callers: those
//!   paths never call [`check_navigation`].
//!
//! - **N2 — Single choke point.** Every top-level navigation passes through
//!   [`check_navigation`] before any state change or `NetworkCmd::Navigate` is
//!   emitted.
//!
//! - **N3 — Agency.** Same-origin top-level navigation is always allowed.
//!   Cross-origin top-level navigation is allowed only when the initiating
//!   cause carries a user gesture.  Logic-initiated navigation without a
//!   gesture (timer tick, network-response batch) may not leave the origin.
//!
//! - **N4 — Scheme.** Only `mizu://` is navigable over the network; `file://`
//!   only under the existing sandbox rules; `http(s)://` and everything else
//!   are refused *at this choke point*, not per call site.  `about:blank` is a
//!   no-op handled upstream.
//!
//! - **N5 — Uniform lifecycle.** Origin-scoped state (`capability_policy`
//!   reset, redirect-chain budget, `url_registry` replacement on load) is
//!   handled identically on every navigation path.  Callers must reset
//!   `capability_policy` on every `Allow` verdict.  No path may set
//!   `chrome_state.url` or emit `NetworkCmd::Navigate` around the choke point.

#![forbid(unsafe_code)]

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

/// Returns `true` when the root initiator (unwinding `RedirectOf` chains)
/// is a [`NavigationInitiator::UserGesture`].
fn has_user_agency(initiator: &NavigationInitiator) -> bool {
    match initiator {
        NavigationInitiator::UserGesture => true,
        NavigationInitiator::DocumentLogic => false,
        NavigationInitiator::RedirectOf(inner) => has_user_agency(inner),
    }
}

/// Returns `true` when `current_url` and `target` are both `mizu://` URLs
/// with the same domain (case-sensitive).
fn is_same_origin(current_url: &str, target: &str) -> bool {
    match (mizu_domain(current_url), mizu_domain(target)) {
        (Some(a), Some(b)) => a == b,
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
/// # Origin rules (N3)
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
        if mizu_domain(effective_target).is_none() {
            return NavigationVerdict::Block("mizu:// URL has an empty domain");
        }

        // N3: origin + gesture check.
        if is_same_origin(current_url, effective_target) {
            return NavigationVerdict::Allow(effective_target.to_owned());
        }

        // Cross-origin: requires user agency.
        if has_user_agency(initiator) {
            return NavigationVerdict::Allow(effective_target.to_owned());
        }

        return NavigationVerdict::Block(
            "cross-origin navigation without user gesture blocked",
        );
    }

    // Any other scheme — fail secure.
    NavigationVerdict::Block("unrecognised scheme; navigation blocked")
}

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
        let initiator = NavigationInitiator::RedirectOf(Box::new(
            NavigationInitiator::RedirectOf(Box::new(NavigationInitiator::UserGesture)),
        ));
        let v = check_navigation(
            "mizu://a.com/page",
            "mizu://c.com/page",
            &initiator,
        );
        assert!(
            matches!(v, NavigationVerdict::Allow(_)),
            "deeply nested redirect with root gesture must be allowed: {v:?}"
        );
    }

    #[test]
    fn deeply_nested_redirect_without_gesture_blocked() {
        let initiator = NavigationInitiator::RedirectOf(Box::new(
            NavigationInitiator::RedirectOf(Box::new(NavigationInitiator::DocumentLogic)),
        ));
        let v = check_navigation(
            "mizu://a.com/page",
            "mizu://c.com/page",
            &initiator,
        );
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
        assert_eq!(
            mizu_domain("mizu://example.com?q=1"),
            Some("example.com")
        );
        assert_eq!(mizu_domain("mizu:///path"), None);
        assert_eq!(mizu_domain("file:///path"), None);
    }

    #[test]
    fn same_origin_comparison() {
        assert!(is_same_origin(
            "mizu://a.com/page1",
            "mizu://a.com/page2"
        ));
        assert!(!is_same_origin(
            "mizu://a.com/page",
            "mizu://b.com/page"
        ));
        assert!(!is_same_origin("file:///path", "mizu://a.com/page"));
    }

    #[test]
    fn has_user_agency_unwraps_redirects() {
        assert!(has_user_agency(&NavigationInitiator::UserGesture));
        assert!(!has_user_agency(&NavigationInitiator::DocumentLogic));
        assert!(has_user_agency(&NavigationInitiator::RedirectOf(
            Box::new(NavigationInitiator::UserGesture)
        )));
        assert!(!has_user_agency(&NavigationInitiator::RedirectOf(
            Box::new(NavigationInitiator::DocumentLogic)
        )));
    }
}
