//! Tests for the typecheck module.

use super::*;
use crate::core::types::StringInterner;
use crate::parser::logic::parse_logic;

// Helper to parse logic string and typecheck the functions
fn check_logic_string(src: &str) -> Result<(), MizuError> {
    let mut interner = StringInterner::new();
    let fns = parse_logic(src, &mut interner)?;
    let dom = ego_tree::Tree::new(MizuNode {
        primitive: crate::parser::layout::Primitive::Box,
        attributes: rustc_hash::FxHashMap::default(),
        events: rustc_hash::FxHashMap::default(),
        iterator_context: None,
        conditional_classes: Vec::new(),
    });
    check_types(&dom, &[], &fns, &[], &interner)
}

#[test]
fn annotated_param_accepted() {
    let src = "f(x: num) : x + 1";
    assert!(check_logic_string(src).is_ok());
}

#[test]
fn missing_field_on_record_rejected() {
    let src = "f(r: record{a: num}) : r.b";
    let err = check_logic_string(src).unwrap_err();
    assert!(matches!(err, MizuError::StaticTypeError(_)));
    if let MizuError::StaticTypeError(msg) = err {
        assert!(msg.contains("field `b` not found"));
    }
}

#[test]
fn field_on_non_record_rejected() {
    let src = "f(x: num) : x.field";
    let err = check_logic_string(src).unwrap_err();
    assert!(matches!(err, MizuError::StaticTypeError(_)));
}

// ────────────────────────────────────────────────────────────────────────
// NetworkCall — `as` format-dependent static payload shape checks
// ────────────────────────────────────────────────────────────────────────

#[test]
fn network_call_as_text_rejects_int_literal_payload() {
    let mut interner = StringInterner::new();
    let functions = FxHashMap::default();
    let env = Env::default();
    let action =
        crate::parser::logic::parse_action("POST(orders, 42) -> resp as text", &mut interner)
            .unwrap();
    let err = check_action(&action, &env, &functions, &interner).unwrap_err();
    assert!(matches!(err, MizuError::StaticTypeError(_)));
}

#[test]
fn network_call_as_text_accepts_string_literal_payload() {
    let mut interner = StringInterner::new();
    let functions = FxHashMap::default();
    let env = Env::default();
    let action =
        crate::parser::logic::parse_action(r#"POST(orders, "hi") -> resp as text"#, &mut interner)
            .unwrap();
    assert!(check_action(&action, &env, &functions, &interner).is_ok());
}

#[test]
fn network_call_as_json_accepts_any_literal_payload() {
    // The default/explicit `json` format imposes no static shape constraint.
    let mut interner = StringInterner::new();
    let functions = FxHashMap::default();
    let env = Env::default();
    let action =
        crate::parser::logic::parse_action("POST(orders, 42) -> resp", &mut interner).unwrap();
    assert!(check_action(&action, &env, &functions, &interner).is_ok());
}

#[test]
fn network_call_as_multipart_rejects_int_literal_payload() {
    let mut interner = StringInterner::new();
    let functions = FxHashMap::default();
    let env = Env::default();
    let action =
        crate::parser::logic::parse_action("POST(orders, 42) -> resp as multipart", &mut interner)
            .unwrap();
    let err = check_action(&action, &env, &functions, &interner).unwrap_err();
    assert!(matches!(err, MizuError::StaticTypeError(_)));
}
