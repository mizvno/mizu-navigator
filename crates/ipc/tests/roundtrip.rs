//! Round-trip serialization tests for all IPC wire types.
//!
//! Each test serializes a value with rkyv, validates the resulting archive,
//! deserializes it back, and asserts structural equality.

use mizu_ipc::wire::{
    WireNetworkMethod, WirePayloadFormat, WireRuntimeAction, WireTabId, WireUiEvent,
    WireWorkerEnvelope, WireWorkerError, WireWorkerResponse,
    value::{WireRecordField, WireValue},

    response::WireWorkerError as WireErr,
};

// ── Helper: serialize → validate → deserialize ───────────────────────────────

fn round_trip<T>(value: &T) -> T
where
    T: rkyv::Archive
        + for<'a> rkyv::Serialize<
            rkyv::api::high::HighSerializer<
                rkyv::util::AlignedVec,
                rkyv::ser::allocator::ArenaHandle<'a>,
                rkyv::rancor::Error,
            >,
        >,
    T::Archived: for<'a> rkyv::bytecheck::CheckBytes<
            rkyv::api::high::HighValidator<'a, rkyv::rancor::Error>,
        >
        + rkyv::Deserialize<T, rkyv::api::high::HighDeserializer<rkyv::rancor::Error>>,
{
    let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(value).expect("serialize");
    let archived = rkyv::access::<T::Archived, rkyv::rancor::Error>(&bytes)
        .expect("access / bytecheck");
    rkyv::deserialize::<T, rkyv::rancor::Error>(archived).expect("deserialize")
}

// ── WireValue ────────────────────────────────────────────────────────────────

#[test]
fn wire_value_null() {
    let v = WireValue::Null;
    let rt = round_trip(&v);
    assert!(matches!(rt, WireValue::Null));
}

#[test]
fn wire_value_bool() {
    for b in [true, false] {
        let rt = round_trip(&WireValue::Bool(b));
        assert!(matches!(rt, WireValue::Bool(x) if x == b));
    }
}

#[test]
fn wire_value_decimal() {
    let n = -123_456_789_00i64;
    let rt = round_trip(&WireValue::Decimal(n));
    assert!(matches!(rt, WireValue::Decimal(x) if x == n));
}

#[test]
fn wire_value_str() {
    let s = "hello, mizu 🦀";
    let rt = round_trip(&WireValue::Str(s.to_owned()));
    assert!(matches!(rt, WireValue::Str(ref x) if x == s));
}

#[test]
fn wire_value_list_nested() {
    let v = WireValue::List(vec![
        WireValue::Bool(true),
        WireValue::Decimal(42_00000000),
        WireValue::Str("x".into()),
    ]);
    let rt = round_trip(&v);
    let WireValue::List(items) = rt else { panic!("expected List") };
    assert_eq!(items.len(), 3);
    assert!(matches!(items[0], WireValue::Bool(true)));
    assert!(matches!(items[1], WireValue::Decimal(n) if n == 42_00000000));
    assert!(matches!(items[2], WireValue::Str(ref s) if s == "x"));
}

#[test]
fn wire_value_record() {
    let v = WireValue::Record(vec![
        WireRecordField { key: "foo".into(), hash: 0xDEADBEEF, value: WireValue::Null },
        WireRecordField { key: "bar".into(), hash: 0xCAFEBABE, value: WireValue::Bool(false) },
    ]);
    let rt = round_trip(&v);
    let WireValue::Record(fields) = rt else { panic!("expected Record") };
    assert_eq!(fields.len(), 2);
    assert_eq!(fields[0].key, "foo");
    assert_eq!(fields[0].hash, 0xDEADBEEF);
    assert_eq!(fields[1].key, "bar");
    assert!(matches!(fields[1].value, WireValue::Bool(false)));
}

#[test]
fn wire_value_file_handle_token() {
    let v = WireValue::FileHandleToken { token: 42, filename: "report.pdf".into() };
    let rt = round_trip(&v);
    let WireValue::FileHandleToken { token, filename } = rt else { panic!() };
    assert_eq!(token, 42);
    assert_eq!(filename, "report.pdf");
}

// ── WireTabId ────────────────────────────────────────────────────────────────

#[test]
fn wire_tab_id() {
    let id = WireTabId(u64::MAX);
    let rt = round_trip(&id);
    assert_eq!(rt.0, u64::MAX);
}

// ── WireUiEvent ──────────────────────────────────────────────────────────────

#[test]
fn wire_ui_event_click() {
    let ev = WireUiEvent::Click { node_id: 99 };
    let rt = round_trip(&ev);
    assert!(matches!(rt, WireUiEvent::Click { node_id: 99 }));
}

#[test]
fn wire_ui_event_root_timer() {
    let ev = WireUiEvent::RootTimer { index: 3 };
    let rt = round_trip(&ev);
    assert!(matches!(rt, WireUiEvent::RootTimer { index: 3 }));
}

#[test]
fn wire_ui_event_submit_form() {
    let ev = WireUiEvent::SubmitForm {
        submitter_node_id: 7,
        field_keys:   vec!["username".into(), "password".into()],
        field_values: vec![
            WireValue::Str("alice".into()),
            WireValue::Str("s3cr3t".into()),
        ],
    };
    let rt = round_trip(&ev);
    let WireUiEvent::SubmitForm { submitter_node_id, field_keys, field_values } = rt else { panic!() };
    assert_eq!(submitter_node_id, 7);
    assert_eq!(field_keys, ["username", "password"]);
    assert_eq!(field_values.len(), 2);
}

#[test]
fn wire_ui_event_update_variable() {
    let ev = WireUiEvent::UpdateVariable {
        name:  "counter".into(),
        value: WireValue::Decimal(100_00000000),
    };
    let rt = round_trip(&ev);
    let WireUiEvent::UpdateVariable { name, value } = rt else { panic!() };
    assert_eq!(name, "counter");
    assert!(matches!(value, WireValue::Decimal(n) if n == 100_00000000));
}

#[test]
fn wire_ui_event_reload() {
    // The payload travels inline now (see `WireUiEvent::Reload`), so the
    // round trip has to carry a real document rather than a handle.
    let payload = mizu_ipc::wire::reload::WireReloadPayload {
        logic_fn_keys: vec![],
        logic_fn_values: vec![],
        click_action_ids: vec![7],
        click_actions: vec![mizu_ipc::wire::reload::WireAction::Eval(
            mizu_ipc::wire::reload::WireExprTree {
                nodes: vec![mizu_ipc::wire::reload::WireExpr::Literal(WireValue::Int(1))],
                args_pool: vec![],
                root: 0,
            },
        )],
        submit_action_ids: vec![],
        submit_actions: vec![],
        root_timer_actions: vec![],
        interner_strings: vec!["greeting".to_string()],
        init_var_keys: vec![],
        init_var_values: vec![],
        url_registry_keys: vec![],
        url_registry_values: vec![],
        document_domain: "example.com".to_string(),
        computed_bindings: vec![],
    };
    let ev = WireUiEvent::Reload(Box::new(payload));
    let rt = round_trip(&ev);
    let WireUiEvent::Reload(p) = rt else { panic!() };
    assert_eq!(p.document_domain, "example.com");
    assert_eq!(p.interner_strings, vec!["greeting".to_string()]);
    assert_eq!(p.click_action_ids, vec![7]);
}

#[test]
fn wire_ui_event_close_tab() {
    let rt = round_trip(&WireUiEvent::CloseTab);
    assert!(matches!(rt, WireUiEvent::CloseTab));
}

// ── WireRuntimeAction ────────────────────────────────────────────────────────

#[test]
fn wire_runtime_action_none() {
    let rt = round_trip(&WireRuntimeAction::None);
    assert!(matches!(rt, WireRuntimeAction::None));
}

#[test]
fn wire_runtime_action_store_local() {
    let a = WireRuntimeAction::StoreLocal {
        key:   "session_token".into(),
        value: WireValue::Str("abc123".into()),
    };
    let rt = round_trip(&a);
    let WireRuntimeAction::StoreLocal { key, value } = rt else { panic!() };
    assert_eq!(key, "session_token");
    assert!(matches!(value, WireValue::Str(ref s) if s == "abc123"));
}

#[test]
fn wire_runtime_action_network_call() {
    let a = WireRuntimeAction::NetworkCall {
        method:              WireNetworkMethod::Post,
        endpoint_symbol:     5,
        payload:             Some(WireValue::Null),
        path_param:          None,
        target_variable_sym: 2,
        format:              WirePayloadFormat::Json,
        header_keys:         vec!["X-Token".into()],
        header_values:       vec![WireValue::Str("tok".into())],
    };
    let rt = round_trip(&a);
    let WireRuntimeAction::NetworkCall { method, endpoint_symbol, .. } = rt else { panic!() };
    assert!(matches!(method, WireNetworkMethod::Post));
    assert_eq!(endpoint_symbol, 5);
}

#[test]
fn wire_runtime_action_navigate() {
    let a = WireRuntimeAction::Navigate { url: "mizu://example.com/home".into() };
    let rt = round_trip(&a);
    let WireRuntimeAction::Navigate { url } = rt else { panic!() };
    assert_eq!(url, "mizu://example.com/home");
}

// ── WireWorkerEnvelope ───────────────────────────────────────────────────────

#[test]
fn wire_worker_envelope_ok() {
    let resp = WireWorkerResponse {
        mutated_syms:    vec![0, 1],
        mutated_values:  vec![WireValue::Bool(true), WireValue::Decimal(0)],
        runtime_actions: vec![WireRuntimeAction::None],
        gesture:         true,
    };
    let env = WireWorkerEnvelope::Ok(resp);
    let rt = round_trip(&env);
    let WireWorkerEnvelope::Ok(r) = rt else { panic!() };
    assert!(r.gesture);
    assert_eq!(r.mutated_syms.len(), 2);
}

#[test]
fn wire_worker_envelope_err_timeout() {
    let env = WireWorkerEnvelope::Err(WireWorkerError::Timeout);
    let rt = round_trip(&env);
    assert!(matches!(rt, WireWorkerEnvelope::Err(WireErr::Timeout)));
}

#[test]
fn wire_worker_envelope_err_security_violation() {
    let env = WireWorkerEnvelope::Err(WireWorkerError::SecurityViolation(
        "attempted open() syscall".into(),
    ));
    let rt = round_trip(&env);
    let WireWorkerEnvelope::Err(WireErr::SecurityViolation(msg)) = rt else { panic!() };
    assert_eq!(msg, "attempted open() syscall");
}
