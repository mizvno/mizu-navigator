//! Tests for `history.rs`: Back/Forward steps must route through the
//! navigation choke point (N2) exactly like any other navigation, not swap
//! the tab's URL directly.

use super::*;

#[test]
fn history_back_step_routes_through_navigation_choke_point() {
    // Security regression: a Back step must not swap the tab's URL directly.
    // It must go through `navigate_to_url`'s Allow branch — the sole emitter
    // of `NetworkCmd::Navigate`, so the dispatched command is proof the real
    // choke point ran rather than a bypass.
    let (mut manager, mut keepalive) = make_minimal_manager();
    commit_url(&mut manager, "mizu://current.example/page");
    manager
        .active_mut()
        .history
        .record_navigation(super::super::history::HistoryEntry {
            url: "mizu://previous.example/page".to_string(),
            scroll_y: 77.0,
        });
    assert!(manager.active_mut().history.can_go_back());
    let _ = keepalive.drain_network_cmds();

    {
        let (t, mut c) = manager.split_active();
        navigate_back(t, &mut c)
    };

    assert_eq!(
        dispatched_navigation(&mut keepalive).as_deref(),
        Some("mizu://previous.example/page"),
        "Back must dispatch the popped history entry through the choke point"
    );
    assert!(
        !manager.active_mut().history.can_go_back(),
        "the popped entry must be gone from the back stack"
    );
    assert!(
        manager.active_mut().history.can_go_forward(),
        "the page left behind must now be on the forward stack"
    );
}

#[test]
fn history_forward_step_routes_through_navigation_choke_point() {
    let (mut manager, mut keepalive) = make_minimal_manager();
    commit_url(&mut manager, "mizu://current.example/page");
    manager
        .active_mut()
        .history
        .record_navigation(super::super::history::HistoryEntry {
            url: "mizu://previous.example/page".to_string(),
            scroll_y: 0.0,
        });
    {
        let (t, mut c) = manager.split_active();
        navigate_back(t, &mut c)
    };
    if let Some(url) = dispatched_navigation(&mut keepalive) {
        commit_dispatched_navigation(&mut manager, url);
    }
    assert!(manager.active_mut().history.can_go_forward());

    {
        let (t, mut c) = manager.split_active();
        navigate_forward(t, &mut c)
    };

    assert_eq!(
        dispatched_navigation(&mut keepalive).as_deref(),
        Some("mizu://current.example/page"),
        "Forward must also route through the choke point"
    );
}

#[test]
fn history_back_with_empty_stack_fires_no_navigation() {
    // Disabled-button behavior: clicking Back with an empty back stack
    // must be a guaranteed no-op, not merely "unlikely to do anything".
    let (mut manager, mut keepalive) = make_minimal_manager();
    assert!(!manager.active_mut().history.can_go_back());
    commit_url(&mut manager, "mizu://only-page.example/");
    let _ = keepalive.drain_network_cmds();

    {
        let (t, mut c) = manager.split_active();
        navigate_back(t, &mut c)
    };

    assert_eq!(
        manager.active().chrome_state.committed_url,
        "mizu://only-page.example/",
        "the origin must be unchanged when the back stack is empty"
    );
    assert!(
        dispatched_navigation(&mut keepalive).is_none(),
        "no navigation may be dispatched at all"
    );
    assert!(!manager.active_mut().history.can_go_forward());
}

#[test]
fn history_forward_with_empty_stack_fires_no_navigation() {
    let (mut manager, mut keepalive) = make_minimal_manager();
    assert!(!manager.active_mut().history.can_go_forward());
    commit_url(&mut manager, "mizu://only-page.example/");
    let _ = keepalive.drain_network_cmds();

    {
        let (t, mut c) = manager.split_active();
        navigate_forward(t, &mut c)
    };

    assert_eq!(
        manager.active().chrome_state.committed_url,
        "mizu://only-page.example/"
    );
    assert!(dispatched_navigation(&mut keepalive).is_none());
}

#[test]
fn fresh_navigation_after_back_clears_forward_stack() {
    // A -> B -> C, back to B, then a fresh navigation to D from B must
    // clear the forward stack (standard browser semantics).
    let (mut manager, mut keepalive) = make_minimal_manager();
    commit_url(&mut manager, "mizu://a.example/");
    let _ = keepalive.drain_network_cmds();
    let gesture = crate::render::navigation::NavigationInitiator::UserGesture;

    navigate_and_commit(
        &mut manager,
        &mut keepalive,
        "mizu://b.example/",
        gesture.clone(),
    );
    let landed = navigate_and_commit(
        &mut manager,
        &mut keepalive,
        "mizu://c.example/",
        gesture.clone(),
    );
    assert_eq!(landed, "mizu://c.example/");

    {
        let (t, mut c) = manager.split_active();
        navigate_back(t, &mut c)
    };
    if let Some(url) = dispatched_navigation(&mut keepalive) {
        commit_dispatched_navigation(&mut manager, url);
    }
    assert_eq!(
        manager.active().chrome_state.committed_url,
        "mizu://b.example/"
    );
    assert!(manager.active_mut().history.can_go_forward());

    let landed = navigate_and_commit(&mut manager, &mut keepalive, "mizu://d.example/", gesture);
    assert_eq!(landed, "mizu://d.example/");
    assert!(
        !manager.active_mut().history.can_go_forward(),
        "a fresh navigation must clear the forward stack"
    );
    assert!(manager.active_mut().history.can_go_back());
}
