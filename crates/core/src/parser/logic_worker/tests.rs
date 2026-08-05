//! Tests for the logic_worker module.

use std::collections::HashMap;

use rustc_hash::FxHashMap;

use super::helpers::resolve_endpoint_url;
use super::session::TabSession;
use super::worker::LogicWorker;
use crate::core::errors::MizuError;
use crate::core::types::{StringInterner, Value, VariableStore};
use crate::messages::{RuntimeAction, WorkerResponse};
use crate::parser::Action;
use crate::parser::urls::{EndpointKind, UrlEndpoint};

fn api(path: &str) -> UrlEndpoint {
    UrlEndpoint {
        kind: EndpointKind::Api,
        raw_target: path.to_owned(),
    }
}

/// Gate G1 must be a property of the *event*, never of the tab.
///
/// The bug this pins: agency used to be an ambient `has_user_gesture`
/// boolean, set on the UI thread when an input was dispatched and read on
/// the UI thread when a worker response was drained. Because responses are
/// drained FIFO with no link to the events that produced them, a
/// `RootTimer` batch that merely arrived near a click was labelled
/// `NavigationInitiator::UserGesture` and cleared `check_navigation`'s N3
/// cross-origin block. Root timers may fire every 16 ms, so a document
/// only had to wait for the user to click anything at all.
///
/// Here both handlers run the *same* `navigate` action, and both events
/// are queued before either response is read — the exact interleaving that
/// used to leak agency. The click's batch must carry it; the timer's must
/// not, whichever order they are drained in.
#[test]
fn gesture_is_per_event_not_ambient() {
    use crate::messages::{ReloadPayload, TabId, UiEvent};
    use crate::parser::logic::{Expr, ExprArena, ExprTree};
    use std::sync::mpsc;

    let (tx_in, rx_in) = mpsc::channel::<(TabId, UiEvent)>();
    let (tx_out, rx_out) = mpsc::channel::<(TabId, Result<WorkerResponse, MizuError>)>();
    let tab = TabId(0);
    let _handle = LogicWorker::spawn(rx_in, tx_out).expect("logic worker thread must spawn");

    // One identical cross-origin `navigate` action, reachable both from a
    // click and from a root timer, so the only difference between the two
    // responses is the event that triggered them.
    let navigate = || {
        let mut arena = ExprArena::new();
        let root = arena.alloc(Expr::Literal(Value::from("mizu://evil.example/")));
        Action::Navigate {
            url: ExprTree { arena, root },
        }
    };
    let mut click_actions = HashMap::new();
    click_actions.insert(0u32, navigate());

    let mut interner = StringInterner::new();
    let interner = interner.freeze();

    tx_in
        .send((
            tab,
            UiEvent::Reload(Box::new(ReloadPayload {
                logic_fns: FxHashMap::default(),
                click_actions,
                submit_actions: HashMap::new(),
                root_timer_actions: vec![navigate()],
                interner,
                initial_variables: Vec::new(),
                url_registry: FxHashMap::default(),
                document_domain: String::new(),
                computed_bindings: Vec::new(),
            })),
        ))
        .expect("worker thread must be alive to receive Reload");
    let reload = rx_out
        .recv()
        .expect("worker must respond to Reload")
        .1
        .expect("reload must not error");
    assert!(
        !reload.gesture,
        "a document load is document agency, never a user gesture"
    );

    // Queue both events before reading either response: this is the
    // interleaving the ambient flag got wrong.
    tx_in
        .send((tab, UiEvent::Click { node_id: 0 }))
        .expect("worker alive");
    tx_in
        .send((tab, UiEvent::RootTimer { index: 0 }))
        .expect("worker alive");

    let click_batch = rx_out
        .recv()
        .expect("worker must respond to Click")
        .1
        .expect("click action must not error");
    assert!(
        click_batch.gesture,
        "a Click batch carries user agency: gate G1 must let its navigate through"
    );

    let timer_batch = rx_out
        .recv()
        .expect("worker must respond to RootTimer")
        .1
        .expect("timer action must not error");
    assert!(
        !timer_batch.gesture,
        "a RootTimer batch must never inherit agency from a click processed \
         just before it — this is the N3 cross-origin bypass"
    );

    // Both really did queue the same privileged action, so the assertions
    // above are about agency and not about one batch being empty.
    for batch in [&click_batch, &timer_batch] {
        assert!(
            batch
                .runtime_actions
                .iter()
                .any(|a| matches!(a, RuntimeAction::Navigate { .. })),
            "both batches must contain the navigate action under test"
        );
    }
}

fn media(url: &str) -> UrlEndpoint {
    UrlEndpoint {
        kind: EndpointKind::Media,
        raw_target: url.to_owned(),
    }
}

/// Simulates `UiEvent::SubmitForm` with a mix of declared and undeclared
/// field names.  After the fix, the interner must not grow for undeclared
/// fields; declared fields must be updated.
#[test]
fn submit_form_with_unknown_field_does_not_grow_interner() {
    let mut store = VariableStore::new();
    store.set("username", Value::from("alice"));
    store.set("email", Value::from("alice@mizu"));
    let mut store = store.freeze();

    let frozen_size = store.interner.vec.len();

    let fields = vec![
        ("username".to_string(), Value::from("bob")),
        ("undeclared_field".to_string(), Value::from("ignored")),
        ("email".to_string(), Value::from("bob@mizu")),
    ];
    for (name, val) in fields {
        store.set_runtime(&name, val);
    }

    assert_eq!(
        store.interner.vec.len(),
        frozen_size,
        "interner must not grow when unknown fields arrive via SubmitForm"
    );
    assert_eq!(*store.get("username").unwrap(), Value::from("bob"));
    assert_eq!(*store.get("email").unwrap(), Value::from("bob@mizu"));
    assert!(
        store.get("undeclared_field").is_err(),
        "undeclared form field must not appear in the store"
    );
}

/// Simulates `UiEvent::UpdateVariable` for a declared and an undeclared name.
#[test]
fn update_variable_with_unknown_name_does_not_grow_interner() {
    let mut store = VariableStore::new();
    store.set("products", Value::Null);
    let mut store = store.freeze();

    let frozen_size = store.interner.vec.len();

    store.set_runtime("products", Value::Decimal(5));
    store.set_runtime("unregistered_response_key", Value::Decimal(99));

    assert_eq!(
        store.interner.vec.len(),
        frozen_size,
        "interner must not grow via UpdateVariable for unknown names"
    );
    assert_eq!(*store.get("products").unwrap(), Value::Decimal(5));
    assert!(store.get("unregistered_response_key").is_err());
}

#[test]
fn api_endpoint_gets_full_mizu_url() {
    let url = resolve_endpoint_url("example.com", &api("/v1/products"), None).unwrap();
    assert_eq!(url, "mizu://example.com/v1/products");
}

#[test]
fn api_path_leading_slash_not_doubled() {
    // raw_target always starts with `/`; the composed URL must not have `//`.
    let url = resolve_endpoint_url("host.mizu", &api("/health"), None).unwrap();
    assert_eq!(url, "mizu://host.mizu/health");
    assert!(
        !url.contains("//health"),
        "double slash must not appear: {url}"
    );
}

#[test]
fn api_endpoint_path_param_appended_when_no_placeholder() {
    let url = resolve_endpoint_url("example.com", &api("/v1/products"), Some("42")).unwrap();
    assert_eq!(url, "mizu://example.com/v1/products/42");
}

#[test]
fn api_endpoint_placeholder_substituted() {
    let url = resolve_endpoint_url("api.local", &api("/v1/items/{id}"), Some("99")).unwrap();
    assert_eq!(url, "mizu://api.local/v1/items/99");
}

#[test]
fn api_endpoint_nested_placeholder_substituted() {
    let url =
        resolve_endpoint_url("api.local", &api("/v1/users/{uid}/posts/{pid}"), Some("7")).unwrap();
    // Only the first placeholder is replaced.
    assert_eq!(url, "mizu://api.local/v1/users/7/posts/{pid}");
}

#[test]
fn media_endpoint_uses_raw_target_unchanged() {
    let url = resolve_endpoint_url(
        "ignored.com",
        &media("mizu://cdn.example.com/logo.png"),
        None,
    )
    .unwrap();
    assert_eq!(url, "mizu://cdn.example.com/logo.png");
}

#[test]
fn media_endpoint_path_param_appended_when_no_placeholder() {
    let url = resolve_endpoint_url(
        "ignored.com",
        &media("mizu://cdn.example.com/assets"),
        Some("icon.png"),
    )
    .unwrap();
    assert_eq!(url, "mizu://cdn.example.com/assets/icon.png");
}

#[test]
fn path_param_with_reserved_chars_percent_encoded() {
    let url =
        resolve_endpoint_url("api.local", &api("/v1/search/{query}"), Some("a b&c?d=1%")).unwrap();
    assert_eq!(url, "mizu://api.local/v1/search/a%20b%26c%3Fd%3D1%25");
}

#[test]
fn path_param_plain_segment_unchanged() {
    let url = resolve_endpoint_url(
        "api.local",
        &api("/v1/items/{id}"),
        Some("foo-bar_123.~baz"),
    )
    .unwrap();
    assert_eq!(url, "mizu://api.local/v1/items/foo-bar_123.~baz");
}

#[test]
fn path_param_with_slash_rejected() {
    let err = resolve_endpoint_url("api.local", &api("/v1/items/{id}"), Some("a/b")).unwrap_err();
    assert!(matches!(err, MizuError::ExecutionError(_)));
}

#[test]
fn path_param_with_traversal_rejected() {
    let err = resolve_endpoint_url("api.local", &api("/v1/items/{id}"), Some("..")).unwrap_err();
    assert!(matches!(err, MizuError::ExecutionError(_)));
}

#[test]
fn path_param_with_control_char_rejected() {
    let err = resolve_endpoint_url("api.local", &api("/v1/items/{id}"), Some("a\nb")).unwrap_err();
    assert!(matches!(err, MizuError::ExecutionError(_)));
}

/// End-to-end regression test for the stack-size fix in
/// `LogicWorker::spawn`: drives a real `LogicWorker` background thread
/// (spawned exactly as production does, via `LogicWorker::spawn`) through
/// a 300-level-deep expression — the same shape used by
/// `core::types::tests::eval_depth_guard` and
/// `cross_function_composition_depth_guard`, deep enough to exceed
/// `MAX_EVAL_DEPTH` (256) — and asserts the worker returns the controlled
/// "evaluation nesting too deep" error rather than the process crashing
/// with a native stack overflow.
///
/// Before `LogicWorker::spawn` used an explicit `stack_size`, this same
/// scenario reliably overflowed the platform-default stack in debug
/// builds (see `STACK_SIZE_BYTES`'s doc comment for the measurement that
/// proved it). Because a real stack overflow aborts the whole process and
/// cannot be caught with `catch_unwind`, this test re-execs the test
/// binary as a child process and inspects its exit status — mirroring
/// `cross_function_composition_depth_guard` in `core::types`.
#[test]
fn logic_worker_thread_survives_max_eval_depth_without_native_crash() {
    const CHILD_ENV: &str = "MIZU_LOGICWORKER_DEPTH_CHILD";
    const OK_MARKER: &str = "LOGICWORKER_DEPTH_GUARD_OK";

    if std::env::var_os(CHILD_ENV).is_some() {
        run_logic_worker_depth_guard_child(OK_MARKER);
        return;
    }

    let exe = std::env::current_exe().expect("current_exe");
    let output = std::process::Command::new(exe)
        .arg("parser::logic_worker::tests::logic_worker_thread_survives_max_eval_depth_without_native_crash")
        .arg("--exact")
        .arg("--nocapture")
        .arg("--test-threads=1")
        .env(CHILD_ENV, "1")
        .output()
        .expect("failed to spawn child test process");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success() && stdout.contains(OK_MARKER),
        "LogicWorker's dedicated thread must survive a MAX_EVAL_DEPTH=256+ \
         evaluation without a native stack overflow (status={:?}).\n\
         --- child stdout ---\n{}\n--- child stderr ---\n{}",
        output.status,
        stdout,
        stderr
    );
}

/// Runs the actual scenario on the current process: spawns a real
/// `LogicWorker`, reloads it with a click action bound to a 300-level
/// deep `BinaryOp` chain, fires the click, and prints `ok_marker` iff the
/// worker responds with the expected controlled error instead of the
/// process crashing.
fn run_logic_worker_depth_guard_child(ok_marker: &str) {
    use crate::messages::{ReloadPayload, TabId, UiEvent};
    use crate::parser::logic::{BinOp, Expr, ExprArena, ExprTree};
    use std::sync::mpsc;

    let (tx_in, rx_in) = mpsc::channel::<(TabId, UiEvent)>();
    let (tx_out, rx_out) = mpsc::channel::<(TabId, Result<WorkerResponse, MizuError>)>();
    let tab = TabId(0);
    let _handle = LogicWorker::spawn(rx_in, tx_out).expect("logic worker thread must spawn");

    // 300-level deep chain: exceeds MAX_EVAL_DEPTH (256), same shape as
    // core::types::tests::eval_depth_guard /
    // cross_function_composition_depth_guard.
    let mut arena = ExprArena::new();
    let mut expr = Expr::Literal(Value::Decimal(0));
    for _ in 0..300 {
        let left = arena.alloc(expr);
        let right = arena.alloc(Expr::Literal(Value::Decimal(0)));
        expr = Expr::BinaryOp {
            left,
            op: BinOp::Add,
            right,
        };
    }
    let root = arena.alloc(expr);

    let mut click_actions = HashMap::new();
    click_actions.insert(0u32, Action::Eval(ExprTree { arena, root }));

    let mut interner = StringInterner::new();
    let interner = interner.freeze();

    tx_in
        .send((
            tab,
            UiEvent::Reload(Box::new(ReloadPayload {
                logic_fns: FxHashMap::default(),
                click_actions,
                submit_actions: HashMap::new(),
                root_timer_actions: Vec::new(),
                interner,
                initial_variables: Vec::new(),
                url_registry: FxHashMap::default(),
                document_domain: String::new(),
                computed_bindings: Vec::new(),
            })),
        ))
        .expect("worker thread must be alive to receive Reload");
    rx_out
        .recv()
        .expect("worker must respond to Reload")
        .1
        .expect("reload must not error");

    tx_in
        .send((tab, UiEvent::Click { node_id: 0 }))
        .expect("worker thread must still be alive after Reload");

    match rx_out.recv().map(|(_, r)| r) {
        Ok(Err(MizuError::ExecutionError(msg))) if msg.contains("nesting too deep") => {
            println!("{ok_marker}");
        }
        // Also acceptable: the instruction budget could in principle be
        // exhausted first depending on constant tuning — still a clean,
        // bounded error, not a crash.
        Ok(Err(MizuError::Timeout)) => {
            println!("{ok_marker}");
        }
        other => {
            println!("UNEXPECTED_RESULT: {other:?}");
        }
    }
}

// ── TabSession: the transport-free state machine ─────────────────────────────

/// Builds a minimal `ReloadPayload` with the given click actions and timers.
fn payload(
    click_actions: HashMap<u32, Action>,
    root_timer_actions: Vec<Action>,
) -> crate::messages::ReloadPayload {
    let mut interner = StringInterner::new();
    let interner = interner.freeze();
    crate::messages::ReloadPayload {
        logic_fns: FxHashMap::default(),
        click_actions,
        submit_actions: HashMap::new(),
        root_timer_actions,
        interner,
        initial_variables: Vec::new(),
        url_registry: FxHashMap::default(),
        document_domain: String::new(),
        computed_bindings: Vec::new(),
    }
}

fn navigate_action(url: &str) -> Action {
    use crate::parser::logic::{Expr, ExprArena, ExprTree};
    let mut arena = ExprArena::new();
    let root = arena.alloc(Expr::Literal(Value::from(url)));
    Action::Navigate {
        url: ExprTree { arena, root },
    }
}

/// The whole point of the refactor: a document can be driven to completion
/// with no channel, no thread, and no `TabId` anywhere in sight.
#[test]
fn a_session_evaluates_without_any_transport() {
    use crate::messages::UiEvent;

    let mut click_actions = HashMap::new();
    click_actions.insert(0u32, navigate_action("mizu://example.com/next"));

    let mut session = TabSession::new();
    let reload = session.apply_reload(payload(click_actions, vec![]));
    assert!(
        !reload.gesture,
        "a document load is document agency, never a user gesture"
    );

    let response = session
        .apply_event(UiEvent::Click { node_id: 0 })
        .expect("a bound click must produce a response")
        .expect("the navigate action must succeed");
    assert!(matches!(
        response.runtime_actions.as_slice(),
        [RuntimeAction::Navigate { .. }]
    ));
}

/// Gate G1 is derived inside `apply_event` from the event variant, so a
/// transport cannot forge it by any means short of fabricating a `Click` —
/// which only the UI layer can emit. This is the same invariant
/// `gesture_is_per_event_not_ambient` pins through the mpsc shell, asserted
/// one level down where the derivation actually happens.
#[test]
fn session_derives_gesture_from_the_event_variant() {
    use crate::messages::UiEvent;

    let mut click_actions = HashMap::new();
    click_actions.insert(0u32, navigate_action("mizu://evil.example/"));

    let mut session = TabSession::new();
    session.apply_reload(payload(
        click_actions,
        vec![navigate_action("mizu://evil.example/")],
    ));

    let click = session
        .apply_event(UiEvent::Click { node_id: 0 })
        .expect("bound click responds")
        .expect("action succeeds");
    let timer = session
        .apply_event(UiEvent::RootTimer { index: 0 })
        .expect("bound timer responds")
        .expect("action succeeds");

    assert!(click.gesture, "a click is user agency");
    assert!(
        !timer.gesture,
        "a timer is document agency and must never inherit a gesture"
    );
}

/// An event that addresses nothing must produce no response at all, not an
/// empty one. The pre-refactor loop sent nothing in these cases; collapsing
/// them into `Some(Ok(empty))` would make the UI process a state update on
/// every stray click.
#[test]
fn events_addressing_nothing_produce_no_response() {
    use crate::messages::UiEvent;

    let mut session = TabSession::new();
    session.apply_reload(payload(HashMap::new(), vec![]));

    assert!(
        session.apply_event(UiEvent::Click { node_id: 99 }).is_none(),
        "a click on a node with no binding must send nothing"
    );
    assert!(
        session.apply_event(UiEvent::RootTimer { index: 7 }).is_none(),
        "a timer index past the end must send nothing"
    );
    assert!(
        session.apply_event(UiEvent::CloseTab).is_none(),
        "CloseTab is the owner's business; a session cannot destroy itself"
    );
}

/// `from_reload` is the one-shot constructor, equivalent to `new` +
/// `apply_reload` with the initial update discarded.
#[test]
fn from_reload_produces_a_ready_session() {
    use crate::messages::UiEvent;

    let mut click_actions = HashMap::new();
    click_actions.insert(3u32, navigate_action("mizu://example.com/x"));

    let mut session = TabSession::from_reload(payload(click_actions, vec![]));
    assert!(
        session.apply_event(UiEvent::Click { node_id: 3 }).is_some(),
        "the document must already be loaded"
    );
}
