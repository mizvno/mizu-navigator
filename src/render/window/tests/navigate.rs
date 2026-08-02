//! Tests for `navigate.rs`: URL resolution/sandboxing (`resolve_navigate_url`)
//! and the N3/N5 origin-safety invariants — a redirect may not manufacture
//! agency it wasn't given, and the origin of record moves only when a
//! document actually commits, never at dispatch time or from editing the URL
//! bar.

use super::*;

// --- Navigation security / URL resolution tests ----------------------------

#[test]
fn test_remote_origin_cannot_navigate_file() {
    let result = resolve_navigate_url("mizu://shop.example.com/index.mizu", "file:///etc/passwd");
    assert!(
        result.is_none(),
        "file:// navigation from mizu:// origin must be blocked"
    );
}

#[test]
fn test_unknown_scheme_origin_is_not_treated_as_remote() {
    // `http://` and `https://` are not valid Mizu schemes and are rejected
    // by navigate_to_url before they can become the current URL.
    // resolve_navigate_url therefore does NOT treat them as remote origins.
    assert!(
        resolve_navigate_url("http://example.com/page", "file:///etc/hosts").is_some(),
        "http:// is not a recognised Mizu origin — file:// block does not apply"
    );
    assert!(
        resolve_navigate_url("https://example.com/page", "file:///etc/hosts").is_some(),
        "https:// is not a recognised Mizu origin — file:// block does not apply"
    );
}

#[test]
fn test_relative_path_from_file_url() {
    let result = resolve_navigate_url("file:///home/user/app/index.mizu", "dettaglio.mizu");
    let url = result.expect("relative navigation from file:// must succeed");
    assert!(url.starts_with("file:///"), "must be a file:// URL: {url}");
    assert!(
        url.ends_with("dettaglio.mizu"),
        "must point to dettaglio.mizu: {url}"
    );
    assert!(
        url.contains("app"),
        "must be resolved into the same directory: {url}"
    );
}

#[test]
fn test_bare_url_normalised_to_mizu() {
    let result = resolve_navigate_url("mizu://origin.com/index.mizu", "other.com/page");
    let url = result.expect("bare URL navigation must succeed");
    assert!(
        url.starts_with("mizu://"),
        "bare URL must be normalised to mizu://: {url}"
    );
}

#[test]
fn test_file_origin_can_navigate_file() {
    let result = resolve_navigate_url(
        "file:///home/user/app/index.mizu",
        "file:///home/user/app/about.mizu",
    );
    assert!(
        result.is_some(),
        "file:// origin must be allowed to navigate to file:// within sandbox"
    );
    assert_eq!(result.unwrap(), "file:///home/user/app/about.mizu");
}

// --- Sandbox enforcement tests -------------------------------------------

#[test]
fn test_file_url_path_traversal_blocked() {
    // Relative ".." traversal must be blocked.
    let result = resolve_navigate_url("file:///home/user/app/index.mizu", "../../etc/passwd");
    assert!(
        result.is_none(),
        "path traversal via '..' must be blocked by sandbox, got: {result:?}"
    );

    // Absolute file:// outside the sandbox must be blocked.
    let result = resolve_navigate_url("file:///home/user/app/index.mizu", "file:///etc/passwd");
    assert!(
        result.is_none(),
        "absolute file:// outside sandbox must be blocked, got: {result:?}"
    );
}

#[test]
fn test_file_url_legitimate_relative_navigation_allowed() {
    // Same-directory relative navigation must succeed and stay in sandbox.
    let result = resolve_navigate_url("file:///home/user/app/index.mizu", "about.mizu");
    let url = result.expect("same-directory navigation must succeed");
    assert!(url.starts_with("file:///"), "must be a file:// URL: {url}");
    assert!(url.ends_with("about.mizu"), "must target about.mizu: {url}");
    assert!(
        url.contains("app"),
        "must stay inside the sandbox directory: {url}"
    );
}

// --- N3: server redirects may not manufacture user agency ---

#[test]
fn cross_origin_redirect_of_document_logic_navigation_is_blocked() {
    // Security regression: a document-logic `navigate` to its OWN origin is
    // allowed (no gesture needed), and the server answering it with
    // `Location: mizu://evil.example/` must not thereby obtain a cross-origin
    // navigation. Before the fix this site hardcoded
    // `RedirectOf(UserGesture)`, so one header cleared the N3 gate.
    let (mut manager, mut keepalive) = make_minimal_manager();
    commit_url(&mut manager, "mizu://own.example/");

    let landed = redirect_to(
        &mut manager,
        &mut keepalive,
        "mizu://evil.example/trap",
        crate::render::navigation::NavigationInitiator::DocumentLogic,
    );

    assert_eq!(
        landed, "mizu://own.example/",
        "a redirect of a document-logic navigation must not cross origin"
    );
}

#[test]
fn same_origin_redirect_of_document_logic_navigation_is_allowed() {
    // The block above must be about the origin hop, not about redirects: a
    // same-origin redirect of a logic navigation is ordinary and must work.
    let (mut manager, mut keepalive) = make_minimal_manager();
    commit_url(&mut manager, "mizu://own.example/");

    let landed = redirect_to(
        &mut manager,
        &mut keepalive,
        "mizu://own.example/next",
        crate::render::navigation::NavigationInitiator::DocumentLogic,
    );

    assert_eq!(landed, "mizu://own.example/next");
}

#[test]
fn cross_origin_redirect_of_user_gesture_navigation_is_allowed() {
    // The mirror image: real user agency still survives the redirect chain,
    // so the fix does not turn into a blanket ban on cross-origin redirects.
    let (mut manager, mut keepalive) = make_minimal_manager();
    commit_url(&mut manager, "mizu://own.example/");

    let landed = redirect_to(
        &mut manager,
        &mut keepalive,
        "mizu://other.example/page",
        crate::render::navigation::NavigationInitiator::UserGesture,
    );

    assert_eq!(landed, "mizu://other.example/page");
}

#[test]
fn redirect_chains_do_not_accumulate_agency() {
    // Hop 2 of a document-logic chain is still document logic: the initiator
    // arrives already wrapped as `RedirectOf(DocumentLogic)`, and re-wrapping
    // it must neither promote it nor nest without bound.
    let (mut manager, mut keepalive) = make_minimal_manager();
    commit_url(&mut manager, "mizu://own.example/");

    let landed = redirect_to(
        &mut manager,
        &mut keepalive,
        "mizu://evil.example/trap",
        crate::render::navigation::NavigationInitiator::RedirectOf(Box::new(
            crate::render::navigation::NavigationInitiator::DocumentLogic,
        )),
    );

    assert_eq!(
        landed, "mizu://own.example/",
        "agency must not be gained by adding redirect hops"
    );
}

// --- Bidi anti-spoofing (ux-7): programmatic chrome_state.url assignment ---

#[test]
fn navigate_to_url_strips_bidi_overrides_from_displayed_url() {
    // Security regression: a document-driven navigation (e.g. a
    // `navigate` action whose target happens to contain a bidi
    // override character) must not be able to plant one into the
    // address bar's display any more than typing one can
    // (chrome_vello.rs's insert_text is the other choke point).
    let (mut manager, mut keepalive) = make_minimal_manager();
    commit_url(&mut manager, "mizu://start.example/");

    let target = "mizu://evil\u{202E}gnp.example/";
    let landed = navigate_and_commit(
        &mut manager,
        &mut keepalive,
        target,
        crate::render::navigation::NavigationInitiator::UserGesture,
    );

    assert!(
        !manager.active().chrome_state.url.contains('\u{202E}'),
        "the displayed URL must never contain an RLO override character, got: {:?}",
        manager.active().chrome_state.url
    );
    assert_eq!(
        landed, target,
        "only the display is sanitised — the origin of record keeps the exact \
         string the document was fetched with, or origin comparisons would be \
         made against a URL nothing was ever fetched from"
    );
}

// --- N5: the origin moves with the document, not with the intent ---------

#[test]
fn a_dispatched_navigation_does_not_relabel_the_running_documents_origin() {
    // Security regression (sandbox escape / exfiltration). A `mizu://`
    // navigation is answered asynchronously and may never be answered at all,
    // while the document that requested it keeps running with its DOM, its
    // logic and its root timers intact. When the origin moved at *dispatch*
    // time, a local `file://` document could shed the file→remote call block
    // — the only thing standing between a local document and an
    // attacker-declared `media mizu://evil.example/…` endpoint — by following
    // a single link to a host that never resolves. The origin must not move
    // until a document actually commits.
    let (mut manager, mut keepalive) = make_minimal_manager();
    let local_doc = "file:///tmp/mizu-app/index.mizu";
    commit_url(&mut manager, local_doc);

    assert!(
        !resolved_call_reaches_network(&mut manager, &mut keepalive, "mizu://evil.example/collect"),
        "precondition: a file:// document may not call a remote host"
    );

    // One user gesture is enough to authorise the cross-scheme hop (N3), and
    // the target is deliberately one that will never answer.
    {
        let (t, mut c) = manager.split_active();
        navigate_to_url(
            t,
            &mut c,
            "mizu://never-resolves.invalid/".to_string(),
            crate::render::navigation::NavigationInitiator::UserGesture,
        );
    }
    assert_eq!(
        dispatched_navigation(&mut keepalive).as_deref(),
        Some("mizu://never-resolves.invalid/"),
        "the navigation must genuinely have been authorised and dispatched"
    );

    assert_eq!(
        manager.active().chrome_state.committed_url,
        local_doc,
        "the origin of record must still describe the document that is running"
    );
    assert!(
        !resolved_call_reaches_network(&mut manager, &mut keepalive, "mizu://evil.example/collect"),
        "the still-running file:// document must not gain remote-call rights \
         from a navigation that has not committed"
    );

    // The fetch then fails, so no document ever replaces the local one. The
    // tab must be exactly where it started.
    let tab_id = manager.active().id;
    {
        let (t, mut c) = manager.split_active();
        process_network_result(
            t,
            &mut c,
            crate::network::NetworkResult::Error(
                Some(tab_id),
                MizuError::Network("no such host".to_string()),
            ),
        );
    }
    assert_eq!(manager.active().chrome_state.committed_url, local_doc);
    assert!(
        !resolved_call_reaches_network(&mut manager, &mut keepalive, "mizu://evil.example/collect"),
        "a failed navigation must not leave the origin pointing at a document \
         that was never loaded"
    );
}

#[test]
fn the_url_bar_buffer_is_not_an_origin() {
    // The URL bar is an editing buffer: typing into it, pasting into it, or
    // accepting an autocomplete suggestion all rewrite it before any
    // navigation is authorised. No capability decision may read it, or a
    // local document would gain remote-call rights from keystrokes.
    let (mut manager, mut keepalive) = make_minimal_manager();
    let local_doc = "file:///tmp/mizu-app/index.mizu";
    commit_url(&mut manager, local_doc);

    manager
        .active_mut()
        .chrome_state
        .set_displayed_url("mizu://evil.example/".to_string());

    assert_eq!(
        manager.active().chrome_state.committed_url,
        local_doc,
        "editing the bar must not touch the origin of record"
    );
    assert!(
        !resolved_call_reaches_network(&mut manager, &mut keepalive, "mizu://evil.example/collect"),
        "the file:// call block must still hold while the bar shows a mizu:// URL"
    );
}

#[test]
fn a_committed_navigation_does_move_the_origin() {
    // The mirror image of the two tests above: this is a deferral, not a
    // refusal. Once a document commits, the new origin is fully in force —
    // otherwise the fix would just be a different confusion.
    let (mut manager, mut keepalive) = make_minimal_manager();
    commit_url(&mut manager, "file:///tmp/mizu-app/index.mizu");

    let landed = navigate_and_commit(
        &mut manager,
        &mut keepalive,
        "mizu://remote.example/page",
        crate::render::navigation::NavigationInitiator::UserGesture,
    );

    assert_eq!(landed, "mizu://remote.example/page");
    assert_eq!(
        manager.active().chrome_state.url,
        "mizu://remote.example/page",
        "the bar must catch up with the document at commit time"
    );
    assert!(
        resolved_call_reaches_network(&mut manager, &mut keepalive, "mizu://remote.example/api/x"),
        "a committed mizu:// document must be able to call its own origin"
    );
    assert_eq!(
        manager.active().capability_policy.quota_bytes,
        crate::render::security::STORAGE_QUOTA_BYTES_REMOTE,
        "the quota tier must be re-derived for the committed origin"
    );
}

#[test]
fn navigating_does_not_refill_an_exhausted_storage_quota() {
    // The bypass this closes: a same-origin `navigate` needs no user gesture,
    // and it rebuilds `capability_policy`. If the byte total lived on the
    // policy, a document could loop navigate → write-a-full-quota → navigate
    // and persist without bound.
    let (mut manager, mut keepalive) = make_minimal_manager();
    let origin = "mizu://greedy.example/index.mizu";
    commit_url(&mut manager, origin);

    let quota = manager.active().capability_policy.quota_bytes;
    manager
        .active_mut()
        .capability_policy
        .check_storage_write(quota)
        .expect("a write of exactly the quota must be accepted");

    // Same-origin navigation: allowed with no gesture, and committing it
    // rebuilds the policy through the choke point exactly as production does.
    navigate_and_commit(
        &mut manager,
        &mut keepalive,
        "mizu://greedy.example/again.mizu",
        crate::render::navigation::NavigationInitiator::DocumentLogic,
    );

    assert_eq!(
        manager.active_mut().capability_policy.bytes_stored(),
        quota,
        "navigation must not zero the origin's accumulated byte total"
    );
    assert!(
        manager
            .active_mut()
            .capability_policy
            .check_storage_write(1)
            .is_err(),
        "an origin at its quota must stay at its quota across a navigation"
    );
}

// Gesture agency is no longer a per-tab field, so there is no cross-tab flag
// left to assert on here: it rides on `WorkerResponse::gesture`, and a
// response is routed to the tab whose id the worker echoed back
// (`drain_logic_worker_results`), so one tab's click cannot reach another
// tab's action batch. The per-event property that replaced it — a `RootTimer`
// batch is never marked as a gesture, even immediately after a `Click` — is
// pinned in the worker itself by
// `mizu_core::parser::logic_worker::tests::gesture_is_per_event_not_ambient`.
