//! Tests for the navigation module.

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
            NavigationVerdict::Block("mizu:// URL must not carry credentials or an explicit port"),
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
