//! Tests for `value.rs`: `Value` construction/`Display`, JSON conversion,
//! and `FileHandle`'s deliberately inert behavior.

use super::*;

#[test]
fn string_from_string_ref() {
    let v = Value::from("hello");
    assert_eq!(v, Value::String(std::sync::Arc::from("hello")));
}

#[test]
fn string_from_owned_string() {
    let v = Value::from(String::from("world"));
    assert_eq!(v, Value::String(std::sync::Arc::from("world")));
}

#[test]
fn string_display_verbatim() {
    let v = Value::String(std::sync::Arc::from("Mizu rocks"));
    assert_eq!(v.to_string(), "Mizu rocks");
}

#[test]
fn bool_from_true() {
    let v = Value::from(true);
    assert_eq!(v, Value::Bool(true));
}

#[test]
fn bool_from_false() {
    let v = Value::from(false);
    assert_eq!(v, Value::Bool(false));
}

#[test]
fn bool_display_lowercase() {
    assert_eq!(Value::Bool(true).to_string(), "true");
    assert_eq!(Value::Bool(false).to_string(), "false");
}

#[test]
fn list_display_empty() {
    let v = Value::List(std::sync::Arc::new(vec![]));
    assert_eq!(v.to_string(), "[]");
}

#[test]
fn list_display_single_element() {
    let v = Value::List(std::sync::Arc::new(vec![Value::Decimal(DECIMAL_SCALE)]));
    assert_eq!(v.to_string(), "[1]");
}

#[test]
fn list_display_multiple_elements() {
    let v = Value::List(std::sync::Arc::new(vec![
        Value::Decimal(DECIMAL_SCALE),
        Value::String(std::sync::Arc::from("two")),
        Value::Bool(false),
    ]));
    assert_eq!(v.to_string(), "[1, two, false]");
}

#[test]
fn list_display_nested() {
    let inner = Value::List(std::sync::Arc::new(vec![
        Value::Decimal(2 * DECIMAL_SCALE),
        Value::Decimal(3 * DECIMAL_SCALE),
    ]));
    let outer = Value::List(std::sync::Arc::new(vec![
        Value::Decimal(DECIMAL_SCALE),
        inner,
    ]));
    assert_eq!(outer.to_string(), "[1, [2, 3]]");
}

#[test]
fn json_object_becomes_record() {
    let json = serde_json::to_string(&serde_json::json!({"id":1,"name":"Neko"})).unwrap();
    let val = from_json_str(&json).unwrap();
    assert_eq!(
        val.get_field(crate::core::types::hash_field("id"), "id"),
        Some(&Value::Decimal(DECIMAL_SCALE))
    );
    assert_eq!(
        val.get_field(crate::core::types::hash_field("name"), "name"),
        Some(&Value::String(Arc::from("Neko")))
    );
}

#[test]
fn json_array_of_objects() {
    let json = serde_json::to_string(&serde_json::json!([{"id":1},{"id":2}])).unwrap();
    let val = from_json_str(&json).unwrap();
    if let Value::List(ref items) = val {
        assert_eq!(items.len(), 2);
        assert!(
            matches!(items[0], Value::Record(_)),
            "first element must be Record"
        );
        assert!(
            matches!(items[1], Value::Record(_)),
            "second element must be Record"
        );
    } else {
        panic!("expected Value::List, got {val:?}");
    }
}

#[test]
fn json_string_passthrough() {
    let json = serde_json::to_string(&serde_json::json!("hello")).unwrap();
    let val = from_json_str(&json).unwrap();
    assert_eq!(val, Value::String(Arc::from("hello")));
}

/// Round-trips a `serde_json::Value` through `from_json_str`, matching the
/// original file's helper of the same name.
fn from_json(json: &serde_json::Value) -> Result<Value, MizuError> {
    let s = serde_json::to_string(json).map_err(|e| MizuError::SecurityViolation(e.to_string()))?;
    from_json_str(&s)
}

#[test]
fn json_null_becomes_value_null() {
    let val = from_json(&serde_json::Value::Null).unwrap();
    assert_eq!(val, Value::Null);
}

#[test]
fn json_bool_becomes_value_bool() {
    assert_eq!(
        from_json(&serde_json::json!(true)).unwrap(),
        Value::Bool(true)
    );
    assert_eq!(
        from_json(&serde_json::json!(false)).unwrap(),
        Value::Bool(false)
    );
}

#[test]
fn json_integer_becomes_value_int() {
    let val = from_json(&serde_json::json!(42)).unwrap();
    assert_eq!(val, Value::Decimal(42 * DECIMAL_SCALE));
}

#[test]
fn json_float_becomes_value_int() {
    let val = from_json(&serde_json::json!(3.14)).unwrap();
    assert_eq!(val, Value::Decimal(314_000_000));
}

#[test]
fn json_integer_exact_path_avoids_float_precision_loss() {
    // A whole-number JSON literal near the top of the representable
    // range must round-trip exactly via the checked_mul integer path,
    // not lose precision the way dividing through f64 unconditionally
    // used to.
    let val = from_json(&serde_json::json!(92_233_720_368_i64)).unwrap();
    assert_eq!(val, Value::Decimal(92_233_720_368 * DECIMAL_SCALE));
}

#[test]
fn json_integer_parses_to_true_int_without_overflow() {
    let val = from_json(&serde_json::json!(i64::MAX)).unwrap();
    assert_eq!(val, Value::Int(i64::MAX));
}

#[test]
fn json_roundtrip_exact_at_top_of_range() {
    // i64::MAX is the largest representable scaled value: exactly
    // 92233720368.54775807. Round-tripping it through to_json/from_json
    // must recover the exact same Value::Decimal, pinning the from_json
    // exact-integer path and to_json's exact-integer emission against
    // silent precision loss at the top of the new 8-decimal-digit range.
    let original = Value::Decimal(i64::MAX);
    let json = to_json(&original).unwrap();
    let roundtripped = from_json(&json).unwrap();
    assert_eq!(roundtripped, original);
}

#[test]
fn record_display_contains_fields() {
    let json = serde_json::to_string(&serde_json::json!({"x":1})).unwrap();
    let val = from_json_str(&json).unwrap();
    let display = val.to_string();
    assert!(
        display.contains("x"),
        "display must contain field name: {display}"
    );
    assert!(
        display.contains("1"),
        "display must contain field value: {display}"
    );
    assert!(
        display.starts_with('{'),
        "display must start with '{{': {display}"
    );
    assert!(
        display.ends_with('}'),
        "display must end with '}}': {display}"
    );
}

#[test]
fn from_json_depth_limit_returns_err() {
    // Build a 300-level nested array: [[[[...[42]...]]]]
    // Nesting beyond MAX_JSON_DEPTH (== MAX_EVAL_DEPTH == 256) must be
    // rejected outright with Err(MizuError::SecurityViolation) rather
    // than silently clamped to Value::Null — a clamp would let a caller
    // mistake a malicious deeply-nested payload for legitimate absent
    // data.
    let mut json = serde_json::json!(42_);
    for _ in 0..300 {
        json = serde_json::json!([json]);
    }

    let result = from_json(&json);

    assert!(
        matches!(result, Err(MizuError::SecurityViolation(_))),
        "deeply-nested JSON must be rejected with SecurityViolation, got: {result:?}"
    );
}

#[test]
fn from_json_shallow_nesting_parses_fully() {
    // A 3-level nested array (well within MAX_JSON_DEPTH) must parse
    // completely — the depth limit must not truncate legitimate data.
    let json = serde_json::json!([[[42_i64]]]);
    let result = from_json(&json).unwrap();

    let l1 = match &result {
        Value::List(v) => &v[0],
        other => panic!("level 0 must be List: {other:?}"),
    };
    let l2 = match l1 {
        Value::List(v) => &v[0],
        other => panic!("level 1 must be List: {other:?}"),
    };
    let leaf = match l2 {
        Value::List(v) => &v[0],
        other => panic!("level 2 must be List: {other:?}"),
    };
    assert_eq!(
        *leaf,
        Value::Decimal(42 * DECIMAL_SCALE),
        "leaf must be Int(42 * DECIMAL_SCALE)"
    );
}

// ────────────────────────────────────────────────────────────────────────
// Value::FileHandle — deliberately inert (never compared/serialised/
// displayed in full)
// ────────────────────────────────────────────────────────────────────────

fn file_handle(filename: &str) -> Value {
    Value::FileHandle(Arc::new(FileHandleData {
        path: std::path::PathBuf::from(format!("/home/user/secret-dir/{filename}")),
        filename: filename.to_string(),
    }))
}

#[test]
fn file_handle_pointer_equality() {
    let a = file_handle("avatar.png");
    // Pointer equality is used for FileHandle to preserve PartialEq reflexivity
    let a2 = a.clone();
    assert_eq!(
        a, a2,
        "Cloned file handles should be equal via pointer equality"
    );

    let b = file_handle("avatar.png"); // same filename, distinct handle
    assert_ne!(a, b);
}

#[test]
fn file_handle_to_json_errors_without_leaking_the_path() {
    let handle = file_handle("resume.pdf");
    let err = to_json(&handle).unwrap_err();
    assert!(matches!(err, MizuError::ExecutionError(_)));
    let msg = err.to_string();
    assert!(
        !msg.contains("secret-dir") && !msg.contains("/home/user"),
        "to_json error message must not leak the file's path: {msg}"
    );
}

#[test]
fn file_handle_nested_in_record_still_fails_to_json() {
    let record = Value::record_from_unsorted(vec![("avatar", file_handle("avatar.png"))]);
    assert!(to_json(&record).is_err());
}

#[test]
fn file_handle_nested_in_list_still_fails_to_json() {
    let list = Value::List(Arc::new(vec![file_handle("a.txt"), file_handle("b.txt")]));
    assert!(to_json(&list).is_err());
}

#[test]
fn file_handle_display_redacts_the_full_path() {
    let handle = file_handle("tax-return-2025.pdf");
    let rendered = handle.to_string();
    assert!(rendered.contains("tax-return-2025.pdf"));
    assert!(
        !rendered.contains("secret-dir") && !rendered.contains("/home/user"),
        "Display must never show the full path: {rendered}"
    );
}
