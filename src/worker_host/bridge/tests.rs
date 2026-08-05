//! Tests for the async bridge.
//!
//! These drive real sandboxed worker processes through the *exact* channel
//! shape the window manager uses, so what is exercised here is the swap
//! itself, not a stand-in for it.

use std::collections::HashMap;

use mizu_core::core::types::{StringInterner, Value};
use mizu_core::messages::{ReloadPayload, RuntimeAction, TabId, UiEvent};
use mizu_core::parser::Action;
use mizu_core::parser::logic::{Expr, ExprArena, ExprTree};
use mizu_core::parser::UrlRegistry;

use super::*;

/// Locates `mizu-worker` from a test binary in `target/debug/deps/`.
fn test_worker_exe() -> std::path::PathBuf {
    let mut dir = std::env::current_exe().expect("current_exe");
    dir.pop();
    if dir.ends_with("deps") {
        dir.pop();
    }
    dir.join(format!("mizu-worker{}", std::env::consts::EXE_SUFFIX))
}

/// A document whose node 0 navigates on click.
fn navigate_doc() -> ReloadPayload {
    let mut arena = ExprArena::new();
    let root = arena.alloc(Expr::Literal(Value::from("mizu://example.com/next")));
    let mut click_actions = HashMap::new();
    click_actions.insert(
        0u32,
        Action::Navigate {
            url: ExprTree { arena, root },
        },
    );
    let mut interner = StringInterner::new();
    let interner = interner.freeze();
    ReloadPayload {
        logic_fns: Default::default(),
        click_actions,
        submit_actions: HashMap::new(),
        root_timer_actions: Vec::new(),
        interner,
        initial_variables: Vec::new(),
        url_registry: UrlRegistry::default(),
        document_domain: "example.com".to_string(),
        computed_bindings: Vec::new(),
    }
}

/// Drains the UI channel until a reply arrives or `timeout` elapses,
/// mirroring the idle loop's non-blocking `try_recv` rather than blocking.
fn drain_one(
    rx: &std::sync::mpsc::Receiver<(TabId, Result<WorkerResponse, MizuError>)>,
    timeout: std::time::Duration,
) -> Option<(TabId, Result<WorkerResponse, MizuError>)> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Ok(msg) = rx.try_recv() {
            return Some(msg);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// The router satisfies the manager's exact channel contract, end to end,
/// against a real sandboxed process.
#[test]
fn the_router_serves_the_window_managers_channel_shape() {
    let (logic_tx, router_rx) = std::sync::mpsc::channel();
    let (router_tx, logic_rx) = std::sync::mpsc::channel();
    spawn_router_with_exe(router_rx, router_tx, test_worker_exe()).expect("router must start");

    let tab = TabId(1);
    // Exactly how the manager posts an event: fire and forget.
    logic_tx
        .send((tab, UiEvent::Reload(Box::new(navigate_doc()))))
        .expect("post reload");

    let (got_tab, res) =
        drain_one(&logic_rx, std::time::Duration::from_secs(20)).expect("reload must be answered");
    assert_eq!(got_tab, tab);
    let response = res.expect("reload must succeed");
    assert!(
        !response.gesture,
        "a document load is document agency, never a user gesture"
    );

    // A click, authorized by the broker on the reader thread.
    logic_tx
        .send((tab, UiEvent::Click { node_id: 0 }))
        .expect("post click");
    let (_, res) =
        drain_one(&logic_rx, std::time::Duration::from_secs(20)).expect("click must be answered");
    let response = res.expect("click must succeed");
    assert!(response.gesture, "a click is user agency");
    assert!(
        matches!(
            response.runtime_actions.as_slice(),
            [RuntimeAction::Navigate { .. }]
        ),
        "the navigate must survive authorization, got {:?}",
        response.runtime_actions
    );

    logic_tx.send((tab, UiEvent::CloseTab)).expect("post close");
}

/// Gate G1 through the bridge: the same bound action reached by a timer
/// instead of a click must be refused, because the FIFO records the agency
/// of the event the router actually sent.
#[test]
fn a_timer_driven_navigate_is_refused_through_the_bridge() {
    let (logic_tx, router_rx) = std::sync::mpsc::channel();
    let (router_tx, logic_rx) = std::sync::mpsc::channel();
    spawn_router_with_exe(router_rx, router_tx, test_worker_exe()).expect("router must start");

    let tab = TabId(2);
    // Same navigate action, reachable from a root timer.
    let mut doc = navigate_doc();
    let mut arena = ExprArena::new();
    let root = arena.alloc(Expr::Literal(Value::from("mizu://example.com/next")));
    doc.root_timer_actions = vec![Action::Navigate {
        url: ExprTree { arena, root },
    }];

    logic_tx
        .send((tab, UiEvent::Reload(Box::new(doc))))
        .expect("post reload");
    drain_one(&logic_rx, std::time::Duration::from_secs(20)).expect("reload answered");

    logic_tx
        .send((tab, UiEvent::RootTimer { index: 0 }))
        .expect("post timer");
    let (_, res) =
        drain_one(&logic_rx, std::time::Duration::from_secs(20)).expect("timer must be answered");
    let response = res.expect("timer evaluation itself succeeds");

    assert!(
        !response.gesture,
        "a timer must never be marked as a user gesture"
    );
    assert!(
        response.runtime_actions.is_empty(),
        "a timer-driven navigate must be refused by gate G1, got {:?}",
        response.runtime_actions
    );

    logic_tx.send((tab, UiEvent::CloseTab)).expect("post close");
}

/// An unbound click produces no UI message at all (the worker answers
/// `NoOp`), and the FIFO stays in step so the next real event is still
/// judged with its own agency.
#[test]
fn an_unbound_click_delivers_nothing_and_keeps_the_fifo_aligned() {
    let (logic_tx, router_rx) = std::sync::mpsc::channel();
    let (router_tx, logic_rx) = std::sync::mpsc::channel();
    spawn_router_with_exe(router_rx, router_tx, test_worker_exe()).expect("router must start");

    let tab = TabId(3);
    logic_tx
        .send((tab, UiEvent::Reload(Box::new(navigate_doc()))))
        .expect("post reload");
    drain_one(&logic_rx, std::time::Duration::from_secs(20)).expect("reload answered");

    // Nothing bound to 4242: the worker replies NoOp, which the reader
    // consumes without waking the manager.
    logic_tx
        .send((tab, UiEvent::Click { node_id: 4242 }))
        .expect("post unbound click");
    assert!(
        drain_one(&logic_rx, std::time::Duration::from_millis(500)).is_none(),
        "an unbound click must not deliver a UI message"
    );

    // The bound click still gets its own agency — proving the NoOp consumed
    // exactly one FIFO slot rather than leaving the queue skewed.
    logic_tx
        .send((tab, UiEvent::Click { node_id: 0 }))
        .expect("post bound click");
    let (_, res) =
        drain_one(&logic_rx, std::time::Duration::from_secs(20)).expect("bound click answered");
    let response = res.expect("click succeeds");
    assert!(response.gesture, "FIFO desynchronized: agency was lost");
    assert!(
        matches!(
            response.runtime_actions.as_slice(),
            [RuntimeAction::Navigate { .. }]
        ),
        "FIFO desynchronized: got {:?}",
        response.runtime_actions
    );

    logic_tx.send((tab, UiEvent::CloseTab)).expect("post close");
}

/// Two tabs get two independent processes, and neither can see the other.
#[test]
fn tabs_get_independent_worker_processes() {
    let (logic_tx, router_rx) = std::sync::mpsc::channel();
    let (router_tx, logic_rx) = std::sync::mpsc::channel();
    spawn_router_with_exe(router_rx, router_tx, test_worker_exe()).expect("router must start");

    for tab in [TabId(10), TabId(11)] {
        logic_tx
            .send((tab, UiEvent::Reload(Box::new(navigate_doc()))))
            .expect("post reload");
    }

    let mut seen = Vec::new();
    for _ in 0..2 {
        let (tab, res) =
            drain_one(&logic_rx, std::time::Duration::from_secs(20)).expect("both tabs answer");
        res.expect("reload succeeds");
        seen.push(tab);
    }
    seen.sort();
    assert_eq!(seen, vec![TabId(10), TabId(11)]);

    for tab in [TabId(10), TabId(11)] {
        logic_tx.send((tab, UiEvent::CloseTab)).expect("post close");
    }
}

/// The production path: a network call through the router must arrive
/// broker-resolved and authorized.
///
/// Regression for the cutover bug where the worker resolved aliases itself,
/// the broker refused the result as forgeable, and every request a document
/// made was silently dropped.
#[test]
fn a_network_call_survives_the_bridge_and_is_resolved() {
    use mizu_core::core::types::Symbol;
    use mizu_core::parser::logic::{NetworkMethod, PayloadFormat};
    use mizu_core::parser::{EndpointKind, UrlEndpoint};

    let alias = Symbol(0);
    let mut registry = UrlRegistry::default();
    registry.insert(
        alias,
        UrlEndpoint {
            kind: EndpointKind::Api,
            raw_target: "/search".to_string(),
        },
    );

    let mut doc = navigate_doc();
    let mut interner = StringInterner::new();
    interner.get_or_intern("results");
    doc.interner = interner.freeze();
    doc.url_registry = registry;
    doc.click_actions.insert(
        0u32,
        Action::NetworkCall {
            method: NetworkMethod::Query,
            alias_sym: alias,
            payload: None,
            path_param: None,
            target_var: "results".to_string(),
            format: PayloadFormat::Json,
            headers: Vec::new(),
        },
    );

    let (logic_tx, router_rx) = std::sync::mpsc::channel();
    let (router_tx, logic_rx) = std::sync::mpsc::channel();
    spawn_router_with_exe(router_rx, router_tx, test_worker_exe()).expect("router must start");

    let tab = TabId(20);
    logic_tx
        .send((tab, UiEvent::Reload(Box::new(doc))))
        .expect("post reload");
    drain_one(&logic_rx, std::time::Duration::from_secs(20)).expect("reload answered");

    logic_tx
        .send((tab, UiEvent::Click { node_id: 0 }))
        .expect("post click");
    let (_, res) =
        drain_one(&logic_rx, std::time::Duration::from_secs(20)).expect("click must be answered");
    let response = res.expect("click succeeds");

    match response.runtime_actions.as_slice() {
        [RuntimeAction::ResolvedCall { url, method, .. }] => {
            assert_eq!(url, "mizu://example.com/search");
            assert_eq!(method, "QUERY");
        }
        other => panic!(
            "the request must reach the UI broker-resolved, got {other:?} — \
             if this is empty, the broker refused it again"
        ),
    }

    logic_tx.send((tab, UiEvent::CloseTab)).expect("post close");
}
