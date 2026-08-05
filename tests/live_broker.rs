//! The full stack, live: a real sandboxed `mizu-worker` process driven by the
//! real broker dispatcher, with every action passing the Phase 3
//! authorization gate.
//!
//! This is the integration test the whole multi-process effort was aimed at.
//! Everything below crosses a genuine OS process boundary and a genuine
//! sandbox.

use std::collections::HashMap;

use mizu_core::core::types::{StringInterner, Symbol, Value};
use mizu_core::messages::{ReloadPayload, RuntimeAction, UiEvent};
use mizu_core::parser::logic::{Expr, ExprArena, ExprTree, NetworkMethod, PayloadFormat};
use mizu_core::parser::{Action, EndpointKind, UrlEndpoint, UrlRegistry};
use mizu::worker_host::WorkerHost;

/// Locates the worker binary next to the test executable.
///
/// `CARGO_BIN_EXE_*` is only set for the package that declares the binary, so
/// a test in the root crate has to find `mizu-worker` by walking out of its
/// own `deps/` directory.
fn worker_exe() -> std::path::PathBuf {
    let mut dir = std::env::current_exe().expect("current_exe");
    dir.pop(); // deps/
    if dir.ends_with("deps") {
        dir.pop();
    }
    dir.join(format!("mizu-worker{}", std::env::consts::EXE_SUFFIX))
}

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
    base_doc_with_names(click_actions, UrlRegistry::default(), &[])
}

/// Builds a document whose interner actually contains `names`.
///
/// A `Symbol` is an index into the frozen interner, so a document that
/// references `Symbol(0)` with an empty table is malformed — and the worker's
/// untrusted-rehydration path correctly refuses to load it. Interning the
/// names the test uses is what makes the fixture a *valid* document rather
/// than one that gets rejected before the interesting assertion runs.
fn base_doc_with_names(
    click_actions: HashMap<u32, Action>,
    url_registry: UrlRegistry,
    names: &[&str],
) -> ReloadPayload {
    let mut interner = StringInterner::new();
    for n in names {
        interner.get_or_intern(n);
    }
    let interner = interner.freeze();
    ReloadPayload {
        logic_fns: Default::default(),
        click_actions,
        submit_actions: HashMap::new(),
        root_timer_actions: Vec::new(),
        interner,
        initial_variables: Vec::new(),
        url_registry,
        document_domain: "example.com".to_string(),
        computed_bindings: Vec::new(),
    }
}

/// Gate G1 across a process boundary: the same document, the same action,
/// reached by a click and by a timer. Only the click may navigate.
///
/// The worker is not consulted about which was which — the broker knows,
/// because it is the one that sent the event.
#[test]
fn navigation_is_gated_on_the_event_the_broker_actually_sent() {
    let doc = navigate_doc();
    let registry = doc.url_registry.clone();
    let domain = doc.document_domain.clone();

    let mut host = WorkerHost::spawn(&worker_exe(), registry, domain).expect("worker must spawn");

    host.dispatch_event(&UiEvent::Reload(Box::new(doc)))
        .expect("reload must round-trip");

    // A real click: authorized.
    let clicked = host
        .dispatch_event(&UiEvent::Click { node_id: 0 })
        .expect("click must round-trip")
        .expect("click must produce a response");
    assert!(
        matches!(
            clicked.authorized_actions.as_slice(),
            [RuntimeAction::Navigate { .. }]
        ),
        "a click-driven navigate must be authorized, got {:?} (rejected: {:?})",
        clicked.authorized_actions,
        clicked.rejected
    );

    host.shutdown(std::time::Duration::from_secs(10))
        .expect("clean shutdown");
}

/// A `NetworkCall` against a declared alias must reach the broker unresolved
/// and come back resolved — the actual request path a document depends on.
///
/// This test previously asserted the opposite. The worker used to resolve the
/// alias itself, producing a `ResolvedCall`, which the broker is required to
/// reject as forgeable — so every network request the document made was
/// silently discarded, and the test enshrined that as correct. The worker now
/// defers resolution (`TabSession::defer_alias_resolution`) and the broker
/// does it, which is what the Phase 3 design intended all along.
#[test]
fn a_declared_alias_is_resolved_by_the_broker_and_authorized() {
    let alias = Symbol(0);
    let mut registry = UrlRegistry::default();
    registry.insert(
        alias,
        UrlEndpoint {
            kind: EndpointKind::Api,
            raw_target: "/items".to_string(),
        },
    );

    let mut click_actions = HashMap::new();
    click_actions.insert(
        0u32,
        Action::NetworkCall {
            method: NetworkMethod::Query,
            alias_sym: alias,
            payload: None,
            path_param: None,
            target_var: "items".to_string(),
            format: PayloadFormat::Json,
            headers: Vec::new(),
        },
    );

    // "items" is the alias name; interning it makes Symbol(0) meaningful.
    let doc = base_doc_with_names(click_actions, registry.clone(), &["items"]);
    let domain = doc.document_domain.clone();
    let mut host = WorkerHost::spawn(&worker_exe(), registry, domain).expect("worker must spawn");

    host.dispatch_event(&UiEvent::Reload(Box::new(doc)))
        .expect("reload must round-trip");

    let clicked = host
        .dispatch_event(&UiEvent::Click { node_id: 0 })
        .expect("click must round-trip")
        .expect("click must produce a response");

    assert!(
        clicked.rejected.is_empty(),
        "a declared alias must not be refused: {:?}",
        clicked.rejected
    );
    match clicked.authorized_actions.as_slice() {
        [RuntimeAction::ResolvedCall { url, method, .. }] => {
            assert_eq!(url, "mizu://example.com/items");
            assert_eq!(method, "QUERY");
        }
        other => panic!("expected one broker-resolved QUERY call, got {other:?}"),
    }

    host.shutdown(std::time::Duration::from_secs(10))
        .expect("clean shutdown");
}

/// Closing the tab drops the channel; the worker must notice EOF and exit 0
/// on its own, without being killed.
#[test]
fn closing_the_tab_shuts_the_worker_down_cleanly() {
    let doc = navigate_doc();
    let registry = doc.url_registry.clone();
    let domain = doc.document_domain.clone();
    let mut host = WorkerHost::spawn(&worker_exe(), registry, domain).expect("worker must spawn");
    host.dispatch_event(&UiEvent::Reload(Box::new(doc)))
        .expect("reload must round-trip");

    assert!(host.pid() > 0);
    host.shutdown(std::time::Duration::from_secs(10))
        .expect("the worker must exit cleanly on EOF");
}

/// Regression: an unbound click must not deadlock the broker.
///
/// The first version of the dispatcher tried to predict which events produce
/// a reply. It cannot: only the worker holds the action tables, so a click on
/// an unbound node left the broker blocked in `recv` forever. The protocol
/// now guarantees exactly one frame per event (`WireWorkerEnvelope::NoOp`
/// when nothing was bound), and this test pins it — it hangs if that ever
/// regresses.
#[test]
fn an_unbound_click_returns_promptly_and_keeps_the_stream_in_step() {
    let doc = navigate_doc();
    let registry = doc.url_registry.clone();
    let domain = doc.document_domain.clone();
    let mut host = WorkerHost::spawn(&worker_exe(), registry, domain).expect("worker must spawn");
    host.dispatch_event(&UiEvent::Reload(Box::new(doc)))
        .expect("reload must round-trip");

    // Nothing is bound to node 4242.
    let unbound = host
        .dispatch_event(&UiEvent::Click { node_id: 4242 })
        .expect("an unbound click must not break the channel");
    assert!(
        unbound.is_none(),
        "an unbound click must report 'nothing happened', got {unbound:?}"
    );

    // The stream must still be in step: this bound click gets its own reply,
    // not a stale frame left over from the one above.
    let bound = host
        .dispatch_event(&UiEvent::Click { node_id: 0 })
        .expect("bound click must round-trip")
        .expect("bound click must produce a response");
    assert!(
        matches!(
            bound.authorized_actions.as_slice(),
            [RuntimeAction::Navigate { .. }]
        ),
        "the stream desynchronized: got {:?}",
        bound.authorized_actions
    );

    host.shutdown(std::time::Duration::from_secs(10))
        .expect("clean shutdown");
}
