//! End-to-end test of the real, fully-sandboxed `mizu-worker` binary.
//!
//! This is the test that proves the whole multi-process design actually
//! works: it spawns the production binary (not a fixture), which applies the
//! production sandbox to itself, and then drives a real document through it.
//!
//! The most important thing it verifies is not visible in the assertions:
//! that the IPC channel *survives confinement*. On Windows the worker drops
//! its own token to Untrusted integrity; on Linux it installs a seccomp
//! filter that kills the process on any syscall outside a tiny allowlist. If
//! either broke the already-open channel, the recv below would hang or the
//! child would die, and no amount of unit testing would have caught it.

use std::collections::HashMap;

use mizu_core::core::types::{StringInterner, Value};
use mizu_core::messages::ReloadPayload;
use mizu_core::parser::Action;
use mizu_core::parser::logic::{Expr, ExprArena, ExprTree};
use mizu_ipc::process::spawn_worker;
use mizu_ipc::wire::reload::WireReloadPayload;
use mizu_ipc::wire::{WireRuntimeAction, WireUiEvent, WireWorkerEnvelope};

/// A document whose node 0 has `click -> navigate "mizu://example.com/next"`.
fn document() -> ReloadPayload {
    let mut arena = ExprArena::new();
    let root = arena.alloc(Expr::Literal(Value::from("mizu://example.com/next")));
    let navigate = Action::Navigate {
        url: ExprTree { arena, root },
    };

    let mut click_actions = HashMap::new();
    click_actions.insert(0u32, navigate);

    let mut interner = StringInterner::new();
    let interner = interner.freeze();

    ReloadPayload {
        logic_fns: Default::default(),
        click_actions,
        submit_actions: HashMap::new(),
        root_timer_actions: Vec::new(),
        interner,
        initial_variables: Vec::new(),
        url_registry: Default::default(),
        document_domain: "example.com".to_string(),
        computed_bindings: Vec::new(),
    }
}

#[test]
fn the_sandboxed_worker_loads_a_document_and_answers_events() {
    let exe = env!("CARGO_BIN_EXE_mizu-worker");
    let mut worker =
        spawn_worker(std::path::Path::new(exe), &[]).expect("worker must spawn and handshake");

    // Load the document. The worker rehydrates it through the untrusted path
    // and builds a `TabSession` from it.
    let payload = WireReloadPayload::from(&document());
    worker
        .tx
        .send(&WireUiEvent::Reload(Box::new(payload)))
        .expect("reload must send");

    match worker.rx.recv().expect("worker must answer the reload") {
        WireWorkerEnvelope::Ok(r) => assert!(
            !r.gesture,
            "a document load is document agency, never a user gesture"
        ),
        other => panic!("expected Ok for reload, got {other:?}"),
    }

    // Now a real click on the bound node. This is evaluation happening
    // inside a process that cannot open a file or a socket.
    worker
        .tx
        .send(&WireUiEvent::Click { node_id: 0 })
        .expect("click must send");

    match worker.rx.recv().expect("worker must answer the click") {
        WireWorkerEnvelope::Ok(r) => {
            assert!(r.gesture, "a click is user agency");
            assert!(
                matches!(
                    r.runtime_actions.as_slice(),
                    [WireRuntimeAction::Navigate { .. }]
                ),
                "expected a single Navigate action, got {:?}",
                r.runtime_actions
            );
        }
        other => panic!("expected Ok for click, got {other:?}"),
    }

    // An unbound node still owes exactly one frame — `NoOp`. Answering
    // every event is what keeps the request/response stream one-to-one; the
    // broker would otherwise have to guess which events produce a reply, and
    // guessing wrong deadlocks it (see `WireWorkerEnvelope::NoOp`).
    worker
        .tx
        .send(&WireUiEvent::Click { node_id: 4242 })
        .expect("unbound click must send");
    match worker.rx.recv().expect("an unbound click must still answer") {
        WireWorkerEnvelope::NoOp => {}
        other => panic!("expected NoOp for an unbound click, got {other:?}"),
    }

    // And the stream is still in step: this event gets its own reply, not a
    // leftover frame from the click above.
    worker
        .tx
        .send(&WireUiEvent::UpdateVariable {
            name: "nonexistent".to_string(),
            value: mizu_ipc::wire::WireValue::Int(1),
        })
        .expect("update must send");

    match worker.rx.recv().expect("worker must answer the update") {
        WireWorkerEnvelope::Ok(r) => assert!(
            !r.gesture,
            "an UpdateVariable is document agency, never a user gesture"
        ),
        other => panic!("expected Ok for update, got {other:?}"),
    }

    let status = worker
        .shutdown(std::time::Duration::from_secs(10))
        .expect("shutdown must reap the worker");
    assert!(
        status.success(),
        "the sandboxed worker must exit 0 on EOF, got {status:?}"
    );
}

/// A frame that survives `bytecheck` but violates Mizu's own invariants must
/// come back as an error, not crash the worker or get evaluated.
#[test]
fn a_forged_document_is_rejected_without_killing_the_worker() {
    let exe = env!("CARGO_BIN_EXE_mizu-worker");
    let mut worker =
        spawn_worker(std::path::Path::new(exe), &[]).expect("worker must spawn and handshake");

    // A structurally valid archive describing an impossible document: the
    // expression tree's root points at a node that does not exist.
    let mut payload = WireReloadPayload::from(&document());
    payload.click_actions = vec![mizu_ipc::wire::reload::WireAction::Eval(
        mizu_ipc::wire::reload::WireExprTree {
            nodes: vec![],
            args_pool: vec![],
            root: 99,
        },
    )];
    payload.click_action_ids = vec![0];

    worker
        .tx
        .send(&WireUiEvent::Reload(Box::new(payload)))
        .expect("forged reload must send");

    match worker.rx.recv().expect("worker must answer") {
        WireWorkerEnvelope::Err(_) => {}
        other => panic!("a dangling root must be rejected, got {other:?}"),
    }

    // Still alive and still correct afterwards.
    worker
        .tx
        .send(&WireUiEvent::Reload(Box::new(WireReloadPayload::from(
            &document(),
        ))))
        .expect("valid reload must send");
    match worker.rx.recv().expect("worker must still be answering") {
        WireWorkerEnvelope::Ok(_) => {}
        other => panic!("worker should have recovered, got {other:?}"),
    }

    let status = worker
        .shutdown(std::time::Duration::from_secs(10))
        .expect("shutdown must reap the worker");
    assert!(status.success(), "worker must still exit cleanly");
}
